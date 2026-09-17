//! Transparent sockets (Linux TPROXY).
//!
//! The listener accepts connections the kernel redirected to it, and reports
//! the address the player originally dialled. The connection onwards to the
//! server is bound to the player's address, so the server sees the player, not
//! the gateway. Both need `CAP_NET_ADMIN` and the rules from `netsetup`.

use std::{io, net::SocketAddr, time::Duration};

use tokio::{net::TcpStream, time};

/// Connects to `server` as `client`, within `timeout`.
///
/// On other platforms this is an ordinary connection, which is enough for the
/// tests to run anywhere.
pub async fn connect(
    server: SocketAddr,
    client: SocketAddr,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let attempt = async {
        #[cfg(target_os = "linux")]
        let stream = connect_as(server, client).await?;
        #[cfg(not(target_os = "linux"))]
        let stream = {
            let _ = client;
            TcpStream::connect(server).await?
        };
        stream.set_nodelay(true)?;
        Ok(stream)
    };
    time::timeout(timeout, attempt)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))?
}

/// Detects dead peers with TCP keepalives instead of an application timeout:
/// intercepted connections belong to someone else's server, which may well
/// stay quiet for a long time on purpose.
pub fn enable_keepalive(stream: &TcpStream) -> io::Result<()> {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let keepalive = keepalive.with_retries(6);
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Binds to the client's address — on a fresh port, since the intercepted
/// connection still occupies the client's own — and connects. The socket
/// carries [`crate::netsetup::SOCKET_MARK`] so the rules can route the replies
/// back here.
#[cfg(target_os = "linux")]
async fn connect_as(server: SocketAddr, client: SocketAddr) -> io::Result<TcpStream> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    use socket2::SockAddr;
    use tokio::net::TcpSocket;

    let mut bind_addr = match_family(client, server)?;
    bind_addr.set_port(0);
    let socket = transparent_socket(server.is_ipv4())?;
    socket.set_reuse_address(true)?;
    socket.set_mark(crate::netsetup::SOCKET_MARK)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SockAddr::from(bind_addr))?;

    // Hand the configured fd to tokio, which drives the non-blocking connect.
    let tcp = unsafe { TcpSocket::from_raw_fd(socket.into_raw_fd()) };
    tcp.connect(server).await
}

/// Binds a listener that accepts connections redirected to it by a TPROXY
/// rule. Its accepted sockets report the address the client originally dialled
/// as their local address.
#[cfg(target_os = "linux")]
pub fn listen(address: SocketAddr, backlog: u32) -> io::Result<tokio::net::TcpListener> {
    use socket2::SockAddr;

    let socket = transparent_socket(address.is_ipv4())?;
    socket.set_reuse_address(true)?;
    if address.is_ipv6() {
        // The IPv4 listener is a separate socket on the same port.
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&SockAddr::from(address))?;
    socket.listen(backlog as i32)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// Creates a socket with `IP_TRANSPARENT` set, with a diagnosable error.
#[cfg(target_os = "linux")]
fn transparent_socket(ipv4: bool) -> io::Result<socket2::Socket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if ipv4 { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    let result = if ipv4 {
        socket.set_ip_transparent_v4(true)
    } else {
        socket.set_ip_transparent_v6(true)
    };

    result.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "cannot set IP_TRANSPARENT ({err}); the gateway needs CAP_NET_ADMIN \
                 (systemd: AmbientCapabilities=CAP_NET_ADMIN)"
            ),
        )
    })?;

    Ok(socket)
}

/// Verifies at start-up that transparent sockets can actually be created.
///
/// Without this the first player is the one who discovers that the capability
/// is missing, and the symptom — every backend connection failing — looks like
/// a network problem rather than a permissions one.
#[cfg(target_os = "linux")]
pub fn preflight() -> io::Result<()> {
    transparent_socket(true).map(drop)
}

#[cfg(not(target_os = "linux"))]
pub fn listen(_address: SocketAddr, _backlog: u32) -> io::Result<tokio::net::TcpListener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "port interception requires Linux TPROXY",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn preflight() -> io::Result<()> {
    Ok(())
}

/// Makes the client address usable as a bind address for a socket of the
/// backend's family.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the Linux path binds")
)]
fn match_family(client: SocketAddr, backend: SocketAddr) -> io::Result<SocketAddr> {
    use std::net::IpAddr;

    let client = match client.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), client.port()),
            None => client,
        },
        _ => client,
    };

    match (client.ip(), backend.ip()) {
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => Ok(client),
        // A v4 client can still be expressed on a v6 socket.
        (IpAddr::V4(v4), IpAddr::V6(_)) => Ok(SocketAddr::new(
            IpAddr::V6(v4.to_ipv6_mapped()),
            client.port(),
        )),
        // A v6 client cannot be expressed as v4 at all.
        (IpAddr::V6(_), IpAddr::V4(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot transparently connect an IPv6 client ({client}) to an IPv4 server \
                 ({backend})"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn unmaps_v4_clients_for_v4_backends() {
        let bound =
            match_family(addr("[::ffff:203.0.113.7]:51234"), addr("10.0.0.1:25565")).unwrap();
        assert_eq!(bound, addr("203.0.113.7:51234"));
    }

    #[test]
    fn maps_v4_clients_onto_v6_backends() {
        let bound = match_family(addr("203.0.113.7:51234"), addr("[2001:db8::1]:25565")).unwrap();
        assert_eq!(bound, addr("[::ffff:203.0.113.7]:51234"));
    }

    #[test]
    fn refuses_v6_client_to_v4_backend() {
        let err = match_family(addr("[2001:db8::7]:51234"), addr("10.0.0.1:25565")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn nothing_to_check_off_linux() {
        assert!(preflight().is_ok());
    }
}
