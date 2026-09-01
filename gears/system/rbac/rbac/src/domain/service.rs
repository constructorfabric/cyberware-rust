//! Pure domain services — business logic with no I/O.

pub(crate) mod builtin_roles_catalog;
pub mod caller_scope;
pub mod etag;
pub(crate) mod name_confusables;
#[doc(hidden)]
pub mod permission_evaluator;
pub(crate) mod permission_matcher;
pub mod scope_validator;
