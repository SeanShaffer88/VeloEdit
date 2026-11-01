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
the Win32 + Direct2D interface, the editor state machine, the theme
library (20 themes since roadmap slice R1), portable settings, and crash recovery in Slice 4; the §6
packaging (NSIS + WiX scripts and the tag-triggered release workflow)
in Slice 5. What remains unbuilt is any release itself — creating one
is the owner's call — and every Windows-runtime verification.

## Scorecard

| Dimension | Score | Evidence |
| --- | --- | --- |
| Architecture & foundation | 4 | Frozen module layout in `PROJECT_RULES.md`; original-bytes seam (`OriginalBuffer`) now carries the Windows mmap backing with no API change; the piece Vec carries a Fenwick weight index for O(log N) lookups (R2a) in place of the sketched B-tree, with the trade-off recorded in Known gaps |
| Correctness (core operations) | 4 | Insert/delete/coalescence, multi-piece reads, undo/redo layout restoration, line checkpoints, hex/scroll math, diff counting, encoding detection/decode/encode, boundary-spanning search, single-undo replace-all, and the atomic save round trips (including both staged failure paths) are all covered by unit tests that exercise the real code paths |
| Tests | 4 | 190 unit tests (55 foundation + 39 Slice 2 + 14 Slice 3 + 42 Slice 4 + 15 roadmap R1 + 16 roadmap R2a + 9 roadmap R2b: Fenwick piece-index equivalence against a naive model over random edit scripts, 100k-piece lookups, coalescing, SWAR-vs-scalar and parallel-vs-serial line-index equivalence, SWAR search equivalence, scoped find/replace, masked search, line sort/dedup, scoped find navigation, masked find flow, the file splitter, CLI size parsing and split runs), host gate fully green (see below). Coverage percentage not yet measured with a coverage tool — the 85% core / 70% overall floors are asserted by test breadth, not yet by a measured number |
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

- Slice 6 — the v1.0.0 release (2026-10-31, owner-confirmed):
  - Tag `v1.0.0` on the release commit of `main`; release published as
    Latest (not draft, not pre-release) with the four §6.1 assets:
    Setup exe (315,283 B), MSI (536,576 B), Portable zip (276,929 B),
    SHA256SUMS.txt (294 B).
  - The release workflow needed five fix iterations, each diagnosed
    from its own run log: (1) four Windows-host tests failed at their
    cleanup lines because a live memory mapping blocks folder deletion
    on Windows (fixed in the tests; the product code was exercised
    successfully by the same run — 149 other checks passed there,
    including mapping reads and the atomic save on a real Windows
    host); (2) `makensis` was not on the workflow process's PATH after
    the Chocolatey install (now resolved by absolute path); (3) the
    WiX UI extension was fetched unpinned at v7 for a v4 tool
    (WIX0144; both now pinned to 4.0.6); (4) the folder-only component
    needed an explicit GUID (WIX0230); (5) payload sources are now
    anchored with `sys.SOURCEFILEDIR` (WIX0103).
  - Every asset was verified three ways after the successful run:
    the hash in the shipped `SHA256SUMS.txt`, GitHub's own asset
    digest, and an independent re-download hashed locally — all three
    agree for all three assets. The portable zip was opened: exactly
    `VeloEdit.exe` (563,712 B), `LICENSE`, and `data/README-data.txt`;
    the shipped exe imports only Windows system DLLs plus
    `d2d1.dll`/`DWrite.dll` (static CRT confirmed) and carries the
    1.0.0 version strings. The MSI's OLE compound-file signature was
    checked; the Setup exe carries the Nullsoft v3.13 manifest.
  - CI now runs the full test suite in its Windows job as well, so
    Windows-host test behaviour is gated on every push, not only at
    release time.

- Post-release addition — program icon (2026-10-31):
  - The icon artwork was supplied by the owner (a stylized V on a
    dark rounded square, 1024x1024 JPEG, kept at
    `assets/icon-source.jpg`). `assets/veloedit.ico` was generated
    from it with the nine standard sizes (16 to 256 px), and a root
    `build.rs` embeds it into the executable by compiling
    `assets/veloedit.rc` (icon id 1): `rc.exe` on MSVC targets,
    MinGW windres on GNU targets, a no-op everywhere else. The same
    icon is wired into the NSIS installer/uninstaller and the WiX
    MSI (Add/Remove Programs entry and Start Menu shortcut).
  - Evidence here: the GNU link path is proven locally. The windres
    build's executable grew from 552,448 to 659,968 bytes, and its
    .rsrc section lists RT_ICON with nine entries (one per size)
    plus RT_GROUP_ICON at id 1. The updated NSIS script compiles
    with the icon defines (Setup exe 590,787 bytes, up from 398,798
    for the icon-less proof build). The MSVC resource compile
    (rc.exe) is first exercised by CI, and how the icon looks on a
    real Windows desktop is Unverified until the owner's machine
    shows it.

- v1.0.1 fix record (2026-10-31) — the first field run of v1.0.0
  failed at startup on the owner's machine:
  - Root cause: one byte of the DirectWrite factory IID was wrong
    (data4 second byte 0xEE instead of the SDK's 0xE8), so
    DWriteCreateFactory returned E_NOINTERFACE (0x80004002) while
    the Direct2D factory (whose IID was correct) returned S_OK.
    The startup code swallowed the HRESULT; it now reports both
    factory codes in the error message and tries both factory
    types before giving up. Regression tests pin both IIDs byte
    for byte and the corrected vtable slots (they run in the
    Windows CI test job).
  - A full audit of the hand-declared vtable slots against the
    SDK-generated layouts found six more wrong: ID2D1RenderTarget
    Clear 46→47, BeginDraw 47→48, EndDraw 48→49, SetDpi 50→51,
    ID2D1HwndRenderTarget Resize 57→58, IDWriteTextLayout
    GetMetrics 45→60. They would have broken painting even after
    the IID fix.
  - Field verification (owner's machine, 2026-10-31): the fixed
    build opens, paints the Obsidian theme, shows menus, toolbar,
    and status bar, and redraws correctly when maximized. Deeper
    interaction (typing, opening a file's contents, find bar,
    theme switching) is still being exercised by the owner.

- Design-fidelity slice (v1.0.1, 2026-10-31) — the GUI was brought
  in line with the design mockup:
  - Matched: a dark title bar through the Desktop Window Manager
    (immersive dark mode, caption and caption-text colours from
    the theme, reapplied on every theme change) with the document
    file name in the title; a 2 DIP accent strip at the top of the
    client area, with all client layout shifted below it; a Search
    menu (Find / Replace / Find Next / Find Previous, with F3 and
    Shift+F3 wired as accelerators); flat owner-drawn buttons with
    a 1 px theme-line border on the toolbar and find bar, the old
    single mode button replaced by Text / Hex toggles anchored to
    the right of the toolbar with the active mode drawn in accent
    text and border; "Find:" / "Replace:" labels and borderless
    themed edit fields on the find bar; a custom-drawn scrollbar
    (10 DIP panel-coloured track, accent thumb with a 24 DIP
    minimum, thumb dragging and track paging — the OS scrollbar
    and its SetScrollInfo sync are gone; the geometry helpers are
    unit-tested); a two-part status bar (caret position and
    selection size on the left; encoding, line-ending style and
    human size on the right — the EOL style is detected from the
    first 64 KB at open, and sizes are exact under 1 KB with one
    decimal above); all find matches washed with the match colour
    and the current match outlined in the accent colour
    (DrawRectangle, vtable slot 16); the current line's number
    and the caret row's hex address in the accent colour.
  - Deliberate differences from the mockup: the window frame, the
    menu bar and the caption buttons remain the operating
    system's (the mockup draws them as custom chrome), so the
    accent strip sits at the top of the client area, below the
    real menu bar; and the find fields keep the editor-background
    brush instead of the panel brush so a borderless field stays
    visible against the find bar.
  - Runtime appearance is Unverified until a field run on a real
    Windows machine shows it, like the rest of the GUI before its
    first field verification.

- 2026-11-01, roadmap slice R1 (quick wins: items 5, 45, 46, 6, 21,
  44, 35, 26/27), built locally on the development host; NOT
  published — v1.0.1 remains held by the owner:
  - (#5) Disk-space pre-check: `save::space_needed` (document length
    + 5%) is checked before the streaming phase of both the editor
    save (`PieceTable::save_to_with_options`) and `--save-copy`
    (`save::save_atomic`); on Windows the target directory is
    queried with `GetDiskFreeSpaceExW` and a short disk refuses the
    save before anything is written, with the needed/free figures
    in the message. A failed query fails open (the save proceeds as
    before). Helper math is unit-tested; the Windows query path is
    compile-verified only.
  - (#45) Standard exit codes everywhere: 0 success (find: ≥1
    match; diff: identical), 1 find-nothing / diff-differ, 2 usage
    or I/O error. Every `run()` and sub-runner return path was
    audited (parse errors, clean, diff, find, save-copy, extract,
    and the no-GUI-build fallthrough, which stays 2). HELP, README
    and PARITY state the same scheme. Host smoke test on the built
    binary: find match 0 / no-match 1, diff identical 0 / differ 1,
    bad flag 2.
  - (#46) `attach_parent_console` now also sets the console input
    and output code pages to UTF-8 (65001) after attaching; results
    ignored, SAFETY comment covers all three calls.
  - (#6) Configurable undo: settings keys `undo_limit` (default
    1024, clamped 16..=65536) and `undo_byte_budget_mb` (default
    256, clamped 1..=4096), parsed/serialised with the other keys.
    `UndoStack::with_limits` tracks the summed payload bytes of the
    stored deltas (kept incrementally) and trims the oldest entries
    when either bound is exceeded; the newest entry is never
    trimmed. `Editor::set_undo_limits` applies them; the shell
    passes the settings values for the startup editor and every
    opened document. Unit tests cover oldest-first trimming, the
    never-empty guarantee, and the editor-level wiring.
  - (#21) Column ruler: settings key `ruler_column` (default 0 =
    off); View ▸ Column Ruler cycles Off → 80 → 120 → Off (check
    mark when on, persisted). The paint pass draws a 1-DIP line in
    the theme line colour at gutter + text padding + column ×
    character width — the same x-grid as the caret — in text mode
    only, when it falls inside the editor area. Compile-verified;
    runtime Unverified.
  - (#44) `extract` subcommand: `VeloEdit extract <file> --start N
    --length M --out DEST`; the input comes from the normal file
    argument, numbers parse via the tested `cli::parse_u64`
    (decimal or 0x-hex), the range streams in 4 MB chunks through
    `save::write_table_range` (clamped; a start beyond EOF exits 2
    before any output file is created). HELP + README documented;
    run-level tests carve, clamp and refuse.
  - (#35) File ▸ Save Selection to File…: enabled only when a
    selection exists (the WM_INITMENUPOPUP grey pattern), shows the
    existing save dialog, and streams the selection through
    `Editor::save_selection_to` → `save::write_range_to_file`
    (range validated before the file is created). Round-trip tested
    at the save and editor levels; the dialog wiring is
    compile-verified only.
  - (#26/27) Themes 19/20: "High Contrast" (pure black background
    and panel, white text, yellow accent, grey muted/line) and
    "E-ink" (paper #F5F1E8, near-black text, restrained dark blue
    accent), appended in the existing `build(...)` pattern; both
    pass the WCAG AA text-on-background test (21:1 and ~15:1). The
    theme-count tests now pin 20. Accent presets gain the
    colour-blind-safe Okabe Blue #0072B2, Okabe Orange #E69F00 and
    Okabe Sky #56B4E9 (11 total); the Custom… menu id is now derived
    from the preset count instead of a hard-coded offset. README,
    PARITY, the in-app guide and the menu comments say 20 themes.
  - Gate, run locally on the development host (true exit codes):
    - `cargo fmt --all` + `cargo fmt --all --check` — clean (exit 0)
    - `cargo test` — **165 passed, 0 failed** (exit 0; 150 → 165,
      +15 new tests across settings, undo, save, editor, theme, cli)
    - `cargo clippy --all-targets -- -D warnings` (host) — clean
      (exit 0)
    - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
      -D warnings` — clean (exit 0; one Windows-only lint,
      `io_other_error`, was found by this run and fixed)
    - `cargo check --target x86_64-pc-windows-msvc` — passed
      (exit 0)
    - `cargo build --release --target x86_64-pc-windows-gnu` —
      passed (exit 0); `VeloEdit.exe` 680,960 bytes, SHA-256
      `cea13ca513a0c233adcfea24eb2344ad60294dfd0de1a6a4eeb473071bd082b6`,
      PE subsystem 2 (Windows GUI); the new feature strings (theme
      names, menu labels, the disk-space message) are present in the
      binary.
  - Honest status: every GUI-facing item (ruler paint, the two new
    menu items and their enable/check behaviour, the undo-limit
    wiring in the shell, the UTF-8 console switch, the Windows
    disk-space query) is compile-verified only. Runtime behaviour
    is Unverified until the owner's Windows machine runs it, as with
    every GUI slice before its field run.

- 2026-11-01, roadmap slice R2a (engine core: items 2, 3, 8, 11,
  12, 23 — core halves only; GUI/CLI wiring lands in a later
  slice), built locally on the development host; NOT published:
  - (#2) Fenwick piece index: `PieceTable` keeps a Fenwick (binary
    indexed) tree over piece lengths in sync on every mutation —
    offset→piece and piece→offset are O(log N)
    (`piece_at_offset` / `offset_of_piece`), and `read_into`,
    `insert` and `delete` locate through it. This is the O(log N)
    structure SPEC §2.2 asks for in place of the sketched
    weight-balanced B-tree: same lookup bound, far less machinery,
    and the piece list itself (which the undo engine splices by
    index) is untouched. Coalescing: `coalesce()` merges neighbours
    over contiguous bytes of one store; it runs automatically past
    8,192 pieces, throttled to one pass per 1,024 further pieces so
    insert runs stay amortised. One hazard was designed for, not
    discovered by failure: an automatic coalesce inside an
    undo-recorded edit would invalidate the entry's segment indices,
    so `UndoStack` snapshots the layout whenever an edit *could*
    cross the threshold and falls back to a whole-layout entry when
    the pass actually fires (generation-counter check). Equivalence
    evidence: a randomised edit-script test compares content and
    lookups against a naive model for 600 mixed edits, a 100k-piece
    table answers boundary lookups exactly, and coalescing reduces
    a fragmented table to the expected count with identical bytes.
    No existing test needed editing: the only count-sensitive
    assertions cover tables far below the auto-coalesce threshold.
  - (#3) Word-parallel + parallel line indexing: newline counting
    is SWAR (eight bytes per step; an exact per-byte zero test that
    cannot over-count through borrow chains — the adversarial
    patterns are in the tests), and `LineIndex::build` reads blocks
    in bounded rounds (≤ 8 × 64 KB live) and counts them on up to
    `available_parallelism` threads (cap 8, `std::thread::scope`),
    assembled in order. A serial reference implementation in the
    tests proves the checkpoint lists identical on randomised
    buffers including block-straddling newlines, `\r\n` data, empty
    input and exact-interval inputs.
  - (#8) Search candidate filtering is SWAR over the needle's first
    byte (both cases when case-insensitive), with full verification
    of candidates only; a naive-matcher equivalence test over
    randomised dense-match data (both case modes, five needles,
    fragmented tables) proves identical results.
  - (#11) Scoped search core: `find_in_range` / `count_in_range`
    (matches fully inside `[start, end)` only — straddlers
    excluded, tested at both edges) and `replace_all_in_range` in
    `search` plus `Editor::replace_all_in_range`, one undo entry,
    outside text untouched (layout-exact undo tested).
  - (#12) Masked search core: `MaskedPattern` parsed from
    `"4A ?? FF 00 2B"`-style text with per-nibble masks (`4?`,
    `?F`); invalid tokens are plain-words errors.
    `find_masked_all` streams like plain search (SWAR anchor on the
    first fully-fixed byte when one exists) and finds matches
    spanning pieces; parse/match/nibble/boundary cases tested.
  - (#23) Sort / dedup core: `Editor::sort_lines_in_range` /
    `dedup_lines_in_range` expand the range to whole lines (line 0
    starts after any BOM, so the mark is never sorted away), sort
    byte-wise and stable with each line's terminator attached (CRLF
    stays CRLF — tested), dedup by content keeping first
    occurrences, apply as one undo entry via the existing replace
    machinery, and refuse an expanded range over 64 MB
    (`EditorError::RangeTooLarge`) before changing anything —
    the refusal is tested on a real >64 MB document.
  - Scope honesty: SWAR is word-parallel portable bit tricks, not
    AVX2/AVX-512 intrinsics — deliberately: intrinsics would need
    `unsafe` and runtime CPU dispatch in a crate that confines
    `unsafe` to three Windows-facing files, for a gain the
    equivalence-tested SWAR path already mostly captures. The
    LengthOnly backing behaviour is unchanged (it has no bytes to
    scan either way). No GUI or CLI surface changed in this slice.
  - Gate, run locally on the development host (true exit codes):
    - `cargo fmt --all` + `cargo fmt --all --check` — clean (exit 0)
    - `cargo test` — **181 passed, 0 failed** (exit 0; 165 → 181,
      +16 new tests across piece_table, line_index, search, editor)
    - `cargo clippy --all-targets -- -D warnings` (host) — clean
      (exit 0; two lints from the new code were found and fixed)
    - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
      -D warnings` — clean (exit 0)
    - `cargo check --target x86_64-pc-windows-msvc` — passed
      (exit 0)
    - `cargo build --release --target x86_64-pc-windows-gnu` —
      passed (exit 0); `VeloEdit.exe` 708,608 bytes, SHA-256
      `27543b1b2c168cf03bea424bdcff90e275e1f2c5c723e2abdd5ce69b2d96e9e9`

- 2026-11-01, roadmap slice R2b (integration: items 11, 12, 23
  GUI surfaces; 36 splitter; 38 Explorer menu; 50 benchmark
  harness), built locally on the development host; NOT published:
  - (#11 GUI) The find bar gains an "In selection" toggle — an
    owner-drawn button in the find bar's themed style, its state
    shown by a ☐/☑ glyph in its label. Two editor operations back
    it: `Editor::set_find_in_range` (scoped match list with the
    same first-at-or-after-caret selection and UTF-16 alignment
    filter as a plain find) and the R2a
    `Editor::replace_all_in_range`. The shell captures the
    selection as the scope (a selection that is merely the current
    match does not shrink it), Find Next / Previous and the count
    stay inside the scope, and Replace All replaces only there,
    still one undo entry. Ticked with no selection and no captured
    scope, the count label shows "Select text first" — the same
    notice surface as "No matches". Editor-level tests cover
    scoped navigation wrapping inside the range.
  - (#12 GUI) Hex-mode masked find: when the find text parses as a
    masked pattern AND contains a space or `?`, Find Next / Count
    run `Editor::set_find_masked` (new); its match list drives the
    ordinary caret/selection/highlight flow (the editor stores a
    stand-in needle of the pattern's length, which is all that
    maths uses). Anything else keeps the previous hex find
    behaviour byte for byte. Replace and Replace All refuse a
    masked pattern with a plain information box — a wildcard has
    no single replacement. One flow note: after an edit clears the
    match list, Find Next does not silently re-run a masked find
    as text (the editor deliberately stores no needle text for
    it); pressing Find again re-runs the pattern.
  - (#23 GUI) Edit ▸ Sort Lines in Selection and Edit ▸ Remove
    Duplicate Lines in Selection call the R2a editor ops on the
    selection range, greyed unless a selection exists (and the
    document is writable) in the existing WM_INITMENUPOPUP
    pattern. A `RangeTooLarge` refusal surfaces through the same
    message-box error path as a failed replace.
  - (#36) Splitter: `save::split_file` streams the source in
    4 MB chunks into `<name>.part001`, `.part002`, … (3-digit
    minimum, width growing with the part count so names sort in
    order), refusing a part size of 0 and a part count over
    10,000 before writing anything; an empty source yields one
    empty part. Tests prove the parts join back byte-identical,
    the exact-multiple and refusal cases included. CLI:
    `--split <FILE> --part-size <SIZE> [--out-dir <DIR>]`, SIZE
    via the new tested `cli::parse_size` (plain/hex numbers plus
    K/M/G suffixes, overflow refused), exit 0/2 per the R1
    scheme; a host smoke run split 250 random bytes into 3 parts
    that rejoin identical, with the error cases at exit 2. GUI:
    File ▸ Split File Copy… opens a small modal dialog (built
    like the Help window — this codebase has no separate Go To
    dialog; the brief's "Go To Line dialog pattern" was read as
    that hand-built small-window pattern) asking for the part
    size in MB (default 4096); it then writes the document to a
    copy in a `<name>-parts` folder via the atomic pipeline,
    splits the copy, deletes it, and reports the part count in a
    message box. (Design choice, recorded: the dialog's buttons
    are owner-drawn and painted by the dialog's own proc with the
    active theme, since owner-draw messages go to the parent
    window, not the main window.)
  - (#38) Explorer menu: `gui_win::shell_menu_install` /
    `shell_menu_uninstall` / `shell_menu_is_installed` write only
    `HKCU\Software\Classes\*\shell\VeloEdit` (MUIVerb "Open in
    VeloEdit", Icon = the exe path, `command` subkey default
    `"<exe>" "%1"`), via the registry APIs — the
    `Win32_System_Registry` windows-sys feature was added for
    this. CLI `--install-shell` / `--uninstall-shell` exit 0/2
    (2 on non-Windows builds, with a plain message — smoke-tested
    on the host). `--clean` now also removes the item on a real
    run (never on `--dry-run`). File ▸ Explorer Menu Item is a
    checkable item reflecting the live registry state. The pure
    helpers (key path, command string incl. a spaced path) are
    unit-tested in the Windows test module. The old "never writes
    to the registry" wording in `--help`, the README, and the
    portable data note was corrected to name this one opt-in
    exception.
  - (#50) Benchmark harness: `scripts/Run-Benchmarks.ps1`
    creates a sparse fixture with `fsutil` at `-SizeGB` (default
    1), plants TODO markers, and times cold launch to `--help`,
    `--find`, `--save-copy`, and `extract` with stopwatches,
    reporting each process's peak working set as a plain table;
    its header warns that save-copy writes a real full-size copy.
    `.github/workflows/benchmarks.yml` is `workflow_dispatch`
    only: same gate + MSVC release build steps as the release
    workflow, runs the script at 1 GB (input, default "1"),
    uploads the table as an artifact. `docs/BENCHMARKS.md`
    describes it and keeps 10/100 GB as local runs.
  - Gate, run locally on the development host (true exit codes):
    - `cargo fmt --all` + `cargo fmt --all --check` — clean (exit 0)
    - `cargo test` — **190 passed, 0 failed** (exit 0; 181 → 190,
      +9 new: editor scoped navigation + masked flow, save split
      names/round-trip/edges/refusals, CLI size parsing, split and
      shell flag parsing, split run; the two Windows-only
      `gui_win` tests — shell-menu path/command — compile under
      the Windows targets and run in the Windows CI test job)
    - `cargo clippy --all-targets -- -D warnings` (host) — clean
      (exit 0)
    - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
      -D warnings` — clean (exit 0; one fix during the slice:
      `EnableWindow` lives under KeyboardAndMouse in
      windows-sys 0.61, not WindowsAndMessaging)
    - `cargo check --target x86_64-pc-windows-msvc` — passed
      (exit 0)
    - `cargo build --release --target x86_64-pc-windows-gnu` —
      passed (exit 0); `VeloEdit.exe` 758,784 bytes, SHA-256
      `d9ee8c0ee8c3a86e9e573bee02b3eaa0e28cc6fcac01729133b60f3281b8a5f1`
    - The workflow YAML parses (python `yaml`); the PowerShell
      script could NOT be parse-checked here — `pwsh` is not
      installed on this VM — so its first real parse is the
      manual workflow run or a local Windows run.
  - Honest status: every GUI item in this slice (the scope
    toggle's look and behaviour, masked navigation/highlighting
    in the window, the sort/dedup menu items and their greying,
    the split dialog and its modal loop, the Explorer menu item's
    check state) and every registry operation is compile-verified
    only — the registry code has never run, so install/uninstall
    behaviour on a real machine is Unverified, as is
    `RegDeleteTreeW`'s not-found code path (2) that the
    uninstall treats as "nothing installed". The benchmark
    script and workflow are likewise unrun anywhere. The core and
    CLI halves (splitter, size parsing, scoped/masked editor
    operations) are unit-tested on the host.

## Known gaps (honest list)

- The installers have been *built* by the release workflow and their
  outputs hash-verified, but neither has been *run* on a real Windows
  machine: the NSIS install/uninstall flow and the MSI install are
  still Unverified until the owner's machine proves them.


- Piece storage is a `Vec` with a Fenwick weight index over it
  (roadmap R2a): lookups are O(log N); structural edits rebuild the
  index in O(N), the same order as the `Vec` splice they accompany.
  SPEC §2.2's weight-balanced B-tree was not built — the Fenwick
  tree provides the same lookup bound with the piece list (and the
  undo engine's index-based segments) unchanged.
- The line-index scan is word-parallel SWAR and the build counts
  blocks on multiple threads (roadmap R2a); SPEC §3.1's AVX2/AVX-512
  intrinsics were deliberately not used (they would need `unsafe`
  and CPU dispatch outside the three sanctioned files).
- Undo history bounds are owner settings since roadmap slice R1
  (`undo_limit` steps, default 1024; `undo_byte_budget_mb`, default
  256 MB of stored deltas). The defaults are the shipped choice —
  the blueprint requires memory-constrained history but names no
  numbers, and no field evidence has asked for different ones yet.
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
