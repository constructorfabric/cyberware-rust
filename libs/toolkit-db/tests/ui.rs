//! Compile-fail UI tests for toolkit-db.
//!
//! These tests verify that certain incorrect usages of the secure database API
//! produce compile-time errors, ensuring security properties are enforced by
//! the type system.

#[test]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/fail/*.rs");

    // The SQL/PGQ guards live in their own directory because their expected
    // output only exists when the feature is on: without it the same file fails
    // for an unrelated reason (the module is not compiled) and the harness would
    // report a mismatch rather than the guarantee under test.
    #[cfg(feature = "pgq")]
    t.compile_fail("tests/ui/fail_pgq/*.rs");
}
