# VeloEdit — Benchmarks & §8 Evidence

This file records what has actually been measured for the SPEC §8
targets, and what is still **Unverified**. Every number below comes from
commands run on 2026-10-31 in the Slice 5 packaging pass; the commands
are printed so the runs can be repeated. Nothing is estimated: where a
metric cannot be measured on this machine, it says Unverified and names
the exact Windows measurement the specification requires.

## Measurement context (host proxies)

- Machine: Linux VM, kernel 7.0.0-39-generic, AMD EPYC 9D25 (4 cores
  visible), 8 GB RAM. Fixtures were generated on the disk-backed home
  volume (`/tmp` is a small tmpfs that cannot hold three 200 MB files —
  the first attempt failed there with "No space left on device").
- Binary: the **host release build** (`cargo build --release`,
  `target/release/VeloEdit`, 350,536 bytes) — not the Windows build.
  These are host-side proxies for the core's speed only; they are not
  the §8 Windows measurements.
- Fixtures: two 200 MB files (209,715,200 bytes each) of repeated log
  lines. File A carries one `TODO` marker per 1 MB block (200 matches,
  first at byte offset 40). File B is A with 4 bytes changed at offset
  100,000,000. Timings are process wall time (`time.perf_counter`
  around each run), median of the stated run count.

## Measured here (host proxies)

| Check | Command | Result |
| --- | --- | --- |
| Executable size (Windows target) | `cargo build --release --target x86_64-pc-windows-gnu`, then size/SHA-256 of `target/x86_64-pc-windows-gnu/release/VeloEdit.exe` | **552,448 bytes**, SHA-256 `48d0847d4af69471da0d2126b150234b06fd8162a6cd8dd21854eeb23517e11b` (gnu-target link proof; the release binary is the MSVC static-CRT build) |
| Binary diff, 200 MB vs 200 MB | `VeloEdit --diff A B --headless --output report.json` (exit 3 = differ) | runs 336.7 / 328.3 / 330.4 ms, **median 330.4 ms**. Report verified: `{"equal":false,"len_a":209715200,"len_b":209715200,"first_diff_offset":100000000,"diff_byte_count":4,"extra_bytes":0}` |
| Find over 200 MB | `VeloEdit A --find TODO --headless` (exit 0) | runs 1390.9 / 1072.8 / 1059.7 ms, **median 1072.8 ms**. Output verified: `"count":200`, first offsets 40, 1048616, 2097192, … (one per 1 MB block) |
| Atomic save-copy, 200 MB | `VeloEdit A --save-copy C` (exit 0) | runs 456.9 / 596.5 / 465.8 ms, **median 465.8 ms**; the copy is exactly 209,715,200 bytes (staging write + flush + atomic swap included) |
| Cold-start proxy | 20 runs of `VeloEdit --version` | **median 1.07 ms**, min 0.98 ms, max 1.96 ms. This is process start + argument parse + version print on Linux — a floor-level proxy only, **not** the §8 cold-start measurement (no window, no QueryPerformanceCounter) |

## SPEC §8 metrics — status

| Metric | SPEC target | Status | How it will be measured (SPEC §8 method) |
| --- | --- | --- | --- |
| Cold startup time | < 50 ms | **Unverified** | On Windows 11: high-resolution tick counting via `QueryPerformanceCounter` from process launch to first frame. Host proxy above suggests ample headroom, but that is not evidence for this metric |
| Initial file ingestion (100 GB) | < 100 ms | **Unverified** | On Windows 11: `MapViewOfFile` timing from launch to first UI frame with a real 100+ GB file opened through the mmap backing |
| Idle baseline working set | < 15 MB | **Unverified** | On Windows 11: OS process query via `GetProcessMemoryInfo` on the idle GUI process |
| Active editing memory cap | < 35 MB | **Unverified** | On Windows 11: `GetProcessMemoryInfo` monitored continuously under heavy piece-table mutations |
| Direct2D render latency | < 16 ms | **Unverified** | On Windows 11: internal timing between `BeginDraw` and `EndDraw` in the GUI render loop |
| Atomic save integrity | Zero data corruption | **Unverified on Windows** | On Windows 11: hard-abort simulation (killing the process, `SIGKILL`-equivalent) during a save, then verifying the target is either the complete old file or the complete new one. On this host, the save pipeline's success, replace, zero-length, staging-failure and swap-failure cases are covered by unit tests, and the Unix-hard-abort case is not among them |
| Idle CPU | < 0.1 % (SPEC §1.1) | **Unverified** | On Windows 11: Task Manager / performance counters on the idle GUI process |

## The benchmark harness (roadmap item 50)

`scripts/Run-Benchmarks.ps1` automates the measurements above so
they can be repeated on any Windows machine: it creates a sparse
test file with `fsutil` at a `-SizeGB` parameter (default 1), plants
a few text markers, then times with a stopwatch — cold launch to
`--help`, a `--find` over the file, a `--save-copy`, and an
`extract` — reporting each process's peak working set as a plain
table. Two cautions are built into the script's own header: the
sparse fixture costs almost no disk, but the save-copy step writes a
*real* full-size copy, so the disk needs about `-SizeGB` of free
space for the run; and the numbers are evidence for this file, not a
pass/fail gate.

`.github/workflows/benchmarks.yml` is **manual only**
(`workflow_dispatch`): it runs the same gate as the release
workflow, builds the release executable, runs the harness at 1 GB
on `windows-latest`, and uploads the table as a build artifact.
The 10 GB and 100 GB runs are local, on a suitable machine (a
GitHub runner's disk is not the place for a 100 GB save-copy). The
§8 metrics in the table above keep their individual statuses —
the harness is how the Windows numbers will be produced.

## Staged open — the instant-open fix (2026-11-01, host proxies)

Opening used to build the whole line index before the window
could answer anything: the first line question ran the full-file
scan on the spot. Measured on the owner's Windows 11 machine, a
10 GB file took 23 seconds to open. The open path is now staged:
open indexes only the first 4 MB (enough for the first screens),
and a background thread finishes the index in 32 MB steps using
the same parallel scanner, publishing snapshots the window picks
up on its normal timer. Every position stays exact at every
stage — lines past the frontier resolve by a forward scan from
the last checkpoint — and the finished index is checkpoint-
identical to the one-shot build.

Measured here with the ignored release-mode bench tests
(`cargo test --release --lib bench_ -- --ignored --nocapture`)
over an in-memory 1,000,000,000-byte source of 10,000,000
100-byte log lines (Linux host, release test build — host
proxies only, **not** §8 Windows measurements):

| Check | Result |
| --- | --- |
| One-shot full build (the old open path) | 3,112.4 ms (~321 MB/s) |
| Head prefix only (first 4,194,304 bytes — all an open now waits for) | 4.89 ms |
| Staged full build in 32 MB steps (background completion) | 2,045.9 ms |
| Editor staged open, first line question answered | 32.81 ms (prefix count 41,944 lines, frontier 4,194,304 bytes) |

The honest reading: what an open waits for fell from the full
3.1-second scan to a 4 MB head build — the editor-level 32.81 ms
includes opening the source and seeding that head region. A true
before/after of the shipped binary cannot be re-measured here
(the old code is gone); the one-shot build above is the old
path's cost, run through the same scanner the old open used.
All §8 metrics in the table above keep their status: the
Windows open time for a real 100+ GB file remains **Unverified**
until measured on Windows 11.

## Packaging artifact proofs (Slice 5, this machine)

These prove the packaging scripts produce correctly shaped artifacts.
They are **not** release assets: the payload everywhere is the
windows-gnu link-proof exe, while releases are built from the MSVC
static-CRT binary by `.github/workflows/release.yml` on a Windows
runner.

| Artifact | How produced | Result |
| --- | --- | --- |
| NSIS Setup exe | `makensis -DEXE_PATH=<…/x86_64-pc-windows-gnu/release/VeloEdit.exe> -DOUT_DIR=/tmp/velo-nsis-proof installer/VeloEdit.nsi` (makensis 3.09) | `/tmp/velo-nsis-proof/VeloEdit-v1.0.0-x64-Setup.exe`, **398,798 bytes**, SHA-256 `7e4185453d8a0e3d68f0a78a344d3cd66821e8d2212fb5d3f1d33a83c8d51e17`, identified as a Nullsoft Installer self-extracting archive. Script-only proof: the installer has not been *run* on Windows |
| Portable zip | Python `zipfile` staging of exe + LICENSE + `data/README-data.txt` | `/tmp/VeloEdit-v1.0.0-x64-Portable.zip`, **275,252 bytes**, SHA-256 `2639f43d07770e0160ab8d9898934d8ec95afcb0692fb04e53dda5d95e3d7bc2`; entries: `VeloEdit.exe` (552,448), `LICENSE` (1,069), `data/README-data.txt` (435) |
| WiX MSI | — | **Not built here**: WiX cannot build an MSI on Linux. `installer/Package.wxs` is authored and is built for the first time by the release workflow on its Windows runner |
