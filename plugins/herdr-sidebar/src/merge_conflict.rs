//! Data model for the 3-way merge editor: git's conflict markers parsed into
//! segments, per-conflict resolutions, and the three aligned views (incoming,
//! current, result) the editor draws.
//!
//! Parsing is lossless. Every line keeps its original line ending, and an
//! unresolved block keeps its raw lines, so [`MergeDoc::render`] returns the
//! input byte for byte until the user picks a side. Malformed markers are
//! never an error: the broken text simply stays ordinary common text. Disk
//! access is limited to [`load`] and [`save`]; `save` refuses to overwrite a
//! file that changed since it was read.

use std::fmt;
use std::io::Read;
use std::path::Path;

use crate::content_search::write_atomically;

const CURRENT_MARKER: &str = "<<<<<<<";
const BASE_MARKER: &str = "|||||||";
const SEPARATOR: &str = "=======";
const INCOMING_MARKER: &str = ">>>>>>>";

/// Largest file the editor opens. Matches the search popup's cap in spirit:
/// the view keeps every line in memory and redraws the whole document.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// What the user has chosen for one conflict. The two "Then" variants keep
/// both sides, in the order they were accepted (VS Code's checkboxes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resolution {
    /// Nothing chosen yet: the block stays as git wrote it, markers included.
    #[default]
    Unresolved,
    Current,
    Incoming,
    CurrentThenIncoming,
    IncomingThenCurrent,
    /// The diff3 base. Only meaningful when the block has a base section.
    Base,
}

/// One `<<<<<<<` … `>>>>>>>` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Text after `<<<<<<< `, trimmed. May be empty.
    pub current_label: String,
    /// Text after `>>>>>>> `, trimmed.
    pub incoming_label: String,
    /// Text after `||||||| ` in a diff3-style block.
    pub base_label: Option<String>,
    /// Lines of the current (ours) side, each with its original line ending.
    pub current: Vec<String>,
    pub base: Option<Vec<String>>,
    pub incoming: Vec<String>,
    /// Every original line of the block, markers included. An unresolved
    /// conflict renders from this, so untouched blocks round-trip exactly.
    pub raw: Vec<String>,
    pub resolution: Resolution,
}

/// A stretch of the file: either text that never conflicted, or a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Common(Vec<String>),
    Conflict(Conflict),
}

/// A parsed conflicted file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeDoc {
    pub segments: Vec<Segment>,
    /// The file began with a UTF-8 byte-order mark, which is restored on render.
    pub bom: bool,
    /// `"\r\n"` when most lines end with CRLF, else `"\n"`. Used when a resolved
    /// side needs a line ending it does not have (see `Conflict::resolved_lines`).
    pub line_ending: &'static str,
}

/// Which pane of the editor a view is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Incoming,
    Current,
    Result,
}

/// What a [`ViewLine`] shows, so the renderer can colour and gutter it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Common,
    Current,
    Incoming,
    Base,
    /// A conflict marker line (`<<<<<<<`, `|||||||`, `=======`, `>>>>>>>`).
    Marker,
    /// The label row that opens each conflict in every view.
    Header,
    /// Padding that keeps the top two views aligned, or the placeholder for a
    /// resolution that contributes no lines.
    Filler,
}

/// One row of a view. Rows of the incoming and current views line up by index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewLine {
    /// The line without its line ending.
    pub text: String,
    pub kind: LineKind,
    /// Conflict ordinal (0-based among conflicts), `None` for common text.
    pub conflict: Option<usize>,
    /// 1-based line in the ORIGINAL file, markers included in the count. `None`
    /// for header and filler rows, which exist only in the view.
    pub source_line: Option<usize>,
}

/// Why [`load`] refused a file.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    NotUtf8,
    TooLarge,
    Io(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotUtf8 => write!(f, "not UTF-8 text"),
            LoadError::TooLarge => write!(f, "larger than {} MiB", MAX_BYTES / (1024 * 1024)),
            LoadError::Io(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Why [`save`] did not write.
#[derive(Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The file's bytes no longer match what the editor loaded. Nothing was
    /// written, so another tool's edit is never silently overwritten.
    ChangedOnDisk,
    Io(String),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::ChangedOnDisk => write!(f, "changed on disk since it was opened"),
            SaveError::Io(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// The line without its `\n` or `\r\n`.
fn content(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// The label after a marker, when `text` is that marker. Git writes the marker
/// followed by a space and a label, or alone. `<<<<<<<<` (eight characters) is
/// not a marker, and neither is `<<<<<<<x`.
fn marker_label<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(marker)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix(' ').map(str::trim)
    }
}

enum Section {
    Current,
    Base,
    Incoming,
}

enum Attempt {
    /// A complete block and the index just past its closing marker.
    Block(Conflict, usize),
    /// The block at the start index is malformed. Lines up to this index are
    /// ordinary text, and scanning resumes here.
    Abandon(usize),
}

/// Parse one block that opens at `start`. Any structural problem abandons the
/// block, so malformed input degrades to common text instead of losing lines.
fn parse_block(lines: &[String], start: usize) -> Attempt {
    let current_label = marker_label(content(&lines[start]), CURRENT_MARKER)
        .unwrap_or_default()
        .to_string();
    let mut section = Section::Current;
    let mut current = Vec::new();
    let mut base: Option<Vec<String>> = None;
    let mut base_label = None;
    let mut incoming = Vec::new();
    for (j, line) in lines.iter().enumerate().skip(start + 1) {
        let text = content(line);
        // A new opener means this block never closed. Let the new one be
        // parsed on its own, so the broken block's text stays as common lines.
        if marker_label(text, CURRENT_MARKER).is_some() {
            return Attempt::Abandon(j);
        }
        match section {
            Section::Current => {
                if let Some(label) = marker_label(text, BASE_MARKER) {
                    base_label = Some(label.to_string());
                    base = Some(Vec::new());
                    section = Section::Base;
                } else if text == SEPARATOR {
                    section = Section::Incoming;
                } else if marker_label(text, INCOMING_MARKER).is_some() {
                    // The closer before the separator: this was never a block.
                    return Attempt::Abandon(start + 1);
                } else {
                    current.push(line.clone());
                }
            }
            Section::Base => {
                if text == SEPARATOR {
                    section = Section::Incoming;
                } else if marker_label(text, INCOMING_MARKER).is_some() {
                    return Attempt::Abandon(start + 1);
                } else {
                    base.get_or_insert_with(Vec::new).push(line.clone());
                }
            }
            Section::Incoming => {
                // A second `=======` inside the incoming side is ordinary text.
                if let Some(label) = marker_label(text, INCOMING_MARKER) {
                    let conflict = Conflict {
                        current_label,
                        incoming_label: label.to_string(),
                        base_label,
                        current,
                        base,
                        incoming,
                        raw: lines[start..=j].to_vec(),
                        resolution: Resolution::Unresolved,
                    };
                    return Attempt::Block(conflict, j + 1);
                }
                incoming.push(line.clone());
            }
        }
    }
    // Reached EOF without a closer.
    Attempt::Abandon(lines.len())
}

/// Parse conflicted text. Pure and total: any input yields a document whose
/// [`MergeDoc::render`] is the input again, whatever it contains.
pub fn parse(text: &str) -> MergeDoc {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    // `split_inclusive` keeps each line's own ending, so nothing is normalised.
    let lines: Vec<String> = body.split_inclusive('\n').map(str::to_string).collect();
    let crlf = lines.iter().filter(|line| line.ends_with("\r\n")).count();
    let lf = lines
        .iter()
        .filter(|line| line.ends_with('\n') && !line.ends_with("\r\n"))
        .count();
    let line_ending = if crlf > lf { "\r\n" } else { "\n" };

    let mut segments = Vec::new();
    let mut common: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if marker_label(content(&lines[i]), CURRENT_MARKER).is_none() {
            common.push(lines[i].clone());
            i += 1;
            continue;
        }
        match parse_block(&lines, i) {
            Attempt::Block(conflict, end) => {
                if !common.is_empty() {
                    segments.push(Segment::Common(std::mem::take(&mut common)));
                }
                segments.push(Segment::Conflict(conflict));
                i = end;
            }
            Attempt::Abandon(resume) => {
                common.extend(lines[i..resume].iter().cloned());
                i = resume;
            }
        }
    }
    if !common.is_empty() {
        segments.push(Segment::Common(common));
    }
    MergeDoc {
        segments,
        bom,
        line_ending,
    }
}

/// Which side a resolved conflict draws its lines from, in output order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Current,
    Incoming,
    Base,
}

impl Conflict {
    /// The lines a resolution contributes, in order. `None` means the block is
    /// rendered raw: it is unresolved, or it chose a base that does not exist.
    fn origins(&self) -> Option<Vec<Origin>> {
        match self.resolution {
            Resolution::Unresolved => None,
            Resolution::Current => Some(vec![Origin::Current]),
            Resolution::Incoming => Some(vec![Origin::Incoming]),
            Resolution::CurrentThenIncoming => Some(vec![Origin::Current, Origin::Incoming]),
            Resolution::IncomingThenCurrent => Some(vec![Origin::Incoming, Origin::Current]),
            Resolution::Base => self.base.as_ref().map(|_| vec![Origin::Base]),
        }
    }

    fn origin_lines(&self, origin: Origin) -> &[String] {
        match origin {
            Origin::Current => &self.current,
            Origin::Incoming => &self.incoming,
            Origin::Base => self.base.as_deref().unwrap_or(&[]),
        }
    }

    /// The lines this conflict renders as. An unresolved block returns `raw`,
    /// so untouched blocks round-trip exactly. When two sides are joined and
    /// the first one's last line has no newline (a file ending mid-line), the
    /// `line_ending` is added first, so the two sides never glue together.
    pub fn resolved_lines(&self, line_ending: &str) -> Vec<String> {
        let Some(origins) = self.origins() else {
            return self.raw.clone();
        };
        let mut out: Vec<String> = Vec::new();
        for origin in origins {
            let lines = self.origin_lines(origin);
            if lines.is_empty() {
                continue;
            }
            if let Some(last) = out.last_mut()
                && !last.ends_with('\n')
            {
                last.push_str(line_ending);
            }
            out.extend(lines.iter().cloned());
        }
        out
    }

    pub fn has_current(&self) -> bool {
        matches!(
            self.resolution,
            Resolution::Current | Resolution::CurrentThenIncoming | Resolution::IncomingThenCurrent
        )
    }

    pub fn has_incoming(&self) -> bool {
        matches!(
            self.resolution,
            Resolution::Incoming
                | Resolution::CurrentThenIncoming
                | Resolution::IncomingThenCurrent
        )
    }

    /// Flip the current side's checkbox. Turning it on from a state where the
    /// incoming side already came first puts current second, and vice versa.
    /// Taking base is the same as choosing a side outright.
    pub fn toggle_current(&mut self) {
        self.resolution = match self.resolution {
            Resolution::Unresolved | Resolution::Base => Resolution::Current,
            Resolution::Current => Resolution::Unresolved,
            Resolution::Incoming => Resolution::IncomingThenCurrent,
            Resolution::IncomingThenCurrent => Resolution::Incoming,
            Resolution::CurrentThenIncoming => Resolution::Incoming,
        };
    }

    /// The mirror of [`Conflict::toggle_current`].
    pub fn toggle_incoming(&mut self) {
        self.resolution = match self.resolution {
            Resolution::Unresolved | Resolution::Base => Resolution::Incoming,
            Resolution::Incoming => Resolution::Unresolved,
            Resolution::Current => Resolution::CurrentThenIncoming,
            Resolution::CurrentThenIncoming => Resolution::Current,
            Resolution::IncomingThenCurrent => Resolution::Current,
        };
    }
}

/// Where each part of a block sits in the original file, as 1-based lines.
struct Layout {
    current: usize,
    base: usize,
    incoming: usize,
}

impl Layout {
    fn new(start: usize, conflict: &Conflict) -> Self {
        let current = start + 1;
        // Without a base section the separator follows the current side directly.
        let (base, separator) = match &conflict.base {
            Some(base) => {
                let base_start = current + conflict.current.len() + 1;
                (base_start, base_start + base.len())
            }
            None => (0, current + conflict.current.len()),
        };
        Layout {
            current,
            base,
            incoming: separator + 1,
        }
    }

    fn first_line(&self, conflict: &Conflict, origin: Origin) -> usize {
        match origin {
            Origin::Current => self.current,
            Origin::Incoming => self.incoming,
            Origin::Base => {
                if conflict.base.is_some() {
                    self.base
                } else {
                    self.current
                }
            }
        }
    }
}

/// The kind of each line of an unresolved block, in `raw` order.
fn raw_kinds(conflict: &Conflict) -> Vec<LineKind> {
    let mut kinds = vec![LineKind::Marker];
    kinds.extend(std::iter::repeat_n(
        LineKind::Current,
        conflict.current.len(),
    ));
    if let Some(base) = &conflict.base {
        kinds.push(LineKind::Marker);
        kinds.extend(std::iter::repeat_n(LineKind::Base, base.len()));
    }
    kinds.push(LineKind::Marker);
    kinds.extend(std::iter::repeat_n(
        LineKind::Incoming,
        conflict.incoming.len(),
    ));
    kinds.push(LineKind::Marker);
    kinds
}

fn view_row(
    text: &str,
    kind: LineKind,
    conflict: Option<usize>,
    source: Option<usize>,
) -> ViewLine {
    ViewLine {
        text: content(text).to_string(),
        kind,
        conflict,
        source_line: source,
    }
}

fn header_line(label: &str, conflict: usize) -> ViewLine {
    ViewLine {
        text: label.to_string(),
        kind: LineKind::Header,
        conflict: Some(conflict),
        source_line: None,
    }
}

fn filler_row(conflict: usize) -> ViewLine {
    ViewLine {
        text: String::new(),
        kind: LineKind::Filler,
        conflict: Some(conflict),
        source_line: None,
    }
}

impl MergeDoc {
    fn conflicts(&self) -> impl Iterator<Item = &Conflict> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Conflict(conflict) => Some(conflict),
            Segment::Common(_) => None,
        })
    }

    pub fn conflict_count(&self) -> usize {
        self.conflicts().count()
    }

    pub fn unresolved_count(&self) -> usize {
        self.conflicts()
            .filter(|c| c.resolution == Resolution::Unresolved)
            .count()
    }

    /// The conflict with this ordinal (0-based among conflicts), if any.
    pub fn conflict(&self, ordinal: usize) -> Option<&Conflict> {
        self.conflicts().nth(ordinal)
    }

    pub fn conflict_mut(&mut self, ordinal: usize) -> Option<&mut Conflict> {
        self.segments
            .iter_mut()
            .filter_map(|segment| match segment {
                Segment::Conflict(conflict) => Some(conflict),
                Segment::Common(_) => None,
            })
            .nth(ordinal)
    }

    /// Set one conflict's resolution. An ordinal past the end is ignored.
    pub fn set_resolution(&mut self, ordinal: usize, resolution: Resolution) {
        if let Some(conflict) = self.conflict_mut(ordinal) {
            conflict.resolution = resolution;
        }
    }

    /// Every conflict's resolution in document order, for an undo stack.
    pub fn resolutions(&self) -> Vec<Resolution> {
        self.conflicts().map(|c| c.resolution).collect()
    }

    /// Put back a snapshot from [`MergeDoc::resolutions`]. Extra entries on
    /// either side are ignored.
    pub fn restore_resolutions(&mut self, resolutions: &[Resolution]) {
        for (conflict, resolution) in self
            .segments
            .iter_mut()
            .filter_map(|segment| match segment {
                Segment::Conflict(conflict) => Some(conflict),
                Segment::Common(_) => None,
            })
            .zip(resolutions)
        {
            conflict.resolution = *resolution;
        }
    }

    /// Whether any conflict has been touched. A clean document needs no save
    /// prompt, because it still renders as the original text.
    pub fn is_dirty(&self) -> bool {
        self.conflicts()
            .any(|c| c.resolution != Resolution::Unresolved)
    }

    /// The whole file with every resolution applied.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.bom {
            out.push('\u{feff}');
        }
        for segment in &self.segments {
            match segment {
                Segment::Common(lines) => {
                    for line in lines {
                        out.push_str(line);
                    }
                }
                Segment::Conflict(conflict) => {
                    for line in conflict.resolved_lines(self.line_ending) {
                        out.push_str(&line);
                    }
                }
            }
        }
        out
    }

    /// The rows one pane draws. The incoming and current views share their
    /// common rows and their conflict heights, so row `i` of both is the same
    /// place in the file. The result view shows what the file will contain.
    pub fn view(&self, side: Side) -> Vec<ViewLine> {
        let mut out = Vec::new();
        // 1-based original line number of the next source line.
        let mut line = 1;
        let mut ordinal = 0;
        for segment in &self.segments {
            match segment {
                Segment::Common(lines) => {
                    for text in lines {
                        out.push(view_row(text, LineKind::Common, None, Some(line)));
                        line += 1;
                    }
                }
                Segment::Conflict(conflict) => {
                    let conflict_index = ordinal;
                    ordinal += 1;
                    let start = line;
                    line += conflict.raw.len();
                    let layout = Layout::new(start, conflict);
                    match side {
                        Side::Current | Side::Incoming => {
                            let (label, lines, kind, first) = if side == Side::Current {
                                (
                                    &conflict.current_label,
                                    &conflict.current,
                                    LineKind::Current,
                                    layout.current,
                                )
                            } else {
                                (
                                    &conflict.incoming_label,
                                    &conflict.incoming,
                                    LineKind::Incoming,
                                    layout.incoming,
                                )
                            };
                            out.push(header_line(label, conflict_index));
                            for (offset, text) in lines.iter().enumerate() {
                                out.push(view_row(
                                    text,
                                    kind,
                                    Some(conflict_index),
                                    Some(first + offset),
                                ));
                            }
                            // Both sides pad to the taller one, so the rows of the
                            // two views stay aligned.
                            let height = conflict.current.len().max(conflict.incoming.len()).max(1);
                            for _ in lines.len()..height {
                                out.push(filler_row(conflict_index));
                            }
                        }
                        Side::Result => {
                            out.push(header_line("Result", conflict_index));
                            match conflict.origins() {
                                None => {
                                    let kinds = raw_kinds(conflict);
                                    for (offset, text) in conflict.raw.iter().enumerate() {
                                        let kind =
                                            kinds.get(offset).copied().unwrap_or(LineKind::Marker);
                                        out.push(view_row(
                                            text,
                                            kind,
                                            Some(conflict_index),
                                            Some(start + offset),
                                        ));
                                    }
                                }
                                Some(origins) => {
                                    let mut any = false;
                                    for origin in origins {
                                        let lines = conflict.origin_lines(origin);
                                        let first = layout.first_line(conflict, origin);
                                        let kind = match origin {
                                            Origin::Current => LineKind::Current,
                                            Origin::Incoming => LineKind::Incoming,
                                            Origin::Base => LineKind::Base,
                                        };
                                        for (offset, text) in lines.iter().enumerate() {
                                            any = true;
                                            out.push(view_row(
                                                text,
                                                kind,
                                                Some(conflict_index),
                                                Some(first + offset),
                                            ));
                                        }
                                    }
                                    // An empty resolution still needs a row, or the
                                    // header would run into the code below it.
                                    if !any {
                                        out.push(filler_row(conflict_index));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The index of a conflict's header row within `view(side)`.
    pub fn header_row(&self, side: Side, ordinal: usize) -> Option<usize> {
        self.view(side)
            .iter()
            .position(|row| row.kind == LineKind::Header && row.conflict == Some(ordinal))
    }

    /// The next unresolved conflict after `after`, wrapping to the first one.
    /// `None` means nothing is left to resolve.
    pub fn next_unresolved(&self, after: Option<usize>) -> Option<usize> {
        let open: Vec<usize> = self
            .conflicts()
            .enumerate()
            .filter(|(_, c)| c.resolution == Resolution::Unresolved)
            .map(|(ordinal, _)| ordinal)
            .collect();
        match after {
            Some(after) => open
                .iter()
                .copied()
                .find(|&ordinal| ordinal > after)
                .or_else(|| open.first().copied()),
            None => open.first().copied(),
        }
    }

    /// 1-based line of a conflict's `<<<<<<<` marker in the original file, so
    /// the editor can open at the conflict. Falls back to 1 for a bad ordinal.
    pub fn first_line_of(&self, ordinal: usize) -> usize {
        let mut line = 1;
        let mut seen = 0;
        for segment in &self.segments {
            match segment {
                Segment::Common(lines) => line += lines.len(),
                Segment::Conflict(conflict) => {
                    if seen == ordinal {
                        return line;
                    }
                    seen += 1;
                    line += conflict.raw.len();
                }
            }
        }
        1
    }
}

/// Read a conflicted file. Returns the parsed document and the exact bytes
/// read, which [`save`] uses to detect a change made outside the editor.
pub fn load(path: &Path) -> Result<(MergeDoc, Vec<u8>), LoadError> {
    let file = std::fs::File::open(path).map_err(|e| LoadError::Io(e.to_string()))?;
    // One byte past the cap tells "exactly at the cap" from "over it".
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| LoadError::Io(e.to_string()))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(LoadError::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| LoadError::NotUtf8)?;
    let doc = parse(&text);
    Ok((doc, text.into_bytes()))
}

/// Write the resolved document, but only if the file still holds `baseline`.
/// Returns the bytes now on disk, which are the next baseline.
pub fn save(path: &Path, doc: &MergeDoc, baseline: &[u8]) -> Result<Vec<u8>, SaveError> {
    let on_disk = std::fs::read(path).map_err(|e| SaveError::Io(e.to_string()))?;
    if on_disk != baseline {
        return Err(SaveError::ChangedOnDisk);
    }
    let text = doc.render();
    write_atomically(path, &text).map_err(SaveError::Io)?;
    Ok(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn only_conflict(doc: &MergeDoc) -> &Conflict {
        doc.conflict(0).expect("one conflict")
    }

    #[test]
    fn simple_block_parses_sides_and_labels() {
        let text = "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\nz\n";
        let doc = parse(text);
        assert_eq!(doc.conflict_count(), 1);
        let c = only_conflict(&doc);
        assert_eq!(c.current_label, "HEAD");
        assert_eq!(c.incoming_label, "feature");
        assert_eq!(c.base_label, None);
        assert_eq!(c.current, lines(&["ours\n"]));
        assert_eq!(c.incoming, lines(&["theirs\n"]));
        assert!(c.base.is_none());
        assert_eq!(c.raw.len(), 5);
        assert_eq!(doc.segments.len(), 3, "common, conflict, common");
        assert_eq!(doc.render(), text);
    }

    #[test]
    fn diff3_block_has_base_and_labels() {
        let text = "<<<<<<< ours\nA\n||||||| base\nO\n=======\nB\n>>>>>>> theirs\n";
        let doc = parse(text);
        let c = only_conflict(&doc);
        assert_eq!(c.base_label.as_deref(), Some("base"));
        assert_eq!(c.base, Some(lines(&["O\n"])));
        assert_eq!(c.current, lines(&["A\n"]));
        assert_eq!(c.incoming, lines(&["B\n"]));
        assert_eq!(doc.render(), text);

        let mut doc = doc;
        doc.set_resolution(0, Resolution::Base);
        assert_eq!(doc.render(), "O\n");
    }

    #[test]
    fn empty_labels_and_empty_sides_are_allowed() {
        let text = "<<<<<<<\n=======\n>>>>>>>\n";
        let doc = parse(text);
        let c = only_conflict(&doc);
        assert_eq!(c.current_label, "");
        assert_eq!(c.incoming_label, "");
        assert!(c.current.is_empty() && c.incoming.is_empty());
        assert_eq!(doc.render(), text);
    }

    #[test]
    fn crlf_file_round_trips_and_views_have_no_carriage_returns() {
        let text = "x\r\n<<<<<<< a\r\nour\r\n=======\r\ntheirs\r\n>>>>>>> b\r\ny\r\n";
        let mut doc = parse(text);
        assert_eq!(doc.line_ending, "\r\n");
        assert_eq!(doc.render(), text);
        for side in [Side::Incoming, Side::Current, Side::Result] {
            for row in doc.view(side) {
                assert!(!row.text.contains('\r'), "{side:?}: {row:?}");
            }
        }
        doc.set_resolution(0, Resolution::CurrentThenIncoming);
        assert_eq!(doc.render(), "x\r\nour\r\ntheirs\r\ny\r\n");
    }

    #[test]
    fn lf_wins_ties_and_bom_is_restored() {
        let text = "\u{feff}<<<<<<< a\r\nx\n=======\ny\n>>>>>>> b\n";
        let doc = parse(text);
        assert!(doc.bom);
        assert_eq!(doc.line_ending, "\n");
        assert_eq!(doc.render(), text);
        let mut doc = doc;
        doc.set_resolution(0, Resolution::Incoming);
        assert_eq!(doc.render(), "\u{feff}y\n");
    }

    #[test]
    fn joined_sides_never_glue_lines_when_a_side_lacks_a_newline() {
        // A file that ends mid-line can leave a side without its final newline.
        let c = Conflict {
            current_label: String::new(),
            incoming_label: String::new(),
            base_label: None,
            current: lines(&["ours"]),
            base: None,
            incoming: lines(&["theirs\n"]),
            raw: Vec::new(),
            resolution: Resolution::CurrentThenIncoming,
        };
        assert_eq!(c.resolved_lines("\n"), lines(&["ours\n", "theirs\n"]));
        assert_eq!(c.resolved_lines("\r\n"), lines(&["ours\r\n", "theirs\n"]));

        let mut flipped = c.clone();
        flipped.current = lines(&["ours\n"]);
        flipped.incoming = lines(&["theirs"]);
        flipped.resolution = Resolution::IncomingThenCurrent;
        assert_eq!(flipped.resolved_lines("\n"), lines(&["theirs\n", "ours\n"]));
    }

    #[test]
    fn malformed_input_round_trips_and_is_not_a_conflict() {
        let unclosed = "<<<<<<< a\nx\n=======\ny\n";
        let missing_separator = "<<<<<<< a\nx\n>>>>>>> b\nafter\n";
        let eight_chars = "<<<<<<<< not a marker\nx\n=======\ny\n>>>>>>> z\n";
        let nested = "<<<<<<< a\nx\n<<<<<<< b\ny\n=======\nz\n>>>>>>> c\n";
        for text in [unclosed, missing_separator, eight_chars, nested] {
            let doc = parse(text);
            assert_eq!(doc.render(), text, "round trip: {text:?}");
        }
        assert_eq!(parse(unclosed).conflict_count(), 0);
        assert_eq!(parse(missing_separator).conflict_count(), 0);
        assert_eq!(parse(eight_chars).conflict_count(), 0);
        // The nested opener abandons the first block; the second one still parses.
        let doc = parse(nested);
        assert_eq!(doc.conflict_count(), 1);
        assert_eq!(only_conflict(&doc).current_label, "b");
    }

    #[test]
    fn every_resolution_renders_its_side() {
        let text = "<<<<<<< a\nO\n||||||| base\nB\n=======\nI\n>>>>>>> b\n";
        let expected = [
            (Resolution::Unresolved, text.to_string()),
            (Resolution::Current, "O\n".to_string()),
            (Resolution::Incoming, "I\n".to_string()),
            (Resolution::CurrentThenIncoming, "O\nI\n".to_string()),
            (Resolution::IncomingThenCurrent, "I\nO\n".to_string()),
            (Resolution::Base, "B\n".to_string()),
        ];
        for (resolution, rendered) in expected {
            let mut doc = parse(text);
            doc.set_resolution(0, resolution);
            assert_eq!(doc.render(), rendered, "{resolution:?}");
        }
    }

    #[test]
    fn base_without_a_base_section_renders_raw() {
        let text = "<<<<<<< a\nO\n=======\nI\n>>>>>>> b\n";
        let mut doc = parse(text);
        doc.set_resolution(0, Resolution::Base);
        assert_eq!(doc.render(), text);
    }

    #[test]
    fn toggle_semantics_match_checkboxes() {
        use Resolution::*;
        let current_cases = [
            (Unresolved, Current),
            (Current, Unresolved),
            (Incoming, IncomingThenCurrent),
            (IncomingThenCurrent, Incoming),
            (CurrentThenIncoming, Incoming),
            (Base, Current),
        ];
        let incoming_cases = [
            (Unresolved, Incoming),
            (Incoming, Unresolved),
            (Current, CurrentThenIncoming),
            (CurrentThenIncoming, Current),
            (IncomingThenCurrent, Current),
            (Base, Incoming),
        ];
        for (from, to) in current_cases {
            let mut c = conflict_with(from);
            c.toggle_current();
            assert_eq!(c.resolution, to, "toggle_current from {from:?}");
        }
        for (from, to) in incoming_cases {
            let mut c = conflict_with(from);
            c.toggle_incoming();
            assert_eq!(c.resolution, to, "toggle_incoming from {from:?}");
        }
        assert!(conflict_with(IncomingThenCurrent).has_current());
        assert!(conflict_with(IncomingThenCurrent).has_incoming());
        assert!(!conflict_with(Base).has_current());
    }

    fn conflict_with(resolution: Resolution) -> Conflict {
        Conflict {
            current_label: String::new(),
            incoming_label: String::new(),
            base_label: None,
            current: lines(&["o\n"]),
            base: None,
            incoming: lines(&["i\n"]),
            raw: lines(&["<<<<<<<\n", "o\n", "=======\n", "i\n", ">>>>>>>\n"]),
            resolution,
        }
    }

    #[test]
    fn incoming_and_current_views_stay_aligned() {
        let text = "top\n\
<<<<<<< a\n1\n2\n3\n=======\nx\n>>>>>>> b\n\
mid\n\
<<<<<<< a\n=======\n>>>>>>> b\n\
<<<<<<< a\nq\n=======\nr\ns\nt\n>>>>>>> b\n\
end\n";
        let doc = parse(text);
        let incoming = doc.view(Side::Incoming);
        let current = doc.view(Side::Current);
        assert_eq!(incoming.len(), current.len());
        for (i, (a, b)) in incoming.iter().zip(&current).enumerate() {
            assert_eq!(
                a.kind == LineKind::Header,
                b.kind == LineKind::Header,
                "row {i} header mismatch"
            );
            assert_eq!(a.conflict, b.conflict, "row {i} conflict mismatch");
        }
        // Conflict 0: current 3 lines, incoming 1 -> height 3, incoming pads 2.
        // Conflict 1: both sides empty -> height 1, one filler on each side.
        // Conflict 2: current 1 line, incoming 3 -> height 3, current pads 2.
        let fillers = |rows: &[ViewLine], k: usize| {
            rows.iter()
                .filter(|r| r.kind == LineKind::Filler && r.conflict == Some(k))
                .count()
        };
        assert_eq!(fillers(&incoming, 0), 2);
        assert_eq!(fillers(&current, 0), 0);
        assert_eq!(fillers(&incoming, 1), 1);
        assert_eq!(fillers(&current, 1), 1);
        assert_eq!(fillers(&incoming, 2), 0);
        assert_eq!(fillers(&current, 2), 2);
    }

    #[test]
    fn result_view_shows_markers_when_unresolved_and_kinds_when_resolved() {
        let text = "<<<<<<< a\nO\n||||||| base\nB\n=======\nI\n>>>>>>> b\n";
        let mut doc = parse(text);
        let raw = doc.view(Side::Result);
        let kinds: Vec<LineKind> = raw.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Header,
                LineKind::Marker,
                LineKind::Current,
                LineKind::Marker,
                LineKind::Base,
                LineKind::Marker,
                LineKind::Incoming,
                LineKind::Marker,
            ]
        );
        assert_eq!(raw[1].text, "<<<<<<< a");

        doc.set_resolution(0, Resolution::IncomingThenCurrent);
        let resolved = doc.view(Side::Result);
        let kinds: Vec<LineKind> = resolved.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [LineKind::Header, LineKind::Incoming, LineKind::Current]
        );

        doc.set_resolution(0, Resolution::Base);
        let base = doc.view(Side::Result);
        assert_eq!(base.last().unwrap().kind, LineKind::Base);

        // An empty result still gets a row, so the header is not glued to code.
        let mut empty = parse("<<<<<<< a\n=======\n>>>>>>> b\nnext\n");
        empty.set_resolution(0, Resolution::Current);
        let rows = empty.view(Side::Result);
        assert_eq!(rows[1].kind, LineKind::Filler);
        assert_eq!(rows[2].text, "next");
    }

    #[test]
    fn source_lines_count_markers_and_common_lines() {
        let text = "top\n<<<<<<< a\nO1\nO2\n=======\nI1\n>>>>>>> b\nmid\n<<<<<<< a\nX\n=======\nY\n>>>>>>> b\n";
        let doc = parse(text);
        assert_eq!(doc.first_line_of(0), 2);
        assert_eq!(doc.first_line_of(1), 9);
        assert_eq!(doc.first_line_of(7), 1, "unknown ordinal falls back to 1");

        let current = doc.view(Side::Current);
        let o1 = current.iter().find(|r| r.text == "O1").unwrap();
        assert_eq!(o1.source_line, Some(3));
        let i1 = doc.view(Side::Incoming);
        assert_eq!(
            i1.iter().find(|r| r.text == "I1").unwrap().source_line,
            Some(6)
        );
        let mid = doc
            .view(Side::Result)
            .into_iter()
            .find(|r| r.text == "mid")
            .unwrap();
        assert_eq!(mid.source_line, Some(8));
        let header = current.iter().find(|r| r.kind == LineKind::Header).unwrap();
        assert_eq!(header.source_line, None);
    }

    #[test]
    fn header_row_and_next_unresolved_wrap() {
        let text =
            "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\nmid\n<<<<<<< a\nz\n=======\nw\n>>>>>>> b\n";
        let mut doc = parse(text);
        assert_eq!(doc.conflict_count(), 2);
        assert_eq!(doc.header_row(Side::Current, 0), Some(0));
        assert_eq!(doc.header_row(Side::Incoming, 1), Some(3));
        assert_eq!(doc.header_row(Side::Result, 5), None);

        assert_eq!(doc.next_unresolved(None), Some(0));
        assert_eq!(doc.next_unresolved(Some(0)), Some(1));
        assert_eq!(doc.next_unresolved(Some(1)), Some(0), "wraps around");
        doc.set_resolution(0, Resolution::Current);
        assert_eq!(doc.next_unresolved(Some(1)), Some(1));
        assert!(doc.is_dirty());
        assert_eq!(doc.unresolved_count(), 1);
        doc.set_resolution(1, Resolution::Incoming);
        assert_eq!(doc.next_unresolved(None), None);
    }

    #[test]
    fn restore_resolutions_round_trips_an_undo_snapshot() {
        let text = "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n";
        let mut doc = parse(text);
        let before = doc.resolutions();
        doc.set_resolution(0, Resolution::Incoming);
        assert!(doc.is_dirty());
        doc.restore_resolutions(&before);
        assert!(!doc.is_dirty());
        assert_eq!(doc.render(), text);
    }

    #[test]
    fn round_trip_holds_for_mixed_input() {
        let inputs = [
            "",
            "no conflicts at all",
            "\u{feff}",
            "<<<<<<< a\n",
            "a\r\nb",
            "<<<<<<< a\n=======\n>>>>>>> b",
        ];
        for text in inputs {
            assert_eq!(parse(text).render(), text, "{text:?}");
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-merge-conflict-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_refuses_changed_bytes_and_writes_otherwise() {
        let dir = scratch("save");
        let path = dir.join("file.txt");
        let text = "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n";
        std::fs::write(&path, text).unwrap();

        let (mut doc, baseline) = load(&path).unwrap();
        doc.set_resolution(0, Resolution::Incoming);

        // Another tool edits the file after we opened it.
        std::fs::write(&path, "someone else\n").unwrap();
        assert_eq!(save(&path, &doc, &baseline), Err(SaveError::ChangedOnDisk));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else\n");

        // Back to the loaded bytes: now the save goes through.
        std::fs::write(&path, text).unwrap();
        let new_baseline = save(&path, &doc, &baseline).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\n");
        assert_eq!(new_baseline, b"y\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reports_non_utf8_and_too_large() {
        let dir = scratch("load");
        let bad = dir.join("bad.bin");
        std::fs::write(&bad, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(load(&bad).unwrap_err(), LoadError::NotUtf8);

        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(load(&big).unwrap_err(), LoadError::TooLarge);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
