//! Parsing agent Markdown files into candidate memories, and splicing
//! generated content back into them without destroying what the user wrote.

use crate::core::model::Category;

/// Marker pair delimiting the region ContextD owns inside a shared file.
pub const BEGIN_MARKER: &str = "<!-- contextd:begin -->";
pub const END_MARKER: &str = "<!-- contextd:end -->";

/// A section extracted from an agent file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub heading: String,
    pub body: String,
    pub category: Category,
}

/// Split a Markdown document into headed sections.
///
/// Content before the first heading is returned under an empty heading, so an
/// unstructured CLAUDE.md still imports as one memory rather than vanishing.
pub fn sections(text: &str) -> Vec<Section> {
    let body_text = strip_front_matter(&strip_managed_block(text));
    let mut sections: Vec<Section> = Vec::new();
    let mut heading = String::new();
    let mut buffer: Vec<&str> = Vec::new();
    let mut in_code_fence = false;

    for line in body_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
        }
        // A `#` inside a fenced block is code, not a heading.
        if !in_code_fence && trimmed.starts_with('#') {
            push_section(&mut sections, &heading, &buffer);
            heading = trimmed.trim_start_matches('#').trim().to_string();
            buffer.clear();
        } else {
            buffer.push(line);
        }
    }
    push_section(&mut sections, &heading, &buffer);
    sections
}

fn push_section(sections: &mut Vec<Section>, heading: &str, buffer: &[&str]) {
    let body = buffer.join("\n").trim().to_string();
    if body.is_empty() && heading.trim().is_empty() {
        return;
    }
    if body.is_empty() {
        return; // a heading with no content carries no memory
    }
    sections.push(Section {
        category: guess_category(heading, &body),
        heading: heading.to_string(),
        body,
    });
}

/// Guess a category from a heading, falling back to the body.
///
/// Import is a best-effort convenience; a wrong guess is a one-word fix with
/// `contextd edit`, whereas refusing to import at all would mean re-typing a
/// year of accumulated instructions.
pub fn guess_category(heading: &str, body: &str) -> Category {
    let text = format!("{heading} {body}").to_lowercase();
    const RULES: [(&[&str], Category); 8] = [
        (
            &["architecture", "design", "component", "system", "data flow", "架構"],
            Category::Architecture,
        ),
        (&["decision", "adr", "chose", "we picked", "決策"], Category::Decision),
        (
            &["convention", "style", "lint", "format", "naming", "規範", "風格"],
            Category::Convention,
        ),
        (&["todo", "task", "roadmap", "next step", "in progress", "待辦"], Category::Task),
        (&["preference", "always", "never", "please", "偏好"], Category::User),
        (&["command", "build", "test", "run", "setup", "install"], Category::Knowledge),
        (&["link", "reference", "docs", "documentation", "參考"], Category::Reference),
        (&["overview", "project", "about", "專案"], Category::Project),
    ];
    for (keywords, category) in RULES {
        if keywords.iter().any(|k| text.contains(k)) {
            return category;
        }
    }
    Category::Project
}

/// Drop leading YAML front matter (Cursor `.mdc` rules, and any Markdown that
/// carries metadata). It describes when a rule applies; it is not a memory.
pub fn strip_front_matter(text: &str) -> String {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return text.to_string();
    };
    let Some(rest) = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n")) else {
        return text.to_string();
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches(['\r', '\n']).to_string(),
        // Unterminated front matter: treat the text as ordinary content rather
        // than silently discarding all of it.
        None => text.to_string(),
    }
}

/// Everything outside the ContextD-managed block.
pub fn strip_managed_block(text: &str) -> String {
    match block_bounds(text) {
        Some((start, end)) => {
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..start]);
            out.push_str(&text[end..]);
            out.trim().to_string()
        }
        None => text.trim().to_string(),
    }
}

/// The current contents of the managed block, if the file has one.
pub fn managed_block(text: &str) -> Option<String> {
    let (start, end) = block_bounds(text)?;
    let inner = &text[start + BEGIN_MARKER.len()..end - END_MARKER.len()];
    Some(inner.trim().to_string())
}

/// Insert or replace the managed block, leaving user content untouched.
///
/// A file that has never been managed gets the block appended rather than
/// overwritten: the user's own CLAUDE.md instructions must survive.
pub fn splice_managed_block(existing: &str, generated: &str) -> String {
    let block = format!("{BEGIN_MARKER}\n{}\n{END_MARKER}", generated.trim());
    match block_bounds(existing) {
        Some((start, end)) => {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..start]);
            out.push_str(&block);
            out.push_str(&existing[end..]);
            normalise_trailing(&out)
        }
        None if existing.trim().is_empty() => normalise_trailing(&block),
        None => normalise_trailing(&format!("{}\n\n{block}", existing.trim_end())),
    }
}

/// Byte range covering the whole block, markers included.
fn block_bounds(text: &str) -> Option<(usize, usize)> {
    let start = text.find(BEGIN_MARKER)?;
    let end_marker = text[start..].find(END_MARKER)? + start;
    Some((start, end_marker + END_MARKER.len()))
}

fn normalise_trailing(text: &str) -> String {
    format!("{}\n", text.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_headings() {
        let text = "# Project\nFerroGrid schedules GPUs.\n\n## Conventions\nUse rustfmt.\n";
        let sections = sections(text);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Project");
        assert_eq!(sections[1].category, Category::Convention);
    }

    #[test]
    fn content_before_the_first_heading_is_kept() {
        let sections = sections("Just some notes about the build.\n\n# Later\nmore");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "");
        assert!(sections[0].body.contains("Just some notes"));
    }

    #[test]
    fn hashes_inside_code_fences_are_not_headings() {
        let text = "# Real\n```sh\n# not a heading\ncargo build\n```\n";
        let sections = sections(text);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].body.contains("# not a heading"));
    }

    #[test]
    fn empty_sections_are_dropped() {
        assert!(sections("# Heading with no body\n").is_empty());
        assert!(sections("   ").is_empty());
    }

    #[test]
    fn category_guesses() {
        assert_eq!(guess_category("Architecture", ""), Category::Architecture);
        assert_eq!(guess_category("Coding style", ""), Category::Convention);
        assert_eq!(guess_category("架構", ""), Category::Architecture);
        assert_eq!(guess_category("Anything else", "random"), Category::Project);
    }

    #[test]
    fn splice_appends_to_an_unmanaged_file() {
        let result = splice_managed_block("# My rules\nBe careful.", "generated context");
        assert!(result.starts_with("# My rules\nBe careful."));
        assert!(result.contains(BEGIN_MARKER));
        assert!(result.trim_end().ends_with(END_MARKER));
    }

    #[test]
    fn splice_replaces_only_the_managed_block() {
        let existing =
            format!("# Mine\nkeep me\n\n{BEGIN_MARKER}\nold\n{END_MARKER}\n\n# Tail\nalso mine\n");
        let updated = splice_managed_block(&existing, "new");
        assert!(updated.contains("keep me"));
        assert!(updated.contains("also mine"));
        assert!(updated.contains("new"));
        assert!(!updated.contains("old"));
        assert_eq!(updated.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn managed_block_roundtrip() {
        let file = splice_managed_block("", "generated body");
        assert_eq!(managed_block(&file).as_deref(), Some("generated body"));
        assert_eq!(strip_managed_block(&file), "");
        assert!(managed_block("no markers here").is_none());
    }

    #[test]
    fn import_ignores_the_managed_block() {
        let file = splice_managed_block("# Mine\nkeep", "## Generated\ncontextd wrote this");
        let sections = sections(&file);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Mine");
    }

    #[test]
    fn front_matter_is_not_imported_as_memory() {
        let text = "---\ndescription: rules\nalwaysApply: true\n---\n\n# Style\n\nUse rustfmt.\n";
        let sections = sections(text);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Style");
        assert!(!sections[0].body.contains("alwaysApply"));
    }

    #[test]
    fn unterminated_front_matter_is_left_alone() {
        let text = "---\nnot really front matter\n\n# Heading\n\nbody";
        assert!(strip_front_matter(text).contains("not really front matter"));
    }

    #[test]
    fn truncated_markers_are_not_treated_as_a_block() {
        let text = format!("{BEGIN_MARKER}\nunterminated");
        assert!(managed_block(&text).is_none());
        // Splicing appends a fresh block rather than corrupting the file.
        let spliced = splice_managed_block(&text, "x");
        assert!(spliced.contains("unterminated"));
        assert!(spliced.contains(END_MARKER));
    }
}
