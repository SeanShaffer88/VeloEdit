# VeloEdit — Quality Scorecard

Version scored: 0.1.0, foundation + Slice 2 (core library + headless CLI).
No GUI, no Windows memory-mapping, and no atomic save exist in this slice,
so dimensions that depend on them are marked **Unverified**, not scored up.
Scores are 0–5 per dimension. Any dimension marked Fail blocks "done".

## Scope note

Foundation slice scope: project rule sheet, this scorecard, the operation
registry (`docs/PARITY.md`), the governing spec (`docs/SPEC.md`), the
piece-table / line-index / hex / undo core, the streaming binary diff, and
the hand-written CLI with the V2.0 global flags.

Owner-confirmed v1 features that are **not** built yet and therefore not
scored yet: the Win32 + Direct2D interface, Windows mmap backing, and the
atomic save pipeline. (Search/replace and text encodings — UTF-8, UTF-16
LE/BE, ANSI, with BOM detection — landed in Slice 2 and are covered by
the gate record below.)

## Scorecard

| Dimension | Score | Evidence |
| --- | --- | --- |
| Architecture & foundation | 4 | Frozen module layout in `PROJECT_RULES.md`; original-bytes seam (`OriginalBuffer`) lets the Windows mmap backing slot in without API change; B-tree target documented where the Vec stands in |
| Correctness (core operations) | 4 | Insert/delete/coalescence, multi-piece reads, undo/redo layout restoration, line checkpoints, hex/scroll math, diff counting, encoding detection/decode/encode, boundary-spanning search, and single-undo replace-all are all covered by unit tests that exercise the real code paths |
| Tests | 4 | 94 unit tests (55 foundation + 39 from Slice 2), host gate fully green (see below). Coverage percentage not yet measured with a coverage tool — the 85% core / 70% overall floors are asserted by test breadth, not yet by a measured number |
| Reliability & data safety | 3 | Undo never touches the append buffer; deletes return their removed pieces; `--clean` is guarded to the adjacent `data/` folder and tested. The atomic save safety path (SPEC §5.1) does not exist yet — it must reach 100% coverage before it ships |
| Performance (SPEC §8 budgets) | Unverified | No benchmark harness yet and no Windows build has run: cold start <50 ms, 100 GB open <100 ms, <15 MB idle / <35 MB active, <16 ms frame are targets, not results |
| Security & privacy | 4 | 100% offline, zero telemetry by construction (no network code at all); `#![forbid(unsafe_code)]` in the core; the only dependency crate is `windows-sys`, gated to `cfg(windows)` |
| Accessibility & UI | Unverified | No UI exists in this slice. UI work is blocked on a confirmed wireframe (V2.0 design gate) |
| Documentation | 4 | README follows the full checklist with honest status, `docs/PARITY.md` registry, `docs/SPEC.md` governing blueprint, this scorecard; screenshots are owed from a real Windows run once a UI exists |
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

## Known gaps (honest list)

- Piece storage is a `Vec`; SPEC §2.2's weight-balanced B-tree is a later
  slice, so point lookups are O(N) in the number of pieces for now.
- The line-index scan is a plain chunked scan; the SIMD inner loop
  (SPEC §3.1) replaces `count_newlines_in` later without API change.
- Undo history cap (1024 entries) is a documented placeholder; the
  blueprint requires memory-constrained history but names no number.
- Windows runtime behaviour of every kind is Unverified until the program
  runs on a real Windows 11 machine.
