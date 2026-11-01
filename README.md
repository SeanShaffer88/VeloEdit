# VeloEdit

Zero-dependency, portable binary and large-file editor written in Rust.

## TL;DR

VeloEdit opens huge files — even over 100 GB — almost instantly and lets
you edit them without your computer slowing down. Think of a normal editor
like photocopying a whole book before you may fix one typo: it loads the
entire file into memory first. VeloEdit instead leaves the file where it is
on disk, looks at only the page you are viewing, and keeps your changes as
a small list of sticky notes saying "this part comes from the original,
this part is new". That is why it aims to open a 100 GB file in under a
tenth of a second while using less than 35 MB of memory. It is one portable
program file for Windows 11: no installer needed, no internet, no accounts,
nothing sent anywhere.

**Current status: foundation build (0.1.0, unreleased).** The editing core
and the command-line tools described below are built and tested. The
graphical editor window is not built yet — see "Known limitations".

## Features

Working in this foundation build:

- **Piece-table editing core** — inserts and deletes rearrange a list of
  small pieces pointing at the original bytes and an append-only buffer;
  file data itself is never copied or shifted while editing.
- **Sparse line index** — line checkpoints every 64 KB make jumping to a
  line fast without scanning the whole file.
- **Hex arithmetic** — hex rows map to byte offsets with O(1) maths, and
  scrollbar positions use 64-bit ratios so they stay exact on files far
  past 2 billion bytes.
- **Undo and redo** — every edit is stored as a small reversible delta and
  can be undone or redone without touching the stored bytes.
- **Headless binary diff** — compare two files of any size from the
  command line in constant memory and get a JSON report.
- **Text encodings** — detection, decoding, and encoding for UTF-8,
  UTF-16 LE/BE, and ANSI (Windows-1252), with byte-order mark (BOM)
  detection. Malformed input is reported with its exact position, never
  silently repaired.
- **Search and replace (core)** — find streams over the piece table
  without loading the document into one buffer, and finds matches that
  span the seams between pieces. Replace and replace-all each count as
  a single undo step. From the command line, `--find` searches a file
  headlessly and reports match offsets as JSON.
- **Portable cleanup** — `--clean` removes only the `data/` folder next to
  the program file, and `--dry-run` shows what would happen first.

Confirmed for v1, scheduled next (not in this build):

- The graphical editor: pure Win32 with Direct2D/DirectWrite rendering,
  per-monitor DPI scaling, virtualized scrolling, and the red-and-black
  theme from the specification. Search/replace and the encoding display
  get their windows then — in this build they are core operations plus
  the headless `--find` command.
- Memory-mapped file opening on Windows and atomic save (write to a
  staging file, flush, then swap it into place so a crash mid-save can
  never corrupt the original). (Editing itself is byte-based, so binary
  files and hex mode work regardless of encoding.)

## Requirements

- Windows 11, 64-bit (the released program).
- The foundation build's core library and command-line tools also compile
  and pass their tests on Linux, which is how they are developed; the
  editor itself is a Windows program.
- To build from source: Rust 1.80 or newer.

## Install

No release has been published yet. When releases begin, they will ship as:

- `VeloEdit-v<version>-x64-Setup.exe` — installer
- `VeloEdit-v<version>-x64-Setup.msi` — enterprise installer
- `VeloEdit-v<version>-x64-Portable.zip` — unzip anywhere and run
  `VeloEdit.exe`; all settings live in the `data/` folder inside the same
  folder as the program
- `SHA256SUMS.txt` — checksums for verifying the downloads

## How to use it (foundation build)

The foundation build is headless: it provides the diff tool and the other
command-line operations. Run these from a terminal in the folder
containing `VeloEdit.exe`:

1. Compare two files and save a JSON report:

   ```bash
   VeloEdit.exe --diff "A.bin" "B.bin" --output "report.json" --headless
   ```

   Exit code 0 means the files are identical; 3 means they differ; the
   report lists both sizes, the first differing byte offset, and how many
   bytes differ.

2. Compare and print the report instead of saving it:

   ```bash
   VeloEdit.exe --diff "A.bin" "B.bin" --headless --json
   ```

3. Search a file without opening it and get the match positions as JSON:

   ```bash
   VeloEdit.exe "notes.txt" --find "TODO" --headless
   ```

   This prints `{"count":2,"offsets":[0,31]}`-style output (byte
   offsets in the file) and exits with code 0 whether or not anything
   matches. The encoding is detected from the file's first 64 KB; force
   it with `--encoding utf8`, `utf16le`, `utf16be`, or `ansi`.

4. See everything the program can do from the command line:

   ```bash
   VeloEdit.exe --help
   VeloEdit.exe --version
   ```

5. Remove the portable data folder (with a safe preview first):

   ```bash
   VeloEdit.exe --clean --dry-run
   VeloEdit.exe --clean
   ```

Opening a file for graphical editing uses the forms below; in this
foundation build they print a short message that the graphical editor is
not included yet, and exit with code 2:

```bash
VeloEdit.exe "C:\Logs\dump.bin" --line 10420
VeloEdit.exe "C:\Memory\core.dmp" --mode hex --read-only
```

## Command line reference

| Option | What it does |
| --- | --- |
| `<file>` | File to open |
| `--line <N>` | Open at line N |
| `--mode <hex\|text>` | Open in hex or text mode (default: text) |
| `--read-only` | Open without allowing edits |
| `--diff <A> <B>` | Compare two files byte by byte |
| `--find <TEXT>` | Find TEXT in the file; prints match count and byte offsets as JSON (exit 0 even with no matches) |
| `--encoding <NAME>` | Force the file encoding for `--find`: `utf8`, `utf16le`, `utf16be`, or `ansi` (default: detect) |
| `--output <PATH>` | Write the diff report JSON to a file |
| `--headless` | Run without opening a window |
| `--clean`, `--uninstall` | Remove the adjacent `data/` folder only |
| `--dry-run` | Show what an operation would do, without doing it |
| `--json` | Print results as JSON |
| `--silent` | Print nothing on success |
| `--verbose`, `--debug`, `--trace` | Print extra detail |
| `--help`, `--version` | Show help or the version |

Exit codes: 0 success (diff: identical), 1 error, 2 graphical editor
requested but not part of this build, 3 files differ (diff only).

## Configuration

VeloEdit has no settings file in the foundation build. When the graphical
editor lands, all configuration and state will live in the `data/` folder
next to the program file — never in `%APPDATA%` or the registry — so the
program stays fully portable.

## Building from source

```bash
cargo build --release
```

For a Windows target check from another host:

```bash
cargo check --target x86_64-pc-windows-gnu --all-targets
```

Windows release builds link the C/C++ runtime statically (`.cargo/config.toml`
sets `crt-static` for the MSVC target), so the program needs no
redistributables installed.

## Project structure

```text
Cargo.toml            Package definition; windows-sys is the only dependency
PROJECT_RULES.md      Binding project rules, budgets, and coverage floors
QUALITY.md            Quality scorecard with evidence
CHANGELOG.md          Change history
docs/SPEC.md          The complete engineering specification (governs design)
docs/PARITY.md        Operation registry: core API, CLI form, GUI form
src/lib.rs            Library root and the operation (op_*) surface
src/piece_table.rs    Memory-mapped piece table engine
src/line_index.rs     Sparse 64 KB line checkpoint index
src/hexview.rs        Hex row and 64-bit scrollbar arithmetic
src/undo.rs           Undo/redo engine (inversion deltas)
src/encoding.rs       Encoding detection, decode, and encode (UTF-8, UTF-16, ANSI)
src/search.rs         Streaming find and single-undo replace over the piece table
src/diff.rs           Streaming binary diff and its JSON report
src/cli.rs            Hand-written argument parsing and headless commands
src/main.rs           Thin program entry point
```

## Tests

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

The test suite covers insert/delete/coalescence across piece boundaries,
undo/redo restoring exact piece layouts, line checkpoints on synthetic
documents with known line layouts, scrollbar maths at sizes past 2^32, diff
results across chunk boundaries, command-line parsing, and the safety
guard on `--clean`. It also covers encoding detection (byte-order marks,
unmarked UTF-16, samples cut mid-character), lossless ANSI round trips of
all 256 byte values, strict UTF-16 error positions, searches whose needle
spans two or three pieces, and replace-all undoing as one step to the
exact original layout.

## Troubleshooting

- **Exit code 2 when opening a file** — expected in this build: the
  graphical editor is not included yet; the file argument was understood.
- **`--diff` reports exit code 1** — one of the two files could not be
  opened; the error message names the problem (for example a missing
  file). Check both paths.
- **`--clean` refuses to remove the data folder** — it only ever removes a
  folder literally named `data` directly next to the program file, and
  refuses when that path is a file instead of a folder. Nothing is deleted
  in that case.

## Security and privacy

- Works 100% offline. There is no network code in the program.
- Zero telemetry: nothing about your files or usage is collected or sent.
- Your data stays in the file you are editing and, later, the adjacent
  `data/` folder. Nothing is written to `%APPDATA%`, `%LOCALAPPDATA%`,
  `%PROGRAMDATA%`, or the registry.

## Screenshots

Screenshots: to be added from a real run once the graphical editor exists.
No placeholder images are used.

## Known limitations

- No graphical editor window yet (foundation build); opening a file exits
  with code 2 after printing a short explanation.
- Files are not yet memory-mapped on Windows in this build: the core works
  on in-memory or length-only originals behind the seam the mmap backing
  will use, so the 100 GB / <100 ms open targets are specification goals,
  not measured results yet.
- Piece storage is a `Vec`; the weight-balanced B-tree from the
  specification is a planned optimisation, so lookups are linear in the
  number of pieces for now.
- The line-index scan is a plain chunked scan; the SIMD-accelerated scan
  from the specification is planned behind the same function.
- Atomic save is not implemented yet; nothing in this build writes to your
  files at all.
- The undo history size (1024 edits in this build) is a placeholder:
  the final cap is to be confirmed.
- Performance targets (cold start, memory caps, frame time) are
  specification targets and are unverified until measured on Windows.

## License

MIT — see `LICENSE`.
