//! Client side of the status ping, used by health checks.
//!
//! A TCP connect only proves something is listening. A status ping proves the
//! server is actually answering Minecraft, and hands back its player counts and
//! version as a bonus.

use std::{io, time::Duration};

use mc_protocol::{
    Writer, encode_packet,
    frame::decode_frame_limited,
    handshake::{HANDSHAKE_ID, NextState},
    status::STATUS_REQUEST_ID,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
};

use crate::registry::StatusSnapshot;

/// Backend status responses carry favicons and player samples, so they get a
/// larger budget than anything we accept from a client.
const MAX_STATUS_RESPONSE: usize = 256 * 1024;

/// Protocol number sent by the probe.
///
/// `-1` is the conventional "not a real client" value. Servers answer status
/// pings regardless of it, and it keeps version-gating plugins from treating
/// the health check as a player trying to join with the wrong client.
const PROBE_PROTOCOL: i32 = -1;

/// Performs a full status exchange on an already-connected stream.
pub async fn status_ping(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    deadline: Duration,
) -> io::Result<StatusSnapshot> {
    let started = Instant::now();

    let mut handshake = Writer::new();
    handshake
        .varint(PROBE_PROTOCOL)
        .string(host)
        .u16(port)
        .varint(NextState::Status.as_i32());

    let mut request = encode_packet(HANDSHAKE_ID, handshake.as_slice());
    request.extend_from_slice(&encode_packet(STATUS_REQUEST_ID, &[]));

    tokio::time::timeout(deadline, async {
        stream.write_all(&request).await?;
        stream.flush().await?;

        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            match decode_frame_limited(&buf, MAX_STATUS_RESPONSE) {
                Ok(frame) => return parse_status(frame.body),
                Err(err) if err.is_incomplete() => {}
                Err(err) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("malformed status response: {err}"),
                    ));
                }
            }

            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "backend closed the connection during the status ping",
                ));
            }
            buf.extend_from_slice(&chunk[..read]);
            if buf.len() > MAX_STATUS_RESPONSE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "status response exceeds the size limit",
                ));
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "status ping timed out"))?
    .map(|mut snapshot| {
        snapshot.latency = started.elapsed();
        snapshot
    })
}

fn parse_status(body: &[u8]) -> io::Result<StatusSnapshot> {
    let mut reader = mc_protocol::Reader::new(body);
    let json = reader
        .string(MAX_STATUS_RESPONSE)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;

    Ok(StatusSnapshot {
        online: value["players"]["online"].as_i64().unwrap_or(0),
        max: value["players"]["max"].as_i64().unwrap_or(0),
        version: value["version"]["name"].as_str().unwrap_or("unknown").to_owned(),
        protocol: value["version"]["protocol"].as_i64().unwrap_or(0) as i32,
        latency: Duration::ZERO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_protocol::{frame::decode_frame, status::encode_status_response};
    use tokio::net::TcpListener;

    /// A backend that replies to a status request with `json`.
    async fn fake_backend(json: String) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let read = stream.read(&mut buf).await.unwrap();

            // The probe must send a handshake with next_state = status.
            let frame = decode_frame(&buf[..read]).unwrap();
            let handshake = mc_protocol::Handshake::decode(&frame).unwrap();
            assert_eq!(handshake.next_state, NextState::Status);
            assert_eq!(handshake.protocol_version, PROBE_PROTOCOL);

            stream.write_all(&encode_status_response(&json)).await.unwrap();
            stream.flush().await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn reads_player_counts_from_a_backend() {
        let json = r#"{"version":{"name":"Paper 1.21.1","protocol":767},
                       "players":{"max":100,"online":7},"description":{"text":"hi"}}"#;
        let addr = fake_backend(json.to_owned()).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let snapshot =
            status_ping(&mut stream, "10.0.0.1", 25565, Duration::from_secs(2)).await.unwrap();

        assert_eq!(snapshot.online, 7);
        assert_eq!(snapshot.max, 100);
        assert_eq!(snapshot.version, "Paper 1.21.1");
        assert_eq!(snapshot.protocol, 767);
    }

    #[tokio::test]
    async fn tolerates_a_sparse_status_document() {
        let addr = fake_backend("{}".to_owned()).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let snapshot =
            status_ping(&mut stream, "10.0.0.1", 25565, Duration::from_secs(2)).await.unwrap();
        assert_eq!((snapshot.online, snapshot.max), (0, 0));
        assert_eq!(snapshot.version, "unknown");
    }

    #[tokio::test]
    async fn a_silent_backend_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _accepted = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let err = status_ping(&mut stream, "10.0.0.1", 25565, Duration::from_millis(150))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn a_backend_that_hangs_up_is_an_error_not_a_hang() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let err = status_ping(&mut stream, "10.0.0.1", 25565, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            matches!(err.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset),
            "got {err:?}"
        );
    }
}
