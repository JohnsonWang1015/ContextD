//! Text utilities shared by search, embeddings and the context builder.

use std::collections::HashSet;
use unicode_segmentation::UnicodeSegmentation;

/// Tokenise into lowercase word tokens.
///
/// CJK text carries no spaces, so each CJK ideograph is emitted as its own
/// token (and, in [`bigrams`], as an adjacent pair). That keeps mixed
/// English/Chinese memories searchable without pulling in a full segmenter.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.unicode_words() {
        let lower = word.to_lowercase();
        if lower.chars().any(is_cjk) {
            for ch in lower.chars() {
                out.push(ch.to_string());
            }
        } else {
            out.push(lower);
        }
    }
    out
}

/// Adjacent token pairs, used to give the local embedding a little word-order
/// sensitivity and to make CJK queries meaningful.
pub fn bigrams(tokens: &[String]) -> Vec<String> {
    tokens.windows(2).map(|w| format!("{} {}", w[0], w[1])).collect()
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF     // kana
        | 0x3400..=0x4DBF   // CJK ext A
        | 0x4E00..=0x9FFF   // CJK unified
        | 0xF900..=0xFAFF   // compatibility
        | 0xAC00..=0xD7AF) // hangul
}

/// Rough token count for context budgeting.
///
/// A real tokenizer would need the model's vocabulary; ContextD only needs a
/// conservative estimate, so it counts ~4 characters per token for latin text
/// and ~1.6 characters per token for CJK (which is denser per character).
pub fn estimate_tokens(text: &str) -> usize {
    let mut latin = 0usize;
    let mut cjk = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            latin += 1;
        }
    }
    (latin as f64 / 4.0).ceil() as usize + (cjk as f64 / 1.6).ceil() as usize
}

/// Jaccard similarity over token sets — cheap near-duplicate detection for
/// `contextd refresh`, with no embedding provider required.
pub fn jaccard(a: &str, b: &str) -> f64 {
    let sa: HashSet<String> = tokenize(a).into_iter().collect();
    let sb: HashSet<String> = tokenize(b).into_iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Truncate on a character boundary, appending an ellipsis when cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == max {
            return format!("{}…", text[..idx].trim_end());
        }
    }
    text.to_string()
}

/// Collapse a multi-line body into a single line for table output.
pub fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&joined, max)
}

/// Escape a user query for an FTS5 MATCH expression.
///
/// Every token becomes a quoted string, which makes operators such as `-`,
/// `*`, `:` and `NEAR` inert. Without this an innocent query like
/// `worker-manager` is parsed as a NOT expression and returns nothing.
pub fn fts_query(query: &str) -> String {
    let tokens = tokenize(query);
    tokens
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_cjk_per_char() {
        assert_eq!(tokenize("GPU 排程"), vec!["gpu", "排", "程"]);
    }

    #[test]
    fn tokenize_lowercases() {
        assert_eq!(tokenize("NATS Transport"), vec!["nats", "transport"]);
    }

    #[test]
    fn fts_query_neutralises_operators() {
        assert_eq!(fts_query("worker-manager"), "\"worker\" OR \"manager\"");
        assert_eq!(fts_query("NEAR*"), "\"near\"");
    }

    #[test]
    fn jaccard_bounds() {
        assert_eq!(jaccard("a b", "a b"), 1.0);
        assert_eq!(jaccard("a", "b"), 0.0);
        assert!((jaccard("a b", "a c") - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn truncate_is_utf8_safe() {
        let s = truncate_chars("排程器使用 NATS", 4);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn estimate_tokens_counts_cjk_denser() {
        assert!(estimate_tokens("排程器") > estimate_tokens("abc"));
    }
}
