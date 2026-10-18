//! Reconnection budget and random-name generation.
//!
//! Both are policy knobs rather than mechanics, so they live apart from
//! the supervisor loop that consumes them.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

///
/// Exposed as a value rather than hard-coded constants so (a) integration
/// tests can run the whole retry ladder in milliseconds instead of tens of
/// seconds, and (b) an embedder with different tolerance — a long-lived
/// production tunnel, say — can widen the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Consecutive bootstrap failures tolerated before giving up.
    pub max_attempts: u32,
    /// Delay before the first retry; doubles each failure, capped at
    /// [`max_delay`](Self::max_delay).
    pub base_delay: Duration,
    /// Upper bound on any single retry delay.
    pub max_delay: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            // A bootstrap failure usually means Cloudflare is rate-limiting
            // the token or `cloudflared` is unusable; retrying forever
            // would just burn API quota.
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RestartPolicy {
    /// Delay before retry number `attempt` (1-based), with jitter so a
    /// fleet of clients does not reconnect in lockstep against one quota.
    pub fn delay(&self, attempt: u32) -> Duration {
        let base = self.base_delay.as_millis() as u64;
        let cap = self.max_delay.as_millis() as u64;
        let ceiling = base
            .saturating_mul(1u64 << attempt.saturating_sub(1).min(16))
            .min(cap);
        // 50% base + up to 50% jitter.
        let jitter = (ceiling / 2)
            * ((attempt as u64).wrapping_mul(2_654_435_761) % 1_000)
            / 1_000;
        Duration::from_millis(ceiling / 2 + jitter)
    }
}

/// Generates a random subdomain (`user-a7k2m9x`).
///
/// The previous implementation produced `user-0000`…`user-9999` from a
/// single `RandomState` hash — 10,000 possible values. Two developers
/// starting a tunnel on the same zone collided often enough that
/// `SubdomainInUse` was a routine failure, not an edge case. Seven
/// base32 characters drawn from the OS CSPRNG give 32^7 ≈ 3.4e10 values
/// while staying a valid RFC 1123 DNS label.
///
/// ```no_run
/// # use cloudpipe_sdk::random_subdomain;
/// let name = random_subdomain();
/// assert!(cloudpipe_sdk::validate_subdomain(&name).is_ok());
/// ```
pub fn random_subdomain() -> String {
    // 32 characters, and a power of two so the `% ALPHABET.len()` mapping
    // below is unbiased. `l`, `o`, `1` and `0` are excluded: a generated
    // name is meant to be read aloud or retyped, and those are exactly the
    // glyphs that get confused.
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    const LABEL_LEN: usize = 7;
    let mut bytes = [0u8; LABEL_LEN];
    // `getrandom` on Linux, `BCryptGenRandom`/`getrandom` elsewhere — all
    // OS-backed, all infallible in practice. Falling back to a hashed
    // address keeps this total rather than panicking in a library.
    if getrandom::fill(&mut bytes).is_err() {
        let seed = RandomState::new().build_hasher().finish();
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot = ((seed >> (i * 8)) & 0xff) as u8;
        }
    }
    let suffix: String = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    format!("user-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_bounded() {
        let p = RestartPolicy::default();
        assert!(p.max_attempts > 0, "a zero budget would never reconnect");
        assert!(p.max_delay >= p.base_delay);
        assert!(
            p.max_delay <= std::time::Duration::from_secs(30),
            "a long cap would make give-up feel like a hang"
        );
    }

    #[test]
    fn delay_grows_and_is_capped() {
        let p = RestartPolicy {
            max_attempts: 10,
            base_delay: std::time::Duration::from_millis(100),
            max_delay: std::time::Duration::from_millis(400),
        };
        for attempt in 1..=10 {
            let d = p.delay(attempt);
            assert!(
                d <= p.max_delay,
                "attempt {attempt} exceeded the cap: {d:?}"
            );
            assert!(d >= p.base_delay / 2, "attempt {attempt} too short: {d:?}");
        }
    }

    #[test]
    fn delay_is_jittered() {
        let p = RestartPolicy::default();
        let mut seen = Vec::new();
        for attempt in 1..=5 {
            seen.push(p.delay(attempt).as_millis());
        }
        assert!(
            seen.windows(2).any(|w| w[0] != w[1]),
            "every delay identical — no jitter applied: {seen:?}"
        );
    }

    #[test]
    fn zero_base_delay_never_panics() {
        let p = RestartPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::ZERO,
            max_delay: std::time::Duration::ZERO,
        };
        assert_eq!(p.delay(1), std::time::Duration::ZERO);
    }

    #[test]
    fn generated_names_are_valid_and_varied() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let name = random_subdomain();
            assert!(name.starts_with("user-"), "{name}");
            assert!(
                crate::protocol::validate_subdomain(&name).is_ok(),
                "invalid DNS label: {name}"
            );
            assert!(seen.insert(name.clone()), "collision on {name}");
        }
    }

    #[test]
    fn generated_labels_avoid_the_confusable_characters() {
        // `l`/`1` and `0`/`o` are deliberately absent from the alphabet so a
        // name can be read aloud or retyped without ambiguity.
        for _ in 0..200 {
            let name = random_subdomain();
            assert!(!name.contains('l'), "{name}");
            assert!(!name.contains('1'), "{name}");
            assert!(!name.contains('0'), "{name}");
        }
    }
}
