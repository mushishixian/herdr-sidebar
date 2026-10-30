/**
 * Selection: share the selected lines
 *
 * Three share commands, all in the Selection menu (after "Git: Line History"):
 *
 *   Selection: Copy Text          Ctrl+Alt+Y  copy exactly the selected text
 *                                             (no selection: the cursor's line)
 *   Selection: Copy as Reference  Ctrl+Alt+L  copy "<path>:<lines>" followed by
 *                                             a fenced block of the full lines
 *   Selection: Send to Agent      Ctrl+Alt+A  paste that same reference into the
 *                                             IDE's Agent pane composer, without
 *                                             submitting it (fresh-to-agent)
 *
 * The reference uses the same line range rules as "Git: Line History": a
 * selection that ends at column 0 of a later line does not include that line.
 * <path> is relative to the editor's working directory when the file lives
 * inside it, otherwise absolute. Reference and agent need a saved file.
 *
 * Each command also raises a herdr toast (title = command, body = key + detail).
 *
 * Tabs: Previous Open Tab  Ctrl+Tab (config.json, when "normal")
 *   Jumps to the most recently used other open file buffer, so
 *   pressing it again toggles between the two most recent tabs. Only file
 *   buffers in the current window count; it never opens files. Recency is an
 *   MRU list fed by the active_buffer_changed hook (see "Tab history" below).
 *
 * Live diff on request: when the herdr sidebar opens a changed file through
 *   fresh-open with HERDR_SIDEBAR_OPEN_DIFF=1, fresh-open leaves a request
 *   file (see "Live diff on request" at the end). Once that file is the active
 *   buffer, this plugin runs Fresh's built-in Live Diff vs HEAD on it.
 *
 * Inline merge-conflict resolver (see "Inline merge conflicts" at the end):
 *   VS Code-style tinted conflict blocks, an action line above each <<<<<<<,
 *   and Alt+C / Alt+I / Alt+M to accept current / incoming / both, Alt+D and
 *   Alt+Shift+D to move between conflicts. Fresh's own 3-panel merge plugin
 *   is untouched; this one never touches its decorations.
 *
 * KEY LABELS: toast bodies read each binding from config.json at runtime via
 * editor.getUserConfig().keybindings. FALLBACK_KEYS is used only when that
 * lookup finds nothing. It must match config.json.
 */

const editor = getEditor();

/** Largest selection that will be sent to the agent pane. */
const AGENT_MAX_CHARS = 200000;

/**
 * Fallback key labels for toasts. MUST MATCH config.json's keybindings; used
 * only when the runtime lookup finds no binding for the action.
 */
const FALLBACK_KEYS: Record<string, string> = {
  selection_copy_text: "Ctrl+Alt+Y",
  selection_copy_reference: "Ctrl+Alt+L",
  selection_send_to_agent: "Ctrl+Alt+A",
  recent_tab: "Ctrl+Tab",
  conflict_accept_current: "Alt+C",
  conflict_accept_incoming: "Alt+I",
  conflict_accept_both: "Alt+M",
  conflict_next: "Alt+D",
  conflict_prev: "Alt+Shift+D",
  // Whole-file commands have no key by default: empty label, no prefix.
  conflict_accept_all_current: "",
  conflict_accept_all_incoming: "",
};

/** Toast and status helpers ------------------------------------------------ */

const MODIFIER_ORDER = ["ctrl", "alt", "shift", "super"];

function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** "Ctrl+Alt+L" for the config binding of `action`, else the fallback. */
function keyLabel(action: string): string {
  try {
    const hit = editor
      .getUserConfig()
      .keybindings?.find((b) => b.action === action && b.key);
    if (hit?.key) {
      const mods = [...(hit.modifiers ?? [])].sort(
        (a, b) => MODIFIER_ORDER.indexOf(a) - MODIFIER_ORDER.indexOf(b),
      );
      return [...mods.map(capitalize), capitalize(hit.key)].join("+");
    }
  } catch {
    // Config unavailable: fall through to the constant map.
  }
  return FALLBACK_KEYS[action] ?? action;
}

/** Fire-and-forget herdr toast. Outside herdr the spawn just fails: ignored. */
function toast(title: string, body: string): void {
  try {
    editor
      .spawnProcess("herdr", ["notification", "show", title, "--body", body])
      .result.catch(() => {});
  } catch {
    // No process support: nothing to show.
  }
}

/** Status-bar message plus a herdr toast naming the command and its key. */
function announce(action: string, title: string, status: string, body: string): void {
  heldStatus = null; // a newer message supersedes a pending status re-assert
  editor.setStatus(status);
  const key = keyLabel(action);
  toast(title, key ? `${key} · ${body}` : body);
}

/** Error text shortened for a toast body. */
function oneLine(text: string, max = 100): string {
  const t = text.replace(/\s+/g, " ").trim();
  return t.length > max ? `${t.slice(0, max - 1)}…` : t;
}

/** UTF-8 byte offset at which each line starts. Line 0 starts at byte 0. */
function lineStartOffsets(text: string): number[] {
  const starts: number[] = [0];
  let bytes = 0;
  for (const ch of text) {
    const cp = ch.codePointAt(0) ?? 0;
    bytes += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    if (ch === "\n") starts.push(bytes);
  }
  return starts;
}

/** 0-based index of the line containing `byte` (binary search over starts). */
function lineOfByte(starts: number[], byte: number): number {
  let lo = 0;
  let hi = starts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (starts[mid] <= byte) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/**
 * 1-based, inclusive line range covered by the cursor or selection.
 *
 * - No selection (or an empty one): the cursor's line.
 * - Reversed selections are normalised.
 * - A multi-line selection that ends exactly at column 0 of a later line
 *   does not include that line: the user selected the line *terminator*,
 *   not the line's content.
 */
function historyLineRange(
  starts: number[],
  cursorByte: number,
  selection: { start: number; end: number } | null,
): { start: number; end: number } {
  if (!selection || selection.start === selection.end) {
    const line = lineOfByte(starts, cursorByte) + 1;
    return { start: line, end: line };
  }
  const lo = Math.min(selection.start, selection.end);
  const hi = Math.max(selection.start, selection.end);
  const first = lineOfByte(starts, lo);
  let last = lineOfByte(starts, hi);
  if (last > first && hi === starts[last]) last -= 1;
  return { start: first + 1, end: last + 1 };
}

/** Path shown in a reference: relative to the cwd when inside it. */
function displayPath(filePath: string, cwd: string): string {
  const root = cwd.replace(/\/+$/, "");
  if (root && filePath.startsWith(root + "/")) {
    return filePath.slice(root.length + 1);
  }
  return filePath;
}

/** File extension without the dot ("rs"), or "" when there is none. */
function languageOf(filePath: string): string {
  const base = filePath.slice(filePath.lastIndexOf("/") + 1);
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(dot + 1) : "";
}

/**
 * A code fence longer than any run of backticks in the body, so the body
 * cannot close the block early. Three backticks in the usual case.
 */
function fenceFor(body: string): string {
  let longest = 0;
  for (const run of body.match(/`+/g) ?? []) {
    longest = Math.max(longest, run.length);
  }
  return "`".repeat(Math.max(3, longest + 1));
}

/** First non-empty line of a process's stderr, for the status bar. */
function firstLine(text: string): string {
  return text.split("\n").map((l) => l.trim()).find((l) => l.length > 0) ?? "";
}

function rangeLabel(range: { start: number; end: number }): string {
  return range.start === range.end
    ? String(range.start)
    : `${range.start}-${range.end}`;
}

interface ShareContext {
  bufferId: number;
  filePath: string;
  text: string;
  range: { start: number; end: number };
  cwd: string;
}

/**
 * Shared state for reference/agent: the saved path and the line range.
 * On failure it reports through announce() itself, so the caller just returns.
 */
async function shareContext(action: string, title: string): Promise<ShareContext | null> {
  const bufferId = editor.getActiveBufferId();
  const filePath = editor.getBufferPath(bufferId);
  if (!filePath) {
    announce(action, title, "Selection: save the file first", "save the file first");
    return null;
  }
  const cursor = editor.getPrimaryCursor();
  if (!cursor) {
    announce(action, title, "Selection: no cursor", "no cursor");
    return null;
  }
  const text = await editor.getBufferText(bufferId);
  const range = historyLineRange(
    lineStartOffsets(text),
    cursor.position,
    cursor.selection,
  );
  return { bufferId, filePath, text, range, cwd: editor.getCwd() };
}

/** "<path>:<range>" header, a fence, the full lines, and a closing fence. */
function buildReference(ctx: ShareContext): { label: string; text: string } {
  const label = `${displayPath(ctx.filePath, ctx.cwd)}:${rangeLabel(ctx.range)}`;
  const body = ctx.text
    .split("\n")
    .slice(ctx.range.start - 1, ctx.range.end)
    .map((line) => line.replace(/\r$/, ""))
    .join("\n");
  const fence = fenceFor(body);
  const lang = languageOf(ctx.filePath);
  return {
    label,
    text: `${label}\n${fence}${lang}\n${body}\n${fence}\n`,
  };
}

async function selection_copy_text(): Promise<void> {
  try {
    const bufferId = editor.getActiveBufferId();
    const cursor = editor.getPrimaryCursor();
    if (!cursor) {
      announce("selection_copy_text", "Copy Text", "Copy text: no cursor", "no cursor");
      return;
    }

    let copied: string;
    const sel = cursor.selection;
    if (sel && sel.start !== sel.end) {
      const lo = Math.min(sel.start, sel.end);
      const hi = Math.max(sel.start, sel.end);
      copied = await editor.getBufferText(bufferId, lo, hi);
    } else {
      // No selection: the cursor's whole line, without its newline.
      const text = await editor.getBufferText(bufferId);
      const starts = lineStartOffsets(text);
      const line = lineOfByte(starts, cursor.position);
      copied = text.split("\n")[line] ?? "";
    }

    editor.setClipboard(copied);
    const n = copied.replace(/\n$/, "").split("\n").length;
    const lines = `${n} line${n === 1 ? "" : "s"}`;
    announce(
      "selection_copy_text",
      "Copy Text",
      `Copied ${lines}`,
      `copied ${lines}`,
    );
  } catch (e) {
    announce("selection_copy_text", "Copy Text", `Copy text: ${String(e)}`, oneLine(String(e)));
  }
}

async function selection_copy_reference(): Promise<void> {
  try {
    const ctx = await shareContext("selection_copy_reference", "Copy as Reference");
    if (!ctx) return;
    const ref = buildReference(ctx);
    editor.setClipboard(ref.text);
    announce(
      "selection_copy_reference",
      "Copy as Reference",
      `Copied reference ${ref.label}`,
      `copied ${ref.label}`,
    );
  } catch (e) {
    announce("selection_copy_reference", "Copy as Reference", `Copy reference: ${String(e)}`, oneLine(String(e)));
  }
}

async function selection_send_to_agent(): Promise<void> {
  try {
    const ctx = await shareContext("selection_send_to_agent", "Send to Agent");
    if (!ctx) return;
    const ref = buildReference(ctx);
    if (ref.text.length > AGENT_MAX_CHARS) {
      announce("selection_send_to_agent", "Send to Agent", "Selection too large", "selection too large");
      return;
    }

    const home = editor.getEnv("HOME") ?? "";
    const binDir =
      editor.getEnv("DOTFILES_IDE_BIN") ?? `${home}/code/herdr-sidebar/dotfiles/herdr-plugins/ide/bin`;
    const helper = `${binDir}/fresh-to-agent`;

    const result = await editor.spawnProcess(helper, [ref.text], ctx.cwd);
    if (result.exit_code !== 0) {
      const reason =
        firstLine(result.stderr) || `fresh-to-agent exited with code ${result.exit_code}`;
      announce("selection_send_to_agent", "Send to Agent", `Send to Agent: ${reason}`, oneLine(reason));
      return;
    }
    announce(
      "selection_send_to_agent",
      "Send to Agent",
      `Sent ${ref.label} to Agent`,
      `sent ${ref.label}`,
    );
  } catch (e) {
    announce("selection_send_to_agent", "Send to Agent", `Send to Agent: ${String(e)}`, oneLine(String(e)));
  }
}

registerHandler("selection_copy_text", selection_copy_text);
registerHandler("selection_copy_reference", selection_copy_reference);
registerHandler("selection_send_to_agent", selection_send_to_agent);

editor.registerCommand(
  "Selection: Copy Text",
  "Copy the selected text (or the cursor line) to the clipboard",
  "selection_copy_text",
  null,
);
editor.registerCommand(
  "Selection: Copy as Reference",
  "Copy <path>:<lines> and the selected lines as a fenced code block",
  "selection_copy_reference",
  null,
);
editor.registerCommand(
  "Selection: Send to Agent",
  "Paste the selected lines' reference into the IDE's Agent pane",
  "selection_send_to_agent",
  null,
);

editor.addMenuItem({
  menu: "Selection",
  label: "Selection: Copy Text",
  action: "selection_copy_text",
  after: "git_line_history",
});
editor.addMenuItem({
  menu: "Selection",
  label: "Selection: Copy as Reference",
  action: "selection_copy_reference",
  after: "selection_copy_text",
});
editor.addMenuItem({
  menu: "Selection",
  label: "Selection: Send to Agent",
  action: "selection_send_to_agent",
  after: "selection_copy_reference",
});

/* ── Tab history: Ctrl+Tab = previous open tab ──────────────────────────── */

/** Most recently used buffer ids first. Entries may be closed: they are skipped. */
let recentBuffers: number[] = [];
const RECENT_MAX = 64;

function touchRecent(bufferId: number): void {
  recentBuffers = [bufferId, ...recentBuffers.filter((id) => id !== bufferId)].slice(
    0,
    RECENT_MAX,
  );
}

/** A real file buffer: backed by a path, not virtual, not a terminal. */
function isFileBuffer(b: BufferInfo): boolean {
  return b.path !== "" && !b.is_virtual && !b.is_terminal;
}

/** active_buffer_changed: every tab switch, split focus and open lands here. */
function on_active_buffer_changed(args: { buffer_id: number }): void {
  touchRecent(args.buffer_id);
  liveDiffCheck();
}

/** buffer_closed: drop it so it can never be selected again. */
function on_buffer_closed(args: { buffer_id: number }): void {
  recentBuffers = recentBuffers.filter((id) => id !== args.buffer_id);
}

/**
 * Switch to the most recently used other open file buffer in this window. Pressing again switches back, because the MRU list is updated
 * by the same hook that handles every other switch. Never opens new files.
 */
async function recent_tab(): Promise<void> {
  try {
    const currentId = editor.getActiveBufferId();
    const windowId = editor.getBufferInfo(currentId)?.window_id;
    const rank = (id: number): number => {
      const i = recentBuffers.indexOf(id);
      return i < 0 ? Number.MAX_SAFE_INTEGER : i;
    };
    // Buffers never seen by the hook keep listBuffers() order after the MRU ones.
    const candidates = editor
      .listBuffers()
      .filter((b) => b.id !== currentId && b.window_id === windowId && isFileBuffer(b))
      .sort((a, b) => rank(a.id) - rank(b.id));

    const target = candidates[0];
    if (!target) {
      announce("recent_tab", "Previous Tab", "No other open file tab", "no other open tab");
      return;
    }
    if (!editor.showBuffer(target.id)) {
      announce("recent_tab", "Previous Tab", `Could not switch to ${target.name}`, oneLine(target.name));
      return;
    }
    touchRecent(target.id);
    announce("recent_tab", "Previous Tab", `Switched to ${target.name}`, target.name);
  } catch (e) {
    announce("recent_tab", "Previous Tab", `Previous tab: ${String(e)}`, oneLine(String(e)));
  }
}

registerHandler("recent_tab", recent_tab);
registerHandler("selection_track_buffer", on_active_buffer_changed);
registerHandler("selection_forget_buffer", on_buffer_closed);
editor.on("active_buffer_changed", "selection_track_buffer");
editor.on("buffer_closed", "selection_forget_buffer");
touchRecent(editor.getActiveBufferId());

editor.registerCommand(
  "Tabs: Previous Open Tab",
  "Ctrl+Tab: jump to the previously used open tab (again = back)",
  "recent_tab",
  null,
);

/* ── Live diff on request ───────────────────────────────────────────────── */
//
// fresh-open (HERDR_SIDEBAR_OPEN_DIFF=1, sent by the herdr sidebar on a git
// change click) writes <state>/fresh-ide/live-diff-request-<daemon>, where
// <daemon> is FRESH_IDE_DAEMON (set by fresh-ide), so only the editor the file
// was handed to consumes it. Line 1 the
// absolute file path, line 2 the epoch seconds. <state> is $XDG_STATE_HOME,
// else $HOME/.local/state. When that file is the active buffer, the request
// is consumed and Fresh's built-in Live Diff runs vs HEAD for it.
//
// Requests are checked only on active_buffer_changed and after_file_open. There
// is deliberately NO timer: a 400 ms setInterval made every idle editor run JS
// and re-render about 2.5 times a second. Consequence: a request for a file that
// is ALREADY the active buffer is not picked up until the next buffer switch or
// open. That is acceptable, because fresh-open opens the file (which fires the
// events) and the sidebar no longer sends these requests.
//
// Consuming writes the file empty (replaceFile is the only write that can
// overwrite; the API has no delete). Empty, malformed, expired or unwritable
// requests are dropped and never retried, so the diff runs at most once.

/** A request older than this is ignored (fresh-open writes it just before handing off). */
const LIVE_DIFF_REQUEST_MAX_AGE_S = 15;

/**
 * This daemon's request file. fresh-ide exports FRESH_IDE_DAEMON (the daemon
 * name) and fresh-open writes live-diff-request-<name>. A daemon started
 * before that change has no such variable and reads the legacy file.
 */
function liveDiffRequestPath(): string | null {
  const xdg = editor.getEnv("XDG_STATE_HOME");
  const home = editor.getEnv("HOME");
  const dir = xdg ? `${xdg}/fresh-ide` : home ? `${home}/.local/state/fresh-ide` : null;
  if (!dir) return null;
  const daemon = editor.getEnv("FRESH_IDE_DAEMON");
  return daemon ? `${dir}/live-diff-request-${daemon}` : `${dir}/live-diff-request`;
}

/** Start Live Diff vs HEAD for the active buffer if a request names it. Never throws. */
function liveDiffCheck(): void {
  try {
    const req = liveDiffRequestPath();
    if (!req || !editor.fileExists(req)) return;
    const raw = editor.readFile(req);
    if (!raw) return; // nothing pending, or already consumed

    const [file = "", stamp = ""] = raw.split("\n");
    const age = Date.now() / 1000 - Number(stamp);
    const valid =
      file.startsWith("/") &&
      Number.isFinite(age) &&
      age >= -5 &&
      age <= LIVE_DIFF_REQUEST_MAX_AGE_S;
    // A valid request waits until its file is the active buffer.
    if (valid && editor.getBufferPath(editor.getActiveBufferId()) !== file) return;

    // Consume before acting, so a repeated poll cannot start the diff twice.
    if (!editor.replaceFile(req, "")) {
      editor.setStatus("Live diff: could not clear the request file");
      return;
    }
    if (!valid) return;

    const name = file.slice(file.lastIndexOf("/") + 1);
    if (!editor.executeAction("live_diff_vs_head")) {
      editor.setStatus("Live diff: could not start Live Diff vs HEAD");
      return;
    }
    // The live_diff plugin sets its own status once the diff is computed, so
    // the toast is what reliably names the file.
    editor.setStatus(`Live Diff vs HEAD · ${name}`);
    toast("Live Diff", `vs HEAD · ${name}`);
  } catch (e) {
    editor.setStatus(`Live diff: ${String(e)}`);
  }
}

registerHandler("selection_live_diff_opened", liveDiffCheck);
editor.on("after_file_open", "selection_live_diff_opened");

/* ── Inline merge conflicts ─────────────────────────────────────────────── */
//
// VS Code-style resolution in the normal buffer: no splits, no virtual buffers.
// Each conflict block gets tinted regions (CURRENT green, INCOMING blue, diff3
// BASE grey, marker lines a shade stronger), an action line above each <<<<<<<
// marker, and a gutter bar on the marker lines. Accepting a side rewrites the
// whole block, moves to the next unresolved conflict, and once the file is
// clean saves it and opens the next conflicted file (from `git diff`).
//
// Keys (config.json, when "normal"): Alt+C / Alt+I / Alt+M accept current /
// incoming / both, Alt+D next, Alt+Shift+D previous. Alt+1/2/3 (bookmarks),
// Alt+N/P (find selection) and Alt+Shift+N/P (sidebar sections) are Fresh
// defaults and stay as they are. A key pressed in a file without conflicts only
// shows "No merge conflicts in this file".
//
// Every namespace here is CONFLICT_NS, so Fresh's own merge_conflict plugin and
// its decorations are never cleared or drawn over by this code.
//
// The host queues deleteRange / insertText and returns before they apply, so
// each rewrite is confirmed by re-reading the buffer (awaitConflictEdit) before
// the cursor moves or the file is saved.

const CONFLICT_NS = "inline-conflict";
type RGB = [number, number, number];
/** Tints on the dark theme (VS Code Dark+ #1e1e1e background). Solid RGB. */
const CONFLICT_CURRENT_BG: RGB = [38, 70, 50];
const CONFLICT_CURRENT_MARK_BG: RGB = [52, 98, 70];
const CONFLICT_INCOMING_BG: RGB = [34, 60, 92];
const CONFLICT_INCOMING_MARK_BG: RGB = [48, 82, 124];
const CONFLICT_BASE_BG: RGB = [58, 58, 58];
const CONFLICT_BASE_MARK_BG: RGB = [74, 74, 74];
/** The action line above each <<<<<<< marker (dim, like a CodeLens). */
const CONFLICT_LENS_FG: RGB = [140, 170, 210];
/** Gutter bar on every marker line. */
const CONFLICT_GUTTER_FG: RGB = [230, 180, 70];
const CONFLICT_GUTTER_SYMBOL = "┃";

const CONFLICT_SCAN_DEBOUNCE_MS = 150;
const CONFLICT_APPLY_POLL_MS = 40;
const CONFLICT_APPLY_MAX_POLLS = 50;
/** Larger buffers are not scanned (no decorations, no commands act on them). */
const CONFLICT_MAX_CHARS = 2_000_000;

type ConflictSide = "current" | "incoming" | "both";
const CONFLICT_SIDE_LABEL: Record<ConflictSide, string> = {
  current: "Accepted current",
  incoming: "Accepted incoming",
  both: "Accepted both",
};

/**
 * One conflict block. Indices are UTF-16 offsets into the buffer text; byte
 * offsets for the editor come from byteIndex(). Marker lines run from their
 * start to the next line's start (newline included), so slicing the text keeps
 * the file's CRLF or LF endings exactly as they were.
 */
interface ConflictBlock {
  startLine: number; // 0-based line of "<<<<<<<"
  sepLine: number; // "======="
  tailLine: number; // ">>>>>>>"
  baseLine: number | null; // "|||||||" (diff3 base), else null
  start: number; // start of the "<<<<<<<" line
  end: number; // just past the ">>>>>>>" line
  curStart: number; // current content: [curStart, curEnd)
  curEnd: number;
  baseStart: number | null; // start of the "|||||||" line
  baseContentStart: number | null;
  sepStart: number; // start of the "=======" line
  incStart: number; // incoming content: [incStart, incEnd)
  incEnd: number; // start of the ">>>>>>>" line
  tailStart: number;
}

interface OpenConflict extends Partial<ConflictBlock> {
  state: 1 | 2 | 3; // 1 current, 2 base, 3 incoming
}

/**
 * Every complete conflict block, in order. Malformed input is dropped: a new
 * "<<<<<<<" restarts the scan, and a block with no ">>>>>>>" is ignored.
 */
function parseConflicts(text: string): ConflictBlock[] {
  const blocks: ConflictBlock[] = [];
  let open: OpenConflict | null = null;
  let pos = 0;
  let lineNo = 0;
  while (pos < text.length) {
    const nl = text.indexOf("\n", pos);
    const next = nl < 0 ? text.length : nl + 1;
    const line = text.slice(pos, nl < 0 ? text.length : nl).replace(/\r$/, "");
    if (line.startsWith("<<<<<<<")) {
      open = {
        state: 1,
        startLine: lineNo,
        start: pos,
        curStart: next,
        baseLine: null,
        baseStart: null,
        baseContentStart: null,
      };
    } else if (open && open.state === 1 && line.startsWith("|||||||")) {
      open.curEnd = pos;
      open.baseLine = lineNo;
      open.baseStart = pos;
      open.baseContentStart = next;
      open.state = 2;
    } else if (open && open.state !== 3 && line.trimEnd() === "=======") {
      if (open.state === 1) open.curEnd = pos;
      open.sepLine = lineNo;
      open.sepStart = pos;
      open.incStart = next;
      open.state = 3;
    } else if (open && open.state === 3 && line.startsWith(">>>>>>>")) {
      blocks.push({
        ...(open as ConflictBlock),
        tailLine: lineNo,
        tailStart: pos,
        incEnd: pos,
        end: next,
      });
      open = null;
    } else if (open && line.startsWith(">>>>>>>")) {
      open = null; // a tail with no separator: malformed
    }
    pos = next;
    lineNo++;
  }
  return blocks;
}

/** cum[i] = UTF-8 byte offset of UTF-16 index i (cum[length] = total bytes). */
function byteIndex(text: string): Uint32Array {
  const cum = new Uint32Array(text.length + 1);
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    cum[i] = bytes;
    bytes += c < 0x80 ? 1 : c < 0x800 ? 2 : c >= 0xd800 && c <= 0xdfff ? 2 : 3;
  }
  cum[text.length] = bytes;
  return cum;
}

/** Namespace-tracked buffers that currently carry decorations. */
const conflictPainted = new Set<number>();

/** Redraw the decorations for one buffer. Never throws into the caller. */
function paintConflicts(bufferId: number, text: string, blocks: ConflictBlock[]): void {
  if (blocks.length === 0 && !conflictPainted.has(bufferId)) return;
  editor.clearNamespace(bufferId, CONFLICT_NS);
  editor.clearVirtualTextNamespace(bufferId, CONFLICT_NS);
  editor.clearLineIndicators(bufferId, CONFLICT_NS);
  conflictPainted.delete(bufferId);
  if (blocks.length === 0) return;
  conflictPainted.add(bufferId);

  const cum = byteIndex(text);
  const lens = conflictLensText();
  const span = (from: number, to: number, bg: RGB): void => {
    if (to > from) {
      editor.addOverlay(bufferId, CONFLICT_NS, cum[from], cum[to], { bg, extendToLineEnd: true });
    }
  };
  const gutter: number[] = [];
  for (const b of blocks) {
    span(b.start, b.curStart, CONFLICT_CURRENT_MARK_BG);
    span(b.curStart, b.curEnd, CONFLICT_CURRENT_BG);
    if (b.baseStart !== null && b.baseContentStart !== null) {
      span(b.baseStart, b.baseContentStart, CONFLICT_BASE_MARK_BG);
      span(b.baseContentStart, b.sepStart, CONFLICT_BASE_BG);
      if (b.baseLine !== null) gutter.push(b.baseLine);
    }
    span(b.sepStart, b.incStart, CONFLICT_INCOMING_MARK_BG);
    span(b.incStart, b.incEnd, CONFLICT_INCOMING_BG);
    span(b.tailStart, b.end, CONFLICT_INCOMING_MARK_BG);
    editor.addVirtualLine(bufferId, cum[b.start], lens, { fg: CONFLICT_LENS_FG }, true, CONFLICT_NS, 0);
    gutter.push(b.startLine, b.sepLine, b.tailLine);
  }
  editor.setLineIndicators(
    bufferId,
    gutter,
    CONFLICT_NS,
    CONFLICT_GUTTER_SYMBOL,
    CONFLICT_GUTTER_FG[0],
    CONFLICT_GUTTER_FG[1],
    CONFLICT_GUTTER_FG[2],
    5,
  );
}

/** The action line text, with the live key labels. */
function conflictLensText(): string {
  const part = (label: string, action: string): string => {
    const key = keyLabel(action);
    return key ? `${label} (${key})` : label;
  };
  return [
    part("Accept Current", "conflict_accept_current"),
    part("Accept Incoming", "conflict_accept_incoming"),
    part("Accept Both", "conflict_accept_both"),
    part("Next", "conflict_next"),
    "Merge editor (Ctrl+Alt+U)",
  ].join(" │ ");
}

/* Detection: debounced scan on open, activate, edit and revert. */

let conflictScanTimer: number | null = null;
let conflictScanBuffer: number | null = null;

function scheduleConflictScan(bufferId: number): void {
  conflictScanBuffer = bufferId;
  if (conflictScanTimer !== null) editor.clearInterval(conflictScanTimer);
  conflictScanTimer = editor.setTimeout(CONFLICT_SCAN_DEBOUNCE_MS, "conflict_scan_timer");
}

async function conflict_scan_timer(): Promise<void> {
  conflictScanTimer = null;
  const bufferId = conflictScanBuffer ?? editor.getActiveBufferId();
  conflictScanBuffer = null;
  try {
    if (!editor.getBufferInfo(bufferId)) return;
    const text = await editor.getBufferText(bufferId);
    if (text.length > CONFLICT_MAX_CHARS || !text.includes("<<<<<<<")) {
      paintConflicts(bufferId, text, []);
      return;
    }
    paintConflicts(bufferId, text, parseConflicts(text));
  } catch (e) {
    editor.debug(`conflict scan: ${String(e)}`);
  }
}

function conflict_on_buffer_event(args: { buffer_id: number }): void {
  scheduleConflictScan(args.buffer_id);
}

registerHandler("conflict_scan_timer", conflict_scan_timer);
registerHandler("conflict_on_buffer_event", conflict_on_buffer_event);
editor.on("buffer_activated", "conflict_on_buffer_event");
editor.on("after_file_open", "conflict_on_buffer_event");
editor.on("after_file_revert", "conflict_on_buffer_event");
editor.on("buffer_modified", "conflict_on_buffer_event");

/* Resolution: state of the active buffer, edits, and the confirm step. */

interface ConflictContext {
  bufferId: number;
  path: string;
  text: string;
  cum: Uint32Array;
  blocks: ConflictBlock[];
  cursor: number; // byte offset
}

/** The pending rewrite: resolved once the buffer reads back as `expected`. */
interface PendingEdit {
  bufferId: number;
  expected: string;
  polls: number;
  action: string;
  title: string;
  then: () => Promise<void> | void;
}
let pendingEdit: PendingEdit | null = null;

/** Active buffer's conflicts, or an announce()d reason there are none. */
async function conflictContext(action: string, title: string): Promise<ConflictContext | null> {
  const bufferId = editor.getActiveBufferId();
  const path = editor.getBufferPath(bufferId);
  if (!path) {
    announce(action, title, "Conflicts: save the file first", "save the file first");
    return null;
  }
  if (pendingEdit) {
    announce(action, title, "Conflicts: an edit is still applying", "still applying");
    return null;
  }
  const text = await editor.getBufferText(bufferId);
  const blocks = text.includes("<<<<<<<") ? parseConflicts(text) : [];
  if (blocks.length === 0) {
    announce(action, title, "No merge conflicts in this file", "no conflicts in this file");
    return null;
  }
  const cursor = editor.getPrimaryCursor();
  return { bufferId, path, text, cum: byteIndex(text), blocks, cursor: cursor?.position ?? 0 };
}

/** Index of the conflict holding the cursor, else the next one (wrapping). */
function conflictTarget(ctx: ConflictContext): number {
  const inside = ctx.blocks.findIndex(
    (b) => ctx.cum[b.start] <= ctx.cursor && ctx.cursor < ctx.cum[b.end],
  );
  if (inside >= 0) return inside;
  const after = ctx.blocks.findIndex((b) => ctx.cum[b.start] > ctx.cursor);
  return after >= 0 ? after : 0;
}

/** The text that replaces a whole block (markers and base dropped). */
function replacementFor(text: string, b: ConflictBlock, side: ConflictSide): string {
  const current = text.slice(b.curStart, b.curEnd);
  const incoming = text.slice(b.incStart, b.incEnd);
  if (side === "current") return current;
  if (side === "incoming") return incoming;
  return current + incoming;
}

interface ConflictEdit {
  start: number; // UTF-16 index into ctx.text
  end: number;
  text: string;
}

/**
 * Queue the edits (ascending, non-overlapping) against the ORIGINAL text, last
 * first so earlier byte offsets stay valid. Returns the text the buffer should
 * read back as, or null if the host refused a command.
 */
function queueEdits(ctx: ConflictContext, edits: ConflictEdit[]): string | null {
  let out = "";
  let last = 0;
  for (const e of edits) {
    out += ctx.text.slice(last, e.start) + e.text;
    last = e.end;
  }
  out += ctx.text.slice(last);
  for (let k = edits.length - 1; k >= 0; k--) {
    const e = edits[k];
    const from = ctx.cum[e.start];
    if (!editor.deleteRange(ctx.bufferId, from, ctx.cum[e.end])) return null;
    if (e.text && !editor.insertText(ctx.bufferId, from, e.text)) return null;
  }
  return out;
}

function awaitConflictEdit(
  ctx: ConflictContext,
  action: string,
  title: string,
  expected: string,
  then: () => Promise<void> | void,
): void {
  pendingEdit = { bufferId: ctx.bufferId, expected, polls: 0, action, title, then };
  editor.setTimeout(CONFLICT_APPLY_POLL_MS, "conflict_apply_poll");
}

async function conflict_apply_poll(): Promise<void> {
  const p = pendingEdit;
  if (!p) return;
  let text = "";
  try {
    text = await editor.getBufferText(p.bufferId);
  } catch {
    text = "";
  }
  if (text === p.expected) {
    pendingEdit = null;
    try {
      await p.then();
    } catch (e) {
      announce(p.action, p.title, `Conflicts: ${String(e)}`, oneLine(String(e)));
    }
    return;
  }
  p.polls++;
  if (p.polls >= CONFLICT_APPLY_MAX_POLLS) {
    pendingEdit = null;
    announce(p.action, p.title, "Conflicts: the edit did not apply", "edit did not apply");
    return;
  }
  editor.setTimeout(CONFLICT_APPLY_POLL_MS, "conflict_apply_poll");
}

registerHandler("conflict_apply_poll", conflict_apply_poll);

/** Cursor to a conflict's <<<<<<< line, its line centred in the split. */
function jumpToBlock(bufferId: number, cum: Uint32Array, block: ConflictBlock): void {
  editor.setBufferCursor(bufferId, cum[block.start]);
  editor.scrollToLineCenter(editor.getActiveSplitId(), bufferId, block.startLine);
}

/** After a rewrite: jump to the next unresolved conflict, or finish the file. */
async function afterResolve(
  ctx: { bufferId: number; path: string },
  action: string,
  title: string,
  text: string,
  resolvedStart: number,
  label: string,
): Promise<void> {
  const blocks = parseConflicts(text);
  paintConflicts(ctx.bufferId, text, blocks);
  if (blocks.length === 0) {
    await finishFile(ctx, action, title);
    return;
  }
  const found = blocks.findIndex((b) => b.start >= resolvedStart);
  const idx = found >= 0 ? found : 0;
  jumpToBlock(ctx.bufferId, byteIndex(text), blocks[idx]);
  const msg = `Conflict ${idx + 1} of ${blocks.length}`;
  announce(action, title, `${label} · ${msg}`, msg);
}

/**
 * The file has no conflicts left: save it in place, then open the next
 * conflicted file of the repository, or report that none remain.
 */
async function finishFile(
  ctx: { bufferId: number; path: string },
  action: string,
  title: string,
): Promise<void> {
  const name = conflictFileName(ctx.path);
  if (!editor.saveBufferToPath(ctx.bufferId, ctx.path)) {
    announce(action, title, `Conflicts: could not save ${name}`, `could not save ${name}`);
    return;
  }
  const next = await nextConflictedFile(ctx.path);
  if (next) {
    editor.openFile(next.path, next.line, null);
    const msg = `All conflicts in ${name} resolved and saved → next: ${next.name}`;
    announce(action, title, msg, msg);
    reassertStatusLater(msg);
    return;
  }
  const msg = "No conflicts left — stage the files in Source Control and commit";
  announce(action, title, msg, msg);
}

function conflictFileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/**
 * The first repository file, other than `currentPath`, still listed as
 * unmerged by git that holds conflict markers. `line` is 1-based.
 */
async function nextConflictedFile(
  currentPath: string,
): Promise<{ path: string; name: string; line: number } | null> {
  const top = await editor.spawnProcess("git", ["rev-parse", "--show-toplevel"], editor.pathDirname(currentPath));
  if (top.exit_code !== 0) return null;
  const root = top.stdout.trim();
  if (!root) return null;
  const unmerged = await editor.spawnProcess(
    "git",
    ["diff", "--name-only", "-z", "--diff-filter=U"],
    root,
  );
  if (unmerged.exit_code !== 0) return null;
  for (const rel of unmerged.stdout.split("\0")) {
    if (!rel) continue;
    const full = `${root}/${rel}`;
    if (full === currentPath) continue;
    const content = editor.readFile(full);
    if (!content || !content.includes("<<<<<<<")) continue;
    const blocks = parseConflicts(content);
    if (blocks.length === 0) continue;
    return { path: full, name: conflictFileName(full), line: blocks[0].startLine + 1 };
  }
  return null;
}

/** Accept one side of the conflict at (or after) the cursor. */
async function resolveConflict(action: string, title: string, side: ConflictSide): Promise<void> {
  try {
    const ctx = await conflictContext(action, title);
    if (!ctx) return;
    const block = ctx.blocks[conflictTarget(ctx)];
    const expected = queueEdits(ctx, [
      { start: block.start, end: block.end, text: replacementFor(ctx.text, block, side) },
    ]);
    if (expected === null) {
      announce(action, title, "Conflicts: the editor refused the edit", "edit refused");
      return;
    }
    awaitConflictEdit(ctx, action, title, expected, () =>
      afterResolve(ctx, action, title, expected, block.start, CONFLICT_SIDE_LABEL[side]),
    );
  } catch (e) {
    announce(action, title, `Conflicts: ${String(e)}`, oneLine(String(e)));
  }
}

/** Accept one side in every conflict of the buffer, then finish the file. */
async function resolveAllConflicts(action: string, title: string, side: ConflictSide): Promise<void> {
  try {
    const ctx = await conflictContext(action, title);
    if (!ctx) return;
    const n = ctx.blocks.length;
    const expected = queueEdits(
      ctx,
      ctx.blocks.map((b) => ({ start: b.start, end: b.end, text: replacementFor(ctx.text, b, side) })),
    );
    if (expected === null) {
      announce(action, title, "Conflicts: the editor refused the edit", "edit refused");
      return;
    }
    const label = `${CONFLICT_SIDE_LABEL[side]} in all ${n} conflicts`;
    awaitConflictEdit(ctx, action, title, expected, () =>
      afterResolve(ctx, action, title, expected, 0, label),
    );
  } catch (e) {
    announce(action, title, `Conflicts: ${String(e)}`, oneLine(String(e)));
  }
}

/* Navigation. */

function goToBlock(ctx: ConflictContext, action: string, title: string, idx: number, prefix: string): void {
  jumpToBlock(ctx.bufferId, ctx.cum, ctx.blocks[idx]);
  const msg = `${prefix} ${idx + 1} of ${ctx.blocks.length}`;
  announce(action, title, msg, msg);
}

/**
 * Fresh's built-in merge plugin sets its own hint when a conflicted file
 * opens, which lands after our status. Re-assert ours once it has settled
 * (the toast already carried it).
 */
const CONFLICT_STATUS_REASSERT_MS = 600;
let heldStatus: string | null = null;

function reassertStatusLater(msg: string): void {
  heldStatus = msg;
  editor.setTimeout(CONFLICT_STATUS_REASSERT_MS, "conflict_reassert_status");
}

function conflict_reassert_status(): void {
  if (heldStatus !== null) editor.setStatus(heldStatus);
  heldStatus = null;
}

registerHandler("conflict_reassert_status", conflict_reassert_status);

/** Next conflict after the cursor; past the last, the next conflicted file. */
async function conflict_next(): Promise<void> {
  const action = "conflict_next";
  const title = "Next Conflict";
  try {
    const ctx = await conflictContext(action, title);
    if (!ctx) return;
    const at = ctx.blocks.findIndex((b) => ctx.cum[b.start] > ctx.cursor);
    if (at >= 0) {
      goToBlock(ctx, action, title, at, "Conflict");
      return;
    }
    // Past the last conflict: move to the next conflicted file. Nothing is saved.
    const other = await nextConflictedFile(ctx.path);
    if (other) {
      editor.openFile(other.path, other.line, null);
      const msg = `Next conflicted file: ${other.name}`;
      announce(action, title, msg, other.name);
      reassertStatusLater(msg);
      return;
    }
    goToBlock(ctx, action, title, 0, "Wrapped to conflict");
  } catch (e) {
    announce(action, title, `Next conflict: ${String(e)}`, oneLine(String(e)));
  }
}

/** Previous conflict before the cursor's own block; wraps to the last. */
async function conflict_prev(): Promise<void> {
  const action = "conflict_prev";
  const title = "Previous Conflict";
  try {
    const ctx = await conflictContext(action, title);
    if (!ctx) return;
    const inside = conflictTarget(ctx);
    const holds = ctx.cum[ctx.blocks[inside].start] <= ctx.cursor && ctx.cursor < ctx.cum[ctx.blocks[inside].end];
    let at = -1;
    for (let i = 0; i < ctx.blocks.length; i++) {
      if (holds && i === inside) continue;
      if (ctx.cum[ctx.blocks[i].start] < ctx.cursor) at = i;
    }
    if (at >= 0) {
      goToBlock(ctx, action, title, at, "Conflict");
      return;
    }
    goToBlock(ctx, action, title, ctx.blocks.length - 1, "Wrapped to conflict");
  } catch (e) {
    announce(action, title, `Previous conflict: ${String(e)}`, oneLine(String(e)));
  }
}

registerHandler("conflict_accept_current", () =>
  resolveConflict("conflict_accept_current", "Accept Current", "current"),
);
registerHandler("conflict_accept_incoming", () =>
  resolveConflict("conflict_accept_incoming", "Accept Incoming", "incoming"),
);
registerHandler("conflict_accept_both", () =>
  resolveConflict("conflict_accept_both", "Accept Both", "both"),
);
registerHandler("conflict_accept_all_current", () =>
  resolveAllConflicts("conflict_accept_all_current", "Accept All Current", "current"),
);
registerHandler("conflict_accept_all_incoming", () =>
  resolveAllConflicts("conflict_accept_all_incoming", "Accept All Incoming", "incoming"),
);
registerHandler("conflict_next", conflict_next);
registerHandler("conflict_prev", conflict_prev);

editor.registerCommand(
  "Conflict: Accept Current",
  "Resolve the conflict at the cursor with the current (HEAD) side",
  "conflict_accept_current",
  null,
);
editor.registerCommand(
  "Conflict: Accept Incoming",
  "Resolve the conflict at the cursor with the incoming side",
  "conflict_accept_incoming",
  null,
);
editor.registerCommand(
  "Conflict: Accept Both",
  "Resolve the conflict at the cursor with current then incoming",
  "conflict_accept_both",
  null,
);
editor.registerCommand(
  "Conflict: Next",
  "Jump to the next conflict; past the last one, open the next conflicted file",
  "conflict_next",
  null,
);
editor.registerCommand(
  "Conflict: Previous",
  "Jump to the previous conflict (wraps to the last)",
  "conflict_prev",
  null,
);
editor.registerCommand(
  "Conflict: Accept All Current (file)",
  "Resolve every conflict in this file with the current side, then save and move on",
  "conflict_accept_all_current",
  null,
);
editor.registerCommand(
  "Conflict: Accept All Incoming (file)",
  "Resolve every conflict in this file with the incoming side, then save and move on",
  "conflict_accept_all_incoming",
  null,
);
