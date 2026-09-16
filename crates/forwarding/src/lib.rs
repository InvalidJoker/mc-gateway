//! How a backend learns the real client IP.
//!
//! The trust boundary is the point of this crate:
//!
//! ```text
//! internet ──untrusted──▶ gateway ──trusted forwarding──▶ backend
//! ```
//!
//! Address information only ever flows outward, from the gateway's own
//! `accept()` result. Nothing a client sends is ever forwarded as identity —
//! that is exactly the hole that lets players spoof their IP through a
//! misconfigured proxy.

pub mod proxy_protocol;
pub mod transparent;

use std::{io, net::SocketAddr, time::Duration};

use mc_config::Forwarding;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, lookup_host},
    time,
};

pub use proxy_protocol::{ProxyHeader, parse as parse_proxy_header};

/// Who this connection is being opened for.
#[derive(Debug, Clone, Copy)]
pub enum Origin {
    /// A player session. `src` is the client as the gateway sees it, `dst` the
    /// address the client connected to.
    Client { src: SocketAddr, dst: SocketAddr },
    /// The gateway itself — health checks and probes.
    Gateway,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("connect to {address} timed out after {timeout:?}")]
    Timeout { address: String, timeout: Duration },

    #[error("cannot resolve `{address}`: {source}")]
    Resolve {
        address: String,
        #[source]
        source: io::Error,
    },

    #[error("`{address}` resolved to no addresses")]
    NoAddress { address: String },

    #[error("connect to {address} failed: {source}")]
    Io {
        address: String,
        #[source]
        source: io::Error,
    },
}

impl ConnectError {
    /// Short metrics label.
    pub fn kind(&self) -> &'static str {
        match self {
            ConnectError::Timeout { .. } => "timeout",
            ConnectError::Resolve { .. } => "resolve",
            ConnectError::NoAddress { .. } => "no_address",
            ConnectError::Io { .. } => "io",
        }
    }
}

/// Resolves `host:port`, which may be a DNS name.
///
/// Resolution happens per connect rather than once at start-up so a backend
/// that moves (container restart, failover) is picked up without a reload.
pub async fn resolve(address: &str) -> Result<SocketAddr, ConnectError> {
    if let Ok(addr) = address.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let mut addrs = lookup_host(address)
        .await
        .map_err(|source| ConnectError::Resolve { address: address.to_owned(), source })?;
    addrs.next().ok_or_else(|| ConnectError::NoAddress { address: address.to_owned() })
}

/// Opens a connection to `backend`, applying the configured forwarding method.
///
/// The PROXY header, when one is used, is written before this returns, so a
/// caller can never accidentally send Minecraft bytes ahead of it.
pub async fn connect(
    backend: SocketAddr,
    forwarding: Forwarding,
    origin: Origin,
    timeout: Duration,
) -> Result<TcpStream, ConnectError> {
    let address = backend.to_string();
    let attempt = async {
        let mut stream = match (forwarding, origin) {
            (Forwarding::Transparent, Origin::Client { src, .. }) => {
                transparent::connect(backend, src).await?
            }
            // A health check has no client to impersonate.
            _ => TcpStream::connect(backend).await?,
        };

        // Minecraft is a chatty, small-packet protocol; Nagle adds latency for
        // nothing.
        stream.set_nodelay(true)?;

        if forwarding == Forwarding::ProxyProtocolV2 {
            let header = match origin {
                Origin::Client { src, dst } => proxy_protocol::encode_v2(src, dst),
                Origin::Gateway => proxy_protocol::encode_v2_local(),
            };
            stream.write_all(&header).await?;
        }

        Ok::<_, io::Error>(stream)
    };

    match time::timeout(timeout, attempt).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(source)) => Err(ConnectError::Io { address, source }),
        Err(_) => Err(ConnectError::Timeout { address, timeout }),
    }
}

/// Fails fast at start-up instead of on the first player.
pub fn check_platform_support(forwarding: Forwarding) -> Result<(), String> {
    if forwarding == Forwarding::Transparent && !transparent::is_supported() {
        return Err(
            "transparent forwarding requires Linux TPROXY and CAP_NET_ADMIN; this build cannot \
             provide it"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncReadExt, net::TcpListener};

    async fn echo_server() -> (SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            // The client closes after writing, which ends this read.
            stream.read_to_end(&mut buf).await.unwrap();
            buf
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn plain_forwarding_sends_no_header() {
        let (addr, server) = echo_server().await;
        let mut stream = connect(addr, Forwarding::None, Origin::Gateway, Duration::from_secs(2))
            .await
            .unwrap();
        stream.write_all(b"handshake").await.unwrap();
        stream.shutdown().await.unwrap();
        assert_eq!(server.await.unwrap(), b"handshake");
    }

    #[tokio::test]
    async fn proxy_protocol_header_precedes_the_payload() {
        let (addr, server) = echo_server().await;
        let client: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        let local: SocketAddr = "198.51.100.1:25565".parse().unwrap();
        let mut stream = connect(
            addr,
            Forwarding::ProxyProtocolV2,
            Origin::Client { src: client, dst: local },
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        stream.write_all(b"handshake").await.unwrap();
        stream.shutdown().await.unwrap();

        let received = server.await.unwrap();
        let (header, len) = proxy_protocol::parse(&received).unwrap();
        assert_eq!(header.source(), Some(client));
        assert_eq!(&received[len..], b"handshake");
    }

    #[tokio::test]
    async fn health_checks_use_the_local_command() {
        let (addr, server) = echo_server().await;
        let stream =
            connect(addr, Forwarding::ProxyProtocolV2, Origin::Gateway, Duration::from_secs(2))
                .await
                .unwrap();
        drop(stream);

        let received = server.await.unwrap();
        assert_eq!(proxy_protocol::parse(&received).unwrap().0, ProxyHeader::Local);
    }

    #[tokio::test]
    async fn connect_timeout_is_reported_as_such() {
        // 203.0.113.0/24 is TEST-NET-3: guaranteed not to answer.
        let err = connect(
            "203.0.113.1:25565".parse().unwrap(),
            Forwarding::None,
            Origin::Gateway,
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), "timeout", "got {err}");
    }

    #[tokio::test]
    async fn resolves_names_and_literals() {
        assert_eq!(resolve("127.0.0.1:25565").await.unwrap().port(), 25565);
        assert!(resolve("localhost:25565").await.unwrap().ip().is_loopback());
        assert!(resolve("no-such-host.invalid:25565").await.is_err());
    }

    #[test]
    fn platform_support_is_checked_up_front() {
        assert!(check_platform_support(Forwarding::None).is_ok());
        assert!(check_platform_support(Forwarding::ProxyProtocolV2).is_ok());
        assert_eq!(check_platform_support(Forwarding::Transparent).is_ok(), cfg!(target_os = "linux"));
    }
}
