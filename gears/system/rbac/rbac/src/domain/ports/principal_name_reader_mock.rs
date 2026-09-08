#![allow(unknown_lints, de0309_must_have_domain_model)]

//! Scriptable [`PrincipalNameReader`] test double: fixed answers per
//! tenant, an optional forced error, and a call counter so tests can
//! assert the batching contract (one call per tenant per read, never one
//! per row).
//!
//! The counters are the point of this fake. A hydrator that regressed to
//! per-row resolution would still return the right names, so only the
//! call count can catch it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::principal_name_reader::{PrincipalNameError, PrincipalNameReader};

/// In-memory [`PrincipalNameReader`] for domain tests.
#[derive(Default)]
pub struct FakePrincipalNameReader {
    /// `tenant -> (principal id -> name)`.
    pub names: Mutex<HashMap<Uuid, HashMap<String, String>>>,
    /// When set, every call fails with this error instead of answering.
    pub fail_with: Mutex<Option<PrincipalNameError>>,
    /// Number of `user_names` invocations.
    pub calls: Arc<AtomicUsize>,
    /// Tenants observed, in call order.
    pub seen_tenants: Mutex<Vec<Uuid>>,
    /// Id sets observed, in call order — lets a test assert that holder
    /// and author ids for the same tenant arrive in ONE call.
    pub seen_ids: Mutex<Vec<Vec<String>>>,
}

impl FakePrincipalNameReader {
    /// Seed one `(tenant, id) -> name` entry; chainable.
    #[must_use]
    pub fn with_name(self, tenant: Uuid, id: &str, name: &str) -> Self {
        self.names
            .lock()
            .entry(tenant)
            .or_default()
            .insert(id.to_owned(), name.to_owned());
        self
    }

    /// Arm the fake so every call fails with `err`; chainable.
    #[must_use]
    pub fn failing(self, err: PrincipalNameError) -> Self {
        *self.fail_with.lock() = Some(err);
        self
    }

    /// Number of `user_names` calls so far.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PrincipalNameReader for FakePrincipalNameReader {
    async fn user_names(
        &self,
        _ctx: &SecurityContext,
        tenant_id: Uuid,
        ids: &[String],
    ) -> Result<HashMap<String, String>, PrincipalNameError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_tenants.lock().push(tenant_id);
        self.seen_ids.lock().push(ids.to_vec());
        if let Some(err) = self.fail_with.lock().as_ref() {
            return Err(err.clone());
        }
        let per_tenant = self.names.lock();
        let table = per_tenant.get(&tenant_id).cloned().unwrap_or_default();
        Ok(ids
            .iter()
            .filter_map(|id| table.get(id).map(|n| (id.clone(), n.clone())))
            .collect())
    }
}
