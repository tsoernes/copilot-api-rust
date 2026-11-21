//! Derive the usage session from the same client headers as the upstream gateway.
//!
//! An absent/blank header is skipped; an invalid or oversized higher-priority
//! value fails closed to the caller's request-specific fallback. In particular,
//! values are never truncated, because that could merge unrelated sessions.
use axum::http::HeaderMap;

const MAX_SESSION_CHARS: usize = 256;

enum Candidate<'a> {
    Missing,
    Invalid,
    Value(&'a str),
}

fn usable(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_SESSION_CHARS
        && value.chars().all(|c| {
            !c.is_control()
                && !matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
        })
}

fn candidate<'a>(headers: &'a HeaderMap, name: &str) -> Candidate<'a> {
    let Some(value) = headers.get(name) else {
        return Candidate::Missing;
    };
    let Ok(value) = std::str::from_utf8(value.as_bytes()) else {
        return Candidate::Invalid;
    };
    let value = value.trim();
    if value.is_empty() {
        Candidate::Missing
    } else if usable(value) {
        Candidate::Value(value)
    } else {
        Candidate::Invalid
    }
}

/// Usage affinity wins over the root session, matching upstream attribution.
/// The fallback is expected to be the gateway's fresh per-request identifier;
/// it is not evidence that unrelated requests belong to the same session.
pub(crate) fn resolve(headers: &HeaderMap, fallback: &str) -> String {
    let fallback = fallback.trim();
    let fallback = if usable(fallback) { fallback } else { "" };
    match candidate(headers, "x-claude-code-session-id") {
        Candidate::Invalid => return fallback.into(),
        Candidate::Value(root) => {
            return match candidate(headers, "x-claude-code-agent-id") {
                Candidate::Missing => root.into(),
                Candidate::Invalid => fallback.into(),
                Candidate::Value(agent) => {
                    let combined = format!("{root}:{agent}");
                    if usable(&combined) {
                        combined
                    } else {
                        fallback.into()
                    }
                }
            };
        }
        Candidate::Missing => {}
    }
    for name in [
        "x-opencode-session-id",
        "x-session-affinity",
        "x-client-request-id",
        "thread-id",
        "session-id",
        "x-root-session-id",
        "x-session-id",
    ] {
        match candidate(headers, name) {
            Candidate::Missing => {}
            Candidate::Invalid => return fallback.into(),
            Candidate::Value(value) => return value.into(),
        }
    }
    fallback.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for &(name, value) in entries {
            map.insert(
                name.parse::<axum::http::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn prefers_upstream_affinity_over_root() {
        let map = headers(&[
            ("session-id", "root"),
            ("x-opencode-session-id", "  leaf  "),
            ("x-session-affinity", "other"),
            ("thread-id", "thread"),
        ]);
        assert_eq!(resolve(&map, "request"), "leaf");
        let map = headers(&[
            ("session-id", "root"),
            ("x-session-affinity", "affinity"),
            ("x-client-request-id", "client"),
            ("thread-id", "thread"),
        ]);
        assert_eq!(resolve(&map, "request"), "affinity");
        let map = headers(&[
            ("session-id", "root"),
            ("x-client-request-id", "client"),
            ("thread-id", "thread"),
        ]);
        assert_eq!(resolve(&map, "request"), "client");
        let map = headers(&[("session-id", "root"), ("thread-id", "thread")]);
        assert_eq!(resolve(&map, "request"), "thread");
    }

    #[test]
    fn codex_root_and_claude_agents_are_grouped_as_upstream() {
        assert_eq!(
            resolve(&headers(&[("session-id", "  codex-session  ")]), "request"),
            "codex-session"
        );
        let map = headers(&[
            ("x-claude-code-session-id", " claude-root "),
            ("x-claude-code-agent-id", " agent-1 "),
            ("x-opencode-session-id", "other"),
        ]);
        assert_eq!(resolve(&map, "request"), "claude-root:agent-1");
        let map = headers(&[
            ("x-claude-code-session-id", "claude-root"),
            ("x-claude-code-agent-id", "   "),
        ]);
        assert_eq!(resolve(&map, "request"), "claude-root");
    }

    #[test]
    fn root_priority_blank_and_fallback_are_explicit() {
        let map = headers(&[
            ("session-id", " "),
            ("x-root-session-id", " root "),
            ("x-session-id", "child"),
        ]);
        assert_eq!(resolve(&map, "request"), "root");
        assert_eq!(
            resolve(&headers(&[("x-session-id", "child")]), "request"),
            "child"
        );
        assert_eq!(resolve(&HeaderMap::new(), " request-1 "), "request-1");
        assert_eq!(resolve(&HeaderMap::new(), ""), "");
    }

    #[test]
    fn oversize_or_invalid_is_never_silently_truncated_or_merged() {
        let long = "a".repeat(257);
        let map = headers(&[("x-opencode-session-id", &long), ("session-id", "weaker")]);
        assert_eq!(resolve(&map, "request-1"), "request-1");
        let root = "r".repeat(240);
        let agent = "a".repeat(20);
        assert_eq!(
            resolve(
                &headers(&[
                    ("x-claude-code-session-id", &root),
                    ("x-claude-code-agent-id", &agent)
                ]),
                "request-2"
            ),
            "request-2"
        );
        let mut map = HeaderMap::new();
        map.insert(
            "session-id",
            HeaderValue::from_bytes(b"not-utf8-\xff").unwrap(),
        );
        assert_eq!(resolve(&map, "request-3"), "request-3");
        let map = headers(&[("session-id", "hi\tthere")]);
        assert_eq!(resolve(&map, "request-4"), "request-4");
    }

    #[test]
    fn unicode_is_preserved_if_printable_and_within_limit() {
        let mut map = HeaderMap::new();
        map.insert(
            "session-id",
            HeaderValue::from_bytes("søk-økt".as_bytes()).unwrap(),
        );
        assert_eq!(resolve(&map, "request"), "søk-økt");
        let value = "ø".repeat(256);
        map.insert(
            "session-id",
            HeaderValue::from_bytes(value.as_bytes()).unwrap(),
        );
        assert_eq!(resolve(&map, "request"), value);
        let value = "ø".repeat(257);
        map.insert(
            "session-id",
            HeaderValue::from_bytes(value.as_bytes()).unwrap(),
        );
        assert_eq!(resolve(&map, "request"), "request");
    }
}
