//! Chat components and legacy `§` colour codes, as far as MOTD lines need them.

use serde_json::Value;

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

/// Strips `§` formatting codes.
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

/// Flattens a chat component into a legacy `§`-coded string.
///
/// This is how a backend's MOTD becomes editable: component trees can nest
/// arbitrarily, but a MOTD is really just two lines of coloured text, and a
/// legacy string is a shape that can be split on `\n` and put back together.
/// Every client understands it, including 1.7.
pub fn to_legacy(value: &Value) -> String {
    let mut out = String::new();
    let mut emitted: Option<Style> = None;
    walk(value, Style::default(), &mut out, &mut emitted);
    out
}

/// The formatting state a component carries, after inheritance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Style {
    /// Already-encoded colour code: `"a"`, or `"x\u{a7}0\u{a7}0…"` for hex.
    color: Option<String>,
    bold: bool,
    italic: bool,
    underlined: bool,
    strikethrough: bool,
    obfuscated: bool,
}

impl Style {
    /// Applies a component's own fields on top of what it inherited.
    fn inherit(&self, object: &serde_json::Map<String, Value>) -> Style {
        let flag = |name: &str, current: bool| object.get(name).and_then(Value::as_bool).unwrap_or(current);
        Style {
            color: object
                .get("color")
                .and_then(Value::as_str)
                .and_then(color_code)
                .or_else(|| self.color.clone()),
            bold: flag("bold", self.bold),
            italic: flag("italic", self.italic),
            underlined: flag("underlined", self.underlined),
            strikethrough: flag("strikethrough", self.strikethrough),
            obfuscated: flag("obfuscated", self.obfuscated),
        }
    }

    fn write_codes(&self, out: &mut String) {
        // `§r` first, because legacy codes are stateful: without a reset the
        // previous segment's bold would bleed into this one.
        out.push(SECTION);
        out.push('r');
        if let Some(color) = &self.color {
            out.push(SECTION);
            out.push_str(color);
        }
        for (enabled, code) in [
            (self.bold, 'l'),
            (self.italic, 'o'),
            (self.underlined, 'n'),
            (self.strikethrough, 'm'),
            (self.obfuscated, 'k'),
        ] {
            if enabled {
                out.push(SECTION);
                out.push(code);
            }
        }
    }
}

fn walk(value: &Value, inherited: Style, out: &mut String, emitted: &mut Option<Style>) {
    match value {
        Value::String(text) => push_text(text, &inherited, out, emitted),
        Value::Array(items) => {
            // In an array the first element styles the rest.
            let mut style = inherited;
            for (index, item) in items.iter().enumerate() {
                walk(item, style.clone(), out, emitted);
                if index == 0
                    && let Value::Object(object) = item
                {
                    style = style.inherit(object);
                }
            }
        }
        Value::Object(object) => {
            let style = inherited.inherit(object);
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                push_text(text, &style, out, emitted);
            }
            // A translate component without a resolver is better skipped than
            // rendered as its key; the `with` arguments are still real text.
            if let Some(Value::Array(args)) = object.get("with") {
                for arg in args {
                    walk(arg, style.clone(), out, emitted);
                }
            }
            if let Some(Value::Array(extra)) = object.get("extra") {
                for item in extra {
                    walk(item, style.clone(), out, emitted);
                }
            }
        }
        _ => {}
    }
}

fn push_text(text: &str, style: &Style, out: &mut String, emitted: &mut Option<Style>) {
    if text.is_empty() {
        return;
    }
    let unchanged = emitted.as_ref() == Some(style);
    let default_start = emitted.is_none() && *style == Style::default();
    if !unchanged && !default_start {
        style.write_codes(out);
    }
    *emitted = Some(style.clone());
    out.push_str(text);
}

/// Named colours and the 1.16+ hex form.
fn color_code(name: &str) -> Option<String> {
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
        && hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        let mut code = String::from("x");
        for digit in hex.chars() {
            code.push(SECTION);
            code.push(digit);
        }
        return Some(code);
    }

    Some(
        match name {
            "black" => "0",
            "dark_blue" => "1",
            "dark_green" => "2",
            "dark_aqua" => "3",
            "dark_red" => "4",
            "dark_purple" => "5",
            "gold" => "6",
            "gray" | "grey" => "7",
            "dark_gray" | "dark_grey" => "8",
            "blue" => "9",
            "green" => "a",
            "aqua" => "b",
            "red" => "c",
            "light_purple" => "d",
            "yellow" => "e",
            "white" => "f",
            _ => return None,
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn a_plain_component_needs_no_codes() {
        assert_eq!(to_legacy(&json!({"text": "A Minecraft Server"})), "A Minecraft Server");
        assert_eq!(to_legacy(&json!("bare string")), "bare string");
    }

    #[test]
    fn named_colours_and_formats_become_legacy_codes() {
        let value = json!({"text": "Hello", "color": "gold", "bold": true});
        assert_eq!(to_legacy(&value), "\u{a7}r\u{a7}6\u{a7}lHello");
    }

    #[test]
    fn hex_colours_use_the_1_16_form() {
        let value = json!({"text": "blue", "color": "#00AAFF"});
        assert_eq!(
            to_legacy(&value),
            "\u{a7}r\u{a7}x\u{a7}0\u{a7}0\u{a7}A\u{a7}A\u{a7}F\u{a7}Fblue"
        );
    }

    #[test]
    fn children_inherit_the_parent_style() {
        let value = json!({
            "text": "A",
            "color": "red",
            "extra": [{"text": "B"}, {"text": "C", "color": "green"}]
        });
        // B keeps red from its parent; C switches to green.
        assert_eq!(to_legacy(&value), "\u{a7}r\u{a7}cAB\u{a7}r\u{a7}aC");
    }

    #[test]
    fn a_reset_precedes_every_style_change() {
        let value = json!({"extra": [
            {"text": "bold", "bold": true},
            {"text": "plain"}
        ]});
        // Without the reset, "plain" would still be bold on the client.
        assert_eq!(to_legacy(&value), "\u{a7}r\u{a7}lbold\u{a7}rplain");
    }

    #[test]
    fn newlines_survive_so_lines_stay_splittable() {
        let value = json!({"text": "first\nsecond", "color": "gray"});
        let legacy = to_legacy(&value);
        assert_eq!(legacy.lines().count(), 2);
        assert_eq!(strip_formatting(&legacy), "first\nsecond");
    }

    #[test]
    fn a_real_backend_motd_roundtrips_to_plain_text() {
        // The shape vanilla and Paper actually send.
        let value = json!({
            "extra": [
                {"text": "A Minecraft Server", "color": "white"},
                {"text": "\n"},
                {"text": "powered by Paper", "color": "gray", "italic": true}
            ],
            "text": ""
        });
        assert_eq!(
            strip_formatting(&to_legacy(&value)),
            "A Minecraft Server\npowered by Paper"
        );
    }
}
