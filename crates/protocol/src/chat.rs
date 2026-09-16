//! Just enough chat-component handling to render a configured MOTD.

use serde_json::{Value, json};

/// Section sign used by the legacy colour codes.
pub const SECTION: char = '\u{00a7}';

const VALID_CODES: &str = "0123456789abcdefklmnorABCDEFKLMNOR";

/// Translates `&`-prefixed colour codes into the `§` codes the client expects,
/// including `&#rrggbb` for the 1.16+ hex palette.
///
/// `&&` is an escape for a literal ampersand.
pub fn translate_colors(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('&') => {
                chars.next();
                out.push('&');
            }
            Some('#') => {
                let hex: String = chars.clone().skip(1).take(6).collect();
                if hex.len() == 6 && hex.chars().all(|h| h.is_ascii_hexdigit()) {
                    for _ in 0..7 {
                        chars.next();
                    }
                    out.push(SECTION);
                    out.push('x');
                    for h in hex.chars() {
                        out.push(SECTION);
                        out.push(h);
                    }
                } else {
                    out.push('&');
                }
            }
            Some(code) if VALID_CODES.contains(code) => {
                chars.next();
                out.push(SECTION);
                out.push(code);
            }
            _ => out.push('&'),
        }
    }

    out
}

/// Builds a chat component from configured text.
///
/// A value that already looks like JSON is passed through untouched, so anyone
/// who wants full component control (hover events, gradients rendered by a
/// generator) can paste raw JSON into the config.
pub fn component(text: &str) -> Value {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            return value;
        }
    }
    json!({ "text": translate_colors(text) })
}

/// Strips formatting, for the legacy ping response and for logging.
pub fn strip_formatting(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == SECTION {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// Flattens a chat component back to plain text (legacy ping has no JSON).
pub fn component_to_plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(component_to_plain).collect(),
        Value::Object(map) => {
            let mut out = String::new();
            if let Some(Value::String(text)) = map.get("text") {
                out.push_str(text);
            }
            if let Some(Value::Array(extra)) = map.get("extra") {
                for item in extra {
                    out.push_str(&component_to_plain(item));
                }
            }
            out
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_legacy_codes() {
        assert_eq!(translate_colors("&aHello &lWorld"), "\u{a7}aHello \u{a7}lWorld");
        assert_eq!(translate_colors("100&& counted"), "100& counted");
        assert_eq!(translate_colors("&zunknown"), "&zunknown");
    }

    #[test]
    fn translates_hex_codes() {
        assert_eq!(
            translate_colors("&#00AAFFblue"),
            "\u{a7}x\u{a7}0\u{a7}0\u{a7}A\u{a7}A\u{a7}F\u{a7}Fblue"
        );
        assert_eq!(translate_colors("&#xyzzyy nope"), "&#xyzzyy nope");
    }

    #[test]
    fn passes_raw_json_through() {
        let value = component(r#"{"text":"hi","bold":true}"#);
        assert_eq!(value["bold"], true);
        assert_eq!(value["text"], "hi");
    }

    #[test]
    fn wraps_plain_text() {
        assert_eq!(component("&ahi")["text"], "\u{a7}ahi");
    }

    #[test]
    fn flattens_components_for_legacy_clients() {
        let value = serde_json::json!({"text": "A", "extra": [{"text": "B"}, "C"]});
        assert_eq!(component_to_plain(&value), "ABC");
        assert_eq!(strip_formatting("\u{a7}aGreen"), "Green");
    }
}
