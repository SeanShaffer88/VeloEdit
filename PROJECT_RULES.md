# VeloEdit — Project Rules

Prompt in force: The Elite Software Master Prompt v1.1 · Annexes: Annex A (Windows platform annex), SEAN Annex C1/C2 · Layered under it: Core Engineering Contract V2.0 (wins on conflict)

Purpose in one line: A zero-dependency, portable Windows 11 64-bit binary and large-file editor that opens 100+ GB files in milliseconds by editing a memory-mapped piece table instead of loading file contents into RAM.

Frozen foundation:
- Module layout (one owner per directory/module; interface changes land alone first):
  - `src/piece_table.rs` — mmap + piece table engine (SPEC §2). The original-bytes seam is `OriginalBuffer`; the Windows mmap variant slots in there without API changes. Piece storage is `Vec<PieceNode>` in v0.1; the production target is the weight-balanced 2-3 B-tree of §2.2 behind the same operations.
  - `src/line_index.rs` — sparse line checkpoints every 64 KB + forward-scan line location (SPEC §3.1). Newline counting is isolated in `count_newlines_in` as the single SIMD replacement seam.
  - `src/hexview.rs` — hex row arithmetic and the 64-bit-ratio scrollbar math in `u128` fixed point (SPEC §3.1/§3.2).
  - `src/undo.rs` — `EditAction` inversion deltas + bounded `UndoStack`; undo/redo restore piece layouts and never touch the append buffer (SPEC §5.2).
  - `src/encoding.rs` — text encoding detection / decode / encode for UTF-8, UTF-16 LE/BE, and ANSI (Windows-1252), owner-confirmed v1 feature. The piece table always stores bytes; encodings live only at the edges.
  - `src/mmap_win.rs` — Windows read-only memory-mapped file backing (SPEC §2.1: CreateFileW/CreateFileMappingW/MapViewOfFile, RAII handle ownership, zero-length files take an `Empty` variant). `cfg(windows)` only; slots into `OriginalBuffer` with no piece-table API change.
  - `src/save.rs` — atomic save pipeline (SPEC §5.1): adjacent `*.tmp_velo_<pid>` staging, ≤4 MB streaming writes, flush, close our own mapping, ReplaceFileW swap, remap + table reset. Save is a history boundary (the UndoStack is cleared). A 100%-coverage safety path.
  - `src/search.rs` — streaming find / replace / replace-all over the piece table (owner-confirmed v1 feature), built on `UndoStack::do_segment_replace` so every replacement is exactly one history entry.
  - `src/editor.rs` — the GUI-independent editor state machine: PieceTable + UndoStack + encoding Detection + mode + caret/selection/scroll + find state. Every editing decision lives here so the shell stays thin and the behaviour is headless-testable. After an edit, line positions stay exact via the line index's still-valid prefix plus bounded scans (see the module docs).
  - `src/theme.rs` — the built-in theme library: exactly 18 themes as full token sets (SPEC §4.1 palette is the Obsidian default) plus 8 accent presets; an accent override replaces only the accent-derived tokens.
  - `src/settings.rs` — portable settings (theme, accent override, font size, window geometry, recent files) as hand-rolled JSON in the adjacent `data/` folder; a corrupt file falls back to defaults and is preserved as `settings.json.bad`.
  - `src/gui_win.rs` — the Win32 + Direct2D/DirectWrite shell (SPEC §4), `cfg(windows)` only. A thin translator from messages to `editor` operations. Direct2D/DirectWrite COM interfaces are declared by hand in this module because windows-sys 0.61 ships no bindings for them (its Graphics features stop at Gdi/GdiPlus/Dwm/Hlsl/OpenGL/Printing) and no other crate is allowed.
  - `src/diff.rs` — streaming headless binary diff with hand-rolled JSON report (SPEC §7).
  - `src/cli.rs` — hand-written argument parsing + headless command execution + guarded `data/` cleanup (SPEC §7 plus the V2.0 global flags).
  - `src/lib.rs` — the operation registry surface (`op_*` functions). Everything is an operation in the library first; the GUI shell and the CLI call the same operations. See `docs/PARITY.md`.
  - `installer/VeloEdit.nsi` + `installer/Package.wxs` — the §6 packaging scripts (Slice 5): NSIS per-user Setup exe and WiX v4 per-user MSI, both embedding the same `VeloEdit.exe` + LICENSE payload; release assets are assembled only by `.github/workflows/release.yml`.
  - All planned v1 modules are now present, and the §6 packaging (scripts + release workflow) landed in Slice 5; the release itself still awaits owner confirmation.
- Governing spec: `docs/SPEC.md` — the complete workflow blueprint and engineering specification, §§1–8. Where this sheet and the spec disagree, the spec wins and this sheet is updated in the same change.

Build: `cargo build` · Full gate: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test` (host), plus `cargo check --target x86_64-pc-windows-gnu --all-targets` before any publish · Test corpus: the unit-test modules inside each `src/*.rs` file (grows with every slice; publish verification adds the Windows-target check)

Budgets (SPEC §8 — these gate this project):
- Cold startup time < 50 ms (QueryPerformanceCounter)
- Initial file ingestion, 100 GB < 100 ms (MapViewOfFile timing to first frame)
- Idle baseline working set < 15 MB; idle CPU < 0.1%
- Active editing memory cap < 35 MB under heavy piece-table mutations
- Direct2D render latency < 16 ms per frame (BeginDraw→EndDraw)
- Atomic save integrity: zero data corruption under hard-abort simulation

Coverage floors: 85% core / 70% overall / 100% safety paths (the atomic save path, when it lands, is a safety path and must be fully covered before it ships)

Special rules for this project:
- **Full v1 scope (owner-confirmed 2026-10-31):** in addition to the blueprint §§1–8, v1 includes **search/replace** (find, replace, replace-all) and **text encodings — UTF-8, UTF-16 LE/BE, and ANSI, with BOM detection** — even though the blueprint omits both. The core landed in Slice 2 (see `docs/PARITY.md`); their GUI forms arrive with the editor window.
- **Encoding strictness (Slice 2 decisions, recorded so they are never silently changed):** decoding never substitutes replacement characters — UTF-16 odd trailing bytes and lone surrogates are errors naming the byte position; encoding a character Windows-1252 cannot represent is an error, never a `'?'`. The five undefined Windows-1252 bytes (0x81, 0x8D, 0x8F, 0x90, 0x9D) decode to the matching C1 control characters so decode→encode round-trips all 256 byte values losslessly. Detection order is BOM → empty → NUL-parity (BOM-less UTF-16, checked before UTF-8 acceptance because BOM-less UTF-16 of ASCII text is valid UTF-8 byte-wise) → strict UTF-8 (trailing mid-codepoint cut tolerated) → ANSI, not confident.
- **Search semantics (Slice 2 decisions):** find reports overlapping matches; replace-all consumes non-overlapping matches left to right and is exactly one history entry; case folding is ASCII-only. Byte-level search over UTF-16 can match straddling two characters (an offset not on a code-unit boundary) — surfaces presenting UTF-16 matches must treat odd offsets from the text start as suspect; the caveat is documented in `src/search.rs` and `docs/PARITY.md`, never hidden.
- **windows-sys is the ONLY allowed dependency crate** (SPEC §1.1 "zero-dependency": direct windows-sys FFI, statically linked CRT, no other crates without a dependency-register entry and exit sketch). No serde — JSON is hand-rolled. No argument-parsing crate — parsing is hand-written.
- **Unsafe confinement rule:** the crate root is `#![deny(unsafe_code)]`. `unsafe` is allowed ONLY inside the Windows FFI modules that cannot avoid it — currently `src/mmap_win.rs`, `src/gui_win.rs` and the Windows half of `src/save.rs` (the `ReplaceFileW` / `FlushFileBuffers` calls) — each opting in with a module-level `#[allow(unsafe_code)]` and a `// SAFETY:` comment on every single unsafe block. No other module may contain `unsafe`; adding a new allowed-unsafe module requires updating this rule in the same change.
- **Windows 11 64-bit only.** Release assets follow SPEC §6.1 naming (x64 Setup .exe, x64 Setup .msi via WiX v4, x64 Portable .zip, SHA256SUMS.txt). No x86 or ARM64 assets.
- **Portable storage only:** all configuration and state live in the `data/` folder adjacent to the program file. Nothing is ever written to %APPDATA%, %LOCALAPPDATA%, %PROGRAMDATA%, or the registry. `--clean` / `--uninstall` may only ever remove that adjacent `data/` folder (guarded in `cli::clean_data_dir`), with `--dry-run` support.
- **100% offline, zero telemetry.** The only permitted timing counters are the local §8 benchmark measurements, which never leave the machine.
- **Palette (SPEC §4.1):** background `#0E0E10`, panels/gutter `#16161A`, accent/cursor/selection `#E02424`, primary text `#E6E6E6`, muted metadata `#7A7A85`, boundaries `#2A2A30`. Fonts: Consolas or Cascadia Code via DirectWrite.
- **UI code requires a confirmed wireframe first** (V2.0 design gate: wireframes, component hierarchy schemas, or ASCII/SVG mockups presented and confirmed before any window/Direct2D code is written). No GUI code exists in v0.1 by design.
- Every program rule applies: tooltips for every control, full command-line parity for every capability, autosave / crash recovery, undo or backup before anything destructive (atomic save with backup per §5.1), useful in the first minute, search where there is lots of content, built-in offline help, drag-and-drop for files, reopen where the user left off (inside `data/`), admin rights only with a plain reason first, updates offered never forced, no nagging.
- Releases are never published without explicit confirmation from the project owner first, and fixes are batched into grouped releases.
- **Release process:** pushing a `v*` tag (or manually dispatching the workflow with a tag) triggers `.github/workflows/release.yml`, which runs the full gate, builds the §6 asset matrix (Setup .exe, Setup .msi, Portable .zip, SHA256SUMS.txt) from the tagged commit on a Windows runner, and creates the GitHub Release. Ordinary pushes and merges never release. **Never create or push a release tag without the owner's explicit confirmation** — the tag push IS the release. The WiX UpgradeCode in `installer/Package.wxs` must never change (it is the upgrade identity of every future MSI); each release gets a fresh ProductCode.

Known limitations live at: `README.md` ("Known limitations" section) · Decision log: this file's git history plus `docs/SPEC.md` §"Still unspecified" items tracked in the project goal notes until decided · QUALITY.md: `QUALITY.md`
