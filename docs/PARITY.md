# VeloEdit — Operation Parity Registry

Every capability is an operation in the core library first (`src/lib.rs`,
the `op_*` functions and the module APIs behind them). The command line and
the future GUI shell call the same operations; nothing may exist only in a
window or only in the CLI. This table is the registry: when an operation
gains a new surface, its row is updated in the same change.

Status key: **Implemented** = working in this build · **Foundation** =
implemented in the core library in this slice, CLI/GUI surface lands later ·
**Planned** = not yet built.

| Operation | Core API | CLI form | GUI form | Status |
| --- | --- | --- | --- | --- |
| Open a file | `PieceTable::from_original` (in-memory) / `PieceTable::new` (length-only) | `VeloEdit <file>` | File ▸ Open (later slice) | Foundation — Windows mmap backing (`CreateFileMappingW`) is a later slice; the `OriginalBuffer` seam is ready for it |
| Insert bytes | `op_insert` (`PieceTable::insert` + `UndoStack::do_insert`) | — (editing is interactive; no headless insert command yet) | Typing / paste in the editor view | Foundation |
| Delete bytes | `op_delete` (`PieceTable::delete` + `UndoStack::do_delete`) | — | Delete / Backspace in the editor view | Foundation |
| Slice / read bytes | `op_read` (`PieceTable::read`, `read_into`, `iter_slices`, `byte_at`) | — | Viewport slice generator (SPEC §3.1) | Foundation |
| Undo | `op_undo` (`UndoStack::apply_undo`) | — | Ctrl+Z | Foundation |
| Redo | `op_redo` (`UndoStack::apply_redo`) | — | Ctrl+Y / Ctrl+Shift+Z | Foundation |
| Save (atomic) | — | — | Ctrl+S | Planned — SPEC §5.1 pipeline (adjacent `*.tmp_velo_[PID]` staging, 4 MB streaming writes, `FlushFileBuffers`, `ReplaceFileW`, remap) is a later slice and a 100%-coverage safety path |
| Binary diff | `op_diff` (`diff::diff_files`, `diff::diff_bytes`) | `VeloEdit --diff A B [--output report.json] [--headless] [--json]` | — (headless-only by design, SPEC §7) | Implemented in this slice |
| Hex offset | `op_hex_offset` (`hexview::row_offset`, `row_count`, `thumb_position`) | `--mode hex` (parsed; view is a later slice) | Hex view (later slice) | Foundation — arithmetic implemented and tested; the view itself is Planned |
| Line locate | `op_line_locate` (`LineIndex::line_start_offset`, `nearest_checkpoint`) | `--line N` (parsed; GUI jump is a later slice) | Go to line (later slice) | Foundation — sparse 64 KB checkpoint index implemented and tested; the SIMD scan replacing `count_newlines_in` is Planned |
| Find | `op_find_all`, `op_find_next` (`search::find_all`, `search::find_next`) | `VeloEdit <file> --find <TEXT> [--encoding <NAME>] [--headless]` — prints `{"count":N,"offsets":[...]}`, exit 0 even with no matches | Ctrl+F (later slice) | **Implemented (core + headless CLI)** — streams over the piece table without materialising the document; matches spanning piece boundaries are found; overlapping occurrences are reported. GUI form Planned |
| Replace | `op_replace_one` (`search::replace_one`) | — (interactive; the operation is reachable headlessly in tests) | Ctrl+H (later slice) | **Implemented (core)** — a single undoable history entry (`EditAction::Replace`) via `UndoStack::do_segment_replace`. GUI form Planned |
| Replace all | `op_replace_all` (`search::replace_all`) | — (interactive; the operation is reachable headlessly in tests) | Ctrl+H (later slice) | **Implemented (core)** — non-overlapping matches, applied as **one undoable history entry**: a single undo restores the exact original piece layout and content. GUI form Planned |
| Detect encoding | `op_detect_encoding` (`encoding::detect`) | Used by `--find` (first 64 KB of the file; `--encoding utf8\|utf16le\|utf16be\|ansi` forces it) | Automatic on open (later slice) | **Implemented (core + headless CLI)** — BOM first (UTF-8, UTF-16 LE, UTF-16 BE), then a NUL-parity heuristic for BOM-less UTF-16, then strict UTF-8 validation (a sample cut mid-codepoint still counts), otherwise ANSI marked not confident. GUI form Planned |
| Decode / encode text | `encoding::decode`, `encoding::encode`, `encoding::encode_needle`, `encoding::bom_bytes` | `--find` encodes its needle with `encode_needle` | Editor view renders decoded text; encoding shown and changeable in the status bar (later slice) | **Implemented (core)** — strict decoding (UTF-16 odd trailing byte / lone surrogate is an error with its byte position, never a silent replacement); ANSI is Windows-1252 with the five undefined bytes (0x81, 0x8D, 0x8F, 0x90, 0x9D) decoded to the matching C1 controls so decode→encode round-trips all 256 bytes losslessly; encoding a character Windows-1252 cannot represent is an error, never a `'?'`. The piece table itself always stores bytes. GUI form Planned |
| Clean portable data | `cli::clean_data_dir` | `VeloEdit --clean` / `--uninstall` [`--dry-run`] | — | Implemented in this slice (guarded to the program-adjacent `data/` folder only) |
| Help / version | `cli::HELP`, `--version` handling | `VeloEdit --help` / `--version` | Help ▸ built-in guide (later slice) | Implemented in this slice (CLI); in-window guide is Planned with the GUI |

## Known caveat: UTF-16 byte-level search alignment

Search runs on encoded bytes. In a UTF-16 document a needle's bytes can
in principle occur straddling two adjacent characters, producing a match
at an offset that is not on a code-unit boundary. Matches at odd offsets
relative to the text start (after the BOM) should be treated as suspect
by any surface presenting them; the GUI is expected to filter to aligned
offsets. This is recorded here rather than hidden (see `src/search.rs`).

## Surfaces that must stay in parity

- Any edit the GUI can perform must be reachable through the library
  operations above, so it can be tested headlessly.
- Global flags (`--json`, `--silent`, `--dry-run`, `--verbose`, `--debug`,
  `--trace`) apply to every headless operation, not only `--diff`.
- Exit codes are part of the CLI contract and are documented in `--help`:
  0 success / identical, 1 error, 2 GUI requested but not in this build,
  3 files differ (diff only).
