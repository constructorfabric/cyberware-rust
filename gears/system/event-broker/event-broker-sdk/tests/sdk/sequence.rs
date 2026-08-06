//! The ordinal type's negative contract: the operations it must not offer.
//!
//! Asserted by compilation rather than at runtime, because the absence of an
//! operator is not observable from a running test.

#[test]
fn sequence_arithmetic_does_not_compile() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/trybuild/sequence/no_arithmetic.rs");
}
