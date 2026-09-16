//! Byte shovelling, once routing is done.
//!
//! After the handshake the gateway understands nothing about the traffic — and
//! must not, or compression, encryption and every modloader's custom packets
//! would become its problem. This is the part that stays a plain L4 proxy.

use std::{io, time::Duration};

use tokio::{io::copy_bidirectional, net::TcpStream};
use tokio_io_timeout::TimeoutStream;

/// Copies in both directions until either side closes.
///
/// `idle` bounds how long a direction may be silent. Without it a half-open
/// connection — a client that vanished without a FIN, which on mobile networks
/// is routine — would hold a backend slot indefinitely.
///
/// Returns `(to_backend, to_client)` on a clean close. On a timeout or a reset
/// the byte counts are lost, which is a deliberate trade: `copy_bidirectional`
/// does the half-close handling correctly, and that is worth more than exact
/// accounting for aborted sessions.
pub async fn run(
    client: TcpStream,
    backend: TcpStream,
    idle: Option<Duration>,
) -> io::Result<(u64, u64)> {
    let mut client = timed(client, idle);
    let mut backend = timed(backend, idle);
    copy_bidirectional(&mut client, &mut backend).await
}

/// Applies the idle timeout to both directions of one stream.
fn timed(stream: TcpStream, idle: Option<Duration>) -> std::pin::Pin<Box<TimeoutStream<TcpStream>>> {
    let mut stream = TimeoutStream::new(stream);
    stream.set_read_timeout(idle);
    stream.set_write_timeout(idle);
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time,
    };

    /// Two ends of one connection.
    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connecting = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        (connecting.await.unwrap(), server)
    }

    #[tokio::test]
    async fn moves_bytes_in_both_directions() {
        let (client_far, client_near) = pair().await;
        let (backend_near, backend_far) = pair().await;

        let proxy = tokio::spawn(async move { run(client_near, backend_near, None).await });

        let echo = tokio::spawn(async move {
            let mut backend_far = backend_far;
            let mut buf = [0u8; 64];
            let n = backend_far.read(&mut buf).await.unwrap();
            backend_far.write_all(b"pong").await.unwrap();
            backend_far.shutdown().await.unwrap();
            buf[..n].to_vec()
        });

        let mut client_far = client_far;
        client_far.write_all(b"ping").await.unwrap();
        client_far.shutdown().await.unwrap();

        let mut reply = Vec::new();
        client_far.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"pong");
        assert_eq!(echo.await.unwrap(), b"ping");
        assert_eq!(proxy.await.unwrap().unwrap(), (4, 4));
    }

    #[tokio::test]
    async fn a_silent_connection_is_closed_by_the_idle_timeout() {
        let (client_far, client_near) = pair().await;
        let (backend_near, backend_far) = pair().await;

        let proxy = tokio::spawn(async move {
            run(client_near, backend_near, Some(Duration::from_millis(100))).await
        });

        // Neither end says anything; the proxy must give up on its own.
        let result = time::timeout(Duration::from_secs(5), proxy)
            .await
            .expect("idle timeout fired")
            .unwrap();
        assert!(result.is_err(), "an idle session ends as an error, not a clean close");
        drop((client_far, backend_far));
    }

    #[tokio::test]
    async fn a_backend_disconnect_releases_the_client() {
        let (client_far, client_near) = pair().await;
        let (backend_near, backend_far) = pair().await;

        let proxy = tokio::spawn(async move {
            run(client_near, backend_near, Some(Duration::from_millis(200))).await
        });

        drop(backend_far);
        let mut client_far = client_far;
        let mut buf = Vec::new();
        // The client sees a clean EOF rather than hanging.
        time::timeout(Duration::from_secs(5), client_far.read_to_end(&mut buf))
            .await
            .expect("client was released")
            .unwrap();
        assert!(buf.is_empty());

        // The still-open, silent client direction is bounded by the idle timeout.
        time::timeout(Duration::from_secs(5), proxy).await.expect("session ended").unwrap().ok();
    }
}
