# VeloEdit — Quality Scorecard

Version scored: 1.0.0, Slices 1–5 (core library, headless CLI, the GUI
shell, and the packaging scripts + release workflow). The GUI is compile-verified only — it has not
run on a real Windows machine — so dimensions that depend on its
*runtime* behaviour stay **Unverified**, not scored up.
Scores are 0–5 per dimension. Any dimension marked Fail blocks "done".

## Scope note

Foundation slice scope: project rule sheet, this scorecard, the operation
registry (`docs/PARITY.md`), the governing spec (`docs/SPEC.md`), the
piece-table / line-index / hex / undo core, the streaming binary diff, and
the hand-written CLI with the V2.0 global flags.

All owner-confirmed v1 features are now built: search/replace and text
encodings (UTF-8, UTF-16 LE/BE, ANSI, with BOM detection) landed in
Slice 2; Windows mmap backing and the atomic save pipeline in Slice 3;
the Win32 + Direct2D interface, the editor state machine, the 18-theme
library, portable settings, and crash recovery in Slice 4; the §6
packaging (NSIS + WiX scripts and the tag-triggered release workflow)
in Slice 5. What remains unbuilt is any release itself — creating one
is the owner's call — and every Windows-runtime verification.

## Scorecard

| Dimension | Score | Evidence |
| --- | --- | --- |
| Architecture & foundation | 4 | Frozen module layout in `PROJECT_RULES.md`; original-bytes seam (`OriginalBuffer`) now carries the Windows mmap backing with no API change; B-tree target documented where the Vec stands in |
| Correctness (core operations) | 4 | Insert/delete/coalescence, multi-piece reads, undo/redo layout restoration, line checkpoints, hex/scroll math, diff counting, encoding detection/decode/encode, boundary-spanning search, single-undo replace-all, and the atomic save round trips (including both staged failure paths) are all covered by unit tests that exercise the real code paths |
| Tests | 4 | 150 unit tests (55 foundation + 39 Slice 2 + 14 Slice 3 + 42 Slice 4: editor state machine, the 18 themes incl. WCAG contrast, settings incl. corruption handling), host gate fully green (see below). Coverage percentage not yet measured with a coverage tool — the 85% core / 70% overall floors are asserted by test breadth, not yet by a measured number |
| Reliability & data safety | 4 | Undo never touches the append buffer; deletes return their removed pieces; `--clean` is guarded to the adjacent `data/` folder and tested. The atomic save safety path (SPEC §5.1) is implemented and covered by host tests for the success, replace, empty, staging-failure and swap-failure cases, with the table provably unchanged after a failed save; its Windows runtime behaviour (real `ReplaceFileW` under a live mapping) is still Unverified — see Known gaps |
| Performance (SPEC §8 budgets) | Unverified | Host proxies are now measured and recorded in `docs/BENCHMARKS.md` (200 MB diff median 330 ms, find 1073 ms, save-copy 466 ms, `--version` start proxy median 1.07 ms, all Linux host build) — evidence about the core only. Every §8 metric itself (cold start <50 ms, 100 GB open <100 ms, <15 MB idle / <35 MB active, <16 ms frame) is a Windows measurement and remains a target, not a result |
| Security & privacy | 4 | 100% offline, zero telemetry by construction (no network code at all); crate root is `#![deny(unsafe_code)]` with `unsafe` confined to `src/mmap_win.rs`, `src/gui_win.rs` and the Windows half of `src/save.rs`, every block carrying a `// SAFETY:` comment (PROJECT_RULES.md confinement rule); the only dependency crate is `windows-sys`, gated to `cfg(windows)` |
| Accessibility & UI | Unverified | The UI now exists (built to the confirmed wireframes): standard menus with full keyboard access, tooltips on every toolbar/find-bar control, per-monitor DPI scaling, themes whose text/background contrast is unit-tested at WCAG AA ≥ 4.5. Runtime behaviour — rendering, focus, screen-reader surface — is Unverified until a real Windows run |
| Documentation | 4 | README follows the full checklist with honest status, `docs/PARITY.md` registry, `docs/SPEC.md` governing blueprint, this scorecard; screenshots are owed from a real Windows run of the UI |
| Maintainability | 4 | No file mixes responsibilities; clippy runs with `-D warnings`; zero-dependency keeps the audit surface to one crate |

**Foundation-slice gate: Pass for the slice as scoped** (host gate green,
below). This is not a v1 quality verdict: the Unverified dimensions must be
scored from real Windows evidence before any release.

## Gate record

- 2026-10-31, foundation slice, run locally on the development host:
  - `cargo fmt --all` — clean
  - `cargo clippy --all-targets -- -D warnings` — clean, no warnings
  - `cargo test` — **55 passed, 0 failed**
  - `cargo check --target x86_64-pc-windows-gnu --all-targets` — passed
    (windows-sys 0.61.2 type-checks for the Windows target; a linked
    Windows binary still needs the mingw linker, which is not installed
    on this host yet)
  - CLI smoke test on the host build: `--version` exit 0, `--diff` exit 3
    on differing files with correct JSON, exit 0 on identical files,
    GUI request exit 2 with the plain foundation-build message

- 2026-10-31, Slice 2 (encodings + search/replace core), run locally on
  the development host:
  - `cargo fmt --all --check` — clean
  - `cargo clippy --all-targets -- -D warnings` — clean, no warnings
  - `cargo test` — **94 passed, 0 failed** (55 foundation + 39 new:
    encoding detection/decode/encode incl. the all-256-byte ANSI round
    trip and UTF-16 error positions; search across 2- and 3-piece
    boundaries, a needle longer than any piece, overlapping matches;
    replace/replace-all restoring the exact original piece layout in one
    undo — including the case where the replaced range starts right
    after an append-buffer piece that a coalescing insert could extend)
  - `cargo check --target x86_64-pc-windows-gnu --all-targets` — passed.
    The mingw linker (`x86_64-w64-mingw32-gcc`) is not installed on this
    host, so no Windows executable was linked in this slice.
  - CLI smoke test on the host build: `--find TODO` on a UTF-8 file
    printed `{"count":2,"offsets":[0,31]}` with exit 0; a needle with no
    matches printed `{"count":0,"offsets":[]}` with exit 0; a BOM-less
    UTF-16LE file was auto-detected and its needle found at byte offset
    12; 1500 matches printed the first 1000 offsets with
    `"truncated":true`.

- 2026-10-31, Slice 3 (Windows mmap backing + atomic save), run locally
  on the development host (the mingw linker is installed now):
  - `cargo fmt --all --check` — clean
  - `cargo clippy --all-targets -- -D warnings` — clean, no warnings
    (host target and `--target x86_64-pc-windows-gnu` both)
  - `cargo test` — **108 passed, 0 failed** (94 before + 14 new: the
    save pipeline's edited-document round trip, chunked streaming with a
    999-byte chunk cap over a 100 KB piece, replace-over-existing,
    zero-length save, staging-failure and swap-failure cleanup with the
    original provably untouched, `save_to` reset semantics — content
    equal, append buffer empty, single Original piece, history cleared —
    `save_to` failure leaving the table byte- and layout-identical, and
    the `--save-copy` parse/run/refusal cases)
  - `cargo check --target x86_64-pc-windows-gnu --all-targets` — passed
    (the mmap module, the `ReplaceFileW`/`FlushFileBuffers` calls and
    their Windows-only tests type-check; windows-sys 0.61 needs the
    `Win32_Security` feature for `CreateFileW`/`CreateFileMappingW`,
    added in this slice)
  - `cargo build --release --target x86_64-pc-windows-gnu` — linked:
    `VeloEdit.exe`, 329,728 bytes, PE32+ executable (console) x86-64,
    SHA-256 `2bfe30f8562ac2c2d65a2a4e5c3e875aac4fb2544c6f81c50a89bdaed2efe272`
  - CLI smoke test on the host build: `--save-copy` wrote an exact copy
    and printed `{"bytes_written":23,"staging_removed":true}`, exit 0,
    with no staging file left behind
  - **Windows runtime behaviour remains Unverified:** the mapping and
    `ReplaceFileW` code has only been compiled for Windows, never run
    there. CI's Windows job compiles it; actually mapping a file on a
    real Windows 11 machine is still owed.

- 2026-10-31, Slice 4 (editor core + themes/settings + Win32/Direct2D
  GUI shell), run locally on the development host:
  - `cargo fmt --all --check` — clean
  - `cargo clippy --all-targets -- -D warnings` — clean, no warnings
    (host target and `--target x86_64-pc-windows-gnu` both)
  - `cargo test` — **150 passed, 0 failed** (108 before + 42 new:
    theme library — exactly 18 unique themes, Obsidian equals the
    SPEC §4.1 palette, every token round-trips through hex, WCAG text
    contrast ≥ 4.5 for all 18, accent override changes only
    accent-derived tokens; settings — round trips incl. Windows
    backslash paths, corrupt file renamed to `settings.json.bad`,
    validation and recent-list capping; editor — open → type → undo →
    redo → save round trip on real temp files, UTF-8 boundary caret
    steps, selection replace as one undo entry, hex row formatting and
    overwrite, find navigation with wrap-around, UTF-16 aligned matches,
    read-only refusals, dirty transitions, exact line counts/positions
    after mid-document edits without a re-index)
  - `cargo check --target x86_64-pc-windows-gnu --all-targets` — passed
  - `cargo check --target x86_64-pc-windows-msvc --all-targets` — passed
  - `cargo build --release --target x86_64-pc-windows-gnu` — linked:
    `VeloEdit.exe`, 552,448 bytes, PE32+ executable (console) x86-64,
    SHA-256 `226d61c352b825ad4d6d6d6cd1120c9a9121773ed39404a03fede4ebb9d2416f`;
    its imports are only system DLLs plus `d2d1.dll` and `DWrite.dll`
  - **Windows runtime behaviour remains Unverified:** the whole GUI —
    Direct2D/DirectWrite rendering through the hand-declared COM
    interfaces, menus, dialogs, drag-and-drop, DPI changes, the
    recovery prompt — has only been compiled, never run. SEAN's run on
    a real Windows 11 machine is the verdict.

- 2026-10-31, Slice 5 (packaging, release workflow, benchmark
  evidence), run locally on the development host:
  - Version bumped to **1.0.0** (`Cargo.toml` + `Cargo.lock`);
    `VeloEdit --version` prints `VeloEdit 1.0.0` (debug and release
    host builds).
  - `cargo fmt --all --check` — clean
  - `cargo clippy --all-targets -- -D warnings` — clean, no warnings
    (host target and `--target x86_64-pc-windows-gnu` both)
  - `cargo test` — **150 passed, 0 failed** (no code changed in this
    slice; the count is the Slice 4 total, re-run on the 1.0.0 tree)
  - `cargo build --release --target x86_64-pc-windows-gnu` — linked:
    `VeloEdit.exe`, 552,448 bytes, SHA-256
    `48d0847d4af69471da0d2126b150234b06fd8162a6cd8dd21854eeb23517e11b`
  - NSIS script proof: `makensis` (3.09) with the exe-path define
    pointed at the gnu exe produced
    `/tmp/velo-nsis-proof/VeloEdit-v1.0.0-x64-Setup.exe`, 398,798
    bytes, SHA-256
    `7e4185453d8a0e3d68f0a78a344d3cd66821e8d2212fb5d3f1d33a83c8d51e17`,
    a Nullsoft self-extracting archive with the exact §6.1 name. The
    payload is the gnu link-proof exe — script proof only; releases
    embed the MSVC build.
  - Portable zip proof (python `zipfile` staging): 275,252 bytes,
    SHA-256
    `2639f43d07770e0160ab8d9898934d8ec95afcb0692fb04e53dda5d95e3d7bc2`,
    entries `VeloEdit.exe`, `LICENSE`, `data/README-data.txt`.
  - Benchmark evidence recorded in `docs/BENCHMARKS.md`: §8 metrics
    stay **Unverified** (Windows-only measurements); host proxies:
    200 MB diff median 330.4 ms, 200 MB find median 1072.8 ms
    (count 200, correct offsets), 200 MB atomic save-copy median
    465.8 ms with an exact-size copy, `--version` start proxy median
    1.07 ms over 20 runs.
  - **Not done in this slice, by design:** no tag, no release, no MSI
    build (impossible on Linux — `installer/Package.wxs` is authored
    and builds for the first time in the release workflow), and the
    NSIS installer has not been *run* on Windows.

## Known gaps (honest list)

- The WiX MSI has never been built anywhere: WiX cannot run on this
  Linux host, so `installer/Package.wxs` (including its WixUI_Minimal
  reference and per-user scope) is first exercised by the release
  workflow on a Windows runner. The NSIS installer is script-proven
  (it compiles to a correctly named Setup exe) but has never been run
  on Windows either.


- Piece storage is a `Vec`; SPEC §2.2's weight-balanced B-tree is a later
  slice, so point lookups are O(N) in the number of pieces for now.
- The line-index scan is a plain chunked scan; the SIMD inner loop
  (SPEC §3.1) replaces `count_newlines_in` later without API change.
- Undo history cap (1024 entries) is a documented placeholder; the
  blueprint requires memory-constrained history but names no number.
- **External-truncation risk on mapped files (by design, SPEC §2.1):**
  the Windows backing opens the file with share modes that let other
  processes write it. If another process truncates the file while it is
  mapped, reading a page past the new end can fault the process instead
  of returning an error. Closing this fully would mean denying write
  sharing, which the blueprint's share-mode design does not do; the GUI
  slice may add a file-change watcher so a changed file is re-opened
  before it is read.
- Windows runtime behaviour of every kind is Unverified until the program
  runs on a real Windows 11 machine.
- The Direct2D/DirectWrite COM interfaces are declared by hand (fixed
  vtable slots from the SDK headers) because windows-sys 0.61 ships no
  bindings for them and the crate rule allows no other dependency. A
  wrong slot would only show at runtime — first item to check in the
  field test is simply whether the window paints.
- After an undo/redo, the editor's line count falls back to the line
  index's own count until a re-index (the newline delta of an undone
  edit is not tracked); only the scrollbar and page clamping consume
  the count in that window, and positions stay exact.
- The recovery check compares the snapshot with the file on disk by
  content for ordinary files; for a file whose current size equals the
  snapshot's it reads and compares the bytes (bounded by the 16 MB
  snapshot cap). A same-size, same-content file is treated as
  unchanged — which it is, byte for byte.
- No horizontal scrolling or word wrap in the editor yet; long lines
  clip at the window edge (rows are capped at 4096 displayed chars).
