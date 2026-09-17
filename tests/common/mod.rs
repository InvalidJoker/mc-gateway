//! Test harness: fake Minecraft servers and a hand-written client.

#![allow(dead_code, reason = "not every test uses every helper")]

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use mc_gateway::protocol::{
    HANDSHAKE_ID, NextState, STATUS_REQUEST_ID, Writer, decode_frame, decode_frame_limited,
    encode_packet, encode_status_response,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};

/// What a fake server does with a connection.
#[derive(Debug, Clone)]
pub enum Behaviour {
    /// Record what arrives, then echo everything after it.
    Echo,
    /// Wait for the handshake and the status request, answer with this JSON,
    /// then echo (which answers the latency ping).
    Status(String),
    /// Send these bytes as soon as a client connects, then echo — a protocol
    /// where the server speaks first.
    Banner(Vec<u8>),
}

pub struct FakeServer {
    pub address: SocketAddr,
    /// The first bytes of every connection that sent something.
    received: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeServer {
    pub async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake server");
        let address = listener.local_addr().expect("local addr");
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                let behaviour = behaviour.clone();
                tokio::spawn(async move {
                    if let Behaviour::Banner(banner) = &behaviour
                        && stream.write_all(banner).await.is_err()
                    {
                        return;
                    }

                    let mut head = vec![0u8; 8192];
                    let Ok(read) = stream.read(&mut head).await else { return };
                    if read == 0 {
                        return;
                    }
                    head.truncate(read);
                    log.lock().unwrap().push(head.clone());

                    if let Behaviour::Status(json) = &behaviour {
                        let mut buf = head;
                        while !has_status_request(&buf) {
                            let mut chunk = [0u8; 4096];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        if stream.write_all(&encode_status_response(json)).await.is_err() {
                            return;
                        }
                    }

                    let mut buf = vec![0u8; 8192];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });

        Self { address, received }
    }

    /// Waits until `count` connections have sent something, then returns what.
    pub async fn wait_for(&self, count: usize) -> Vec<Vec<u8>> {
        for _ in 0..200 {
            let received = self.received.lock().unwrap().clone();
            if received.len() >= count {
                return received;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {count} connections to send something");
    }
}

fn has_status_request(buf: &[u8]) -> bool {
    let Ok(handshake) = decode_frame(buf) else { return false };
    matches!(decode_frame(&buf[handshake.total_len..]), Ok(frame) if frame.id == STATUS_REQUEST_ID)
}

/// A status document shaped like Paper's.
pub fn paper_status(motd: &str, online: u32) -> String {
    format!(
        r#"{{"version":{{"name":"Paper 1.21.1","protocol":767}},
            "players":{{"max":100,"online":{online},"sample":[{{"name":"Notch","id":"x"}}]}},
            "description":{{"text":"{motd}"}},
            "favicon":"data:image/png;base64,AAAA",
            "enforcesSecureChat":false}}"#
    )
}

pub fn handshake(next: NextState) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(767).string("node.example.net").u16(30123).varint(next.as_i32());
    encode_packet(HANDSHAKE_ID, body.as_slice())
}

pub fn login_start(name: &str) -> Vec<u8> {
    let mut body = Writer::new();
    body.string(name).bytes(&[0u8; 16]);
    encode_packet(0x00, body.as_slice())
}

/// Reads one packet, returning `(id, body)`, or `None` if the stream ends.
pub async fn read_packet(stream: &mut TcpStream) -> Option<(i32, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Ok(frame) = decode_frame_limited(&buf, 4 * 1024 * 1024) {
            return Some((frame.id, frame.body.to_vec()));
        }
        let read = time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await.ok()?.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}
