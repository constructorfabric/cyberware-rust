//! Metrics vocabulary and recording port for credential-store operations.
//!
//! Defines bounded labels for outcomes, dependencies, value-fingerprint
//! verification, and the maintenance job's garbage-collection counters
//! (ADR-0006). No inventory gauges: the shipped reaper's per-status row-count
//! and gc-queue-depth gauges were `COUNT … GROUP BY` queries, forbidden by
//! the platform's no-`COUNT` rule, and are withdrawn rather than reimplemented.

use toolkit_macros::domain_model;

#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    HitOwn,
    HitInherited,
    Miss,
}
impl ReadOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HitOwn => "hit_own",
            Self::HitInherited => "hit_inherited",
            Self::Miss => "miss",
        }
    }
}

#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    TenantResolver,
    Plugin,
    Pdp,
    TypesRegistry,
}
impl Dep {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TenantResolver => "tenant_resolver",
            Self::Plugin => "plugin",
            Self::Pdp => "pdp",
            Self::TypesRegistry => "types_registry",
        }
    }
}

#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepOp {
    GetAncestors,
    PluginGet,
    PluginPut,
    PluginDelete,
    Evaluate,
    GetTypeSchemaByUuid,
}
impl DepOp {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GetAncestors => "get_ancestors",
            Self::PluginGet => "plugin_get",
            Self::PluginPut => "plugin_put",
            Self::PluginDelete => "plugin_delete",
            Self::Evaluate => "evaluate",
            Self::GetTypeSchemaByUuid => "get_type_schema_by_uuid",
        }
    }
}

/// Value-fingerprint fence verdict for a read (ADR-0003, narrowed by
/// ADR-0006 to an integrity check only — no more out-of-band-seeded
/// "legacy" case). `Mismatch` is the fail-closed anti-enumeration miss — the
/// alertable signal.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceVerify {
    Ok,
    Mismatch,
}
impl FenceVerify {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Mismatch => "mismatch",
        }
    }
}

#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    NotFound,
    Error,
}
impl Outcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotFound => "not_found",
            Self::Error => "error",
        }
    }
}

pub trait CredStoreMetricsPort: Send + Sync + 'static {
    fn read_outcome(&self, outcome: ReadOutcome);
    fn walkup_depth(&self, depth: u64);
    fn dependency(&self, dep: Dep, op: DepOp, outcome: Outcome, secs: f64);
    fn cross_tenant_denied(&self);
    /// Records the fence verdict of a read (`ok`/`mismatch`); `mismatch` is
    /// the fail-closed 404 worth alerting on.
    fn fence_verify(&self, outcome: FenceVerify);
    /// Maintenance job (`credstore gc`): versions deleted by the gc drain
    /// (`reason != pending`: superseded/removed/aborted).
    fn gc_deleted(&self, n: u64);
    /// Maintenance job: orphaned `pending` versions reclaimed (older than
    /// `gc.pending_max_age_secs` and unreferenced by any row) — a sustained
    /// climb here means writes are crashing or timing out before commit.
    fn gc_pending_reclaimed(&self, n: u64);
    /// Maintenance job: expired `active` rows removed (and their versions
    /// enqueued for collection).
    fn expired_deleted(&self, n: u64);
}

#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;
impl CredStoreMetricsPort for NoopMetrics {
    fn read_outcome(&self, _: ReadOutcome) {}
    fn walkup_depth(&self, _: u64) {}
    fn dependency(&self, _: Dep, _: DepOp, _: Outcome, _: f64) {}
    fn cross_tenant_denied(&self) {}
    fn fence_verify(&self, _: FenceVerify) {}
    fn gc_deleted(&self, _: u64) {}
    fn gc_pending_reclaimed(&self, _: u64) {}
    fn expired_deleted(&self, _: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_snake_case() {
        assert_eq!(ReadOutcome::HitInherited.as_str(), "hit_inherited");
        assert_eq!(Dep::TenantResolver.as_str(), "tenant_resolver");
        assert_eq!(DepOp::PluginGet.as_str(), "plugin_get");
        assert_eq!(Outcome::NotFound.as_str(), "not_found");
    }

    #[test]
    fn all_label_variants_render() {
        assert_eq!(ReadOutcome::HitOwn.as_str(), "hit_own");
        assert_eq!(ReadOutcome::Miss.as_str(), "miss");
        assert_eq!(Dep::Plugin.as_str(), "plugin");
        assert_eq!(Dep::Pdp.as_str(), "pdp");
        assert_eq!(Dep::TypesRegistry.as_str(), "types_registry");
        assert_eq!(DepOp::GetAncestors.as_str(), "get_ancestors");
        assert_eq!(DepOp::PluginPut.as_str(), "plugin_put");
        assert_eq!(DepOp::PluginDelete.as_str(), "plugin_delete");
        assert_eq!(DepOp::Evaluate.as_str(), "evaluate");
        assert_eq!(
            DepOp::GetTypeSchemaByUuid.as_str(),
            "get_type_schema_by_uuid"
        );
        assert_eq!(Outcome::Success.as_str(), "success");
        assert_eq!(Outcome::Error.as_str(), "error");
        assert_eq!(FenceVerify::Ok.as_str(), "ok");
        assert_eq!(FenceVerify::Mismatch.as_str(), "mismatch");
    }

    #[test]
    fn noop_metrics_port_is_inert() {
        let noop = NoopMetrics;
        noop.read_outcome(ReadOutcome::Miss);
        noop.walkup_depth(3);
        noop.dependency(Dep::Pdp, DepOp::Evaluate, Outcome::Success, 0.1);
        noop.cross_tenant_denied();
        noop.fence_verify(FenceVerify::Ok);
        noop.gc_deleted(2);
        noop.gc_pending_reclaimed(1);
        noop.expired_deleted(4);
    }
}
