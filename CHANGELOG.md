# Changelog

All notable changes to VeloEdit are recorded here. Versions follow the
release asset naming in the specification.

## [1.1.0] — Unreleased

### Added

- Scoped search core: find, count, and replace-all can be limited
  to a byte range (for example the current selection); only matches
  lying fully inside the range are reported or replaced, and a
  scoped replace-all is still a single undo step.
- Masked byte search core: patterns like `4A ?? FF 00 2B` with
  per-nibble wildcards (`4?`, `?F`) find binary sequences in hex
  data, streaming over the document like plain search.
- Line operations core: sort the lines in a range (byte-wise,
  stable, each line keeping its own line ending) or remove
  duplicate lines keeping the first occurrence — each as a single
  undo step, refusing ranges over 64 MB.
- Search in selection (window): the find bar gains an "In
  selection" toggle — with a selection made, Find Next, the match
  count, and Replace All work only inside it (Replace All stays
  one undo step); with no selection, the bar says so.
- Masked search (window): in hex mode, a find text that parses as
  a byte pattern (it contains a space or a `?`, like `4A ?? FF`)
  searches by pattern, with the same match navigation and
  highlighting as a plain hex find. Patterns are find-only;
  replace reports that instead of guessing.
- Sort / dedup (window): Edit ▸ Sort Lines in Selection and Edit
  ▸ Remove Duplicate Lines in Selection, enabled when a selection
  exists; an over-limit range is refused with an explanation.
- File splitter: `--split <FILE> --part-size <SIZE>` (sizes like
  `500M` or `4G`) writes `<name>.part001`, `.part002`, … parts,
  streamed, and File ▸ Split File Copy… does the same from the
  window through a small dialog (part size in MB, default 4096),
  splitting a copy in a `<name>-parts` folder and reporting the
  part count. At most 10,000 parts are made.
- Explorer menu item (opt-in): `--install-shell` adds an "Open in
  VeloEdit" entry to the Explorer right-click menu for all files
  (current user only, no administrator rights);
  `--uninstall-shell` removes it, `--clean` removes it too, and
  File ▸ Explorer Menu Item toggles it from the window. This is
  the only registry entry the program ever writes.
- Benchmark harness: `scripts/Run-Benchmarks.ps1` builds sparse
  test files (1 GB by default) and times cold launch, find,
  save-copy, and extract with peak memory, and a manual-only
  GitHub workflow runs it at 1 GB on a Windows runner and uploads
  the table.
- Ctrl+mouse-wheel zoom: holding Ctrl and rolling the mouse wheel
  steps the font size up or down (the same steps, limits, and
  saving as View ▸ Font Size); a plain wheel scrolls as before.
- Live theme and accent preview: moving through the View ▸ Theme
  and View ▸ Accent menus (mouse or keyboard) previews each
  highlighted choice in the window without saving it — clicking
  keeps the choice, closing the menu without a click puts the
  previous theme and accent back.
- File hashes core: MD5, SHA-1, and SHA-256 written by hand from
  their public specifications (no library, per the dependency
  rule), streaming over a file in 64 KB windows so hashing never
  loads the file.
- Data inspector core: the bytes at a position read as signed and
  unsigned 8/16/32/64-bit integers and 32/64-bit floats in both
  byte orders, plus hex/binary renderings, Unix-timestamp
  readings (seconds and milliseconds, UTC), a GUID reading (the
  standard layout, first three fields little-endian as Windows
  stores them), a Windows FILETIME reading, and a short UTF-8
  text preview.
- Bookmarks core: named anchors at byte offsets, kept per file in
  `data/bookmarks.json` and guarded by the file's size and
  modification stamp — a file that changed since its bookmarks
  were set drops them rather than pointing at the wrong bytes.
  Toggle and next/previous navigation with wrap-around.
- Delta-log recovery core: a document's recovery record now
  stores the piece-table state — which parts of the original file
  are still visible and the bytes of every edit — plus the
  original file's SHA-256 fingerprint, instead of a copy of the
  document's contents, so recovery is no longer limited by a
  content-size cap. Rebuilding re-checks the fingerprint and
  refuses when the original file changed.
- Line-ending and BOM control on save: a save can normalise line
  endings to LF or CRLF and add or remove the byte-order mark,
  applied as a streaming transform in the document encoding's
  unit width (UTF-16 included), correct across chunk boundaries.
  The default keeps both exactly as they are.
- Incremental find-all core: a windowed find returns the matches
  starting inside one byte window, so a background worker can
  populate the full match list piece by piece; joined over
  consecutive windows the results equal a one-shot find-all.
- Bookmarks (window): Ctrl+F2 toggles a bookmark at the caret,
  F2 / Shift+F2 jump to the next / previous bookmark with
  wrap-around. A bookmarked line carries a small accent square in
  the gutter (in hex mode, beside the row's offset), and bookmarks
  also appear as ticks in the scrollbar. They persist per file in
  `data/bookmarks.json` and are dropped when the file changes.
- Background find-all and scrollbar ticks (window): on documents
  over 4 MB, a plain find's match list now fills in on a worker
  thread — the count reads "Counting…" until the last window
  lands, Find Next / Previous already work over the matches found
  so far, and the first match is selected as soon as it is known.
  Matches (accent) and bookmarks (text colour) draw as ticks in
  the scrollbar track, one per track row. Documents up to 4 MB
  keep the synchronous find, as do scoped and masked finds.
- External-change notice: if the open file changes on disk, a
  non-modal bar appears directly under the toolbar — "This file
  changed on disk." with Reload and Ignore buttons — when the
  window gains focus or just before a save. Reload re-opens the
  file (asking about unsaved changes first, and cancelling keeps
  the bar); Ignore dismisses the notice until the file changes
  again.
- Delta recovery (window): documents backed by a file now record
  their recovery as the delta record (`data/recovery2.bin` — the
  edit log plus the original file's fingerprint) at any size; the
  fingerprint is computed on a background thread after opening.
  Startup checks the delta record first: if the original file
  changed, the record is deleted and a plain message says the
  recovery was skipped; otherwise the rebuilt document is offered
  exactly like the snapshot was. The 16 MB content snapshot
  remains for documents with no backing file, and a successful
  save or clean exit clears both forms.
- Data Inspector (window): View ▸ Data Inspector docks a panel on
  the right showing the readings of the 16 bytes at the caret —
  integers and floats in both byte orders, hex and binary, Unix
  and FILETIME dates, a GUID, and a text preview — refreshed as
  the caret moves or the bytes change. The editor area shrinks
  while the panel is open, and the open state is remembered.
- Hex layout choices: View ▸ Hex Columns shows 8, 16, 24, 32 or
  64 bytes per row, or fits the row width to the window
  (recomputed on resize). View ▸ Hex Grouping displays bytes in
  groups of 2, 4 or 8, and Big-Endian Groups shows each group
  reversed, the way a stored number reads — display only: the
  file's byte order never changes, the ASCII column is
  unaffected, and the caret stays byte-based. All three choices
  are remembered.
- Relative offsets: View ▸ Relative Offsets shows hex row
  addresses as distances from a base — the caret when the mode is
  switched on — with signed negative offsets for rows above the
  base; Set Offset Base at Caret moves the base, and the status
  bar names the base while the mode is on.
- Save Options (window): the File ▸ Save Options submenu chooses
  the save pipeline's line-ending conversion (Keep / LF / CRLF)
  and byte-order-mark handling (Keep / Add / Remove); the choices
  persist, apply to Save and Save As, and show in the status bar
  (for example "· → CRLF · +BOM") while a conversion is armed.
- File Hashes (window): File ▸ File Hashes… streams the file on
  disk through MD5, SHA-1 and SHA-256 on a worker thread and
  fills in the three digests when the hashing completes; a
  compare box reports which digest a pasted checksum matches, or
  "No match". When the document has unsaved changes the dialog
  says it is hashing the file on disk.

### Changed

- Piece lookups are now logarithmic: a Fenwick weight index over
  the piece lengths replaces the linear piece scan for offset
  lookups, and neighbouring pieces pointing at contiguous bytes in
  the same store are merged automatically once the table grows
  past 8,192 pieces.
- Line indexing is faster and parallel: newline counting processes
  eight bytes per step with word-parallel (SWAR) bit tricks, and
  the initial index build counts blocks on up to eight worker
  threads, producing checkpoints identical to the serial build.
- Search scanning uses the same word-parallel technique to filter
  candidate positions before verifying them; results are unchanged.
- Find bar restyle: the bar is now the same height as the toolbar
  and its buttons share the toolbar buttons' size and drawn style.
  The field labels and the match count sit on the bar's own
  background instead of in panel-coloured boxes — the count in the
  muted colour, vertically centred with the buttons, with clear
  space on both sides — and the "In selection" toggle shows its
  on state like the Text / Hex toggles (accent border and text,
  with a ✓ before its name) instead of a ballot-box glyph.
- Instant open for huge files: opening a file no longer waits
  for its whole line index to be built. The window opens after
  indexing just the first few megabytes — enough for the first
  screens — and the rest of the index finishes on the background
  thread while you work. The status bar shows "Indexing NN%"
  until the index lands, and until then the scrollbar measures
  your place by file size rather than by line; scrolling reaches
  as far as the index has built, which sweeps the file in
  seconds. Positions are exact at every stage, and the finished
  index is identical to a one-shot build.
- Window polish pass: the toolbar buttons (Open, Save, Undo,
  Redo, Find, Replace) carry small drawn icons beside their
  labels. The Text / Hex switch is now one segmented control —
  the two sides joined with no gap under one shared frame and a
  single divider, the side in use filled with the accent colour
  and labelled in white. Every owner-drawn button (toolbar,
  find bar, change bar, and the dialogs) now shows keyboard
  focus as an accent frame inset inside its border. The current
  line sits on a raised band — the background blended halfway
  toward the panel colour — spanning the text area in both
  modes, and line numbers away from the current line are drawn
  one step brighter than before. The status bar leads its
  right-hand facts with the text size as a percentage (100% is
  the standard size, updated live by the font commands and
  Ctrl+mouse-wheel) and keeps clear space at both ends of the
  window.

## [1.0.1] — Unreleased

### Fixed

- The editor window failed to start (error 0x80004002): one byte of
  the DirectWrite factory identifier was wrong, so DirectWrite
  refused the startup request while Direct2D started normally. The
  window now opens, paints, and resizes correctly.
- Six internal Direct2D / DirectWrite call addresses (clear, begin
  frame, end frame, set DPI, resize, text measuring) were corrected
  to the values in the Windows SDK headers; they would have painted
  wrongly even once startup succeeded.
- A startup failure now reports the exact Direct2D and DirectWrite
  error codes in its message and on the console, instead of a
  generic note.
- Launching the editor no longer opens a console window beside
  it. The command-line tools are unchanged: started from a
  terminal, they print in that terminal as before.

### Changed

- The editor window now follows the design mockup: a dark themed
  title bar showing the document's file name, an accent strip along
  the top of the window, and a Search menu between Edit and View.
- Toolbar and find-bar buttons are flat and theme-drawn with thin
  borders; the mode switch is a pair of Text / Hex toggle buttons
  at the right of the toolbar, with the active mode shown in the
  accent colour.
- The find bar has "Find:" and "Replace:" labels and borderless,
  theme-coloured input fields.
- The scrollbar is drawn by the editor itself — a slim track with
  an accent-coloured thumb — instead of the system scrollbar.
- The status bar shows the caret position and selection size on
  the left, and the encoding, line-ending style, and file size on
  the right; sizes above 1 KB use one decimal (for example
  "104.2 GB").
- Every find match in view is washed with the match colour and
  the current match is also outlined in the accent colour; the
  current line's number (or hex address) uses the accent colour.
- Command-line exit codes now follow one scheme everywhere:
  0 = success (for `--find`: at least one match; for `--diff`:
  files identical), 1 = `--find` found nothing / `--diff` found
  differences, 2 = usage or I/O error. Previously `--diff` used 3
  for "files differ", `--find` returned 0 even with no matches,
  and errors returned 1.
- When the command-line tools run in a terminal, the console now
  uses UTF-8, so file names and reports print correctly.

### Added

- Program icon: the supplied artwork is embedded in the executable
  (Explorer, taskbar, and window icon), and the same icon is used by
  the installer, the uninstaller, and the MSI package.
- `extract` command: `VeloEdit extract <file> --start <N>
  --length <M> --out <file>` copies one byte range of a file to
  another file, streamed in 4 MB chunks. Offsets are decimal or
  0x-hex; the range is clamped to the file, and a start past the
  end of the file is an error.
- File ▸ Save Selection to File… writes the selected bytes
  straight to a file, streamed in chunks — a selection of any size
  can be exported, with no clipboard limits.
- View ▸ Column Ruler cycles a vertical guideline at column 80 or
  120 (or off), drawn in text mode; the choice is remembered.
- Two more themes — "High Contrast" (black background, white text,
  yellow accent) and "E-ink" (paper background, near-black text) —
  for 20 built-in themes in total. The accent picker gains three
  colour-blind-safe presets: Okabe Blue, Okabe Orange, Okabe Sky.
- Saves and save-copy now check the free disk space before any
  byte is written: when the target's disk has less free than the
  document's size plus 5%, the save is refused up front with a
  clear message instead of failing halfway through the write.
- Undo depth is now configurable in settings: `undo_limit` (steps,
  default 1024) and `undo_byte_budget_mb` (megabytes of stored
  changes, default 256). The oldest changes are dropped first when
  either limit is reached.

## [1.0.0] — 2026-10-31

The 1.0.0 build: the editing core, the headless tools, the graphical
editor window, and the packaging scripts.

### Added

- Piece table editing core: documents are edited as an ordered list of
  pieces over the original bytes and an append-only buffer, so inserts and
  deletes never copy or shift file data. Sequential typing coalesces into a
  single piece instead of fragmenting the table.
- Sparse line index: line checkpoints every 64 KB with binary-search
  lookup and forward-scan line location, so line numbers resolve without
  scanning the whole file.
- Hex view arithmetic: O(1) row-to-offset conversion and 64-bit-ratio
  scrollbar maths that stays exact for files past 2 billion bytes.
- Undo/redo engine: edits are stored as inversion deltas and undo restores
  the exact previous piece layout without touching the append buffer.
  History is bounded (1024 entries by default in this build).
- Headless binary diff: `VeloEdit --diff A B --output report.json
  --headless` streams both files in constant memory and writes a JSON
  report (exit code 0 when identical, 3 when different).
- Command-line interface with `--line`, `--mode hex|text`, `--read-only`,
  `--json`, `--silent`, `--dry-run`, `--verbose`, `--debug`, `--trace`,
  `--help`, and `--version`.
- Portable cleanup: `--clean` / `--uninstall` remove only the `data/`
  folder next to the program file, with a `--dry-run` preview.
- Project documents: `PROJECT_RULES.md`, `QUALITY.md`, `docs/PARITY.md`
  (operation registry), and `docs/SPEC.md` (the governing specification).
- Text encodings (owner-confirmed for v1): detection for UTF-8, UTF-16
  LE/BE, and ANSI (Windows-1252) — byte-order marks first, a NUL-pattern
  check for unmarked UTF-16, strict UTF-8 validation, and an ANSI
  fallback. Strict decode/encode for all four encodings: malformed
  UTF-16 is reported with its byte position instead of being silently
  repaired, and every one of the 256 ANSI byte values round-trips
  exactly through decode and encode.
- Search and replace over the piece table (owner-confirmed for v1):
  find-all and find-next stream the document piece by piece without
  loading it whole, and find matches that span piece boundaries.
  Replace and replace-all are each recorded as one undo step, so a
  single undo restores the document exactly. Replace-all replaces
  non-overlapping matches, in the usual editor way.
- Headless find from the command line: `VeloEdit.exe <file> --find
  <TEXT> --headless` prints the match count and byte offsets as JSON
  and exits 0 whether or not anything matches; `--encoding` forces the
  encoding instead of detecting it.
- Windows memory-mapped file backing (SPEC §2.1): on Windows a file is
  opened with `CreateFileW` / `CreateFileMappingW` / `MapViewOfFile`
  as a read-only view, so opening even a very large file loads nothing
  into memory up front — the operating system pages bytes in as they
  are read. Zero-length files take a dedicated empty path (a mapping
  is never created for them). Open failures name the step that failed
  (open, size, mapping, or view).
- Atomic save (SPEC §5.1): saving streams the document to a staging
  file next to the target (`<filename>.tmp_velo_<pid>`) in chunks of
  at most 4 MB, flushes it to disk, closes our own mapping of the
  target, then swaps the staging file into place with `ReplaceFileW`,
  and re-opens the saved file as the new original. A failure before
  the swap leaves the original file untouched and removes the staging
  file. Saving clears the undo history — it is a clean starting point,
  like reopening the file. Headless form: `VeloEdit.exe <file>
  --save-copy <DEST>` saves a copy through the same pipeline and
  prints a JSON report with `--json`.

- Graphical editor window (Windows): a pure Win32 window with
  Direct2D/DirectWrite rendering of only the visible rows, per-monitor
  DPI scaling, a themed gutter with line numbers, current-line and
  selection highlighting, a blinking caret, and a status bar showing
  line, column, byte offset, encoding, size, and theme. Menus, a toolbar
  with tooltips, keyboard shortcuts for every action, drag-and-drop
  opening, and a recent-files list. The window is a thin shell over a
  tested editor core, so every action behaves the same in tests as in
  the window. (Windows runtime behaviour is unverified until a real run
  on Windows 11.)
- Text and hex modes (F6): hex mode shows 16 bytes per row with offsets
  and an ASCII column; typing hex digits edits bytes in place.
- Find and replace bar (Ctrl+F / Ctrl+H) with a live match count,
  previous/next navigation, replace, and replace-all as one undo step.
- 18 built-in colour themes (View ▸ Theme) plus a separate accent
  colour choice (View ▸ Accent: theme default, 8 presets, or any custom
  colour), applied instantly and remembered between runs.
- Portable settings: theme, accent, font size, window position, and
  recent files are stored in `data/settings.json` next to the program;
  a damaged settings file falls back to defaults and is kept aside as
  `settings.json.bad`.
- Crash recovery: while you edit a document up to 16 MB, a snapshot is
  kept in the `data/` folder and offered back on the next start if the
  program closed unexpectedly; it is removed on a clean save or close.
- Built-in guide (F1 / Help ▸ Guide) that works fully offline.
- Packaging for the §6 release matrix: an NSIS installer script
  (`installer/VeloEdit.nsi`, per-user install with an uninstaller and
  an opt-in data-folder removal), a WiX v4 package script
  (`installer/Package.wxs`, per-user MSI with major-upgrade support),
  and a release workflow (`.github/workflows/release.yml`) that — only
  when a `v*` tag is pushed or it is run manually — gates the build,
  produces `VeloEdit-v1.0.0-x64-Setup.exe`,
  `VeloEdit-v1.0.0-x64-Setup.msi`, and
  `VeloEdit-v1.0.0-x64-Portable.zip` plus `SHA256SUMS.txt`, and attaches
  them to the GitHub Release. Host-side benchmark evidence for the core
  tools is recorded in `docs/BENCHMARKS.md`.

### Not yet included

- Any published release. The packaging is built and the installer
  script is proven; creating the 1.0.0 release itself is still to come.
