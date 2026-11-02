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
| Tests | 4 | 416 host unit tests (55 foundation + 39 Slice 2 + 14 Slice 3 + 42 Slice 4 + 15 roadmap R1 + 16 roadmap R2a + 9 roadmap R2b + 37 roadmap R3a: Fenwick piece-index equivalence against a naive model over random edit scripts, 100k-piece lookups, coalescing, SWAR-vs-scalar and parallel-vs-serial line-index equivalence, SWAR search equivalence, scoped find/replace, masked search, line sort/dedup, scoped find navigation, masked find flow, the file splitter, CLI size parsing and split runs, hash known-answer vectors, inspector readings and civil-date conversion, bookmark store round trips and staleness, delta-recovery rebuilds and refusals, save EOL/BOM transforms, windowed find + 15 roadmap R3b-2: GUID, FILETIME and text-preview inspector readings, width-parameterised hex row maths, grouped and big-endian hex formatting and click hit-testing, fit-to-window widths, settings validation for the new hex/save keys, editor hex movement and formatting at row widths 8 and 32, save-with-options transforms + 7 instant-open: the indexing percent/suffix helper, staged-build prefix and completion identity against the one-shot build, small-file head-build completeness, forward-resolver agreement within and beyond the indexed prefix, staged-open exactness with a head-only frontier, staged completion attach with a monotonic frontier, and edit-during-staged-build discard ending exact + 57 roadmap R4a: zero-run reads proven on every read path with exact undo (including a 5,000,000-byte run stored as one piece and a 1 GB run as one piece), Ghost Tail growth extension and refusal rules with builder-resume identity from a mid-grid frontier, CRC32 and xxHash64 known-answer vectors (reference-checked against a reference implementation), filter line mapping on complete and partial indexes, the slicing grammar with its reversed / out-of-range / overlap / order refusals, VELOPAT1 patch round trips (insert-at-start, delete-at-end, no-change, duplicated original bytes) and corruption refusals, strings runs across the 64 KiB window edge in both encodings, entropy known values, pointer readings and the 128-entry jump-history cap, timestamp validation and consecutive-pair deltas, delimiter splitting, quoting and detection + 3 roadmap R4b (the window paths behind the view wiring): growth adoption through a fresh backing — the shell's re-map route — with the piece table's length refusal surfaced, the caret-preserving in-place byte replace behind the inspector's bit row, and the editor-level filter view with its index-snapshot discipline + 38 roadmap R4c: the ISO-8859-1 all-256-byte round trip and the ISO-8859-15 specials table, code-page re-decode/re-encode through the editor codec, the keybinding parser/merger (defaults, per-action fallback on unknown actions and bad chords, unbind, duplicate-chord problems), structural-diff block regions with coalescing and first divergence, mapping-window maths (alignment, clamping, forward-walk coverage), the incremental filter builder proven equal to the one-shot build at five window sizes with honest mid-build snapshots, bookmark annotation storage (old-format files load with defaults and re-serialise without annotation keys; annotations round-trip and validate), the read-only attribute helpers, the fast-clone fallback save (memory-backed tables fall back and save byte-exact), the R4c settings keys' round trip and validation, and the CLI runs for --slice (byte and line ranges), the patch export/apply round trip, --insert-zeroes in place, and --compare region reporting + 50 roadmap R5a (regex equivalence against an independent backtracking reference over generated cases, template parsing/apply with truncation, linediff alignment, disassembler known lengths and truncation, the external-tools protocol, multi-range and rectangular editing as single undo entries) + 19 roadmap R5b (tab order open/close/reorder/active-index invariants across a full close-out, strip width fitting, the session list mapping with its active-index rule, restore planning against an existence probe, session normalisation, the settings session round trip with duplicates kept, and the tab actions' default chords) — plus 2 ignored 1 GB bench tests for staged open versus the one-shot build), host gate fully green (see below); the Windows-only GUI suite adds layout/formatting/menu/theme-preview tests plus R3b-1's scrollbar-tick bucketing, R3b-2's inspector-rows, hash-verdict and armed-save-suffix tests, and the polish pass's zoom-percentage and colour-blend tests, plus R4b's wrap-range, ANSI/control-tag segment, segment cell-mapping and log-delta-format tests, and runs in CI. Coverage percentage not yet measured with a coverage tool — the 85% core / 70% overall floors are asserted by test breadth, not yet by a measured number |
| Reliability & data safety | 4 | Undo never touches the append buffer; deletes return their removed pieces; `--clean` is guarded to the adjacent `data/` folder and tested. The atomic save safety path (SPEC §5.1) is implemented and covered by host tests for the success, replace, empty, staging-failure and swap-failure cases, with the table provably unchanged after a failed save; its Windows runtime behaviour (real `ReplaceFileW` under a live mapping) is still Unverified — see Known gaps |
| Performance (SPEC §8 budgets) | Unverified | Host proxies are now measured and recorded in `docs/BENCHMARKS.md` (200 MB diff median 330 ms, find 1073 ms, save-copy 466 ms, `--version` start proxy median 1.07 ms, all Linux host build; the instant-open slice adds the 1 GB staged-open proxies: one-shot index build 3,112 ms against a 4.89 ms head prefix, editor first line question 32.81 ms) — evidence about the core only. Every §8 metric itself (cold start <50 ms, 100 GB open <100 ms, <15 MB idle / <35 MB active, <16 ms frame) is a Windows measurement and remains a target, not a result |
| Security & privacy | 4 | 100% offline, zero telemetry by construction (no network code at all); crate root is `#![deny(unsafe_code)]` with `unsafe` confined to `src/mmap_win.rs`, `src/gui_win.rs` and the Windows half of `src/save.rs`, every block carrying a `// SAFETY:` comment (PROJECT_RULES.md confinement rule); the only dependency crate is `windows-sys`, gated to `cfg(windows)` |
| Accessibility & UI | Unverified | The UI now exists (built to the confirmed wireframes): standard menus with full keyboard access, tooltips on every toolbar/find-bar control, per-monitor DPI scaling, themes whose text/background contrast is unit-tested at WCAG AA ≥ 4.5. The polish pass adds a visible keyboard-focus frame on every owner-drawn button, toolbar glyph icons, a segmented Text / Hex control, and a status-bar zoom readout. Runtime behaviour — rendering, focus, screen-reader surface — is Unverified until a real Windows run |
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

- 2026-11-01, micro-slice (owner-requested window features:
  Ctrl+wheel font zoom; live theme/accent menu preview), built
  locally on the development host; NOT published:
  - Ctrl+wheel zoom: `on_wheel` checks MK_CONTROL (0x0008) in
    the message's low word first. With Ctrl held it does not
    scroll; deltas feed a dedicated accumulator (`wheel_zoom_rem`,
    kept separate from the scroll remainder `wheel_rem` so partial
    notches never mix) and each full 120 units steps the font by
    calling the Font Bigger / Font Smaller command path itself —
    same ±2 step, same 8–48 clamp, same settings write. Plain
    wheel handling below the branch is untouched.
  - Live preview: `apply_theme` was split into a committing
    wrapper (settings → visual → save) and `apply_theme_visual`
    (visual only, never touches settings). WM_MENUSELECT on the
    Theme/Accent menus folds the highlighted item into a session
    preview pair held in `App::menu_preview` — the first highlight
    snapshots the committed (theme, accent); a theme highlight
    resolves with the accent currently in effect and vice versa,
    so cross-menu browsing keeps one backup — and applies it
    visually. A click runs the ordinary command handlers (they
    discard the backup and persist as before); WM_EXITMENULOOP
    and WM_CANCELMODE restore the snapshot and resync the check
    marks (which are drawn from committed settings and never
    moved during preview). The Accent ▸ Custom… item has no
    preview (its dialog behaviour is unchanged; if the dialog is
    cancelled while a preview is somehow still in flight, the
    committed look is restored). Keyboard menu navigation
    previews too, because MENUSELECT covers it.
  - The built-in guide and README name both behaviours;
    CHANGELOG [1.1.0] and PARITY carry them. No new unit tests:
    the behaviour lives entirely in window-message handling,
    which the host suite cannot drive; the count stays 190.
  - Gate, run locally on the development host (true exit codes):
    - `cargo fmt --all` + `cargo fmt --all --check` — clean (exit 0)
    - `cargo test` — **190 passed, 0 failed** (exit 0)
    - `cargo clippy --all-targets -- -D warnings` (host) — clean
      (exit 0)
    - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
      -D warnings` — clean (exit 0)
    - `cargo check --target x86_64-pc-windows-msvc` — passed
      (exit 0)
    - `cargo build --release --target x86_64-pc-windows-gnu` —
      passed (exit 0); `VeloEdit.exe` 762,880 bytes, SHA-256
      `1f2d16594e888b3affc9393f93c37a0c5f5103aa4d0ef85b52d8f34402f92ff2`
  - Honest status: both features are compile-verified only. The
    Ctrl+wheel branch and the whole menu-preview flow
    (MENUSELECT ordering, EXITMENULOOP/CANCELMODE restore, check
    marks after dismissal) depend on the real Windows menu
    message sequence and are Unverified until the owner's machine
    runs them.

- 2026-11-01, roadmap slice R3a (feature cores: streaming hashes,
  data inspector readings, per-file bookmarks, delta-log recovery,
  save-time line-ending/BOM transforms, windowed find), run locally
  on the development host; NOT published:
  - Scope split, stated plainly: this slice is the core halves
    only. `src/hash.rs`, `src/inspector.rs`, `src/bookmarks.rs`
    and `src/recovery.rs` are new standalone modules (exported
    from `lib.rs`, with `op_` wrappers for inspect / windowed
    find / byte-source hashing); the save transforms extend
    `SaveOptions` (`encoding`, `eol`, `bom` — defaults keep both
    transforms off); `find_in_window` joins the search module.
    No window or CLI wiring was touched: the hash dialog, the
    inspector panel, the bookmark keys, the delta-recovery
    adoption and the EOL/BOM save controls all land in the R3
    wiring slice, and the GUI keeps using its snapshot recovery
    until then.
  - `cargo fmt --all` + `cargo fmt --all --check` — clean
    (exit 0)
  - `cargo test` — **227 passed, 0 failed** (190 before + 37 new:
    MD5/SHA-1/SHA-256 known-answer vectors — empty, "abc", the
    56- and 112-byte FIPS messages, and a million 'a' bytes fed
    in odd-sized chunks; inspector scalar/float readings in both
    byte orders, short-input gaps, and the days-to-civil
    conversion at the epoch, leap days, and the 2100 range edge;
    bookmark toggle ordering, wrap-around navigation, real-file
    JSON round trips, and stale-stamp record drops; recovery
    serialize/rebuild round trips recreating edited documents
    byte-identically, refusals for resized and tampered
    originals, corrupt-input errors without panics; save
    transforms — CRLF↔LF across chunk and piece boundaries,
    mixed-ending normalisation both ways, UTF-16LE/BE unit-width
    conversion, BOM add/remove for UTF-8 and UTF-16, ANSI no-op,
    Keep = identity; windowed find concatenating to find-all at
    window widths 1–7 plus straddler/degenerate windows)
  - `cargo clippy --all-targets -- -D warnings` (host) — clean
    (exit 0)
  - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
    -D warnings` — clean (exit 0)
  - `cargo check --target x86_64-pc-windows-msvc` — passed
    (exit 0)
  - `cargo build --release --target x86_64-pc-windows-gnu` —
    passed (exit 0); `VeloEdit.exe` 765,952 bytes, SHA-256
    `767746b194ce1d4a12caccc2992a6d803dfb70e80428d38035d8bcc87698a4f9`
  - Honest status: the cores are unit-tested on the host against
    published vectors and round trips. Nothing in this slice has
    a user-visible surface yet, so there is no new Windows
    runtime behaviour to verify; the fingerprint check in
    recovery rebuild compares size, modification stamp, and
    SHA-256 and refuses on any mismatch, which the host tests
    exercise for size and content changes (a stamp-only change
    cannot be simulated portably and is covered by the same
    comparison code path).

- 2026-11-01, roadmap slice R3b-1 (window wiring, part 1: roadmap
  items 20, 10, 24, 34, 7), run locally on the development host;
  NOT published:
  - What landed, all in `src/gui_win.rs`: bookmarks in the window
    (Ctrl+F2 toggles at the caret and persists the store at once;
    F2 / Shift+F2 jump next / previous with wrap-around; the
    file's list loads stamp-guarded at open; accent-square gutter
    markers in text mode and beside the offset in hex mode);
    background find-all (a plain find on a document over 4 MB
    walks `find_in_window` over 4 MB windows on a worker thread
    into a generation-tagged slot — the line-index thread's epoch
    discipline — the count reads "Counting…" until the final
    window lands, next / previous run over the matches known so
    far, and the first match at or after the caret is selected as
    soon as it is known; an edit, a document change, a
    scoped/masked find, or clearing the find cancels it, and the
    Replace flow forces one synchronous re-run first so it always
    works on a complete match list); scrollbar ticks (matches in
    the accent, bookmarks in the text colour, bucketed one per
    track row by the pure `tick_rows` helper, unit-tested in the
    Windows-only GUI suite); the external-change notice (the open
    file's (size, mtime) stamp re-checked on WM_ACTIVATE and
    immediately before a save; a difference raises a non-modal
    bar under the toolbar — "This file changed on disk.", Reload /
    Ignore — Reload goes through the ordinary dirty-document
    prompt and re-opens with the full per-document setup, Ignore
    dismisses until the stamp changes again); and delta recovery
    (file-backed documents write `recovery::capture` to
    `data/recovery2.bin`, the original's SHA-256 fingerprint
    computed on a background thread after open; startup checks
    the delta record first and reports an OriginalChanged skip in
    plain words; the 16 MB content snapshot remains only for
    documents with no backing file; a successful save or clean
    exit clears both forms).
  - `cargo fmt --all` + `cargo fmt --all --check` — clean
    (exit 0)
  - `cargo test` — **227 passed, 0 failed** on the host (the
    cores are unchanged; the new `tick_rows` bucketing test is
    Windows-only and runs in CI)
  - `cargo clippy --all-targets -- -D warnings` (host) — clean
    (exit 0)
  - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
    -D warnings` — clean (exit 0)
  - `cargo check --target x86_64-pc-windows-msvc` — passed
    (exit 0)
  - `cargo build --release --target x86_64-pc-windows-gnu` —
    passed (exit 0); `VeloEdit.exe` 841,728 bytes, SHA-256
    `0b1f04a337ac625b0474ffa70305e15587e00f39c3bf16902fad46ba866b8581`
  - Honest status: **every GUI behaviour in this slice is
    compile-verified only; Windows runtime is Unverified** until
    the program runs on a real Windows 11 machine. Known
    limitation (also in the README): a document restored from a
    delta record is memory-backed, so further unsaved edits fall
    back to the snapshot path — a huge restored document's new
    edits are not re-captured as a delta; the original record
    stays until a save or clean exit.

- 2026-11-01, roadmap slice R3b-2 (window wiring, part 2: roadmap
  items 15, 13, 14, 16, 31, 32, 37), run locally on the
  development host; NOT published:
  - What landed: the inspector core gained the GUID, FILETIME
    (1601-based, sane range to 2100, pre-epoch dates through the
    same civil conversion) and UTF-8 preview readings, with
    known-value tests (a stored GUID's byte order, FILETIME 0 =
    1601-01-01, 2021-01-01 and 1900-01-01 tick values, preview
    filtering/capping); hex layout became a parameter
    (`hexview::HexLayout`: row width, group size, big-endian group
    display) — the row maths gained width-taking variants the
    classic functions delegate to, formatting/cell/hit-test
    helpers keep the renderer's and the click maths on one code
    path, and the editor's hex internals (rows, vertical movement,
    Home/End, caret visibility) all follow the layout, with
    equivalence and formatting tests at widths 8/16/32 and groups
    2/4/8; settings gained validated keys for all of it
    (`inspector_open`, `hex_cols` with 0 = fit, `hex_group`,
    `hex_endian`, `save_eol`, `save_bom`) plus a boolean reader
    for the hand-rolled JSON; the editor gained
    `save_with_options` / `save_as_with_options` over the R3a
    pipeline. In the window (`src/gui_win.rs`): View ▸ Data
    Inspector docks a 280-DIP panel that re-reads the 16 bytes at
    the caret on every paint (the editor area, scrollbar, ruler
    and click maths all bound to the shrunken editor edge);
    View ▸ Hex Columns (8/16/24/32/64/Fit, fit recomputed on
    resize and when the panel toggles) and View ▸ Hex Grouping
    (1/2/4/8 + Big-Endian Groups) drive the layout through one
    sync point on startup, open, resize and menu change; View ▸
    Relative Offsets + Set Offset Base at Caret show signed
    address distances from a session-only base the status bar
    names; File ▸ Save Options radios feed every window save
    through the options pipeline, and the status bar announces an
    armed conversion ("· → CRLF", "· +BOM"); File ▸ File Hashes…
    runs the streaming hash of the file on disk on a worker
    thread into a slot the modal dialog polls (the Split dialog's
    pattern), fills the three digests on completion, and checks
    pasted text against them via the pure `hash_verdict` helper;
    the menu item is greyed without a file. The built-in guide
    covers all five features.
  - `cargo fmt --all` + `cargo fmt --all --check` — clean
    (exit 0)
  - `cargo test` — **242 passed, 0 failed** on the host (227
    before this slice; +15: 4 inspector, 7 hexview, 1 settings,
    3 editor; the three new shell-helper tests —
    inspector rows, hash verdict, armed suffix — are Windows-only
    and run in CI)
  - `cargo clippy --all-targets -- -D warnings` (host) — clean
    (exit 0)
  - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
    -D warnings` — clean (exit 0)
  - `cargo check --target x86_64-pc-windows-msvc` — passed
    (exit 0)
  - `cargo build --release --target x86_64-pc-windows-gnu` —
    passed (exit 0); `VeloEdit.exe` 915,968 bytes, SHA-256
    `e16283a220c94b0ced21e9065a23c2334eff79fca51361c3c76377f944ff6016`;
    the binary was string-checked for the new menu and dialog
    text to prove it is the fresh build
  - Honest status: **every GUI behaviour in this slice is
    compile-verified only; Windows runtime is Unverified** until
    the program runs on a real Windows 11 machine. One
    mid-implementation correction worth recording: an editor test
    first asserted Home at an 8-byte row width would return to
    the previous row's start; the editor was right (the caret
    after End *is* the next row's start at that width) and the
    test was fixed, not the code.

- 2026-11-01, find-bar restyle slice (owner feedback on a field
  run: the find bar's buttons read as plain grey slabs beside the
  toolbar's), run locally on the development host; NOT published:
  - What the audit found first: the find-bar buttons and the
    toolbar buttons were *already* one family — the same
    `BS_OWNERDRAW` child buttons drawn by the same `WM_DRAWITEM`
    pass (panel fill, editor-background fill while pressed, 1px
    line-colour frame, centred theme-text label) — and every
    find-bar control already had a tooltip. The visible divergence
    was elsewhere: (1) the find bar row was 34 DIPs tall against
    the toolbar's 30, so its controls came out 28 DIPs tall
    against the toolbar buttons' 24 — literally bigger slabs;
    (2) the bar's three statics (`Find:`, the match count,
    `Replace:`) were painted with the opaque panel brush, so each
    floated on the row as a small panel-coloured patch, and
    static text draws at the top of its rectangle while the
    buttons' labels are vertically centred, leaving the row
    ragged; (3) the count wore the full text colour wedged
    between the button groups at the standard 4-DIP gap; (4) the
    "In selection" toggle showed its state only by swapping a
    ☐/☑ glyph — it never took the accent border + accent text
    the Text / Hex toggles get while on.
  - What landed (`src/gui_win.rs` only): `FINDBAR_H` is now
    30 DIPs, the toolbar row's height, so every find-bar control
    is the toolbar buttons' 24 DIPs at the same +3 offset; the
    three statics gained `SS_CENTERIMAGE` so their text is
    vertically centred with the buttons, and `WM_CTLCOLORSTATIC`
    now blends just the find-bar statics into the row background
    (the status strip and change bar keep the panel treatment),
    with the count in the muted colour; layout gives the count
    an extra 4 DIPs on both sides (8 DIPs clear in all); the
    close button is square at the shared height (24 DIPs wide);
    the scope toggle's label is now "In selection", gaining a
    "✓ " prefix while on, and the draw pass's accent state
    covers it while `find_in_selection` holds. No find/replace
    behaviour, settings, or version changed.
  - `cargo fmt --all` + `cargo fmt --all --check` — clean
    (exit 0)
  - `cargo test` — **242 passed, 0 failed** on the host
    (unchanged count: the slice touches Windows-only GUI code
    and adds no host tests)
  - `cargo clippy --all-targets -- -D warnings` (host) — clean
    (exit 0)
  - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
    -D warnings` — clean (exit 0)
  - `cargo check --target x86_64-pc-windows-msvc` — passed
    (exit 0)
  - `cargo build --release --target x86_64-pc-windows-gnu` —
    passed (exit 0); `VeloEdit.exe` 916,480 bytes, SHA-256
    `4c672b6a02d1ddedf6917dab1a508f1628f67bf91f1520dc29d7a43e335bc959`;
    the binary was string-checked to prove it is the fresh
    build (the "✓ In selection" label is present; both
    ballot-box label strings are gone)
  - Honest status: **the restyle is compile-verified only;
    Windows runtime is Unverified** until the program runs on a
    real Windows 11 machine and the owner judges the find bar
    beside the toolbar there.

- 2026-11-01, polish pass slice (owner-approved mockup: a
  usability / UX / accessibility layer over R3 and the find-bar
  restyle), run locally on the development host; NOT published:
  - What landed (`src/gui_win.rs` only): the six toolbar buttons
    draw small glyph icons (folder, floppy, curved arrows — a
    nine-segment semicircle polyline, mirrored by arrowhead
    placement — magnifier, opposed arrows) left of their labels,
    icon and label centred as a group using the button DC's own
    text measurement. The icons are GDI, not Direct2D: the
    owner-draw pass is the GDI `WM_DRAWITEM` handler that draws
    the labels, so the glyphs are one pen in the label colour
    plus the hollow stock brush in that same pass. Every
    owner-drawn button — toolbar, find bar, change bar, and the
    Split / File Hashes dialog buttons, which have their own
    draw handlers — now draws an accent frame inset 2 px inside
    its border while it holds keyboard focus (`ODS_FOCUS`), the
    first visible focus indicator these buttons have had; a
    disabled button (`ODS_DISABLED`) labels in muted, though no
    button is disabled today. The Text / Hex pair became one
    segmented control: no gap, a single shared frame drawn as
    edge strips with the divider drawn once (as the Text
    segment's right edge), the active segment accent-filled
    with a white label; the pair's combined footprint is
    unchanged at 108 DIPs (each segment widened to 54). The
    status bar's right cluster leads with the zoom percentage —
    the settings default font size (14 pt) is 100%, via the new
    pure `zoom_percent` helper, and the readout is live because
    the font menu commands and Ctrl+mouse-wheel already route
    through `refresh_status` — and its widths grew to fit
    (392 / 528 DIPs) with 8 DIPs of clear space at both ends of
    the strip. In the editor, the translucent current-line wash
    became an opaque raised band (the background blended
    halfway toward the panel colour, via the new pure
    `blend_rgba` helper) spanning the text area only — not the
    gutter — drawn before the match, selection and text in both
    modes; gutter numbers and hex addresses off the caret row
    moved from muted to muted blended halfway toward the text
    colour. The theme struct gained no fields: both tones are
    derived in the shell as two extra Direct2D brushes. The
    built-in guide gained a short "Reading the window"
    paragraph covering the icons, the focus outline and the
    zoom readout.
  - `cargo fmt --all` + `cargo fmt --all --check` — clean
    (exit 0)
  - `cargo test` — **242 passed, 0 failed** on the host
    (unchanged count: the slice touches Windows-only GUI code;
    its two new helper tests — zoom percentage, 50/50 colour
    blend — are Windows-only and run in CI)
  - `cargo clippy --all-targets -- -D warnings` (host) — clean
    (exit 0)
  - `cargo clippy --target x86_64-pc-windows-gnu --all-targets --
    -D warnings` — clean (exit 0)
  - `cargo check --target x86_64-pc-windows-msvc` — passed
    (exit 0)
  - `cargo build --release --target x86_64-pc-windows-gnu` —
    passed (exit 0); `VeloEdit.exe` 922,624 bytes, SHA-256
    `0c85ddb168847dd6c20461e0feb5e68b78f15a1d4cd1a436231e4dc19615904f`;
    the binary was string-checked for the new guide text
    ("READING THE WINDOW", "100% is the standard size") to
    prove it is the fresh build
  - Honest status: **every visual change in this slice is
    compile-verified only; Windows runtime is Unverified**
    until the program runs on a real Windows 11 machine and
    the owner judges the polish there.
- 2026-11-01, instant-open slice (follow-up to the owner's
  field report: a 10 GB file took 23 seconds to open on
  Windows 11), run locally on the development host:
  - Diagnosis: a background index thread already existed,
    but the first status, layout and paint calls ran
    `ensure_index()`, which built the **whole** line index
    synchronously on the UI thread — the background result
    arrived later, redundant. The mapping, the encoding
    sniff (first 64 KB only) and the recovery fingerprint
    (already a background thread) were cleared.
  - What changed: the editor seeds only the first 4 MB
    (`INDEX_HEAD_BYTES`) at the first line question; a
    resumable `LineIndexBuilder` in the core finishes the
    scan in the shell on a background thread, publishing
    snapshots every 32 MB through the existing epoch-tagged
    slot, and the 200 ms tick attaches each in turn
    (monotonic frontier). An edit bumps the document
    generation and the in-flight build is discarded,
    restarting from the next open/save baseline; a
    superseded worker stops when it finds a newer epoch
    already in the slot. While the index is partial,
    positions stay exact (forward scan from the last
    checkpoint), `line_count` reports the indexed prefix
    only, the status bar appends "· Indexing NN%", the
    text scrollbar maps proportionally to file size,
    scrolling clamps to the indexed prefix, and a row
    anchor memoizes the first visible line's start so a
    view parked past the frontier never re-scans per
    frame. Small files (≤ 4 MB) keep the old behaviour
    exactly; hex mode is untouched.
  - `cargo fmt --all -- --check` — clean (exit 0)
  - `cargo test` — **249 passed, 0 failed, 2 ignored**
    (exit 0)
  - `cargo clippy --all-targets -- -D warnings` (host) —
    clean (exit 0)
  - `cargo clippy --target x86_64-pc-windows-gnu
    --all-targets -- -D warnings` — clean (exit 0)
  - `cargo check --target x86_64-pc-windows-msvc` —
    passed (exit 0)
  - `cargo build --release --target
    x86_64-pc-windows-gnu` — passed (exit 0);
    `VeloEdit.exe` 927,744 bytes, SHA-256
    `06c7b690c7abc7f65a78a7b7d8c3a44fdfca1ebc87ab240d2b2fc59da33df7d2`;
    string-checked for the new status text ("Indexing")
    and the size/hash differ from the previous build,
    proving it is the fresh binary
  - Bench (host proxies, in-memory 1 GB source, release
    test build): one-shot index build 3,112.4 ms; head
    prefix (4 MB) 4.89 ms; staged full build in 32 MB
    steps 2,045.9 ms; editor staged open answering its
    first line question in 32.81 ms. Recorded in
    `docs/BENCHMARKS.md`.
  - Honest status: **the staged open path is verified by
    host tests and compile checks only; Windows runtime
    is Unverified** until the owner opens a huge file on
    a real Windows 11 machine and times it.

- 2026-11-01, architectural audit pass (six zero-overhead Rust
  patterns, audit-first: checklists and evidence before any change,
  changes only where the audit proved a case). Nothing in this pass
  changes user-visible behaviour, so CHANGELOG.md has no entry for
  it — every change is internal (compile-time layout pins, hot-loop
  allocation reuse, one dead error path removed).

  **Pattern 1 — ZST capability tokens: audited, nothing changed.**
  Examined: `Editor.index: Option<LineIndex>` +
  `Editor.index_dirty_from: Option<u64>` (`src/editor.rs`), the
  staged-build states inside `LineIndex` / `LineIndexBuilder`
  (`src/line_index.rs`: `complete: bool`, `indexed_bytes`,
  `LineIndexBuilder.stopped`), the save pipeline's streaming state
  (`SaveTransform.pending_cr` / `unit_hold`, `src/save.rs`), and
  every boolean parameter on internal core functions. Findings: no
  loose runtime flag is passed between internal functions anywhere
  in the core — the only bool parameters are the caret API's
  `extend_selection` family and `SearchOptions`-style inputs, which
  are user-level choices, not state verification. Each audited
  state lives in exactly one owner: partial-vs-complete is
  `LineIndex`'s own data (derived by `LineIndexBuilder::snapshot`,
  never passed in), edit invalidation is `Editor`'s
  `index_dirty_from` (an `Option<u64>`, already a sum type: clean,
  or dirty-from-offset), and all transitions run through three
  `Editor` methods (`ensure_index`, `attach_line_index`,
  `after_mutation`). Every consumer that presents totals gates on
  `is_complete()` (e.g. `editor.rs` line-count gate). Verdict: the
  invalid states a capability token would kill are already
  unrepresentable or centrally guarded; a typestate split of
  `LineIndex` would rewrite the editor/GUI index surface to defend
  against a bug class the audit found no instance of. No change.

  **Pattern 2 — scoped/arena allocation: audited; three genuine
  repeated allocations removed, no arena introduced.** Path by
  path: the SWAR counters (`count_byte_swar`, `window_matches`)
  allocate nothing — slices in, values out. The save pipeline's
  Keep/Keep path streams borrowed piece slices with zero per-chunk
  allocation, and its transform path already reuses one output
  buffer with `clear()` per chunk (`write_staging`); the split
  path likewise reuses one buffer. Sort/dedup materialise their
  range exactly once under a tested 64 MB refusal cap — a cold,
  user-invoked operation, not a hot loop. Three real finds:
  (a) `LineIndexBuilder::advance` allocated up to eight fresh
  64 KB block buffers plus a block list and a result list for
  *every round* of *every* stage — roughly three allocations per
  512 KB scanned, ~20,000 allocations for a 1 GB one-shot build.
  The builder now keeps a scratch pool (`scratch_buffers`,
  `scratch_blocks`, `scratch_results`) recycled between rounds;
  the staged/serial/one-shot checkpoint equivalence tests pin
  identical output. (b) `PieceTable::rebuild_weights` — called by
  every structural mutation — collected the piece lengths into a
  throwaway `Vec<u64>` before building the Fenwick tree;
  `build_fenwick` now consumes an iterator, so all three call
  sites allocate nothing beyond the tree itself. (c) The three
  streaming search scans (`collect_matches`, `find_in_window`,
  `find_masked_all`) allocated a fresh scan window (tail + slice)
  per piece, plus a fresh candidate list per piece in the masked
  scan; the window (and the masked scan's `starts` list) is now
  hoisted out of the slice loop and refilled with `clear()`.
  Disclosed, deliberately unchanged: `PieceTable::read` allocates
  by API contract (its callers want an owned `Vec`; the
  zero-allocation `read_into` exists beside it), `coalesce` and
  `delete` allocate a replacement piece list per call (structural,
  once per edit, not per byte), `snapshot()` clones the checkpoint
  list by definition, and the scoped-thread round in `advance`
  still collects ≤ 8 join handles per round (bounded, trivially
  small next to the 64 KB buffers). A hand-rolled bump arena was
  considered and rejected: every retained fix is plain buffer
  reuse inside one function or one builder — an arena type would
  add a lifetime discipline the code does not need. Bench (host
  proxies, ignored release benches, 1 GB in-memory source, this
  machine): before — full build 2,003.5 ms, staged full build
  983.1 ms, head prefix 3.48 ms, editor staged open 36.95 ms;
  after — full build 898.7 ms, staged full build 994.5 ms
  (flat, inside run-to-run variance), head prefix 3.16 ms,
  editor staged open 3.46 ms. Single runs, variance visible in
  the spread; the claim made is only "no regression, and the
  one-shot path that shares the pooled code is faster".

  **Pattern 3 — `const` layout assertions: applied.** Measured
  first (identical field layouts probed out-of-tree, then pinned
  in-tree — the pins themselves are the proof, since a wrong value
  would fail this build): `PieceNode` = 24 B / align 8 and the
  Fenwick weight = one `u64` per piece, pinned in
  `src/piece_table.rs` — together they are the entire per-piece
  memory the SPEC section 8 active-memory budget (< 35 MB) is
  spent against. `LineCheckpoint` = 24 B / align 8, pinned in
  `src/line_index.rs` — one checkpoint per 64 KB of source, so a
  complete index costs exactly source_len / 64 KB × 24 B.
  `EditAction` = 48 B and `HistoryEntry` = 104 B, pinned in
  `src/undo.rs` — the records the history step limit (1,024) and
  stored-delta byte budget (default 256 MB) are spent against.
  The `MaskedPattern` element `(u8, u8)` = 2 B, pinned in
  `src/search.rs` (a tuple cannot carry `#[repr(transparent)]`;
  the pin states the same flat two-bytes-per-pattern-byte
  guarantee). One honest observation the arithmetic surfaces, not
  a doc contradiction (no document states a per-checkpoint size):
  a fully-built index over a 100 GB source holds ~1.6 M
  checkpoints ≈ 39 MB in this record shape — the SPEC section 8
  active-memory figure remains **Unverified** on Windows (see
  `docs/BENCHMARKS.md`) and this observation is now on record
  beside it.

  **Pattern 4 — transparent newtypes for offsets: AUDIT ONLY,
  decision recorded here.** The raw flows: byte offsets and line
  numbers are both `u64` end to end. 54 public signatures across
  the core modules (`piece_table` 8, `line_index` 9, `search` 6,
  `editor` 20, `save` 5, `undo` 4, `recovery` 1, `hash` 1)
  mention `u64`; six search entry points return offset lists as
  `Vec<u64>` / `Option<u64>`; the public struct fields carrying
  raw offsets include `PieceNode.source_offset` / `length`,
  `LineCheckpoint.byte_offset` / `line_number` (two different
  units in one record — the strongest single argument for
  newtypes), and `EditAction`'s offset/length fields. Past those
  signatures, `gui_win.rs` (~7,900 lines, Windows-only) and
  `cli.rs` thread the same `u64`s through dozens of call sites
  each. Nothing in the audit found a *swapped-units bug* — no
  place where a line number was used as a byte offset or vice
  versa; the confusion risk is real but unrealised, and field
  names (`byte_offset`, `line_number`) currently carry the
  distinction. Recommendation: PARTIALLY ADOPT in a dedicated
  future slice, core-only — introduce `ByteOffset` / `LineNumber`
  `#[repr(transparent)]` newtypes in `line_index` and
  `piece_table` (where the two units meet in one record), convert
  the core's internal flows, and keep the editor's public API in
  raw `u64` until the shell can be converted in the same slice,
  so the crate never carries a half-wrapped boundary. Estimated
  blast radius: ~10 core files' signatures for the partial form
  (the 54 signatures above plus internals), versus every offset
  call site in `gui_win.rs` + `cli.rs` for the full form — the
  full form should not be attempted without a Windows machine in
  the loop, since the shell cannot be exercised from here. NOT
  done in this pass, by instruction and by judgement.

  **Pattern 5 — infallible stripping: applied where genuine.**
  One dead error path removed: `format_unix_millis`
  (`src/inspector.rs`) unwrapped `strip_suffix(" UTC")` with
  `.expect(...)`; the suffix is guaranteed by
  `format_unix_seconds`' format string (same module, NO_READING
  case returned earlier), so the strip is now a `unwrap_or`
  fallback that returns the string unmodified on the unreachable
  path — identical output on every reachable input, no panic
  path. Every other non-test `.unwrap()` / `.expect()` in the
  crate was inventoried and stays, each with its justification:
  (i) fixed-size slice→array conversions
  (`try_into().expect("8 bytes")` and kin in `hash.rs`,
  `search.rs`, `line_index.rs`, `inspector.rs`, `recovery.rs`)
  — the lengths are exact by construction (`chunks_exact`, fixed
  block indexing, a length-checked `take`); the `expect` *is* the
  proof-carrying form, and no unwrap-free safe spelling exists.
  (ii) `diff_bytes`' `.expect("comparing in-memory slices cannot
  fail")` — `io::Cursor` reads are infallible in fact but
  `io::Error` is inhabited, so an `Infallible` pipeline would
  mean duplicating the chunk engine; the expect documents the
  invariant at the one call site. (iii) `Editor`'s
  `.expect("index present")` (four sites) and
  `.expect("counted newlines are present")` — postconditions of
  `ensure_index()` / a counted scan in the same method.
  (iv) `PieceTable::insert`'s `.expect("offset < total_length
  has a piece")` — the same shape, guarded by the assert two
  statements above. (v) `UndoStack`'s three
  `.expect("coalesce implies the snapshot was taken")` and
  `.expect("len > 1")` — option/queue states established by the
  branch that takes them, in the same function. (vi) The
  line-index worker's `join().expect("line-index worker
  panicked")` — deliberate policy: a panicked scan worker must
  fail the build loudly, not yield a silently truncated index.
  (vii) `bookmarks.rs`' `.expect("just pushed")` — same-statement
  invariant. (viii) `theme.rs`' default-theme lookup — a
  compile-time table containing the key by construction.
  (ix) `gui_win.rs`' five `.expect("checked above")`-style sites —
  each guarded by the check its message names, in the same
  function; the shell's lock discipline needs no unwraps at all:
  every `Mutex` slot access uses `if let Ok(...)`, `.ok()`, or
  `try_lock`, so a poisoned slot degrades to a skipped update
  instead of a panic (the poisoned-lock policy for this codebase,
  applied consistently). (x) `save.rs`' `.expect("mapped backing
  has a path")` — the `Mapped` variant carries its path by
  construction. False positive worth recording: the many
  `parser.expect(...)` / `self.expect(...)` calls in
  `bookmarks.rs` and `settings.rs` are a hand-written JSON
  parser's own `expect(char) -> Result` method — fallible
  parsing, not panics. No trait implementation in the crate
  returns a dummy error: the genuine error types (`EditorError`,
  `SaveError`, `io::Result`, parse `Result<_, String>`) all carry
  reachable failure paths (disk, permissions, malformed input)
  and were left exactly as they are.

  **Pattern 6 — cache-line alignment: audited, nothing changed.**
  Examined: the four shared worker slots in `src/gui_win.rs` —
  `index_slot`, `find_slot`, `fingerprint_slot` (all
  `Arc<Mutex<Option<…>>>`) and the hash dialog's slot — plus the
  epoch counters (`index_epoch`, `doc_epoch`) and a crate-wide
  search for atomics (there are none: no `Atomic*` type appears
  anywhere in `src/`). Each slot is an independent heap allocation
  behind its own `Arc`, so no two mutated fields share a struct,
  let alone a cache line. Cadence is the deciding evidence: the
  index worker publishes one snapshot per 32 MB scanned, the find
  worker's results are picked up on the window's 200 ms tick, and
  the fingerprint and hash slots are each written once per
  document/operation. The epoch counters are mutated only on the
  UI thread. Nothing is written by two threads at a cadence where
  false sharing could cost a measurable cycle; `#[repr(align(64))]`
  padding would add bytes to defeat a sharing pattern that does
  not exist. Verdict: no change; revisit only if a future design
  introduces a genuinely hot cross-thread counter or queue.

  - Gate (this pass, true exit codes): `cargo fmt --all --
    --check` — clean (0). `cargo test` — **249 passed, 0 failed,
    2 ignored** (0). `cargo clippy --all-targets -- -D warnings`
    (host) — clean (0). `cargo clippy --target
    x86_64-pc-windows-gnu --all-targets -- -D warnings` — clean
    (0). `cargo check --target x86_64-pc-windows-msvc` — passed
    (0). `cargo build --release --target x86_64-pc-windows-gnu`
    — passed (0); `VeloEdit.exe` 930,304 bytes, SHA-256
    `6424aff403776266a5ecdd6ea635df3ac7961b35bbe16b168c083c124769a53d`
    (previous build 927,744 bytes / `06c7b690…`; the size and hash
    differ, proving this is the fresh build).
  - Honest status: core changes are verified by the host test
    suite (including the staged/serial index equivalence and the
    search naive-equivalence tests). No GUI code changed in this
    pass. Windows runtime remains **Unverified** for everything
    the §8 table lists, exactly as before.

- 2026-11-01, roadmap R4a (R4 engine cores), run locally on the
  development host. Core modules only — `src/gui_win.rs` and
  `src/cli.rs` untouched; the CLI/GUI surfaces for these cores
  land in R4b/R4c. What was added: the Ghost Tail growth path
  (`PieceTable::extend_original` / `adopt_grown_original`,
  `LineIndex::extended_to`, `LineIndexBuilder::resume_from` with
  a mid-interval top-up so a resumed build stays
  checkpoint-identical to a one-shot build, and
  `Editor::note_file_growth` refusing dirty or history-carrying
  documents); the structural filter view (`src/filter.rs`); the
  multi-range slicing core (`src/slicing.rs`); the zero-run piece
  (`BufferSource::Zeroes` through the piece table, undo, editor
  and the recovery record); the `VELOPAT1` delta patch core
  (`src/patch.rs`); CRC32 and xxHash64 in `src/hash.rs`; and the
  strings, entropy, pointers/jump-history, time-delta and
  delimiter cores (`src/strings.rs`, `src/entropy.rs`,
  `src/pointers.rs`, `src/timedeltas.rs`, `src/delimited.rs`),
  plus `inspector::days_from_civil` (the civil-date inverse the
  time-delta core validates dates with) and four `op_*` facades.
  - Gate (this slice, true exit codes): `cargo fmt --all --
    --check` — clean (0). `cargo test` — **306 passed, 0 failed,
    2 ignored** (0). `cargo clippy --all-targets -- -D warnings`
    (host) — clean (0). `cargo clippy --target
    x86_64-pc-windows-gnu --all-targets -- -D warnings` — clean
    (0). `cargo check --target x86_64-pc-windows-msvc` — passed
    (0). `cargo build --release --target x86_64-pc-windows-gnu`
    — passed (0); `VeloEdit.exe` 935,936 bytes, SHA-256
    `585671e9e60b37f023a9c81d88099dcef7582fca51d8c4ae3ef4e373bbafe89a`
    (previous build 930,304 bytes / `6424aff4…`; the size and hash
    differ, proving this is the fresh build).
  - Known-answer honesty note: the xxHash64 values first pinned
    for the two FIPS test messages were wrong in the test, not in
    the implementation — the values were re-derived from the
    reference implementation during this slice and the corrected
    constants are what the suite now pins (the implementation
    matched the reference on every vector, including the two
    corrected ones). The CRC32 vectors were reference-checked
    the same way.
  - Honest status: all R4a cores are verified by the host test
    suite only. Nothing in this slice is user-visible yet (every
    PARITY row is **Foundation**); Windows runtime remains
    **Unverified**, exactly as before.

- 2026-11-01, roadmap slice R4b (VIEW wiring: the R4a cores get
  their window surfaces, plus the item 19 / 24 / 30 expansions).
  All work is in `src/gui_win.rs` except three small editor
  additions (`Editor::adopt_grown_backing` — the re-map growth
  path sharing `note_file_growth`'s guards through a common tail;
  `Editor::line_index_snapshot`; `Editor::filter_view`;
  `Editor::replace_byte_at`) and five persisted settings keys
  (`follow_tail`, `word_wrap`, `strings_open`, `strings_min_len`,
  `freeze_lines`). What landed: Follow Tail / Ghost Tail (item
  22) through the existing external-change check with a resumed
  index build; the structural Filter view (item 18) over a new
  display-row model that word wrap (item 19), the freeze pane
  and the filter all share, so paint and pointer maths cannot
  drift; the Strings pane and Entropy strip (epoch-guarded
  workers mirroring the find/index discipline, synchronous under
  the same 4 MB threshold); the Time-Delta gutter; the Delimiter
  Grid (paint-only alignment); focus dimming; the ANSI-colour +
  control-tag segment painter (16 cached ANSI brushes and a
  16-step entropy ramp now live in the brush set); pointer
  following + jump history (item 11) with pushes at the shell's
  jump points; the Data Inspector's bit row; and scrollbar tick
  hover previews (item 24 expansion) driven through a manually
  tracked tooltip tool. Editing is refused, with the reason in
  the status bar, in exactly two new places: everywhere while
  the filter view is up, and inside the frozen block.
  - Gate (this slice, true exit codes): `cargo fmt --all --
    --check` — clean (0). `cargo test` — **309 passed, 0 failed,
    2 ignored** (0). `cargo clippy --all-targets -- -D warnings`
    (host) — clean (0). `cargo clippy --target
    x86_64-pc-windows-gnu --all-targets -- -D warnings` — clean
    (0). `cargo check --target x86_64-pc-windows-msvc` — passed
    (0). `cargo build --release --target x86_64-pc-windows-gnu`
    — passed (0); `VeloEdit.exe` 1,056,256 bytes, SHA-256
    `5b0ed92e04d447e3c8041029f2f6dda5da97dc59608c848670a9beb406b44fe7`
    (previous build 935,936 bytes / `585671e9…`; the size and
    hash differ, proving this is the fresh build).
  - Scope notes, stated plainly: the filter prompt builds
    literal queries (the core's masked filter queries remain
    covered by core tests and the headless facade); word wrap's
    exactness limits (the editor's own row fetch caps) and the
    grid's byte-true columns are documented in docs/PARITY.md;
    the hover preview uses a tracking tooltip driven by the
    shell (350 ms settle, re-validated against the pointer every
    tick), which only a field run can prove feels right.
  - Honest status: GUI compile-verified only. The new GUI tests
    (wrap ranges, segments, cell maps, delta formats) are
    Windows-only and run in CI; every R4b surface's Windows
    runtime is **Unverified** until the owner's machine runs it.

- 2026-11-01, roadmap slice R4c (SYSTEMS wiring: the R4a cores
  get their remaining surfaces, plus keybindings, protected
  files, mapping windows, block-clone save, structural diff,
  extended code pages, and the two filter field fixes). What
  landed: the Encoding menu with a Code Pages submenu (seven
  pages; Win32 conversion behind a converter hook the shell
  installs, pure-Rust tables for ISO-8859-1/15; the piece table
  still stores the file's own bytes — the codec translates at
  the display/typing/search/save edges); Extract Ranges,
  Export/Apply Delta Patch, Insert Zeroes and Compare With in
  both the window (dialogs cloned from the Split/Hash patterns)
  and the CLI (`--slice`, `--export-patch`, `--apply-patch`,
  `--insert-zeroes --at`, `--compare`); bookmark colours and
  notes in the same central store (old files load unchanged);
  the File Hashes dialog completed with CRC32 and xxHash64;
  `data/keybindings.json` with the dispatch built from the
  merged map and menu labels showing the effective chords;
  read-only-file handling (status note, title suffix, Save
  offering a copy, Open as Administrator with the explanation
  before the UAC prompt); the `map_window_mb` windowed reader
  wired into the open path (backing migration deliberately NOT
  in this slice — see the scope notes); the opt-in
  `fast_save_clone` block-clone save with silent fallback; and
  the filter fixes — an empty Filter confirmation (or the new
  Search ▸ Clear Filter) exits the filter, and the filter
  build runs on a generation-tagged background worker with
  "Filtering… NN%" progress on documents over 4 MB.
  - Gate (this slice, true exit codes): `cargo fmt --all --
    --check` — clean (0). `cargo test` — **347 passed, 0
    failed, 2 ignored** (0). `cargo clippy --all-targets --
    -D warnings` (host) — clean (0). `cargo clippy --target
    x86_64-pc-windows-gnu --all-targets -- -D warnings` —
    clean (0). `cargo check --target x86_64-pc-windows-msvc`
    — passed (0). `cargo build --release --target
    x86_64-pc-windows-gnu` — passed (0); `VeloEdit.exe`
    1,240,576 bytes, SHA-256
    `37974df2585032f865259d48c97b89729b186f3b6bcedd8b0044af8f883d5dcd`
    (previous build 1,056,256 bytes / `5b0ed92e…`; the size and
    hash differ, proving this is the fresh build).
  - Scope notes, stated plainly: mapping windows are Partial —
    the windowed reader and its maths are delivered and the
    open path samples through it, but the piece-table backing
    still maps the whole file (its reads borrow the mapping as
    one slice; the migration is a piece-table slice of its
    own). The block-clone save is compile-verified and its
    fallback host-tested; the FSCTL path itself can only be
    proven on a Windows machine with a suitable volume. The
    code pages' Win32 half is compile-verified; the ISO tables
    carry the host proof. Three new dialogs (Extract Ranges,
    Compare With, bookmark editor) follow the established
    modal patterns.
  - Honest status: GUI compile-verified only; every R4c
    surface's Windows runtime is **Unverified** until the
    owner's machine runs it.
  - CI correction (same day): the first R4c publish's CI run
    failed one Windows test —
    `save::tests::fast_clone_request_falls_back_to_a_correct_save`
    deleted its temp directory while the table still held the
    saved file's live mapping, which Windows refuses with
    ERROR_SHARING_VIOLATION (the host gate cannot see this;
    Unix permits the unlink). The test now drops the table
    before cleanup, the rule the neighbouring save_to tests
    already document. Test-only change; no product code moved.

- 2026-11-01, roadmap slice R5a (R5 pure cores: the six
  host-testable cores the R5 wiring slices build on; no GUI
  wiring in this slice beyond operation facades in
  `src/lib.rs`). What landed: (1) `src/regex.rs` — streaming
  regular expressions (roadmap item 9): a Thompson NFA compiled
  to a bytecode program (cap 8192 instructions; counted repeats
  expand; nesting cap 128) and run as a Pike VM over a windowed
  `FnMut(u64, &mut [u8]) -> usize` reader (the `patch.rs`
  reader shape, chosen and named in the module docs) —
  backtracking-free, so cost is bounded by input × program and
  pathological patterns cannot blow up; literals, `.`,
  classes/ranges/negation, `\d \w \s` and complements, `\xHH`,
  line anchors, greedy + lazy quantifiers and `{n,m}`,
  alternation, groups; whole-match spans only, streamed in
  bounded memory (two thread lists + one window).
  (2) `src/templates.rs` — the binary-template language
  (item 17): struct definitions with every sized int/float
  primitive in both endians, `char` arrays as text, nested
  structs, `//` comments, and count-from-field arrays (the
  count names an earlier scalar integer field of the same
  struct); parse errors carry line numbers, recursive structs
  and unknown types are refused at parse time, and applying
  over a reader yields a flat leaf list with offsets, sizes
  and display values — truncation returns a partial result
  flagged incomplete, never an error. (3) `src/linediff.rs` —
  the line-diff core for the side-by-side diff: common
  prefix/suffix framing, lockstep consumption, and
  bounded-window anchor resynchronisation (SHA-256 line hashes
  confirmed by exact comparison, 3-line confirm), emitting
  coalesced regions plus the first differing line; documented
  in the module and in PARITY as anchor-and-trim alignment,
  NOT a minimal Myers script. (4) `src/disasm.rs` — an x86-64
  length-accurate instruction decoder: legacy prefixes, REX,
  one-byte + 0F/0F38/0F3A maps (3DNow suffix byte included),
  VEX/EVEX/XOP, ModRM/SIB/displacement/immediate sizing per
  the Intel rules in 64-bit and 67h 32-bit addressing;
  mnemonic text is best-effort with honest generic renderings
  otherwise; 64-bit-dropped opcodes and truncation return
  typed errors, never panics. (5) `src/exttools.rs` — the
  external-tools protocol core (item 49): parses/validates
  `data/tools.json` (input/output enums with defaults,
  non-empty unique names, per-field errors, unknown fields
  tolerated) and the pure `{file}`/`{offset}` substitution;
  no process spawning (wiring slice). (6) `src/undo.rs` gains
  multi-range editing: `UndoStack::do_multi_replace` applies
  range edits back-to-front as ONE undo entry on the
  segment-replace mechanism (validated first — ascending,
  non-overlapping, in-bounds; a refusal mutates and records
  nothing; no-op edits dropped; a single pure insert degrades
  to `do_insert`), plus byte-column rectangular
  extract/delete/insert over a caller-supplied line map as
  single undo entries (clamped per line, no padding, line
  terminators untouched). Facades: `op_find_regex`,
  `op_line_diff`, `op_apply_template`, `op_multi_replace`,
  `op_rect_extract`, `op_rect_delete`, `op_rect_insert`.
  Fifty new host tests (347 → 397): the regex suite includes
  an independent continuation-passing backtracking reference
  matcher and a 400-case generated equivalence sweep, 3-byte
  window spanning, anchor/lazy/empty-match cases, and
  `a*a*a*b` over 200 000 non-matching bytes completing inside
  the normal run; templates run a PE-shaped template with
  known values, count-from-field, nesting, truncation and
  eleven parse-error cases; linediff covers
  identical/insert/delete/change/both-ends/coalescing and
  indexed-reader equivalence; disasm carries a known-length
  corpus, truncation at every prefix of eight multi-byte
  forms, the dropped-opcode set, and an exact-end stream walk;
  the multi-edit suite pins exact bytes, single-undo layout
  restoration, validation refusals and rectangle clamping.
  Two real bugs were caught by the suites and fixed in-slice:
  the Pike VM's per-step match arbitration (a same-step
  lower-priority thread could overwrite a leftmost match),
  and the 0F BA group encoding being shadowed in the 0F
  length table (wrong length for a ModRM+imm8 form).
  - Gate, run locally on this tree: `cargo fmt --all -- --check`
    clean (exit 0). Host `cargo test`: **397 passed; 0 failed;
    2 ignored**. `cargo clippy --all-targets -- -D warnings`
    clean (exit 0, host) and
    `cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`
    clean (exit 0).
    `cargo check --target x86_64-pc-windows-msvc --all-targets`
    clean (exit 0). Release build
    (`cargo build --release --target x86_64-pc-windows-gnu`,
    exit 0): `VeloEdit.exe` 1 240 576 bytes, SHA-256
    `4b34222060e98f5b4306d8d4e1689066c21a854f2e006d2a8d01972a90eda19c`
    — the same size as the R4c build with a fresh hash and
    timestamp: the new cores are not yet referenced by the GUI
    binary, so the release linker strips them. Toolchain note:
    the MinGW linker was missing on this machine (the VM had
    been replaced) and was reinstalled from apt before the
    build, per the standing recovery note.
  - Honest status: every R5a core is host-verified by its test
    suite; nothing in this slice is Windows-only code, and no
    core is reachable from the GUI or CLI yet — the wiring
    (regex in Find, the side-by-side diff view, the templates
    panel, the tools menu, multi-cursor UI, the disassembly
    gutter) is the R5b/R5c slices, and Windows runtime
    behaviour stays **Unverified** until the field checks on
    a real machine.
- 2026-11-01, roadmap slice R5b (tabs and multiple windows,
  roadmap item 39 — the largest interface change on the
  roadmap). What landed: (1) `src/tabs.rs` — the pure tab
  cores: `TabOrder` (the open/close/reorder/active-index
  bookkeeping with the parked-list conversion the shell's
  swap-based parking relies on), `fit_widths` (tab widths
  fitted to the strip, widest giving up space first, floored
  at a minimum), and the session-list mapping
  (`session_from_docs` with its active-index rule,
  `plan_restore` against an existence probe,
  `normalise_session`, capped at 32 tabs). (2) The shell
  restructure in `src/gui_win.rs`: a `DocState` struct now
  owns everything per-document — the editor, the find state
  and its background find-all worker slot, the staged
  line-index builder and its snapshot slot, the filter view
  and its background builder slot, follow-tail/growth state,
  the bookmarks view, the external-change bar state, the
  delta-recovery fingerprint state, inspector/hex state,
  the jump history, and the armed save options (now
  per-document; the settings keep the default for new
  documents). The App keeps the live document in the same
  fields the code always used and parks inactive documents
  by swapping state, so per-file workers — which hold their
  own document's Arc slots — keep running on inactive tabs
  and land in their own tab; no worker ever writes to "the
  active document". (3) The tab strip, painted on the canvas
  with the toolbar family between the accent strip and the
  toolbar: file name, a filled accent square for unsaved
  changes, a hollow square for a read-only file, a close ×
  on the active and hovered tabs, the full path as the
  hover tooltip, a "+" new-tab button; click switches,
  middle-click closes, the wheel over the strip scrolls it,
  tabs shrink to a minimum width then scroll, and dragging
  a tab reorders it with an insertion marker. A tab context
  menu offers Close, Close Others, Close All, Copy Path and
  Open in New Window. (4) Commands, all routed through the
  merged keybinding map as new actions: `new_tab` Ctrl+T,
  `close_tab` Ctrl+W, `next_tab` Ctrl+Tab, `prev_tab`
  Ctrl+Shift+Tab, `tab_1`..`tab_8` Ctrl+1..8 and `tab_last`
  Ctrl+9 (the find bar's edit controls post the tab chords
  too, and the filter view lets tab commands fall through).
  File ▸ Open, the Recent list and drag-and-drop replace an
  untouched empty tab and otherwise open a new tab;
  File ▸ New Tab / Close Tab / New Window / Open in New
  Window complete the menu. (5) Closing: a dirty tab prompts
  with the existing Save / Don't Save / Cancel prompt (a
  dirty parked tab is brought to the front to be asked
  about); closing the last tab leaves a fresh empty
  document; the window close asks about every dirty
  document in turn and Cancel aborts it. (6) Multiple
  windows: the program has no single-instance design at all
  (investigated: no mutex, no forwarding anywhere), so a
  new window is a separate process — New Window and Open in
  New Window start the program's own exe again, with the
  file as its argument for the latter; the Explorer entry
  and command-line arguments open in a new process exactly
  as they always have (nothing to regress, nothing faked).
  (7) Session restore: settings gain `session_tabs` /
  `session_active`; a clean exit stores the open tabs'
  paths and the active index, and the next launch reopens
  them (missing or unreadable files skipped silently — the
  restore plan probes each path by opening its mapping),
  unless the launch was given a file, which takes
  precedence; untitled documents are not persisted (a dirty
  never-saved document keeps the delta-recovery path).
  The Guide gains a tabs section, and the shortcuts list
  names the new actions. Nineteen new host tests
  (397 → 416): the tab-order invariants across a scripted
  open/activate/close/reorder run and a full close-out to
  zero, next/previous wrap-around, parked-index round
  trips, width fitting (untouched fits, water-filling,
  the minimum floor, degenerate inputs), the session
  mapping incl. unnamed-document skipping and the active
  index rule, restore planning (missing files skipped,
  saved order kept, active index following the nearest
  surviving earlier tab), duplicate paths kept, the
  settings session round trip, and the tab actions'
  default chords (including Ctrl+Tab parsing).
  - Gate, run locally on this tree: `cargo fmt --check`
    clean (exit 0). Host `cargo test`: **416 passed;
    0 failed; 2 ignored**. `cargo clippy --all-targets --
    -D warnings` clean (exit 0, host) and
    `cargo clippy --target x86_64-pc-windows-gnu
    --all-targets -- -D warnings` clean (exit 0).
    `cargo check --target x86_64-pc-windows-msvc
    --all-targets` clean (exit 0). Release build
    (`cargo build --release --target x86_64-pc-windows-gnu`,
    exit 0): `VeloEdit.exe` 1 278 464 bytes, SHA-256
    `8af9513a0c359f8c5bd4c3b8fb1024f0fecf6222c9857ade9694333a07fc616c`
    (R4c build: 1 240 576 bytes — the growth is the tab
    shell, now linked in; the binary's strings were checked
    for the new action names and the `session_tabs` key to
    prove the shipped program is the gated build).
    Toolchain note: the MinGW linker was again missing on
    this machine (VM replaced) and was reinstalled from apt
    before the build — the configured mirror stalled
    repeatedly, so the package lists and packages were
    fetched from archive.ubuntu.com through a temporary
    apt source override; the system sources were not
    changed.
  - Honest status: the tab cores, the settings session
    list and the keybinding additions are host-verified;
    the GUI wiring (the DocState swap, the strip, session
    restore in the window) is compile-verified on both
    Windows targets only, and Windows runtime behaviour
    stays **Unverified** until the field checks on a real
    machine. Two stated limits (also in PARITY): the
    on-disk crash-recovery record still covers only the
    active document (the record predates tabs — one record,
    rewritten for whichever document is active, as document
    switches behaved before tabs), and the settings-backed
    view toggles (`word_wrap`, `follow_tail`, `freeze_lines`)
    remain program-wide rather than per-tab.

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
  before it is read. R3b-1 added a stamp-based notice (checked when
  the window gains focus and immediately before a save, with a
  Reload / Ignore bar) — that covers the common editor-versus-editor
  case, but it is not a watcher: a page already mapped can still
  fault before the next check runs.
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
- No horizontal scrolling in the editor; long lines clip at the
  window edge unless View ▸ Word Wrap is on (R4b) — rows are capped
  at 4096 displayed chars either way.
