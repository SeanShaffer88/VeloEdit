# Changelog

All notable changes to VeloEdit are recorded here. Versions follow the
release asset naming in the specification.

## [1.1.1] — 2026-11-01

### Fixed

- Tab labels lost their last character ("Untitled" painted as
  "Untitle"): the strip sized each tab from an estimated label
  width and a chrome allowance smaller than what the painter
  spends, and the painter clips the label to the tab's rect.
  Tab widths now come from the label's measured width plus the
  painter's exact chrome.
- Status bar text was shaved at the bottom: the two status
  readouts kept the window class's default font with no
  vertical centring in a strip sized for the app's own font.
  They now carry an app-sized font, centre vertically, and
  the strip is taller.

- Switching between text and hex mode jumped the view to the
  caret — after pasting, that is the end of the file, so a
  text→hex→text round trip "moved the document to the end".
  Mode switches now translate the view's top through the
  shared byte anchor and keep the reader's place; the caret
  itself does not move.
- In the split view, the Text and Hex commands only moved the
  keyboard focus between panes, leaving no way back to a
  single view from them. They now leave the split for the
  named view, opening exactly where that pane was; focus
  inside the split still moves by clicking a pane or
  pressing F6.

## [1.1.0] — 2026-11-01

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
- Two more checksums in the hash core: CRC32 (IEEE) and xxHash64
  (seed 0), also written by hand and streaming through the same
  windowed interface as the digests, so the File Hashes results
  gain both values when the dialog surface is extended.
- Ghost Tail core: when the open file grows on disk, the document
  can now extend its view of the original file to the new length
  and resume line indexing from where the earlier build stopped,
  keeping the caret, selection and scroll position — no reload.
  A file that shrank or changed size-for-size never takes this
  path; it stays with the external-change notice. Only an
  unedited document may grow this way. The window behaviour is
  listed as Follow Tail with the view features below.
- Log filter core: the model behind filter mode — given a literal
  or masked search, the ordered list of matching lines with their
  byte ranges and two-way mapping between filtered rows and
  full-file lines/offsets, for one-click jumps back into the full
  file. On a still-building (partial) line index the filter
  covers only the indexed prefix and reports that it is
  incomplete. The filter window is listed with the view
  features below.
- Multi-range slicing core: carve several ranges out of a
  document into one streamed output — a spec like
  `1,000,000-1,050,000; L100-L200` mixes byte ranges (both ends
  included) and 1-based line ranges, resolves lines through the
  index, and writes the ranges in order in bounded chunks.
  Reversed, out-of-range, overlapping or out-of-order specs are
  refused with an explicit reason before anything is written.
  The command/window surface arrives in a later slice.
- Zero-run insert core: the piece table gains a true zeroes
  region — inserting a zero run stores no bytes, yet every read
  path (editor, find, save, extract) yields `0x00` for the run,
  a run next to a run extends it in place, and undo restores the
  previous layout exactly.
- Delta patch core: export the difference between a document's
  original file and its edited state as a compact binary patch
  (`VELOPAT1` format: a magic header, the original and result
  lengths, and an ordered hunk list — original offset, original
  length, replacement bytes; unchanged spans are referenced,
  never copied), parse it back with full validation, and apply
  it to the original to reproduce the document byte for byte.
  The export/apply surface arrives in a later slice.
- Strings core: scan a document for printable ASCII runs and
  UTF-16LE runs (minimum length in characters, default 4),
  returning each run's byte offset, byte length and a capped
  text preview — windowed, so the file is never loaded whole.
  The strings pane is listed with the view features below.
- Entropy core: Shannon entropy per block over a document
  (block size a parameter, default 4096 bytes), returning one
  value per block in the range 0.0 to 8.0 — the data behind an
  entropy strip, listed with the view features below.
- Pointer-following core: at a byte offset, the stored value
  read as a 32- or 64-bit integer in both byte orders, each
  marked when it lands inside the document (a plausible jump
  target); plus a jump-history stack (push on jump, back /
  forward, capped at 128 entries). The jump surface is listed
  with the view features below.
- Time-delta core: parse the timestamp at the start of a log
  line (ISO 8601 with `T` or space separator, and time-of-day
  only, each with an optional 1–3 digit fraction; invalid dates
  and times are rejected) and compute the millisecond delta
  between consecutive lines. The delta gutter is listed with
  the view features below.
- Delimiter grid core: detect a delimited file's delimiter
  (comma, tab, semicolon, pipe) by consistency scoring over a
  sample of lines, and split a line into column byte ranges —
  with CSV quoting (double-quote escapes) applied to comma
  files only, as documented in the core. The grid mode is
  listed with the view features below.
- Follow Tail (window, Ghost Tail): View ▸ Follow Tail watches
  the open file; while it is on and the document has no edits of
  your own, a file that only grew on disk is adopted silently —
  the new bytes join the document, line indexing resumes where
  it stopped, and your caret, selection, scroll position and
  mode are untouched (a view at the end stays at the end). The
  status bar reads "Following" while the mode is on. A shrink or
  a same-size rewrite still shows the external-change bar, as
  does an edited document — growth is only ever adopted into a
  document with no edit history.
- Filter view (window): Search ▸ Filter… asks for a pattern and
  switches the text view to only the lines that contain it, each
  shown with its original line number under a banner naming the
  pattern and the line count. Arrow keys walk the rows; Enter
  returns to the full file at that line; Esc returns without
  moving. The view is read-only — the status bar says so. On a
  file whose line index is still building, the filter covers
  the indexed prefix and the banner says matches so far are
  partial; it rebuilds itself when the index completes.
- Word Wrap (window): View ▸ Word Wrap folds long lines at the
  window's edge in text mode. Wrapping is display only; clicks,
  caret movement (Up/Down move by display row) and selection all
  follow the wrapped rows.
- Strings pane (window): View ▸ Strings Pane docks a list of the
  readable text runs found in the document along the bottom,
  each with its offset; the scan runs on a worker for large
  files. Double-click a row to jump to the string, and the
  header's min 4 / 6 / 8 chips set the shortest run listed.
- Entropy strip (window): View ▸ Entropy Strip shows a thin
  strip under the toolbar coloured per block from the panel
  colour (low entropy) to the accent (packed or encrypted
  data); clicking the strip seeks the view to that part of the
  file. Computed on a worker for large files.
- Time Deltas (window): View ▸ Time Deltas adds a gutter column
  showing the time between consecutive timestamped log lines
  (+45ms, +1.25s); lines without a timestamp show nothing.
- Freeze Top Lines (window): View ▸ Freeze Top Lines… pins a
  chosen number of top lines (0–50, 0 = off) while the rest
  scrolls. Frozen lines can be viewed, clicked and jumped to,
  but not edited; scrolling up stops at the first unfrozen
  line. Plain text view only.
- Dim Inactive Paragraphs (window): View ▸ Dim Inactive
  Paragraphs paints paragraphs other than the caret's in the
  muted colour, so the block being worked on stands out.
- ANSI Colours and control-character tags (window): View ▸ ANSI
  Colours renders SGR colour codes in the text (standard and
  bright foregrounds, bold brightens) and consumes the escape
  bytes; View ▸ Show Control Characters shows control bytes as
  small tags (^A, ^?). Byte columns in the status bar stay
  byte-true either way — painted widths can differ from byte
  columns when sequences or tags are present.
- Delimiter Grid (window): View ▸ Delimiter Grid aligns the
  visible lines' fields into columns when a consistent
  delimiter is detected in view. Alignment is paint-only: the
  file's bytes never change, caret and status columns stay
  byte-based, and Copy copies the raw text, never padded.
- Pointer following and jump history (window): in hex mode,
  Ctrl+click a byte (or place the caret and press Ctrl+Enter)
  to read it as a 32/64-bit pointer in both byte orders and
  jump to the first reading that lands inside the document;
  the status bar names the value jumped through. Go To jumps,
  filter jumps, bookmark jumps, strings jumps and pointer
  jumps are recorded; Alt+Left / Alt+Right (Search ▸ Back /
  Forward) walk the history. The history clears when a
  document opens.
- Bit editing in the Data Inspector (window): in hex mode the
  inspector gains a Bits row — eight boxes for the byte at the
  caret, bits 7 to 0; clicking a box flips that bit as a
  normal undoable edit, without moving the caret.
- Scrollbar tick previews (window): resting the pointer on the
  scrollbar where a match or bookmark tick sits shows a tooltip
  with the line number and the start of that line.
- Extended code pages: the new Encoding menu re-reads the
  document under UTF-8, UTF-16 LE / BE or ANSI, and its Code
  Pages submenu adds Shift-JIS (932), GBK (936), EUC-KR (949),
  ISO-8859-1 (28591), ISO-8859-15 (28605), Windows-1250 and
  Windows-1251 for files written by older or regional programs.
  The status bar names the page in use (for example CP932);
  typing and saving convert back through the same page.
  Conversion is lossy where a page cannot represent a
  character: a replacement mark on decode, "?" on encode.
- Extract Ranges: File ▸ Extract Ranges… copies chosen byte
  ranges (0-1023, 0x1000-0x1FFF) and line ranges (L10-L20) of
  the document into a new file, in order, separated by ";" —
  also headless as `--slice "<spec>" --out <path>`.
- Delta patches: File ▸ Export Delta Patch… writes a small
  VELOPAT1 patch holding only the current edits against the
  file on disk; File ▸ Apply Patch… applies a patch to the
  current document's file and writes the result to a new file
  (the base is never overwritten). Headless forms:
  `--export-patch <path>` and `--apply-patch <patch> --out
  <path>`.
- Insert Zeroes: Edit ▸ Insert Zeroes… adds a run of zero bytes
  at the caret (a count, or a K / M / G size) as one undo step;
  headless `--insert-zeroes <count> [--at <offset>]` edits a
  file in place through the atomic save.
- Bookmark colours and notes: a bookmark can carry one of six
  colours and a short note (right-click its gutter square, or
  Search ▸ Bookmarks ▸ Edit Bookmark…). The gutter square and
  scrollbar tick draw in the colour, and jumping to a noted
  bookmark shows the note in the status bar. Annotations live
  in the same `data/bookmarks.json`; older store files load
  unchanged.
- Compare With: File ▸ Compare With… compares the document
  with another file block by block (1 MB blocks, SHA-256) on a
  worker and lists the regions that differ, with a Go to that
  jumps the caret; headless `--compare <other>` prints the
  regions and exits 1 when the files differ.
- Keyboard shortcuts can be rebound: `data/keybindings.json`
  maps action names ("find", "save", "toggle_hex", …) to chord
  strings ("Ctrl+Shift+F"); menu labels show the effective
  chords, an empty string unbinds, and a file with mistakes
  falls back to the defaults for the actions concerned with a
  one-line status note at startup. The Built-in Guide lists
  every action and its default.
- Read-only files: a file with the read-only mark opens with
  "(read-only)" in the title and "Read-only file" in the
  status bar; Save explains and offers Save a Copy… instead of
  failing. File ▸ Open as Administrator relaunches the program
  with full rights on the same file, explaining before the
  Windows prompt appears.
- Two new settings-file keys: `map_window_mb` (0 = map the
  whole file, the default; 512 / 1024 / 2048 / 4096 read the
  open-path samples through a moving mapped window) and
  `fast_save_clone` (off by default; when on, an in-place save
  reuses the file's unchanged extents where the drive supports
  it and silently falls back to the normal save when it
  cannot).
- Tabs: one window holds many documents, one tab each in a
  strip under the top edge. Each tab keeps its own caret,
  scroll, text/hex mode, undo history, encoding, bookmarks,
  find and filter state (a search, filter or index build on an
  inactive tab keeps running and lands in its own tab), and
  its own armed Save Options. A filled square marks unsaved
  changes, a hollow square a read-only file; resting on a tab
  shows its full path. Ctrl+T new tab, Ctrl+W close tab,
  Ctrl+Tab / Ctrl+Shift+Tab next / previous, Ctrl+1…8 jump to
  a tab, Ctrl+9 to the last; click switches, middle-click
  closes, dragging reorders, right-click offers Close, Close
  Others, Close All, Copy Path and Open in New Window. File ▸
  Open, the Recent list and dropped files open into a new tab
  when the current tab holds a file or unsaved changes, and
  replace an untouched empty tab otherwise. Closing a dirty
  tab asks Save / Don't Save / Cancel; closing the last tab
  leaves a fresh empty document; closing the window asks
  about every dirty document in turn. File ▸ New Window
  starts a separate VeloEdit window; the open tabs and the
  active one are remembered at a clean exit and reopened at
  the next launch (missing files skipped silently; untitled
  documents are not remembered). The new tab shortcuts are
  ordinary rebindable actions (`new_tab`, `close_tab`,
  `next_tab`, `prev_tab`, `tab_1`…`tab_8`, `tab_last`).
- Split view: View ▸ Split Text/Hex shows the document as text
  on the left and as hex on the right at the same time, tied
  together by the byte at the top of each pane — scroll one
  and the other follows. Clicking a pane (or wheeling over it)
  chooses where the keyboard works; the caret's byte is
  marked in the pane not in use. The divider between the
  panes drags, and its position is remembered. New rebindable
  action `toggle_split` (default Ctrl+\). The filter view and
  frozen lines each decline while split — and split declines
  while they are up — with the reason in the status bar; the
  delimiter grid waits while split.
- Compare side by side: File ▸ Compare Side by Side… opens a
  second file read-only in a pane on the right (its pane
  takes no typing), scrolling in step with the document — by
  line in text mode, by byte row in hex mode. In text mode
  the line differences are worked out in the background and
  shaded in both panes; the status bar counts them, and
  Search ▸ Next Difference / Previous Difference (new
  rebindable actions `diff_next`, default F7, and `diff_prev`,
  default Shift+F7) jump between them. Editing the document
  marks the shading stale until Search ▸ Compare Differences
  recomputes it. Esc, the menu item again, or switching tabs
  closes the compare.
- Regular expressions in the find bar: the new Regex toggle
  searches the whole document with a regular expression —
  Find Next and Previous, the match count, and the background
  find on large files all use it, and matches highlight at
  their true lengths. Replace and Replace All insert the
  replacement literally, and Replace All is still one undo
  step. Regex works on UTF-8 and ANSI files; on UTF-16 files
  the bar says so instead of guessing, and a pattern that does
  not parse shows its error in the bar and searches nothing.
  The headless find gains the same engine via `--find
  <pattern> --regex`.
- Binary templates window: Tools ▸ Apply Template… picks a
  template file from `data/templates/`, a structure in it, and
  an offset (the caret by default), and lists the decoded
  fields — dotted name, offset, size, value — in a docked
  list; double-clicking a field jumps to it and selects its
  bytes. Two example templates (PE and PNG headers) ship in
  `examples/templates/` in the repository.
- External tools: the Tools menu runs the programs declared
  in `data/tools.json` — each tool is fed the selection, the
  caret's line, the file, or nothing, and its output replaces
  the selection, inserts at the caret, is written to a new
  file, or is simply shown. Runs happen in the background
  with a 30-second limit and a 16 MB output cap; Esc stops a
  running tool, failures report the exit code and the tool's
  own error output, and output is shown rather than applied
  if the document changed while the tool ran. Tools ▸ Reload
  Tools re-reads the file.
- Multiple carets and rectangular selection (text mode):
  Alt+click adds a caret (or removes one), Ctrl+Alt+Up and
  Ctrl+Alt+Down (new rebindable actions `caret_up` and
  `caret_down`) add a caret on the line above or below, and
  typing, Backspace, Delete, Enter and Tab apply at every
  caret as a single undo step; the status bar counts the
  carets. Alt+drag selects a rectangle of columns: Copy and
  Cut take its text line by line, Delete and typing replace
  it, again as one undo step.
- Disassembly panel: View ▸ Disassembly (hex mode) lists the
  machine instructions decoded from the caret — offset, bytes,
  instruction — following the caret as it moves; its heading
  names the offset it decoded from, it re-decodes after edits,
  and double-clicking a row moves the caret to that
  instruction.
- Taskbar jump list: right-clicking the taskbar button shows
  the recent files (opening one works like opening it in the
  program) and a New Window entry.
- Screen-reader exposure (scoped): assistive tools querying
  the window are told the file name, the caret's line and
  column, and the current line's text.
- Wide characters line up: on lines containing wide or
  otherwise non-fixed-width glyphs (such as CJK text), the
  caret, selection and match highlighting, and clicking now
  use the line's own measured widths instead of assuming
  every character takes one cell.
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
- Filter view on large files: the filter now opens at once and
  fills in as a background worker scans the document — the
  status bar reads "Filtering… NN%" until the build lands, and
  leaving the filter or opening another document cancels it.
  Documents under 4 MB still build in one step.
- Filter prompt: confirming with an empty pattern now leaves
  the filter view (before, it did nothing); Search ▸ Clear
  Filter does the same and is enabled only while filtering.
- File Hashes dialog: the CRC32 and xxHash64 checksums join
  MD5, SHA-1 and SHA-256 in the same worker pass, and the
  compare box checks pasted text against all five.
- Regular-expression search core (wired into the find bar
  above): a streaming, backtracking-free regex engine —
  classes, alternation, greedy and lazy quantifiers, counted
  repeats, and line anchors — that finds matches in files of
  any size in bounded memory, with no pattern able to make
  the search blow up.
- Binary templates core (wired into the Tools menu above):
  a plain-text struct definition language for describing file
  headers and packets, applied over a file to list every
  field's offset, size, and decoded value, including counted
  arrays whose length comes from an earlier field.
- Line diff core (for the coming side-by-side compare):
  reports the regions where two files' lines differ,
  streaming both sides in bounded memory.
- Disassembly length decoder core (wired into the View menu
  above): exact x86-64 instruction lengths with a
  best-effort text rendering.
- External tools core (wired into the Tools menu above):
  the `data/tools.json` format for running user programs on a
  selection or range and bringing their output back — parsing,
  validation, and `{file}` / `{offset}` substitution.
- Multi-cursor editing core (wired into the window above):
  many range replacements applied as a single undo step, plus
  rectangular (column) extract, delete, and insert over a
  line range, also as single undo steps.
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

### Fixed

- The first rows of the document painted underneath the
  toolbar buttons: the canvas top still used the pre-tab
  chrome height after the tab bar was added, so the toolbar
  covered the top of the text (visible in both the normal and
  split views). The canvas now starts below the full chrome
  stack.
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
