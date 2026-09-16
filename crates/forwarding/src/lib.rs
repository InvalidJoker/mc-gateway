//! How the gateway reaches a backend, and how the backend learns who is
//! calling.
//!
//! There is one rule and no configuration:
//!
//! ```text
//! Linux  ──▶ TPROXY: the backend sees the player's real address
//! else   ──▶ plain TCP: the backend sees the gateway
//! ```
//!
//! Address information only ever flows outward, from the gateway's own
//! `accept()` result. Nothing a client sends is forwarded as identity — that is
//! exactly the hole that lets players spoof their IP through a misconfigured
//! proxy.

pub mod inbound;
pub mod transparent;

use std::{fmt, io, net::SocketAddr, time::Duration};

use tokio::{
    net::{TcpStream, lookup_host},
    time,
};

/// Who this connection is being opened for.
#[derive(Debug, Clone, Copy)]
pub enum Origin {
    /// A client's connection — a session or a status ping being passed through.
    /// On Linux this address is what the backend will see.
    Client(SocketAddr),
    /// The gateway itself: health checks and probes, which have no client to
    /// impersonate and always connect normally.
    Gateway,
}

/// What this build does for client connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Linux: bind the client's address (TPROXY).
    Transparent,
    /// Everywhere else: an ordinary connection.
    Direct,
}

impl Mode {
    pub const fn current() -> Self {
        if transparent::is_supported() { Mode::Transparent } else { Mode::Direct }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Transparent => "transparent",
            Mode::Direct => "direct",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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

/// Opens a connection to `backend`.
pub async fn connect(
    backend: SocketAddr,
    origin: Origin,
    timeout: Duration,
) -> Result<TcpStream, ConnectError> {
    let address = backend.to_string();

    let attempt = async {
        let stream = match origin {
            Origin::Client(client) if transparent::is_supported() => {
                transparent::connect(backend, client).await?
            }
            _ => TcpStream::connect(backend).await?,
        };
        // Minecraft is a chatty, small-packet protocol; Nagle adds latency for
        // nothing.
        stream.set_nodelay(true)?;
        Ok::<_, io::Error>(stream)
    };

    match time::timeout(timeout, attempt).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(source)) => Err(ConnectError::Io { address, source }),
        Err(_) => Err(ConnectError::Timeout { address, timeout }),
    }
}

/// Fails fast at start-up if transparent sockets cannot be created.
///
/// On Linux every client connection is transparent, so a missing capability is
/// not a degraded mode — it is a gateway that cannot reach any backend. Saying
/// so at start-up, with the fix, beats letting the first player discover it as
/// a connection that hangs.
pub fn preflight() -> Result<(), String> {
    transparent::preflight().map_err(|err| {
        format!(
            "{err}\n\nOn Linux the gateway connects to backends transparently, which needs \
             CAP_NET_ADMIN. Grant it with one of:\n  \
             sudo setcap cap_net_admin+ep <path to mc-gateway>\n  \
             systemd: AmbientCapabilities=CAP_NET_ADMIN (see deploy/systemd/)\n  \
             docker: cap_add: [NET_ADMIN]\n\n\
             The return path also has to be set up; see docs/forwarding.md."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn echo_server() -> (SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            buf
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn nothing_is_prepended_to_the_stream() {
        // Whichever mode is in use, the backend must receive Minecraft bytes
        // and nothing else — no header, ever.
        let (addr, server) = echo_server().await;
        let mut stream = connect(addr, Origin::Gateway, Duration::from_secs(2)).await.unwrap();
        stream.write_all(b"handshake").await.unwrap();
        stream.shutdown().await.unwrap();
        assert_eq!(server.await.unwrap(), b"handshake");
    }

    #[tokio::test]
    async fn a_gateway_origin_never_binds_a_client_address() {
        let (addr, server) = echo_server().await;
        let stream = connect(addr, Origin::Gateway, Duration::from_secs(2)).await.unwrap();
        // Health checks connect as themselves, from an ephemeral local port.
        assert_ne!(stream.local_addr().unwrap().port(), 0);
        drop(stream);
        assert!(server.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn connect_timeout_is_reported_as_such() {
        // 203.0.113.0/24 is TEST-NET-3: guaranteed not to answer.
        let err = connect(
            "203.0.113.1:25565".parse().unwrap(),
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
    fn the_mode_follows_the_platform() {
        assert_eq!(Mode::current() == Mode::Transparent, cfg!(target_os = "linux"));
        assert_eq!(Mode::current().to_string(), if cfg!(target_os = "linux") {
            "transparent"
        } else {
            "direct"
        });
    }
}
