//! Content hashing for sync conflict detection.

use sha2::{Digest, Sha256};

/// Hex SHA-256 of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Hash of text with line endings normalised, so a file that only differs by
/// CRLF/LF (a very common Windows round-trip) is not reported as a conflict.
pub fn content_hash(text: &str) -> String {
    let normalised: String = text.replace("\r\n", "\n");
    sha256_hex(normalised.trim_end().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_do_not_change_hash() {
        assert_eq!(content_hash("a\r\nb\r\n"), content_hash("a\nb"));
    }

    #[test]
    fn different_content_differs() {
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}
