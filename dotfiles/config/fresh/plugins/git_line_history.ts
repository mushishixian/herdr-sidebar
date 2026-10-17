/**
 * Git: Line History
 *
 * Opens a herdr popup showing the git history of the selected lines (or the
 * cursor line when nothing is selected). The popup itself is the herdr
 * plugin `herdr-sidebar`, entrypoint `git-line-history`; this file only
 * launches it, with the file and line range passed through --env flags.
 *
 * Bound to a key through config.json (Ctrl+Alt+O, see the keybindings note in
 * the accompanying report); also listed in the Selection menu and reachable by
 * right-clicking in a buffer.
 *
 * Each run also raises a herdr toast (title "Git: Line History", body = key and
 * file:range), via a small helper duplicated from selection_share.ts: plugins
 * cannot import each other.
 */

const editor = getEditor();

const HERDR_PLUGIN = "herdr-sidebar";
const HERDR_ENTRYPOINT = "git-line-history";

/**
 * Fallback key label for the toast. MUST MATCH config.json's keybindings; used
 * only when the runtime lookup (editor.getUserConfig) finds no binding.
 */
const FALLBACK_KEY = "Ctrl+Alt+O";
const ACTION = "git_line_history";

/** "Ctrl+Alt+O" for the config binding of ACTION, else FALLBACK_KEY. */
function keyLabel(): string {
  const order = ["ctrl", "alt", "shift", "super"];
  const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
  try {
    const hit = editor
      .getUserConfig()
      .keybindings?.find((b) => b.action === ACTION && b.key);
    if (hit?.key) {
      const mods = [...(hit.modifiers ?? [])].sort(
        (a, b) => order.indexOf(a) - order.indexOf(b),
      );
      return [...mods.map(cap), cap(hit.key)].join("+");
    }
  } catch {
    // Config unavailable: use the fallback.
  }
  return FALLBACK_KEY;
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
function announce(status: string, body: string): void {
  editor.setStatus(status);
  toast("Git: Line History", `${keyLabel()} · ${body}`);
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

/** Directory part of an absolute file path. */
function directoryOf(path: string): string {
  const slash = path.lastIndexOf("/");
  if (slash < 0) return ".";
  if (slash === 0) return "/";
  return path.slice(0, slash);
}

/** First non-empty line of a process's stderr, for the status bar. */
function firstLine(text: string): string {
  return text.split("\n").map((l) => l.trim()).find((l) => l.length > 0) ?? "";
}

async function git_line_history(): Promise<void> {
  try {
    const bufferId = editor.getActiveBufferId();
    const filePath = editor.getBufferPath(bufferId);
    if (!filePath) {
      announce("Line history: save the file first", "save the file first");
      return;
    }

    const cursor = editor.getPrimaryCursor();
    if (!cursor) {
      announce("Line history: no cursor", "no cursor");
      return;
    }

    if (editor.isBufferModified(bufferId)) {
      editor.setStatus(
        "Line history: buffer has unsaved changes; history reflects the saved file, line numbers may be off",
      );
    }

    // Line numbers come from the buffer's own text so they work even when the
    // editor has no line index yet (cursor.line is null on huge files).
    const text = await editor.getBufferText(bufferId);
    const range = historyLineRange(
      lineStartOffsets(text),
      cursor.position,
      cursor.selection,
    );

    const dir = directoryOf(filePath);
    const args = [
      "plugin", "pane", "open",
      "--plugin", HERDR_PLUGIN,
      "--entrypoint", HERDR_ENTRYPOINT,
      "--placement", "popup",
      "--width", "90%",
      "--height", "90%",
      "--env", `HERDR_SIDEBAR_SPAWN_CWD=${dir}`,
      "--env", `HERDR_SIDEBAR_HISTORY_FILE=${filePath}`,
      "--env", `HERDR_SIDEBAR_HISTORY_LINES=${range.start},${range.end}`,
    ];

    const where = `${basenameOf(filePath)}:${range.start === range.end ? range.start : `${range.start}-${range.end}`}`;
    const result = await editor.spawnProcess("herdr", args, dir);
    if (result.exit_code !== 0) {
      const reason =
        firstLine(result.stderr) || `herdr exited with code ${result.exit_code}`;
      announce(`Line history: ${reason}`, reason.slice(0, 100));
    } else {
      announce(`Line history: ${where}`, where);
    }
  } catch (e) {
    announce(`Line history: ${String(e)}`, String(e).slice(0, 100));
  }
}

/** Last path segment, for a short toast body. */
function basenameOf(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

registerHandler("git_line_history", git_line_history);

editor.registerCommand(
  "Git: Line History",
  "Show the git history of the selected lines in a herdr popup",
  "git_line_history",
  null,
);

editor.addMenuItem({
  menu: "Selection",
  label: "Git: Line History",
  action: "git_line_history",
  after: "expand_selection",
});

/**
 * A right-click in a buffer would run the same action as Ctrl+Alt+O.
 *
 * NOTE: Fresh 0.5.3 only emits `mouse_click` for the LEFT button — right and
 * middle clicks never reach this handler. Verified by injecting SGR mouse
 * sequences into a pane: `ESC [ < 0 ; … M` fires, `ESC [ < 2 ; … M` (right)
 * and `ESC [ < 1 ; … M` (middle) do not. Fresh also has no mouse-button
 * keybindings. So this stays dormant until a Fresh version reports them; the
 * working triggers are Ctrl+Alt+O and Selection → "Git: Line History".
 */
function on_mouse_click(ev: MouseClickHookArgs): void {
  if (ev.button !== "right") return;
  void git_line_history();
}

registerHandler("git_line_history_click", on_mouse_click);
editor.on("mouse_click", "git_line_history_click");
