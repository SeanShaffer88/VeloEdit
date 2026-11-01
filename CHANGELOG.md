# Changelog

All notable changes to VeloEdit are recorded here. Versions follow the
release asset naming in the specification once releases begin; no release
has been published yet.

## [0.1.0] — Unreleased (foundation)

The foundation build: the editing core and headless tools, with no
graphical interface yet.

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

### Not yet included

- The graphical editor window (planned after the interface design is
  confirmed), Windows memory-mapped file backing, and atomic save.
