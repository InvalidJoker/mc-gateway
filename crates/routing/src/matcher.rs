//! Hostname matching for the handshake address.
//!
//! Lookup order, first hit wins:
//!
//! 1. exact host (`survival.example.net`)
//! 2. longest matching wildcard (`*.play.example.net` beats `*.example.net`)
//! 3. the catch-all rule (`*`)
//! 4. `routing.default`
//!
//! Exact hosts are a hash lookup, so the cost does not grow with the number of
//! rules. Wildcards are scanned, but there are rarely more than a handful.

use std::collections::HashMap;

use mc_config::{Config, Rule};

#[derive(Debug)]
pub struct Matcher {
    rules: Vec<Rule>,
    exact: HashMap<String, Vec<usize>>,
    /// (suffix including the leading dot, rule index), longest suffix first.
    wildcard: Vec<(String, usize)>,
    catch_all: Vec<usize>,
    default: Option<String>,
}

/// Where a connection should go, and which rule decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteMatch<'a> {
    pub target: &'a str,
    pub rule: Option<&'a Rule>,
}

impl Matcher {
    pub fn build(config: &Config) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        let mut wildcard = Vec::new();
        let mut catch_all = Vec::new();

        for (index, rule) in config.routing.rules.iter().enumerate() {
            let host = rule.host.to_ascii_lowercase();
            if host == "*" {
                catch_all.push(index);
            } else if let Some(rest) = host.strip_prefix("*.") {
                wildcard.push((format!(".{rest}"), index));
            } else {
                exact.entry(host).or_default().push(index);
            }
        }

        // Longest suffix first, so the most specific wildcard wins.
        wildcard.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));

        Self {
            rules: config.routing.rules.clone(),
            exact,
            wildcard,
            catch_all,
            default: config.routing.default.clone(),
        }
    }

    /// Resolves `host` (already normalised by `Handshake::hostname`) for a
    /// connection that arrived on `listener`.
    pub fn match_host(&self, host: &str, listener: &str) -> Option<RouteMatch<'_>> {
        let host = host.to_ascii_lowercase();

        if let Some(indices) = self.exact.get(&host)
            && let Some(rule) = self.first_for_listener(indices, listener)
        {
            return Some(rule);
        }

        for (suffix, index) in &self.wildcard {
            // `*.example.net` covers `a.example.net` and `a.b.example.net`, and
            // also the bare `example.net` — matching what operators expect from
            // a wildcard DNS record.
            let matches = host.ends_with(suffix.as_str())
                || host == suffix.trim_start_matches('.');
            if matches
                && let Some(rule) = self.first_for_listener(std::slice::from_ref(index), listener)
            {
                return Some(rule);
            }
        }

        if let Some(rule) = self.first_for_listener(&self.catch_all, listener) {
            return Some(rule);
        }

        self.default.as_deref().map(|target| RouteMatch { target, rule: None })
    }

    fn first_for_listener(&self, indices: &[usize], listener: &str) -> Option<RouteMatch<'_>> {
        indices.iter().find_map(|&index| {
            let rule = &self.rules[index];
            let allowed = match &rule.listeners {
                Some(names) => names.iter().any(|n| n == listener),
                None => true,
            };
            allowed.then_some(RouteMatch { target: &rule.target, rule: Some(rule) })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(yaml: &str) -> Matcher {
        let config = mc_config::Config::parse(yaml, "test").unwrap().config;
        Matcher::build(&config)
    }

    const ROUTES: &str = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
  - name: internal
    bind: "127.0.0.1:25566"
routing:
  default: fallback
  rules:
    - host: "survival.example.net"
      target: survival
    - host: "*.play.example.net"
      target: play-specific
    - host: "*.example.net"
      target: play-generic
    - host: "admin.example.net"
      target: admin
      listeners: ["internal"]
servers:
  survival: { address: "10.0.0.1:25565" }
  play-specific: { address: "10.0.0.2:25565" }
  play-generic: { address: "10.0.0.3:25565" }
  admin: { address: "10.0.0.4:25565" }
  fallback: { address: "10.0.0.5:25565" }
"#;

    #[test]
    fn exact_hosts_win_over_wildcards() {
        let m = matcher(ROUTES);
        assert_eq!(m.match_host("survival.example.net", "public").unwrap().target, "survival");
    }

    #[test]
    fn the_longest_wildcard_wins() {
        let m = matcher(ROUTES);
        assert_eq!(m.match_host("eu.play.example.net", "public").unwrap().target, "play-specific");
        assert_eq!(m.match_host("creative.example.net", "public").unwrap().target, "play-generic");
    }

    #[test]
    fn wildcards_cover_the_bare_domain_and_deep_subdomains() {
        let m = matcher(ROUTES);
        assert_eq!(m.match_host("example.net", "public").unwrap().target, "play-generic");
        assert_eq!(m.match_host("a.b.c.example.net", "public").unwrap().target, "play-generic");
    }

    #[test]
    fn unmatched_hosts_fall_back_to_the_default() {
        let m = matcher(ROUTES);
        let hit = m.match_host("something.else.invalid", "public").unwrap();
        assert_eq!(hit.target, "fallback");
        assert!(hit.rule.is_none(), "the default is not a rule");
    }

    #[test]
    fn listener_restrictions_are_honoured() {
        let m = matcher(ROUTES);
        // On the public listener the admin rule is skipped and the wildcard
        // catches it instead.
        assert_eq!(m.match_host("admin.example.net", "public").unwrap().target, "play-generic");
        assert_eq!(m.match_host("admin.example.net", "internal").unwrap().target, "admin");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let m = matcher(ROUTES);
        assert_eq!(m.match_host("SURVIVAL.Example.NET", "public").unwrap().target, "survival");
    }

    #[test]
    fn without_a_default_unknown_hosts_are_refused() {
        let yaml = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  rules:
    - host: "survival.example.net"
      target: survival
servers:
  survival: { address: "10.0.0.1:25565" }
"#;
        let m = matcher(yaml);
        assert!(m.match_host("unknown.example.net", "public").is_none());
    }

    #[test]
    fn a_catch_all_rule_beats_no_default() {
        let yaml = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  rules:
    - host: "*"
      target: hub
servers:
  hub: { address: "10.0.0.1:25565" }
"#;
        let m = matcher(yaml);
        assert_eq!(m.match_host("literally.anything", "public").unwrap().target, "hub");
        assert_eq!(m.match_host("", "public").unwrap().target, "hub");
    }
}
