//! Linux TPROXY: connect to the backend *as* the client.
//!
//! The outbound socket is bound to the client's own address before connecting,
//! so the backend's `accept()` reports the real player IP. Nothing is added to
//! the byte stream, which is why this is the only method that works for vanilla
//! and for the modloaders built on it.
//!
//! Three things are required on the host, and all three are outside this
//! process (see `deploy/nftables/`):
//!
//! 1. `CAP_NET_ADMIN` (or root) to set `IP_TRANSPARENT`
//! 2. a routing rule that brings the backend's replies back to the gateway
//! 3. backends whose default route points at the gateway, or policy routing
//!    that has the same effect

use std::{io, net::SocketAddr};

use tokio::net::TcpStream;

/// Whether this build can do transparent connections at all.
pub const fn is_supported() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(target_os = "linux")]
pub async fn connect(backend: SocketAddr, client: SocketAddr) -> io::Result<TcpStream> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    use tokio::net::TcpSocket;

    let bind_addr = match_family(client, backend)?;
    let domain = if backend.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };

    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    // Without SO_REUSEADDR a client reconnecting from the same port while the
    // previous socket lingers in TIME_WAIT would be refused.
    socket.set_reuse_address(true)?;
    set_transparent(&socket, backend.is_ipv4())?;
    socket.bind(&SockAddr::from(bind_addr))?;

    // Hand the configured fd to tokio, which drives the non-blocking connect.
    let tcp = unsafe { TcpSocket::from_raw_fd(socket.into_raw_fd()) };
    tcp.connect(backend).await
}

#[cfg(target_os = "linux")]
fn set_transparent(socket: &socket2::Socket, ipv4: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    // libc exposes these as plain ints; going through setsockopt directly keeps
    // this independent of socket2's shifting helper names.
    let (level, option) = if ipv4 {
        (libc::IPPROTO_IP, libc::IP_TRANSPARENT)
    } else {
        (libc::IPPROTO_IPV6, libc::IPV6_TRANSPARENT)
    };
    let enable: libc::c_int = 1;

    // SAFETY: the fd is owned by `socket` and outlives the call; the option
    // value is a correctly sized and aligned c_int.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            option,
            std::ptr::from_ref(&enable).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };

    if rc != 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!(
                "cannot set IP_TRANSPARENT ({err}); the gateway needs CAP_NET_ADMIN \
                 (systemd: AmbientCapabilities=CAP_NET_ADMIN)"
            ),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn connect(_backend: SocketAddr, _client: SocketAddr) -> io::Result<TcpStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "transparent forwarding requires Linux TPROXY; use `forwarding: proxy_protocol_v2` \
         or `none` on this platform",
    ))
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
                 ({backend}); give the backend an IPv6 address or use proxy_protocol_v2"
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
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(connect(addr("10.0.0.1:25565"), addr("203.0.113.7:51234")))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
