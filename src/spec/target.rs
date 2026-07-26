// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub registry: String,
    pub repository: String,
}

impl Target {
    /// The registry-qualified reference, `registry/repository`.
    ///
    /// Always spell the registry out when handing a target to `ocx` as a
    /// string. A bare `repository` parses as a reference on the *default*
    /// registry, which silently routed a ghcr.io mirror's pushes at `ocx.sh`
    /// and answered `403 UNAUTHORIZED: No permission to write manifest`. The
    /// bug was invisible while every mirror targeted the default.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-default registry must survive into the reference.
    ///
    /// `pipeline push` built its `-i` argument from `repository` alone. On
    /// `ocx.sh` that was indistinguishable from correct, because a bare
    /// reference resolves against the default registry — so the first ghcr.io
    /// mirror pushed five versions at `ocx.sh` and collected
    /// `403 UNAUTHORIZED: No permission to write manifest` for every one.
    #[test]
    fn reference_keeps_a_non_default_registry() {
        let target = Target {
            registry: "ghcr.io".into(),
            repository: "ocx-contrib/bazelbuild/bazelisk".into(),
        };

        assert_eq!(target.reference(), "ghcr.io/ocx-contrib/bazelbuild/bazelisk");
    }
}
