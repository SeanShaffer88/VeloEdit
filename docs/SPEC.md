# VeloEdit: Complete Workflow Blueprint & Engineering Specification

**VeloEdit** is a zero-dependency, portable Windows 11 64-bit binary and large-file editor written in Rust. It functions 100% offline with zero telemetry, uses pure Win32 and Direct2D/DirectWrite for UI rendering, and employs a memory-mapped piece table architecture capable of opening 100+ GB files in milliseconds using less than 35 MB of RAM.

---

## 1. System Architecture & Core Pipelines

```
 ┌───────────────────────────────────────────────────────────────┐
 │                   Win32 Application Shell                     │
 │   - Per-Monitor V2 DPI Aware Window (WndProc loop)            │
 │   - Native Direct2D / DirectWrite Hardware Render Target      │
 │   - Custom Virtualized Scroll & Selection Engine              │
 └──────────────┬──────────────────────────────▲─────────────────┘
                │ User Inputs / View Requests   │ Direct Draw Calls
                ▼                              │ (Visible Viewport)
 ┌───────────────────────────────────────────────────────────────┐
 │                   Core Editor Coordinator                     │
 │   - Line Offset Indexer (SIMD-accelerated newline scan)       │
 │   - Viewport Slice Generator (Row-bounded chunk provider)     │
 │   - Undo / Redo Command History Engine                        │
 └──────────────┬──────────────────────────────▲─────────────────┘
                │ Buffer Mutations / Lookups   │ Span Byte Slices
                ▼                              │
 ┌───────────────────────────────────────────────────────────────┐
 │            Memory-Mapped Piece Table Engine                   │
 │   - Original Buffer: Read-Only Mmap (`CreateFileMappingW`)   │
 │   - Append Buffer: Read-Write Dynamic Heap (`Vec<u8>`)        │
 │   - Piece Table Structure: Balanced Interval Tree / B-Tree    │
 └──────────────┬──────────────────────────────▲─────────────────┘
                │ Low-Priority IO Sync         │ Lazy Page Faults
                ▼                              │
 ┌───────────────────────────────────────────────────────────────┐
 │                      Disk Subsystem                           │
 │   - Windows NT Paging File & OS Virtual Memory Manager        │
 │   - Atomic Replacement Output Pipeline (`ReplaceFileW`)       │
 └───────────────────────────────────────────────────────────────┘

```

### 1.1 Architectural Pillars

* **Strict Static Compilation:** Statically linked against the C/C++ runtime (`crt-static`, `/MT`) with direct `windows-sys` FFI bindings. Zero reliance on external MSVC redistributables or managed runtimes.
* **Bounded Resource Footprint:** Idle CPU strictly <0.1%, idle baseline working set <15 MB, active 100+ GB buffer operations capped below 35 MB RAM.
* **Zero UI Latency:** Rendering pipeline runs on Direct2D hardware acceleration, decoupling file traversal from the primary message dispatch loop to guarantee frame delivery in under 16 ms.

---

## 2. Low-Level Memory Model & Buffer Representation

### 2.1 Mmap + Piece Table Design

Rather than loading file contents directly into contiguous RAM buffers, VeloEdit maps the source file as an immutable read-only view and routes modifications through an append-only append buffer.

```rust
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferSource {
    Original = 0,
    Add = 1,
}

#[derive(Clone, Copy, Debug)]
pub struct PieceNode {
    pub source: BufferSource,
    pub source_offset: u64,
    pub length: u64,
}

pub struct PieceTable {
    pub original_file_size: u64,
    pub append_buffer: Vec<u8>,
    pub pieces: Vec<PieceNode>, // Production target: Weight-balanced 2-3 B-Tree
    pub total_length: u64,
}

```

* **Zero-Copy File Ingestion:** Source files are bound via `CreateFileW` (`GENERIC_READ`, `FILE_SHARE_READ`), `CreateFileMappingW` (`PAGE_READONLY`), and `MapViewOfFile`. The OS loads pages lazily via hardware faults; no upfront disk read loop occurs.
* **Append Buffer Isolation:** Inlined edits, character inserts, and multi-byte injections append directly to the process heap. No existing byte array is shifted or reallocated during editing operations.
* **Piece Node Slicing:** A deletion splits a target piece into two bounds, reducing intermediate spans without copying memory. Insertions bisect target nodes and insert a references entry pointing into the heap append buffer.

### 2.2 Piece Table B-Tree Weight Management

* **Offset Tree Indexing:** Each piece stores its own byte length. The internal B-Tree index maintains subtree lengths, enabling $O(\log N)$ point lookups, insertions, and splits.
* **Fragment Coalescence:** Sequential contiguous inserts append to the active node if `piece.source == BufferSource::Add` and `piece.source_offset + piece.length == append_buffer.len()`, eliminating node fragmentation during live typing.

---

## 3. SIMD Virtualized Viewport Engine

### 3.1 Line Offset & Viewport Resolution

To display 100+ GB text files without scanning the entire file upfront:

* **Sparse Line Indexing:** A secondary thread performs an asynchronous, SIMD-accelerated (AVX2 / SSE4.2 `_mm256_cmpeq_epi8`) newline discovery pass, building a sparse index of line offset checkpoints every 64 KB.
* **On-Demand Line Traversal:** When the user navigates to an arbitrary vertical coordinate, the viewport resolver finds the nearest preceding checkpoint in $O(\log M)$ time, seeks to the offset in the mapped space, and scans forward only the exact number of lines needed to saturate the visible screen height.
* **Hex / Binary Virtualization:** When running in hex view mode, line offset calculation is deterministic:

$$\text{Byte Offset} = \text{Row Index} \times \text{Bytes Per Row}$$



Row indexing executes in $O(1)$ arithmetic time without requiring any pre-computed index.

### 3.2 Dual-View Virtual Scrolling

```
 ┌───────────────┬──────────────────────────────────────────┬──────────────────┐
 │ Address (Hex) │ Raw Bytes (00-0F)                        │ ASCII Decoded    │
 ├───────────────┼──────────────────────────────────────────┼──────────────────┤
 │ 000000000000  │ 4D 5A 90 00 03 00 00 00 04 00 00 00 FF FF │ MZ..........ÿÿ   │
 │ 000000000010  │ 00 00 B8 00 00 00 00 00 00 00 40 00 00 00 │ ..¸.......@...   │
 └───────────────┴──────────────────────────────────────────┴──────────────────┘

```

* **Native Virtual Scrollbar:** Driven by `SetScrollInfo` with a 64-bit normalized ratio translation. Because standard Win32 32-bit integer scroll ranges truncate past 2 billion units, VeloEdit computes scroll thumb displacement using 64-bit fixed-point ratios:

$$\text{Thumb Position} = \left\lfloor \frac{\text{Current Offset}}{\text{Total Length}} \times \text{Track Range} \right\rfloor$$



---

## 4. Win32 & Direct2D Rendering Pipeline

### 4.1 Rendering Loop Subsystem

* **Device Context Lifecycle:** Binds `ID2D1HwndRenderTarget` to the client `HWND`. Device loss (`D2DERR_RECREATE_TARGET`) triggers an atomic cache purge and rebuild of the render context.
* **DirectWrite Typography:** Uses `IDWriteFactory` to instantiate fixed-pitch glyph runs via `Consolas` or `Cascadia Code`. Subpixel anti-aliasing is enabled with clear-type geometry snapping.
* **Red & Black Theme Palette:**
* Background Canvas: `#0E0E10` (Deep obsidian black)
* Secondary Gutter / Panels: `#16161A`
* Active Cursor & Selection Accent: `#E02424` (Crimson red)
* Primary Text Font: `#E6E6E6` (Off-white mono)
* Muted Metadata / Offsets: `#7A7A85`
* Boundary Lines & Splitters: `#2A2A30`



### 4.2 Win32 Event Integration

* **`WM_PAINT` / `WM_DISPLAYCHANGE`:** Enforces zero frame stutter during window resizes through double-buffered Direct2D swap chaining.
* **`WM_MOUSEWHEEL`:** Translates wheel ticks directly into line increments using `SPI_GETWHEELSCROLLLINES`.
* **`WM_DPICHANGED`:** Intercepts Per-Monitor V2 DPI updates, extracts the recommended window rectangle from `lParam`, calls `SetWindowPos`, and scales DirectWrite text layouts dynamically without bitmap stretching.

---

## 5. Non-Destructive Mutations & Atomic Disk Sync

### 5.1 Atomic Persistence Pipeline

To prevent data loss, corrupted saves, or torn writes across 100+ GB targets:

1. **Target Staging:** Construct an adjacent staging file in the target directory using the pattern `Filename.ext.tmp_velo_[PID]`.
2. **Streaming Serialization:** Iterate through the piece table nodes sequentially. For each node, stream its byte slice directly into the temporary target using `WriteFile` in 4 MB block chunks.
3. **Hardware Flush:** Issue `FlushFileBuffers(hTempFile)` to force all modified disk sectors through the drive's onboard write cache.
4. **Close Handlers:** Terminate open read mappings to the original file via `UnmapViewOfFile` and `CloseHandle`.
5. **Atomic Swap:** Execute `ReplaceFileW`:
```rust
unsafe {
    ReplaceFileW(
        target_path_wide.as_ptr(),
        temp_path_wide.as_ptr(),
        backup_path_wide_or_null,
        REPLACEFILE_IGNORE_MERGE_ERRORS,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
}

```


6. **Remap:** Immediately re-establish read-only mappings to the updated destination file and reset the append buffer.

### 5.2 Transactional Undo/Redo Engine

* **Command Stack Invariants:** Mutations exist as lightweight inversion deltas:
```rust
pub enum EditAction {
    Insert { offset: u64, length: u64 },
    Delete { offset: u64, previous_pieces: Vec<PieceNode> },
}

```


* **Memory Constrained History:** Undo operations restore previous piece table node trees directly without altering the underlying data residing in the append buffer.

---

## 6. Zero-Dependency Windows Packaging & Verification

### 6.1 Strict Asset Manifest

* **Setup Installer:** `VeloEdit-v1.0.0-x64-Setup.exe` (Self-extracting, embedded payload)
* **Enterprise MSI:** `VeloEdit-v1.0.0-x64-Setup.msi` (Native WiX v4 build)
* **Zero-Install Portable:** `VeloEdit-v1.0.0-x64-Portable.zip` (Contains standalone `VeloEdit.exe` and localized `data/` sandbox folder)
* **Checksum Manifest:** `SHA256SUMS.txt` accompanying all releases.

### 6.2 Portability & Sandboxing Standard

* When running in portable mode, all configuration, state, and user settings reside exclusively in an adjacent `data/` directory.
* No data or registry values are ever written to `%APPDATA%`, `%LOCALAPPDATA%`, or `%PROGRAMDATA%`.

---

## 7. CLI Interfaces & Headless Mode

```bash
# Launch GUI targeting specific file at line offset
VeloEdit.exe "C:\Logs\dump.bin" --line 10420

# Launch GUI directly into hex inspection mode
VeloEdit.exe "C:\Memory\core.dmp" --mode hex --read-only

# Headless binary diff and reporting (Zero UI window launched)
VeloEdit.exe --diff "A.bin" "B.bin" --output "report.json" --headless

# Display help and version metadata completely offline
VeloEdit.exe --help

```

---

## 8. Verification & Performance Benchmarks

| Metric | Target Specification | Validation Method |
| --- | --- | --- |
| **Cold Startup Time** | $< 50\text{ ms}$ | High-resolution tick counting via `QueryPerformanceCounter` |
| **Initial File Ingestion (100 GB)** | $< 100\text{ ms}$ | `MapViewOfFile` timing from launch to first UI frame |
| **Idle Baseline Working Set** | $< 15\text{ MB}$ | OS Process Query via `GetProcessMemoryInfo` |
| **Active Editing Memory Cap** | $< 35\text{ MB}$ | Continuous monitoring under heavy piece table mutations |
| **Direct2D Render Latency** | $< 16\text{ ms}$ | Internal telemetry timing between `BeginDraw` and `EndDraw` |
| **Atomic Save Integrity** | Zero data corruption | Hard abort simulation (`SIGKILL`) during save operations |