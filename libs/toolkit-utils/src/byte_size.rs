//! Byte-size constants, because Rust has none.
//!
//! A byte bound written as `64 * 1024 * 1024` is read by counting zeros, and
//! the standard library offers no constant or unit type to write it any other
//! way. These do, and they compose: `96 * MIB`, `2 * GIB`.
//!
//! Two families, differing only in type. A limit compared against a database
//! counter is `i64`; a length compared against `Vec::len` is `usize`. Casting
//! between them in a `const` buys nothing but a lint, so both exist and each
//! is named for what it measures.

/// One kibibyte, for values compared against database counters.
pub const KIB: i64 = 1024;

/// One mebibyte, for values compared against database counters.
pub const MIB: i64 = KIB * 1024;

/// One gibibyte, for values compared against database counters.
pub const GIB: i64 = MIB * 1024;

/// One kibibyte, for values compared against in-memory lengths.
pub const KIB_LEN: usize = 1024;

/// One mebibyte, for values compared against in-memory lengths.
pub const MIB_LEN: usize = KIB_LEN * 1024;

/// One gibibyte, for values compared against in-memory lengths.
pub const GIB_LEN: usize = MIB_LEN * 1024;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_two_families_agree() {
        assert_eq!(KIB, 1024);
        assert_eq!(MIB, 1_048_576);
        assert_eq!(GIB, 1_073_741_824);
        assert_eq!(usize::try_from(KIB).unwrap(), KIB_LEN);
        assert_eq!(usize::try_from(MIB).unwrap(), MIB_LEN);
        assert_eq!(usize::try_from(GIB).unwrap(), GIB_LEN);
    }
}
