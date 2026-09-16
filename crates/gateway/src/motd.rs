//! MOTD rewriting.
//!
//! The gateway does not build a status response. It asks the backend and hands
//! the answer back — version, player counts, sample and favicon included. The
//! only thing it changes is the MOTD text, and only the lines the operator
//! named.
//!
//! Doing it this way means a backend's real player count, its version string
//! and its icon all keep working with no configuration, and a new field in some
//! future protocol version passes through without the gateway knowing about it.

use mc_config::Motd;
use mc_protocol::chat;
use serde_json::{Value, json};

/// Protocol number that makes a client render the entry as incompatible, which
/// is the clearest way to say "the gateway is up, that server is not".
const INCOMPATIBLE: i32 = -1;

/// Replaces the configured lines in a status response document.
///
/// Returns `None` when the document is not JSON or carries no description, in
/// which case the caller forwards the original bytes unchanged — a response the
/// gateway cannot understand is still a response the client might.
pub fn rewrite_status(json: &str, motd: &Motd) -> Option<String> {
    let mut document: Value = serde_json::from_str(json).ok()?;
    let description = document.get("description")?;
    let rewritten = rewrite_description(description, motd);
    document["description"] = rewritten;
    serde_json::to_string(&document).ok()
}

/// Replaces line 1 and/or line 2 of a chat component.
///
/// The component tree is flattened to a legacy `§`-coded string first. A MOTD
/// is two lines of coloured text; a component tree is an arbitrary nesting of
/// spans, and there is no meaningful way to say "the second line" inside one
/// without flattening it first.
pub fn rewrite_description(description: &Value, motd: &Motd) -> Value {
    let flattened = chat::to_legacy(description);
    let mut lines: Vec<String> = flattened.split('\n').map(str::to_owned).collect();

    for (index, replacement) in [(0, &motd.line1), (1, &motd.line2)] {
        let Some(text) = replacement else { continue };
        let text = chat::translate_colors(text);
        // A backend with a one-line MOTD still gets a second line if one is
        // configured.
        while lines.len() <= index {
            lines.push(String::new());
        }
        lines[index] = text;
    }

    json!({ "text": lines.join("\n") })
}

/// The one response the gateway has to invent: there is no backend to ask.
pub fn offline_status(motd: &Motd, client_protocol: i32) -> String {
    let offline = &motd.offline;
    let protocol = if offline.mark_incompatible { INCOMPATIBLE } else { client_protocol };

    json!({
        "version": { "name": offline.version_name, "protocol": protocol },
        "players": { "max": offline.max_players, "online": 0 },
        "description": { "text": chat::translate_colors(&offline.text) },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_config::OfflineMotd;

    /// What Paper actually sends.
    const BACKEND: &str = r#"{
        "version": {"name": "Paper 1.21.1", "protocol": 767},
        "players": {"max": 100, "online": 7, "sample": [{"name": "Notch", "id": "x"}]},
        "description": {"extra": [
            {"text": "A Minecraft Server", "color": "white"},
            {"text": "\n"},
            {"text": "powered by Paper", "color": "gray"}
        ], "text": ""},
        "favicon": "data:image/png;base64,AAAA",
        "enforcesSecureChat": false
    }"#;

    fn motd(line1: Option<&str>, line2: Option<&str>) -> Motd {
        Motd {
            line1: line1.map(str::to_owned),
            line2: line2.map(str::to_owned),
            offline: OfflineMotd::default(),
        }
    }

    fn description_text(json: &str) -> String {
        let value: Value = serde_json::from_str(json).unwrap();
        value["description"]["text"].as_str().unwrap().to_owned()
    }

    #[test]
    fn replacing_the_second_line_keeps_the_first() {
        let out = rewrite_status(BACKEND, &motd(None, Some("&7my network"))).unwrap();
        let text = description_text(&out);
        let lines: Vec<&str> = text.split('\n').collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("A Minecraft Server"), "{:?}", lines[0]);
        assert_eq!(lines[1], "\u{a7}7my network");
    }

    #[test]
    fn everything_but_the_description_passes_through() {
        let out = rewrite_status(BACKEND, &motd(None, Some("&7my network"))).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();

        // The backend's own numbers, icon and version survive untouched — the
        // whole point of rewriting instead of synthesising.
        assert_eq!(value["version"]["name"], "Paper 1.21.1");
        assert_eq!(value["version"]["protocol"], 767);
        assert_eq!(value["players"]["online"], 7);
        assert_eq!(value["players"]["max"], 100);
        assert_eq!(value["players"]["sample"][0]["name"], "Notch");
        assert_eq!(value["favicon"], "data:image/png;base64,AAAA");
        assert_eq!(value["enforcesSecureChat"], false);
    }

    #[test]
    fn both_lines_can_be_taken_over() {
        let out = rewrite_status(BACKEND, &motd(Some("&b&lMY NETWORK"), Some("&7second"))).unwrap();
        assert_eq!(
            description_text(&out),
            "\u{a7}b\u{a7}lMY NETWORK\n\u{a7}7second"
        );
    }

    #[test]
    fn the_original_colours_are_preserved_when_a_line_is_kept() {
        let backend = r#"{"description":{"text":"first","color":"gold"}}"#;
        let out = rewrite_status(backend, &motd(None, Some("&7second"))).unwrap();
        let text = description_text(&out);
        assert!(text.starts_with("\u{a7}r\u{a7}6first"), "{text:?}");
    }

    #[test]
    fn a_one_line_backend_motd_gains_a_second_line() {
        let backend = r#"{"description":{"text":"only one line"}}"#;
        let out = rewrite_status(backend, &motd(None, Some("&7added"))).unwrap();
        assert_eq!(description_text(&out), "only one line\n\u{a7}7added");
    }

    #[test]
    fn a_plain_string_description_is_handled() {
        let backend = r#"{"description":"legacy string\nsecond"}"#;
        let out = rewrite_status(backend, &motd(None, Some("&7replaced"))).unwrap();
        assert_eq!(description_text(&out), "legacy string\n\u{a7}7replaced");
    }

    #[test]
    fn nothing_configured_still_produces_a_faithful_document() {
        let out = rewrite_status(BACKEND, &motd(None, None)).unwrap();
        let text = description_text(&out);
        assert!(text.contains("A Minecraft Server"));
        assert!(text.contains("powered by Paper"));
    }

    #[test]
    fn a_response_that_is_not_json_is_left_to_the_client() {
        assert_eq!(rewrite_status("not json at all", &motd(None, Some("x"))), None);
        assert_eq!(rewrite_status(r#"{"players":{}}"#, &motd(None, Some("x"))), None);
    }

    #[test]
    fn the_offline_response_is_a_complete_status_document() {
        let motd = Motd {
            offline: OfflineMotd {
                text: "&cMaintenance".into(),
                version_name: "offline".into(),
                max_players: 0,
                mark_incompatible: true,
            },
            ..motd(None, None)
        };
        let value: Value = serde_json::from_str(&offline_status(&motd, 767)).unwrap();

        assert_eq!(value["description"]["text"], "\u{a7}cMaintenance");
        assert_eq!(value["version"]["protocol"], INCOMPATIBLE);
        assert_eq!(value["players"]["online"], 0);
    }

    #[test]
    fn an_offline_entry_can_stay_looking_compatible() {
        let mut motd = motd(None, None);
        motd.offline.mark_incompatible = false;
        let value: Value = serde_json::from_str(&offline_status(&motd, 767)).unwrap();
        assert_eq!(value["version"]["protocol"], 767);
    }
}
