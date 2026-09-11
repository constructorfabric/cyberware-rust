//! Who a set of settings belongs to.
//!
//! Settings are stored under a user key, and by default that key is the token
//! subject — the identity that signed in. For a deployment where one human has
//! exactly one way to sign in, subject and human are the same thing and there is
//! nothing to decide.
//!
//! They come apart as soon as a product lets somebody reach the same account
//! through more than one identity: an e-mail login and a brokered GitHub login,
//! two accounts merged after the fact, a migration between identity providers.
//! Then the token subject names *a way in*, not the person, and settings keyed
//! on it silently fork — the same human signs in the other way and their theme,
//! their language and everything else stored here is gone.
//!
//! A deployment that knows better can say so by publishing a
//! [`SettingsOwnerResolver`] on the `ClientHub`. Nothing else changes: the gear
//! keeps its storage, its API and its authorization, and only the key it files a
//! person's settings under comes from somewhere better informed.

use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Resolve the caller to the user key their settings are filed under.
///
/// Register an implementation on the `ClientHub` and the gear will consult it
/// for every read and write:
///
/// ```ignore
/// let resolver: Arc<dyn SettingsOwnerResolver> = Arc::new(MyResolver);
/// hub.register(resolver);
/// ```
///
/// With no implementation registered, the gear uses `ctx.subject_id()` exactly
/// as it always has, so adding this costs an existing deployment nothing.
#[async_trait]
pub trait SettingsOwnerResolver: Send + Sync + 'static {
    /// The key this caller's settings belong under, or `None` to fall back to
    /// the token subject.
    ///
    /// Returning `None` rather than an error is deliberate: a deployment whose
    /// resolver cannot answer for some callers (a service account, an identity
    /// it has never seen) should degrade to the subject, which is always
    /// available, rather than deny somebody their preferences.
    ///
    /// # Errors
    ///
    /// Only for a genuine failure to look the caller up — a database that is
    /// down, not a caller the resolver has no opinion about. The gear reports it
    /// rather than guessing, because silently filing a person's settings under
    /// the wrong key is worse than telling them the read failed.
    async fn settings_owner(&self, ctx: &SecurityContext) -> Result<Option<Uuid>, CanonicalError>;
}
