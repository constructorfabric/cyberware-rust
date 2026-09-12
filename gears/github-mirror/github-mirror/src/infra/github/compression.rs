//! Body compression for the raw-response cache (PRD §5.6).
//!
//! Cached bodies are GitHub JSON, which gzips to roughly a fifth of its size,
//! so compression is the difference between a cache that is cheap to keep and
//! one that is not.
//!
//! The content hash is always computed over the **uncompressed** body, so an
//! integrity check does not depend on which mode was in force when the entry
//! was written — a requirement of PRD §5.6, and what makes changing the mode
//! safe for entries already stored.

use std::io::{Read as _, Write as _};

use aws_lc_rs::digest::{self, SHA256};

use crate::domain::error::DomainError;

/// How a cached body is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    /// Stored verbatim.
    None,
    /// Deflate with a gzip wrapper.
    #[default]
    Gzip,
    /// Zstandard. Declared by the design but not built: it needs a workspace
    /// dependency the repository does not carry yet, so selecting it fails at
    /// startup rather than silently falling back.
    Zstd,
}

impl Compression {
    /// Parse `none`, `gzip` or `zstd`.
    ///
    /// # Errors
    /// `Validation` for anything else.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(Self::None),
            "gzip" | "gz" => Ok(Self::Gzip),
            "zstd" | "zst" => Ok(Self::Zstd),
            other => Err(DomainError::Validation {
                field: "compression".to_owned(),
                message: format!("unknown compression `{other}` (valid: none, gzip, zstd)"),
            }),
        }
    }

    /// The name stored alongside each entry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        }
    }

    /// Compress `body` for storage.
    ///
    /// # Errors
    /// `Internal` when the encoder fails, or when the mode is not built.
    pub fn compress(self, body: &[u8]) -> Result<Vec<u8>, DomainError> {
        match self {
            Self::None => Ok(body.to_vec()),
            Self::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder
                    .write_all(body)
                    .map_err(|e| DomainError::internal(format!("gzip write failed: {e}")))?;
                encoder
                    .finish()
                    .map_err(|e| DomainError::internal(format!("gzip finish failed: {e}")))
            }
            Self::Zstd => Err(Self::zstd_unavailable()),
        }
    }

    /// Restore a stored body.
    ///
    /// # Errors
    /// `Internal` when the stored bytes do not decode, which means the entry
    /// is corrupt and should be treated as a miss.
    pub fn decompress(self, stored: &[u8]) -> Result<Vec<u8>, DomainError> {
        match self {
            Self::None => Ok(stored.to_vec()),
            Self::Gzip => {
                let mut decoder = flate2::read::GzDecoder::new(stored);
                let mut body = Vec::new();
                decoder
                    .read_to_end(&mut body)
                    .map_err(|e| DomainError::internal(format!("gzip read failed: {e}")))?;
                Ok(body)
            }
            Self::Zstd => Err(Self::zstd_unavailable()),
        }
    }

    fn zstd_unavailable() -> DomainError {
        DomainError::internal(
            "zstd compression is declared by the design but not built into this gear; \
             use `none` or `gzip`",
        )
    }
}

/// Hex SHA-256 of the **uncompressed** body, for integrity checks.
#[must_use]
pub fn content_hash(body: &[u8]) -> String {
    hex::encode(digest::digest(&SHA256, body).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"[{"id":1,"title":"an issue"},{"id":2,"title":"another"}]"#;

    #[test]
    fn none_stores_the_body_verbatim() {
        let stored = Compression::None.compress(BODY).unwrap();
        assert_eq!(stored, BODY);
        assert_eq!(Compression::None.decompress(&stored).unwrap(), BODY);
    }

    #[test]
    fn gzip_round_trips() {
        let stored = Compression::Gzip.compress(BODY).unwrap();
        assert_ne!(stored, BODY, "the stored form is encoded");
        assert_eq!(Compression::Gzip.decompress(&stored).unwrap(), BODY);
    }

    #[test]
    fn the_hash_is_over_the_uncompressed_body() {
        let plain = Compression::None.compress(BODY).unwrap();
        let gzipped = Compression::Gzip.compress(BODY).unwrap();
        assert_ne!(plain, gzipped);
        assert_eq!(
            content_hash(&Compression::None.decompress(&plain).unwrap()),
            content_hash(&Compression::Gzip.decompress(&gzipped).unwrap()),
            "the mode must not change the integrity hash"
        );
    }

    #[test]
    fn corrupt_bytes_do_not_decode() {
        assert!(Compression::Gzip.decompress(b"not gzip at all").is_err());
    }

    #[test]
    fn modes_parse_and_round_trip_their_names() {
        for (text, mode) in [
            ("none", Compression::None),
            ("GZIP", Compression::Gzip),
            ("zstd", Compression::Zstd),
        ] {
            let parsed = Compression::parse(text).unwrap();
            assert_eq!(parsed, mode);
            assert_eq!(Compression::parse(parsed.as_str()).unwrap(), mode);
        }
        assert!(Compression::parse("lzma").is_err());
    }

    #[test]
    fn zstd_fails_loudly_rather_than_falling_back() {
        assert!(Compression::Zstd.compress(BODY).is_err());
        assert!(Compression::Zstd.decompress(BODY).is_err());
    }
}
