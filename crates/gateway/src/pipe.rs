//! Byte shovelling, once routing is done.
//!
//! After the handshake the gateway understands nothing about the traffic — and
//! must not, or compression, encryption and every modloader's custom packets
//! would become its problem. This is the part that stays a plain L4 proxy.

use std::{
    io,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mc_metrics::Metrics;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time,
};

/// Chunk loading moves a lot of data; anything smaller wastes syscalls.
const BUFFER_SIZE: usize = 32 * 1024;

/// Grace period for the second direction once the first has closed.
///
/// Minecraft never half-closes a connection on purpose, so once one side is
/// done the session is over. This window only exists so the last few bytes
/// already in flight still arrive.
const LINGER: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Transferred {
    pub to_backend: u64,
    pub to_client: u64,
}

/// Copies in both directions until either side closes.
///
/// `idle` bounds how long a direction may be silent. Without it a half-open
/// connection — a client that vanished without a FIN, which on mobile networks
/// is routine — would hold a backend slot indefinitely.
pub async fn run(
    mut client: TcpStream,
    mut backend: TcpStream,
    idle: Option<Duration>,
    metrics: &Arc<Metrics>,
) -> Transferred {
    let (client_read, client_write) = client.split();
    let (backend_read, backend_write) = backend.split();

    // Counters live outside the futures so a direction that gets cancelled
    // still contributes what it managed to copy.
    let to_backend = AtomicU64::new(0);
    let to_client = AtomicU64::new(0);

    {
        let mut upstream = pin!(pump(client_read, backend_write, idle, &to_backend));
        let mut downstream = pin!(pump(backend_read, client_write, idle, &to_client));

        // Whichever side finishes first ends the session; the other gets a
        // bounded grace period to flush. Waiting for it unconditionally would
        // hang forever on a client that never sends a FIN.
        tokio::select! {
            _ = &mut upstream => {
                let _ = time::timeout(LINGER, &mut downstream).await;
            }
            _ = &mut downstream => {
                let _ = time::timeout(LINGER, &mut upstream).await;
            }
        }
    }

    let transferred = Transferred {
        to_backend: to_backend.load(Ordering::Relaxed),
        to_client: to_client.load(Ordering::Relaxed),
    };
    metrics.bytes.add("to_backend", transferred.to_backend);
    metrics.bytes.add("to_client", transferred.to_client);
    transferred
}

/// One direction. Shuts the writer down on completion so the peer sees a clean
/// close rather than a reset, which is what ends the opposite direction too.
async fn pump<R, W>(
    mut from: R,
    mut to: W,
    idle: Option<Duration>,
    counter: &AtomicU64,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUFFER_SIZE];
    let mut total = 0u64;

    let result = loop {
        let read = match idle {
            Some(limit) => match time::timeout(limit, from.read(&mut buf)).await {
                Ok(result) => result,
                Err(_) => break Err(io::Error::new(io::ErrorKind::TimedOut, "idle timeout")),
            },
            None => from.read(&mut buf).await,
        };

        match read {
            Ok(0) => break Ok(total),
            Ok(n) => {
                if let Err(err) = to.write_all(&buf[..n]).await {
                    break Err(err);
                }
                total += n as u64;
                counter.store(total, Ordering::Relaxed);
            }
            Err(err) => break Err(err),
        }
    };

    // Best effort: the peer may already be gone.
    let _ = to.shutdown().await;
    result.map(|_| total).or(Ok(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Returns (client side, backend side) of two connected socket pairs plus
    /// the far ends a test can drive.
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
        let metrics = Metrics::new();

        let proxy = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            async move { run(client_near, backend_near, None, &metrics).await }
        });

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

        let transferred = proxy.await.unwrap();
        assert_eq!(transferred, Transferred { to_backend: 4, to_client: 4 });
        assert_eq!(metrics.bytes.get("to_backend"), 4);
        assert_eq!(metrics.bytes.get("to_client"), 4);
    }

    #[tokio::test]
    async fn a_silent_connection_is_closed_by_the_idle_timeout() {
        let (client_far, client_near) = pair().await;
        let (backend_near, backend_far) = pair().await;
        let metrics = Metrics::new();

        let proxy = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            async move {
                run(client_near, backend_near, Some(Duration::from_millis(100)), &metrics).await
            }
        });

        // Neither end says anything; the proxy must give up on its own.
        let transferred =
            time::timeout(Duration::from_secs(5), proxy).await.expect("idle timeout fired");
        assert_eq!(transferred.unwrap(), Transferred::default());
        drop((client_far, backend_far));
    }

    #[tokio::test]
    async fn a_backend_disconnect_ends_the_session() {
        let (client_far, client_near) = pair().await;
        let (backend_near, backend_far) = pair().await;
        let metrics = Metrics::new();

        let proxy = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            async move { run(client_near, backend_near, None, &metrics).await }
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

        // The still-open, silent client direction must not keep the session
        // alive; the linger window bounds it.
        time::timeout(LINGER * 2, proxy).await.expect("session ended").unwrap();
    }
}
