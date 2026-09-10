//! Contract validation — the gate every check passes before delegation.
//!
//! Shape and compatibility only. What a `metadata` property *means*, and whether
//! the pair is actually licensed, stay with the backend; a request that fails
//! here is rejected as a violation and never reaches a plugin.

use std::sync::Arc;

use gts::GtsSchema;
use gts::schema_modifiers::X_GTS_ABSTRACT;
use license_resolver_sdk::gts::{LicenseResourceV1, LicenseSubjectV1};
use license_resolver_sdk::{FieldViolation, LicenseCheckRequest, field};
use serde::Serialize;
use serde_json::Value;
use toolkit_macros::domain_model;
use types_registry_sdk::GtsTypeSchema;

use super::error::DomainError;
use super::ports::{ContractRegistry, ContractRegistryError};

/// Trait key a derived Resource contract declares its admitted Subject contracts
/// under. Mirrors the `admitted_subjects` field of the SDK's `ResourceTraits`.
const ADMITTED_SUBJECTS_TRAIT: &str = "admitted_subjects";

/// Upper bound on the violations reported for one contract object.
///
/// Without it the error payload grows with the caller's own input — an object
/// with thousands of unexpected properties would produce a violation per
/// property, all of it rendered into the RFC-9457 body.
const MAX_REPORTED_VIOLATIONS: usize = 20;

/// Which half of the request a contract object came from.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Subject,
    Resource,
}

impl Role {
    fn field_root(self) -> &'static str {
        match self {
            Self::Subject => field::SUBJECT_FIELD,
            Self::Resource => field::RESOURCE_FIELD,
        }
    }

    fn type_field(self) -> &'static str {
        match self {
            Self::Subject => field::SUBJECT_TYPE_FIELD,
            Self::Resource => field::RESOURCE_TYPE_FIELD,
        }
    }

    /// The licensing base a contract in this slot must derive from.
    fn base_type_id(self) -> &'static str {
        match self {
            Self::Subject => <LicenseSubjectV1<()> as GtsSchema>::TYPE_ID,
            Self::Resource => <LicenseResourceV1<()> as GtsSchema>::TYPE_ID,
        }
    }
}

/// Validates a check request against the contracts it declares.
#[domain_model]
pub struct ContractValidator {
    registry: Arc<dyn ContractRegistry>,
}

impl ContractValidator {
    #[must_use]
    pub fn new(registry: Arc<dyn ContractRegistry>) -> Self {
        Self { registry }
    }

    /// Validate the request, reporting every violation it carries.
    ///
    /// # Errors
    ///
    /// - [`DomainError::ContractViolation`] — the request does not conform.
    /// - [`DomainError::TypesRegistryUnavailable`] — a contract could not be
    ///   resolved, so conformance cannot be determined.
    /// - [`DomainError::ContractUnusable`] — a registered contract is unusable.
    /// - [`DomainError::Internal`] — a contract object failed to serialize.
    pub async fn validate(&self, request: &LicenseCheckRequest) -> Result<(), DomainError> {
        let mut violations = Vec::new();
        let subject_type = request.subject.gts_type.as_ref();

        let resource_contract = self
            .resolve(
                Role::Resource,
                request.resource.gts_type.as_ref(),
                &mut violations,
            )
            .await?;
        let subject_contract = self
            .resolve(Role::Subject, subject_type, &mut violations)
            .await?;

        if let Some(contract) = resource_contract.as_ref() {
            validate_object(Role::Resource, contract, &request.resource, &mut violations)?;
        }
        if let Some(contract) = subject_contract.as_ref() {
            validate_object(Role::Subject, contract, &request.subject, &mut violations)?;
        }
        // Admissibility is a statement about a *pair*, so it is only meaningful
        // once both contracts resolved — otherwise it would pile a second,
        // derivative violation on top of the one that already explains the
        // failure.
        if let (Some(resource_contract), Some(_)) = (&resource_contract, &subject_contract) {
            validate_admitted(resource_contract, subject_type, &mut violations)?;
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(DomainError::ContractViolation { violations })
        }
    }

    /// `Ok(None)` means a violation was recorded and the object cannot be
    /// validated further; `Err` is reserved for cannot-determine conditions.
    async fn resolve(
        &self,
        role: Role,
        type_id: &str,
        violations: &mut Vec<FieldViolation>,
    ) -> Result<Option<GtsTypeSchema>, DomainError> {
        let contract = match self.registry.contract_schema(type_id).await {
            Ok(contract) => contract,
            Err(ContractRegistryError::Unregistered) => {
                violations.push(FieldViolation::new(
                    role.type_field(),
                    format!("licensing contract '{type_id}' is not registered"),
                    field::CONTRACT_NOT_REGISTERED,
                ));
                return Ok(None);
            }
            Err(ContractRegistryError::MalformedTypeId(reason)) => {
                violations.push(FieldViolation::new(
                    role.type_field(),
                    format!("'{type_id}' is not a well-formed GTS type id: {reason}"),
                    field::CONTRACT_TYPE_MALFORMED,
                ));
                return Ok(None);
            }
            Err(ContractRegistryError::Unavailable(reason)) => {
                return Err(DomainError::TypesRegistryUnavailable(reason));
            }
        };

        // `x-gts-abstract` is an own declaration and is never inherited, so this
        // reads the contract's own body rather than the merged chain.
        if contract.raw_schema.get(X_GTS_ABSTRACT) == Some(&Value::Bool(true)) {
            violations.push(FieldViolation::new(
                role.type_field(),
                format!(
                    "licensing contract '{type_id}' is abstract; a check names a derived contract"
                ),
                field::CONTRACT_ABSTRACT,
            ));
            return Ok(None);
        }

        // `ancestors` yields the contract itself first, so skipping it demands a
        // *proper* descendant — a base presented as its own derivation is not one.
        let base = role.base_type_id();
        if !contract
            .ancestors()
            .skip(1)
            .any(|ancestor| ancestor.type_id.as_ref() == base)
        {
            violations.push(FieldViolation::new(
                role.type_field(),
                format!("licensing contract '{type_id}' does not derive from '{base}'"),
                field::CONTRACT_NOT_DERIVED,
            ));
            return Ok(None);
        }

        Ok(Some(contract))
    }
}

fn validate_object<T: Serialize>(
    role: Role,
    contract: &GtsTypeSchema,
    object: &T,
    violations: &mut Vec<FieldViolation>,
) -> Result<(), DomainError> {
    let contract_id = contract.type_id.as_ref();
    let schema = contract.effective_schema();

    // The draft is pinned rather than detected: a GTS chain id can appear in
    // `$schema`, and it is not a registered meta-schema, so autodetection fails
    // the whole compile.
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .build(&schema)
        .map_err(|e| DomainError::ContractUnusable {
            type_id: contract_id.to_owned(),
            reason: format!("not a valid JSON Schema: {e}"),
        })?;

    let payload = serde_json::to_value(object).map_err(|e| {
        DomainError::Internal(format!(
            "failed to serialize the {} object: {e}",
            role.field_root()
        ))
    })?;

    let root = role.field_root();
    for error in validator
        .iter_errors(&payload)
        .take(MAX_REPORTED_VIOLATIONS)
    {
        let pointer = error.instance_path();
        violations.push(FieldViolation::new(
            format!("{root}{pointer}"),
            format!("{contract_id}: {error}"),
            field::SCHEMA_MISMATCH,
        ));
    }

    Ok(())
}

fn validate_admitted(
    resource_contract: &GtsTypeSchema,
    subject_type: &str,
    violations: &mut Vec<FieldViolation>,
) -> Result<(), DomainError> {
    let contract_id = resource_contract.type_id.as_ref();
    let traits = resource_contract.effective_traits();

    let declared = match traits.get(ADMITTED_SUBJECTS_TRAIT) {
        // Absent or null resolves to the same answer as an empty list: a
        // contract that declares nothing admits nobody.
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(items)) => items.as_slice(),
        Some(_) => {
            return Err(DomainError::ContractUnusable {
                type_id: contract_id.to_owned(),
                reason: format!("trait `{ADMITTED_SUBJECTS_TRAIT}` is not an array"),
            });
        }
    };

    let mut admitted = Vec::with_capacity(declared.len());
    for item in declared {
        // One malformed entry voids the whole list: a non-string must not slip
        // through under cover of legitimate siblings.
        let Some(id) = item.as_str() else {
            return Err(DomainError::ContractUnusable {
                type_id: contract_id.to_owned(),
                reason: format!("trait `{ADMITTED_SUBJECTS_TRAIT}` contains a non-string entry"),
            });
        };
        admitted.push(id);
    }

    if !admitted.contains(&subject_type) {
        violations.push(FieldViolation::new(
            field::SUBJECT_TYPE_FIELD,
            format!(
                "'{subject_type}' is not admitted by '{contract_id}' (admitted: [{}])",
                admitted.join(", ")
            ),
            field::SUBJECT_NOT_ADMITTED,
        ));
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "validation_tests.rs"]
mod validation_tests;
