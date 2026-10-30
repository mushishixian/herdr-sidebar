/**
 * Git: Resolve in Merge Editor
 *
 * Opens the active buffer's file in herdr's 3-way merge editor: a herdr popup
 * (the plugin `herdr-sidebar`, entrypoint `merge-editor`) showing Incoming and
 * Current side by side with the Result below. The popup reads the file from
 * disk, so unsaved edits in this buffer are not part of the merge.
 *
 * No keybinding lives here: the merge editor's key is Ctrl+Alt+U in herdr's
 * own keymap (config.toml). It is listed in the Selection menu, next to Git:
 * Line History, and in the command palette.
 *
 * Each run raises a herdr toast (title "Git: Resolve in Merge Editor", body =
 * file name). The toast helper is duplicated from git_line_history.ts: plugins
 * cannot import each other.
 */

const editor = getEditor();

const HERDR_PLUGIN = "herdr-sidebar";
const HERDR_ENTRYPOINT = "merge-editor";
const TITLE = "Git: Resolve in Merge Editor";

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

/** Status-bar message plus a herdr toast. */
function announce(status: string, body: string): void {
  editor.setStatus(status);
  toast(TITLE, body);
}

/** Directory part of an absolute file path. */
function directoryOf(path: string): string {
  const slash = path.lastIndexOf("/");
  if (slash < 0) return ".";
  if (slash === 0) return "/";
  return path.slice(0, slash);
}

/** Last path segment, for a short toast body. */
function basenameOf(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/** First non-empty line of a process's stderr, for the status bar. */
function firstLine(text: string): string {
  return text.split("\n").map((l) => l.trim()).find((l) => l.length > 0) ?? "";
}

async function git_merge_editor(): Promise<void> {
  try {
    const bufferId = editor.getActiveBufferId();
    const filePath = editor.getBufferPath(bufferId);
    if (!filePath) {
      announce("Merge editor: save the file first", "save the file first");
      return;
    }

    if (editor.isBufferModified(bufferId)) {
      editor.setStatus(
        "Merge editor: this buffer has unsaved changes; the merge editor reads the saved file",
      );
    }

    const dir = directoryOf(filePath);
    const args = [
      "plugin", "pane", "open",
      "--plugin", HERDR_PLUGIN,
      "--entrypoint", HERDR_ENTRYPOINT,
      "--placement", "popup",
      "--width", "90%",
      "--height", "85%",
      "--env", `HERDR_SIDEBAR_SPAWN_CWD=${dir}`,
      "--env", `HERDR_SIDEBAR_MERGE_FILE=${filePath}`,
    ];

    const name = basenameOf(filePath);
    const result = await editor.spawnProcess("herdr", args, dir);
    if (result.exit_code !== 0) {
      const reason =
        firstLine(result.stderr) || `herdr exited with code ${result.exit_code}`;
      announce(`Merge editor: ${reason}`, reason.slice(0, 100));
    } else {
      announce(`Merge editor: ${name}`, name);
    }
  } catch (e) {
    announce(`Merge editor: ${String(e)}`, String(e).slice(0, 100));
  }
}

registerHandler("git_merge_editor", git_merge_editor);

editor.registerCommand(
  "Git: Resolve in Merge Editor",
  "Open the active file in the herdr 3-way merge editor",
  "git_merge_editor",
  null,
);

editor.addMenuItem({
  menu: "Selection",
  label: "Git: Resolve in Merge Editor",
  action: "git_merge_editor",
  after: "expand_selection",
});
