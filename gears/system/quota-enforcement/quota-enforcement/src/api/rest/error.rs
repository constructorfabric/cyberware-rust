//! REST error lift: `DomainError -> CanonicalError -> Problem` (RFC 9457).

use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::DomainError;

/// Handler result type. Every 4xx and 5xx is a `Problem`.
pub type ApiResult<T> = Result<T, Problem>;

impl From<DomainError> for Problem {
    fn from(err: DomainError) -> Self {
        Self::from(CanonicalError::from(err))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
