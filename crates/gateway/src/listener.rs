//! The accept loop.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    net::{TcpListener, TcpSocket},
    sync::{mpsc, watch},
    time,
};
use tracing::{debug, error, info};

use crate::{app::{App, ListenerRuntime}, session};

/// Binds with an explicit backlog.
///
/// The default backlog is small on some systems, and a burst of joins after a
/// restart is exactly when it matters.
pub fn bind(address: SocketAddr, backlog: u32) -> io::Result<TcpListener> {
    let socket = if address.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    // Without this a restart fails for as long as the old socket sits in
    // TIME_WAIT.
    socket.set_reuseaddr(true)?;
    socket.bind(address)?;
    socket.listen(backlog)
}

/// Accepts until shutdown, spawning one task per connection.
pub async fn run(
    listener: TcpListener,
    context: Arc<ListenerRuntime>,
    app: Arc<App>,
    mut shutdown: watch::Receiver<bool>,
    sessions: mpsc::Sender<()>,
) {
    info!(listener = %context.name, bind = %context.bind, "listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let context = Arc::clone(&context);
                    let app = Arc::clone(&app);
                    // Cloned into the task so a graceful shutdown can wait for
                    // it: the channel closes only when every session is gone.
                    let token = sessions.clone();
                    tokio::spawn(async move {
                        session::serve(stream, peer, context, app).await;
                        drop(token);
                    });
                }
                Err(err) => {
                    // Running out of file descriptors would otherwise spin this
                    // loop at full speed while every accept fails.
                    debug!(listener = %context.name, %err, "accept failed");
                    if is_fatal(&err) {
                        error!(listener = %context.name, %err, "listener stopping");
                        return;
                    }
                    time::sleep(Duration::from_millis(50)).await;
                }
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!(listener = %context.name, "no longer accepting connections");
                    return;
                }
            }
        }
    }
}

fn is_fatal(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::BrokenPipe)
}
