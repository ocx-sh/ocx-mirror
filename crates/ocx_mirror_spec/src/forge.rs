// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The announce forge vocabulary `validate_announce_config` checks a spec
//! against: the `[HOST/]NAMESPACE/PROJECT` coordinate, the two forges and the
//! two write transports.
//!
//! **Mirror-owned rather than linked.** These types live in `ocx_announce`,
//! which is an [internal]-tier crate in `ocx`'s stability table, and the
//! satellite linking rule — the one `task satellite:verify` enforces — forbids
//! a lockstep consumer linking an internal crate: internal code carries no
//! stability at all, so a link is a standing break waiting for the next
//! rename.
//!
//! **This is fail-fast, not enforcement.** The mirror validates a spec it then
//! hands to an *installed* `ocx` binary as argv, never to the vendored library
//! — so linking never guaranteed agreement across versions either. `ocx`
//! parses `--index-repo`, `--fork`, `--forge` and `--transport` again with its
//! own stricter grammar and refuses at exit 64. Nothing here widens what
//! reaches a forge; it only decides how early the operator hears about a spec
//! that cannot work, and `plan` is much earlier than "after the images are
//! published".
//
// ponytail: six pure rules restated. The upgrade path is upstream — move
// `ForgeKind`, `WriteTransport` and `RepoCoordinate` into an ecosystem-tier
// crate (`ocx_index` is the natural home: announce writes the index repo) and
// link them again. That is an `ocx` change plus a lockstep submodule bump.

/// How `ocx package announce` writes the index request: `ocx`'s `--transport`
/// vocabulary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteTransport {
    /// The forge's REST API.
    #[default]
    Api,
    /// A local `git` clone and one authenticated push carrying the
    /// merge-request options — GitLab only.
    Git,
}

impl WriteTransport {
    /// Parse `ocx`'s own `--transport` spelling. `None` for anything else,
    /// which `validate_announce_config` reports under `announce.transport`.
    pub(crate) fn parse(spelled: &str) -> Option<Self> {
        match spelled {
            "api" => Some(Self::Api),
            "git" => Some(Self::Git),
            _ => None,
        }
    }
}

/// The forges `ocx package announce` can write to: `ocx`'s `--forge`
/// vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgeKind {
    /// GitHub.com or a GitHub Enterprise Server instance.
    GitHub,
    /// GitLab.com or a self-managed GitLab instance.
    GitLab,
}

impl ForgeKind {
    /// Parse `ocx`'s own `--forge` spelling.
    pub(crate) fn parse(spelled: &str) -> Option<Self> {
        match spelled {
            "github" => Some(Self::GitHub),
            "gitlab" => Some(Self::GitLab),
            _ => None,
        }
    }

    /// The forge a canonical host belongs to, or `None` for anything else.
    ///
    /// A self-hosted instance is deliberately not guessed: hostnames carry no
    /// convention, and `ocx` makes the publisher declare it rather than send
    /// the announce credential to the wrong API.
    fn from_host(host: Option<&str>) -> Option<Self> {
        match host {
            // No host at all means the default index, which is on GitHub.
            None => Some(Self::GitHub),
            Some(host) if host.eq_ignore_ascii_case("github.com") => Some(Self::GitHub),
            Some(host) if host.eq_ignore_ascii_case("gitlab.com") => Some(Self::GitLab),
            Some(_) => None,
        }
    }

    /// Resolve the forge for `coordinate`, honouring an explicit `declared`
    /// kind. `None` when the host is self-hosted and nothing was declared.
    pub(crate) fn resolve(declared: Option<Self>, coordinate: &RepoCoordinate) -> Option<Self> {
        declared.or_else(|| Self::from_host(coordinate.host.as_deref()))
    }

    /// The host this forge lives on when a coordinate names none — so that
    /// `ocx-sh/index` and `github.com/ocx-sh/index` are one instance, not two.
    pub(crate) fn canonical_host(self) -> &'static str {
        match self {
            Self::GitHub => "github.com",
            Self::GitLab => "gitlab.com",
        }
    }

    /// Whether two coordinates name the same instance of this forge, with an
    /// omitted host resolved to [`Self::canonical_host`] on both sides.
    pub(crate) fn same_host(self, left: &RepoCoordinate, right: &RepoCoordinate) -> bool {
        let resolve = |coordinate: &RepoCoordinate| {
            coordinate
                .host
                .clone()
                .unwrap_or_else(|| self.canonical_host().to_string())
        };
        resolve(left).eq_ignore_ascii_case(&resolve(right))
    }

    /// Refuse a coordinate this forge cannot express. GitHub namespaces are a
    /// single segment; GitLab nests groups arbitrarily deep.
    pub(crate) fn validate_coordinate(self, coordinate: &RepoCoordinate) -> Result<(), String> {
        match self {
            Self::GitHub if coordinate.namespace.contains('/') => Err(format!(
                "GitHub has no nested namespaces — '{}' is a nested group path",
                coordinate.namespace
            )),
            Self::GitHub | Self::GitLab => Ok(()),
        }
    }

    /// Whether this forge can serve a write transport: GitHub has no
    /// push-option merge-request creation, so `git` has nothing to do there.
    pub(crate) fn serves_transport(self, transport: WriteTransport) -> bool {
        match (self, transport) {
            (Self::GitHub, WriteTransport::Git) => false,
            (Self::GitHub, WriteTransport::Api) | (Self::GitLab, WriteTransport::Api | WriteTransport::Git) => true,
        }
    }
}

/// Whether an `[HOST/]NAMESPACE/PROJECT` index repository resolves to GitLab —
/// the declared `forge` first, then the index host, exactly as
/// `validate_announce_config` resolves it. Unparsable or unresolvable is
/// `false`.
pub fn forge_is_gitlab(index_repo: &str, declared: Option<ForgeKind>) -> bool {
    RepoCoordinate::parse(index_repo).and_then(|index| ForgeKind::resolve(declared, &index)) == Some(ForgeKind::GitLab)
}

/// A forge repository coordinate: `[HOST/]NAMESPACE/PROJECT`.
///
/// The namespace may hold slashes — GitLab nests groups — so the type is
/// forge-neutral and [`ForgeKind::validate_coordinate`] rejects what a
/// particular forge cannot express.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoCoordinate {
    /// The forge host, or `None` for the forge's canonical host.
    pub(crate) host: Option<String>,
    /// The owning account, organization, or (possibly nested) group path.
    pub(crate) namespace: String,
    /// The repository/project name.
    pub(crate) project: String,
}

impl RepoCoordinate {
    /// Parse `[HOST/]NAMESPACE/PROJECT`, or `None` when the value is not one.
    ///
    /// Whether a leading segment is a host is decided by the same rule OCI
    /// identifiers use ([`ocx_oci::identifier::segment_is_host`]) — one
    /// spelling of "that looks like a host", not a second one free to drift.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let mut segments: Vec<&str> = value.split('/').collect();
        // A leading host is only recognised when something is left to be a
        // `namespace/project` after it — `acme/index` is a two-segment path,
        // never a host with a bare project.
        let host = if segments.len() >= 3 && ocx_oci::identifier::segment_is_host(segments[0]) {
            // A segment that looks like a host but is not a well-formed one is
            // refused, never demoted to a namespace segment: `ocx` interpolates
            // it into the API base URL the announce credential is sent to, so
            // `gitlab.com@evil.example` would put `gitlab.com` in the userinfo
            // and the token on another host. Same answer, one stage earlier.
            if !is_host_like(segments[0]) {
                return None;
            }
            Some(segments.remove(0).to_string())
        } else {
            None
        };
        if segments.len() < 2 || !segments.iter().all(|segment| is_path_segment(segment)) {
            return None;
        }
        let project = segments.pop()?.to_string();
        Some(Self {
            host,
            namespace: segments.join("/"),
            project,
        })
    }
}

/// Whether a segment can be a `host` or `host:port` and nothing else — no
/// userinfo, no path, no query, no IPv6 literal.
//
// ponytail: a charset check, not `ocx`'s label-by-label grammar. It rejects
// every character that could shift a URL's authority, which is the property
// that matters here; `ocx` applies the full rule when it builds the URL.
fn is_host_like(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 253
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
}

/// Whether a segment is a namespace or project name `ocx` accepts.
fn is_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_host_namespace_and_project() {
        let bare = RepoCoordinate::parse("ocx-sh/index").expect("two segments are a coordinate");
        assert_eq!(bare.host, None);
        assert_eq!(bare.namespace, "ocx-sh");
        assert_eq!(bare.project, "index");

        let nested = RepoCoordinate::parse("gitlab.example/acme/platform/index").expect("nested group path");
        assert_eq!(nested.host.as_deref(), Some("gitlab.example"));
        assert_eq!(nested.namespace, "acme/platform");
        assert_eq!(nested.project, "index");
    }

    /// A host-looking first segment is only a host when a `namespace/project`
    /// remains after it.
    #[test]
    fn a_two_segment_path_is_never_host_plus_project() {
        let coordinate = RepoCoordinate::parse("gitlab.com/index").expect("two segments");
        assert_eq!(coordinate.host, None);
        assert_eq!(coordinate.namespace, "gitlab.com");
    }

    /// The refusal that matters: a segment that looks like a host but could
    /// move the URL's authority is rejected, not demoted to a namespace.
    #[test]
    fn parse_refuses_a_malformed_host_and_empty_segments() {
        for value in [
            "gitlab.com@evil.example/ns/project",
            "gitlab.com/ns//project",
            "ocx-sh",
            "gitlab.example/../project",
        ] {
            assert_eq!(RepoCoordinate::parse(value), None, "accepted '{value}'");
        }
    }

    #[test]
    fn an_omitted_host_is_the_canonical_host_on_both_sides() {
        let bare = RepoCoordinate::parse("ocx-sh/index").expect("coordinate");
        let spelled = RepoCoordinate::parse("github.com/me/index").expect("coordinate");
        assert!(ForgeKind::GitHub.same_host(&bare, &spelled));
        assert!(!ForgeKind::GitHub.same_host(
            &bare,
            &RepoCoordinate::parse("github.example/me/index").expect("coordinate"),
        ));
    }

    #[test]
    fn resolve_needs_a_declared_forge_for_a_self_hosted_host() {
        let self_hosted = RepoCoordinate::parse("git.example/acme/index").expect("coordinate");
        assert_eq!(ForgeKind::resolve(None, &self_hosted), None);
        assert_eq!(
            ForgeKind::resolve(Some(ForgeKind::GitLab), &self_hosted),
            Some(ForgeKind::GitLab)
        );
        assert_eq!(
            ForgeKind::resolve(
                None,
                &RepoCoordinate::parse("gitlab.com/acme/index").expect("coordinate")
            ),
            Some(ForgeKind::GitLab)
        );
    }

    #[test]
    fn github_refuses_a_nested_namespace_and_the_git_transport() {
        let nested = RepoCoordinate::parse("github.com/acme/platform/index").expect("coordinate");
        assert!(ForgeKind::GitHub.validate_coordinate(&nested).is_err());
        assert!(ForgeKind::GitLab.validate_coordinate(&nested).is_ok());
        assert!(!ForgeKind::GitHub.serves_transport(WriteTransport::Git));
        assert!(ForgeKind::GitLab.serves_transport(WriteTransport::Git));
    }

    #[test]
    fn forge_is_gitlab_reads_the_declared_forge_before_the_host() {
        assert!(forge_is_gitlab("gitlab.com/acme/index", None));
        assert!(!forge_is_gitlab("ocx-sh/index", None));
        assert!(forge_is_gitlab("git.example/acme/index", Some(ForgeKind::GitLab)));
        // Unresolvable and unparsable both answer `false`.
        assert!(!forge_is_gitlab("git.example/acme/index", None));
        assert!(!forge_is_gitlab("acme", Some(ForgeKind::GitLab)));
    }

    /// The two spellings are `ocx`'s own flag values, and the default is
    /// `ocx`'s default.
    #[test]
    fn the_vocabularies_are_ocx_s_flag_values() {
        assert_eq!(WriteTransport::parse("api"), Some(WriteTransport::Api));
        assert_eq!(WriteTransport::parse("git"), Some(WriteTransport::Git));
        assert_eq!(WriteTransport::parse("API"), None);
        assert_eq!(WriteTransport::default(), WriteTransport::Api);
        assert_eq!(ForgeKind::parse("github"), Some(ForgeKind::GitHub));
        assert_eq!(ForgeKind::parse("gitlab"), Some(ForgeKind::GitLab));
        assert_eq!(ForgeKind::parse("gitea"), None);
    }
}
