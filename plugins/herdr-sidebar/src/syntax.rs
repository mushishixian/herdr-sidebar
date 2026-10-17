//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only: the terminal keeps its own
//! background, and unknown file types fall back to plain lines.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lines longer than this skip highlighting entirely. syntect's `regex-fancy`
/// backtracking engine is roughly quadratic in line length on pathological
/// input (minified JS/CSS, generated data) — a single multi-hundred-KB line
/// can take tens of seconds and freeze the render thread.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2000;

/// Grammar + theme assets, loaded once (the bundled dumps take a few ms).
/// BOTH themes are built up front: switching the color theme must not stall a
/// render, and the dumps are small.
fn assets() -> &'static (SyntaxSet, Theme, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = two_face::syntax::extra_newlines();
        let mut themes = ThemeSet::load_defaults();
        let mut pick = |name: &str| {
            themes
                .themes
                .remove(name)
                .or_else(|| themes.themes.pop_first().map(|(_, t)| t))
                .unwrap_or_default()
        };
        let dark = pick("base16-ocean.dark");
        let light = pick("InspiredGitHub");
        (syntaxes, dark, light)
    })
}

/// The grammar set plus the theme that suits the active color theme. syntect
/// colors foregrounds only, so a dark grammar theme on a light terminal is
/// exactly the washed-out case the light palette exists to fix.
fn syntaxes_and_theme() -> (&'static SyntaxSet, &'static Theme) {
    let (syntaxes, dark, light) = assets();
    (syntaxes, if crate::ui::is_light() { light } else { dark })
}

/// Grammar for a file called `name`: by extension, then by whole name, then
/// by the first line (a shebang). `None` when nothing matches.
fn find_syntax<'a>(
    syntaxes: &'a SyntaxSet,
    name: &str,
    first_line: Option<&str>,
) -> Option<&'a SyntaxReference> {
    let ext = name.rsplit('.').next().unwrap_or("");
    syntaxes
        .find_syntax_by_extension(ext)
        .or_else(|| syntaxes.find_syntax_by_extension(name))
        .or_else(|| first_line.and_then(|l| syntaxes.find_syntax_by_first_line(l)))
}

/// Convert syntect regions to styled spans. Foreground only; the trailing
/// newline the grammar needs is never shown, and empty chunks are dropped.
fn styled_spans(regions: Vec<(SyntectStyle, &str)>) -> Vec<Span<'static>> {
    regions
        .into_iter()
        .filter_map(|(style, chunk)| {
            let chunk = chunk.trim_end_matches(['\n', '\r']);
            if chunk.is_empty() {
                return None;
            }
            let fg = style.foreground;
            let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
            if style.font_style.contains(FontStyle::BOLD) {
                out = out.add_modifier(Modifier::BOLD);
            }
            if style.font_style.contains(FontStyle::ITALIC) {
                out = out.add_modifier(Modifier::ITALIC);
            }
            Some(Span::styled(chunk.to_string(), out))
        })
        .collect()
}

fn plain_lines(lines: &[String]) -> Vec<Line<'static>> {
    lines.iter().map(|line| Line::raw(line.clone())).collect()
}

/// Highlight `text` for a file called `name`, up to `max` lines. `None` when
/// no grammar matches (caller falls back to plain lines).
pub fn highlight(name: &str, text: &str, max: usize) -> Option<Vec<Line<'static>>> {
    let (syntaxes, theme) = syntaxes_and_theme();
    let syntax = find_syntax(syntaxes, name, text.lines().next())?;

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(text).take(max) {
        let content = raw.trim_end_matches(['\n', '\r']);
        if raw.len() > MAX_HIGHLIGHT_LINE_LEN {
            lines.push(Line::raw(content.to_string()));
            continue;
        }
        match highlighter.highlight_line(raw, syntaxes) {
            Ok(regions) => lines.push(Line::from(styled_spans(regions))),
            Err(_) => lines.push(Line::raw(content.to_string())),
        }
    }
    Some(lines)
}

/// Highlight a window of consecutive file lines, returning exactly
/// `lines.len()` lines whatever the window contains (blank or trailing
/// lines included). `file_first_line` is the file's own first line, used for
/// shebang detection; the window's first line stands in when it is `None`.
/// Plain lines for every line when no grammar matches.
pub fn highlight_lines(
    name: &str,
    file_first_line: Option<&str>,
    lines: &[String],
) -> Vec<Line<'static>> {
    let (syntaxes, theme) = syntaxes_and_theme();
    let first = file_first_line.or_else(|| lines.first().map(String::as_str));
    let Some(syntax) = find_syntax(syntaxes, name, first) else {
        return plain_lines(lines);
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    lines
        .iter()
        .map(|line| {
            if line.len() > MAX_HIGHLIGHT_LINE_LEN {
                return Line::raw(line.clone());
            }
            // Grammars expect line endings; the newline is trimmed from the spans.
            let with_newline = format!("{line}\n");
            match highlighter.highlight_line(&with_newline, syntaxes) {
                Ok(regions) => Line::from(styled_spans(regions)),
                Err(_) => Line::raw(line.clone()),
            }
        })
        .collect()
}

/// Stateful per-line highlighter (for diff rendering, where old/new file
/// contexts advance independently). Plain spans when no grammar matches.
pub struct LineHighlighter {
    inner: Option<HighlightLines<'static>>,
}

impl LineHighlighter {
    pub fn new(name: &str) -> Self {
        let (syntaxes, theme) = syntaxes_and_theme();
        let ext = name.rsplit('.').next().unwrap_or("");
        let syntax = syntaxes
            .find_syntax_by_extension(ext)
            .or_else(|| syntaxes.find_syntax_by_extension(name));
        Self {
            inner: syntax.map(|s| HighlightLines::new(s, theme)),
        }
    }

    /// Highlight one line (no trailing newline in, none out).
    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        let Some(hl) = self.inner.as_mut() else {
            return vec![Span::raw(text.to_string())];
        };
        if text.len() > MAX_HIGHLIGHT_LINE_LEN {
            return vec![Span::raw(text.to_string())];
        }
        let (syntaxes, _) = syntaxes_and_theme();
        let with_nl = format!("{text}\n");
        match hl.highlight_line(&with_nl, syntaxes) {
            Ok(regions) => styled_spans(regions),
            Err(_) => vec![Span::raw(text.to_string())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_highlight_with_colors() {
        let lines = highlight("main.rs", "fn main() {}\n", 10).expect("rust grammar");
        assert_eq!(lines.len(), 1);
        // The `fn` keyword must carry a non-default foreground color.
        let colored = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("fn") && s.style.fg.is_some());
        assert!(colored, "expected a colored keyword span");
        assert_eq!(lines[0].to_string(), "fn main() {}");
    }

    #[test]
    fn extended_grammars_cover_typescript_and_toml() {
        assert!(
            highlight(
                "app.ts",
                "const x: string = \"hi\";
",
                10
            )
            .is_some()
        );
        assert!(
            highlight(
                "Cargo.toml",
                "[package]
name = \"x\"
",
                10
            )
            .is_some()
        );
    }

    #[test]
    fn unknown_extensions_fall_back_to_none() {
        assert!(highlight("data.qqzz", "gibberish content\n", 10).is_none());
    }

    #[test]
    fn pathologically_long_lines_skip_highlighting_instead_of_hanging() {
        let long_line = format!(
            "const x = \"{}\";\n",
            "a".repeat(MAX_HIGHLIGHT_LINE_LEN + 1)
        );
        let lines = highlight("bundle.min.js", &long_line, 10).expect("js grammar matches");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), long_line.trim_end_matches('\n'));

        let mut hl = LineHighlighter::new("bundle.min.js");
        let spans = hl.line(long_line.trim_end_matches('\n'));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, long_line.trim_end_matches('\n'));
    }

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    fn has_colored_span(line: &Line<'_>) -> bool {
        line.spans.iter().any(|span| span.style.fg.is_some())
    }

    #[test]
    fn highlight_lines_keeps_the_count_when_the_window_ends_blank() {
        let lines = owned(&["fn main() {", "}", ""]);
        let out = highlight_lines("main.rs", Some("fn main() {"), &lines);
        assert_eq!(out.len(), lines.len());
        assert!(out.iter().any(has_colored_span), "rust must be colored");
        assert_eq!(out[1].to_string(), "}");
        assert_eq!(out[2].to_string(), "");
    }

    #[test]
    fn highlight_lines_detects_shebang_from_the_file_first_line() {
        // The window does not start at the shebang, so only the file's first
        // line can identify the bash grammar.
        let lines = owned(&["if [ -f x ]; then", "  echo yes", "fi"]);
        let out = highlight_lines("fresh-open", Some("#!/usr/bin/env bash"), &lines);
        assert_eq!(out.len(), lines.len());
        assert!(out.iter().any(has_colored_span), "bash must be colored");
    }

    #[test]
    fn highlight_lines_unknown_extension_is_plain_with_same_count() {
        let lines = owned(&["gibberish content", "", "more"]);
        let out = highlight_lines("data.qqzz", None, &lines);
        assert_eq!(out.len(), lines.len());
        assert!(out.iter().all(|line| !has_colored_span(line)));
        assert_eq!(out[0].to_string(), "gibberish content");
    }

    #[test]
    fn highlight_lines_long_line_stays_plain_and_count_is_preserved() {
        let long = format!("const x = \"{}\";", "a".repeat(MAX_HIGHLIGHT_LINE_LEN + 1));
        let lines = vec![
            "fn a() {}".to_string(),
            long.clone(),
            "fn b() {}".to_string(),
        ];
        let out = highlight_lines("main.rs", None, &lines);
        assert_eq!(out.len(), 3);
        assert!(has_colored_span(&out[0]));
        assert!(!has_colored_span(&out[1]));
        assert_eq!(out[1].to_string(), long);
        assert!(has_colored_span(&out[2]));
    }
}
