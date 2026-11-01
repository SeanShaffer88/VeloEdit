# VeloEdit — Roadmap

The planned completion route for the feature and engineering backlog.
Items are grouped into slices in dependency order: quick wins first,
then the engine, then features that build on the engine, then the
large interface projects. Each item keeps the number it was proposed
under. Status is stated plainly; nothing here is described as done
until it has shipped in a release.

## Slice R1 — quick wins (folds into v1.0.1)

- **5. Disk space pre-allocation check** — before an atomic save,
  query free space (`GetDiskFreeSpaceExW`); if less than 1.05× the
  file size is free, say so immediately instead of failing halfway
  through a multi-gigabyte write.
- **45. Exit code standardization** — `0` success / match found,
  `1` no match (find), `2` I/O or syntax error, applied across the
  headless tools and documented in `--help`.
- **46. CLI output encoding** — the console the tools borrow is set
  to UTF-8 (code page 65001) so output is never mangled by a legacy
  console code page.
- **6. Configurable undo bounds** — undo history sized by a step
  limit and a byte budget (defaults 1,024 steps / 256 MB), settable
  in `data/settings.json`; replaces the single hard-coded cap.
- **21. Column ruler** — an optional vertical guideline at column
  80, 120, or a custom column, toggled from the View menu.
- **44. Headless extraction** — `VeloEdit extract --start N
  --length M --out FILE` carves a byte range out of a file by
  streaming, with decimal or `0x` numbers.
- **35. Save selection to file** — write the current selection
  straight to a new file, streamed, with no clipboard limits.
- **26/27. Two more themes + safer accents** — a high-contrast
  theme and a monochrome e-ink theme join the library (20 total),
  and the accent picker gains colour-blind-safe presets
  (Okabe–Ito blue, orange, sky).

## Slice R2 — the engine (v1.1)

- **2. Piece-table rebalancing** — the deferred B-tree piece index
  from the specification, plus automatic coalescing of tiny pieces,
  keeping lookup and traversal at O(log n) under heavy typing.
- **3. Parallel line pre-indexing** — the sparse line index is
  completed on background threads with word-parallel scanning
  (64-bit words tested for `\n`/`\r` in parallel), so deep jumps
  become exact without blocking the window.
- **8. Faster streaming search** — the same word-parallel technique
  applied to needle search through the mapped file.
- **11. Selection-scoped search** — find and replace can be limited
  to the current selection or a byte range.
- **12. Hex masked search** — byte patterns with wildcards
  (`4A ?? FF 00 2B`) in hex mode.
- **23. Sort and deduplicate** — line sort / dedup inside a selected
  range.
- **36. File splitter** — split a large file into fixed-size parts,
  streamed, from the CLI and the window.
- **38. Explorer context menu (opt-in)** — "Open in VeloEdit" on
  the per-user registry menu, installed and removed by explicit
  commands; no shell extension DLL, no admin rights.
- **50. Benchmark harness** — a workflow that builds synthetic
  sparse files (1 GB / 10 GB / 100 GB) on a Windows runner and
  measures cold start, open time, working set, and save integrity,
  so the published performance figures are regression-tested.

## Slice R3 — features on the engine (v1.1)

- **20. Bookmarks** — named anchors at byte offsets
  (Ctrl+F2 toggles, F2 / Shift+F2 jump), persisted per file.
  Bookmarks grow colour tags and free-text notes (annotations);
  they stay in the portable store, not in sidecar files next to
  the document, so read-only folders and the portable layout keep
  working.
- **10 + 24. Background find-all + scrollbar tick map** — matches
  are populated on a worker thread and drawn as ticks in the custom
  scrollbar track, alongside bookmark and edited-region ticks.
  Later the strip gains hover previews (line number and a text
  glimpse at that depth).
- **15. Data inspector** — a docked panel reading the bytes at the
  caret as int8–64 (signed/unsigned), float, double, hex, binary,
  and Unix timestamp.
  Also reads a GUID, a Windows FILETIME timestamp, and a short
  UTF-8 string preview of the bytes at the caret.
- **13/14. Hex layout options** — 8/16/24/32 bytes per row or
  fit-to-window, and grouping by 1/2/4/8 bytes in either byte order.
- **16. Relative offsets** — the offset column can count from the
  caret or a user-set base instead of the file start.
- **7. Delta-log recovery** — recovery files store the piece-table
  state and edit log (kilobytes) plus the original file's hash and
  timestamp, replacing full-content snapshots and their 16 MB cap.
- **34. External-change notice** — if the file changes on disk, a
  non-modal bar offers Reload or Ignore; never a surprise prompt.
- **31/32. Line-ending and BOM control** — convert CRLF ↔ LF and
  add/remove the byte-order mark as part of saving, streamed.
- **37. File hashes** — MD5, SHA-1, and SHA-256 computed by
  streaming (hand-written, no dependencies), with a compare field.
  (BLAKE3 is not included: a from-scratch implementation is out of
  proportion for this program's dependency rules.) The hash set
  grows CRC32 and xxHash64 checksums, and hashing gains a
  background mode that fills the status bar in on a low-priority
  thread while work continues.

## Slice R4 — reach (v1.2)

- **1. Mapping windows** — for files beyond comfortable address
  space, an LRU cache of sliding mapped windows (512 MB–4 GB)
  replaces the single whole-file map.
- **4. Fast save by block cloning (opt-in)** — on filesystems that
  support it, `FSCTL_DUPLICATE_EXTENTS_TO_FILE` clones the file and
  patches only the changed extents; the streamed atomic save stays
  the default and the fallback.
- **18. Log filter mode** — temporarily show only lines matching a
  query, over the line index, without creating files.
  Any filtered line jumps back into the full file at its exact
  byte offset with one click.
- **19. Soft wrap** — view-bounded wrapping computed for visible
  rows only.
- **22. Follow mode ("Ghost Tail")** — watch the file for appends
  with `ReadDirectoryChangesW` and tail new bytes live, extending
  the line index and the mapping as the file grows, without
  disturbing the current position, selection, or mode.
- **29. Extended code pages** — Shift-JIS, GBK, EUC-KR, and
  ISO-8859 viewing and save-back through the Win32 conversion APIs.
- **30. Invalid-byte tokens** — malformed sequences render as
  `[0xFE]`-style tokens in text mode instead of replacement marks,
  and control characters (NUL, CR, LF, ESC) render as small glyph
  tags inline rather than invisibly.
- **33. Protected-file saving** — a lock badge for read-only or
  protected-path files and a plain-words "Save as administrator"
  path through an elevated copy of the program.
- **48. Rebindable keys** — every shortcut defined in
  `data/keybindings.json`, with Vim / VS Code / Notepad++ presets.
- **43. Structural diff** — `--diff` grows unified block-level
  output for text and binary files, still streaming.

- **Slicing export (extends 44)** — carve several non-contiguous
  byte or line ranges (for example lines 1,000,000–1,050,000 and
  84,000,000–84,020,000) into one new file, streamed in order with
  near-zero working memory.

- **Entropy strip** — a one-pixel-wide lane beside the scrollbar
  shading each sampled chunk by Shannon entropy / null density,
  computed on a background thread, so packed, encrypted, textual,
  and zero regions are visible at a glance and clickable.
- **Bit-level hex tooling** — toggle a single bit, byte-swap
  (BSWAP), apply AND / OR / XOR / NOT masks, and shift bits across
  any selection in hex mode, all as ordinary undoable edits.
- **Delta patch export** — write the session's edits as a small
  patch file instead of a full copy (the program's own patch
  format and IPS first; VCDIFF as the standard-format follow-on),
  built on the delta recovery format.
- **Zero-run insert** — insert any number of zero bytes, or resize
  a range, as a single piece-table node with no storage behind it
  until a save streams it out.
- **Pointer following + jump history** — read the 32/64-bit value
  at the caret as a target offset, Ctrl+Click to jump there, and
  Alt+Left / Alt+Right to walk the jump history.
- **Strings pane** — a background scan lists ASCII and UTF-16
  strings (configurable minimum length) in a side drawer; clicking
  one jumps to its offset.
- **Delimiter grid mode** — CSV / TSV / pipe files align into
  visual columns computed for the visible lines only.
- **ANSI colour rendering** — terminal escape sequences in raw
  logs (build output, container logs) paint as colours; the bytes
  themselves are never changed.
- **Time-delta gutter** — timestamps detected on visible log lines
  show the elapsed gap to the previous entry in the gutter, so
  stalls and freezes stand out.
- **Freeze pane** — pin the first lines (a header row or a log
  preamble) to the top of the view while the rest scrolls.
- **Focus dimming** — rows outside the current selection or match
  set dim toward black, for long auditing sessions.

## Slice R5 — large projects and open decisions (v1.3+)

- **39. Tabs and multiple windows** — several documents in one
  workspace; the largest single interface project on this list.
- **28. Text/hex split view** — both representations side by side,
  synchronised by offset.
- **Side-by-side streaming diff** — two large files compared
  chunk by chunk over their memory maps in locked-scroll panes
  (text or hex), differences painted directly on the surface; no
  full-file loads. A lighter sync-scroll compare (same locked
  panes, no difference computation) comes first.
- **Multi-cursor and rectangular editing** — column/rectangular
  selections and synchronised multi-point insert/overwrite across
  hex columns and tabular text.
- **Disassembly gutter** — a small built-in, table-driven
  length-accurate decoder (x86-64 first, ARM64 later) rendering
  instruction text beside the bytes in hex mode. Scoped tightly:
  correct instruction boundaries over cleverness.
- **9. Streaming regular expressions** — a backtracking-free regex
  engine that matches across piece boundaries in bounded memory.
- **17. Binary templates** — a plain-text struct definition
  language (for example `PE_HEADER { u32 signature; u16 machine;
  ... }`) kept in `data/templates/`, mapped over offsets for
  header and packet inspection, field names echoed in the status
  bar.
- **40. Taskbar jump list** — recent and pinned files on the
  Windows taskbar icon.
- **47. Screen-reader support (scoped)** — UI Automation exposure
  of the current line and caret offset for Narrator/NVDA.
- **25. Wide-glyph pass** — audit and fix grid alignment for CJK
  and other wide glyphs in the text canvas.

### Decisions recorded

- **49. Plugins — external tools instead.** A WASM or Lua runtime
  would break the program's one-dependency rule and multiply its
  size. The planned alternative is an external-tools protocol:
  user programs receive a selection or range on standard input and
  return replacement bytes on standard output. No plugin engine
  will be added to the core.
- **42. `.com` wrapper — not needed.** The requirement (terminal
  launches behaving like a console program) is already met by the
  GUI-subsystem binary attaching to the parent console (v1.0.1).
- **Direct I/O bypass — not applicable.** `FILE_FLAG_NO_BUFFERING`
  governs handle-based reads; a memory-mapped editor's reads go
  through the cache manager by design, so there is no unbuffered
  path to switch to. The streaming tools already read sequentially
  without extra buffering.
- **Raw device paths — not planned.** Crash-dump and disk-image
  *files* open today like any other file. Raw volume/device access
  (`\\.\…`) needs administrator rights and sits outside the
  file model the whole editor is built on.
- **41. Borderless mode — not planned; snap layouts already work**
  because the window keeps the standard Windows frame. A frameless
  window would trade working snap, resize, and accessibility for
  appearance.
