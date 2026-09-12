//! REST API for ledger provisioning, posting, payments, adjustments, revenue
//! recognition, reconciliation, audit, and period-close operations.
//!
//! Handlers share authenticated request context, DTOs, and canonical error
//! mapping so authorization and body failures become RFC 9457 `Problem`
//! responses.

pub mod adjustments;
pub mod approvals;
pub mod audit;
pub(crate) mod auth_context;
pub(crate) mod canonical_json;
pub mod closure;
pub mod control;
pub mod credit;
pub mod disputes;
pub mod dto;
pub mod error;
pub mod exceptions;
pub mod fx;
pub mod fx_revaluation_mode;
pub mod journal_entries;
pub(crate) mod odata_list;
pub mod payers;
pub mod payments;
pub mod posting_policy;
pub mod provisioning;
pub mod recognition;
pub mod reconciliation;
pub mod refunds;
