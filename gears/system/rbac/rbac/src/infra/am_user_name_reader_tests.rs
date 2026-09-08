//! Tests for [`super::AmUserNameReader`].
//!
//! These are cost tests as much as correctness tests. Every assertion on
//! `list_calls()` / `get_calls()` exists because the naive implementation
//! — one `get_user` per principal — returns exactly the same names while
//! costing one full Keycloak membership drain per row. Only the call
//! counts can tell the two apart.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use account_management_sdk::tenant::Tenant;
use account_management_sdk::{
    AccountManagementClient, CreateTenantRequest, IdpNewUser, IdpServiceAccountCredentials,
    IdpServiceAccountSummary, IdpUser, IdpUserPatch, ListUsersQuery, MetadataEntry,
    UpdateTenantRequest, UpsertMetadataRequest,
};
use gts::GtsTypeId;
use parking_lot::Mutex;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::{ODataQuery, Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::{Uuid, uuid};

use super::AmUserNameReader;
use crate::config::PrincipalNamesConfig;
use crate::domain::ports::principal_name_reader::{PrincipalNameError, PrincipalNameReader};

const T1: Uuid = uuid!("11111111-1111-1111-1111-111111111111");
const U1: Uuid = uuid!("aaaaaaaa-0000-0000-0000-000000000001");
const U2: Uuid = uuid!("aaaaaaaa-0000-0000-0000-000000000002");
const U3: Uuid = uuid!("aaaaaaaa-0000-0000-0000-000000000003");

fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

/// One scripted `list_users` response.
enum Step {
    /// A page of users; `has_next` decides whether the reader keeps
    /// paging.
    Page { users: Vec<IdpUser>, has_next: bool },
    /// `Err(ServiceUnavailable)` — the upstream-outage path.
    Unavailable,
}

/// Scripted `dyn AccountManagementClient`.
///
/// `list_users` pops the next scripted step (an exhausted script answers
/// with a final empty page, so an over-eager pass cannot hang); `get_user`
/// answers from a seeded table. Both count their calls. The other 15
/// trait methods are `unimplemented!()` — reaching one would mean the
/// reader started doing something it has no business doing.
struct MockAm {
    steps: Mutex<VecDeque<Step>>,
    users: Mutex<HashMap<Uuid, IdpUser>>,
    list_calls: AtomicUsize,
    get_calls: AtomicUsize,
}

impl MockAm {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            users: Mutex::new(HashMap::new()),
            list_calls: AtomicUsize::new(0),
            get_calls: AtomicUsize::new(0),
        }
    }

    /// Seed a user reachable through `get_user` only (the point-lookup
    /// fallback path).
    fn with_point_user(self, user: IdpUser) -> Self {
        self.users.lock().insert(user.id, user);
        self
    }

    fn list_calls(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }

    fn get_calls(&self) -> usize {
        self.get_calls.load(Ordering::SeqCst)
    }
}

fn user(id: Uuid, username: &str) -> IdpUser {
    IdpUser::new(id, username)
}

/// A reader over `mock`, registered in a fresh `ClientHub`.
fn reader_over(mock: &Arc<MockAm>, cfg: PrincipalNamesConfig) -> AmUserNameReader {
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn AccountManagementClient>(
        Arc::clone(mock) as Arc<dyn AccountManagementClient>
    );
    AmUserNameReader::new(hub, cfg)
}

fn ids(v: &[Uuid]) -> Vec<String> {
    v.iter().map(Uuid::to_string).collect()
}

#[async_trait::async_trait]
impl AccountManagementClient for MockAm {
    async fn list_users(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        match self.steps.lock().pop_front() {
            Some(Step::Unavailable) => Err(CanonicalError::service_unavailable().create()),
            Some(Step::Page { users, has_next }) => {
                let limit = users.len() as u64;
                Ok(Page {
                    items: users,
                    page_info: PageInfo {
                        next_cursor: has_next.then(|| "next".to_owned()),
                        prev_cursor: None,
                        limit,
                    },
                })
            }
            // Script exhausted: a terminal empty page, so an over-eager
            // pass ends rather than looping.
            None => Ok(Page {
                items: Vec::new(),
                page_info: PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: 0,
                },
            }),
        }
    }

    async fn get_user(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        user_id: Uuid,
    ) -> Result<IdpUser, CanonicalError> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        match self.users.lock().get(&user_id).cloned() {
            Some(u) => Ok(u),
            None => Err(CanonicalError::service_unavailable().create()),
        }
    }

    async fn create_tenant(
        &self,
        _ctx: &SecurityContext,
        _input: CreateTenantRequest,
    ) -> Result<Tenant, CanonicalError> {
        unimplemented!("MockAm: create_tenant is not part of name resolution")
    }

    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: Uuid,
    ) -> Result<Tenant, CanonicalError> {
        unimplemented!("MockAm: get_tenant is not part of name resolution")
    }

    async fn list_children(
        &self,
        _ctx: &SecurityContext,
        _parent_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<Tenant>, CanonicalError> {
        unimplemented!("MockAm: list_children is not part of name resolution")
    }

    async fn update_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: Uuid,
        _patch: UpdateTenantRequest,
    ) -> Result<Tenant, CanonicalError> {
        unimplemented!("MockAm: update_tenant is not part of name resolution")
    }

    async fn suspend_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: Uuid,
    ) -> Result<Tenant, CanonicalError> {
        unimplemented!("MockAm: suspend_tenant is not part of name resolution")
    }

    async fn unsuspend_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: Uuid,
    ) -> Result<Tenant, CanonicalError> {
        unimplemented!("MockAm: unsuspend_tenant is not part of name resolution")
    }

    async fn delete_tenant(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
    ) -> Result<Tenant, CanonicalError> {
        unimplemented!("MockAm: delete_tenant is not part of name resolution")
    }

    async fn create_user(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payload: IdpNewUser,
    ) -> Result<IdpUser, CanonicalError> {
        unimplemented!("MockAm: create_user is not part of name resolution")
    }

    async fn update_user(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _user_id: Uuid,
        _patch: IdpUserPatch,
    ) -> Result<IdpUser, CanonicalError> {
        unimplemented!("MockAm: update_user is not part of name resolution")
    }

    async fn delete_user(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _user_id: Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!("MockAm: delete_user is not part of name resolution")
    }

    async fn get_metadata(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _type_id: GtsTypeId,
    ) -> Result<MetadataEntry, CanonicalError> {
        unimplemented!("MockAm: get_metadata is not part of name resolution")
    }

    async fn resolve_metadata(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _type_id: GtsTypeId,
    ) -> Result<Option<MetadataEntry>, CanonicalError> {
        unimplemented!("MockAm: resolve_metadata is not part of name resolution")
    }

    async fn list_metadata(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<MetadataEntry>, CanonicalError> {
        unimplemented!("MockAm: list_metadata is not part of name resolution")
    }

    async fn upsert_metadata(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _input: UpsertMetadataRequest,
    ) -> Result<MetadataEntry, CanonicalError> {
        unimplemented!("MockAm: upsert_metadata is not part of name resolution")
    }

    async fn delete_metadata(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _type_id: GtsTypeId,
    ) -> Result<(), CanonicalError> {
        unimplemented!("MockAm: delete_metadata is not part of name resolution")
    }

    async fn create_service_account(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _name: String,
        _scopes: Vec<String>,
    ) -> Result<IdpServiceAccountCredentials, CanonicalError> {
        unimplemented!("MockAm: create_service_account is not part of name resolution")
    }

    async fn list_service_accounts(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
    ) -> Result<Vec<IdpServiceAccountSummary>, CanonicalError> {
        // Not the reader's path even though it looks close: a service
        // principal's name would come from here, but nothing maps an SP
        // subject id back to a client id, so the hydrator never asks.
        unimplemented!("MockAm: list_service_accounts is not part of name resolution")
    }

    async fn rotate_service_account_secret(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _client_id: &str,
    ) -> Result<IdpServiceAccountCredentials, CanonicalError> {
        unimplemented!("MockAm: rotate_service_account_secret is not part of name resolution")
    }

    async fn revoke_service_account(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _client_id: &str,
    ) -> Result<(), CanonicalError> {
        unimplemented!("MockAm: revoke_service_account is not part of name resolution")
    }
}

/// One pass names every principal asked for, and a second read inside the
/// TTL performs no upstream call at all — the property that keeps a
/// repeatedly rendered assignment grid off Keycloak entirely.
#[tokio::test]
async fn one_pass_populates_the_cache_and_the_second_read_is_free() {
    let mock = Arc::new(MockAm::new(vec![Step::Page {
        users: vec![user(U1, "ada"), user(U2, "alan"), user(U3, "grace")],
        has_next: false,
    }]));
    let reader = reader_over(&mock, PrincipalNamesConfig::default());

    let first = reader
        .user_names(&ctx(), T1, &ids(&[U1, U2]))
        .await
        .expect("first read");
    assert_eq!(first.get(&U1.to_string()).map(String::as_str), Some("ada"));
    assert_eq!(first.get(&U2.to_string()).map(String::as_str), Some("alan"));
    assert_eq!(mock.list_calls(), 1, "one pass, not one call per id");

    // U3 was never asked for, but the pass saw it: the second read is a
    // pure cache hit even for an id the first read did not mention.
    let second = reader
        .user_names(&ctx(), T1, &ids(&[U1, U3]))
        .await
        .expect("second read");
    assert_eq!(
        second.get(&U3.to_string()).map(String::as_str),
        Some("grace")
    );
    assert_eq!(mock.list_calls(), 1, "a warm cache must not call upstream");
    assert_eq!(mock.get_calls(), 0, "no point lookups on a warm cache");
}

/// A duplicated id costs nothing extra, and the request is deduplicated
/// before the pass starts.
#[tokio::test]
async fn duplicate_ids_do_not_add_calls() {
    let mock = Arc::new(MockAm::new(vec![Step::Page {
        users: vec![user(U1, "ada")],
        has_next: false,
    }]));
    let reader = reader_over(&mock, PrincipalNamesConfig::default());

    let out = reader
        .user_names(&ctx(), T1, &ids(&[U1, U1, U1]))
        .await
        .expect("read");

    assert_eq!(out.len(), 1);
    assert_eq!(mock.list_calls(), 1);
}

/// Ids the pass could not cover (page budget exhausted) fall back to
/// point lookups — and only those ids.
#[tokio::test]
async fn unresolved_ids_fall_back_to_point_lookups() {
    let cfg = PrincipalNamesConfig {
        // One page of budget against a two-page membership: the pass is
        // truncated, so it cannot prove U2 has no name.
        max_pages_per_tenant: 1,
        ..PrincipalNamesConfig::default()
    };
    let mock = Arc::new(
        MockAm::new(vec![
            Step::Page {
                users: vec![user(U1, "ada")],
                has_next: true,
            },
            Step::Page {
                users: vec![user(U2, "alan")],
                has_next: false,
            },
        ])
        .with_point_user(user(U2, "alan")),
    );
    let reader = reader_over(&mock, cfg);

    let out = reader
        .user_names(&ctx(), T1, &ids(&[U1, U2]))
        .await
        .expect("read");

    assert_eq!(out.get(&U1.to_string()).map(String::as_str), Some("ada"));
    assert_eq!(out.get(&U2.to_string()).map(String::as_str), Some("alan"));
    assert_eq!(mock.list_calls(), 1, "the pass stops at its page budget");
    assert_eq!(mock.get_calls(), 1, "only the uncovered id is looked up");
}

/// The point-lookup fallback is itself bounded: a truncated pass with
/// more unresolved ids than the budget allows leaves the remainder
/// unnamed rather than issuing one membership drain per row.
#[tokio::test]
async fn point_lookup_fallback_is_bounded() {
    let cfg = PrincipalNamesConfig {
        max_pages_per_tenant: 1,
        max_point_lookups_per_tenant: 1,
        ..PrincipalNamesConfig::default()
    };
    let mock = Arc::new(
        MockAm::new(vec![Step::Page {
            users: Vec::new(),
            has_next: true,
        }])
        .with_point_user(user(U1, "ada"))
        .with_point_user(user(U2, "alan")),
    );
    let reader = reader_over(&mock, cfg);

    let out = reader
        .user_names(&ctx(), T1, &ids(&[U1, U2]))
        .await
        .expect("read");

    assert_eq!(out.len(), 1, "budget of 1 must not name both: {out:?}");
    assert_eq!(mock.get_calls(), 1, "point lookups are capped by config");
}

/// A miss is cached: a principal absent from a *fully drained* tenant is
/// not re-fetched within the TTL, and never point-looked-up — the pass
/// already proved there is nothing to find.
#[tokio::test]
async fn negative_results_are_cached() {
    let mock = Arc::new(MockAm::new(vec![Step::Page {
        users: vec![user(U1, "ada")],
        has_next: false,
    }]));
    let reader = reader_over(&mock, PrincipalNamesConfig::default());

    let first = reader
        .user_names(&ctx(), T1, &ids(&[U2]))
        .await
        .expect("first read");
    assert!(first.is_empty());
    assert_eq!(mock.get_calls(), 0, "a drained tenant needs no fallback");

    let second = reader
        .user_names(&ctx(), T1, &ids(&[U2]))
        .await
        .expect("second read");
    assert!(second.is_empty());
    assert_eq!(mock.list_calls(), 1, "the miss must be cached");
}

/// An upstream failure surfaces as `Unavailable` and caches nothing: the
/// very next read tries again, so a blip does not pin "no name" for a
/// whole TTL.
#[tokio::test]
async fn upstream_error_is_not_cached() {
    let mock = Arc::new(MockAm::new(vec![
        Step::Unavailable,
        Step::Page {
            users: vec![user(U1, "ada")],
            has_next: false,
        },
    ]));
    let reader = reader_over(&mock, PrincipalNamesConfig::default());

    let err = reader
        .user_names(&ctx(), T1, &ids(&[U1]))
        .await
        .expect_err("upstream is down");
    assert!(
        matches!(err, PrincipalNameError::Unavailable { .. }),
        "{err:?}"
    );

    let recovered = reader
        .user_names(&ctx(), T1, &ids(&[U1]))
        .await
        .expect("second read");
    assert_eq!(
        recovered.get(&U1.to_string()).map(String::as_str),
        Some("ada"),
        "a failed read must not have cached a miss"
    );
}

/// With no client registered in the hub the reader answers empty without
/// erroring: RBAC must serve role-assignment reads on a deployment that
/// has no account management at all.
#[tokio::test]
async fn absent_client_yields_no_names() {
    let hub = Arc::new(ClientHub::new());
    let reader = AmUserNameReader::new(hub, PrincipalNamesConfig::default());

    let out = reader
        .user_names(&ctx(), T1, &ids(&[U1]))
        .await
        .expect("absence is not an error");

    assert!(out.is_empty());
}

/// `display_name` preference order, and the rule that a whitespace-only
/// value counts as absent — an empty string on the wire would render as a
/// blank cell instead of the id.
#[test]
fn display_name_prefers_display_then_full_then_username() {
    let full = IdpUser::new(U1, "ada")
        .with_display_name("Ada Lovelace".to_owned())
        .with_first_name("Ada".to_owned())
        .with_last_name("Lovelace".to_owned());
    assert_eq!(
        AmUserNameReader::display_name(&full).as_deref(),
        Some("Ada Lovelace")
    );

    let names_only = IdpUser::new(U1, "ada")
        .with_first_name("Ada".to_owned())
        .with_last_name("Lovelace".to_owned());
    assert_eq!(
        AmUserNameReader::display_name(&names_only).as_deref(),
        Some("Ada Lovelace")
    );

    let username_only = IdpUser::new(U1, "ada");
    assert_eq!(
        AmUserNameReader::display_name(&username_only).as_deref(),
        Some("ada")
    );

    let blank_display = IdpUser::new(U1, "ada").with_display_name("   ".to_owned());
    assert_eq!(
        AmUserNameReader::display_name(&blank_display).as_deref(),
        Some("ada"),
        "whitespace-only display name falls through"
    );

    let nothing = IdpUser::new(U1, "  ");
    assert_eq!(
        AmUserNameReader::display_name(&nothing),
        None,
        "no renderable name at all is None, never Some(\"\")"
    );
}

/// A principal id spelled in a different case than AM's canonical UUID
/// form still matches. Without canonicalisation this is a permanent,
/// silent "no name" for that row.
#[tokio::test]
async fn principal_ids_match_case_insensitively() {
    let mock = Arc::new(MockAm::new(vec![Step::Page {
        users: vec![user(U1, "ada")],
        has_next: false,
    }]));
    let reader = reader_over(&mock, PrincipalNamesConfig::default());
    let shouty = U1.to_string().to_uppercase();

    let out = reader
        .user_names(&ctx(), T1, std::slice::from_ref(&shouty))
        .await
        .expect("read");

    assert_eq!(out.get(&shouty).map(String::as_str), Some("ada"));
}

/// End-to-end over the shared blank rule: a user whose only renderable
/// value is whitespace is *absent* from the map, never present with an
/// empty string. Absent is what makes the row render its id; `Some("")`
/// would render an empty cell that reads as a bug.
#[tokio::test]
async fn a_user_with_no_renderable_name_is_absent_from_the_map() {
    let mock = Arc::new(MockAm::new(vec![Step::Page {
        users: vec![
            IdpUser::new(U1, "   "),
            IdpUser::new(U2, "ada").with_display_name("  Ada Lovelace \n".to_owned()),
        ],
        has_next: false,
    }]));
    let reader = reader_over(&mock, PrincipalNamesConfig::default());

    let out = reader
        .user_names(&ctx(), T1, &ids(&[U1, U2]))
        .await
        .expect("a blank name is not an error");

    assert!(
        !out.contains_key(&U1.to_string()),
        "a blank name MUST be absent, not an empty string"
    );
    assert_eq!(
        out.get(&U2.to_string()).map(String::as_str),
        Some("Ada Lovelace"),
        "incidental whitespace is trimmed rather than dropped"
    );
}
