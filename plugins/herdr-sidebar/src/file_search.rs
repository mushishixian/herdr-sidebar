//! Shared file search & indexing logic: file collection, fuzzy scoring, matching and truncation.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

pub const QUICK_OPEN_FILE_LIMIT: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickFile {
    pub path: PathBuf,
    pub label: String,
    pub label_lower: String,
}

#[derive(Clone, Debug)]
pub struct QuickIndex {
    pub root: PathBuf,
    pub show_hidden: bool,
    pub files: Arc<Vec<QuickFile>>,
    pub truncated: bool,
}

pub fn collect_quick_files(root: &Path, show_hidden: bool, limit: usize) -> (Vec<QuickFile>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!show_hidden)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let label_lower = label.to_lowercase();
        files.push(QuickFile {
            path,
            label,
            label_lower,
        });
        if files.len() >= limit {
            truncated = true;
            break;
        }
    }
    files.sort_by(|left, right| left.label_lower.cmp(&right.label_lower));
    (files, truncated)
}

pub fn quick_matches(files: &[QuickFile], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..files.len()).collect();
    }
    let query_lower = query.to_lowercase();
    let mut ranked = files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            fuzzy_score_lowercased(&query_lower, &file.label_lower).map(|score| (index, score))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| {
                files[*left_index]
                    .label
                    .len()
                    .cmp(&files[*right_index].label.len())
            })
            .then_with(|| files[*left_index].label.cmp(&files[*right_index].label))
    });
    ranked.into_iter().map(|(index, _)| index).collect()
}

pub fn fuzzy_score_lowercased(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut wanted = query.chars();
    let mut current = wanted.next()?;
    let mut score = 0i64;
    let mut previous_match = None;
    let mut previous_char = None;
    for (index, ch) in candidate.chars().enumerate() {
        if ch == current {
            score += 10;
            if previous_match == Some(index.saturating_sub(1)) {
                score += 8;
            }
            if index == 0
                || previous_char
                    .is_some_and(|before| matches!(before, '/' | '\\' | '-' | '_' | '.'))
            {
                score += 6;
            }
            previous_match = Some(index);
            match wanted.next() {
                Some(next) => current = next,
                None => return Some(score - candidate.chars().count() as i64),
            }
        }
        previous_char = Some(ch);
    }
    None
}

pub fn truncate_path_tail(label: &str, max: usize) -> String {
    let width = ratatui::text::Span::raw(label).width();
    if width <= max {
        return label.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut reversed = Vec::new();
    for ch in label.chars().rev() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width > max {
            break;
        }
        used += char_width;
        reversed.push(ch);
    }
    let tail = reversed.into_iter().rev().collect::<String>();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_open_truncation_keeps_the_filename_visible() {
        assert_eq!(
            truncate_path_tail("src/very/deep/nested/thing.rs", 12),
            "…ed/thing.rs"
        );
        assert_eq!(truncate_path_tail("main.rs", 12), "main.rs");
        assert_eq!(truncate_path_tail("main.rs", 0), "");
        assert_eq!(truncate_path_tail("src/界面.rs", 8), "…界面.rs");
    }

    #[test]
    fn quick_open_matches_case_insensitive_subsequences() {
        let files = [
            QuickFile {
                path: PathBuf::from("src/main.rs"),
                label: "src/main.rs".to_string(),
                label_lower: "src/main.rs".to_string(),
            },
            QuickFile {
                path: PathBuf::from("src/editor.rs"),
                label: "src/editor.rs".to_string(),
                label_lower: "src/editor.rs".to_string(),
            },
        ];
        assert_eq!(quick_matches(&files, "smr"), vec![0]);
        assert_eq!(quick_matches(&files, "ED"), vec![1]);
        assert_eq!(quick_matches(&files, "missing"), Vec::<usize>::new());
    }
}
