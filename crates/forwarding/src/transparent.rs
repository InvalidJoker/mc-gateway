//! Linux TPROXY: connect to the backend *as* the client.
//!
//! The outbound socket is bound to the client's own address before connecting,
//! so the backend's `accept()` reports the real player IP. Nothing is added to
//! the byte stream, which is why this works for vanilla and for every modloader
//! without any support on their side.
//!
//! Three things are required on the host, and all three are outside this
//! process (see `deploy/nftables/` and `docs/forwarding.md`):
//!
//! 1. `CAP_NET_ADMIN` (or root) to set `IP_TRANSPARENT`
//! 2. a routing rule that brings the backend's replies back to the gateway
//! 3. backends whose return path leads through the gateway

use std::{io, net::SocketAddr};

use tokio::net::TcpStream;

/// Whether this build can make transparent connections at all.
pub const fn is_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Connects to `backend` from the client's address.
///
/// `preserve_port` binds the client's exact source port too. That must be off
/// when the client's own connection is still in the conntrack table under the
/// same address pair — which is the case for an intercepted connection — or
/// the kernel cannot tell the two apart.
///
/// Every transparent socket carries [`crate::netsetup::SOCKET_MARK`], which
/// the generated nftables rules use to find the replies and deliver them here.
#[cfg(target_os = "linux")]
pub async fn connect(
    backend: SocketAddr,
    client: SocketAddr,
    preserve_port: bool,
) -> io::Result<TcpStream> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    use socket2::SockAddr;
    use tokio::net::TcpSocket;

    let mut bind_addr = match_family(client, backend)?;
    if !preserve_port {
        bind_addr.set_port(0);
    }
    let socket = transparent_socket(backend.is_ipv4())?;
    // Without SO_REUSEADDR a client reconnecting from the same port while the
    // previous socket lingers in TIME_WAIT would be refused.
    socket.set_reuse_address(true)?;
    socket.set_mark(crate::netsetup::SOCKET_MARK)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SockAddr::from(bind_addr))?;

    // Hand the configured fd to tokio, which drives the non-blocking connect.
    let tcp = unsafe { TcpSocket::from_raw_fd(socket.into_raw_fd()) };
    tcp.connect(backend).await
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

#[cfg(not(target_os = "linux"))]
pub fn listen(_address: SocketAddr, _backlog: u32) -> io::Result<tokio::net::TcpListener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "port interception requires Linux TPROXY",
    ))
}

#[cfg(not(target_os = "linux"))]
pub async fn connect(
    _backend: SocketAddr,
    _client: SocketAddr,
    _preserve_port: bool,
) -> io::Result<TcpStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "transparent forwarding requires Linux TPROXY",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn preflight() -> io::Result<()> {
    Ok(())
}

/// Makes the client address usable as a bind address for a socket of the
/// backend's family.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "only the Linux path binds"))]
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
        (IpAddr::V4(v4), IpAddr::V6(_)) => {
            Ok(SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), client.port()))
        }
        // A v6 client cannot be expressed as v4 at all.
        (IpAddr::V6(_), IpAddr::V4(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "cannot transparently connect an IPv6 client ({client}) to an IPv4 backend \
                 ({backend}); give the backend an IPv6 address"
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
        let bound = match_family(addr("[::ffff:203.0.113.7]:51234"), addr("10.0.0.1:25565")).unwrap();
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
    fn reports_unsupported_off_linux() {
        assert!(!is_supported());
        assert!(preflight().is_ok(), "nothing to check when TPROXY is not used");
    }
}
