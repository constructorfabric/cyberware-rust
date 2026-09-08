#![allow(unknown_lints, de0309_must_have_domain_model)]

//! Test double for [`RoleAssignmentRepository`] — a store that behaves as
//! though `role_assignments` were empty.
//!
//! It exists because [`crate::domain::role_definition::RoleDefinitionService`]
//! takes an assignment repository (role-definition reads carry a per-role
//! assignment count), while most of the tests that build that service care
//! about role definitions and nothing else. Handing them a shared, obviously
//! inert double keeps a dozen hand-rolled seven-method stubs out of the test
//! suite — and keeps them from drifting apart when the trait grows.
//!
//! Reads answer as if the table were empty; writes return an `Internal`
//! error rather than panicking, so a test that reaches one by mistake fails
//! with a diagnosable message instead of unwinding inside an async task.

use std::collections::HashMap;
use toolkit_db::secure::DBRunner;

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page, PageInfo};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::RoleAssignmentModel;
use crate::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, SubjectAssignmentsQuery, VisibilityFilter,
};

/// A [`RoleAssignmentRepository`] holding no rows and accepting no writes.
///
/// Use it wherever a service under test needs the assignment repo to exist
/// but not to answer anything interesting. A test that asserts on actual
/// counts should use the real repository against a `SQLite` or Postgres
/// fixture instead — the number is the thing under test there, and a double
/// that always says "empty" cannot prove it.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyRoleAssignmentRepository;

#[async_trait]
impl RoleAssignmentRepository for EmptyRoleAssignmentRepository {
    async fn create<C: DBRunner>(
        &self,
        _db: &C,
        _new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        Err(DomainError::internal(
            "EmptyRoleAssignmentRepository accepts no writes",
        ))
    }

    async fn find_by_id<C: DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        Ok(None)
    }

    async fn list<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _query: &ODataQuery,
    ) -> Result<Page<RoleAssignmentModel>, DomainError> {
        Ok(Page {
            items: Vec::new(),
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 0,
            },
        })
    }

    async fn get_subject_assignments<C: DBRunner>(
        &self,
        _db: &C,
        _query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        Ok(Vec::new())
    }

    async fn delete<C: DBRunner>(&self, _db: &C, _id: Uuid) -> Result<bool, DomainError> {
        Ok(false)
    }

    /// No rows, so no role has any assignments. Note that this is the empty
    /// *map*, not a map of zeros: the trait's contract is that an absent
    /// role means "no visible assignments", and the service turns that into
    /// `Some(0)` — so a test built on this double still exercises the
    /// absent-means-zero path rather than side-stepping it.
    async fn count_by_role<C: DBRunner>(
        &self,
        _db: &C,
        _visibility: VisibilityFilter,
        _ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError> {
        Ok(HashMap::new())
    }
}
