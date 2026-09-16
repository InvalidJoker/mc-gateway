//! Human-readable durations in the config file (`10s`, `500ms`, `2m`).

use std::{fmt, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Wrapper that deserialises from `"10s"`, `"1m30s"` or a plain number of
/// seconds, and serialises back to the compact string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self(Duration::from_millis(ms))
    }

    pub const fn get(self) -> Duration {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl From<HumanDuration> for Duration {
    fn from(value: HumanDuration) -> Self {
        value.0
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        if ms == 0 {
            f.write_str("0s")
        } else if ms % 60_000 == 0 {
            write!(f, "{}m", ms / 60_000)
        } else if ms % 1_000 == 0 {
            write!(f, "{}s", ms / 1_000)
        } else {
            write!(f, "{ms}ms")
        }
    }
}

pub fn parse(input: &str) -> Result<Duration, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty duration".into());
    }

    let mut total = Duration::ZERO;
    let mut number = String::new();
    let mut unit = String::new();
    let mut saw_component = false;

    let flush = |number: &mut String, unit: &mut String, total: &mut Duration| -> Result<(), String> {
        if number.is_empty() {
            return Err(format!("missing number before unit `{unit}`"));
        }
        let value: u64 = number.parse().map_err(|_| format!("invalid number `{number}`"))?;
        let scaled = match unit.as_str() {
            "ms" => Duration::from_millis(value),
            "s" | "" => Duration::from_secs(value),
            "m" => Duration::from_secs(value * 60),
            "h" => Duration::from_secs(value * 3600),
            "d" => Duration::from_secs(value * 86400),
            other => return Err(format!("unknown unit `{other}`, expected ms, s, m, h or d")),
        };
        *total += scaled;
        number.clear();
        unit.clear();
        Ok(())
    };

    for c in input.chars() {
        if c.is_ascii_digit() {
            if !unit.is_empty() {
                flush(&mut number, &mut unit, &mut total)?;
            }
            number.push(c);
            saw_component = true;
        } else if c.is_ascii_alphabetic() {
            unit.push(c);
        } else if c.is_whitespace() {
            continue;
        } else {
            return Err(format!("unexpected character `{c}` in duration"));
        }
    }

    if !saw_component {
        return Err(format!("`{input}` is not a duration"));
    }
    flush(&mut number, &mut unit, &mut total)?;
    Ok(total)
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration such as `10s`, `500ms` or `2m`")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                parse(value).map(HumanDuration).map_err(E::custom)
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(HumanDuration::from_secs(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                u64::try_from(value)
                    .map(HumanDuration::from_secs)
                    .map_err(|_| E::custom("duration must not be negative"))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                if value < 0.0 {
                    return Err(E::custom("duration must not be negative"));
                }
                Ok(HumanDuration(Duration::from_secs_f64(value)))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_forms() {
        assert_eq!(parse("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse("30").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse("").is_err());
        assert!(parse("soon").is_err());
        assert!(parse("10x").is_err());
        assert!(parse("-5s").is_err());
    }

    #[test]
    fn roundtrips_through_yaml() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Holder {
            t: HumanDuration,
        }
        let parsed: Holder = serde_yaml_ng::from_str("t: 1m30s").unwrap();
        assert_eq!(parsed.t.get(), Duration::from_secs(90));
        assert_eq!(serde_yaml_ng::to_string(&parsed).unwrap().trim(), "t: 90s");

        let numeric: Holder = serde_yaml_ng::from_str("t: 7").unwrap();
        assert_eq!(numeric.t.get(), Duration::from_secs(7));
    }
}
