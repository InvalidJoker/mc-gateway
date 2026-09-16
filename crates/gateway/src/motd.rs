//! Building the status response the gateway answers with.
//!
//! Answering here instead of proxying to a backend is what makes the server
//! list independent of the backends: restarts, crashes and maintenance windows
//! all look like a normal MOTD to the outside world.

use base64::{Engine, engine::general_purpose::STANDARD};
use mc_config::{Motd, PlayerSource};
use mc_protocol::{
    chat,
    status::{PlayerInfo, PlayerSample, ServerStatus, VersionInfo},
};

/// Everything that varies per ping.
#[derive(Debug, Clone, Copy)]
pub struct MotdContext<'a> {
    /// Protocol number the client announced in its handshake.
    pub client_protocol: i32,
    /// Sessions currently proxied, for `players: sessions`.
    pub sessions: i64,
    /// Players the backends report, for `players: backends`.
    pub reported: i64,
    /// Pre-encoded `data:image/png;base64,...`.
    pub favicon: Option<&'a str>,
    /// No healthy backend for this route.
    pub offline: bool,
}

/// Protocol number that makes a client render the entry as incompatible, which
/// is the clearest way to say "this is up, that server is not".
const INCOMPATIBLE: i32 = -1;

pub fn build(motd: &Motd, ctx: MotdContext<'_>) -> ServerStatus {
    let offline = ctx.offline;
    let text = if offline { &motd.offline.text } else { &motd.text };

    let protocol = if offline && motd.offline.mark_incompatible {
        INCOMPATIBLE
    } else {
        motd.protocol.resolve(ctx.client_protocol)
    };

    let online = if offline {
        0
    } else {
        match motd.players {
            PlayerSource::Sessions => ctx.sessions,
            PlayerSource::Static => motd.online,
            PlayerSource::Backends => ctx.reported,
        }
    };

    ServerStatus {
        version: VersionInfo { name: motd.version_name.clone(), protocol },
        players: PlayerInfo {
            max: motd.max_players,
            online,
            sample: motd
                .sample
                .iter()
                .map(|line| PlayerSample::line(chat::translate_colors(line)))
                .collect(),
        },
        description: chat::component(text),
        favicon: ctx.favicon.map(str::to_owned),
        enforces_secure_chat: motd.enforces_secure_chat,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FaviconError {
    #[error("cannot read favicon {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("favicon {path} is not a PNG file")]
    NotPng { path: String },
    #[error("favicon {path} is {width}x{height}; Minecraft requires exactly 64x64")]
    WrongSize { path: String, width: u32, height: u32 },
}

/// Loads a favicon into the `data:` URL the protocol expects.
///
/// The size is checked here because a wrong one is not a visible error: clients
/// simply drop the whole status response, and the server looks offline.
pub async fn load_favicon(path: &std::path::Path) -> Result<String, FaviconError> {
    let display = path.display().to_string();
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| FaviconError::Io { path: display.clone(), source })?;

    let (width, height) = png_dimensions(&bytes)
        .ok_or_else(|| FaviconError::NotPng { path: display.clone() })?;
    if (width, height) != (64, 64) {
        return Err(FaviconError::WrongSize { path: display, width, height });
    }

    Ok(format!("data:image/png;base64,{}", STANDARD.encode(&bytes)))
}

/// Reads width and height straight out of the PNG signature and IHDR chunk.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> MotdContext<'static> {
        MotdContext {
            client_protocol: 767,
            sessions: 42,
            reported: 100,
            favicon: None,
            offline: false,
        }
    }

    #[test]
    fn auto_protocol_mirrors_the_client() {
        let motd = Motd::default();
        assert_eq!(build(&motd, context()).version.protocol, 767);

        let old_client = MotdContext { client_protocol: 47, ..context() };
        assert_eq!(build(&motd, old_client).version.protocol, 47, "1.8 sees itself as supported");
    }

    #[test]
    fn a_fixed_protocol_is_reported_verbatim() {
        let motd = Motd { protocol: mc_config::ProtocolPolicy::Fixed(767), ..Motd::default() };
        let old_client = MotdContext { client_protocol: 47, ..context() };
        assert_eq!(build(&motd, old_client).version.protocol, 767);
    }

    #[test]
    fn player_counts_follow_the_configured_source() {
        let mut motd = Motd { online: 7, ..Motd::default() };

        motd.players = PlayerSource::Sessions;
        assert_eq!(build(&motd, context()).players.online, 42);

        motd.players = PlayerSource::Static;
        assert_eq!(build(&motd, context()).players.online, 7);

        motd.players = PlayerSource::Backends;
        assert_eq!(build(&motd, context()).players.online, 100);
    }

    #[test]
    fn the_offline_motd_replaces_text_and_counts() {
        let motd = Motd { text: "online".into(), ..Motd::default() };
        let status = build(&motd, MotdContext { offline: true, ..context() });

        assert_eq!(status.players.online, 0);
        assert_eq!(status.version.protocol, INCOMPATIBLE);
        let text = status.description["text"].as_str().unwrap();
        assert!(text.contains("Currently offline"), "{text}");
    }

    #[test]
    fn offline_can_keep_the_entry_looking_compatible() {
        let motd = Motd {
            offline: mc_config::OfflineMotd {
                text: "maintenance".into(),
                mark_incompatible: false,
            },
            ..Motd::default()
        };
        let status = build(&motd, MotdContext { offline: true, ..context() });
        assert_eq!(status.version.protocol, 767);
    }

    #[test]
    fn colour_codes_are_translated_in_text_and_sample() {
        let motd = Motd {
            text: "&bNetwork".into(),
            sample: vec!["&7line one".into()],
            ..Motd::default()
        };
        let status = build(&motd, context());
        assert_eq!(status.description["text"], "\u{a7}bNetwork");
        assert_eq!(status.players.sample[0].name, "\u{a7}7line one");
    }

    #[test]
    fn the_json_survives_a_roundtrip() {
        let motd = Motd::default();
        let json: serde_json::Value =
            serde_json::from_str(&build(&motd, context()).to_json()).unwrap();
        assert_eq!(json["players"]["max"], 1000);
        assert!(json["description"]["text"].is_string());
    }

    #[tokio::test]
    async fn favicons_are_validated_before_they_break_the_ping() {
        let dir = std::env::temp_dir().join("mc-gateway-favicon-tests");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let not_png = dir.join("not.png");
        tokio::fs::write(&not_png, b"definitely not a png").await.unwrap();
        assert!(matches!(
            load_favicon(&not_png).await.unwrap_err(),
            FaviconError::NotPng { .. }
        ));

        // A syntactically valid PNG header declaring 32x32.
        let mut small = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        small.extend_from_slice(&13u32.to_be_bytes());
        small.extend_from_slice(b"IHDR");
        small.extend_from_slice(&32u32.to_be_bytes());
        small.extend_from_slice(&32u32.to_be_bytes());
        let wrong_size = dir.join("small.png");
        tokio::fs::write(&wrong_size, &small).await.unwrap();
        assert!(matches!(
            load_favicon(&wrong_size).await.unwrap_err(),
            FaviconError::WrongSize { width: 32, height: 32, .. }
        ));

        let mut correct = small.clone();
        correct[16..20].copy_from_slice(&64u32.to_be_bytes());
        correct[20..24].copy_from_slice(&64u32.to_be_bytes());
        let good = dir.join("good.png");
        tokio::fs::write(&good, &correct).await.unwrap();
        let encoded = load_favicon(&good).await.unwrap();
        assert!(encoded.starts_with("data:image/png;base64,"), "{encoded}");

        assert!(matches!(
            load_favicon(&dir.join("missing.png")).await.unwrap_err(),
            FaviconError::Io { .. }
        ));
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
