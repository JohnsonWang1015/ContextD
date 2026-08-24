//! Terminal output.
//!
//! ContextD's CLI is meant to be read at a glance, so output is aligned,
//! quiet, and colourful only when a human is looking. Colour is disabled when
//! stdout is not a terminal, when `NO_COLOR` is set (the de-facto standard),
//! when `TERM=dumb`, or when the user says so in config or on the command line.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static COLOR: AtomicBool = AtomicBool::new(false);

/// Decide once, at startup, whether to emit ANSI codes.
pub fn init_color(preference: &str, force_off: bool) {
    let enabled = if force_off {
        false
    } else {
        match preference.trim().to_lowercase().as_str() {
            "always" => true,
            "never" => false,
            // "auto" and anything unrecognised
            _ => {
                std::io::stdout().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none()
                    && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
            }
        }
    };
    COLOR.store(enabled, Ordering::Relaxed);
}

/// Whether colour is currently enabled.
pub fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

fn paint(code: &str, text: &str) -> String {
    if color_enabled() {
        format!("\u{1b}[{code}m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(text: &str) -> String {
    paint("1", text)
}

pub fn dim(text: &str) -> String {
    paint("2", text)
}

pub fn green(text: &str) -> String {
    paint("32", text)
}

pub fn yellow(text: &str) -> String {
    paint("33", text)
}

pub fn red(text: &str) -> String {
    paint("31", text)
}

pub fn cyan(text: &str) -> String {
    paint("36", text)
}

/// Section header with a rule beneath it.
pub fn header(title: &str) -> String {
    format!("{}\n{}", bold(title), dim(&"─".repeat(title.chars().count().max(33))))
}

/// Aligned `key   value` block.
pub fn kv(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(k, _)| display_width(k)).max().unwrap_or(0);
    rows.iter()
        .map(|(key, value)| {
            let padding = " ".repeat(width.saturating_sub(display_width(key)) + 2);
            format!("{}{padding}{value}", dim(key))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Simple column table with a dimmed header row.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
    }

    let mut out = String::new();
    let header_line: Vec<String> =
        headers.iter().enumerate().map(|(i, h)| pad(h, widths[i])).collect();
    out.push_str(&dim(header_line.join("  ").trim_end()));
    out.push('\n');

    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| if i < widths.len() { pad(cell, widths[i]) } else { cell.clone() })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Pad to a display width, accounting for wide (CJK) characters.
fn pad(text: &str, width: usize) -> String {
    let current = display_width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(current)))
}

/// Approximate terminal width of a string: CJK and emoji occupy two cells.
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| {
            if is_wide(ch) {
                2
            } else if ch.is_control() {
                0
            } else {
                1
            }
        })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF)
}

/// `✓` when true, `–` when false.
pub fn check(ok: bool) -> String {
    if ok {
        green("✓")
    } else {
        dim("–")
    }
}

/// Success line.
pub fn ok(message: &str) -> String {
    format!("{} {message}", green("✓"))
}

/// Warning line.
pub fn warn(message: &str) -> String {
    format!("{} {message}", yellow("!"))
}

/// Error line.
pub fn error(message: &str) -> String {
    format!("{} {message}", red("✗"))
}

/// Hint shown under a result.
pub fn hint(message: &str) -> String {
    dim(&format!("  {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_can_be_forced_off() {
        init_color("always", true);
        assert!(!color_enabled());
        assert_eq!(bold("x"), "x");
        init_color("always", false);
        assert!(color_enabled());
        assert!(bold("x").contains("\u{1b}["));
        init_color("never", false);
        assert_eq!(green("x"), "x");
    }

    #[test]
    fn kv_aligns_keys() {
        init_color("never", false);
        let text = kv(&[("Project", "FerroGrid".into()), ("Branch", "main".into())]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Project  FerroGrid");
        assert_eq!(lines[1], "Branch   main");
    }

    #[test]
    fn table_pads_columns() {
        init_color("never", false);
        let text = table(
            &["id", "title"],
            &[vec!["a".into(), "short".into()], vec!["bbbb".into(), "x".into()]],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "id    title");
        assert_eq!(lines[1], "a     short");
        assert_eq!(lines[2], "bbbb  x");
    }

    #[test]
    fn empty_table_is_empty() {
        assert_eq!(table(&["a"], &[]), "");
    }

    #[test]
    fn cjk_counts_as_double_width() {
        assert_eq!(display_width("排程"), 4);
        assert_eq!(display_width("ab"), 2);
        init_color("never", false);
        // A CJK cell and an ASCII cell should line up.
        let text = table(&["a"], &[vec!["排程".into()], vec!["abcd".into()]]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(display_width(lines[1]), display_width(lines[2]));
    }
}
