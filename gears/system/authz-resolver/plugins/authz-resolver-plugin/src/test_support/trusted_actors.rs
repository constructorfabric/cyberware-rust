//! Trusted-actor fixtures. Which in-process actors a deployment trusts is
//! configuration, so the plugin compiles in none — tests supply their own.

use crate::config::TrustedSystemActor;
use crate::domain::subject_type::TrustedSystemActors;

/// Fixture subject-type tags.
pub const AM_SYSTEM_SUBJECT_TYPE: &str = "am.system";
pub const RMS_SYSTEM_SUBJECT_TYPE: &str = "rms.system";
/// Fixture subject ids, shaped like the in-process sentinels a host mints
/// (version nibble `cf01`/`cf02`, so neither is a valid v4/v5 UUID).
pub const AM_SYSTEM_ACTOR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000_0000_0000_cf01_0000_616d_7379_7374);
pub const RMS_SYSTEM_ACTOR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000_0000_0000_cf02_0000_726d_7379_7374);

/// The two fixture pairs, as a deployment would configure them.
#[must_use]
pub fn trusted_actors() -> TrustedSystemActors {
    TrustedSystemActors::from_config(&[
        TrustedSystemActor {
            subject_type: AM_SYSTEM_SUBJECT_TYPE.to_owned(),
            subject_id: AM_SYSTEM_ACTOR_UUID,
        },
        TrustedSystemActor {
            subject_type: RMS_SYSTEM_SUBJECT_TYPE.to_owned(),
            subject_id: RMS_SYSTEM_ACTOR_UUID,
        },
    ])
}
