//! Content search engine shared by the sidebar's Search view and the
//! `Ctrl+Shift+F` search popup: literal / whole-word / regex matching with
//! VS Code semantics, include/exclude globs, and highlighted result context.
//! Moved out of `explorer_app.rs` so both callers run one implementation.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

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

// ── Replacement (search popup "replace" mode) ──────────────────────────

/// Which occurrences a replacement touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Every match in the text (or file).
    All,
    /// One match. `line` is 1-based and `start..end` are byte offsets within
    /// that raw line, exactly as [`content_line_match_ranges`] reports them.
    One {
        line: usize,
        start: usize,
        end: usize,
    },
}

/// Outcome of replacing across files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplaceReport {
    pub files_changed: usize,
    pub replacements: usize,
    /// Files that were not written, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Replace the matches that `target` selects in `text`. Matching is the
/// engine's own (`compile_pattern` + `content_line_match_ranges`), line by
/// line. Line endings (`\n` or `\r\n`), a UTF-8 BOM and a missing final
/// newline are untouched: only the matched bytes of each line change.
///
/// Regex mode expands `$1`, `${name}` and `$$` through the match's captures;
/// literal mode inserts `replacement` verbatim. Returns the new text and the
/// number of matches replaced. A `One` target replaces only when its line
/// still has a match at exactly that range.
pub fn replace_in_text(
    text: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
    replacement: &str,
    target: Target,
) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    for (index, segment) in text.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        if let Target::One { line, .. } = target
            && line != line_number
        {
            out.push_str(segment);
            continue;
        }
        let (body, ending) = split_line_ending(segment);
        let (replaced, replaced_here) = replace_line(
            body,
            query,
            options,
            pattern,
            replacement,
            |start, end| match target {
                Target::All => true,
                Target::One {
                    start: wanted_start,
                    end: wanted_end,
                    ..
                } => start == wanted_start && end == wanted_end,
            },
        );
        count += replaced_here;
        out.push_str(&replaced);
        out.push_str(ending);
    }
    (out, count)
}

/// Split one `split_inclusive('\n')` segment into its content and its line
/// ending (`""`, `"\n"` or `"\r\n"`). A lone trailing `\r` stays content, as
/// `str::lines` treats it.
fn split_line_ending(segment: &str) -> (&str, &str) {
    let Some(body) = segment.strip_suffix('\n') else {
        return (segment, "");
    };
    let body_len = body.strip_suffix('\r').map_or(body.len(), str::len);
    (&segment[..body_len], &segment[body_len..])
}

fn replace_line(
    line: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
    replacement: &str,
    selected: impl Fn(usize, usize) -> bool,
) -> (String, usize) {
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0;
    let mut count = 0;
    if let Some(pattern) = pattern {
        for captures in pattern.captures_iter(line) {
            let Some(whole) = captures.get(0) else {
                continue;
            };
            // Empty matches are not matches for the engine either.
            if whole.is_empty() || !selected(whole.start(), whole.end()) {
                continue;
            }
            out.push_str(&line[cursor..whole.start()]);
            captures.expand(replacement, &mut out);
            cursor = whole.end();
            count += 1;
        }
    } else {
        for (start, end) in literal_match_ranges(line, query, options) {
            if !selected(start, end) {
                continue;
            }
            out.push_str(&line[cursor..start]);
            out.push_str(replacement);
            cursor = end;
            count += 1;
        }
    }
    out.push_str(&line[cursor..]);
    (out, count)
}

/// The `One` target for the first match on 1-based `line` of `text`, or
/// `None` when that line has no match.
pub fn first_match_target(
    text: &str,
    line: usize,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> Option<Target> {
    let raw = text.lines().nth(line.checked_sub(1)?)?;
    let &(start, end) = content_line_match_ranges(raw, query, options, pattern).first()?;
    Some(Target::One { line, start, end })
}

static REPLACE_TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Replace matches across `files`. Each entry lists the targets for one file:
/// a `Target::All` entry replaces every match in it, and `One` entries replace
/// exactly those occurrences. Paths must resolve inside `root` (symlinks are
/// followed before the check). Binary, non-UTF-8, and over-1-MiB files are
/// skipped with a reason; so is any file that fails to read or write. A file
/// is written only when it changes, atomically (temp file in the same
/// directory, original permissions copied, then renamed over the original).
/// Only an invalid query or root is an `Err`.
pub fn apply_replacements(
    root: &Path,
    files: &[(PathBuf, Vec<Target>)],
    query: &str,
    options: SearchOptions,
    replacement: &str,
) -> Result<ReplaceReport, String> {
    let pattern = compile_pattern(query, options)?;
    let mut report = ReplaceReport::default();
    if query.is_empty() {
        return Ok(report);
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot open project root: {error}"))?;
    for (path, targets) in files {
        match replace_in_file(
            &root,
            path,
            query,
            options,
            pattern.as_ref(),
            replacement,
            targets,
        ) {
            Ok(0) => {}
            Ok(count) => {
                report.files_changed += 1;
                report.replacements += count;
            }
            Err(reason) => report.skipped.push((path.clone(), reason)),
        }
    }
    Ok(report)
}

fn replace_in_file(
    root: &Path,
    path: &Path,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
    replacement: &str,
    targets: &[Target],
) -> Result<usize, String> {
    let path = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve: {error}"))?;
    if !path.starts_with(root) {
        return Err("outside the project root".into());
    }
    let metadata = std::fs::metadata(&path).map_err(|error| format!("cannot read: {error}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    if metadata.len() > CONTENT_SEARCH_MAX_BYTES {
        return Err("over 1 MiB".into());
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("cannot read: {error}"))?;
    if bytes.contains(&0) {
        return Err("binary file".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "not UTF-8".to_string())?;

    // A whole-file target wins. Otherwise apply single occurrences from the
    // bottom of the file up, so earlier byte offsets stay valid.
    let mut steps: Vec<Target> = if targets.contains(&Target::All) {
        vec![Target::All]
    } else {
        let mut ones = targets.to_vec();
        ones.sort_by_key(|target| match target {
            Target::One { line, start, .. } => (*line, *start),
            Target::All => (0, 0),
        });
        ones.reverse();
        ones.dedup();
        ones
    };
    let mut current = text.to_string();
    let mut count = 0;
    for target in steps.drain(..) {
        let (next, replaced) =
            replace_in_text(&current, query, options, pattern, replacement, target);
        current = next;
        count += replaced;
    }
    if count == 0 {
        return Ok(0);
    }
    write_atomically(&path, &current)?;
    Ok(count)
}

/// Replace `path` with `contents` through a sibling temp file and a rename, so
/// a crash or a full disk leaves the original intact. The original's permissions
/// are copied over. Shared by the search/replace popup and the merge editor.
pub fn write_atomically(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "no parent directory".to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let permissions = std::fs::metadata(path)
        .map_err(|error| format!("cannot read: {error}"))?
        .permissions();
    let temp = dir.join(format!(
        ".{name}.herdr-replace-{}-{}.tmp",
        std::process::id(),
        REPLACE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&temp, permissions)?;
        std::fs::rename(&temp, path)
    })();
    result
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .map_err(|error| format!("write failed: {error}"))
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

#[cfg(test)]
mod replace_tests {
    use super::*;

    fn literal(match_case: bool, whole_word: bool) -> SearchOptions {
        SearchOptions {
            match_case,
            whole_word,
            regex: false,
        }
    }

    fn regex_options() -> SearchOptions {
        SearchOptions {
            regex: true,
            match_case: true,
            ..SearchOptions::default()
        }
    }

    fn replace_all(text: &str, query: &str, options: SearchOptions, with: &str) -> (String, usize) {
        let pattern = compile_pattern(query, options).unwrap();
        replace_in_text(text, query, options, pattern.as_ref(), with, Target::All)
    }

    /// A fresh temp project root unique to this test.
    fn project(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-replace-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn literal_replacement_is_case_insensitive_by_default() {
        assert_eq!(
            replace_all("Foo foo FOO", "foo", literal(false, false), "bar"),
            ("bar bar bar".to_string(), 3)
        );
        assert_eq!(
            replace_all("Foo foo FOO", "foo", literal(true, false), "bar"),
            ("Foo bar FOO".to_string(), 1)
        );
    }

    #[test]
    fn whole_word_replacement_skips_embedded_matches() {
        assert_eq!(
            replace_all("foo foobar afoo foo", "foo", literal(false, true), "bar"),
            ("bar foobar afoo bar".to_string(), 2)
        );
    }

    #[test]
    fn literal_replacement_is_verbatim_and_empty_deletes() {
        assert_eq!(
            replace_all("a$1-a", "a", literal(true, false), "$1"),
            ("$1$1-$1".to_string(), 2)
        );
        assert_eq!(
            replace_all("a-b-a", "a", literal(true, false), ""),
            ("-b-".to_string(), 2)
        );
    }

    #[test]
    fn regex_replacement_expands_groups_names_and_dollar() {
        let options = regex_options();
        assert_eq!(
            replace_all("ann@x bob@y", r"(\w+)@(\w+)", options, "$2<-$1"),
            ("x<-ann y<-bob".to_string(), 2)
        );
        let pattern = Some(regex::Regex::new(r"(?P<who>\w+)@(\w+)").unwrap());
        let (text, count) = replace_in_text(
            "ann@x bob@y",
            r"(?P<who>\w+)@(\w+)",
            options,
            pattern.as_ref(),
            "${who}-$2 $$",
            Target::All,
        );
        assert_eq!((text.as_str(), count), ("ann-x $ bob-y $", 2));
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_skip() {
        let root = project("invalid-regex");
        let error = apply_replacements(&root, &[], "(", regex_options(), "x");
        assert!(error.is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        let (text, count) = replace_all("x foo\r\nfoo\r\n", "foo", literal(true, false), "bar");
        assert_eq!((text.as_str(), count), ("x bar\r\nbar\r\n", 2));
    }

    #[test]
    fn utf8_bom_is_preserved() {
        let (text, count) = replace_all("\u{feff}foo\nfoo", "foo", literal(true, false), "bar");
        assert_eq!((text.as_str(), count), ("\u{feff}bar\nbar", 2));
    }

    #[test]
    fn missing_final_newline_is_preserved() {
        let (text, count) = replace_all("foo\nfoo", "foo", literal(true, false), "bar");
        assert_eq!((text.as_str(), count), ("bar\nbar", 2));
        let (text, _) = replace_all("foo\n", "foo", literal(true, false), "bar");
        assert_eq!(text, "bar\n");
    }

    #[test]
    fn one_target_replaces_only_that_occurrence() {
        let (text, count) = replace_in_text(
            "foo foo\nfoo",
            "foo",
            literal(true, false),
            None,
            "bar",
            Target::One {
                line: 1,
                start: 4,
                end: 7,
            },
        );
        assert_eq!((text.as_str(), count), ("foo bar\nfoo", 1));
    }

    #[test]
    fn one_target_on_a_changed_line_replaces_nothing() {
        let target = Target::One {
            line: 1,
            start: 0,
            end: 3,
        };
        let (text, count) =
            replace_in_text("zzz\n", "foo", literal(true, false), None, "bar", target);
        assert_eq!((text.as_str(), count), ("zzz\n", 0));
        let (text, count) =
            replace_in_text("xfoo\n", "foo", literal(true, false), None, "bar", target);
        assert_eq!((text.as_str(), count), ("xfoo\n", 0));
    }

    #[test]
    fn multibyte_text_around_matches_is_intact() {
        let (text, count) = replace_all(
            "héllo wörld foo ✓foo 日本",
            "foo",
            literal(true, false),
            "bar",
        );
        assert_eq!((text.as_str(), count), ("héllo wörld bar ✓bar 日本", 2));
    }

    #[test]
    fn apply_refuses_paths_outside_the_root() {
        let root = project("outside");
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let outside = root.join("outside.txt");
        std::fs::write(&outside, "foo\n").unwrap();
        let report = apply_replacements(
            &inner,
            &[(outside.clone(), vec![Target::All])],
            "foo",
            literal(true, false),
            "bar",
        )
        .unwrap();
        assert_eq!(report.replacements, 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].1, "outside the project root");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "foo\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_skips_binary_files_and_reports_counts() {
        let root = project("counts");
        let text_file = root.join("a.txt");
        let other = root.join("b.txt");
        let binary = root.join("c.bin");
        std::fs::write(&text_file, "foo one\nfoo two\n").unwrap();
        std::fs::write(&other, "nothing here\n").unwrap();
        std::fs::write(&binary, b"foo\0foo").unwrap();
        let report = apply_replacements(
            &root,
            &[
                (text_file.clone(), vec![Target::All]),
                (other.clone(), vec![Target::All]),
                (binary.clone(), vec![Target::All]),
            ],
            "foo",
            literal(true, false),
            "bar",
        )
        .unwrap();
        assert_eq!(report.files_changed, 1);
        assert_eq!(report.replacements, 2);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].1, "binary file");
        assert_eq!(
            std::fs::read_to_string(&text_file).unwrap(),
            "bar one\nbar two\n"
        );
        assert_eq!(std::fs::read(&binary).unwrap(), b"foo\0foo");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            3,
            "no temp files left"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn several_one_targets_on_one_line_apply_bottom_up() {
        let root = project("several");
        let file = root.join("line.txt");
        std::fs::write(&file, "foo foo foo\n").unwrap();
        let report = apply_replacements(
            &root,
            &[(
                file.clone(),
                vec![
                    Target::One {
                        line: 1,
                        start: 0,
                        end: 3,
                    },
                    Target::One {
                        line: 1,
                        start: 8,
                        end: 11,
                    },
                ],
            )],
            "foo",
            literal(true, false),
            "longer",
        )
        .unwrap();
        assert_eq!(report.replacements, 2);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "longer foo longer\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn first_match_target_picks_the_first_range_on_the_line() {
        let text = "none\n  foo and foo\n";
        assert_eq!(
            first_match_target(text, 2, "foo", literal(true, false), None),
            Some(Target::One {
                line: 2,
                start: 2,
                end: 5,
            })
        );
        assert_eq!(
            first_match_target(text, 1, "foo", literal(true, false), None),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = project("perms");
        let file = root.join("run.sh");
        std::fs::write(&file, "foo\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        let report = apply_replacements(
            &root,
            &[(file.clone(), vec![Target::All])],
            "foo",
            literal(true, false),
            "bar",
        )
        .unwrap();
        assert_eq!(report.replacements, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bar\n");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
        let _ = std::fs::remove_dir_all(root);
    }
}
