/**
 * Session memory: remember the open files so they come back after a restart.
 *
 * Fresh writes its workspace file only on a clean quit. Kill the daemon (herdr
 * restart, reboot) and the next start restores a stale list. So this plugin keeps
 * its own record: on every open, tab switch, close and save it writes the real
 * file buffers to $FRESH_IDE_SESSION_FILE (set by fresh-ide). The
 * fresh-session-restore script reopens them when the daemon comes back.
 *
 * Without that variable (Fresh outside the IDE) the plugin does nothing.
 * No timers: it runs only on editor events and writes only when the list changes.
 */

const editor = getEditor();

/** Where the record goes. Empty means the plugin is inactive. */
const SESSION_FILE: string = editor.getEnv("FRESH_IDE_SESSION_FILE") ?? "";

/** Last record written, without saved_at, so an unchanged state is not rewritten. */
let lastWritten = "";
/** Last real file that was active. Kept while the active buffer is not a file. */
let lastActive: string | null = null;

/** A real file buffer: backed by a path, not virtual, not a terminal. */
function isFileBuffer(b: BufferInfo): boolean {
  return b.path !== "" && !b.is_virtual && !b.is_terminal;
}

/** Build the file list and active path. `exclude` is a buffer being closed. */
function snapshot(exclude: number | null): { files: string[]; active: string | null } {
  const activeId = editor.getActiveBufferId();
  const files: string[] = [];
  let active: string | null = null;
  for (const b of editor.listBuffers()) {
    if (b.id === exclude || !isFileBuffer(b)) continue;
    if (!files.includes(b.path)) files.push(b.path);
    if (b.id === activeId) active = b.path;
  }
  if (active !== null) {
    lastActive = active;
  } else if (lastActive !== null && files.includes(lastActive)) {
    active = lastActive;
  }
  return { files, active };
}

function writeSession(exclude: number | null): void {
  if (SESSION_FILE === "") return;
  const { files, active } = snapshot(exclude);
  // A daemon that is starting reports no files before fresh-session-restore has
  // reopened them; writing that would erase the record. Closing every tab
  // therefore keeps the previous list.
  if (files.length === 0) return;
  const state = JSON.stringify({ version: 1, files, active });
  if (state === lastWritten) return;
  const record = JSON.stringify({ version: 1, saved_at: Date.now(), files, active });
  // replaceFile, not writeFile: writeFile refuses a path that already exists.
  if (editor.replaceFile(SESSION_FILE, record)) lastWritten = state;
}

function session_memory_update(): void {
  try {
    writeSession(null);
  } catch (e) {
    editor.debug(`session memory: ${String(e)}`);
  }
}

/** buffer_closed carries the id of the buffer going away; it may still be listed. */
function session_memory_closed(args: { buffer_id: number }): void {
  try {
    writeSession(args.buffer_id);
  } catch (e) {
    editor.debug(`session memory: ${String(e)}`);
  }
}

if (SESSION_FILE !== "") {
  registerHandler("session_memory_update", session_memory_update);
  registerHandler("session_memory_closed", session_memory_closed);
  editor.on("after_file_open", "session_memory_update");
  editor.on("buffer_activated", "session_memory_update");
  editor.on("after_file_save", "session_memory_update");
  editor.on("buffer_closed", "session_memory_closed");
}
