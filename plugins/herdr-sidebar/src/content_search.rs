//! Content search engine shared by the sidebar's Search view and the
//! `Ctrl+Shift+F` search popup: literal / whole-word / regex matching with
//! VS Code semantics, include/exclude globs, and highlighted result context.
//! Moved out of `explorer_app.rs` so both callers run one implementation.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ratatui::style::Style;
use ratatui::text::Span;
use regex::{Regex, RegexBuilder};
use unicode_width::UnicodeWidthChar;

use crate::file_search::collect_quick_files;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentHit {
    pub path: PathBuf,
    pub label: String,
    pub line: usize,
    pub context: String,
    pub matches: Vec<(usize, usize)>,
}

pub const CONTENT_SEARCH_MATCH_LIMIT: usize = 1_000;

const CONTENT_SEARCH_FILE_LIMIT: usize = 20_000;

const CONTENT_SEARCH_MAX_BYTES: u64 = 1024 * 1024;

pub fn highlighted_search_context(
    context: &str,
    matches: &[(usize, usize)],
    max_width: usize,
    match_style: Style,
) -> Vec<Span<'static>> {
    let context_width = Span::raw(context).width();
    let (visible_end, truncated) = if context_width <= max_width {
        (context.len(), false)
    } else if max_width < 2 {
        (0, false)
    } else {
        let mut width = 0;
        let mut end = 0;
        for (index, character) in context.char_indices() {
            let character_width = character.width().unwrap_or(0);
            if width + character_width + 1 > max_width {
                break;
            }
            width += character_width;
            end = index + character.len_utf8();
        }
        (end, true)
    };
    let mut spans = Vec::new();
    let mut cursor = 0;
    for &(start, end) in matches {
        if start >= visible_end {
            break;
        }
        let start = start.max(cursor);
        let end = end.min(visible_end);
        if start >= end {
            continue;
        }
        if start > cursor {
            spans.push(Span::raw(context[cursor..start].to_string()));
        }
        spans.push(Span::styled(context[start..end].to_string(), match_style));
        cursor = end;
    }
    if cursor < visible_end {
        spans.push(Span::raw(context[cursor..visible_end].to_string()));
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    spans
}

pub fn collect_content_hits(
    root: &Path,
    show_hidden: bool,
    query: &str,
    include: &str,
    exclude: &str,
    options: SearchOptions,
    limit: usize,
) -> Result<(Vec<ContentHit>, bool), String> {
    if query.is_empty() || limit == 0 {
        return Ok((Vec::new(), false));
    }
    let pattern = compile_pattern(query, options)?;
    let includes = build_search_globs(include, "include")?;
    let excludes = build_search_globs(exclude, "exclude")?;
    let (files, file_limit_reached) =
        collect_quick_files(root, show_hidden, CONTENT_SEARCH_FILE_LIMIT);
    let mut hits = Vec::new();
    for file in files {
        if includes
            .as_ref()
            .is_some_and(|patterns| !patterns.is_match(&file.label))
            || excludes
                .as_ref()
                .is_some_and(|patterns| patterns.is_match(&file.label))
        {
            continue;
        }
        if std::fs::metadata(&file.path)
            .map(|metadata| metadata.len() > CONTENT_SEARCH_MAX_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file.path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let ranges = content_line_match_ranges(line, query, options, pattern.as_ref());
            let matched = pattern
                .as_ref()
                .is_some_and(|pattern| pattern.is_match(line))
                || !ranges.is_empty();
            if !matched {
                continue;
            }
            let (context, matches) = content_hit_context(line, &ranges);
            hits.push(ContentHit {
                path: file.path.clone(),
                label: file.label.clone(),
                line: index + 1,
                context,
                matches,
            });
            if hits.len() >= limit {
                return Ok((hits, true));
            }
        }
    }
    Ok((hits, file_limit_reached))
}

pub fn build_search_globs(raw: &str, label: &str) -> Result<Option<GlobSet>, String> {
    let patterns = raw
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).map_err(|error| format!("Invalid {label} pattern: {error}"))?;
        builder.add(glob);
        if !pattern.contains(['*', '?', '[', '{']) {
            let directory = pattern.trim_end_matches(['/', '\\']);
            let descendant = format!("{directory}/**");
            builder.add(
                Glob::new(&descendant)
                    .map_err(|error| format!("Invalid {label} pattern: {error}"))?,
            );
            if !directory.contains(['/', '\\']) {
                let nested_descendant = format!("**/{directory}/**");
                builder.add(
                    Glob::new(&nested_descendant)
                        .map_err(|error| format!("Invalid {label} pattern: {error}"))?,
                );
            }
        }
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| format!("Invalid {label} pattern: {error}"))
}

#[cfg(test)]
fn content_line_matches(
    line: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> bool {
    if let Some(pattern) = pattern {
        return pattern.is_match(line);
    }
    !literal_match_ranges(line, query, options).is_empty()
}

pub fn content_line_match_ranges(
    line: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> Vec<(usize, usize)> {
    if let Some(pattern) = pattern {
        return pattern
            .find_iter(line)
            .filter(|found| !found.is_empty())
            .map(|found| (found.start(), found.end()))
            .collect();
    }
    literal_match_ranges(line, query, options)
}

fn literal_match_ranges(line: &str, query: &str, options: SearchOptions) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    if options.match_case {
        return line
            .match_indices(query)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                word_boundary_matches(line, start, end, options.whole_word).then_some((start, end))
            })
            .collect();
    }

    let lowered = line.to_lowercase();
    let mut source_by_lowered_character = Vec::new();
    for (source_start, character) in line.char_indices() {
        let source_range = (source_start, source_start + character.len_utf8());
        source_by_lowered_character.extend(std::iter::repeat_n(
            source_range,
            character.to_lowercase().count(),
        ));
    }
    let mut lowered_boundaries = lowered
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    lowered_boundaries.push(lowered.len());
    if source_by_lowered_character.len() + 1 != lowered_boundaries.len() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut ranges = Vec::new();
    for (lowered_start, matched) in lowered.match_indices(&needle) {
        let lowered_end = lowered_start + matched.len();
        if !word_boundary_matches(&lowered, lowered_start, lowered_end, options.whole_word) {
            continue;
        }
        let Ok(first) = lowered_boundaries.binary_search(&lowered_start) else {
            continue;
        };
        let Ok(after_last) = lowered_boundaries.binary_search(&lowered_end) else {
            continue;
        };
        if first >= after_last {
            continue;
        }
        let range = (
            source_by_lowered_character[first].0,
            source_by_lowered_character[after_last - 1].1,
        );
        if ranges.last() != Some(&range) {
            ranges.push(range);
        }
    }
    ranges
}

fn word_boundary_matches(haystack: &str, start: usize, end: usize, whole_word: bool) -> bool {
    if !whole_word {
        return true;
    }
    let before = haystack[..start].chars().next_back();
    let after = haystack[end..].chars().next();
    !before.is_some_and(search_word_char) && !after.is_some_and(search_word_char)
}

pub fn content_hit_context(line: &str, ranges: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return (String::new(), Vec::new());
    }
    let start = line.len() - line.trim_start().len();
    let end = start + trimmed.len();
    let ranges = ranges
        .iter()
        .filter_map(|&(match_start, match_end)| {
            if match_start >= end || match_end <= start {
                return None;
            }
            let match_start = match_start.max(start);
            let match_end = match_end.min(end);
            Some((match_start - start, match_end - start))
        })
        .collect();
    (trimmed.replace('\t', " "), ranges)
}

fn search_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The compiled regex for `query` under `options`, or `None` for literal
/// search. Shared by the Explorer search and the search popup so both reject
/// and interpret the same patterns.
pub fn compile_pattern(query: &str, options: SearchOptions) -> Result<Option<Regex>, String> {
    if !options.regex {
        return Ok(None);
    }
    let pattern = if options.whole_word {
        format!(r"\b(?:{query})\b")
    } else {
        query.to_string()
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!options.match_case)
        .build()
        .map(Some)
        .map_err(|error| format!("Invalid regular expression: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn content_search_groups_ignored_and_hidden_rules_with_source_lines() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-content-search-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "first\nNeedle here\nneedle again\n",
        )
        .unwrap();
        std::fs::write(root.join(".secret"), "needle\n").unwrap();
        std::fs::write(root.join("target/generated.rs"), "needle\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let (visible, truncated) =
            collect_content_hits(&root, false, "NEEDLE", "", "", SearchOptions::default(), 20)
                .unwrap();
        assert!(!truncated);
        assert_eq!(
            visible
                .iter()
                .map(|hit| (hit.label.as_str(), hit.line))
                .collect::<Vec<_>>(),
            vec![("src/main.rs", 2), ("src/main.rs", 3)]
        );
        let (with_hidden, _) =
            collect_content_hits(&root, true, "needle", "", "", SearchOptions::default(), 20)
                .unwrap();
        assert_eq!(with_hidden.len(), 3);
        assert!(with_hidden.iter().any(|hit| hit.label == ".secret"));

        let (included, _) = collect_content_hits(
            &root,
            true,
            "needle",
            "src/**",
            "",
            SearchOptions::default(),
            20,
        )
        .unwrap();
        assert_eq!(included.len(), 2);
        let (excluded, _) = collect_content_hits(
            &root,
            true,
            "needle",
            "",
            "src/**",
            SearchOptions::default(),
            20,
        )
        .unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].label, ".secret");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_search_options_match_vscode_semantics() {
        assert!(content_line_matches(
            "Needle needlebox",
            "needle",
            SearchOptions::default(),
            None,
        ));
        assert!(!content_line_matches(
            "Needle",
            "needle",
            SearchOptions {
                match_case: true,
                ..SearchOptions::default()
            },
            None,
        ));
        assert!(content_line_matches(
            "a needle here",
            "needle",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            None,
        ));
        assert!(!content_line_matches(
            "needlebox",
            "needle",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            None,
        ));

        let regex = RegexBuilder::new(r"need(le|ful)")
            .case_insensitive(true)
            .build()
            .unwrap();
        assert!(content_line_matches(
            "NEEDFUL",
            "ignored",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
            Some(&regex),
        ));
        assert!(build_search_globs("[", "include").is_err());
        let directories = build_search_globs("node_modules", "exclude")
            .unwrap()
            .unwrap();
        assert!(directories.is_match("web/node_modules/pkg/index.js"));
        assert!(directories.is_match("node_modules/pkg/index.js"));
    }

    #[test]
    fn search_result_context_bolds_every_visible_match() {
        let line = "Needle plus needlebox";
        let ranges = content_line_match_ranges(line, "needle", SearchOptions::default(), None);
        let spans = highlighted_search_context(line, &ranges, 80, Style::default().bold());
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "Needle plus needlebox"
        );
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .count(),
            2
        );

        let line = "needle plus needlebox";
        let ranges = content_line_match_ranges(
            line,
            "needle",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
            None,
        );
        let spans = highlighted_search_context(line, &ranges, 80, Style::default().bold());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .count(),
            1
        );
    }

    #[test]
    fn search_highlights_follow_literal_boundaries_unicode_and_clipping() {
        let whole_word = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };
        let punctuation = "use C++ here";
        let ranges = content_line_match_ranges(punctuation, "C++", whole_word, None);
        assert_eq!(ranges, [(4, 7)]);

        let unicode = "İ and s ſ";
        assert_eq!(
            content_line_match_ranges(unicode, "i", SearchOptions::default(), None),
            [(0, 2)]
        );
        assert_eq!(
            content_line_match_ranges(unicode, "s", SearchOptions::default(), None),
            [(7, 8)]
        );
        assert_eq!(
            content_line_match_ranges("ΟΣ", "ΟΣ", SearchOptions::default(), None),
            [(0, 4)]
        );

        let (_, trimmed_ranges) = content_hit_context(" needle", &[(0, 1)]);
        assert!(trimmed_ranges.is_empty());

        let clipped = "needlebox needle";
        let ranges = content_line_match_ranges(clipped, "needle", whole_word, None);
        let spans = highlighted_search_context(clipped, &ranges, 8, Style::default().bold());
        assert!(
            spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD))
        );

        let ranges = content_line_match_ranges("needle", "needle", whole_word, None);
        let spans = highlighted_search_context("needle", &ranges, 5, Style::default().bold());
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "need…"
        );
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }
}
