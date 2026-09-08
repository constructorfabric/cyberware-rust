//! Repository trait definitions. SeaORM-backed implementations live under
//! `infra/storage/repo/`.

pub mod role_assignment_repo;
/// Inert test double for the assignment repository. Gated behind
/// `test-support` for the same reason the port mocks are: a repository that
/// silently reports "no assignments" must never be wired into a release
/// artifact, where it would make every role look unused.
#[cfg(any(test, feature = "test-support"))]
pub mod role_assignment_repo_mock;
pub mod role_definition_repo;
