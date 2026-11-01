//! Parses the `[CODE]` / `[CODE:KIND]` prefix off a cloudpipe error
//! message.
//!
//! The prefix is produced by [`crate::error::error_prefix`]. This is the
//! inverse: it hands JS callers back the bare code string so they can
//! `switch` on it without hand-rolling a regex.

use crate::error::{CloudflareApiKind, CloudpipeErrorCode};

/// Extracts the code from a `[CODE]`-prefixed message.
///
/// Returns:
///
/// - the bare code (`"MISSING_CREDENTIAL"`) for top-level codes,
/// - the composed `PARENT:KIND` form for API errors
///   (`"CLOUDFLARE_API:RATE_LIMITED"`),
/// - the raw text for a parent/kind this build doesn't know about, so a
///   newer SDK emitting a brand-new code still round-trips instead of
///   throwing,
/// - `None` when no well-formed prefix is present.
pub fn extract_code(message: &str) -> Option<String> {
    let raw = message.strip_prefix('[')?;
    let end = raw.find(']')?;
    let inner = &raw[..end];
    let mut parts = inner.splitn(2, ':');
    let parent = parts.next()?;

    // Resolve against the typed enum rather than a list of string
    // literals: the enum's `as_str` is the only place a code is spelled
    // out, so the parser and the producer cannot drift apart.
    if CloudpipeErrorCode::parse(parent).is_none() {
        // Unknown parent — return the raw text verbatim, sub-kind included.
        // We deliberately don't fail so forward-compat works both ways: a
        // newer SDK emitting an unknown code gives an older JS helper the
        // whole string back, not an exception and not a truncated prefix.
        return Some(match parts.next() {
            Some(suffix) => format!("{parent}:{suffix}"),
            None => parent.to_string(),
        });
    }

    let Some(suffix) = parts.next() else {
        return Some(parent.to_string());
    };
    if CloudflareApiKind::parse(suffix).is_none() {
        return Some(format!("{parent}:{suffix}"));
    }
    Some(format!("{parent}:{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_code() {
        assert_eq!(
            extract_code("[MISSING_CREDENTIAL] need a token"),
            Some("MISSING_CREDENTIAL".to_string())
        );
    }

    #[test]
    fn parses_compound_kind() {
        assert_eq!(
            extract_code("[CLOUDFLARE_API:RATE_LIMITED] too many"),
            Some("CLOUDFLARE_API:RATE_LIMITED".to_string())
        );
    }

    #[test]
    fn returns_none_on_plain_message() {
        assert_eq!(extract_code("no prefix here"), None);
    }

    #[test]
    fn returns_none_on_malformed_prefix() {
        assert_eq!(extract_code("[not closed here"), None);
    }

    #[test]
    fn returns_none_on_empty_message() {
        assert_eq!(extract_code(""), None);
    }

    #[test]
    fn unknown_code_passes_through() {
        // Forward-compat: a future SDK version emits a brand-new code
        // that this build doesn't know about -> return it verbatim
        // instead of throwing.
        assert_eq!(
            extract_code("[FUTURE_CODE] blah"),
            Some("FUTURE_CODE".to_string())
        );
        assert_eq!(
            extract_code("[FUTURE_CODE:NEXT_KIND] blah"),
            Some("FUTURE_CODE:NEXT_KIND".to_string())
        );
    }

    #[test]
    fn known_parent_with_unknown_subkind_passes_through() {
        assert_eq!(
            extract_code("[CLOUDFLARE_API:RATE_LIMITED_EXTENDED] blah"),
            Some("CLOUDFLARE_API:RATE_LIMITED_EXTENDED".to_string())
        );
    }

    #[test]
    fn every_documented_code_round_trips() {
        let codes = [
            "[MISSING_CREDENTIAL] x",
            "[CLOUDFLARE_API] x",
            "[CLOUDFLARE_API:RATE_LIMITED] x",
            "[CLOUDFLARE_API:DNS_EXISTS] x",
            "[CLOUDFLARE_API:AUTH_FAILED] x",
            "[CLOUDFLARE_API:INVALID_TOKEN] x",
            "[CLOUDFLARE_API:OTHER] x",
            "[CLOUDFLARE_BINARY] x",
            "[INVALID_SUBDOMAIN] x",
            "[INVALID_DOMAIN] x",
            "[SUBDOMAIN_IN_USE] x",
            "[ALREADY_SHUT_DOWN] x",
            "[IO] x",
            "[INVALID_ARGUMENT] x",
            "[OTHER] x",
        ];
        for input in codes {
            let expected = &input[1..input.find(']').unwrap()];
            assert_eq!(
                extract_code(input).as_deref(),
                Some(expected),
                "round-trip failed for {input}"
            );
        }
    }
}
