//! Crate-private test helpers — the `stub_impl!` macro used by service-handler
//! tests. Compiled only under `#[cfg(test)]`.

// Currently unused: the service tests under `tests/api_*.rs` run against real
// repos on testcontainer Postgres rather than per-trait stubs. Kept for the
// per-service tests (`domain/<aggregate>/service_tests.rs`) that will want it.
#[allow(unused_macros)]
/// Emit a complete `#[async_trait] impl Trait for Stub { ... }` block from a
/// compact signature list.
///
/// * `methods = [...]` — signatures whose bodies the macro fills in with a
///   `panic!` carrying `StubLabel::<method>`. Use for methods the test does
///   NOT exercise.
/// * `custom = { ... }` — full `async fn` definitions written by the caller
///   for the methods the test DOES exercise.
///
/// The macro owns the whole impl block including `#[async_trait]` so the
/// attribute processes a fully-formed item (avoids E0195 from nested
/// `macro_rules`! expansion).
///
/// # Example
///
/// ```ignore
/// stub_impl! {
///     impl RoleDefinitionRepository => for StubRepo,
///     stub_label = "StubRepo",
///         async fn create(_new: NewRoleDefinition) -> Result<RoleDefinition, RoleDefinitionRepoError>;
///         async fn delete(_id: Uuid, _expected_etag: &Etag)
///             -> Result<(), RoleDefinitionRepoError>;
///     custom = {
///         async fn find_by_id<C: toolkit_db::secure::DBRunner>(
///             &self,
///             _db: &C,
///             id: Uuid,
///         ) -> Result<Option<RoleDefinition>, RoleDefinitionRepoError> {
///             if id == self.seeded.id {
///                 Ok(Some(self.seeded.clone()))
///             } else {
///                 Ok(None)
///             }
///         }
///     }
/// }
/// ```
macro_rules! stub_impl {
    (
        impl $trait_path:path => for $stub_ty:ident,
        stub_label = $stub:literal,
        methods = [
            $(
                async fn $name:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? )
                    -> $ret:ty ;
            )*
        ]
        $(, custom = { $($custom:tt)* } )?
        $(,)?
    ) => {
        #[async_trait::async_trait]
        impl $trait_path for $stub_ty {
            $(
                // The repo traits take the executor as `<C: DBRunner>`; the
                // stub ignores it, but the signature has to match.
                async fn $name<C: toolkit_db::secure::DBRunner>(
                    &self,
                    _db: &C,
                    $( $arg : $arg_ty ),*
                ) -> $ret {
                    panic!(concat!(
                        $stub, "::", stringify!($name),
                        " called but stub provides no canned response"
                    ))
                }
            )*
            $( $($custom)* )?
        }
    };
}
