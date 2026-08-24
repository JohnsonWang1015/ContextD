//! Identifier generation.
//!
//! ContextD uses lowercase UUIDv4 strings as primary keys. They are stable
//! across export/import and safe to embed in Markdown.

use uuid::Uuid;

/// Generate a fresh record id.
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Short, human-typeable prefix used in CLI output (`8f3a1c2d`).
pub fn short(id: &str) -> &str {
    let end = id.char_indices().nth(8).map_or(id.len(), |(i, _)| i);
    &id[..end]
}

/// Normalise a user supplied slug: lowercase, non-alphanumeric collapsed to `-`.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = true; // avoids a leading dash
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalises() {
        assert_eq!(slugify("FerroGrid Scheduler!"), "ferrogrid-scheduler");
        assert_eq!(slugify("  --a--b  "), "a-b");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn short_is_utf8_safe() {
        assert_eq!(short("0123456789"), "01234567");
        assert_eq!(short("abc"), "abc");
    }
}
