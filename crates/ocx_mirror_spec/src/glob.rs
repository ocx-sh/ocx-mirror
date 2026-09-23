// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Package-name globs for `include:` / `exclude:` (C-009, C-010).
//!
//! Grammar is literal characters plus `*`, and nothing else. Built on the
//! already-direct `regex` dependency by escaping every literal run and joining
//! on `.*` — the ADR's "no regex anywhere in the schema" governs the spec
//! surface, not the implementation, and a glob crate would spend a dependency
//! on twelve lines.

/// A compiled `include:` / `exclude:` pattern (C-009).
#[derive(Debug, Clone)]
pub struct Glob {
    /// The anchored translation of the pattern as written. `Regex`'s own
    /// `Debug` prints its source, so the original string needs no second copy
    /// here — `GlobError` carries it for the one message that names it.
    matcher: regex::Regex,
}

/// Why a pattern was refused at compile time (C-009).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobError {
    /// The pattern used a metacharacter outside the grammar — `**`, `?` or
    /// `{`. Carries the offending character so the message can name it.
    UnsupportedMetacharacter { pattern: String, character: char },
}

impl std::fmt::Display for GlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedMetacharacter { pattern, character } => write!(
                f,
                "glob pattern '{pattern}' uses unsupported character '{character}': only literal characters and '*' are allowed"
            ),
        }
    }
}

impl std::error::Error for GlobError {}

impl Glob {
    /// Compile one pattern (C-009).
    ///
    /// `*` matches any run of characters **including `/`** — the subject is
    /// the whole two-segment package name, not one path segment. Every literal
    /// run is escaped, so a pattern containing regex metacharacters (`a.b/c+d`)
    /// matches them literally.
    ///
    /// # Errors
    ///
    /// [`GlobError::UnsupportedMetacharacter`] for `**`, `?` or `{`, naming
    /// the offending character.
    pub fn compile(pattern: &str) -> Result<Glob, GlobError> {
        let mut previous_was_star = false;
        for character in pattern.chars() {
            match character {
                '?' | '{' => {
                    return Err(GlobError::UnsupportedMetacharacter {
                        pattern: pattern.to_string(),
                        character,
                    });
                }
                '*' if previous_was_star => {
                    return Err(GlobError::UnsupportedMetacharacter {
                        pattern: pattern.to_string(),
                        character: '*',
                    });
                }
                '*' => previous_was_star = true,
                _ => previous_was_star = false,
            }
        }

        // Every literal run between `*`s is escaped, so regex metacharacters
        // in the pattern (`a.b/c+d`) are matched literally, never as regex
        // syntax. `*` alone becomes `.*` and matches any run of characters,
        // including `/` — the subject is the whole two-segment name.
        let translated = pattern.split('*').map(regex::escape).collect::<Vec<_>>().join(".*");

        // Invariant: every segment above went through `regex::escape`, so the
        // joined-and-anchored string is always syntactically valid regex —
        // this can never fail to compile.
        let matcher = regex::Regex::new(&format!("^{translated}$"))
            .expect("escaped literal segments anchored with .* is always a valid regex");

        Ok(Glob { matcher })
    }

    /// Whether this pattern matches `name` in full (anchored both ends).
    pub fn matches(&self, name: &str) -> bool {
        self.matcher.is_match(name)
    }
}

/// Whether a package name survives one source's filters (C-010).
///
/// `true` iff (`include` is empty **or** some include matches) **and** no
/// exclude matches. Exclude is an unconditional veto: a name matching both an
/// include and an exclude is rejected.
pub fn package_selected(name: &str, include: &[Glob], exclude: &[Glob]) -> bool {
    let included = include.is_empty() || include.iter().any(|glob| glob.matches(name));
    included && !exclude.iter().any(|glob| glob.matches(name))
}

#[cfg(test)]
#[path = "glob/tests.rs"]
mod tests;
