// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Retry-ladder helpers shared by every leg that backs off: the `ocx package
//! push` ladder and the GitHub release listing.

use std::time::Duration;

/// `delay` spread by ±10%.
///
/// The herd this breaks up is not the one inside a run — pushes there are
/// strictly sequential — but the one across repositories: dozens of mirrors run
/// scheduled workflows against the same registry, so a rate limit or an outage
/// starts all of their ladders at the same instant and an undithered ladder
/// keeps them in lockstep for every retry after. Same ±10% default
/// go-containerregistry and oras-go ship, each despite being sequential too.
///
/// The clock's nanoseconds are the entropy. The spread only has to be
/// uncorrelated between processes, which is a far weaker property than
/// randomness, and it costs no dependency.
pub fn jitter(delay: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    delay.saturating_mul(90 + nanos % 21) / 100
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spread is a tenth and stays one: it rides on top of the cap rather
    /// than replacing it, so the capped delay lands in 27–33s and a bug that
    /// made the entropy the delay would put the ladder anywhere.
    #[test]
    fn jitter_spreads_the_delay_by_a_tenth_and_no_further() {
        // Sampled rather than asserted once: the entropy is the wall clock, so
        // a single call proves nothing about the range it can produce.
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            let spread = jitter(Duration::from_secs(30));
            assert!(
                (Duration::from_secs(27)..=Duration::from_secs(33)).contains(&spread),
                "got: {spread:?}",
            );
            seen.insert(spread);
        }
        // The range alone passes for a `jitter` that returns its argument, which
        // is the one mutation this test exists to catch. Distinctness is the
        // assertion that does not: two samples differ only if the spread is
        // actually applied.
        //
        // Not flaky. `subsec_nanos()` comes from `clock_gettime`, which has
        // nanosecond resolution, and each iteration allocates into a `BTreeSet`
        // — the loop period is neither zero nor a stable multiple of the 21-value
        // modulus, so 200 samples cannot collapse onto one bucket.
        assert!(
            seen.len() > 1,
            "one value across 200 samples — the delay is not being spread: {seen:?}",
        );
    }
}
