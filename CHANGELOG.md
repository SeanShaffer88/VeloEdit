# Changelog

All notable changes to VeloEdit are recorded here. Versions follow the
release asset naming in the specification once releases begin; no release
has been published yet.

## [1.0.0] — Unreleased

The 1.0.0 build: the editing core, the headless tools, the graphical
editor window, and the packaging scripts. (The release date is added
when the release is published.)

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
