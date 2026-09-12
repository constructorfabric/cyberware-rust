//! Which parts of a repository a sync collects.
//!
//! Two independent axes, ported from the reference implementation's
//! `engine/src/scope.rs`:
//!
//! - [`SyncScope`] decides **whether** an object type is mirrored at all.
//! - [`CollectionScope`] decides **how much** of the expensive per-entity
//!   sub-resources (reactions, timeline, CI checks) to fetch.
//!
//! Defaults follow PRD §5.4: the standard repository and collaboration
//! entities are on, security is opt-in because it needs elevated token
//! permissions.

use serde::{Deserialize, Serialize};
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;

/// Whether each mirrored object type is collected.
///
/// The field set matches the reference implementation so a scope written for
/// one is readable by the other; each flag gates a group of the mirror's 26
/// tables rather than a single table.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SyncScope {
    /// Issues, their comments, events, timeline and reactions.
    pub issues: bool,
    /// Pull requests, reviews, review comments, review threads, files, commits.
    pub pull_requests: bool,
    /// Commits, commit files, commit comments, commit statuses, check runs.
    pub commits: bool,
    pub releases: bool,
    /// Branches and tags.
    pub branches: bool,
    pub labels: bool,
    pub milestones: bool,
    /// Workflow runs, workflow jobs and deployments.
    pub github_actions: bool,
    pub contributors: bool,
    /// Security advisories, Dependabot and code-scanning alerts. Off by
    /// default: GitHub requires elevated token scopes (PRD §5.4), and the
    /// mirror does not fetch them yet.
    pub security: bool,
}

impl Default for SyncScope {
    /// PRD §5.4's default scope: every standard entity, security opt-in.
    fn default() -> Self {
        Self {
            issues: true,
            pull_requests: true,
            commits: true,
            releases: true,
            branches: true,
            labels: true,
            milestones: true,
            github_actions: true,
            contributors: true,
            security: false,
        }
    }
}

impl SyncScope {
    /// A scope with every object type disabled.
    #[must_use]
    pub fn none() -> Self {
        Self {
            issues: false,
            pull_requests: false,
            commits: false,
            releases: false,
            branches: false,
            labels: false,
            milestones: false,
            github_actions: false,
            contributors: false,
            security: false,
        }
    }

    /// Whether any object type is enabled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !(self.issues
            || self.pull_requests
            || self.commits
            || self.releases
            || self.branches
            || self.labels
            || self.milestones
            || self.github_actions
            || self.contributors
            || self.security)
    }

    /// # Errors
    /// `Validation` when the scope would collect nothing at all, which is
    /// always a mistake rather than a cheap sync.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.is_empty() {
            return Err(DomainError::Validation {
                field: "scope".to_owned(),
                message: "at least one object type must be enabled".to_owned(),
            });
        }
        Ok(())
    }
}

/// How much of an expensive per-entity sub-resource to collect.
///
/// These are fetched once per issue or pull request and dominate the API-call
/// cost of a sync, so the useful default is the open working set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionMode {
    /// Collect for every issue or pull request, open or closed.
    All,
    /// Collect only for open issues and pull requests.
    #[default]
    #[serde(alias = "open_only", alias = "open-only")]
    Open,
    /// Do not collect this sub-resource at all.
    #[serde(alias = "off", alias = "skip", alias = "never", alias = "disabled")]
    None,
}

impl CollectionMode {
    /// Whether to collect for an entity in the given open/closed state.
    #[must_use]
    pub fn includes(self, is_open: bool) -> bool {
        match self {
            Self::All => true,
            Self::Open => is_open,
            Self::None => false,
        }
    }

    /// Parse `all` / `open` / `none`, with the reference's aliases.
    ///
    /// # Errors
    /// `Validation` for an unrecognised value.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "open" | "open_only" | "open-only" => Ok(Self::Open),
            "none" | "off" | "skip" | "never" | "disabled" => Ok(Self::None),
            other => Err(DomainError::Validation {
                field: "collection_mode".to_owned(),
                message: format!("invalid collection mode `{other}` (valid: all, open, none)"),
            }),
        }
    }
}

/// Per-sub-resource collection breadth (PRD §5.19's `--*-scope` flags).
///
/// Independent of [`SyncScope`]: that decides whether issues are mirrored at
/// all, this decides whether each mirrored issue also costs a reactions call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectionScope {
    /// Workflow runs and jobs associated with commits and pull requests.
    pub actions: CollectionMode,
    /// Reactions on issues and pull requests.
    pub reactions: CollectionMode,
    /// Timeline events. Off by default, as in the reference: a high-volume,
    /// low-signal feed that costs one request per issue (PRD §5.2).
    pub timeline: CollectionMode,
}

impl Default for CollectionScope {
    fn default() -> Self {
        Self {
            actions: CollectionMode::Open,
            reactions: CollectionMode::Open,
            timeline: CollectionMode::None,
        }
    }
}

/// Everything one sync needs to know about what to collect.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopeConfig {
    pub objects: SyncScope,
    pub collection: CollectionScope,
}

impl ScopeConfig {
    /// # Errors
    /// Whatever [`SyncScope::validate`] returns.
    pub fn validate(&self) -> Result<(), DomainError> {
        self.objects.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_scope_matches_the_prd_list() {
        let scope = SyncScope::default();
        assert!(scope.issues);
        assert!(scope.pull_requests);
        assert!(scope.commits);
        assert!(scope.releases);
        assert!(scope.branches);
        assert!(scope.labels);
        assert!(scope.milestones);
        assert!(scope.github_actions);
        assert!(scope.contributors);
        assert!(
            !scope.security,
            "security needs elevated token scopes, so it is opt-in"
        );
    }

    #[test]
    fn an_empty_scope_is_rejected() {
        assert!(SyncScope::none().is_empty());
        assert!(SyncScope::none().validate().is_err());
        assert!(SyncScope::default().validate().is_ok());
    }

    #[test]
    fn collection_modes_gate_on_open_state() {
        assert!(CollectionMode::All.includes(true));
        assert!(CollectionMode::All.includes(false));
        assert!(CollectionMode::Open.includes(true));
        assert!(!CollectionMode::Open.includes(false));
        assert!(!CollectionMode::None.includes(true));
        assert!(!CollectionMode::None.includes(false));
    }

    #[test]
    fn collection_modes_parse_their_aliases() {
        assert_eq!(CollectionMode::parse("All").unwrap(), CollectionMode::All);
        assert_eq!(
            CollectionMode::parse("open-only").unwrap(),
            CollectionMode::Open
        );
        assert_eq!(CollectionMode::parse("off").unwrap(), CollectionMode::None);
        assert!(CollectionMode::parse("sometimes").is_err());
    }

    #[test]
    fn the_timeline_default_follows_the_reference() {
        assert_eq!(CollectionScope::default().timeline, CollectionMode::None);
        assert_eq!(CollectionScope::default().actions, CollectionMode::Open);
        assert_eq!(CollectionScope::default().reactions, CollectionMode::Open);
    }
}
