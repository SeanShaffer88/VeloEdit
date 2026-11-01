//! The Windows GUI shell: one Win32 window over the platform-independent
//! core (SPEC.md sections 3, 4 and 7).
//!
//! Everything testable lives in the core modules ([`crate::editor`],
//! [`crate::theme`], [`crate::settings`], [`crate::search`],
//! [`crate::encoding`]); this module is deliberately a thin shell that
//! translates Win32 messages into editor operations and paints what the
//! editor returns. It is one of only three places in the crate allowed
//! to contain `unsafe` (see the crate-level unsafe policy), and every
//! block carries a `// SAFETY:` note.
//!
//! Rendering is Direct2D + DirectWrite as SPEC.md section 4 mandates.
//! windows-sys 0.61 does not ship Direct2D/DirectWrite bindings (its
//! Graphics features stop at Gdi/GdiPlus/Dwm/Hlsl/OpenGL/Printing), and
//! the crate rule allows no other dependency, so the handful of COM
//! interfaces used here are declared by hand below: fixed vtable slot
//! numbers taken from the Windows SDK headers (`d2d1.h`, `dwrite.h`),
//! where the ABI has been stable since Windows 7. Only slots actually
//! called are given signatures; the rest of each vtable is never touched.
//! GDI is used only for window *chrome* (the status strip and find bar
//! colours, owner-drawn toolbar buttons) — never for the editor surface.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    GlobalFree, COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect,
    FrameRect, GetStockObject, GetTextExtentPoint32W, InvalidateRect, LineTo, MoveToEx, Rectangle,
    ScreenToClient, SelectObject, SetBkColor, SetBkMode, SetTextColor, UpdateWindow, HBRUSH, HDC,
    HGDIOBJ, HPEN, NULL_BRUSH, OPAQUE, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetOpenFileNameW, GetSaveFileNameW, CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT,
    CHOOSECOLORW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTTOOLINFOW,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::cli::{CliOptions, Mode as CliMode};
use crate::editor::{Editor, EditorMode};
use crate::encoding;
use crate::hexview;
use crate::line_index::{LineIndex, LineIndexBuilder};
use crate::mmap_win::MmapFile;
use crate::piece_table::PieceTable;
use crate::save::SaveOptions;
use crate::search::{self, SearchOptions};
use crate::settings::{Settings, DEFAULT_FONT_SIZE};
use crate::theme::{Rgba, Theme, ACCENT_PRESETS};

// ---------------------------------------------------------------------------
// Command ids (menus, toolbar buttons and find-bar buttons share them, so a
// menu item and its toolbar button can never drift apart).
// ---------------------------------------------------------------------------

const CMD_OPEN: usize = 1001;
const CMD_SAVE: usize = 1002;
const CMD_SAVE_AS: usize = 1003;
const CMD_EXIT: usize = 1004;
const CMD_SAVE_SELECTION: usize = 1005;
const CMD_SPLIT_FILE: usize = 1006;
const CMD_EXPLORER_MENU: usize = 1007;
const CMD_CHANGE_RELOAD: usize = 1008;
const CMD_CHANGE_IGNORE: usize = 1009;
// File Hashes dialog (roadmap item 37).
const CMD_FILE_HASHES: usize = 1010;
const CMD_UNDO: usize = 1011;
const CMD_REDO: usize = 1012;
const CMD_CUT: usize = 1013;
const CMD_COPY: usize = 1014;
const CMD_PASTE: usize = 1015;
const CMD_FIND: usize = 1016;
const CMD_REPLACE: usize = 1017;
const CMD_SELECT_ALL: usize = 1018;
const CMD_SORT_LINES: usize = 1019;
const CMD_DEDUP_LINES: usize = 1020;
const CMD_MODE_TEXT: usize = 1021;
const CMD_MODE_HEX: usize = 1022;
const CMD_MODE_TOGGLE: usize = 1025;
const CMD_FONT_BIGGER: usize = 1023;
const CMD_FONT_SMALLER: usize = 1024;
const CMD_VIEW_RULER: usize = 1026;
// Data inspector panel toggle (roadmap item 15).
const CMD_INSPECTOR: usize = 1027;
// Relative offsets (roadmap item 16).
const CMD_REL_OFFSETS: usize = 1028;
const CMD_SET_OFFSET_BASE: usize = 1029;
const CMD_GUIDE: usize = 1031;
const CMD_ABOUT: usize = 1032;
const CMD_FIND_NEXT: usize = 1041;
const CMD_FIND_PREV: usize = 1042;
const CMD_DO_REPLACE: usize = 1043;
const CMD_DO_REPLACE_ALL: usize = 1044;
const CMD_FIND_CLOSE: usize = 1045;
const CMD_FIND_SCOPE: usize = 1046;
const CMD_THEME_BASE: usize = 3000; // + theme index (20 themes)
const CMD_ACCENT_BASE: usize = 3100; // 0 = theme default, 1..=11 presets, then Custom…
const CMD_ACCENT_CUSTOM: usize = CMD_ACCENT_BASE + 1 + ACCENT_PRESETS.len();
const CMD_RECENT_BASE: usize = 3200; // + recent index (max 8)
                                     // View ▸ Hex Columns radio group (roadmap item 13): indices into
                                     // HEX_COLS_MENU in order — 8, 16, 24, 32, 64, Fit to Window.
const CMD_HEXCOLS_BASE: usize = 3300;
const HEX_COLS_MENU: [u32; 6] = [8, 16, 24, 32, 64, 0];
// View ▸ Hex Grouping radio group (roadmap item 14): group sizes
// 1, 2, 4, 8; plus the big-endian display toggle.
const CMD_HEXGROUP_BASE: usize = 3320;
const CMD_HEX_ENDIAN: usize = 3330;
// File ▸ Save Options radio groups (roadmap items 31/32): line
// endings Keep / LF / CRLF, then BOM Keep / Add / Remove.
const CMD_SAVE_EOL_BASE: usize = 3340;
const CMD_SAVE_BOM_BASE: usize = 3360;

const ID_FIND_EDIT: usize = 2001;
const ID_REPLACE_EDIT: usize = 2002;
const ID_FIND_COUNT: usize = 2003;

const TIMER_BLINK: usize = 1;
const TIMER_TICK: usize = 2;

/// Recovery *content snapshots* are only written for documents with
/// no backing file, and only up to this size. Documents backed by a
/// file use the delta recovery (`data/recovery2.bin`, roadmap item
/// 7) at any size instead — it stores the edit log plus the original
/// file's fingerprint, never a copy of the contents.
const RECOVERY_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Documents larger than this populate a plain find's match list on
/// a background worker (roadmap items 10 + 24) instead of scanning
/// synchronously on the UI thread; the worker walks the document in
/// windows of this size too.
const BG_FIND_THRESHOLD: u64 = 4 * 1024 * 1024;
const BG_FIND_WINDOW: u64 = 4 * 1024 * 1024;

/// The background line-index thread publishes a fresh snapshot to
/// its slot after every step of this many newly indexed bytes, so
/// the tick can extend the editor's indexed prefix and advance the
/// status readout's "Indexing NN%" while the build runs.
const INDEX_PUBLISH_STEP: u64 = 32 * 1024 * 1024;

/// One staged line-index publication: how far the background build
/// has got, plus the index snapshot at that frontier (partial until
/// the last one, whose snapshot is complete). Delivered through
/// `App::index_slot`, tagged with the spawn epoch — the same slot
/// discipline as the background find's progress.
#[derive(Clone)]
struct IndexProgress {
    indexed_bytes: u64,
    snapshot: LineIndex,
}

// Layout constants (in DIPs; scaled to physical pixels for child windows).
const TOOLBAR_H: f32 = 30.0;
/// The find bar row matches the toolbar row's height, so the find
/// bar's controls come out the same size as the toolbar buttons
/// (both rows place their controls at `row height − 6` DIPs).
const FINDBAR_H: f32 = 30.0;
/// Height of the external-change notice bar (roadmap item 34) when
/// it is showing, directly under the toolbar.
const CHANGEBAR_H: f32 = 30.0;
const STATUS_H: f32 = 22.0;
const TEXT_PAD: f32 = 6.0;
/// Accent strip along the very top of the client area (design mockup).
const STRIP_H: f32 = 2.0;
/// Width of the custom-drawn scrollbar track at the editor's right edge.
const SCROLLBAR_W: f32 = 10.0;
/// Width of the data inspector panel (roadmap item 15) while it is
/// docked open on the editor's right side.
const INSPECTOR_W: f32 = 280.0;
/// Smallest scrollbar thumb height, so huge files keep a grabbable thumb.
const SCROLL_MIN_THUMB: f32 = 24.0;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// NUL-terminated UTF-16 for a Rust string.
fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A Rust string from a NUL-terminated UTF-16 buffer (dialog results).
fn string_from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Text of a window (find/replace edits).
unsafe fn window_text(hwnd: HWND) -> String {
    // SAFETY: `hwnd` is one of our child windows; the length query and
    // the read use the standard two-call pattern with our own buffer.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let read = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if read <= 0 {
            return String::new();
        }
        buf.truncate(read as usize);
        String::from_utf16_lossy(&buf)
    }
}

/// Fill selected 1-px edge strips of `rect` with `brush`. The
/// segmented Text / Hex pair shares one outer frame with a single
/// divider, so its edges are drawn strip by strip instead of two
/// `FrameRect`s (which would double the shared edge).
fn draw_frame_edges(
    hdc: HDC,
    rect: &RECT,
    brush: HBRUSH,
    top: bool,
    bottom: bool,
    left: bool,
    right: bool,
) {
    let strips = [
        (
            top,
            RECT {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.top + 1,
            },
        ),
        (
            bottom,
            RECT {
                left: rect.left,
                top: rect.bottom - 1,
                right: rect.right,
                bottom: rect.bottom,
            },
        ),
        (
            left,
            RECT {
                left: rect.left,
                top: rect.top,
                right: rect.left + 1,
                bottom: rect.bottom,
            },
        ),
        (
            right,
            RECT {
                left: rect.right - 1,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            },
        ),
    ];
    for (on, strip) in strips {
        if on {
            // SAFETY: GDI fill on the caller's button DC; the strip
            // is a plain value inside the button's own rect.
            unsafe { FillRect(hdc, &strip, brush) };
        }
    }
}

/// Draw one toolbar glyph (polish pass) in the `size` × `size` box
/// at (`x`, `y`) on a button's DC, in `colour`. The buttons are
/// owner-drawn through GDI (the `WM_DRAWITEM` pass draws their
/// labels with `DrawTextW`), so the glyphs are GDI too: one pen in
/// the label colour plus the hollow stock brush, keeping every
/// shape an outline. Simple geometry only — folder (Open), floppy
/// (Save), curved arrows (Undo / Redo, a nine-segment semicircle
/// polyline, mirrored by arrowhead placement), magnifier (Find),
/// and two opposed arrows (Replace).
fn draw_toolbar_icon(hdc: HDC, id: usize, x: i32, y: i32, size: i32, colour: COLORREF) {
    // A hairline at the reference size, thickening as the box
    // grows so high-DPI buttons keep the same visual weight.
    let pen_width = (size / 13).max(1);
    // SAFETY: plain GDI object juggling on the button's own DC;
    // the pen is created here and deleted before returning, the
    // stock hollow brush is borrowed and never deleted, and the
    // previously selected objects are restored.
    unsafe {
        let pen: HPEN = CreatePen(PS_SOLID, pen_width, colour);
        let old_pen = SelectObject(hdc, pen as HGDIOBJ);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        // One straight segment on the same DC.
        let seg = |x1: i32, y1: i32, x2: i32, y2: i32| {
            MoveToEx(hdc, x1, y1, std::ptr::null_mut());
            LineTo(hdc, x2, y2);
        };
        let s = size;
        match id {
            CMD_OPEN => {
                // Folder: a tab stepping up on the top-left, over a
                // body outline.
                let top = y + s * 35 / 100;
                seg(x + 1, top, x + 1, y + s * 22 / 100);
                seg(x + 1, y + s * 22 / 100, x + s * 45 / 100, y + s * 22 / 100);
                seg(x + s * 45 / 100, y + s * 22 / 100, x + s * 58 / 100, top);
                Rectangle(hdc, x + 1, top, x + s - 1, y + s - 1);
            }
            CMD_SAVE => {
                // Floppy: the outline, the shutter slot across the
                // top, and the label window at the bottom.
                Rectangle(hdc, x + 1, y + 1, x + s - 1, y + s - 1);
                Rectangle(
                    hdc,
                    x + s * 30 / 100,
                    y + 1,
                    x + s * 74 / 100,
                    y + s * 38 / 100,
                );
                Rectangle(
                    hdc,
                    x + s * 28 / 100,
                    y + s * 60 / 100,
                    x + s * 72 / 100,
                    y + s - 1,
                );
            }
            CMD_UNDO | CMD_REDO => {
                // A top semicircle as a polyline (no reliance on
                // Arc's direction convention); the arrowhead sits
                // at the end the action points to.
                let cx = x + s / 2;
                let cy = y + s / 2 + s * 14 / 100;
                let rx = s / 2 - 2;
                let ry = s / 2 - 2;
                let mut prev: Option<(i32, i32)> = None;
                for i in 0..=8 {
                    let t = std::f32::consts::PI * i as f32 / 8.0;
                    let px = cx + (rx as f32 * t.cos()).round() as i32;
                    let py = cy - (ry as f32 * t.sin()).round() as i32;
                    if let Some((ox, oy)) = prev {
                        seg(ox, oy, px, py);
                    }
                    prev = Some((px, py));
                }
                let h = (s * 28 / 100).max(2);
                if id == CMD_UNDO {
                    seg(cx - rx, cy, cx - rx + h, cy - h);
                    seg(cx - rx, cy, cx - rx + h, cy + h);
                } else {
                    seg(cx + rx, cy, cx + rx - h, cy - h);
                    seg(cx + rx, cy, cx + rx - h, cy + h);
                }
            }
            CMD_FIND => {
                // Magnifier: a circle with a handle running
                // down-right from its rim.
                let d = s * 62 / 100;
                Ellipse(hdc, x + 1, y + 1, x + 1 + d, y + 1 + d);
                seg(x + 1 + d * 3 / 4, y + 1 + d * 3 / 4, x + s - 1, y + s - 1);
            }
            CMD_REPLACE => {
                // Two horizontal arrows pointing opposite ways.
                let h = (s * 22 / 100).max(2);
                let y1 = y + s * 35 / 100;
                let y2 = y + s * 68 / 100;
                seg(x + 1, y1, x + s - 1, y1);
                seg(x + s - 1, y1, x + s - 1 - h, y1 - h);
                seg(x + s - 1, y1, x + s - 1 - h, y1 + h);
                seg(x + s - 1, y2, x + 1, y2);
                seg(x + 1, y2, x + 1 + h, y2 - h);
                seg(x + 1, y2, x + 1 + h, y2 + h);
            }
            _ => {}
        }
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        DeleteObject(pen as HGDIOBJ);
    }
}

/// Human-readable byte size for the status bar: exact below 1 KB,
/// one decimal above (for example `104.2 GB`).
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Display name for the status bar's right side.
fn encoding_display(enc: encoding::TextEncoding) -> &'static str {
    match enc {
        encoding::TextEncoding::Utf8 => "UTF-8",
        encoding::TextEncoding::Utf16Le => "UTF-16 LE",
        encoding::TextEncoding::Utf16Be => "UTF-16 BE",
        encoding::TextEncoding::Ansi => "ANSI",
    }
}

/// Line-ending style of a document sample: CRLF when the sample
/// contains a CR LF pair, LF otherwise (the default for new files).
fn detect_eol(sample: &[u8]) -> &'static str {
    if sample.windows(2).any(|pair| pair == b"\r\n") {
        "CRLF"
    } else {
        "LF"
    }
}

/// Scrollbar thumb geometry for the custom-drawn scrollbar: given the
/// track height (DIPs), the total row count, the visible row count and
/// the first visible row, return `(thumb_y, thumb_h)` in DIPs from the
/// track top — or `None` when everything fits and no bar is drawn.
/// The thumb height is the visible fraction of the track, clamped to
/// a grabbable minimum; its position is the first-row fraction of the
/// remaining travel.
fn scrollbar_geometry(
    track_h: f32,
    total_rows: u64,
    visible_rows: u64,
    first_row: u64,
) -> Option<(f32, f32)> {
    if track_h <= 0.0 || total_rows == 0 || visible_rows >= total_rows {
        return None;
    }
    let thumb_h = (track_h * visible_rows as f32 / total_rows as f32)
        .max(SCROLL_MIN_THUMB)
        .min(track_h);
    let max_first = (total_rows - visible_rows) as f32;
    let frac = if max_first > 0.0 {
        (first_row as f32 / max_first).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some((frac * (track_h - thumb_h), thumb_h))
}

/// Inverse of [`scrollbar_geometry`]: the first visible row for a
/// thumb dragged to `thumb_y` DIPs from the track top.
fn scrollbar_first_row(track_h: f32, total_rows: u64, visible_rows: u64, thumb_y: f32) -> u64 {
    if total_rows == 0 || visible_rows >= total_rows {
        return 0;
    }
    let thumb_h = (track_h * visible_rows as f32 / total_rows as f32)
        .max(SCROLL_MIN_THUMB)
        .min(track_h.max(0.0));
    let travel = track_h - thumb_h;
    if travel <= 0.0 {
        return 0;
    }
    let frac = (thumb_y / travel).clamp(0.0, 1.0);
    (frac * (total_rows - visible_rows) as f32).round() as u64
}

/// The scrollbar-track rows (whole DIPs from the track top) that
/// get a tick for the given sorted offsets (roadmap item 24): each
/// offset maps to a row by its fraction of the document —
/// `(offset * rows) / doc_len` computed in u128, the same 64-bit
/// ratio maths the scrollbar thumb uses — and offsets landing on a
/// row already in the list collapse into it. Pure, so the
/// bucketing is unit-tested; the draw pass turns rows into 1-DIP
/// lines.
fn tick_rows(offsets: &[u64], doc_len: u64, rows: u64) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    if offsets.is_empty() || doc_len == 0 || rows == 0 {
        return out;
    }
    // Bound the walk on pathological match counts: past a few
    // thousand offsets every row is spoken for anyway, so
    // stride-sample the rest.
    let stride = (offsets.len() as u64 / 8192).max(1) as usize;
    let mut i = 0;
    while i < offsets.len() {
        let row = (offsets[i] as u128 * rows as u128 / doc_len as u128) as u64;
        if out.last() != Some(&row) {
            out.push(row);
        }
        i += stride;
    }
    out
}

/// Theme colour as a GDI COLORREF (0x00BBGGRR) for window chrome.
fn colorref(c: crate::theme::Rgba) -> COLORREF {
    (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16)
}

/// Blend two theme colours in plain sRGB space: `t` is the weight
/// of `b` (0 = all `a`, 1 = all `b`). The polish pass derives its
/// two chrome tones this way — the current-line band (background
/// toward panel) and the brighter gutter numbers (muted toward
/// text) — so no theme grows new fields for them.
fn blend_rgba(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let mix = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Rgba::rgb(mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b))
}

/// The editor zoom as a status-bar percentage: the settings'
/// default font size is 100%, everything else scales from it.
fn zoom_percent(font_size: u16) -> u32 {
    (font_size as u32 * 100 + DEFAULT_FONT_SIZE as u32 / 2) / DEFAULT_FONT_SIZE as u32
}

/// The plain-words built-in guide (F1 / Help ▸ Guide). Kept short and
/// jargon-free on purpose; the README carries the same content.
const GUIDE_TEXT: &str = "\
VeloEdit — a quick guide\r\n\
\r\n\
WHAT IT DOES\r\n\
VeloEdit opens and edits very large files without loading them into\r\n\
memory. A 100 GB file opens as fast as a small one, and the program\r\n\
stays light while you work. It also works as a hex editor for looking\r\n\
at a file's raw bytes.\r\n\
\r\n\
THE TWO MODES\r\n\
Text mode is for reading and typing, like a plain notepad. Hex mode\r\n\
shows the raw bytes: each row shows bytes as numbers plus the same\r\n\
bytes as characters on the right. Press F6 (or View ▸ Hex/Text) to\r\n\
switch. Typing a hex digit (0-9, A-F) in hex mode changes the byte\r\n\
under the caret.\r\n\
Hex rows can show 8, 16, 24, 32 or 64 bytes, or fit themselves to\r\n\
the window (View ▸ Hex Columns). View ▸ Hex Grouping displays the\r\n\
bytes in pairs, fours or eights; Big-Endian Groups shows each group\r\n\
reversed, the way a stored number reads. None of this changes the\r\n\
file — it is only how the bytes are shown. View ▸ Relative Offsets\r\n\
counts the row addresses from a place you choose instead of from\r\n\
the start of the file: switch it on to count from the caret, or use\r\n\
Set Offset Base at Caret to move the starting point.\r\n\
\r\n\
KEYS\r\n\
Ctrl+O open · Ctrl+S save · Ctrl+Z undo · Ctrl+Y redo\r\n\
Ctrl+X / Ctrl+C / Ctrl+V cut, copy, paste · Ctrl+A select all\r\n\
Ctrl+F find · Ctrl+H find and replace · F6 switch text/hex\r\n\
F1 this guide · Esc closes the find bar\r\n\
Ctrl+F2 sets or clears a bookmark at the caret\r\n\
F2 / Shift+F2 jump to the next / previous bookmark\r\n\
Ctrl+mouse wheel makes the text bigger or smaller\r\n\
Arrows, Home, End, Page Up and Page Down move the caret; hold Shift\r\n\
to select while you move. You can also drop a file onto the window\r\n\
to open it.\r\n\
\r\n\
SAVING IS SAFE\r\n\
Saving writes the new content to a temporary file first, checks it,\r\n\
and only then swaps it into place, so a crash or power cut cannot\r\n\
leave a half-written file. Undo history is cleared when you save.\r\n\
While you edit a document that lives in a file, VeloEdit also\r\n\
records your edits next to the program — just the edits, plus a\r\n\
fingerprint of the original file, never a copy of the whole\r\n\
file — and offers them back if the program closes unexpectedly.\r\n\
Files of any size are covered this way. An untitled document\r\n\
instead keeps a snapshot of its contents, up to 16 MB.\r\n\
If another program changes the file you are editing, a bar\r\n\
appears under the toolbar when you come back to the window:\r\n\
Reload opens the file from disk (you are asked about your own\r\n\
unsaved changes first), Ignore keeps editing your copy. The\r\n\
notice returns if the file changes again.\r\n\
File ▸ Save Options can convert line endings (to LF or to CRLF)\r\n\
and add or remove the byte-order mark at the start of a text file,\r\n\
every time you save. While a conversion is switched on, the status\r\n\
bar shows it (for example → CRLF or +BOM), so you can see what the\r\n\
next save will do before you save.\r\n\
\r\n\
FINDING MORE\r\n\
Tick \"In selection\" on the find bar to search and replace only\r\n\
inside the part of the document you selected first. In hex mode\r\n\
you can also search for byte patterns with wildcards, like\r\n\
4A ?? FF 00 2B — a ? stands for any nibble, so 4? finds 40 to 4F.\r\n\
Patterns are for finding only; replace does not use them.\r\n\
On files over 4 MB the match count fills in as the search works\r\n\
through the file — the bar reads Counting\u{2026} until it is done,\r\n\
and Find Next already works over the matches found so far.\r\n\
Matches show as small ticks in the scrollbar, and so do\r\n\
bookmarks: Ctrl+F2 marks the caret's place in a file (or clears\r\n\
the mark), F2 and Shift+F2 hop between marks, and a marked line\r\n\
carries a small square in the gutter. Marks are kept per file\r\n\
in the data folder and dropped when the file changes.\r\n\
The Edit menu can sort the lines in a selection, or remove\r\n\
duplicate lines from it. File \u{25B8} Split File Copy\u{2026} writes a\r\n\
large file out as numbered parts (part001, part002, \u{2026}) so it\r\n\
can be moved in pieces. File \u{25B8} Explorer Menu Item adds an\r\n\
\"Open in VeloEdit\" entry to the right-click menu in Explorer;\r\n\
it is off until you turn it on, and turning it off removes it.\r\n\
View ▸ Data Inspector opens a panel that reads the 16 bytes at\r\n\
the caret as numbers, text, a GUID, and dates — handy for picking\r\n\
apart headers and other binary formats. File ▸ File Hashes… works\r\n\
out the MD5, SHA-1 and SHA-256 checksums of the file on disk, and\r\n\
checks any checksum you paste in against them.\r\n\
\r\n\
THEMES\r\n\
View ▸ Theme offers 20 built-in colour themes; View ▸ Accent recolours\r\n\
the highlight colour on top of any theme. Moving through those menus\r\n\
previews each choice in the window — click to keep it, or close the\r\n\
menu to go back to what you had. Your choices are stored in\r\n\
the data folder next to the program — VeloEdit is fully portable and\r\n\
works completely offline.\r\n\
\r\n\
READING THE WINDOW\r\n\
The toolbar buttons carry small icons beside their words, and\r\n\
the Text / Hex switch is one joined control with the side in\r\n\
use filled in the accent colour. If you move through the\r\n\
controls with the keyboard, the control you are on is outlined\r\n\
inside its border in the accent colour, so you can always see\r\n\
where you are. The status bar shows the text size as a\r\n\
percentage beside the file facts — 100% is the standard size.\r\n";

// ---------------------------------------------------------------------------
// Hand-declared Direct2D / DirectWrite COM (see the module note). Vtable
// slots follow the SDK headers; only the slots called are typed.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// IID_ID2D1Factory from d2d1.h.
const IID_ID2D1_FACTORY: Guid = Guid {
    data1: 0x0615_2247,
    data2: 0x6f50,
    data3: 0x465a,
    data4: [0x92, 0x45, 0x11, 0x8b, 0xfd, 0x3b, 0x60, 0x07],
};

/// IID_IDWriteFactory from dwrite.h. (The data4 second byte is 0xe8;
/// an 0xee typo here made DWriteCreateFactory return E_NOINTERFACE in
/// the first field run while Direct2D started fine — the IID test at
/// the bottom of this file pins every byte.)
const IID_IDWRITE_FACTORY: Guid = Guid {
    data1: 0xb859_ee5a,
    data2: 0xd838,
    data3: 0x4b5b,
    data4: [0xa2, 0xe8, 0x1a, 0xdc, 0x7d, 0x93, 0xdb, 0x48],
};

const D2DERR_RECREATE_TARGET: i32 = 0x8899_000Cu32 as i32;

// Vtable slots (0-based, IUnknown included), per d2d1.h / dwrite.h.
// The full tables were re-verified against the SDK-generated winapi
// layouts after the first field run: the render-target slots from
// Clear onward and the text-layout GetMetrics slot were off (Clear is
// preceded by both PushAxisAlignedClip AND PopAxisAlignedClip, and
// IDWriteTextLayout inherits all 25 IDWriteTextFormat methods plus its
// own getters before GetMetrics) — those calls would have painted
// wrongly even once startup succeeded.
const SLOT_RELEASE: usize = 2; // IUnknown::Release — every interface
const SLOT_D2D_CREATE_HWND_RT: usize = 14; // ID2D1Factory::CreateHwndRenderTarget
const SLOT_RT_CREATE_SOLID_BRUSH: usize = 8; // ID2D1RenderTarget::CreateSolidColorBrush
const SLOT_RT_DRAW_RECTANGLE: usize = 16; // ID2D1RenderTarget::DrawRectangle
const SLOT_RT_FILL_RECTANGLE: usize = 17; // ID2D1RenderTarget::FillRectangle
const SLOT_RT_DRAW_TEXT: usize = 27; // ID2D1RenderTarget::DrawText
const SLOT_RT_CLEAR: usize = 47; // ID2D1RenderTarget::Clear
const SLOT_RT_BEGIN_DRAW: usize = 48; // ID2D1RenderTarget::BeginDraw
const SLOT_RT_END_DRAW: usize = 49; // ID2D1RenderTarget::EndDraw
const SLOT_RT_SET_DPI: usize = 51; // ID2D1RenderTarget::SetDpi
const SLOT_RT_RESIZE: usize = 58; // ID2D1HwndRenderTarget::Resize
const SLOT_DW_GET_FONT_COLLECTION: usize = 3; // IDWriteFactory::GetSystemFontCollection
const SLOT_DW_CREATE_TEXT_FORMAT: usize = 15; // IDWriteFactory::CreateTextFormat
const SLOT_DW_CREATE_TEXT_LAYOUT: usize = 18; // IDWriteFactory::CreateTextLayout
const SLOT_FC_FIND_FAMILY_NAME: usize = 5; // IDWriteFontCollection::FindFamilyName
const SLOT_LAYOUT_GET_METRICS: usize = 60; // IDWriteTextLayout::GetMetrics

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D2dColorF {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl From<crate::theme::Rgba> for D2dColorF {
    fn from(c: crate::theme::Rgba) -> Self {
        D2dColorF {
            r: c.r as f32 / 255.0,
            g: c.g as f32 / 255.0,
            b: c.b as f32 / 255.0,
            a: c.a as f32 / 255.0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D2dRectF {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D2dSizeU {
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct D2dRenderTargetProps {
    kind: u32,                // D2D1_RENDER_TARGET_TYPE_DEFAULT = 0
    pixel_format_format: u32, // DXGI_FORMAT_UNKNOWN = 0 (D2D picks)
    pixel_format_alpha: u32,  // D2D1_ALPHA_MODE_UNKNOWN = 0
    dpi_x: f32,
    dpi_y: f32,
    usage: u32,
    min_level: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D2dHwndRenderTargetProps {
    hwnd: HWND,
    pixel_size: D2dSizeU,
    present_options: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DWriteTextMetrics {
    left: f32,
    top: f32,
    width: f32,
    width_including_trailing: f32,
    height: f32,
    layout_width: f32,
    layout_height: f32,
    max_bidi_depth: u32,
    line_count: u32,
}

#[link(name = "d2d1")]
extern "system" {
    fn D2D1CreateFactory(
        factory_type: u32, // D2D1_FACTORY_TYPE_SINGLE_THREADED = 0
        riid: *const Guid,
        options: *const c_void,
        factory: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "dwrite")]
extern "system" {
    fn DWriteCreateFactory(
        factory_type: u32, // DWRITE_FACTORY_TYPE_SHARED = 0
        riid: *const Guid,
        factory: *mut *mut c_void,
    ) -> i32;
}

/// Read the function pointer at vtable `slot` of a COM object.
///
/// # Safety
/// `obj` must be a live COM interface pointer whose vtable has at least
/// `slot + 1` entries, and `T` must be the exact signature the header
/// declares for that slot. Every call site names its slot constant.
unsafe fn vfn<T>(obj: *mut c_void, slot: usize) -> T {
    // SAFETY: a COM object's first word is its vtable pointer; the
    // vtable is an array of function pointers that outlives the object.
    // The caller guarantees the slot exists and `T` matches it; the
    // transmute_copy only reinterprets the pointer-sized slot value.
    unsafe {
        let vtable = *(obj as *const *const usize);
        std::mem::transmute_copy(&*vtable.add(slot))
    }
}

/// Release a COM interface pointer (IUnknown::Release, slot 2).
unsafe fn com_release(obj: *mut c_void) {
    if !obj.is_null() {
        // SAFETY: slot 2 is Release for every COM interface; `obj` is a
        // live interface pointer owned by the caller, released once here.
        let release: unsafe extern "system" fn(*mut c_void) -> u32 =
            unsafe { vfn(obj, SLOT_RELEASE) };
        // SAFETY: as above.
        unsafe { release(obj) };
    }
}

/// Create the Direct2D factory: single-threaded first (the documented
/// fit for a one-UI-thread app), multi-threaded as a fallback. Returns
/// the factory (or null) plus the last attempt's HRESULT, so a startup
/// failure can be reported with its exact code instead of a guess.
fn d2d_create_factory() -> (*mut c_void, i32) {
    let mut last_hr = 0;
    for factory_type in [0u32, 1] {
        let mut factory: *mut c_void = std::ptr::null_mut();
        // SAFETY: the IID is the documented ID2D1Factory GUID; the
        // options pointer may be null; `factory` is a writable
        // out-slot. The returned pointer is checked before use and
        // owned by the caller.
        let hr = unsafe {
            D2D1CreateFactory(
                factory_type,
                &IID_ID2D1_FACTORY,
                std::ptr::null(),
                &mut factory,
            )
        };
        last_hr = hr;
        if hr >= 0 && !factory.is_null() {
            return (factory, hr);
        }
    }
    (std::ptr::null_mut(), last_hr)
}

/// Create the DirectWrite factory: shared first, isolated as a
/// fallback. Same (pointer, last HRESULT) contract as above.
fn dwrite_create_factory() -> (*mut c_void, i32) {
    let mut last_hr = 0;
    for factory_type in [0u32, 1] {
        let mut factory: *mut c_void = std::ptr::null_mut();
        // SAFETY: the IID is the documented IDWriteFactory GUID;
        // `factory` is a writable out-slot; the result is checked and
        // owned.
        let hr = unsafe { DWriteCreateFactory(factory_type, &IID_IDWRITE_FACTORY, &mut factory) };
        last_hr = hr;
        if hr >= 0 && !factory.is_null() {
            return (factory, hr);
        }
    }
    (std::ptr::null_mut(), last_hr)
}

// ---------------------------------------------------------------------------
// GUI state. One instance per window, boxed into GWLP_USERDATA at WM_CREATE
// and freed at WM_NCDESTROY. Only ever touched on the UI thread (the line
// index background thread touches nothing but its own Arc<Mutex<…>> slot).
// ---------------------------------------------------------------------------

/// The ten theme brushes, created lazily against the live render target
/// and released whenever the target or the theme changes.
struct Brushes {
    bg: *mut c_void,
    panel: *mut c_void,
    text: *mut c_void,
    muted: *mut c_void,
    line: *mut c_void,
    gutter: *mut c_void,
    /// The current-line band (polish pass): the theme background
    /// blended halfway toward the panel colour, opaque.
    band: *mut c_void,
    /// Gutter numbers off the current line (polish pass): muted
    /// blended halfway toward the text colour.
    gutter_num: *mut c_void,
    selection: *mut c_void,
    match_bg: *mut c_void,
    accent: *mut c_void,
}

impl Brushes {
    fn release_all(&mut self) {
        for ptr in [
            &mut self.bg,
            &mut self.panel,
            &mut self.text,
            &mut self.muted,
            &mut self.line,
            &mut self.gutter,
            &mut self.band,
            &mut self.gutter_num,
            &mut self.selection,
            &mut self.match_bg,
            &mut self.accent,
        ] {
            if !ptr.is_null() {
                // SAFETY: each brush is a live ID2D1SolidColorBrush owned
                // by this struct, released exactly once here.
                unsafe { com_release(*ptr) };
                *ptr = std::ptr::null_mut();
            }
        }
    }
}

/// Which kind of find produced the editor's current match list —
/// the find bar re-runs the search when the kind changes, even for
/// unchanged text (a pattern can be plain in one mode and masked in
/// another, and a scoped list must never be advanced as a plain one).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FindKind {
    Plain,
    Scoped,
    Masked,
}

/// A live theme/accent menu preview in progress (see WM_MENUSELECT):
/// the committed (theme, accent) snapshot taken when the first
/// previewable item of a menu session is highlighted, plus the
/// (theme, accent) pair the session is currently showing. One backup
/// covers the whole session, even when the user moves straight from
/// the Theme menu into the Accent menu. Previewing never writes
/// settings; a click commits through the ordinary command path,
/// which discards the backup, and closing the menu without a click
/// restores the snapshot.
struct MenuPreview {
    committed_theme: String,
    committed_accent: Option<String>,
    theme: String,
    accent: Option<String>,
}

/// Shared state for the background find-all worker (roadmap items
/// 10 + 24). The worker owns a snapshot of the piece table and walks
/// it window by window, appending each window's matches here under
/// a short lock; the UI thread snapshots the vector on its 200 ms
/// tick. Both generations are re-checked on every touch: a new find
/// bumps `find_gen`, any edit bumps the document generation, and a
/// worker that finds its tags no longer current simply stops —
/// the same epoch discipline as the line-index thread.
struct FindProgress {
    find_gen: u64,
    doc_gen: u64,
    matches: Vec<u64>,
    done: bool,
}

struct App {
    hwnd: HWND,
    editor: Editor,
    settings: Settings,
    theme: Theme,
    read_only_flag: bool,
    d2d: *mut c_void,
    dwrite: *mut c_void,
    target: *mut c_void,
    brushes: Option<Brushes>,
    text_format: *mut c_void,
    font_family: String,
    dpi: u32,
    char_w: f32,
    line_h: f32,
    panel_brush: HBRUSH,
    edit_brush: HBRUSH,
    line_brush: HBRUSH,
    accent_brush: HBRUSH,
    toolbar: Vec<HWND>,
    find_controls: Vec<HWND>,
    find_edit: HWND,
    replace_edit: HWND,
    find_count: HWND,
    find_label: HWND,
    replace_label: HWND,
    /// The find bar's "In selection" toggle (an owner-drawn button,
    /// drawn like the other find-bar buttons; while on it takes the
    /// accent state in the draw pass and a ✓ prefix in its text,
    /// like the Text / Hex mode toggles).
    find_scope_check: HWND,
    /// Whether the find bar is scoped to a selection (roadmap 11).
    find_in_selection: bool,
    /// The selection captured as the active find scope. Kept after a
    /// scoped find selects a match (the selection is then the match,
    /// not the scope), and used by scoped Replace All.
    find_scope_range: Option<(u64, u64)>,
    /// The scope the current match list was computed for; a different
    /// scope forces a re-run instead of an advance.
    last_scope_range: Option<(u64, u64)>,
    last_find_kind: FindKind,
    /// The mapping the current document was opened with, retained so
    /// background workers can snapshot the piece table over the same
    /// bytes. Refreshed after every save (a save remaps the editor
    /// over the file it just wrote); `None` for documents with no
    /// mapped backing (untitled, recovery-built).
    doc_mmap: Option<Arc<MmapFile>>,
    /// Background find-all (roadmap items 10 + 24): the worker's
    /// shared progress slot, and the UI thread's mirror of it. While
    /// `bg_active` is set, the display match list, the count label
    /// and Find Next / Previous run over `bg_matches`, not the
    /// editor's own (synchronous) list.
    find_slot: Arc<Mutex<Option<FindProgress>>>,
    /// Bumped on every new find and every document change, so a
    /// stale worker's results are never picked up.
    find_gen: u64,
    bg_active: bool,
    bg_matches: Vec<u64>,
    bg_done: bool,
    /// Index into `bg_matches` of the selected match, if any.
    bg_current: Option<usize>,
    /// Byte length of the background find's needle (the selected
    /// match's range and the highlight wash need it).
    bg_needle_len: u64,
    /// One-shot: the next plain find runs synchronously even on a
    /// large document. Replace sets this — it needs the editor's
    /// complete match state, so it cancels the background find and
    /// re-runs the find the ordinary way.
    find_sync_once: bool,
    /// Bookmarks (roadmap item 20): the persisted store, loaded at
    /// startup, and the current document's session list.
    bookmark_store: crate::bookmarks::BookmarkStore,
    bookmarks: crate::bookmarks::BookmarkList,
    /// The current file's stamp as it was when the document was
    /// opened — the bookmark store's staleness guard keys on it, so
    /// it is deliberately not refreshed by saves (a saved file has
    /// new bytes; the store drops records whose stamp no longer
    /// matches when the file is next opened).
    doc_stamp: Option<crate::bookmarks::FileStamp>,
    /// The current file's stamp at open / after the last save — the
    /// external-change check's baseline (roadmap item 34).
    file_stamp: Option<crate::bookmarks::FileStamp>,
    /// The external-change notice bar (roadmap item 34) and the
    /// stamps it has already been shown / dismissed for, so it never
    /// nags twice about the same on-disk state.
    change_label: HWND,
    change_reload_btn: HWND,
    change_ignore_btn: HWND,
    change_visible: bool,
    change_shown_for: Option<crate::bookmarks::FileStamp>,
    change_ignored_for: Option<crate::bookmarks::FileStamp>,
    /// Delta recovery (roadmap item 7): the original file's
    /// fingerprint is computed on a background thread at open (it
    /// streams the whole file — far too slow for the open path of a
    /// huge file) and delivered through this slot, tagged with
    /// `doc_epoch`; `doc_fingerprint` is the picked-up value the
    /// recovery writer captures against.
    fingerprint_slot: Arc<Mutex<Option<(u64, crate::recovery::FileFingerprint)>>>,
    doc_epoch: u64,
    doc_fingerprint: Option<crate::recovery::FileFingerprint>,
    status: HWND,
    status_right: HWND,
    /// Line-ending style detected at open ("LF" or "CRLF").
    eol: &'static str,
    /// Custom-scrollbar thumb drag: the pointer's DIP offset from the
    /// thumb's top, while a drag is in progress.
    scroll_drag: Option<f32>,
    help_hwnd: HWND,
    file_menu: HMENU,
    view_menu: HMENU,
    recent_menu: HMENU,
    theme_menu: HMENU,
    accent_menu: HMENU,
    /// The View ▸ Hex Columns / Hex Grouping submenu handles and
    /// the File ▸ Save Options submenu handle (roadmap items 13,
    /// 14, 31, 32), kept for the check-mark sync.
    hex_cols_menu: HMENU,
    hex_group_menu: HMENU,
    save_options_menu: HMENU,
    /// Relative offsets (roadmap item 16): session-only state —
    /// whether the hex address column shows offsets from
    /// `rel_base`, and the base itself (defaults to the caret when
    /// the mode is switched on).
    rel_offsets: bool,
    rel_base: u64,
    find_visible: bool,
    has_focus: bool,
    caret_on: bool,
    dragging: bool,
    wheel_rem: i32,
    /// Ctrl+wheel zoom accumulator (same 120-unit notch maths as
    /// `wheel_rem`, but separate: partial scroll and zoom remainders
    /// must never mix).
    wheel_zoom_rem: i32,
    /// The live theme/accent menu preview session, if one is active.
    menu_preview: Option<MenuPreview>,
    pending_high_surrogate: Option<u16>,
    hex_nibble: Option<u8>,
    index_slot: Arc<Mutex<Option<(u64, IndexProgress)>>>,
    /// The staged line-index build's progress for the status
    /// readout — `(indexed bytes, total bytes)` while a build is
    /// running for the current document, `None` otherwise. The
    /// status bar turns it into "· Indexing NN%" (and suppresses
    /// it whenever the editor already holds a complete index).
    indexing: Option<(u64, u64)>,
    doc_generation: u64,
    open_generation: u64,
    last_edit: Option<Instant>,
    custom_colors: [u32; 16],
    last_find_text: String,
    /// A `--line` target waiting for the background line index.
    pending_goto: Option<u64>,
    /// Incremented on every index-thread spawn (open, startup,
    /// post-save baseline); index threads tag every publication
    /// with the epoch they were spawned for, and the tick drops
    /// publications from older epochs.
    index_epoch: u64,
}

impl App {
    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// DIPs → physical pixels for child-window layout.
    fn px(&self, dip: f32) -> i32 {
        (dip * self.scale()).round() as i32
    }

    /// Physical pixels → DIPs for editor-area maths.
    fn dip(&self, px: i32) -> f32 {
        px as f32 / self.scale()
    }

    /// Note a document mutation: stales the background line index,
    /// restarts the recovery debounce.
    fn note_edit(&mut self) {
        self.doc_generation += 1;
        self.last_edit = Some(Instant::now());
        self.hex_nibble = None;
        // An edit invalidates a background find exactly like the
        // editor's own match list (which the editor clears itself):
        // drop the mirrored matches — and their scrollbar ticks —
        // and bump the generation so the worker's late results are
        // never picked up.
        cancel_bg_find(self);
    }
}

/// The state pointer stored in GWLP_USERDATA, or null when WM_CREATE
/// has not stored it yet (several messages — WM_GETMINMAXINFO among
/// them — arrive before WM_CREATE and must get default handling).
///
/// # Safety
/// `hwnd` must be our main window; the returned pointer is either null
/// or the live Box<App> stored by WM_CREATE.
unsafe fn app_ptr(hwnd: HWND) -> *mut App {
    // SAFETY: reading our own window's user data slot.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App }
}

// ---------------------------------------------------------------------------
// Direct2D / DirectWrite plumbing
// ---------------------------------------------------------------------------

fn make_brush(target: *mut c_void, color: crate::theme::Rgba) -> *mut c_void {
    let mut brush: *mut c_void = std::ptr::null_mut();
    let color = D2dColorF::from(color);
    // SAFETY: slot 8 is ID2D1RenderTarget::CreateSolidColorBrush per
    // d2d1.h; `target` is a live render target, the colour is a plain
    // value, the properties pointer may be null, and the out-slot is
    // writable. The result is null-checked.
    let create: unsafe extern "system" fn(
        *mut c_void,
        *const D2dColorF,
        *const c_void,
        *mut *mut c_void,
    ) -> i32 = unsafe { vfn(target, SLOT_RT_CREATE_SOLID_BRUSH) };
    // SAFETY: as above.
    let hr = unsafe { create(target, &color, std::ptr::null(), &mut brush) };
    if hr < 0 {
        return std::ptr::null_mut();
    }
    brush
}

fn release_target(app: &mut App) {
    if let Some(mut brushes) = app.brushes.take() {
        brushes.release_all();
    }
    if !app.target.is_null() {
        // SAFETY: the render target is owned by the app and released
        // exactly once here (the field is nulled immediately after).
        unsafe { com_release(app.target) };
        app.target = std::ptr::null_mut();
    }
}

/// Create the render target + brushes if they don't exist yet.
fn ensure_target(app: &mut App) {
    if !app.target.is_null() || app.d2d.is_null() {
        return;
    }
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: `app.hwnd` is our window and `client` is a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let size = D2dSizeU {
        width: (client.right - client.left).max(1) as u32,
        height: (client.bottom - client.top).max(1) as u32,
    };
    let props = D2dRenderTargetProps {
        dpi_x: app.dpi as f32,
        dpi_y: app.dpi as f32,
        ..Default::default()
    };
    let hwnd_props = D2dHwndRenderTargetProps {
        hwnd: app.hwnd,
        pixel_size: size,
        present_options: 0,
    };
    let mut target: *mut c_void = std::ptr::null_mut();
    // SAFETY: slot 14 is ID2D1Factory::CreateHwndRenderTarget per
    // d2d1.h; the factory is live, both property structs outlive the
    // call, and the out-slot is writable and null-checked after.
    let create: unsafe extern "system" fn(
        *mut c_void,
        *const D2dRenderTargetProps,
        *const D2dHwndRenderTargetProps,
        *mut *mut c_void,
    ) -> i32 = unsafe { vfn(app.d2d, SLOT_D2D_CREATE_HWND_RT) };
    // SAFETY: as above.
    let hr = unsafe { create(app.d2d, &props, &hwnd_props, &mut target) };
    if hr < 0 || target.is_null() {
        return;
    }
    app.target = target;
    let t = &app.theme;
    app.brushes = Some(Brushes {
        bg: make_brush(target, t.bg),
        panel: make_brush(target, t.panel),
        text: make_brush(target, t.text),
        muted: make_brush(target, t.muted),
        line: make_brush(target, t.line),
        gutter: make_brush(target, t.gutter_bg),
        band: make_brush(target, blend_rgba(t.bg, t.panel, 0.5)),
        gutter_num: make_brush(target, blend_rgba(t.muted, t.text, 0.5)),
        selection: make_brush(target, t.selection),
        match_bg: make_brush(target, t.match_bg),
        accent: make_brush(target, t.accent),
    });
}

/// Pick the first available family from the SPEC.md section 4.2 chain
/// (Consolas, then Cascadia Code, then Courier New) by asking the system
/// font collection — CreateTextFormat alone cannot report a miss.
fn pick_font_family(dwrite: *mut c_void) -> String {
    let mut collection: *mut c_void = std::ptr::null_mut();
    // SAFETY: slot 3 is IDWriteFactory::GetSystemFontCollection per
    // dwrite.h; the out-slot is writable and the result checked.
    let get_collection: unsafe extern "system" fn(*mut c_void, *mut *mut c_void, i32) -> i32 =
        unsafe { vfn(dwrite, SLOT_DW_GET_FONT_COLLECTION) };
    // SAFETY: as above; FALSE = do not check for updates.
    let hr = unsafe { get_collection(dwrite, &mut collection, 0) };
    if hr < 0 || collection.is_null() {
        return "Consolas".to_string();
    }
    let chosen = ["Consolas", "Cascadia Code", "Courier New"]
        .iter()
        .find(|family| {
            let wide = wstr(family);
            let mut index: u32 = 0;
            let mut exists: i32 = 0;
            // SAFETY: slot 5 is IDWriteFontCollection::FindFamilyName
            // per dwrite.h; the name is NUL-terminated and both
            // out-slots are writable.
            let find: unsafe extern "system" fn(
                *mut c_void,
                *const u16,
                *mut u32,
                *mut i32,
            ) -> i32 = unsafe { vfn(collection, SLOT_FC_FIND_FAMILY_NAME) };
            // SAFETY: as above.
            let hr = unsafe { find(collection, wide.as_ptr(), &mut index, &mut exists) };
            hr >= 0 && exists != 0
        })
        .map(|s| s.to_string());
    // SAFETY: the font collection is owned by us, released exactly once.
    unsafe { com_release(collection) };
    chosen.unwrap_or_else(|| "Consolas".to_string())
}

/// (Re)create the text format for the current font size and re-measure
/// the monospace cell from a probe layout.
fn ensure_format(app: &mut App) {
    if !app.text_format.is_null() || app.dwrite.is_null() {
        return;
    }
    if app.font_family.is_empty() {
        app.font_family = pick_font_family(app.dwrite);
    }
    let family = wstr(&app.font_family.clone());
    let locale = wstr("en-us");
    let mut format: *mut c_void = std::ptr::null_mut();
    // SAFETY: slot 15 is IDWriteFactory::CreateTextFormat per dwrite.h;
    // weight 400 (normal), style 0 (normal), stretch 5 (normal); both
    // strings are NUL-terminated and outlive the call; the font
    // collection pointer may be null (system collection).
    let create: unsafe extern "system" fn(
        *mut c_void,
        *const u16,
        *mut c_void,
        u32,
        u32,
        u32,
        f32,
        *const u16,
        *mut *mut c_void,
    ) -> i32 = unsafe { vfn(app.dwrite, SLOT_DW_CREATE_TEXT_FORMAT) };
    // SAFETY: as above.
    let hr = unsafe {
        create(
            app.dwrite,
            family.as_ptr(),
            std::ptr::null_mut(),
            400,
            0,
            5,
            app.settings.font_size as f32,
            locale.as_ptr(),
            &mut format,
        )
    };
    if hr < 0 || format.is_null() {
        return;
    }
    app.text_format = format;
    measure_cell(app);
}

/// Measure the monospace cell (DIPs) from a ten-digit probe layout.
/// Falls back to size-based estimates if measurement fails.
fn measure_cell(app: &mut App) {
    let size = app.settings.font_size as f32;
    app.char_w = size * 0.6;
    app.line_h = size * 1.35;
    if app.dwrite.is_null() || app.text_format.is_null() {
        return;
    }
    let probe = wstr("0123456789");
    let mut layout: *mut c_void = std::ptr::null_mut();
    // SAFETY: slot 18 is IDWriteFactory::CreateTextLayout per dwrite.h;
    // the string and format are live, the out-slot writable.
    let create: unsafe extern "system" fn(
        *mut c_void,
        *const u16,
        u32,
        *mut c_void,
        f32,
        f32,
        *mut *mut c_void,
    ) -> i32 = unsafe { vfn(app.dwrite, SLOT_DW_CREATE_TEXT_LAYOUT) };
    // SAFETY: as above.
    let hr = unsafe {
        create(
            app.dwrite,
            probe.as_ptr(),
            10,
            app.text_format,
            10_000.0,
            1_000.0,
            &mut layout,
        )
    };
    if hr < 0 || layout.is_null() {
        return;
    }
    let mut metrics = DWriteTextMetrics::default();
    // SAFETY: slot 60 is IDWriteTextLayout::GetMetrics per dwrite.h;
    // `metrics` is a writable struct of exactly the header's layout.
    let get_metrics: unsafe extern "system" fn(*mut c_void, *mut DWriteTextMetrics) -> i32 =
        unsafe { vfn(layout, SLOT_LAYOUT_GET_METRICS) };
    // SAFETY: as above.
    let hr = unsafe { get_metrics(layout, &mut metrics) };
    if hr >= 0 && metrics.width > 0.0 && metrics.height > 0.0 {
        app.char_w = metrics.width / 10.0;
        app.line_h = metrics.height;
    }
    // SAFETY: the probe layout is owned by us, released exactly once.
    unsafe { com_release(layout) };
}

/// Drop the text format (font size changed) so it is recreated.
fn drop_format(app: &mut App) {
    if !app.text_format.is_null() {
        // SAFETY: owned by the app, released exactly once here.
        unsafe { com_release(app.text_format) };
        app.text_format = std::ptr::null_mut();
    }
}

// ---------------------------------------------------------------------------
// Layout (all in DIPs; converted to pixels only for child windows)
// ---------------------------------------------------------------------------

struct Layout {
    editor_top: f32,
    editor_bottom: f32,
    width: f32,
    gutter_w: f32,
    /// Right edge of the editor area: the client width minus the
    /// inspector panel while it is open (roadmap item 15). Mouse
    /// maths, the scrollbar, and the content columns all bound to
    /// this; the chrome (accent strip, row washes, status bar) uses
    /// the full `width`.
    editor_right: f32,
}

fn compute_layout(app: &mut App) -> Layout {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let width = app.dip(client.right - client.left);
    let height = app.dip(client.bottom - client.top);
    let top = STRIP_H
        + TOOLBAR_H
        + if app.change_visible { CHANGEBAR_H } else { 0.0 }
        + if app.find_visible { FINDBAR_H } else { 0.0 };
    let gutter_w = match app.editor.mode() {
        EditorMode::Text => {
            let digits = app.editor.line_count().max(1).to_string().len() as f32;
            (digits + 2.0) * app.char_w + TEXT_PAD
        }
        EditorMode::Hex => 18.0 * app.char_w + TEXT_PAD,
    };
    let editor_right = if app.settings.inspector_open {
        (width - INSPECTOR_W).max(gutter_w + TEXT_PAD + SCROLLBAR_W + 40.0)
    } else {
        width
    };
    Layout {
        editor_top: top,
        editor_bottom: (height - STATUS_H).max(top),
        width,
        gutter_w,
        editor_right,
    }
}

/// The fit-to-window hex width for the current window (roadmap
/// item 13): the character cells available to the hex area,
/// counted in cells (the monospace grid makes this exact).
fn fit_hex_width(app: &mut App) -> u64 {
    let layout = compute_layout(app);
    let avail = ((layout.editor_right - SCROLLBAR_W - layout.gutter_w - TEXT_PAD)
        / app.char_w.max(1.0))
    .floor()
    .max(0.0) as usize;
    hexview::fit_bytes_per_row(avail, app.settings.hex_group as usize)
}

/// Push the settings-described hex layout into the editor
/// (roadmap items 13/14), resolving fit-to-window against the
/// current window size. Called on startup, on resize, on document
/// open, and whenever a hex layout menu choice changes.
fn sync_hex_layout(app: &mut App) {
    let fit = if app.settings.hex_cols == crate::settings::HEX_COLS_FIT {
        fit_hex_width(app)
    } else {
        0
    };
    let layout = app.settings.hex_layout(fit);
    if layout != app.editor.hex_layout() {
        app.editor.set_hex_layout(layout);
    }
}

fn page_rows(app: &mut App) -> u64 {
    let layout = compute_layout(app);
    let rows = ((layout.editor_bottom - layout.editor_top) / app.line_h.max(1.0)).floor();
    (rows as u64).max(1)
}

/// Position every child window for the current client size / DPI /
/// find-bar visibility.
fn layout_children(app: &mut App) {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let (cw, ch) = (client.right - client.left, client.bottom - client.top);
    let toolbar_h = app.px(TOOLBAR_H);
    let row_y = app.px(STRIP_H + 3.0);
    // Toolbar buttons: fixed DIP widths in creation order. The first
    // six flow from the left; the last two (the Text / Hex mode
    // toggles) are anchored to the right end of the row.
    let widths: [f32; 6] = [58.0, 58.0, 58.0, 58.0, 58.0, 74.0];
    let mut x = app.px(4.0);
    for (i, button) in app.toolbar.iter().take(6).enumerate() {
        let w = app.px(widths.get(i).copied().unwrap_or(58.0));
        // SAFETY: all handles are our child windows.
        unsafe { MoveWindow(*button, x, row_y, w, toolbar_h - app.px(6.0), 1) };
        x += w + app.px(4.0);
    }
    if app.toolbar.len() >= 8 {
        // The Text / Hex pair is one segmented control (polish
        // pass): no gap between the segments, and each segment is
        // 54 DIPs so the pair's combined footprint (108 DIPs)
        // matches the old two-buttons-plus-gap layout exactly.
        let mode_w = app.px(54.0);
        let hex_x = cw - app.px(4.0) - mode_w;
        let text_x = hex_x - mode_w;
        // SAFETY: our child windows.
        unsafe {
            MoveWindow(
                app.toolbar[6],
                text_x,
                row_y,
                mode_w,
                toolbar_h - app.px(6.0),
                1,
            );
            MoveWindow(
                app.toolbar[7],
                hex_x,
                row_y,
                mode_w,
                toolbar_h - app.px(6.0),
                1,
            );
        }
    }
    // External-change bar (roadmap item 34): directly under the
    // toolbar when showing — the label, then Reload / Ignore.
    if app.change_visible {
        let y = app.px(STRIP_H) + toolbar_h + app.px(3.0);
        let h = app.px(CHANGEBAR_H) - app.px(6.0);
        let mut cx = app.px(4.0);
        let label = app.change_label;
        let reload = app.change_reload_btn;
        let ignore = app.change_ignore_btn;
        let place = |app: &App, hwnd: HWND, cx: &mut i32, w_dip: f32| {
            let w = app.px(w_dip);
            // SAFETY: our child window.
            unsafe { MoveWindow(hwnd, *cx, y, w, h, 1) };
            *cx += w + app.px(4.0);
        };
        place(app, label, &mut cx, 260.0);
        place(app, reload, &mut cx, 74.0);
        place(app, ignore, &mut cx, 74.0);
    }
    // Find bar (second row when visible).
    if app.find_visible {
        let y = app.px(STRIP_H)
            + toolbar_h
            + if app.change_visible {
                app.px(CHANGEBAR_H)
            } else {
                0
            }
            + app.px(3.0);
        let h = app.px(FINDBAR_H) - app.px(6.0);
        let mut fx = app.px(4.0);
        let place = |app: &App, hwnd: HWND, fx: &mut i32, w_dip: f32| {
            let w = app.px(w_dip);
            // SAFETY: our child window.
            unsafe { MoveWindow(hwnd, *fx, y, w, h, 1) };
            *fx += w + app.px(4.0);
        };
        let find_edit = app.find_edit;
        let replace_edit = app.replace_edit;
        let find_count = app.find_count;
        let find_label = app.find_label;
        let replace_label = app.replace_label;
        place(app, find_label, &mut fx, 34.0);
        place(app, find_edit, &mut fx, 190.0);
        // Buttons follow in find_controls order after the two edits:
        // [find_edit, prev, next, count, replace_edit, replace, replace_all, close]
        // The match count is quiet text between the two button
        // groups, not another box: it gets an extra 4 DIPs of clear
        // space on both sides (8 in all with the standard gap).
        place(app, app.find_controls[1], &mut fx, 52.0);
        place(app, app.find_controls[2], &mut fx, 52.0);
        fx += app.px(4.0);
        place(app, find_count, &mut fx, 96.0);
        fx += app.px(4.0);
        place(app, replace_label, &mut fx, 50.0);
        place(app, replace_edit, &mut fx, 190.0);
        place(app, app.find_controls[5], &mut fx, 68.0);
        place(app, app.find_controls[6], &mut fx, 82.0);
        // The scope toggle follows the action buttons; the close
        // button stays last (find_controls[8] is the toggle, appended
        // after the original eight controls) and is square at the
        // shared control height.
        if app.find_controls.len() > 8 {
            let scope_btn = app.find_controls[8];
            place(app, scope_btn, &mut fx, 124.0);
        }
        let close_btn = app.find_controls[7];
        place(app, close_btn, &mut fx, 24.0);
    }
    // Status strip along the bottom: position text on the left, the
    // document facts (zoom · encoding · line endings · size) on the
    // right. An armed save conversion (roadmap items 31/32) widens
    // the right control so the "· → CRLF · +BOM" suffix is not
    // clipped; both widths grew by the zoom segment's room in the
    // polish pass. The strip also keeps 8 DIPs of clear space at
    // both ends of the window now (polish pass padding).
    let status_h = app.px(STATUS_H);
    let armed = !save_armed_suffix(
        app.settings.save_eol_choice(),
        app.settings.save_bom_choice(),
    )
    .is_empty();
    let right_w = app.px(if armed { 528.0 } else { 392.0 }).min(cw);
    let pad = app.px(8.0);
    // SAFETY: our child windows.
    unsafe {
        MoveWindow(
            app.status,
            pad,
            ch - status_h,
            (cw - right_w - 2 * pad).max(0),
            status_h,
            1,
        );
        MoveWindow(
            app.status_right,
            cw - right_w - pad,
            ch - status_h,
            right_w,
            status_h,
            1,
        );
    }
}

fn invalidate(app: &App) {
    // SAFETY: our window; a null rect invalidates the whole client area
    // and FALSE keeps the background (Direct2D repaints everything).
    unsafe { InvalidateRect(app.hwnd, std::ptr::null(), 0) };
}

// ---------------------------------------------------------------------------
// Title bar, status bar, custom scrollbar
// ---------------------------------------------------------------------------

/// Theme the window's title bar through the Desktop Window Manager:
/// immersive dark mode, the panel colour as the caption background,
/// the theme text colour for the caption text. Failures are ignored —
/// the attributes need Windows 10 20H1 / Windows 11 and the window
/// keeps the system look without them.
fn apply_titlebar_theme(app: &App) {
    // DWMWA_USE_IMMERSIVE_DARK_MODE = 20, DWMWA_CAPTION_COLOR = 35,
    // DWMWA_TEXT_COLOR = 36 (dwmapi.h).
    let dark: i32 = 1;
    let caption: COLORREF = colorref(app.theme.panel);
    let text: COLORREF = colorref(app.theme.text);
    // SAFETY: our window; each attribute value is a plain 4-byte value
    // that outlives the call, and the HRESULTs are deliberately unused.
    unsafe {
        DwmSetWindowAttribute(
            app.hwnd,
            20,
            &dark as *const i32 as *const c_void,
            std::mem::size_of::<i32>() as u32,
        );
        DwmSetWindowAttribute(
            app.hwnd,
            35,
            &caption as *const COLORREF as *const c_void,
            std::mem::size_of::<COLORREF>() as u32,
        );
        DwmSetWindowAttribute(
            app.hwnd,
            36,
            &text as *const COLORREF as *const c_void,
            std::mem::size_of::<COLORREF>() as u32,
        );
    }
}

fn refresh_title(app: &App) {
    let mut title = String::from("VeloEdit");
    if let Some(path) = app.editor.path() {
        title.push_str(" \u{2014} ");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        title.push_str(&name);
    }
    if app.editor.is_dirty() {
        title.push_str(" *");
    }
    let wide = wstr(&title);
    // SAFETY: our window plus a NUL-terminated string.
    unsafe { SetWindowTextW(app.hwnd, wide.as_ptr()) };
}

fn refresh_status(app: &mut App) {
    let st = app.editor.status();
    // Left: caret position, plus the selection size when there is one.
    let mut left = format!(
        "Ln {} \u{00B7} Col {} \u{00B7} Offset {}",
        st.line, st.col, st.offset
    );
    if st.selection_len > 0 {
        left.push_str(&format!(
            " \u{00B7} Selection {}",
            human_size(st.selection_len)
        ));
    }
    if st.read_only {
        left.push_str(" \u{00B7} Read-only");
    }
    if st.dirty {
        left.push_str(" \u{00B7} Unsaved changes");
    }
    if app.rel_offsets {
        // Relative offsets (roadmap item 16): the hex address
        // column is counting from this base.
        left.push_str(&format!(" \u{00B7} Offsets from base 0x{:X}", app.rel_base));
    }
    if let Some((indexed, total)) = app.indexing {
        // The staged line index is still building: report its
        // percentage. The status bar never presents the partial
        // index's prefix line count as a document total, so this
        // progress note is the honest readout while it runs; it
        // disappears the moment the complete index lands.
        if !app.editor.has_complete_index() {
            if let Some(suffix) = crate::line_index::indexing_status_suffix(indexed, total) {
                left.push_str(&suffix);
            }
        }
    }
    let wide = wstr(&left);
    // SAFETY: our status control plus a NUL-terminated string.
    unsafe { SetWindowTextW(app.status, wide.as_ptr()) };
    // Right: the zoom readout (polish pass — 100% is the default
    // font size; this refresh runs on every font change, menu or
    // Ctrl+wheel alike, so the percentage is always live), then
    // the document facts.
    let mut right = format!(
        "{}% \u{00B7} {} \u{00B7} {} \u{00B7} {}",
        zoom_percent(app.settings.font_size),
        encoding_display(st.encoding),
        app.eol,
        human_size(st.size)
    );
    if !st.encoding_confident {
        right.push_str(" (detected, not certain)");
    }
    // Armed save conversions (roadmap items 31/32) are visible
    // here before anything is saved.
    right.push_str(save_armed_suffix(
        app.settings.save_eol_choice(),
        app.settings.save_bom_choice(),
    ));
    let wide = wstr(&right);
    // SAFETY: our status control plus a NUL-terminated string.
    unsafe { SetWindowTextW(app.status_right, wide.as_ptr()) };
}

/// (total rows, first visible row, visible rows) for the current mode
/// — the numbers the custom scrollbar's geometry helpers work in.
/// While the staged line index is partial, text mode reports *byte*
/// units instead of lines: line numbers do not yet span the
/// document, but byte positions always do, so the thumb shows the
/// view's true position and extent within the file and settles into
/// line units when the complete index lands.
fn scroll_metrics(app: &mut App) -> (u64, u64, u64) {
    let (total, first) = match app.editor.mode() {
        EditorMode::Text => {
            if app.editor.index_is_partial() {
                let total = app.editor.total_len();
                let first_line = app.editor.first_line();
                let first = app.editor.line_start_byte(first_line).unwrap_or(0);
                let page_lines = page_rows(app);
                let end = app
                    .editor
                    .line_start_byte(first_line + page_lines)
                    .unwrap_or(total);
                return (total, first, end.saturating_sub(first).max(1));
            }
            (app.editor.line_count(), app.editor.first_line())
        }
        EditorMode::Hex => (
            hexview::row_count_w(
                app.editor.total_len(),
                app.editor.hex_layout().bytes_per_row,
            ),
            app.editor.first_row(),
        ),
    };
    (total, first, page_rows(app))
}

// ---------------------------------------------------------------------------
// Rendering (Direct2D). All editor data is gathered first (the editor
// needs &mut), then the draw calls run off the gathered snapshot.
// ---------------------------------------------------------------------------

type RtFillRect = unsafe extern "system" fn(*mut c_void, *const D2dRectF, *mut c_void);
type RtDrawRectangle =
    unsafe extern "system" fn(*mut c_void, *const D2dRectF, *mut c_void, f32, *const c_void);
type RtDrawText = unsafe extern "system" fn(
    *mut c_void,
    *const u16,
    u32,
    *mut c_void,
    *const D2dRectF,
    *mut c_void,
    u32,
    u32,
);

fn fill_rect(fill: RtFillRect, target: *mut c_void, rect: D2dRectF, brush: *mut c_void) {
    if !brush.is_null() {
        // SAFETY: `fill` is ID2D1RenderTarget::FillRectangle (slot 17)
        // resolved from the live target; the rect is a plain value.
        unsafe { fill(target, &rect, brush) };
    }
}

/// Draw the scrollbar ticks for one offset set (roadmap item 24):
/// one 1-DIP line per [`tick_rows`] row, inset a DIP from the
/// track's edges so the thumb stays readable over them.
#[allow(clippy::too_many_arguments)]
fn draw_offset_ticks(
    fill: RtFillRect,
    target: *mut c_void,
    track_left: f32,
    track_right: f32,
    top: f32,
    track_h: f32,
    offsets: &[u64],
    doc_len: u64,
    brush: *mut c_void,
) {
    if track_h < 1.0 {
        return;
    }
    for row in tick_rows(offsets, doc_len, track_h as u64) {
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: track_left + 1.0,
                top: top + row as f32,
                right: track_right - 1.0,
                bottom: top + row as f32 + 1.0,
            },
            brush,
        );
    }
}

fn outline_rect(
    draw_rect: RtDrawRectangle,
    target: *mut c_void,
    rect: D2dRectF,
    brush: *mut c_void,
) {
    if !brush.is_null() {
        // SAFETY: `draw_rect` is ID2D1RenderTarget::DrawRectangle
        // (slot 16) resolved from the live target; the rect is a plain
        // value, the stroke width is 1 DIP, and a null stroke style
        // means a solid stroke.
        unsafe { draw_rect(target, &rect, brush, 1.0, std::ptr::null()) };
    }
}

fn draw_text(
    draw: RtDrawText,
    target: *mut c_void,
    format: *mut c_void,
    text: &str,
    rect: D2dRectF,
    brush: *mut c_void,
) {
    if brush.is_null() || format.is_null() || text.is_empty() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: `draw` is ID2D1RenderTarget::DrawText (slot 27) resolved
    // from the live target; the string buffer and rect outlive the call;
    // option 1 = D2D1_DRAW_TEXT_OPTIONS_CLIP, measuring mode 0 = natural.
    unsafe {
        draw(
            target,
            wide.as_ptr(),
            wide.len() as u32,
            format,
            &rect,
            brush,
            1,
            0,
        )
    };
}

/// Character column of `byte` within a row starting at `row_start`,
/// decoding the prefix (rows are visited rarely and prefixes are short,
/// except on very long lines, where the row-fetch cap applies).
fn col_of_byte(
    table: &PieceTable,
    enc: crate::encoding::TextEncoding,
    row_start: u64,
    byte: u64,
) -> f32 {
    if byte <= row_start {
        return 0.0;
    }
    let bytes = table.read(row_start, byte - row_start);
    crate::editor::decode_lossy(&bytes, enc).chars().count() as f32
}

/// Format a float reading for the inspector panel: `NaN` and the
/// infinities get their plain names; magnitudes outside
/// 1e-4..1e15 use exponent form so a value always fits the panel.
fn fmt_f64(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        }
    } else if v != 0.0 && !(1e-4..1e15).contains(&v.abs()) {
        format!("{v:e}")
    } else {
        format!("{v}")
    }
}

/// The inspector panel's label/value rows for one report
/// (roadmap item 15). Integers carry both byte orders where the
/// type has them; a reading the slice was too short for shows the
/// no-reading marker, exactly as the core formats it.
fn inspector_rows(report: &crate::inspector::InspectorReport, caret: u64) -> Vec<(String, String)> {
    fn int<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|x| x.to_string())
            .unwrap_or_else(|| "\u{2014}".to_string())
    }
    fn flt(v: Option<f64>) -> String {
        v.map(fmt_f64).unwrap_or_else(|| "\u{2014}".to_string())
    }
    vec![
        ("Offset".into(), format!("{caret} (0x{caret:X})")),
        ("Hex".into(), report.hex.clone()),
        ("Binary".into(), report.binary.clone()),
        ("Int8".into(), int(report.i8)),
        ("UInt8".into(), int(report.u8)),
        ("Int16 LE".into(), int(report.i16_le)),
        ("Int16 BE".into(), int(report.i16_be)),
        ("UInt16 LE".into(), int(report.u16_le)),
        ("UInt16 BE".into(), int(report.u16_be)),
        ("Int32 LE".into(), int(report.i32_le)),
        ("Int32 BE".into(), int(report.i32_be)),
        ("UInt32 LE".into(), int(report.u32_le)),
        ("UInt32 BE".into(), int(report.u32_be)),
        ("Float32 LE".into(), flt(report.f32_le.map(|v| v as f64))),
        ("Float32 BE".into(), flt(report.f32_be.map(|v| v as f64))),
        ("Int64 LE".into(), int(report.i64_le)),
        ("Int64 BE".into(), int(report.i64_be)),
        ("UInt64 LE".into(), int(report.u64_le)),
        ("UInt64 BE".into(), int(report.u64_be)),
        ("Float64 LE".into(), flt(report.f64_le)),
        ("Float64 BE".into(), flt(report.f64_be)),
        ("Unix s (u32)".into(), report.unix_seconds_u32.clone()),
        ("Unix s (u64)".into(), report.unix_seconds_u64.clone()),
        ("Unix ms (u64)".into(), report.unix_millis_u64.clone()),
        (
            "GUID".into(),
            report
                .guid
                .clone()
                .unwrap_or_else(|| "\u{2014}".to_string()),
        ),
        ("FILETIME".into(), report.filetime_u64.clone()),
        ("Text".into(), report.utf8_preview.clone()),
    ]
}

fn render(app: &mut App) {
    ensure_format(app);
    ensure_target(app);
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let layout = compute_layout(app);
    let visible =
        (((layout.editor_bottom - layout.editor_top) / app.line_h.max(1.0)).ceil() as usize).max(1);
    let selection = app.editor.selection_range();
    let current_match = display_current_match(app);
    // Every match start (all matches share the needle's byte length),
    // plus the numbers the custom scrollbar is drawn from. While a
    // background find owns the display these come from its mirror,
    // not the editor's own list (see display_matches).
    let all_matches: Vec<u64> = display_matches(app).to_vec();
    let match_len = current_match
        .map(|(s, e)| e - s)
        .unwrap_or(if app.bg_active { app.bg_needle_len } else { 0 });
    // Bookmark offsets (sorted) for the gutter markers and the
    // scrollbar ticks.
    let bookmark_offsets: Vec<u64> = app.bookmarks.iter().map(|b| b.offset).collect();
    let (sb_total, sb_first, sb_page) = scroll_metrics(app);
    let (caret_line, caret_col) = app.editor.line_col_of(app.editor.caret());
    let caret = app.editor.caret();

    // Gather rows first; the draw pass below uses no &mut editor calls.
    enum RowData {
        Text(Vec<crate::editor::TextRow>),
        Hex(Vec<crate::editor::HexRow>),
    }
    let rows = match app.editor.mode() {
        EditorMode::Text => RowData::Text(
            app.editor
                .visible_text_rows(app.editor.first_line(), visible),
        ),
        EditorMode::Hex => {
            RowData::Hex(app.editor.visible_hex_rows(app.editor.first_row(), visible))
        }
    };

    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let format = app.text_format;
    let table = app.editor.table();
    let enc = app.editor.encoding();
    // SAFETY: slots 48/49/47 are BeginDraw/EndDraw/Clear per d2d1.h,
    // resolved from the live render target.
    let begin: unsafe extern "system" fn(*mut c_void) = unsafe { vfn(target, SLOT_RT_BEGIN_DRAW) };
    let end: unsafe extern "system" fn(*mut c_void, *mut u64, *mut u64) -> i32 =
        unsafe { vfn(target, SLOT_RT_END_DRAW) };
    let clear: unsafe extern "system" fn(*mut c_void, *const D2dColorF) =
        unsafe { vfn(target, SLOT_RT_CLEAR) };
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    let draw_rect: RtDrawRectangle = unsafe { vfn(target, SLOT_RT_DRAW_RECTANGLE) };
    let draw: RtDrawText = unsafe { vfn(target, SLOT_RT_DRAW_TEXT) };

    // SAFETY: the target is live and not in a draw session; the colour
    // is a plain value.
    unsafe { begin(target) };
    let bg = D2dColorF::from(app.theme.bg);
    // SAFETY: as above.
    unsafe { clear(target, &bg) };

    let top = layout.editor_top;
    let bottom = layout.editor_bottom;
    let width = layout.width;
    // The accent strip along the very top of the client area.
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: 0.0,
            top: 0.0,
            right: width,
            bottom: STRIP_H,
        },
        brushes.accent,
    );
    // Gutter background for the editor strip.
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: 0.0,
            top,
            right: layout.gutter_w,
            bottom,
        },
        brushes.gutter,
    );

    match &rows {
        RowData::Text(text_rows) => {
            for (i, row) in text_rows.iter().enumerate() {
                let y = top + i as f32 * app.line_h;
                // Current-line band (polish pass): a raised strip
                // behind the caret's row — background blended
                // halfway toward the panel colour — spanning the
                // text area only, not the gutter. It goes first, so
                // match, selection, and text all draw over it.
                if row.line_no == caret_line {
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: layout.gutter_w,
                            top: y,
                            right: layout.editor_right,
                            bottom: y + app.line_h,
                        },
                        brushes.band,
                    );
                }
                // Find-match highlights: every match overlapping this
                // row gets the match wash; the current match also gets
                // an accent outline so it stands out from the rest.
                if match_len > 0 {
                    let lo = row.start.saturating_sub(match_len);
                    let begin = all_matches.partition_point(|&m| m < lo);
                    for &ms in &all_matches[begin..] {
                        if ms >= row.end {
                            break;
                        }
                        let me = ms + match_len;
                        if me <= row.start {
                            continue;
                        }
                        let c0 = col_of_byte(table, enc, row.start, ms);
                        let c1 = col_of_byte(table, enc, row.start, me.min(row.end));
                        let rect = D2dRectF {
                            left: layout.gutter_w + TEXT_PAD + c0 * app.char_w,
                            top: y,
                            right: layout.gutter_w + TEXT_PAD + c1 * app.char_w,
                            bottom: y + app.line_h,
                        };
                        fill_rect(fill, target, rect, brushes.match_bg);
                        if current_match == Some((ms, me)) {
                            outline_rect(draw_rect, target, rect, brushes.accent);
                        }
                    }
                }
                // Selection highlight.
                if let Some((ss, se)) = selection {
                    if se > row.start && ss <= row.end {
                        let from = ss.max(row.start);
                        let c0 = col_of_byte(table, enc, row.start, from);
                        let x1 = if se > row.end {
                            layout.editor_right
                        } else {
                            layout.gutter_w
                                + TEXT_PAD
                                + col_of_byte(table, enc, row.start, se) * app.char_w
                        };
                        fill_rect(
                            fill,
                            target,
                            D2dRectF {
                                left: layout.gutter_w + TEXT_PAD + c0 * app.char_w,
                                top: y,
                                right: x1,
                                bottom: y + app.line_h,
                            },
                            brushes.selection,
                        );
                    }
                }
                // Line number, right-aligned in the gutter (monospace
                // metrics make the width exact).
                let number = (row.line_no + 1).to_string();
                let num_w = number.len() as f32 * app.char_w;
                let num_brush = if row.line_no == caret_line {
                    brushes.accent
                } else {
                    // Polish pass: off-current numbers sit one
                    // step brighter than muted (a 50/50 blend
                    // toward the text colour) for legibility.
                    brushes.gutter_num
                };
                draw_text(
                    draw,
                    target,
                    format,
                    &number,
                    D2dRectF {
                        left: layout.gutter_w - TEXT_PAD - num_w,
                        top: y,
                        right: layout.gutter_w - TEXT_PAD,
                        bottom: y + app.line_h,
                    },
                    num_brush,
                );
                // Bookmark marker (roadmap item 20): a small accent
                // square at the gutter's left edge on a line holding
                // a bookmark (the numbers are right-aligned, so the
                // left edge is free).
                let bm_at = bookmark_offsets.partition_point(|&b| b < row.start);
                if matches!(bookmark_offsets.get(bm_at), Some(&b) if b < row.end || b == row.start)
                {
                    let size = 6.0;
                    let my = y + (app.line_h - size) / 2.0;
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: 2.0,
                            top: my,
                            right: 2.0 + size,
                            bottom: my + size,
                        },
                        brushes.accent,
                    );
                }
                // The line's text.
                draw_text(
                    draw,
                    target,
                    format,
                    &row.text,
                    D2dRectF {
                        left: layout.gutter_w + TEXT_PAD,
                        top: y,
                        right: layout.editor_right,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
                // Caret.
                if app.has_focus && app.caret_on && row.line_no == caret_line {
                    let x = layout.gutter_w + TEXT_PAD + caret_col as f32 * app.char_w;
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: x,
                            top: y,
                            right: x + 2.0,
                            bottom: y + app.line_h,
                        },
                        brushes.accent,
                    );
                }
            }
        }
        RowData::Hex(hex_rows) => {
            // Column geometry follows the current hex layout
            // (roadmap items 13/14): the ASCII column starts after
            // the formatted hex text plus a two-cell gap.
            let hex_layout = app.editor.hex_layout();
            let hex_x = layout.gutter_w + TEXT_PAD;
            let ascii_x = hex_x
                + (hexview::hex_text_len(hex_layout.bytes_per_row as usize, &hex_layout) + 2)
                    as f32
                    * app.char_w;
            for (i, row) in hex_rows.iter().enumerate() {
                let y = top + i as f32 * app.line_h;
                if caret >= row.address && caret < row.address + row.len {
                    // The same current-line band as text mode
                    // (polish pass), text area only, drawn before
                    // the selection cells and the row's text.
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: layout.gutter_w,
                            top: y,
                            right: layout.editor_right,
                            bottom: y + app.line_h,
                        },
                        brushes.band,
                    );
                }
                // Selection: one background cell per selected byte.
                if let Some((ss, se)) = selection {
                    let from = ss.max(row.address);
                    let to = se.min(row.address + row.len);
                    let mut byte = from;
                    while byte < to {
                        let idx = (byte - row.address) as usize;
                        // The byte's hex cell sits where its digits
                        // are displayed (grouping and big-endian
                        // order move it; the ASCII cell never moves).
                        let cell =
                            hexview::byte_char_col(idx, row.len as usize, &hex_layout) as f32;
                        fill_rect(
                            fill,
                            target,
                            D2dRectF {
                                left: hex_x + cell * app.char_w,
                                top: y,
                                right: hex_x + (cell + 2.0) * app.char_w,
                                bottom: y + app.line_h,
                            },
                            brushes.selection,
                        );
                        fill_rect(
                            fill,
                            target,
                            D2dRectF {
                                left: ascii_x + idx as f32 * app.char_w,
                                top: y,
                                right: ascii_x + (idx as f32 + 1.0) * app.char_w,
                                bottom: y + app.line_h,
                            },
                            brushes.selection,
                        );
                        byte += 1;
                    }
                }
                // Bookmark marker (roadmap item 20): the same
                // accent square as text mode, at the gutter's right
                // edge here — the address text starts at the left.
                let bm_at = bookmark_offsets.partition_point(|&b| b < row.address);
                if matches!(bookmark_offsets.get(bm_at), Some(&b) if b < row.address + row.len) {
                    let size = 6.0;
                    let my = y + (app.line_h - size) / 2.0;
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: layout.gutter_w - 2.0 - size,
                            top: my,
                            right: layout.gutter_w - 2.0,
                            bottom: my + size,
                        },
                        brushes.accent,
                    );
                }
                // The address column: absolute offsets, or — while
                // relative offsets are on (roadmap item 16) — the
                // signed distance from the base, in the same 16
                // characters (a leading minus replaces one zero).
                let address = if app.rel_offsets {
                    let rel = row.address as i128 - app.rel_base as i128;
                    if rel < 0 {
                        format!("-{:015X}", -rel)
                    } else {
                        format!("{:016X}", rel)
                    }
                } else {
                    format!("{:016X}", row.address)
                };
                // The caret row's address takes the accent colour,
                // mirroring the current line number in text mode;
                // other rows take the same brightened gutter tone
                // as text-mode line numbers (polish pass).
                let address_brush = if caret >= row.address && caret < row.address + row.len {
                    brushes.accent
                } else {
                    brushes.gutter_num
                };
                draw_text(
                    draw,
                    target,
                    format,
                    &address,
                    D2dRectF {
                        left: TEXT_PAD,
                        top: y,
                        right: layout.gutter_w,
                        bottom: y + app.line_h,
                    },
                    address_brush,
                );
                draw_text(
                    draw,
                    target,
                    format,
                    &row.hex,
                    D2dRectF {
                        left: hex_x,
                        top: y,
                        right: ascii_x,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
                draw_text(
                    draw,
                    target,
                    format,
                    &row.ascii,
                    D2dRectF {
                        left: ascii_x,
                        top: y,
                        right: layout.editor_right,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
                // Caret: an outline box around the current byte.
                if app.has_focus
                    && app.caret_on
                    && caret >= row.address
                    && caret <= row.address + row.len
                {
                    let idx =
                        (caret.min(row.address + row.len.saturating_sub(1)) - row.address) as usize;
                    if row.len > 0 {
                        let cell =
                            hexview::byte_char_col(idx, row.len as usize, &hex_layout) as f32;
                        let x0 = hex_x + cell * app.char_w;
                        let x1 = x0 + 2.0 * app.char_w;
                        let t = 1.5f32;
                        for r in [
                            D2dRectF {
                                left: x0,
                                top: y,
                                right: x1,
                                bottom: y + t,
                            },
                            D2dRectF {
                                left: x0,
                                top: y + app.line_h - t,
                                right: x1,
                                bottom: y + app.line_h,
                            },
                            D2dRectF {
                                left: x0,
                                top: y,
                                right: x0 + t,
                                bottom: y + app.line_h,
                            },
                            D2dRectF {
                                left: x1 - t,
                                top: y,
                                right: x1,
                                bottom: y + app.line_h,
                            },
                        ] {
                            fill_rect(fill, target, r, brushes.accent);
                        }
                    }
                }
            }
        }
    }

    // The column ruler (roadmap item 21): a quiet 1-DIP vertical
    // guideline at the configured character column, text mode only.
    // It sits on the same x-grid as the caret and the text itself
    // (gutter + padding + column × character width), and is drawn
    // only when it falls inside the editor area, clear of the
    // scrollbar.
    if app.settings.ruler_column > 0 && matches!(rows, RowData::Text(_)) {
        let x = layout.gutter_w + TEXT_PAD + app.settings.ruler_column as f32 * app.char_w;
        if x > layout.gutter_w && x < layout.editor_right - SCROLLBAR_W {
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x,
                    top,
                    right: x + 1.0,
                    bottom,
                },
                brushes.line,
            );
        }
    }

    // The custom scrollbar, drawn over the editor's right edge: a
    // panel-coloured track with an accent thumb. Only drawn when the
    // document overflows the visible rows.
    if let Some((thumb_y, thumb_h)) = scrollbar_geometry(bottom - top, sb_total, sb_page, sb_first)
    {
        let track = D2dRectF {
            left: layout.editor_right - SCROLLBAR_W,
            top,
            right: layout.editor_right,
            bottom,
        };
        fill_rect(fill, target, track, brushes.panel);
        fill_rect(
            fill,
            target,
            D2dRectF {
                top: top + thumb_y,
                bottom: top + thumb_y + thumb_h,
                ..track
            },
            brushes.accent,
        );
        // Ticks (roadmap item 24): find matches in the accent,
        // bookmarks in the text colour — one thin line per track
        // row, bucketed by tick_rows.
        draw_offset_ticks(
            fill,
            target,
            track.left,
            track.right,
            top,
            bottom - top,
            &all_matches,
            table.len(),
            brushes.accent,
        );
        draw_offset_ticks(
            fill,
            target,
            track.left,
            track.right,
            top,
            bottom - top,
            &bookmark_offsets,
            table.len(),
            brushes.text,
        );
    }

    // The data inspector panel (roadmap item 15): docked on the
    // right while open, drawn in the theme's panel colour with the
    // same monospace format as the editor. The readings are taken
    // from the 16 bytes at the caret on every paint, so caret moves
    // and edits refresh it with no extra plumbing.
    if app.settings.inspector_open {
        let panel_left = layout.editor_right;
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: panel_left,
                top,
                right: width,
                bottom,
            },
            brushes.panel,
        );
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: panel_left,
                top,
                right: panel_left + 1.0,
                bottom,
            },
            brushes.line,
        );
        let bytes = table.read(caret, (table.len() - caret).min(16));
        let report = crate::inspector::inspect(&bytes);
        let label_w = 92.0;
        let value_x = panel_left + TEXT_PAD + label_w;
        let value_right = width - TEXT_PAD;
        // Characters that fit the value column; longer values wrap
        // onto continuation lines.
        let value_chars = (((value_right - value_x) / app.char_w.max(1.0)).floor() as usize).max(4);
        let mut y = top + 4.0;
        for (label, value) in inspector_rows(&report, caret) {
            if y + app.line_h > bottom {
                break;
            }
            draw_text(
                draw,
                target,
                format,
                &label,
                D2dRectF {
                    left: panel_left + TEXT_PAD,
                    top: y,
                    right: value_x - 4.0,
                    bottom: y + app.line_h,
                },
                brushes.muted,
            );
            let chars: Vec<char> = value.chars().collect();
            let mut lines = 0usize;
            for chunk in chars.chunks(value_chars) {
                if y + app.line_h > bottom {
                    break;
                }
                let text: String = chunk.iter().collect();
                draw_text(
                    draw,
                    target,
                    format,
                    &text,
                    D2dRectF {
                        left: value_x,
                        top: y,
                        right: value_right,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
                y += app.line_h;
                lines += 1;
            }
            if lines == 0 {
                // An empty value (the hex row of an empty file)
                // still takes its line.
                y += app.line_h;
            }
        }
    }

    // SAFETY: pairs with the BeginDraw above; null tag pointers are
    // permitted. D2DERR_RECREATE_TARGET means the device was lost: drop
    // the target and brushes; the next paint recreates them.
    let hr = unsafe { end(target, std::ptr::null_mut(), std::ptr::null_mut()) };
    if hr == D2DERR_RECREATE_TARGET {
        release_target(app);
    }
}

/// Map a mouse position (physical pixels) to a caret byte offset using
/// monospace cell maths (the SPEC.md section 4 font is fixed-pitch, so
/// cell maths is exact; documented in docs/PARITY.md).
fn pos_to_offset(app: &mut App, x_px: i32, y_px: i32) -> u64 {
    let layout = compute_layout(app);
    let x = app.dip(x_px);
    let y = app.dip(y_px);
    let row_in_view = ((y - layout.editor_top) / app.line_h.max(1.0)).floor();
    if row_in_view < 0.0 {
        return app.editor.caret();
    }
    if x > layout.editor_right {
        // A press on the inspector panel (roadmap item 15) is not
        // an editor click: keep the caret where it is.
        return app.editor.caret();
    }
    match app.editor.mode() {
        EditorMode::Text => {
            let count = app.editor.line_count();
            if count == 0 {
                return 0;
            }
            let line = (app.editor.first_line() + row_in_view as u64).min(count - 1);
            let col = ((x - layout.gutter_w - TEXT_PAD) / app.char_w.max(1.0)).floor();
            let col = col.max(0.0) as u64;
            app.editor
                .offset_of_line_col(line, col)
                .unwrap_or(app.editor.total_len())
        }
        EditorMode::Hex => {
            let hex_layout = app.editor.hex_layout();
            let width = hex_layout.bytes_per_row;
            let total_rows = hexview::row_count_w(app.editor.total_len(), width);
            if total_rows == 0 {
                return 0;
            }
            let row = (app.editor.first_row() + row_in_view as u64).min(total_rows - 1);
            let row_start = hexview::row_offset_w(row, width);
            let row_len = width.min(app.editor.total_len() - row_start);
            let hex_x = layout.gutter_w + TEXT_PAD;
            let ascii_x = hex_x
                + (hexview::hex_text_len(width as usize, &hex_layout) + 2) as f32 * app.char_w;
            let byte = if x >= ascii_x {
                ((x - ascii_x) / app.char_w.max(1.0)).floor().max(0.0) as u64
            } else {
                // Hit-test through the same display maths the
                // renderer uses, so grouping and big-endian order
                // select the byte actually under the pointer.
                let col = ((x - hex_x) / app.char_w.max(1.0)).floor().max(0.0) as usize;
                hexview::byte_at_char_col(col, row_len as usize, &hex_layout) as u64
            };
            row_start + byte.min(row_len)
        }
    }
}

/// Select the word at the caret (double-click, text mode).
fn select_word_at_caret(app: &mut App) {
    let (line, col) = app.editor.line_col_of(app.editor.caret());
    let Some(text) = app.editor.line_text(line) else {
        return;
    };
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut idx = (col as usize).min(chars.len() - 1);
    if !is_word(chars[idx]) {
        // Clicked on whitespace/punctuation: select that single cell.
        if let (Some(s), Some(e)) = (
            app.editor.offset_of_line_col(line, idx as u64),
            app.editor.offset_of_line_col(line, idx as u64 + 1),
        ) {
            app.editor.move_caret_to(s, false);
            app.editor.move_caret_to(e, true);
        }
        return;
    }
    let mut start = idx;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    idx = start;
    let _ = idx;
    if let (Some(s), Some(e)) = (
        app.editor.offset_of_line_col(line, start as u64),
        app.editor.offset_of_line_col(line, end as u64),
    ) {
        app.editor.move_caret_to(s, false);
        app.editor.move_caret_to(e, true);
    }
}

// ---------------------------------------------------------------------------
// Clipboard (CF_UNICODETEXT; the editor core owns what the bytes mean)
// ---------------------------------------------------------------------------

fn clipboard_set_text(hwnd: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: standard clipboard sequence; the global handle is
    // moveable memory we fill while locked, and ownership passes to the
    // clipboard on a successful SetClipboardData.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let handle = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        if !handle.is_null() {
            let dst = GlobalLock(handle) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                GlobalUnlock(handle);
                if SetClipboardData(CF_UNICODETEXT as u32, handle).is_null() {
                    GlobalFree(handle);
                }
            } else {
                GlobalFree(handle);
            }
        }
        CloseClipboard();
    }
}

fn clipboard_get_text(hwnd: HWND) -> Option<String> {
    // SAFETY: standard clipboard read; the handle stays owned by the
    // clipboard (we never free it), and the lock is released before
    // CloseClipboard.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT as u32) == 0 {
            return None;
        }
        if OpenClipboard(hwnd) == 0 {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT as u32);
        let result = if handle.is_null() {
            None
        } else {
            let src = GlobalLock(handle) as *const u16;
            if src.is_null() {
                None
            } else {
                let mut len = 0usize;
                while *src.add(len) != 0 && len < 64 * 1024 * 1024 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(src, len));
                GlobalUnlock(handle);
                Some(text)
            }
        };
        CloseClipboard();
        result
    }
}

// ---------------------------------------------------------------------------
// Common dialogs
// ---------------------------------------------------------------------------

fn filter_wide() -> Vec<u16> {
    // Double-NUL-terminated filter pairs for GetOpenFileNameW.
    let mut out = Vec::new();
    for part in [
        "All files (*.*)",
        "*.*",
        "Text files",
        "*.txt;*.log;*.md;*.rs",
    ] {
        out.extend(part.encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

fn file_dialog(app: &App, save: bool, initial_name: Option<&str>) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    if let Some(name) = initial_name {
        for (dst, src) in buf.iter_mut().zip(name.encode_utf16()) {
            *dst = src;
        }
    }
    let filter = filter_wide();
    // SAFETY: OPENFILENAMEW is a plain C struct, zero-initialised then
    // filled with pointers to buffers that outlive the call.
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = app.hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
    if save {
        ofn.Flags |= OFN_OVERWRITEPROMPT;
    } else {
        ofn.Flags |= OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    }
    // SAFETY: the dialog reads/writes only through the struct above.
    let ok = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    };
    if ok == 0 {
        return None;
    }
    Some(PathBuf::from(string_from_wide(&buf)))
}

fn choose_accent_color(app: &mut App, initial: crate::theme::Rgba) -> Option<crate::theme::Rgba> {
    // SAFETY: CHOOSECOLORW is a plain C struct; the custom-colours array
    // lives in the app state and outlives the call.
    let mut cc: CHOOSECOLORW = unsafe { std::mem::zeroed() };
    cc.lStructSize = std::mem::size_of::<CHOOSECOLORW>() as u32;
    cc.hwndOwner = app.hwnd;
    cc.rgbResult = colorref(initial);
    cc.lpCustColors = app.custom_colors.as_mut_ptr();
    cc.Flags = CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR;
    // SAFETY: as above.
    if unsafe { ChooseColorW(&mut cc) } == 0 {
        return None;
    }
    Some(crate::theme::Rgba {
        r: (cc.rgbResult & 0xFF) as u8,
        g: ((cc.rgbResult >> 8) & 0xFF) as u8,
        b: ((cc.rgbResult >> 16) & 0xFF) as u8,
        a: 255,
    })
}

fn message_box(hwnd: HWND, text: &str, caption: &str, style: u32) -> i32 {
    let text_w = wstr(text);
    let caption_w = wstr(caption);
    // SAFETY: both strings are NUL-terminated and outlive the modal call.
    unsafe { MessageBoxW(hwnd, text_w.as_ptr(), caption_w.as_ptr(), style) }
}

// ---------------------------------------------------------------------------
// Document lifecycle
// ---------------------------------------------------------------------------

/// Build the line index for a freshly opened mapping on a
/// background thread (SPEC.md section 3.1: the secondary indexing
/// thread), in stages: after every [`INDEX_PUBLISH_STEP`] of newly
/// indexed bytes the worker publishes a snapshot of the build so
/// far to the shared slot, and the UI thread's tick attaches each
/// one in turn — the editor's indexed prefix (and the status
/// readout's "Indexing NN%") grows while the scan runs, and the
/// final snapshot is the complete index. Each spawn bumps the
/// epoch, so a superseded thread's publications are dropped by the
/// tick — and a worker that finds a newer epoch already in the
/// slot stops quietly rather than overwriting it; an edit bumps
/// the document generation, which makes the tick discard the
/// build outright (the editor keeps answering exactly from its
/// stale index's valid prefix meanwhile).
fn spawn_index_thread(app: &mut App, mmap: Arc<MmapFile>) {
    app.index_epoch += 1;
    let epoch = app.index_epoch;
    let slot = app.index_slot.clone();
    if let Ok(mut guard) = slot.lock() {
        *guard = None;
    }
    app.indexing = Some((0, mmap.len()));
    std::thread::spawn(move || {
        let total = mmap.len();
        let mut builder = LineIndexBuilder::new(total);
        loop {
            let complete = builder.advance(
                builder.indexed_bytes() + INDEX_PUBLISH_STEP,
                |offset, out| {
                    let bytes = mmap.as_slice();
                    let start = offset as usize;
                    if start >= bytes.len() {
                        return 0;
                    }
                    let n = out.len().min(bytes.len() - start);
                    out[..n].copy_from_slice(&bytes[start..start + n]);
                    n
                },
            );
            let mut superseded = false;
            if let Ok(mut guard) = slot.lock() {
                // A newer spawn (another open, or a save-baseline
                // restart) may have published first: its epoch in
                // the slot means this worker is superseded. Stop
                // quietly instead of overwriting the newer build's
                // publication — the tick would drop ours anyway,
                // and clobbering the slot could lose the newer
                // build's final snapshot.
                if let Some((held_epoch, _)) = guard.as_ref() {
                    superseded = *held_epoch != epoch;
                }
                if !superseded {
                    *guard = Some((
                        epoch,
                        IndexProgress {
                            indexed_bytes: builder.indexed_bytes(),
                            snapshot: builder.snapshot(),
                        },
                    ));
                }
            }
            if complete || superseded {
                break;
            }
        }
    });
}

/// Fingerprint the original file on a background thread (roadmap
/// item 7): the fingerprint streams the whole file's SHA-256, which
/// is far too slow to do on the open path of a huge file. Delivery
/// is through the slot, tagged with the document epoch — the same
/// pattern as [`spawn_index_thread`]; the tick picks it up.
fn spawn_fingerprint_thread(app: &App, path: &Path) {
    let slot = app.fingerprint_slot.clone();
    let epoch = app.doc_epoch;
    if let Ok(mut guard) = slot.lock() {
        *guard = None;
    }
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        if let Ok(fp) = crate::recovery::fingerprint_file(&path) {
            if let Ok(mut guard) = slot.lock() {
                *guard = Some((epoch, fp));
            }
        }
    });
}

/// Mirror the current document's bookmarks out of the persisted
/// store into the session list. The store itself applies the
/// staleness guard: a record whose stamp no longer matches is
/// dropped and the session list comes back empty.
fn load_session_bookmarks(app: &mut App) {
    app.bookmarks.clear();
    let Some(path) = app.editor.path().map(|p| p.to_string_lossy().into_owned()) else {
        return;
    };
    let Some(stamp) = app.doc_stamp else {
        return;
    };
    let entries: Vec<(u64, String)> = app
        .bookmark_store
        .bookmarks(&path, stamp)
        .iter()
        .map(|b| (b.offset, b.name.clone()))
        .collect();
    for (offset, name) in entries {
        app.bookmarks.toggle(offset, &name);
    }
}

/// Set up the per-document tracking for the document now in the
/// editor: bookmark guard stamp + session bookmarks, the
/// external-change baseline, the delta-recovery fingerprint, and a
/// reset of the find / change-notice state that belonged to the
/// previous document. Called by [`open_document`] and once at
/// startup for the initial document.
fn begin_document_tracking(app: &mut App) {
    app.doc_epoch += 1;
    cancel_bg_find(app);
    app.change_visible = false;
    app.change_shown_for = None;
    app.change_ignored_for = None;
    for hwnd in [
        app.change_label,
        app.change_reload_btn,
        app.change_ignore_btn,
    ] {
        if !hwnd.is_null() {
            // SAFETY: our child windows (or null before creation).
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    }
    match app.editor.path().map(|p| p.to_path_buf()) {
        Some(path) => {
            let stamp = crate::bookmarks::stamp_path(&path).ok();
            app.doc_stamp = stamp;
            app.file_stamp = stamp;
            load_session_bookmarks(app);
            app.doc_fingerprint = None;
            spawn_fingerprint_thread(app, &path);
        }
        None => {
            app.doc_stamp = None;
            app.file_stamp = None;
            app.bookmarks.clear();
            app.doc_fingerprint = None;
            app.doc_mmap = None;
        }
    }
}

/// Refresh the tracking after a successful save: the file on disk
/// now holds exactly the document, so the external-change baseline
/// moves to the saved file's stamp, our retained mapping is
/// re-opened over it (the editor remapped itself the same way), and
/// a fresh fingerprint is computed for future recovery captures.
/// With `adopted_new_path` (Save As) the bookmark guard stamp and
/// session list move to the new path as well.
fn after_save_tracking(app: &mut App, adopted_new_path: bool) {
    app.doc_epoch += 1;
    if app.change_visible {
        set_change_bar_visible(app, false);
    }
    app.change_shown_for = None;
    app.change_ignored_for = None;
    let Some(path) = app.editor.path().map(|p| p.to_path_buf()) else {
        return;
    };
    app.file_stamp = crate::bookmarks::stamp_path(&path).ok();
    app.doc_mmap = MmapFile::open(&path).ok().map(Arc::new);
    // The saved file is the document's new baseline, exactly like a
    // fresh open: the editor dropped any stale index at the save
    // boundary, so re-align the generations and let the staged
    // background build re-index the saved bytes (its publications
    // attach under the same rules as at open).
    app.open_generation = app.doc_generation;
    if let Some(mmap) = app.doc_mmap.clone() {
        spawn_index_thread(app, mmap);
    }
    if adopted_new_path {
        app.doc_stamp = app.file_stamp;
        load_session_bookmarks(app);
    }
    app.doc_fingerprint = None;
    spawn_fingerprint_thread(app, &path);
}

/// Ctrl+F2 (roadmap item 20): toggle a bookmark at the caret,
/// persist the store immediately, and mirror the result into the
/// session list. Bookmarks belong to files, so an untitled document
/// is a quiet no-op.
fn toggle_bookmark(app: &mut App) {
    let Some(path) = app.editor.path().map(|p| p.to_string_lossy().into_owned()) else {
        return;
    };
    let Some(stamp) = app.doc_stamp else {
        return;
    };
    let caret = app.editor.caret();
    let (line, _) = app.editor.line_col_of(caret);
    let name = format!("Line {}", line + 1);
    app.bookmark_store.toggle(&path, stamp, caret, &name);
    let _ = app
        .bookmark_store
        .save_to(&crate::bookmarks::bookmarks_path());
    load_session_bookmarks(app);
    invalidate(app);
}

/// F2 / Shift+F2 (roadmap item 20): jump to the next / previous
/// bookmark from the caret, wrapping around the document.
fn jump_bookmark(app: &mut App, forward: bool) {
    let caret = app.editor.caret();
    let target = if forward {
        app.bookmarks.next_after(caret)
    } else {
        app.bookmarks.prev_before(caret)
    };
    if let Some(offset) = target {
        app.editor.move_caret_to(offset, false);
        after_caret_move(app);
    }
}

// ---------------------------------------------------------------------------
// Background find-all (roadmap items 10 + 24)
// ---------------------------------------------------------------------------

/// Cancel the background find, if one owns the match display: bump
/// the generation so the worker's late results are dropped, and
/// clear the mirrored matches (and their scrollbar ticks).
fn cancel_bg_find(app: &mut App) {
    if !app.bg_active {
        return;
    }
    app.find_gen += 1;
    app.bg_active = false;
    app.bg_matches.clear();
    app.bg_current = None;
    app.bg_done = false;
    if let Ok(mut guard) = app.find_slot.lock() {
        *guard = None;
    }
}

/// An owned snapshot of the editor's piece table for the find
/// worker: the same pieces and append buffer over the same backing,
/// so the worker scans exactly the document the user committed the
/// find on, whatever the UI thread does next. Memory-backed
/// documents (untitled, recovery-built) are materialised whole —
/// they already live in memory by construction.
fn snapshot_table(app: &App) -> PieceTable {
    let table = app.editor.table();
    if let Some(mmap) = &app.doc_mmap {
        let mut snap = PieceTable::from_mapped(mmap.clone());
        snap.original_file_size = table.original_file_size;
        snap.append_buffer = table.append_buffer.clone();
        snap.pieces = table.pieces.clone();
        snap.total_length = table.total_length;
        snap.rebuild_weights();
        snap
    } else {
        PieceTable::from_original(table.read(0, table.len()))
    }
}

/// Start the background find for `text` (plain whole-document find
/// on a document over [`BG_FIND_THRESHOLD`]). Returns false when the
/// needle cannot be encoded — the caller then runs the ordinary
/// synchronous find, which reports the same error the same way.
fn start_bg_find(app: &mut App, text: &str) -> bool {
    let needle = match encoding::encode_needle(text, app.editor.encoding()) {
        Ok(needle) => needle,
        Err(_) => return false,
    };
    let snapshot = snapshot_table(app);
    let len = snapshot.len();
    let options = SearchOptions::default();
    // The editor filters UTF-16 matches to code-unit-aligned
    // offsets; the worker applies the same filter so the two paths
    // never disagree.
    let utf16_bom = match app.editor.encoding() {
        encoding::TextEncoding::Utf16Le | encoding::TextEncoding::Utf16Be => {
            Some(app.editor.detection().bom_len as u64)
        }
        _ => None,
    };
    app.find_gen += 1;
    let find_gen = app.find_gen;
    let doc_gen = app.doc_generation;
    let slot = app.find_slot.clone();
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(FindProgress {
            find_gen,
            doc_gen,
            matches: Vec::new(),
            done: false,
        });
    }
    app.bg_active = true;
    app.bg_matches.clear();
    app.bg_current = None;
    app.bg_done = false;
    app.bg_needle_len = needle.len() as u64;
    std::thread::spawn(move || {
        let mut start = 0u64;
        loop {
            let end = (start + BG_FIND_WINDOW).min(len);
            let mut found = search::find_in_window(&snapshot, &needle, &options, start, end);
            if let Some(bom) = utf16_bom {
                found.retain(|&m| m >= bom && (m - bom) % 2 == 0);
            }
            let last = end >= len;
            if let Ok(mut guard) = slot.lock() {
                match guard.as_mut() {
                    Some(progress)
                        if progress.find_gen == find_gen && progress.doc_gen == doc_gen =>
                    {
                        progress.matches.extend(found);
                        progress.done = last;
                    }
                    // Superseded or cancelled: stop quietly.
                    _ => return,
                }
            }
            if last {
                return;
            }
            start = end;
        }
    });
    true
}

/// Select the background find's match at `idx`: the selection is
/// placed with the editor's ordinary caret moves (anchor at the
/// match start, caret at its end — the same shape the editor's own
/// match selection produces).
fn select_bg_match(app: &mut App, idx: usize) {
    let Some(&start) = app.bg_matches.get(idx) else {
        return;
    };
    app.bg_current = Some(idx);
    app.editor.move_caret_to(start, false);
    app.editor.move_caret_to(start + app.bg_needle_len, true);
}

/// Find Next / Previous over the matches known so far while a
/// background find is populating (or once it has finished): cycle
/// from the selected match, or enter at the first match at or after
/// the caret when none is selected yet.
fn bg_advance(app: &mut App, advance: i32) {
    if app.bg_matches.is_empty() {
        return;
    }
    let len = app.bg_matches.len();
    let idx = match app.bg_current {
        Some(i) if advance > 0 => (i + 1) % len,
        Some(i) if advance < 0 => (i + len - 1) % len,
        _ => {
            let caret = app.editor.caret();
            if advance < 0 {
                app.bg_matches
                    .iter()
                    .rposition(|&m| m < caret)
                    .unwrap_or(len - 1)
            } else {
                app.bg_matches.iter().position(|&m| m >= caret).unwrap_or(0)
            }
        }
    };
    select_bg_match(app, idx);
}

/// The find count label while a background find owns the display:
/// "Counting…" until the final window lands, then the ordinary
/// position / no-matches forms over the completed list.
fn refresh_bg_find_label(app: &App) {
    if !app.bg_done {
        set_find_label(app, "Counting\u{2026}");
    } else if app.bg_matches.is_empty() {
        set_find_label(app, "No matches");
    } else {
        set_find_label(
            app,
            &format!(
                "{} of {}",
                app.bg_current.map(|i| i + 1).unwrap_or(0),
                app.bg_matches.len()
            ),
        );
    }
}

/// The match list the window displays right now: the background
/// find's mirror while it owns the display, the editor's own list
/// otherwise.
fn display_matches(app: &App) -> &[u64] {
    if app.bg_active {
        &app.bg_matches
    } else {
        app.editor.matches()
    }
}

/// The currently selected match's byte range, from whichever find
/// owns the display.
fn display_current_match(app: &App) -> Option<(u64, u64)> {
    if app.bg_active {
        let idx = app.bg_current?;
        let start = *app.bg_matches.get(idx)?;
        Some((start, start + app.bg_needle_len))
    } else {
        app.editor.current_match()
    }
}

// ---------------------------------------------------------------------------
// External-change notice (roadmap item 34)
// ---------------------------------------------------------------------------

/// Show or hide the notice bar, repositioning the rows below it.
fn set_change_bar_visible(app: &mut App, visible: bool) {
    if app.change_visible == visible {
        return;
    }
    app.change_visible = visible;
    let show = if visible { SW_SHOW } else { SW_HIDE };
    for hwnd in [
        app.change_label,
        app.change_reload_btn,
        app.change_ignore_btn,
    ] {
        if !hwnd.is_null() {
            // SAFETY: our child windows.
            unsafe { ShowWindow(hwnd, show) };
        }
    }
    layout_children(app);
    invalidate(app);
}

/// Re-stat the open file and show the notice bar when its stamp no
/// longer matches the one at open / last save. Called when the
/// window gains focus (WM_ACTIVATE) and immediately before a save.
/// The bar is non-modal: it never blocks the save or the editing,
/// and it never appears twice for the same on-disk state.
fn check_external_change(app: &mut App) {
    let Some(path) = app.editor.path().map(|p| p.to_path_buf()) else {
        return;
    };
    let Some(base) = app.file_stamp else {
        return;
    };
    let Ok(now) = crate::bookmarks::stamp_path(&path) else {
        return;
    };
    if now == base {
        // The file is at the state we know: a showing bar no longer
        // applies (it may have been changed back).
        if app.change_visible {
            app.change_shown_for = None;
            set_change_bar_visible(app, false);
        }
        return;
    }
    if app.change_shown_for == Some(now) || app.change_ignored_for == Some(now) {
        return;
    }
    app.change_shown_for = Some(now);
    set_change_bar_visible(app, true);
}

/// Open `path` in the window: memory-mapped, encoding detected, index
/// building in the background. Errors are reported, never fatal.
fn open_document(app: &mut App, path: &Path) {
    let mmap = match MmapFile::open(path) {
        Ok(mmap) => Arc::new(mmap),
        Err(err) => {
            message_box(
                app.hwnd,
                &format!("Could not open {}:\n{err}", path.display()),
                "VeloEdit",
                MB_OK | MB_ICONERROR,
            );
            return;
        }
    };
    let sample_len = (mmap.len() as usize).min(64 * 1024);
    let detection = encoding::detect(&mmap.as_slice()[..sample_len]);
    app.eol = detect_eol(&mmap.as_slice()[..sample_len]);
    let table = if mmap.is_empty() {
        PieceTable::new(0)
    } else {
        PieceTable::from_mapped(mmap.clone())
    };
    app.editor = Editor::from_table(
        table,
        detection,
        Some(path.to_path_buf()),
        app.read_only_flag,
    );
    // Staged open: index the head region synchronously so the
    // first paint and the status readout below never wait for a
    // full scan; the background thread spawned further down
    // completes the index in stages.
    app.editor.seed_line_index();
    // Roadmap item 6: the owner's configured undo limits apply to
    // every freshly opened document.
    app.editor.set_undo_limits(
        app.settings.undo_limit,
        app.settings.undo_byte_budget_mb * 1024 * 1024,
    );
    // The persisted hex layout (roadmap items 13/14) applies to the
    // fresh editor too.
    sync_hex_layout(app);
    app.doc_generation = 0;
    app.open_generation = 0;
    app.last_edit = None;
    app.hex_nibble = None;
    app.doc_mmap = Some(mmap.clone());
    spawn_index_thread(app, mmap);
    delete_recovery_files();
    begin_document_tracking(app);
    let path_string = path.to_string_lossy().into_owned();
    app.settings.add_recent(&path_string);
    save_settings(app);
    refresh_title(app);
    refresh_status(app);
    invalidate(app);
}

/// Ask about unsaved changes when needed. Returns true to proceed.
fn prompt_save_if_dirty(app: &mut App) -> bool {
    if !app.editor.is_dirty() {
        return true;
    }
    let name = app
        .editor
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "Untitled".to_string());
    let answer = message_box(
        app.hwnd,
        &format!("Save changes to {name}?"),
        "VeloEdit",
        MB_YESNOCANCEL | MB_ICONQUESTION,
    );
    match answer {
        IDYES => do_save(app),
        IDNO => true,
        _ => false,
    }
}

/// Save over the current file (or Save As when there is none).
/// Returns success.
/// The save options the window currently has chosen (roadmap
/// items 31/32): the document's own encoding plus the line-ending
/// and BOM handling from File ▸ Save Options. Every window save
/// routes through these, so an armed conversion applies to Save
/// and Save As alike.
fn gui_save_options(app: &App) -> SaveOptions {
    SaveOptions {
        encoding: app.editor.encoding(),
        eol: app.settings.save_eol_choice(),
        bom: app.settings.save_bom_choice(),
        ..SaveOptions::default()
    }
}

/// The status-bar suffix announcing an armed save conversion:
/// `""` when both choices are Keep, otherwise the parts that will
/// change the bytes on the next save (`" · → CRLF"`, `" · → LF"`,
/// `" · +BOM"`, `" · −BOM"`, in that order, joined).
fn save_armed_suffix(eol: crate::save::EolChoice, bom: crate::save::BomChoice) -> &'static str {
    use crate::save::{BomChoice, EolChoice};
    match (eol, bom) {
        (EolChoice::Keep, BomChoice::Keep) => "",
        (EolChoice::Lf, BomChoice::Keep) => " \u{00B7} \u{2192} LF",
        (EolChoice::Crlf, BomChoice::Keep) => " \u{00B7} \u{2192} CRLF",
        (EolChoice::Keep, BomChoice::Add) => " \u{00B7} +BOM",
        (EolChoice::Keep, BomChoice::Remove) => " \u{00B7} \u{2212}BOM",
        (EolChoice::Lf, BomChoice::Add) => " \u{00B7} \u{2192} LF \u{00B7} +BOM",
        (EolChoice::Lf, BomChoice::Remove) => " \u{00B7} \u{2192} LF \u{00B7} \u{2212}BOM",
        (EolChoice::Crlf, BomChoice::Add) => " \u{00B7} \u{2192} CRLF \u{00B7} +BOM",
        (EolChoice::Crlf, BomChoice::Remove) => " \u{00B7} \u{2192} CRLF \u{00B7} \u{2212}BOM",
    }
}

fn do_save(app: &mut App) -> bool {
    if app.editor.path().is_none() {
        return do_save_as(app);
    }
    // Roadmap item 34: surface an external change before we
    // overwrite the file. The notice is non-modal — the save still
    // proceeds; the bar simply stays visible until the save lands
    // (after_save_tracking then retires it).
    check_external_change(app);
    match app.editor.save_with_options(&gui_save_options(app)) {
        Ok(_) => {
            delete_recovery_files();
            app.last_edit = None;
            after_save_tracking(app, false);
            refresh_title(app);
            refresh_status(app);
            true
        }
        Err(err) => {
            message_box(
                app.hwnd,
                &format!("{err}"),
                "VeloEdit",
                MB_OK | MB_ICONERROR,
            );
            false
        }
    }
}

fn do_save_as(app: &mut App) -> bool {
    let initial = app
        .editor
        .path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled.txt".to_string());
    let Some(path) = file_dialog(app, true, Some(&initial)) else {
        return false;
    };
    match app
        .editor
        .save_as_with_options(&path, &gui_save_options(app))
    {
        Ok(_) => {
            delete_recovery_files();
            app.last_edit = None;
            after_save_tracking(app, true);
            let path_string = path.to_string_lossy().into_owned();
            app.settings.add_recent(&path_string);
            save_settings(app);
            refresh_title(app);
            refresh_status(app);
            true
        }
        Err(err) => {
            message_box(
                app.hwnd,
                &format!("{err}"),
                "VeloEdit",
                MB_OK | MB_ICONERROR,
            );
            false
        }
    }
}

/// File ▸ Save Selection to File… (roadmap item 35): stream the
/// selected bytes straight to a file the owner picks — no clipboard
/// round-trip, so a selection of any size can be exported. The
/// byte-moving core is `Editor::save_selection_to`; this only wires
/// the dialog and the error box. The menu item is greyed unless a
/// selection exists (see WM_INITMENUPOPUP); the guard here is a
/// second line of defence.
fn do_save_selection(app: &mut App) {
    if app.editor.selection_range().is_none() {
        return;
    }
    let Some(path) = file_dialog(app, true, Some("selection.bin")) else {
        return;
    };
    if let Err(err) = app.editor.save_selection_to(&path) {
        message_box(
            app.hwnd,
            &format!("Could not save the selection to {}:\n{err}", path.display()),
            "VeloEdit",
            MB_OK | MB_ICONERROR,
        );
    }
}

// ---------------------------------------------------------------------------
// Split File Copy dialog (roadmap item 36): a small modal window in
// the same hand-built style as the Help window — a label, one edit
// for the part size in megabytes, and Split / Cancel buttons. A
// nested message loop makes it modal to the main window.
// ---------------------------------------------------------------------------

const SPLIT_CLASS: &str = "VeloEditSplit";
const ID_SPLIT_SIZE_EDIT: usize = 5101;
const ID_SPLIT_GO: usize = 5102;
const ID_SPLIT_CANCEL: usize = 5103;

struct SplitDialogState {
    /// The main window's App (UI thread only; the dialog is modal,
    /// so nothing else can mutate it while the dialog is up).
    app: *mut App,
    edit: HWND,
    /// The part size in MB the user confirmed, if any.
    result: Option<u64>,
}

unsafe extern "system" fn split_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's SplitDialogState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut SplitDialogState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (it lives in the
        // caller's stack frame, which waits in the modal loop).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        let label = create_child(
            app,
            "Static",
            "Part size in megabytes (MB). A copy is split; the original file is not changed.",
            WS_CHILD | WS_VISIBLE,
            0,
            0,
        );
        // The child helper parents to the main window's App handle —
        // reparent the label and the rest to the dialog explicitly.
        // SAFETY: `label` is a fresh child window of ours.
        unsafe { SetParent(label, hwnd) };
        // SAFETY: position the label inside the dialog.
        unsafe { MoveWindow(label, px(12.0), px(10.0), px(330.0), px(30.0), 1) };
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        let edit = create_child(
            app,
            "Edit",
            "4096",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_SPLIT_SIZE_EDIT,
        );
        // SAFETY: as above — reparent and position our child window.
        unsafe {
            SetParent(edit, hwnd);
            MoveWindow(edit, px(12.0), px(44.0), px(120.0), px(24.0), 1);
            SetFocus(edit);
        }
        state.edit = edit;
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW, like the find bar
        let go = create_child(app, "Button", "Split", button_style, 0, ID_SPLIT_GO);
        let cancel = create_child(app, "Button", "Cancel", button_style, 0, ID_SPLIT_CANCEL);
        // SAFETY: as above — reparent and position our child windows.
        unsafe {
            SetParent(go, hwnd);
            MoveWindow(go, px(196.0), px(44.0), px(70.0), px(24.0), 1);
            SetParent(cancel, hwnd);
            MoveWindow(cancel, px(274.0), px(44.0), px(70.0), px(24.0), 1);
        }
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE; messages
    // before it (or after the dialog is gone) get default handling.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut SplitDialogState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_COMMAND => {
            match wparam & 0xFFFF {
                ID_SPLIT_GO => {
                    // SAFETY: our edit control.
                    let text = unsafe { window_text(state.edit) };
                    match text.trim().parse::<u64>() {
                        Ok(mb) if mb > 0 => {
                            state.result = Some(mb);
                            // SAFETY: our dialog window.
                            unsafe { DestroyWindow(hwnd) };
                        }
                        _ => {
                            message_box(
                                hwnd,
                                "Enter a part size in megabytes — a whole number of 1 or more.",
                                "VeloEdit",
                                MB_OK | MB_ICONERROR,
                            );
                        }
                    }
                }
                ID_SPLIT_CANCEL => {
                    // SAFETY: our dialog window.
                    unsafe { DestroyWindow(hwnd) };
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: our dialog window; closing is cancelling.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        // The buttons are owner-drawn; reuse the main window's themed
        // drawing by forwarding to its stored App is not possible
        // from another window proc, so the dialog's buttons fall back
        // to a plain frame-and-text draw here.
        WM_DRAWITEM => {
            // SAFETY: lparam is a DRAWITEMSTRUCT for one of our buttons.
            let dis = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
            // SAFETY: the App is alive (modal dialog, UI thread).
            let app = unsafe { &*state.app };
            let mut rect = dis.rcItem;
            // SAFETY: GDI drawing on the button's own DC, colours from
            // the active theme like the find-bar buttons.
            unsafe {
                FillRect(dis.hDC, &rect, app.panel_brush);
                FrameRect(dis.hDC, &rect, app.line_brush);
                if dis.itemState & 0x10 != 0 {
                    // ODS_FOCUS: the same accent inset focus frame
                    // the main window's buttons draw (polish pass).
                    let mut inner = rect;
                    inner.left += 2;
                    inner.top += 2;
                    inner.right -= 2;
                    inner.bottom -= 2;
                    FrameRect(dis.hDC, &inner, app.accent_brush);
                }
                SetBkMode(dis.hDC, TRANSPARENT as i32);
                SetTextColor(dis.hDC, colorref(app.theme.text));
                let text = window_text(dis.hwndItem);
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                if !wide.is_empty() {
                    DrawTextW(
                        dis.hDC,
                        wide.as_mut_ptr(),
                        wide.len() as i32,
                        &mut rect,
                        0x1 | 0x4 | 0x20,
                    );
                }
            }
            1
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Show the Split dialog modally; returns the part size in MB, or
/// `None` when the user cancelled.
fn show_split_dialog(app: &mut App) -> Option<u64> {
    let mut state = SplitDialogState {
        app: app as *mut App,
        edit: std::ptr::null_mut(),
        result: None,
    };
    let class = wstr(SPLIT_CLASS);
    let title = wstr("Split File Copy");
    // SAFETY: the split class is registered in run_gui; the state
    // travels in lpCreateParams and lives in this stack frame until
    // the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(368.0),
            app.px(116.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut SplitDialogState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: modal behaviour — the main window is disabled while the
    // dialog is up and re-enabled as soon as it is destroyed.
    unsafe {
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error — either ends the
        // modal wait (a quit is re-posted so the app still exits).
        let result = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            // SAFETY: preserve the quit for the main loop.
            unsafe { PostQuitMessage(0) };
            break;
        }
        // SAFETY: standard message pumping.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // SAFETY: a plain liveness query on our dialog.
        if unsafe { IsWindow(hwnd) } == 0 {
            break;
        }
    }
    // SAFETY: restore the main window and give it the focus back.
    unsafe {
        EnableWindow(app.hwnd, 1);
        SetFocus(app.hwnd);
    }
    state.result
}

/// File ▸ Split File Copy… (roadmap item 36): write the current
/// document to a copy inside a new `<name>-parts` folder next to the
/// file and split that copy into numbered parts, leaving the original
/// file untouched. The transient copy is removed once the parts
/// exist; the part count is reported when done.
fn do_split_file(app: &mut App) {
    let Some(src) = app.editor.path().map(|p| p.to_path_buf()) else {
        message_box(
            app.hwnd,
            "Save the document to a file first, then split it.",
            "VeloEdit",
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    };
    let Some(mb) = show_split_dialog(app) else {
        return;
    };
    let part_size = mb.saturating_mul(1024 * 1024);
    let parent = src.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "veloedit".to_string());
    let file_name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "veloedit".to_string());
    let folder = parent.join(format!("{stem}-parts"));
    if let Err(e) = std::fs::create_dir_all(&folder) {
        message_box(
            app.hwnd,
            &format!("Could not create {}:\n{e}", folder.display()),
            "VeloEdit",
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    // The copy: the document as it stands in the editor (unsaved
    // edits included), written with the atomic save pipeline.
    let copy_path = folder.join(&file_name);
    if let Err(err) = crate::save::save_atomic(
        app.editor.table(),
        &copy_path,
        &crate::save::SaveOptions::default(),
    ) {
        message_box(
            app.hwnd,
            &format!("Could not write the copy to split:\n{err}"),
            "VeloEdit",
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    match crate::save::split_file(&copy_path, part_size, &folder) {
        Ok(parts) => {
            let count = parts.len();
            let _ = std::fs::remove_file(&copy_path);
            message_box(
                app.hwnd,
                &format!(
                    "Split {file_name} into {count} part(s) of at most {mb} MB in:\n{}",
                    folder.display()
                ),
                "VeloEdit",
                MB_OK | MB_ICONINFORMATION,
            );
        }
        Err(e) => {
            let _ = std::fs::remove_file(&copy_path);
            message_box(
                app.hwnd,
                &format!("Could not split the copy:\n{e}"),
                "VeloEdit",
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// File Hashes dialog (roadmap item 37): a small modal window in the
// Split dialog's pattern. A worker thread streams the file ON DISK
// through the hash core (the file can be huge — nothing is loaded
// into memory) and delivers the three digests through a slot the
// dialog polls on a timer. The compare box checks pasted text
// against whichever digest it equals.
// ---------------------------------------------------------------------------

const HASH_CLASS: &str = "VeloEditHash";
const ID_HASH_COMPARE: usize = 5204;
const ID_HASH_CLOSE: usize = 5206;
const TIMER_HASH: usize = 7;
/// EN_CHANGE, the edit-control change notification.
const EN_CHANGE: usize = 0x0300;

struct HashDialogState {
    /// The main window's App (UI thread only; the dialog is modal).
    app: *mut App,
    /// The worker's result slot; `None` while hashing runs.
    slot: Arc<Mutex<Option<crate::hash::FileHashes>>>,
    /// First line: what is being hashed (and, for a dirty document,
    /// that it is the file on disk, not the edited bytes).
    note: String,
    /// Second line: the file's path.
    file_label: String,
    done: bool,
    compare: HWND,
    verdict: HWND,
    /// The three digest value controls, MD5 / SHA-1 / SHA-256.
    values: [HWND; 3],
}

/// The compare verdict for the dialog: what `input` (the compare
/// box's text, trimmed) means against the computed digests — empty
/// while the box is empty, a "which hash matched" line on a match
/// (compared case-insensitively), "No match" otherwise, and a
/// still-working note while the worker runs.
fn hash_verdict(input: &str, hashes: Option<&crate::hash::FileHashes>) -> String {
    let pasted = input.trim().to_lowercase();
    if pasted.is_empty() {
        return String::new();
    }
    let Some(h) = hashes else {
        return "Still hashing\u{2026}".to_string();
    };
    if pasted == h.sha256 {
        "Matches SHA-256".to_string()
    } else if pasted == h.sha1 {
        "Matches SHA-1".to_string()
    } else if pasted == h.md5 {
        "Matches MD5".to_string()
    } else {
        "No match".to_string()
    }
}

/// Refresh the dialog's verdict line from the compare box.
fn update_hash_verdict(state: &HashDialogState) {
    // SAFETY: our dialog's compare edit.
    let text = unsafe { window_text(state.compare) };
    let hashes = state.slot.lock().ok().and_then(|g| g.clone());
    let verdict = hash_verdict(&text, hashes.as_ref());
    let wide = wstr(&verdict);
    // SAFETY: our verdict static; the string outlives the call.
    unsafe { SetWindowTextW(state.verdict, wide.as_ptr()) };
}

unsafe extern "system" fn hash_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's HashDialogState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut HashDialogState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (it lives in the
        // caller's stack frame, which waits in the modal loop).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        // Children are created against the main window by the helper
        // and reparented to the dialog, exactly like the Split
        // dialog's.
        let place = |hwnd_child: HWND, x: f32, y: f32, w: f32, h: f32| {
            // SAFETY: `hwnd_child` is a fresh child window of ours.
            unsafe {
                SetParent(hwnd_child, hwnd);
                MoveWindow(hwnd_child, px(x), px(y), px(w), px(h), 1);
            }
        };
        let note = create_child(
            app,
            "Static",
            &state.note.clone(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
        );
        place(note, 12.0, 10.0, 536.0, 20.0);
        let file = create_child(
            app,
            "Static",
            &state.file_label.clone(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
        );
        place(file, 12.0, 32.0, 536.0, 20.0);
        let labels = ["MD5", "SHA-1", "SHA-256"];
        for (i, label) in labels.iter().enumerate() {
            let y = 62.0 + i as f32 * 28.0;
            let lab = create_child(app, "Static", label, WS_CHILD | WS_VISIBLE, 0, 0);
            place(lab, 12.0, y, 70.0, 22.0);
            let val = create_child(
                app,
                "Static",
                "Computing\u{2026}",
                WS_CHILD | WS_VISIBLE,
                0,
                0,
            );
            place(val, 88.0, y, 460.0, 22.0);
            state.values[i] = val;
        }
        let cmp_label = create_child(app, "Static", "Compare:", WS_CHILD | WS_VISIBLE, 0, 0);
        place(cmp_label, 12.0, 152.0, 56.0, 22.0);
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        let compare = create_child(
            app,
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_HASH_COMPARE,
        );
        place(compare, 74.0, 150.0, 474.0, 24.0);
        state.compare = compare;
        let verdict = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE, 0, 0);
        place(verdict, 12.0, 182.0, 536.0, 22.0);
        state.verdict = verdict;
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW, like the Split dialog
        let close = create_child(app, "Button", "Close", button_style, 0, ID_HASH_CLOSE);
        place(close, 468.0, 212.0, 80.0, 26.0);
        // SAFETY: the poll timer on our dialog; a null TIMERPROC
        // means WM_TIMER messages.
        unsafe { SetTimer(hwnd, TIMER_HASH, 100, None) };
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE; messages
    // before it (or after the dialog is gone) get default handling.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut HashDialogState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_TIMER if wparam == TIMER_HASH => {
            if !state.done {
                let hashes = state.slot.lock().ok().and_then(|g| g.clone());
                if let Some(h) = hashes {
                    state.done = true;
                    // SAFETY: our dialog's timer and value statics.
                    unsafe { KillTimer(hwnd, TIMER_HASH) };
                    for (control, digest) in state.values.iter().zip([
                        h.md5.as_str(),
                        h.sha1.as_str(),
                        h.sha256.as_str(),
                    ]) {
                        let wide = wstr(digest);
                        // SAFETY: as above; the string outlives the call.
                        unsafe { SetWindowTextW(*control, wide.as_ptr()) };
                    }
                    update_hash_verdict(state);
                }
            }
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let code = (wparam >> 16) & 0xFFFF;
            if id == ID_HASH_CLOSE {
                // SAFETY: our dialog window.
                unsafe { DestroyWindow(hwnd) };
            } else if id == ID_HASH_COMPARE && code == EN_CHANGE {
                update_hash_verdict(state);
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: our dialog window; closing is the only exit.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            // SAFETY: our dialog's timer.
            unsafe { KillTimer(hwnd, TIMER_HASH) };
            0
        }
        // The Close button is owner-drawn, drawn exactly like the
        // Split dialog's buttons (theme panel frame, theme text).
        WM_DRAWITEM => {
            // SAFETY: lparam is a DRAWITEMSTRUCT for our button.
            let dis = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
            // SAFETY: the App is alive (modal dialog, UI thread).
            let app = unsafe { &*state.app };
            let mut rect = dis.rcItem;
            // SAFETY: GDI drawing on the button's own DC, colours
            // from the active theme.
            unsafe {
                FillRect(dis.hDC, &rect, app.panel_brush);
                FrameRect(dis.hDC, &rect, app.line_brush);
                if dis.itemState & 0x10 != 0 {
                    // ODS_FOCUS: the same accent inset focus frame
                    // the main window's buttons draw (polish pass).
                    let mut inner = rect;
                    inner.left += 2;
                    inner.top += 2;
                    inner.right -= 2;
                    inner.bottom -= 2;
                    FrameRect(dis.hDC, &inner, app.accent_brush);
                }
                SetBkMode(dis.hDC, TRANSPARENT as i32);
                SetTextColor(dis.hDC, colorref(app.theme.text));
                let text = window_text(dis.hwndItem);
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                if !wide.is_empty() {
                    DrawTextW(
                        dis.hDC,
                        wide.as_mut_ptr(),
                        wide.len() as i32,
                        &mut rect,
                        0x1 | 0x4 | 0x20,
                    );
                }
            }
            1
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Show the File Hashes dialog modally over a hashing worker that
/// is already running into `slot`.
fn show_hash_dialog(
    app: &mut App,
    note: String,
    file_label: String,
    slot: Arc<Mutex<Option<crate::hash::FileHashes>>>,
) {
    let mut state = HashDialogState {
        app: app as *mut App,
        slot,
        note,
        file_label,
        done: false,
        compare: std::ptr::null_mut(),
        verdict: std::ptr::null_mut(),
        values: [std::ptr::null_mut(); 3],
    };
    let class = wstr(HASH_CLASS);
    let title = wstr("File Hashes");
    // SAFETY: the hash class is registered in run_gui; the state
    // travels in lpCreateParams and lives in this stack frame until
    // the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(584.0),
            app.px(286.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut HashDialogState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return;
    }
    // SAFETY: modal behaviour — the main window is disabled while
    // the dialog is up and re-enabled as soon as it is destroyed.
    unsafe {
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error — either ends
        // the modal wait (a quit is re-posted so the app still exits).
        let result = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            // SAFETY: preserve the quit for the main loop.
            unsafe { PostQuitMessage(0) };
            break;
        }
        // SAFETY: standard message pumping.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // SAFETY: a plain liveness query on our dialog.
        if unsafe { IsWindow(hwnd) } == 0 {
            break;
        }
    }
    // SAFETY: restore the main window and give it the focus back.
    unsafe {
        EnableWindow(app.hwnd, 1);
        SetFocus(app.hwnd);
    }
}

/// File ▸ File Hashes… (roadmap item 37): stream the file on disk
/// through MD5, SHA-1, and SHA-256 on a worker thread — the file
/// can be huge, so the read side is a seek-and-read closure and
/// nothing is loaded into memory. With unsaved changes the dialog
/// says plainly that it is hashing the file on disk.
fn do_file_hashes(app: &mut App) {
    let Some(path) = app.editor.path().map(|p| p.to_path_buf()) else {
        message_box(
            app.hwnd,
            "Save the document to a file first, then hash it.",
            "VeloEdit",
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    };
    let slot: Arc<Mutex<Option<crate::hash::FileHashes>>> = Arc::new(Mutex::new(None));
    let worker_slot = slot.clone();
    let worker_path = path.clone();
    std::thread::spawn(move || {
        let hashes = (|| {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = std::fs::File::open(&worker_path).ok()?;
            let total = file.metadata().ok()?.len();
            Some(crate::hash::hash_byte_source(total, |offset, buf| {
                if file.seek(SeekFrom::Start(offset)).is_err() {
                    return 0;
                }
                file.read(buf).unwrap_or(0)
            }))
        })();
        // A file that could not be read at all still completes the
        // dialog — with the digests of an empty input — rather than
        // leaving it on "Computing…" forever. (The menu item is
        // only enabled for documents that have a file, and the file
        // was there at open; this is the vanished-since case.)
        let hashes = hashes.unwrap_or_else(|| crate::hash::hash_bytes(&[]));
        if let Ok(mut guard) = worker_slot.lock() {
            *guard = Some(hashes);
        }
    });
    let note = if app.editor.is_dirty() {
        "The document has unsaved changes \u{2014} hashing the file on disk:"
    } else {
        "Hashes of the file on disk:"
    }
    .to_string();
    let file_label = path.display().to_string();
    show_hash_dialog(app, note, file_label, slot);
}

// ---------------------------------------------------------------------------
// Find bar
// ---------------------------------------------------------------------------

fn show_find_bar(app: &mut App, focus_replace: bool) {
    app.find_visible = true;
    for hwnd in app
        .find_controls
        .iter()
        .chain([&app.find_label, &app.replace_label])
    {
        // SAFETY: our child windows.
        unsafe { ShowWindow(*hwnd, SW_SHOW) };
    }
    // Pre-fill from the current selection — the usual editor habit.
    if let Some((start, end)) = app.editor.selection_range() {
        if end - start <= 256 {
            let text = app.editor.selected_text_lossy();
            if !text.is_empty() && !text.contains('\n') {
                let wide = wstr(&text);
                // SAFETY: our edit control plus a NUL-terminated string.
                unsafe { SetWindowTextW(app.find_edit, wide.as_ptr()) };
            }
        }
    }
    layout_children(app);
    invalidate(app);
    // SAFETY: our child window.
    unsafe {
        SetFocus(if focus_replace {
            app.replace_edit
        } else {
            app.find_edit
        })
    };
}

fn hide_find_bar(app: &mut App) {
    app.find_visible = false;
    for hwnd in app
        .find_controls
        .iter()
        .chain([&app.find_label, &app.replace_label])
    {
        // SAFETY: our child windows.
        unsafe { ShowWindow(*hwnd, SW_HIDE) };
    }
    layout_children(app);
    invalidate(app);
    // SAFETY: our window.
    unsafe { SetFocus(app.hwnd) };
}

/// Set the find bar's count/notice label.
fn set_find_label(app: &App, text: &str) {
    let wide = wstr(text);
    // SAFETY: our static control; the string outlives the call.
    unsafe { SetWindowTextW(app.find_count, wide.as_ptr()) };
}

/// The masked pattern the find text names, when there is one
/// (roadmap item 12): hex mode only, the text must parse as a masked
/// pattern AND look like one — it contains a space or a `?` — so an
/// ordinary hex-mode text search keeps its current behaviour.
fn masked_pattern_for(app: &App, text: &str) -> Option<search::MaskedPattern> {
    if app.editor.mode() != EditorMode::Hex {
        return None;
    }
    if !text.contains(' ') && !text.contains('?') {
        return None;
    }
    search::parse_masked_pattern(text).ok()
}

/// Which kind of find the find-bar state asks for right now.
fn find_kind_for(app: &App, text: &str) -> FindKind {
    if masked_pattern_for(app, text).is_some() {
        FindKind::Masked
    } else if app.find_in_selection {
        FindKind::Scoped
    } else {
        FindKind::Plain
    }
}

/// Run (or re-run) the find from the find-bar text and update the count
/// label. `advance`: 1 = next match, -1 = previous, 0 = stay.
///
/// Three kinds of find share this flow (see [`FindKind`]): the plain
/// whole-document find; the scoped find (roadmap item 11 — the "In
/// selection" toggle: the selection is captured as the scope, and
/// Find Next / the count stay inside it); and the masked hex find
/// (roadmap item 12), whose matches drive the same caret, selection
/// and highlight flow as a plain hex find. With the toggle on but no
/// selection and no captured scope, the count label carries the
/// notice — the same place "No matches" appears.
fn find_from_bar(app: &mut App, advance: i32) {
    // SAFETY: our edit controls.
    let text = unsafe { window_text(app.find_edit) };
    if text.is_empty() {
        cancel_bg_find(app);
        set_find_label(app, "");
        app.last_find_text.clear();
        app.last_find_kind = FindKind::Plain;
        app.last_scope_range = None;
        return;
    }
    let kind = find_kind_for(app, &text);
    // A background find owns the display only while the committed
    // find is the plain whole-document find for this text. Any other
    // kind — or a one-shot synchronous request from Replace, which
    // needs the editor's complete match state — cancels it.
    let sync_once = std::mem::take(&mut app.find_sync_once);
    if kind != FindKind::Plain || sync_once {
        cancel_bg_find(app);
    }
    let rerun = if kind == FindKind::Plain && app.bg_active {
        // The editor's own list stays empty while the background
        // find owns the display; only a text or kind change re-runs.
        text != app.last_find_text || kind != app.last_find_kind
    } else {
        text != app.last_find_text || kind != app.last_find_kind || app.editor.matches().is_empty()
    };
    match kind {
        FindKind::Masked => {
            let pattern = masked_pattern_for(app, &text).expect("kind checked above");
            if rerun {
                app.editor.set_find_masked(&pattern);
            } else if advance > 0 {
                app.editor.find_next();
            } else if advance < 0 {
                app.editor.find_prev();
            }
        }
        FindKind::Scoped => {
            // Capture the scope: a fresh selection (one that is not
            // just the current match a scoped find selected) becomes
            // the scope; after that the captured range rules, because
            // a find moves the selection onto the match itself.
            if let Some(sel) = app.editor.selection_range() {
                if Some(sel) != app.editor.current_match() {
                    app.find_scope_range = Some(sel);
                }
            }
            let Some(scope) = app.find_scope_range else {
                set_find_label(app, "Select text first");
                return;
            };
            if rerun || app.last_scope_range != Some(scope) {
                match app.editor.set_find_in_range(
                    &text,
                    SearchOptions::default(),
                    scope.0,
                    scope.1,
                ) {
                    Ok(_) => app.last_scope_range = Some(scope),
                    Err(err) => {
                        set_find_label(app, &format!("{err}"));
                        return;
                    }
                }
            } else if advance > 0 {
                app.editor.find_next();
            } else if advance < 0 {
                app.editor.find_prev();
            }
        }
        FindKind::Plain => {
            if app.bg_active && !rerun {
                // The background find for this exact text is
                // populating (or done): navigation runs over the
                // matches known so far.
                if advance != 0 {
                    bg_advance(app, advance);
                }
            } else if rerun {
                cancel_bg_find(app);
                if !sync_once
                    && app.editor.total_len() > BG_FIND_THRESHOLD
                    && start_bg_find(app, &text)
                {
                    // Population started; the first selection lands
                    // with the first results (tick), and an explicit
                    // advance applies to whatever is known by now.
                    if advance != 0 {
                        bg_advance(app, advance);
                    }
                } else {
                    match app.editor.set_find(&text, SearchOptions::default()) {
                        Ok(_) => {}
                        Err(err) => {
                            set_find_label(app, &format!("{err}"));
                            return;
                        }
                    }
                }
            } else if advance > 0 {
                app.editor.find_next();
            } else if advance < 0 {
                app.editor.find_prev();
            }
        }
    }
    app.last_find_text = text;
    app.last_find_kind = kind;
    if kind == FindKind::Plain && app.bg_active {
        refresh_bg_find_label(app);
    } else {
        let count = app.editor.matches().len();
        let label = if count == 0 {
            "No matches".to_string()
        } else {
            format!(
                "{} of {}",
                app.editor.match_index().map(|i| i + 1).unwrap_or(0),
                count
            )
        };
        let wide = wstr(&label);
        // SAFETY: our static control.
        unsafe { SetWindowTextW(app.find_count, wide.as_ptr()) };
    }
    let page = page_rows(app);
    app.editor.ensure_caret_visible(page);
    refresh_status(app);
    invalidate(app);
}

fn do_replace(app: &mut App, all: bool) {
    // SAFETY: our edit controls.
    let replacement = unsafe { window_text(app.replace_edit) };
    // Masked patterns are find-only (roadmap item 12): replacing
    // "whatever the wildcards matched" has no single meaning, so say
    // so instead of replacing the pattern text literally.
    // SAFETY: our edit control.
    let find_text = unsafe { window_text(app.find_edit) };
    if masked_pattern_for(app, &find_text).is_some() {
        message_box(
            app.hwnd,
            "Masked patterns (like 4A ?? FF) are for finding only — replace does not use them.",
            "VeloEdit",
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    // Make sure the needle is current before replacing. A
    // background find (roadmap item 10) holds its matches outside
    // the editor, but replace works on the editor's own match
    // state — so the find below is flagged to re-run synchronously
    // and completely first (find_from_bar cancels the background
    // find when it sees the flag).
    if app.bg_active {
        app.find_sync_once = true;
    }
    find_from_bar(app, 0);
    let result = if all && app.find_in_selection {
        // Scoped replace-all (roadmap item 11): one undo entry, only
        // inside the captured scope. With no scope, find_from_bar
        // has already put the notice in the count label.
        match app.find_scope_range {
            Some((start, end)) => app
                .editor
                .replace_all_in_range(&replacement, start, end)
                .map(|n| n > 0),
            None => return,
        }
    } else if all {
        app.editor.replace_all(&replacement).map(|n| n > 0)
    } else {
        app.editor.replace_current(&replacement)
    };
    match result {
        Ok(true) => {
            app.note_edit();
            app.last_find_text.clear(); // force a fresh match list
            find_from_bar(app, 0);
        }
        Ok(false) => {}
        Err(err) => {
            message_box(
                app.hwnd,
                &format!("{err}"),
                "VeloEdit",
                MB_OK | MB_ICONERROR,
            );
        }
    }
    refresh_title(app);
    refresh_status(app);
    invalidate(app);
}

// ---------------------------------------------------------------------------
// Settings + theme application
// ---------------------------------------------------------------------------

fn save_settings(app: &App) {
    let path = crate::settings::settings_path();
    let _ = app.settings.save_to(&path);
}

/// Apply the committed theme/accent from settings and persist the
/// choice. This is the path every theme/accent command takes.
fn apply_theme(app: &mut App) {
    let theme = app.settings.theme.clone();
    let accent = app.settings.accent_override.clone();
    apply_theme_visual(app, &theme, accent.as_deref());
    save_settings(app);
}

/// The visual half of a theme change: resolve (theme, accent) and
/// rebuild everything derived from it — GDI chrome brushes are
/// recreated, the Direct2D brushes are dropped (recreated against
/// the live target on the next paint), the title bar recolours. It
/// never reads or writes settings, so the menu live preview
/// (WM_MENUSELECT) can show a choice without committing it.
fn apply_theme_visual(app: &mut App, theme_name: &str, accent: Option<&str>) {
    app.theme = Theme::resolve(theme_name, accent);
    // SAFETY: the GDI brushes are owned by the app; DeleteObject on a
    // brush not currently selected into any DC is safe.
    unsafe {
        if !app.panel_brush.is_null() {
            DeleteObject(app.panel_brush as HGDIOBJ);
        }
        if !app.edit_brush.is_null() {
            DeleteObject(app.edit_brush as HGDIOBJ);
        }
        if !app.line_brush.is_null() {
            DeleteObject(app.line_brush as HGDIOBJ);
        }
        if !app.accent_brush.is_null() {
            DeleteObject(app.accent_brush as HGDIOBJ);
        }
    }
    // SAFETY: CreateSolidBrush is a plain factory call; the brushes
    // are owned by the app and deleted in apply_theme / WM_DESTROY.
    unsafe {
        app.panel_brush = CreateSolidBrush(colorref(app.theme.panel));
        app.edit_brush = CreateSolidBrush(colorref(app.theme.bg));
        app.line_brush = CreateSolidBrush(colorref(app.theme.line));
        app.accent_brush = CreateSolidBrush(colorref(app.theme.accent));
    }
    apply_titlebar_theme(app);
    if let Some(mut brushes) = app.brushes.take() {
        brushes.release_all();
    }
    // Recreate the D2D brushes against the (possibly live) target.
    if !app.target.is_null() {
        let target = app.target;
        let t = &app.theme;
        app.brushes = Some(Brushes {
            bg: make_brush(target, t.bg),
            panel: make_brush(target, t.panel),
            text: make_brush(target, t.text),
            muted: make_brush(target, t.muted),
            line: make_brush(target, t.line),
            gutter: make_brush(target, t.gutter_bg),
            band: make_brush(target, blend_rgba(t.bg, t.panel, 0.5)),
            gutter_num: make_brush(target, blend_rgba(t.muted, t.text, 0.5)),
            selection: make_brush(target, t.selection),
            match_bg: make_brush(target, t.match_bg),
            accent: make_brush(target, t.accent),
        });
    }
    refresh_status(app);
    // The owner-drawn buttons pick their colours up on repaint.
    for button in app.toolbar.iter().chain(app.find_controls.iter()) {
        // SAFETY: our child windows.
        unsafe { InvalidateRect(*button, std::ptr::null(), 1) };
    }
    invalidate(app);
}

/// Begin the menu preview session if none is active, snapshotting
/// the committed (theme, accent) from settings. Previewing only ever
/// changes `app.theme`, never settings, so the snapshot is simply
/// what settings still hold; one snapshot covers the whole session,
/// across both the Theme and the Accent menus.
fn menu_preview_begin(app: &mut App) -> &mut MenuPreview {
    if app.menu_preview.is_none() {
        app.menu_preview = Some(MenuPreview {
            committed_theme: app.settings.theme.clone(),
            committed_accent: app.settings.accent_override.clone(),
            theme: app.settings.theme.clone(),
            accent: app.settings.accent_override.clone(),
        });
    }
    app.menu_preview.as_mut().expect("set above")
}

/// End a menu preview without a choice: put the committed
/// (theme, accent) back on screen and resync the menu check marks
/// (they are drawn from the committed settings, so this also undoes
/// anything the preview session might have left inconsistent).
/// A no-op when no preview is active — in particular after a click
/// committed a choice, because the command path discards the backup.
fn restore_menu_preview(app: &mut App) {
    if let Some(preview) = app.menu_preview.take() {
        apply_theme_visual(
            app,
            &preview.committed_theme,
            preview.committed_accent.as_deref(),
        );
        refresh_menu_checks(app);
    }
}

/// WM_MENUSELECT: as the user highlights items in the Theme or
/// Accent menu (mouse or keyboard — MENUSELECT covers both), preview
/// the highlighted choice in the window without persisting it. The
/// Custom… accent item has no preview: clicking it keeps its dialog
/// behaviour, as before.
fn on_menu_select(app: &mut App, wparam: WPARAM, lparam: LPARAM) {
    let menu = lparam as HMENU;
    let is_theme = menu == app.theme_menu;
    if !is_theme && menu != app.accent_menu {
        return;
    }
    let flags = (wparam >> 16) as u16;
    // 0x0010 is MF_POPUP (the low word is a position, not a command
    // id) and 0x2000 is MF_SYSMENU; neither names a previewable item.
    if flags & 0x0010 != 0 || flags & 0x2000 != 0 {
        return;
    }
    let id = wparam & 0xFFFF;
    // Work out the (theme, accent) pair to show, folding the
    // highlighted item into the session's current preview pair —
    // so a theme preview keeps the accent already in effect (a
    // previewed one, or the committed one) and vice versa.
    let pair = {
        if is_theme {
            let names = crate::theme::names();
            if !(CMD_THEME_BASE..CMD_THEME_BASE + names.len()).contains(&id) {
                return;
            }
            let preview = menu_preview_begin(app);
            preview.theme = names[id - CMD_THEME_BASE].to_string();
            Some((preview.theme.clone(), preview.accent.clone()))
        } else if id == CMD_ACCENT_BASE {
            let preview = menu_preview_begin(app);
            preview.accent = None;
            Some((preview.theme.clone(), preview.accent.clone()))
        } else if (CMD_ACCENT_BASE + 1..CMD_ACCENT_BASE + 1 + ACCENT_PRESETS.len()).contains(&id) {
            let preview = menu_preview_begin(app);
            preview.accent = Some(ACCENT_PRESETS[id - CMD_ACCENT_BASE - 1].1.to_string());
            Some((preview.theme.clone(), preview.accent.clone()))
        } else {
            None // Custom…: no preview.
        }
    };
    if let Some((theme, accent)) = pair {
        apply_theme_visual(app, &theme, accent.as_deref());
    }
}

fn rebuild_recent_menu(app: &App) {
    let menu = app.recent_menu;
    if menu.is_null() {
        return;
    }
    // SAFETY: menu surgery on our own menu; items are removed by
    // position from the end backwards so indices stay valid.
    unsafe {
        let count = GetMenuItemCount(menu);
        for i in (0..count).rev() {
            RemoveMenu(menu, i as u32, MF_BYPOSITION);
        }
        if app.settings.recent.is_empty() {
            let label = wstr("(No recent files)");
            AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        } else {
            for (i, path) in app.settings.recent.iter().enumerate() {
                let label = wstr(path);
                AppendMenuW(menu, MF_STRING, CMD_RECENT_BASE + i, label.as_ptr());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery (never lose the user's work), in two forms:
//
// * Delta recovery (roadmap item 7) for documents backed by a file:
//   data/recovery2.bin holds the piece-table state — the edit log —
//   plus the original file's fingerprint (src/recovery.rs). Any
//   document size; the file itself is never copied.
// * A content snapshot for documents with no backing file:
//   data/recovery.bin + data/recovery.json, capped at
//   RECOVERY_MAX_BYTES as before.
//
// Both are written debounced (1 s after the last edit) and deleted
// on a successful save, a clean exit, and a fresh open.
// ---------------------------------------------------------------------------

fn recovery_paths() -> (PathBuf, PathBuf) {
    let dir = crate::settings::data_dir();
    (dir.join("recovery.bin"), dir.join("recovery.json"))
}

fn recovery2_path() -> PathBuf {
    crate::settings::data_dir().join("recovery2.bin")
}

fn delete_recovery_files() {
    let (bin, json) = recovery_paths();
    let _ = std::fs::remove_file(bin);
    let _ = std::fs::remove_file(json);
    let _ = std::fs::remove_file(recovery2_path());
}

/// Write whichever recovery form fits the current document.
/// Returns false when the delta form applies but the original
/// file's fingerprint has not arrived yet — the caller (the tick)
/// then keeps the debounce open and retries shortly.
fn write_recovery(app: &App) -> bool {
    if app.editor.path().is_some() && app.doc_mmap.is_some() {
        match &app.doc_fingerprint {
            Some(fingerprint) => {
                let path = app.editor.path().expect("checked above");
                let state = crate::recovery::capture(
                    app.editor.table(),
                    path,
                    fingerprint,
                    app.editor.caret(),
                    app.editor.encoding(),
                );
                let dir = crate::settings::data_dir();
                let _ = std::fs::create_dir_all(&dir);
                if std::fs::write(recovery2_path(), state.to_bytes()).is_ok() {
                    // The delta record supersedes any snapshot of
                    // the same document.
                    let (bin, json) = recovery_paths();
                    let _ = std::fs::remove_file(bin);
                    let _ = std::fs::remove_file(json);
                }
                true
            }
            None => false,
        }
    } else {
        write_snapshot_recovery(app);
        true
    }
}

fn write_snapshot_recovery(app: &App) {
    if app.editor.total_len() > RECOVERY_MAX_BYTES {
        return;
    }
    let (bin, json) = recovery_paths();
    if let Some(parent) = bin.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let bytes = app.editor.table().read(0, app.editor.total_len());
    if std::fs::write(&bin, &bytes).is_err() {
        return;
    }
    let path = app
        .editor
        .path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let saved_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = format!(
        "{{\"path\": \"{}\", \"saved_at_unix\": {}, \"encoding\": \"{}\"}}",
        crate::settings::json_escape(&path),
        saved_at,
        app.editor.encoding().name(),
    );
    let _ = std::fs::write(&json, meta);
}

/// Extract a `"key": "value"` string from our own recovery JSON
/// (written by [`write_recovery`] with [`crate::settings::json_escape`],
/// so only `\"`, `\\` and `\u00XX` escapes can appear).
fn json_string_value(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = json.find(&needle)?;
    let after = &json[at + needle.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let inner = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            c => out.push(c),
        }
    }
}

/// Startup recovery check: the delta record first (roadmap item
/// 7), then the content snapshot. Returns the editor to start with
/// — plus, for a delta recovery, the original file's fingerprint,
/// so the tracking setup can adopt it instead of recomputing it —
/// or `None` for the normal startup path.
fn recovery_startup_editor() -> Option<(Editor, Option<crate::recovery::FileFingerprint>)> {
    if let Some(handled) = delta_recovery_startup() {
        return handled;
    }
    snapshot_recovery_startup().map(|editor| (editor, None))
}

/// The delta half of the startup check. The outer `Option` says
/// whether a delta record existed and was dealt with; the inner
/// one is the startup result in [`recovery_startup_editor`]'s
/// shape (`None` = start blank).
fn delta_recovery_startup() -> Option<Option<(Editor, Option<crate::recovery::FileFingerprint>)>> {
    let path2 = recovery2_path();
    let bytes = std::fs::read(&path2).ok()?;
    let state = match crate::recovery::RecoveryState::from_bytes(&bytes) {
        Ok(state) => state,
        Err(_) => {
            // A corrupt record can never be rebuilt; drop it and
            // let the snapshot check run.
            let _ = std::fs::remove_file(&path2);
            return None;
        }
    };
    let table = match state.rebuild() {
        Ok(table) => table,
        Err(crate::recovery::RecoveryError::OriginalChanged(_)) => {
            // The file moved on without these edits: say so plainly,
            // drop the recovery, and start normally.
            delete_recovery_files();
            message_box(
                std::ptr::null_mut(),
                &format!(
                    "VeloEdit found unsaved changes from last time, but the file they belong to \u{2014} {} \u{2014} has changed since they were recorded, so the changes were not restored.",
                    state.original_path
                ),
                "VeloEdit",
                MB_OK | MB_ICONINFORMATION,
            );
            return Some(None);
        }
        Err(_) => {
            // The original is gone or unreadable: nothing to rebuild
            // against. Drop the record and let the snapshot check run.
            let _ = std::fs::remove_file(&path2);
            return None;
        }
    };
    let answer = message_box(
        std::ptr::null_mut(),
        &format!(
            "VeloEdit closed unexpectedly last time.\n\nRecover the unsaved changes to {}?",
            state.original_path
        ),
        "VeloEdit",
        MB_YESNO | MB_ICONQUESTION,
    );
    if answer != IDYES {
        delete_recovery_files();
        return Some(None);
    }
    let path = PathBuf::from(&state.original_path);
    let sample = table.read(0, table.len().min(64 * 1024));
    let mut detection = encoding::detect(&sample);
    // The capture recorded the document's encoding; the sample
    // detection only contributes the BOM length.
    detection.encoding = state.encoding;
    let mut editor = Editor::from_table(table, detection, Some(path), false);
    editor.mark_dirty();
    editor.move_caret_to(state.caret, false);
    let fingerprint = crate::recovery::FileFingerprint {
        size: state.original_size,
        mtime_nanos: state.original_mtime_nanos,
        sha256: state.original_sha256,
    };
    Some(Some((editor, Some(fingerprint))))
}

/// The snapshot half of the startup check: if a snapshot exists and
/// the file on disk does not match it, ask whether to open the
/// recovered copy. Returns the editor to start with, or `None` for
/// the normal startup path.
fn snapshot_recovery_startup() -> Option<Editor> {
    let (bin, json) = recovery_paths();
    let meta = std::fs::read_to_string(&json).ok()?;
    let bytes = std::fs::read(&bin).ok()?;
    let path_string = json_string_value(&meta, "path")?;
    let differs = if path_string.is_empty() {
        true // an untitled document exists only in the snapshot
    } else {
        let path = Path::new(&path_string);
        match std::fs::read(path) {
            Ok(on_disk) => on_disk != bytes,
            Err(_) => true,
        }
    };
    if !differs {
        delete_recovery_files();
        return None;
    }
    let answer = message_box(
        std::ptr::null_mut(),
        &format!(
            "VeloEdit closed unexpectedly last time.\n\nRecover the unsaved changes to {}?",
            if path_string.is_empty() {
                "an untitled document"
            } else {
                &path_string
            }
        ),
        "VeloEdit",
        MB_YESNO | MB_ICONQUESTION,
    );
    if answer != IDYES {
        delete_recovery_files();
        return None;
    }
    let path = if path_string.is_empty() {
        None
    } else {
        Some(PathBuf::from(path_string))
    };
    let mut editor = Editor::open_from_bytes(bytes, path, false);
    editor.mark_dirty();
    Some(editor)
}

// ---------------------------------------------------------------------------
// Child controls, menus, tooltips (created once, at WM_CREATE)
// ---------------------------------------------------------------------------

fn create_child(app: &App, class: &str, text: &str, style: u32, ex_style: u32, id: usize) -> HWND {
    let class_w = wstr(class);
    let text_w = wstr(text);
    // SAFETY: the class names are standard window classes (or the
    // tooltip class); parent and instance handles are valid; the id
    // doubles as the child HMENU, the standard child-control scheme.
    unsafe {
        CreateWindowExW(
            ex_style,
            class_w.as_ptr(),
            text_w.as_ptr(),
            style,
            0,
            0,
            10,
            10,
            app.hwnd,
            id as HMENU,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    }
}

fn add_tooltip(tooltip: HWND, parent: HWND, control: HWND, text: &str) {
    if tooltip.is_null() || control.is_null() {
        return;
    }
    // The tooltip control keeps the text pointer, so the buffer is
    // deliberately leaked — a few hundred bytes for the process life.
    let text_buf: &'static mut [u16] = Box::leak(wstr(text).into_boxed_slice());
    // SAFETY: TTTOOLINFOW is a plain C struct, zero-initialised then
    // filled; TTF_IDISHWND makes uId the control's handle.
    let mut info: TTTOOLINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
    info.uFlags = TTF_SUBCLASS | TTF_IDISHWND;
    info.hwnd = parent;
    info.uId = control as usize;
    info.lpszText = text_buf.as_mut_ptr();
    // SAFETY: the tooltip window is live; `info` outlives the call.
    unsafe {
        SendMessageW(
            tooltip,
            TTM_ADDTOOLW,
            0,
            &info as *const TTTOOLINFOW as LPARAM,
        )
    };
}

const PROP_OLD_PROC: &str = "VeloEdit.OldProc";
const PROP_MAIN_HWND: &str = "VeloEdit.MainHwnd";

/// Subclass a find-bar edit so Enter/Esc and the global shortcuts work
/// while it has focus (the main window proc never sees its keys).
fn subclass_edit(edit: HWND, main: HWND) {
    let name_old = wstr(PROP_OLD_PROC);
    let name_main = wstr(PROP_MAIN_HWND);
    // SAFETY: `edit` is our child window; the old proc is stored in a
    // window property before the new proc is installed.
    unsafe {
        let old = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as usize as isize);
        SetPropW(edit, name_old.as_ptr(), old as HANDLE);
        SetPropW(edit, name_main.as_ptr(), main as HANDLE);
    }
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let name_old = wstr(PROP_OLD_PROC);
    let name_main = wstr(PROP_MAIN_HWND);
    // SAFETY: the props were stored by subclass_edit before this proc
    // was installed, and are removed at WM_NCDESTROY below.
    let old = unsafe { GetPropW(hwnd, name_old.as_ptr()) } as isize;
    let main = unsafe { GetPropW(hwnd, name_main.as_ptr()) } as HWND;
    if msg == WM_KEYDOWN {
        let vk = wparam as u32;
        // SAFETY: GetKeyState(0x11 = VK_CONTROL) is a plain state read.
        let ctrl = unsafe { GetKeyState(0x11) } as u16 & 0x8000 != 0;
        let cmd = match vk {
            0x0D => Some(CMD_FIND_NEXT),       // Enter: find next
            0x1B => Some(CMD_FIND_CLOSE),      // Esc: close the bar
            0x4F if ctrl => Some(CMD_OPEN),    // Ctrl+O
            0x53 if ctrl => Some(CMD_SAVE),    // Ctrl+S
            0x46 if ctrl => Some(CMD_FIND),    // Ctrl+F
            0x48 if ctrl => Some(CMD_REPLACE), // Ctrl+H
            _ => None,
        };
        if let Some(cmd) = cmd {
            // SAFETY: `main` is the main window handle stored at
            // subclass time; the command is handled there.
            unsafe { PostMessageW(main, WM_COMMAND, cmd, 0) };
            return 0;
        }
    }
    if msg == WM_NCDESTROY {
        // SAFETY: restore the original proc and drop our props before
        // the window is destroyed.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
            RemovePropW(hwnd, name_old.as_ptr());
            RemovePropW(hwnd, name_main.as_ptr());
        }
    }
    let proc_fn: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: proc_fn is the edit control's original window procedure,
    // retrieved from our own property above.
    unsafe { CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam) }
}

fn create_children(app: &mut App) {
    // Tooltip control (TTS_ALWAYSTIP = 0x1, TTS_NOPREFIX = 0x2).
    let tooltip_class = wstr("tooltips_class32");
    // SAFETY: standard tooltip window creation.
    let tooltip = unsafe {
        CreateWindowExW(
            0,
            tooltip_class.as_ptr(),
            std::ptr::null(),
            WS_POPUP | 0x1 | 0x2,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };

    // Toolbar: owner-drawn buttons (themed in WM_DRAWITEM), ids shared
    // with the menus. The first six flow from the left; the Text / Hex
    // mode toggles are created last and anchored to the right end of
    // the row by layout_children.
    let toolbar_defs: [(usize, &str, &str); 6] = [
        (CMD_OPEN, "Open", "Open a file (Ctrl+O)"),
        (CMD_SAVE, "Save", "Save the file (Ctrl+S)"),
        (CMD_UNDO, "Undo", "Undo the last change (Ctrl+Z)"),
        (CMD_REDO, "Redo", "Redo the last undone change (Ctrl+Y)"),
        (CMD_FIND, "Find", "Find text (Ctrl+F)"),
        (CMD_REPLACE, "Replace", "Find and replace (Ctrl+H)"),
    ];
    for (id, label, tip) in toolbar_defs {
        let button = create_child(app, "Button", label, WS_CHILD | WS_VISIBLE | 0xB, 0, id);
        app.toolbar.push(button);
        add_tooltip(tooltip, app.hwnd, button, tip);
    }
    let mode_defs: [(usize, &str, &str); 2] = [
        (CMD_MODE_TEXT, "Text", "Text mode"),
        (CMD_MODE_HEX, "Hex", "Hex mode (F6 toggles)"),
    ];
    for (id, label, tip) in mode_defs {
        let button = create_child(app, "Button", label, WS_CHILD | WS_VISIBLE | 0xB, 0, id);
        app.toolbar.push(button);
        add_tooltip(tooltip, app.hwnd, button, tip);
    }

    // Find bar (created hidden; shown by Ctrl+F / Ctrl+H). The order
    // in find_controls is fixed — layout_children indexes into it.
    let edit_style = WS_CHILD | 0x80; // ES_AUTOHSCROLL
    let button_style = WS_CHILD | 0xB; // BS_OWNERDRAW
                                       // SS_CENTERIMAGE (0x200) centres the statics' text vertically,
                                       // so the labels and the match count sit on the same line as the
                                       // buttons' centred labels instead of hugging the top of the bar.
    let label_style = WS_CHILD | 0x200;
    app.find_label = create_child(app, "Static", "Find:", label_style, 0, 0);
    app.replace_label = create_child(app, "Static", "Replace:", label_style, 0, 0);
    app.find_edit = create_child(app, "Edit", "", edit_style, 0, ID_FIND_EDIT);
    let prev = create_child(app, "Button", "Prev", button_style, 0, CMD_FIND_PREV);
    let next = create_child(app, "Button", "Next", button_style, 0, CMD_FIND_NEXT);
    app.find_count = create_child(app, "Static", "", label_style, 0, ID_FIND_COUNT);
    app.replace_edit = create_child(app, "Edit", "", edit_style, 0, ID_REPLACE_EDIT);
    let replace = create_child(app, "Button", "Replace", button_style, 0, CMD_DO_REPLACE);
    let replace_all = create_child(
        app,
        "Button",
        "Replace All",
        button_style,
        0,
        CMD_DO_REPLACE_ALL,
    );
    let close = create_child(app, "Button", "\u{2715}", button_style, 0, CMD_FIND_CLOSE);
    app.find_controls = vec![
        app.find_edit,
        prev,
        next,
        app.find_count,
        app.replace_edit,
        replace,
        replace_all,
        close,
    ];
    add_tooltip(tooltip, app.hwnd, app.find_edit, "Text to find");
    add_tooltip(tooltip, app.hwnd, app.replace_edit, "Replacement text");
    add_tooltip(
        tooltip,
        app.hwnd,
        prev,
        "Previous match (Shift+Enter in the find box goes forward)",
    );
    add_tooltip(tooltip, app.hwnd, next, "Next match (Enter)");
    add_tooltip(tooltip, app.hwnd, replace, "Replace the current match");
    add_tooltip(
        tooltip,
        app.hwnd,
        replace_all,
        "Replace every match (one undo step)",
    );
    add_tooltip(tooltip, app.hwnd, close, "Close the find bar (Esc)");
    // The "In selection" scope toggle (roadmap item 11): an
    // owner-drawn button like the others, so it draws in the same
    // themed style; its state lives in `app.find_in_selection` —
    // while on, the draw pass gives it the accent state (like the
    // Text / Hex toggles) and its text gains a ✓ prefix.
    app.find_scope_check = create_child(
        app,
        "Button",
        "In selection",
        button_style,
        0,
        CMD_FIND_SCOPE,
    );
    app.find_controls.push(app.find_scope_check);
    add_tooltip(
        tooltip,
        app.hwnd,
        app.find_scope_check,
        "Search and replace only inside the current selection",
    );
    subclass_edit(app.find_edit, app.hwnd);
    subclass_edit(app.replace_edit, app.hwnd);

    // Status strip: two static controls, themed via WM_CTLCOLORSTATIC
    // — position on the left, document facts right-aligned (SS_RIGHT
    // = 0x2).
    app.status = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE, 0, 0);
    app.status_right = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE | 0x2, 0, 0);

    // External-change notice bar (roadmap item 34): created hidden,
    // shown by check_external_change. The label is a static (themed
    // via WM_CTLCOLORSTATIC like the find bar's), the buttons are
    // owner-drawn in the toolbar family.
    app.change_label = create_child(app, "Static", "This file changed on disk.", WS_CHILD, 0, 0);
    app.change_reload_btn =
        create_child(app, "Button", "Reload", button_style, 0, CMD_CHANGE_RELOAD);
    app.change_ignore_btn =
        create_child(app, "Button", "Ignore", button_style, 0, CMD_CHANGE_IGNORE);
    add_tooltip(
        tooltip,
        app.hwnd,
        app.change_reload_btn,
        "Throw away this copy and open the file from disk (you are asked about unsaved changes first)",
    );
    add_tooltip(
        tooltip,
        app.hwnd,
        app.change_ignore_btn,
        "Keep editing this copy; the notice returns if the file changes again",
    );
}

fn menu_item(menu: HMENU, id: usize, text: &str) {
    let wide = wstr(text);
    // SAFETY: our menu; the string outlives the call.
    unsafe { AppendMenuW(menu, MF_STRING, id, wide.as_ptr()) };
}

fn menu_sep(menu: HMENU) {
    // SAFETY: our menu; a separator needs no text.
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null()) };
}

fn menu_popup(bar: HMENU, popup: HMENU, text: &str) {
    let wide = wstr(text);
    // SAFETY: our menus; the popup handle becomes the submenu.
    unsafe { AppendMenuW(bar, MF_POPUP, popup as usize, wide.as_ptr()) };
}

fn build_menus(app: &mut App) {
    // SAFETY: the whole menu tree is built from fresh popup menus and
    // installed on our window; nothing else holds these handles.
    unsafe {
        let bar = CreateMenu();

        let file = CreatePopupMenu();
        app.file_menu = file;
        menu_item(file, CMD_OPEN, "Open\tCtrl+O");
        menu_item(file, CMD_SAVE, "Save\tCtrl+S");
        menu_item(file, CMD_SAVE_AS, "Save As\u{2026}");
        menu_item(file, CMD_SAVE_SELECTION, "Save Selection to File\u{2026}");
        menu_item(file, CMD_SPLIT_FILE, "Split File Copy\u{2026}");
        menu_item(file, CMD_FILE_HASHES, "File Hashes\u{2026}");
        // Save Options (roadmap items 31/32): two radio groups —
        // what happens to line endings and to the byte-order mark
        // on every save. The choices persist in settings.
        app.save_options_menu = CreatePopupMenu();
        menu_item(
            app.save_options_menu,
            CMD_SAVE_EOL_BASE,
            "Keep Line Endings",
        );
        menu_item(
            app.save_options_menu,
            CMD_SAVE_EOL_BASE + 1,
            "Convert to LF",
        );
        menu_item(
            app.save_options_menu,
            CMD_SAVE_EOL_BASE + 2,
            "Convert to CRLF",
        );
        menu_sep(app.save_options_menu);
        menu_item(
            app.save_options_menu,
            CMD_SAVE_BOM_BASE,
            "Keep Byte-Order Mark",
        );
        menu_item(
            app.save_options_menu,
            CMD_SAVE_BOM_BASE + 1,
            "Add Byte-Order Mark",
        );
        menu_item(
            app.save_options_menu,
            CMD_SAVE_BOM_BASE + 2,
            "Remove Byte-Order Mark",
        );
        menu_popup(file, app.save_options_menu, "Save Options");
        app.recent_menu = CreatePopupMenu();
        menu_popup(file, app.recent_menu, "Recent");
        menu_sep(file);
        menu_item(file, CMD_EXPLORER_MENU, "Explorer Menu Item");
        menu_sep(file);
        menu_item(file, CMD_EXIT, "Exit");

        let edit = CreatePopupMenu();
        menu_item(edit, CMD_UNDO, "Undo\tCtrl+Z");
        menu_item(edit, CMD_REDO, "Redo\tCtrl+Y");
        menu_sep(edit);
        menu_item(edit, CMD_CUT, "Cut\tCtrl+X");
        menu_item(edit, CMD_COPY, "Copy\tCtrl+C");
        menu_item(edit, CMD_PASTE, "Paste\tCtrl+V");
        menu_sep(edit);
        menu_item(edit, CMD_FIND, "Find\tCtrl+F");
        menu_item(edit, CMD_REPLACE, "Replace\tCtrl+H");
        menu_sep(edit);
        menu_item(edit, CMD_SELECT_ALL, "Select All\tCtrl+A");
        menu_sep(edit);
        menu_item(edit, CMD_SORT_LINES, "Sort Lines in Selection");
        menu_item(edit, CMD_DEDUP_LINES, "Remove Duplicate Lines in Selection");

        let search = CreatePopupMenu();
        menu_item(search, CMD_FIND, "Find\u{2026}\tCtrl+F");
        menu_item(search, CMD_REPLACE, "Replace\u{2026}\tCtrl+H");
        menu_sep(search);
        menu_item(search, CMD_FIND_NEXT, "Find Next\tF3");
        menu_item(search, CMD_FIND_PREV, "Find Previous\tShift+F3");

        let view = CreatePopupMenu();
        app.view_menu = view;
        menu_item(view, CMD_MODE_TEXT, "Text Mode");
        menu_item(view, CMD_MODE_HEX, "Hex Mode\tF6");
        menu_sep(view);
        menu_item(view, CMD_INSPECTOR, "Data Inspector");
        menu_sep(view);
        app.theme_menu = CreatePopupMenu();
        for (i, name) in crate::theme::names().iter().enumerate() {
            menu_item(app.theme_menu, CMD_THEME_BASE + i, name);
        }
        menu_popup(view, app.theme_menu, "Theme");
        app.accent_menu = CreatePopupMenu();
        menu_item(app.accent_menu, CMD_ACCENT_BASE, "Theme Default");
        for (i, (name, _)) in ACCENT_PRESETS.iter().enumerate() {
            menu_item(app.accent_menu, CMD_ACCENT_BASE + 1 + i, name);
        }
        menu_sep(app.accent_menu);
        menu_item(app.accent_menu, CMD_ACCENT_CUSTOM, "Custom\u{2026}");
        menu_popup(view, app.accent_menu, "Accent");
        menu_sep(view);
        menu_item(view, CMD_FONT_BIGGER, "Font Size +\tCtrl++");
        menu_item(view, CMD_FONT_SMALLER, "Font Size \u{2212}\tCtrl+-");
        menu_sep(view);
        menu_item(view, CMD_VIEW_RULER, "Column Ruler");
        menu_sep(view);
        // Hex layout (roadmap items 13/14): the row width and the
        // grouping are display choices for hex mode; text mode
        // never sees them.
        app.hex_cols_menu = CreatePopupMenu();
        for (i, label) in [
            "8 Columns",
            "16 Columns",
            "24 Columns",
            "32 Columns",
            "64 Columns",
            "Fit to Window",
        ]
        .iter()
        .enumerate()
        {
            menu_item(app.hex_cols_menu, CMD_HEXCOLS_BASE + i, label);
        }
        menu_popup(view, app.hex_cols_menu, "Hex Columns");
        app.hex_group_menu = CreatePopupMenu();
        for (i, label) in [
            "Bytes (No Grouping)",
            "Words (2 Bytes)",
            "Dwords (4 Bytes)",
            "Qwords (8 Bytes)",
        ]
        .iter()
        .enumerate()
        {
            menu_item(app.hex_group_menu, CMD_HEXGROUP_BASE + i, label);
        }
        menu_sep(app.hex_group_menu);
        menu_item(app.hex_group_menu, CMD_HEX_ENDIAN, "Big-Endian Groups");
        menu_popup(view, app.hex_group_menu, "Hex Grouping");
        menu_sep(view);
        // Relative offsets (roadmap item 16): session-only.
        menu_item(view, CMD_REL_OFFSETS, "Relative Offsets");
        menu_item(view, CMD_SET_OFFSET_BASE, "Set Offset Base at Caret");

        let help = CreatePopupMenu();
        menu_item(help, CMD_GUIDE, "Built-in Guide\tF1");
        menu_item(help, CMD_ABOUT, "About VeloEdit");

        menu_popup(bar, file, "File");
        menu_popup(bar, edit, "Edit");
        menu_popup(bar, search, "Search");
        menu_popup(bar, view, "View");
        menu_popup(bar, help, "Help");
        SetMenu(app.hwnd, bar);
    }
    refresh_menu_checks(app);
    rebuild_recent_menu(app);
}

/// Sync the radio/check marks with the active mode, theme and accent.
fn refresh_menu_checks(app: &App) {
    // SAFETY: menu state changes on our own menus.
    unsafe {
        let mode_id = match app.editor.mode() {
            EditorMode::Text => CMD_MODE_TEXT,
            EditorMode::Hex => CMD_MODE_HEX,
        };
        CheckMenuRadioItem(
            app.view_menu,
            CMD_MODE_TEXT as u32,
            CMD_MODE_HEX as u32,
            mode_id as u32,
            MF_BYCOMMAND,
        );
        let names = crate::theme::names();
        let current = names
            .iter()
            .position(|n| *n == app.settings.theme)
            .unwrap_or(0);
        CheckMenuRadioItem(
            app.theme_menu,
            CMD_THEME_BASE as u32,
            (CMD_THEME_BASE + names.len() - 1) as u32,
            (CMD_THEME_BASE + current) as u32,
            MF_BYCOMMAND,
        );
        let active_accent = match &app.settings.accent_override {
            None => CMD_ACCENT_BASE,
            Some(hex) => ACCENT_PRESETS
                .iter()
                .position(|(_, preset)| preset.eq_ignore_ascii_case(hex))
                .map(|i| CMD_ACCENT_BASE + 1 + i)
                .unwrap_or(CMD_ACCENT_CUSTOM),
        };
        for i in 0..=(ACCENT_PRESETS.len() + 1) {
            let id = CMD_ACCENT_BASE + i;
            let flags = MF_BYCOMMAND | if id == active_accent { MF_CHECKED } else { 0 };
            CheckMenuItem(app.accent_menu, id as u32, flags);
        }
        // The column ruler item is checked whenever a column is set
        // (the item cycles Off → 80 → 120 → Off when chosen).
        CheckMenuItem(
            app.view_menu,
            CMD_VIEW_RULER as u32,
            MF_BYCOMMAND
                | if app.settings.ruler_column > 0 {
                    MF_CHECKED
                } else {
                    0
                },
        );
        // The Explorer menu item (roadmap item 38) is checked while
        // the registry entry exists; choosing it toggles the entry.
        if !app.file_menu.is_null() {
            CheckMenuItem(
                app.file_menu,
                CMD_EXPLORER_MENU as u32,
                MF_BYCOMMAND
                    | if shell_menu_is_installed() {
                        MF_CHECKED
                    } else {
                        0
                    },
            );
        }
        // Data inspector (roadmap item 15): checked while open.
        CheckMenuItem(
            app.view_menu,
            CMD_INSPECTOR as u32,
            MF_BYCOMMAND
                | if app.settings.inspector_open {
                    MF_CHECKED
                } else {
                    0
                },
        );
        // Hex layout radios (roadmap items 13/14).
        if !app.hex_cols_menu.is_null() {
            let idx = HEX_COLS_MENU
                .iter()
                .position(|&c| c == app.settings.hex_cols)
                .unwrap_or(1);
            CheckMenuRadioItem(
                app.hex_cols_menu,
                CMD_HEXCOLS_BASE as u32,
                (CMD_HEXCOLS_BASE + HEX_COLS_MENU.len() - 1) as u32,
                (CMD_HEXCOLS_BASE + idx) as u32,
                MF_BYCOMMAND,
            );
        }
        if !app.hex_group_menu.is_null() {
            let gidx = [1u32, 2, 4, 8]
                .iter()
                .position(|&g| g == app.settings.hex_group)
                .unwrap_or(0);
            CheckMenuRadioItem(
                app.hex_group_menu,
                CMD_HEXGROUP_BASE as u32,
                (CMD_HEXGROUP_BASE + 3) as u32,
                (CMD_HEXGROUP_BASE + gidx) as u32,
                MF_BYCOMMAND,
            );
            CheckMenuItem(
                app.hex_group_menu,
                CMD_HEX_ENDIAN as u32,
                MF_BYCOMMAND
                    | if app.settings.hex_endian {
                        MF_CHECKED
                    } else {
                        0
                    },
            );
        }
        // Relative offsets (roadmap item 16): checked while on.
        CheckMenuItem(
            app.view_menu,
            CMD_REL_OFFSETS as u32,
            MF_BYCOMMAND | if app.rel_offsets { MF_CHECKED } else { 0 },
        );
        // Save Options radios (roadmap items 31/32).
        if !app.save_options_menu.is_null() {
            let eol_idx = match app.settings.save_eol_choice() {
                crate::save::EolChoice::Lf => 1,
                crate::save::EolChoice::Crlf => 2,
                crate::save::EolChoice::Keep => 0,
            };
            CheckMenuRadioItem(
                app.save_options_menu,
                CMD_SAVE_EOL_BASE as u32,
                (CMD_SAVE_EOL_BASE + 2) as u32,
                (CMD_SAVE_EOL_BASE + eol_idx) as u32,
                MF_BYCOMMAND,
            );
            let bom_idx = match app.settings.save_bom_choice() {
                crate::save::BomChoice::Add => 1,
                crate::save::BomChoice::Remove => 2,
                crate::save::BomChoice::Keep => 0,
            };
            CheckMenuRadioItem(
                app.save_options_menu,
                CMD_SAVE_BOM_BASE as u32,
                (CMD_SAVE_BOM_BASE + 2) as u32,
                (CMD_SAVE_BOM_BASE + bom_idx) as u32,
                MF_BYCOMMAND,
            );
        }
    }
    // The toolbar's Text / Hex toggle buttons show their state in
    // their owner-drawn faces; a mode change repaints them via
    // set_mode, and menu opens repaint nothing, so there is nothing
    // further to sync here.
}

// ---------------------------------------------------------------------------
// Help window (F1): a modeless window with a read-only guide edit.
// ---------------------------------------------------------------------------

const HELP_CLASS: &str = "VeloEditHelp";

unsafe extern "system" fn help_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let class = wstr("Edit");
            let guide = wstr(GUIDE_TEXT);
            // SAFETY: standard child creation; the guide text becomes
            // the edit control's initial content.
            let edit = unsafe {
                CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    class.as_ptr(),
                    guide.as_ptr(),
                    WS_CHILD | WS_VISIBLE | WS_VSCROLL | 0x4 | 0x800 | 0x40, // ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL
                    0,
                    0,
                    100,
                    100,
                    hwnd,
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            // SAFETY: stash the child handle for WM_SIZE.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, edit as isize) };
            0
        }
        WM_SIZE => {
            // SAFETY: the child handle was stored at WM_CREATE.
            let edit = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as HWND;
            if !edit.is_null() {
                let w = (lparam & 0xFFFF) as i32;
                let h = ((lparam >> 16) & 0xFFFF) as i32;
                // SAFETY: our child window.
                unsafe { MoveWindow(edit, 0, 0, w, h, 1) };
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: our help window; modeless, so close destroys it.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn show_help(app: &mut App) {
    // SAFETY: the stored handle is checked for liveness before reuse.
    if !app.help_hwnd.is_null() && unsafe { IsWindow(app.help_hwnd) } != 0 {
        // SAFETY: a live window of ours.
        unsafe { SetForegroundWindow(app.help_hwnd) };
        return;
    }
    let class = wstr(HELP_CLASS);
    let title = wstr("VeloEdit \u{2014} Guide");
    // SAFETY: the help class is registered in run_gui; sizes are
    // DPI-scaled from the main window's DPI.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(620.0),
            app.px(540.0),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    if !hwnd.is_null() {
        app.help_hwnd = hwnd;
        // SAFETY: a fresh window of ours.
        unsafe { ShowWindow(hwnd, SW_SHOW) };
    }
}

// ---------------------------------------------------------------------------
// Commands and input
// ---------------------------------------------------------------------------

fn after_caret_move(app: &mut App) {
    let page = page_rows(app);
    app.editor.ensure_caret_visible(page);
    refresh_status(app);
    invalidate(app);
}

fn after_edit(app: &mut App) {
    app.pending_goto = None; // an edit overtakes a pending --line jump
    app.note_edit();
    let page = page_rows(app);
    app.editor.ensure_caret_visible(page);
    refresh_title(app);
    refresh_status(app);
    invalidate(app);
}

fn set_mode(app: &mut App, mode: EditorMode) {
    if app.editor.mode() == mode {
        return;
    }
    app.editor.set_mode(mode);
    refresh_menu_checks(app);
    refresh_status(app);
    // The mode toggle buttons redraw their active state.
    for button in app.toolbar.iter().skip(6) {
        // SAFETY: our child windows.
        unsafe { InvalidateRect(*button, std::ptr::null(), 1) };
    }
    invalidate(app);
}

fn scroll_by(app: &mut App, delta: i64) {
    match app.editor.mode() {
        EditorMode::Text => {
            let next = (app.editor.first_line() as i64 + delta).max(0) as u64;
            app.editor.set_first_line(next);
        }
        EditorMode::Hex => {
            let next = (app.editor.first_row() as i64 + delta).max(0) as u64;
            app.editor.set_first_row(next);
        }
    }
    invalidate(app);
}

fn on_wheel(app: &mut App, wparam: WPARAM) {
    let delta = ((wparam >> 16) as u16) as i16 as i32;
    // Ctrl+wheel zooms the font instead of scrolling: MK_CONTROL
    // (0x0008) rides in the message's low word. Zoom steps go
    // through the Font Bigger / Font Smaller commands themselves,
    // so clamping and persistence are exactly the menu behaviour;
    // a separate accumulator keeps partial zoom and scroll
    // remainders from mixing.
    if wparam & 0xFFFF & 0x0008 != 0 {
        app.wheel_zoom_rem += delta;
        while app.wheel_zoom_rem >= 120 {
            on_command(app, CMD_FONT_BIGGER);
            app.wheel_zoom_rem -= 120;
        }
        while app.wheel_zoom_rem <= -120 {
            on_command(app, CMD_FONT_SMALLER);
            app.wheel_zoom_rem += 120;
        }
        return;
    }
    let mut lines: u32 = 3;
    // SAFETY: SPI_GETWHEELSCROLLLINES (104) writes one u32 through the
    // pointer; a failure leaves the default of 3.
    unsafe {
        SystemParametersInfoW(104, 0, &mut lines as *mut u32 as *mut c_void, 0);
    }
    if lines == u32::MAX {
        // WHEEL_PAGESCROLL: one notch scrolls a whole page.
        let page = page_rows(app) as i64;
        scroll_by(app, if delta > 0 { -page } else { page });
        return;
    }
    app.wheel_rem += delta;
    let per = lines.max(1) as i64;
    while app.wheel_rem >= 120 {
        scroll_by(app, -per);
        app.wheel_rem -= 120;
    }
    while app.wheel_rem <= -120 {
        scroll_by(app, per);
        app.wheel_rem += 120;
    }
}

/// The text a Copy/Cut puts on the clipboard: the decoded selection in
/// text mode; in hex mode, the selected bytes as a hex string.
fn copy_text(app: &mut Editor) -> Option<String> {
    app.selection_range()?;
    match app.mode() {
        EditorMode::Text => Some(app.selected_text_lossy()),
        EditorMode::Hex => Some(
            app.selected_bytes()
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

/// Parse a hex string (whitespace ignored) for hex-mode paste.
fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if digits.is_empty() || digits.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        let s = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

fn on_command(app: &mut App, id: usize) {
    match id {
        CMD_OPEN => {
            if prompt_save_if_dirty(app) {
                if let Some(path) = file_dialog(app, false, None) {
                    open_document(app, &path);
                }
            }
        }
        CMD_SAVE => {
            do_save(app);
        }
        CMD_SAVE_AS => {
            do_save_as(app);
        }
        CMD_SAVE_SELECTION => {
            do_save_selection(app);
        }
        CMD_SPLIT_FILE => {
            do_split_file(app);
        }
        CMD_FILE_HASHES => {
            do_file_hashes(app);
        }
        CMD_EXPLORER_MENU => {
            // Toggle the opt-in Explorer item (roadmap item 38): the
            // registry state is the truth, the check mark follows it.
            let result = if shell_menu_is_installed() {
                shell_menu_uninstall().map(|_| ())
            } else {
                shell_menu_install()
            };
            if let Err(message) = result {
                message_box(app.hwnd, &message, "VeloEdit", MB_OK | MB_ICONERROR);
            }
            refresh_menu_checks(app);
        }
        CMD_CHANGE_RELOAD => {
            // Roadmap item 34: re-open the file from disk. Unsaved
            // edits go through the ordinary dirty-document prompt;
            // cancelling it keeps the bar (and the edits). A reload
            // re-runs the whole per-document setup, exactly like a
            // fresh open of the path.
            if let Some(path) = app.editor.path().map(|p| p.to_path_buf()) {
                if prompt_save_if_dirty(app) {
                    open_document(app, &path);
                }
            }
        }
        CMD_CHANGE_IGNORE => {
            // Roadmap item 34: dismiss the notice for this on-disk
            // state; a further change (a different stamp) shows the
            // bar again.
            app.change_ignored_for = app.change_shown_for.take();
            set_change_bar_visible(app, false);
        }
        CMD_EXIT => {
            // SAFETY: posting our own close message.
            unsafe { PostMessageW(app.hwnd, WM_CLOSE, 0, 0) };
        }
        CMD_UNDO => {
            if app.editor.undo() {
                after_edit(app);
            }
        }
        CMD_REDO => {
            if app.editor.redo() {
                after_edit(app);
            }
        }
        CMD_CUT => {
            if let Some(text) = copy_text(&mut app.editor) {
                clipboard_set_text(app.hwnd, &text);
                if app.editor.delete_selection().unwrap_or(false) {
                    after_edit(app);
                }
            }
        }
        CMD_COPY => {
            if let Some(text) = copy_text(&mut app.editor) {
                clipboard_set_text(app.hwnd, &text);
            }
        }
        CMD_PASTE => {
            if let Some(text) = clipboard_get_text(app.hwnd) {
                match app.editor.mode() {
                    EditorMode::Text => {
                        if app.editor.insert_text(&text).is_ok() {
                            after_edit(app);
                        }
                    }
                    EditorMode::Hex => {
                        if let Some(bytes) = parse_hex(&text) {
                            if app.editor.insert_bytes(&bytes).is_ok() {
                                after_edit(app);
                            }
                        }
                    }
                }
            }
        }
        CMD_FIND => {
            if app.find_visible {
                // SAFETY: our child window.
                unsafe { SetFocus(app.find_edit) };
            } else {
                show_find_bar(app, false);
            }
            find_from_bar(app, 0);
        }
        CMD_REPLACE => {
            show_find_bar(app, true);
            find_from_bar(app, 0);
        }
        CMD_FIND_NEXT => find_from_bar(app, 1),
        CMD_FIND_PREV => find_from_bar(app, -1),
        CMD_DO_REPLACE => do_replace(app, false),
        CMD_DO_REPLACE_ALL => do_replace(app, true),
        CMD_FIND_CLOSE => hide_find_bar(app),
        CMD_FIND_SCOPE => {
            // The "In selection" toggle: update the state, the
            // button's ✓ prefix (the draw pass adds the accent
            // state while on), and re-run the find so the count
            // immediately reflects the new scope (or the whole
            // document, when toggled off).
            app.find_in_selection = !app.find_in_selection;
            if !app.find_in_selection {
                app.find_scope_range = None;
                app.last_scope_range = None;
            }
            let label = if app.find_in_selection {
                "\u{2713} In selection"
            } else {
                "In selection"
            };
            let wide = wstr(label);
            // SAFETY: our find-bar button; the string outlives the call.
            unsafe {
                SetWindowTextW(app.find_scope_check, wide.as_ptr());
                InvalidateRect(app.find_scope_check, std::ptr::null(), 1);
                SetFocus(app.find_edit);
            }
            find_from_bar(app, 0);
        }
        CMD_SELECT_ALL => {
            app.editor.select_all();
            after_caret_move(app);
        }
        CMD_SORT_LINES | CMD_DEDUP_LINES => {
            // Line operations on the selection (roadmap item 23):
            // the editor ops expand the range to whole lines and are
            // one undo entry each; a refusal (a range past the size
            // limit) goes through the standard error box.
            let Some((start, end)) = app.editor.selection_range() else {
                return;
            };
            let result = if id == CMD_SORT_LINES {
                app.editor.sort_lines_in_range(start, end).map(|_| ())
            } else {
                app.editor.dedup_lines_in_range(start, end).map(|_| ())
            };
            match result {
                Ok(()) => after_edit(app),
                Err(err) => {
                    message_box(
                        app.hwnd,
                        &format!("{err}"),
                        "VeloEdit",
                        MB_OK | MB_ICONERROR,
                    );
                }
            }
        }
        CMD_MODE_TEXT => set_mode(app, EditorMode::Text),
        CMD_MODE_HEX => set_mode(app, EditorMode::Hex),
        CMD_MODE_TOGGLE => {
            let next = match app.editor.mode() {
                EditorMode::Text => EditorMode::Hex,
                EditorMode::Hex => EditorMode::Text,
            };
            set_mode(app, next);
        }
        CMD_FONT_BIGGER | CMD_FONT_SMALLER => {
            let delta: i16 = if id == CMD_FONT_BIGGER { 2 } else { -2 };
            let size = (app.settings.font_size as i16 + delta).clamp(8, 48) as u16;
            if size != app.settings.font_size {
                app.settings.font_size = size;
                drop_format(app);
                ensure_format(app);
                save_settings(app);
                refresh_status(app);
                invalidate(app);
            }
        }
        CMD_VIEW_RULER => {
            // Cycle Off → 80 → 120 → Off (roadmap item 21), persisted
            // like every other view choice.
            app.settings.ruler_column = match app.settings.ruler_column {
                0 => 80,
                80 => 120,
                _ => 0,
            };
            save_settings(app);
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_INSPECTOR => {
            // Data inspector panel (roadmap item 15): docked right;
            // the editor area shrinks by the panel's width in
            // compute_layout, and the hex fit width follows.
            app.settings.inspector_open = !app.settings.inspector_open;
            save_settings(app);
            refresh_menu_checks(app);
            sync_hex_layout(app);
            layout_children(app);
            invalidate(app);
        }
        CMD_REL_OFFSETS => {
            // Relative offsets (roadmap item 16): switching on
            // starts counting from the caret.
            app.rel_offsets = !app.rel_offsets;
            if app.rel_offsets {
                app.rel_base = app.editor.caret();
            }
            refresh_menu_checks(app);
            refresh_status(app);
            invalidate(app);
        }
        CMD_SET_OFFSET_BASE => {
            app.rel_base = app.editor.caret();
            if !app.rel_offsets {
                app.rel_offsets = true;
                refresh_menu_checks(app);
            }
            refresh_status(app);
            invalidate(app);
        }
        _ if (CMD_HEXCOLS_BASE..CMD_HEXCOLS_BASE + HEX_COLS_MENU.len()).contains(&id) => {
            // Hex Columns (roadmap item 13): 0 is fit-to-window.
            app.settings.hex_cols = HEX_COLS_MENU[id - CMD_HEXCOLS_BASE];
            save_settings(app);
            sync_hex_layout(app);
            refresh_menu_checks(app);
            invalidate(app);
        }
        _ if (CMD_HEXGROUP_BASE..CMD_HEXGROUP_BASE + 4).contains(&id) => {
            // Hex Grouping (roadmap item 14).
            app.settings.hex_group = [1, 2, 4, 8][id - CMD_HEXGROUP_BASE];
            save_settings(app);
            sync_hex_layout(app);
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_HEX_ENDIAN => {
            app.settings.hex_endian = !app.settings.hex_endian;
            save_settings(app);
            sync_hex_layout(app);
            refresh_menu_checks(app);
            invalidate(app);
        }
        _ if (CMD_SAVE_EOL_BASE..CMD_SAVE_EOL_BASE + 3).contains(&id) => {
            // Save Options ▸ Line Endings (roadmap item 31): the
            // status bar announces the armed conversion at once.
            app.settings.save_eol = match id - CMD_SAVE_EOL_BASE {
                1 => crate::settings::SAVE_EOL_LF.to_string(),
                2 => crate::settings::SAVE_EOL_CRLF.to_string(),
                _ => crate::settings::SAVE_EOL_KEEP.to_string(),
            };
            save_settings(app);
            refresh_menu_checks(app);
            layout_children(app);
            refresh_status(app);
        }
        _ if (CMD_SAVE_BOM_BASE..CMD_SAVE_BOM_BASE + 3).contains(&id) => {
            // Save Options ▸ Byte-Order Mark (roadmap item 32).
            app.settings.save_bom = match id - CMD_SAVE_BOM_BASE {
                1 => crate::settings::SAVE_BOM_ADD.to_string(),
                2 => crate::settings::SAVE_BOM_REMOVE.to_string(),
                _ => crate::settings::SAVE_BOM_KEEP.to_string(),
            };
            save_settings(app);
            refresh_menu_checks(app);
            layout_children(app);
            refresh_status(app);
        }
        CMD_GUIDE => show_help(app),
        CMD_ABOUT => {
            message_box(
                app.hwnd,
                concat!(
                    "VeloEdit ",
                    env!("CARGO_PKG_VERSION"),
                    "\n\n",
                    "Portable binary and large-file editor.\n",
                    "A memory-mapped piece table opens huge files instantly,\n",
                    "and saving is atomic: a crash cannot leave a half-written file.\n\n",
                    "Free software (MIT). Works completely offline."
                ),
                "About VeloEdit",
                MB_OK,
            );
        }
        _ if (CMD_THEME_BASE..CMD_THEME_BASE + crate::theme::names().len()).contains(&id) => {
            // A click commits: any menu-preview backup is discarded
            // (WM_EXITMENULOOP then has nothing to restore).
            app.menu_preview = None;
            let name = crate::theme::names()[id - CMD_THEME_BASE];
            app.settings.theme = name.to_string();
            apply_theme(app);
            refresh_menu_checks(app);
        }
        _ if id == CMD_ACCENT_BASE => {
            app.menu_preview = None;
            app.settings.accent_override = None;
            apply_theme(app);
            refresh_menu_checks(app);
        }
        _ if (CMD_ACCENT_BASE + 1..CMD_ACCENT_BASE + 1 + ACCENT_PRESETS.len()).contains(&id) => {
            app.menu_preview = None;
            let (_, hex) = ACCENT_PRESETS[id - CMD_ACCENT_BASE - 1];
            app.settings.accent_override = Some(hex.to_string());
            apply_theme(app);
            refresh_menu_checks(app);
        }
        _ if id == CMD_ACCENT_CUSTOM => {
            if let Some(color) = choose_accent_color(app, app.theme.accent) {
                app.menu_preview = None;
                app.settings.accent_override = Some(color.to_hex());
                apply_theme(app);
                refresh_menu_checks(app);
            } else {
                // Dialog cancelled: if a preview was still in flight
                // (the menu loop may not have sent WM_EXITMENULOOP
                // yet), put the committed look back.
                restore_menu_preview(app);
            }
        }
        _ if (CMD_RECENT_BASE..CMD_RECENT_BASE + 8).contains(&id) => {
            if let Some(path) = app.settings.recent.get(id - CMD_RECENT_BASE).cloned() {
                if prompt_save_if_dirty(app) {
                    open_document(app, Path::new(&path));
                }
            }
        }
        _ => {}
    }
}

/// Handle WM_KEYDOWN for the main window. Returns true when handled.
/// Virtual-key codes are the stable Win32 numbers, written literally.
fn on_keydown(app: &mut App, wparam: WPARAM) -> bool {
    let vk = wparam as u32;
    // SAFETY: plain key-state reads (VK_CONTROL = 0x11, VK_SHIFT = 0x10).
    let ctrl = unsafe { GetKeyState(0x11) } as u16 & 0x8000 != 0;
    let shift = unsafe { GetKeyState(0x10) } as u16 & 0x8000 != 0;
    let page = page_rows(app);
    match vk {
        0x25 => app.editor.move_left(shift),  // VK_LEFT
        0x27 => app.editor.move_right(shift), // VK_RIGHT
        0x26 => app.editor.move_up(shift),    // VK_UP
        0x28 => app.editor.move_down(shift),  // VK_DOWN
        0x24 => {
            // VK_HOME (Ctrl: document start)
            if ctrl {
                app.editor.move_doc_start(shift)
            } else {
                app.editor.move_home(shift)
            }
        }
        0x23 => {
            // VK_END (Ctrl: document end)
            if ctrl {
                app.editor.move_doc_end(shift)
            } else {
                app.editor.move_end(shift)
            }
        }
        0x21 => app.editor.move_page_up(page, shift), // VK_PRIOR
        0x22 => app.editor.move_page_down(page, shift), // VK_NEXT
        0x08 => {
            // VK_BACK
            if app.editor.backspace().unwrap_or(false) {
                after_edit(app);
            }
            return true;
        }
        0x2E => {
            // VK_DELETE
            if app.editor.delete_forward().unwrap_or(false) {
                after_edit(app);
            }
            return true;
        }
        0x70 => {
            // VK_F1
            show_help(app);
            return true;
        }
        0x71 => {
            // VK_F2: Ctrl toggles a bookmark at the caret; Shift
            // jumps to the previous bookmark, plain F2 to the next
            // (roadmap item 20).
            if ctrl {
                toggle_bookmark(app);
            } else {
                jump_bookmark(app, !shift);
            }
            return true;
        }
        0x72 => {
            // VK_F3 (Shift+F3: previous match)
            on_command(app, if shift { CMD_FIND_PREV } else { CMD_FIND_NEXT });
            return true;
        }
        0x75 => {
            // VK_F6
            on_command(app, CMD_MODE_TOGGLE);
            return true;
        }
        0x1B => {
            // VK_ESCAPE
            if app.find_visible {
                hide_find_bar(app);
            } else {
                let caret = app.editor.caret();
                app.editor.move_caret_to(caret, false);
                invalidate(app);
            }
            return true;
        }
        _ if ctrl => {
            match vk {
                0x41 => app.editor.select_all(),    // Ctrl+A
                0x43 => on_command(app, CMD_COPY),  // Ctrl+C
                0x58 => on_command(app, CMD_CUT),   // Ctrl+X
                0x56 => on_command(app, CMD_PASTE), // Ctrl+V
                0x5A => on_command(app, CMD_UNDO),  // Ctrl+Z
                0x59 => on_command(app, CMD_REDO),  // Ctrl+Y
                0x4F => on_command(app, CMD_OPEN),  // Ctrl+O
                0x53 => {
                    // Ctrl+S (Ctrl+Shift+S: Save As)
                    on_command(app, if shift { CMD_SAVE_AS } else { CMD_SAVE });
                }
                0x46 => on_command(app, CMD_FIND),        // Ctrl+F
                0x48 => on_command(app, CMD_REPLACE),     // Ctrl+H
                0xBB => on_command(app, CMD_FONT_BIGGER), // Ctrl+'+' (VK_OEM_PLUS)
                0xBD => on_command(app, CMD_FONT_SMALLER), // Ctrl+'-' (VK_OEM_MINUS)
                _ => return false,
            }
            // Navigation arms above fall through to the caret update;
            // command arms have already refreshed what they touch.
            if matches!(
                vk,
                0x25 | 0x27 | 0x26 | 0x28 | 0x24 | 0x23 | 0x21 | 0x22 | 0x41
            ) {
                after_caret_move(app);
            }
            return true;
        }
        _ => return false,
    }
    after_caret_move(app);
    true
}

fn insert_str(app: &mut App, text: &str) {
    if app.editor.insert_text(text).is_ok() {
        after_edit(app);
    }
}

fn insert_char(app: &mut App, c: char) {
    let mut buf = [0u8; 4];
    let text = c.encode_utf8(&mut buf).to_owned();
    insert_str(app, &text);
}

/// Handle WM_CHAR for the main window. Returns true when handled.
fn on_char(app: &mut App, wparam: WPARAM) -> bool {
    let ch = wparam as u32;
    match app.editor.mode() {
        EditorMode::Hex => {
            if let Some(c) = char::from_u32(ch) {
                if let Some(digit) = c.to_digit(16) {
                    match app.hex_nibble {
                        None => {
                            app.hex_nibble = Some(digit as u8);
                        }
                        Some(hi) => {
                            let byte = (hi << 4) | digit as u8;
                            app.hex_nibble = None;
                            if app.editor.overwrite_byte(byte).is_ok() {
                                after_edit(app);
                            }
                        }
                    }
                    return true;
                }
            }
            false
        }
        EditorMode::Text => {
            // Assemble UTF-16 surrogate pairs (characters outside the
            // Basic Multilingual Plane arrive as two WM_CHARs).
            if (0xD800..=0xDBFF).contains(&ch) {
                app.pending_high_surrogate = Some(ch as u16);
                return true;
            }
            if (0xDC00..=0xDFFF).contains(&ch) {
                if let Some(hi) = app.pending_high_surrogate.take() {
                    let code = 0x1_0000 + (((hi as u32) - 0xD800) << 10) + (ch - 0xDC00);
                    if let Some(c) = char::from_u32(code) {
                        insert_char(app, c);
                    }
                }
                return true;
            }
            app.pending_high_surrogate = None;
            match ch {
                0x0D => {
                    // Enter
                    if app.editor.insert_newline().is_ok() {
                        after_edit(app);
                    }
                    true
                }
                0x09 => {
                    insert_str(app, "\t");
                    true
                }
                // Backspace is handled in WM_KEYDOWN; Escape in both;
                // other control codes (Ctrl+letter echoes) are ignored.
                0x00..=0x1F => true,
                _ => {
                    if let Some(c) = char::from_u32(ch) {
                        insert_char(app, c);
                    }
                    true
                }
            }
        }
    }
}

fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam & 0xFFFF) as i16 as i32,
        ((lparam >> 16) & 0xFFFF) as i16 as i32,
    )
}

fn on_mouse_down(app: &mut App, lparam: LPARAM, double: bool) {
    // SAFETY: clicking the editor surface focuses the main window.
    unsafe { SetFocus(app.hwnd) };
    let (x, y) = mouse_xy(lparam);
    let offset = pos_to_offset(app, x, y);
    if double && app.editor.mode() == EditorMode::Text {
        app.editor.move_caret_to(offset, false);
        select_word_at_caret(app);
    } else {
        // SAFETY: plain key-state read (VK_SHIFT = 0x10).
        let shift = unsafe { GetKeyState(0x10) } as u16 & 0x8000 != 0;
        app.editor.move_caret_to(offset, shift);
        app.dragging = true;
        // SAFETY: capture the mouse for the drag; released on button up.
        unsafe { SetCapture(app.hwnd) };
    }
    app.caret_on = true;
    refresh_status(app);
    invalidate(app);
}

/// What a press on the custom scrollbar means: a thumb grab (with the
/// pointer's DIP offset from the thumb's top) or a page step toward
/// the click.
enum ScrollHit {
    Thumb(f32),
    Page(i64),
}

/// Hit-test the custom scrollbar for a point in physical pixels.
fn scrollbar_hit(app: &mut App, x_px: i32, y_px: i32) -> Option<ScrollHit> {
    let layout = compute_layout(app);
    let x = app.dip(x_px);
    let y = app.dip(y_px);
    if x < layout.editor_right - SCROLLBAR_W
        || x >= layout.editor_right
        || y < layout.editor_top
        || y >= layout.editor_bottom
    {
        return None;
    }
    let (total, first, visible) = scroll_metrics(app);
    let track_h = layout.editor_bottom - layout.editor_top;
    let (thumb_y, thumb_h) = scrollbar_geometry(track_h, total, visible, first)?;
    let rel = y - layout.editor_top;
    // A page step is expressed in the editor's scroll unit (text
    // lines / hex rows) even while the geometry above runs in bytes
    // (partial index): in line units `visible` already is a page of
    // lines, so only text mode needs the translation.
    let page_step = match app.editor.mode() {
        EditorMode::Text => page_rows(app) as i64,
        EditorMode::Hex => visible as i64,
    };
    if rel < thumb_y {
        Some(ScrollHit::Page(-page_step))
    } else if rel > thumb_y + thumb_h {
        Some(ScrollHit::Page(page_step))
    } else {
        Some(ScrollHit::Thumb(rel - thumb_y))
    }
}

/// WM_LBUTTONDOWN on the scrollbar: grab the thumb or page toward the
/// click. Returns true when the press was the scrollbar's.
fn on_scrollbar_down(app: &mut App, lparam: LPARAM) -> bool {
    let (x, y) = mouse_xy(lparam);
    match scrollbar_hit(app, x, y) {
        None => false,
        Some(ScrollHit::Page(delta)) => {
            scroll_by(app, delta);
            true
        }
        Some(ScrollHit::Thumb(grab)) => {
            app.scroll_drag = Some(grab);
            // SAFETY: clicking the scrollbar focuses the main window
            // and captures the mouse for the drag (released on
            // button up).
            unsafe { SetFocus(app.hwnd) };
            // SAFETY: as above.
            unsafe { SetCapture(app.hwnd) };
            true
        }
    }
}

/// WM_MOUSEMOVE during a scrollbar thumb drag: convert the pointer
/// back into a first visible row through the inverse geometry helper
/// and the same editor scroll setters the wheel uses.
fn on_scrollbar_move(app: &mut App, lparam: LPARAM) -> bool {
    let Some(grab) = app.scroll_drag else {
        return false;
    };
    let (_, y_px) = mouse_xy(lparam);
    let layout = compute_layout(app);
    let (total, _, visible) = scroll_metrics(app);
    let track_h = layout.editor_bottom - layout.editor_top;
    let thumb_y = app.dip(y_px) - layout.editor_top - grab;
    let first = scrollbar_first_row(track_h, total, visible, thumb_y);
    match app.editor.mode() {
        EditorMode::Text => {
            if app.editor.index_is_partial() {
                // Byte units while the staged index builds: the
                // drag position is a byte offset; land on the line
                // containing it (exact, from the checkpoints plus
                // a forward count). `set_first_line` still clamps
                // to the indexed prefix, so a drag past the
                // frontier rests at the frontier until the build
                // sweeps past — the thumb keeps showing the true
                // file proportion throughout.
                let line = app.editor.line_col_of(first).0;
                app.editor.set_first_line(line);
            } else {
                app.editor.set_first_line(first);
            }
        }
        EditorMode::Hex => app.editor.set_first_row(first),
    }
    invalidate(app);
    true
}

// ---------------------------------------------------------------------------
// The window procedure and the entry point
// ---------------------------------------------------------------------------

const MAIN_CLASS: &str = "VeloEdit";

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // WM_CREATE arrives before GWLP_USERDATA is set; WM_NCDESTROY after
    // it is cleared. Both are handled without touching the App.
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the Box<App> from run_gui.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let app_ptr = create.lpCreateParams as *mut App;
        // SAFETY: storing the pointer for the window's lifetime.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize) };
        // SAFETY: the box is alive and exclusively ours on this thread.
        let app = unsafe { &mut *app_ptr };
        app.hwnd = hwnd;
        // SAFETY: our window handle is valid during WM_CREATE.
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        app.dpi = if dpi == 0 { 96 } else { dpi };
        create_children(app);
        build_menus(app);
        // The persisted hex layout (roadmap items 13/14) applies
        // from the first paint.
        sync_hex_layout(app);
        // SAFETY: accept file drops on our own window.
        unsafe { DragAcceptFiles(hwnd, 1) };
        // SAFETY: timers on our window; a null TIMERPROC means WM_TIMER.
        let blink = unsafe { GetCaretBlinkTime() };
        unsafe {
            SetTimer(
                hwnd,
                TIMER_BLINK,
                if blink == 0 { 530 } else { blink },
                None,
            );
            SetTimer(hwnd, TIMER_TICK, 200, None);
        }
        layout_children(app);
        refresh_title(app);
        refresh_status(app);
        apply_titlebar_theme(app);
        return 0;
    }
    if msg == WM_NCDESTROY {
        // SAFETY: free the App box exactly once, here, at the very end
        // of the window's life.
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
        if !ptr.is_null() {
            // SAFETY: as above.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
        }
        // SAFETY: default handling for the message itself.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: messages arriving before WM_CREATE stored the App (or
    // after it was freed) get default handling; every handled message
    // below runs while the App is alive, serially, on the UI thread.
    let app_ptr = unsafe { app_ptr(hwnd) };
    if app_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: as above — the Box<App> outlives the window and message
    // handling never re-enters it concurrently.
    let app = unsafe { &mut *app_ptr };

    match msg {
        WM_PAINT => {
            // SAFETY: PAINTSTRUCT is a plain C struct; BeginPaint /
            // EndPaint bracket the one render pass.
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            unsafe { BeginPaint(hwnd, &mut ps) };
            render(app);
            // SAFETY: pairs with BeginPaint above.
            unsafe { EndPaint(hwnd, &ps) };
            0
        }
        WM_ERASEBKGND => 1, // Direct2D covers the whole client area
        WM_SIZE => {
            if !app.target.is_null() {
                let size = D2dSizeU {
                    width: ((lparam & 0xFFFF) as u32).max(1),
                    height: (((lparam >> 16) & 0xFFFF) as u32).max(1),
                };
                // SAFETY: slot 58 is ID2D1HwndRenderTarget::Resize per
                // d2d1.h; on failure the target is dropped and the next
                // paint recreates it.
                let resize: unsafe extern "system" fn(*mut c_void, *const D2dSizeU) -> i32 =
                    unsafe { vfn(app.target, SLOT_RT_RESIZE) };
                // SAFETY: as above.
                if unsafe { resize(app.target, &size) } < 0 {
                    release_target(app);
                }
            }
            // Fit-to-window hex columns follow the new size
            // (roadmap item 13).
            sync_hex_layout(app);
            layout_children(app);
            0
        }
        WM_GETMINMAXINFO => {
            // SAFETY: lparam is a writable MINMAXINFO.
            let mmi = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            mmi.ptMinTrackSize = POINT {
                x: app.px(680.0),
                y: app.px(420.0),
            };
            0
        }
        WM_MOUSEWHEEL => {
            on_wheel(app, wparam);
            0
        }
        WM_LBUTTONDOWN => {
            if !on_scrollbar_down(app, lparam) {
                on_mouse_down(app, lparam, false);
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            if !on_scrollbar_down(app, lparam) {
                on_mouse_down(app, lparam, true);
            }
            0
        }
        WM_MOUSEMOVE => {
            if !on_scrollbar_move(app, lparam) && app.dragging {
                let (x, y) = mouse_xy(lparam);
                let offset = pos_to_offset(app, x, y);
                app.editor.move_caret_to(offset, true);
                refresh_status(app);
                invalidate(app);
            }
            0
        }
        WM_LBUTTONUP => {
            if app.scroll_drag.is_some() {
                app.scroll_drag = None;
                // SAFETY: pairs with the SetCapture at thumb grab.
                unsafe { ReleaseCapture() };
            } else if app.dragging {
                app.dragging = false;
                // SAFETY: pairs with the SetCapture at mouse-down.
                unsafe { ReleaseCapture() };
            }
            0
        }
        WM_KEYDOWN => {
            if on_keydown(app, wparam) {
                0
            } else {
                // SAFETY: unhandled keys get default processing.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_CHAR => {
            if on_char(app, wparam) {
                0
            } else {
                // SAFETY: as above.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_COMMAND => {
            on_command(app, wparam & 0xFFFF);
            0
        }
        WM_INITMENUPOPUP => {
            if (wparam as HMENU) == app.recent_menu {
                rebuild_recent_menu(app);
            }
            refresh_menu_checks(app);
            // SAFETY: enable/disable items on our own menu bar.
            let bar = unsafe { GetMenu(hwnd) };
            if !bar.is_null() {
                let can_edit = !app.editor.read_only();
                let has_selection = app.editor.selection_range().is_some();
                // SAFETY: as above; GetSubMenu positions are File=0,
                // Edit=1 as built in build_menus.
                unsafe {
                    let file_menu = GetSubMenu(bar, 0);
                    let edit_menu = GetSubMenu(bar, 1);
                    let set = |menu: HMENU, id: usize, enabled: bool| {
                        EnableMenuItem(
                            menu,
                            id as u32,
                            MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
                        );
                    };
                    set(file_menu, CMD_SAVE, can_edit);
                    set(file_menu, CMD_SAVE_SELECTION, has_selection);
                    set(file_menu, CMD_SPLIT_FILE, app.editor.path().is_some());
                    // File Hashes (roadmap item 37) hashes the file
                    // on disk, so a document with no file has
                    // nothing to hash.
                    set(file_menu, CMD_FILE_HASHES, app.editor.path().is_some());
                    set(edit_menu, CMD_UNDO, app.editor.can_undo());
                    set(edit_menu, CMD_SORT_LINES, can_edit && has_selection);
                    set(edit_menu, CMD_DEDUP_LINES, can_edit && has_selection);
                    set(edit_menu, CMD_REDO, app.editor.can_redo());
                    set(edit_menu, CMD_CUT, can_edit && has_selection);
                    set(edit_menu, CMD_COPY, has_selection);
                    set(
                        edit_menu,
                        CMD_PASTE,
                        can_edit && IsClipboardFormatAvailable(CF_UNICODETEXT as u32) != 0,
                    );
                }
            }
            0
        }
        WM_MENUSELECT => {
            on_menu_select(app, wparam, lparam);
            0
        }
        WM_EXITMENULOOP => {
            // The menu session is over. If a theme/accent preview
            // was never committed by a click (Esc, a click
            // elsewhere), restore the committed look.
            restore_menu_preview(app);
            0
        }
        WM_CANCELMODE => {
            // Same dismissal path as WM_EXITMENULOOP for the
            // preview; default handling performs the actual cancel
            // of the internal mode (menu, capture, scrolling).
            restore_menu_preview(app);
            // SAFETY: default handling for the message itself.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_TIMER => {
            match wparam {
                TIMER_BLINK => {
                    if app.has_focus {
                        app.caret_on = !app.caret_on;
                        invalidate(app);
                    }
                }
                TIMER_TICK => {
                    // Pick up the staged line index's latest
                    // publication: each snapshot extends the editor's
                    // indexed prefix, and the last one completes it.
                    // Publications from a superseded spawn (an older
                    // epoch) are dropped. When the document has been
                    // edited since the build's baseline, the build is
                    // discarded outright — the safe rule for an edit
                    // landing mid-build; the editor keeps answering
                    // exactly from its stale index's valid prefix,
                    // and the next baseline (open / save) restarts
                    // the build.
                    let publication = match app.index_slot.try_lock() {
                        Ok(mut guard) => guard.take(),
                        Err(_) => None,
                    };
                    if let Some((epoch, progress)) = publication {
                        if epoch == app.index_epoch {
                            if app.doc_generation == app.open_generation {
                                let complete = progress.snapshot.is_complete();
                                let total = progress.snapshot.total_length();
                                app.editor.attach_line_index(progress.snapshot);
                                app.indexing = if complete {
                                    None
                                } else {
                                    Some((progress.indexed_bytes, total))
                                };
                                refresh_status(app);
                                invalidate(app);
                            } else if app.indexing.is_some() {
                                app.indexing = None;
                                refresh_status(app);
                            }
                        }
                    }
                    if app.pending_goto.is_some() && app.editor.has_complete_index() {
                        let line = app.pending_goto.take().expect("checked above");
                        app.editor.goto_line(line);
                        after_caret_move(app);
                    }
                    // Pick up the original file's fingerprint for
                    // the delta recovery (roadmap item 7).
                    if app.doc_fingerprint.is_none() {
                        let ready = match app.fingerprint_slot.try_lock() {
                            Ok(mut guard) => guard.take(),
                            Err(_) => None,
                        };
                        if let Some((epoch, fingerprint)) = ready {
                            if epoch == app.doc_epoch {
                                app.doc_fingerprint = Some(fingerprint);
                            }
                        }
                    }
                    // Pick up background find-all progress (roadmap
                    // items 10 + 24): mirror the worker's matches,
                    // select the first one at or after the caret as
                    // soon as any are known (the synchronous find
                    // selects at commit; here the selection lands
                    // with the first results), and keep the count
                    // label honest.
                    if app.bg_active {
                        let snapshot = match app.find_slot.try_lock() {
                            Ok(guard) => guard.as_ref().and_then(|progress| {
                                if progress.find_gen == app.find_gen
                                    && progress.doc_gen == app.doc_generation
                                {
                                    Some((progress.matches.clone(), progress.done))
                                } else {
                                    None
                                }
                            }),
                            Err(_) => None,
                        };
                        if let Some((matches, done)) = snapshot {
                            let changed =
                                matches.len() != app.bg_matches.len() || (done && !app.bg_done);
                            if changed {
                                app.bg_matches = matches;
                                app.bg_done = done;
                                if app.bg_current.is_none() && !app.bg_matches.is_empty() {
                                    let caret = app.editor.caret();
                                    let idx = app
                                        .bg_matches
                                        .iter()
                                        .position(|&m| m >= caret)
                                        .unwrap_or(0);
                                    select_bg_match(app, idx);
                                    let page = page_rows(app);
                                    app.editor.ensure_caret_visible(page);
                                    refresh_status(app);
                                }
                                if app.find_visible {
                                    refresh_bg_find_label(app);
                                }
                                invalidate(app);
                            }
                        }
                    }
                    // Recovery: one second after the last edit,
                    // write the record. A delta write that must wait
                    // for the fingerprint keeps the debounce open
                    // and retries on a later tick.
                    if let Some(when) = app.last_edit {
                        if when.elapsed() >= std::time::Duration::from_secs(1)
                            && (!app.editor.is_dirty() || write_recovery(app))
                        {
                            app.last_edit = None;
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_ACTIVATE => {
            // Roadmap item 34: coming back to the window is the
            // moment to notice the file changed under us. WA_INACTIVE
            // is 0; plain activation (1) and click-activation (2)
            // both run the (cheap, deduplicated) stamp check.
            if (wparam & 0xFFFF) != 0 {
                check_external_change(app);
            }
            0
        }
        WM_SETFOCUS => {
            app.has_focus = true;
            app.caret_on = true;
            invalidate(app);
            0
        }
        WM_KILLFOCUS => {
            app.has_focus = false;
            invalidate(app);
            0
        }
        WM_DPICHANGED => {
            app.dpi = (wparam & 0xFFFF) as u32;
            if app.dpi == 0 {
                app.dpi = 96;
            }
            if !app.target.is_null() {
                // SAFETY: slot 51 is ID2D1RenderTarget::SetDpi per
                // d2d1.h; the following SetWindowPos triggers WM_SIZE,
                // which resizes the target.
                let set_dpi: unsafe extern "system" fn(*mut c_void, f32, f32) =
                    unsafe { vfn(app.target, SLOT_RT_SET_DPI) };
                // SAFETY: as above.
                unsafe { set_dpi(app.target, app.dpi as f32, app.dpi as f32) };
            }
            // SAFETY: lparam is the suggested RECT for the new DPI.
            let rect = unsafe { *(lparam as *const RECT) };
            // SAFETY: move/resize our window to the suggested rect.
            unsafe {
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            layout_children(app);
            invalidate(app);
            0
        }
        WM_DROPFILES => {
            // SAFETY: wparam is the drop handle; it is finished here.
            let hdrop = wparam as HDROP;
            let mut buf = vec![0u16; 1024];
            // SAFETY: as above; index 0 is the first dropped file.
            let n = unsafe { DragQueryFileW(hdrop, 0, buf.as_mut_ptr(), buf.len() as u32) };
            // SAFETY: as above.
            unsafe { DragFinish(hdrop) };
            if n > 0 {
                let path = string_from_wide(&buf);
                if prompt_save_if_dirty(app) {
                    open_document(app, Path::new(&path));
                }
            }
            0
        }
        WM_CTLCOLORSTATIC => {
            let hdc = wparam as HDC;
            // The find bar's statics are quiet text on the bar's own
            // background, not panel-coloured patches: they blend
            // with the row like the find edits do, and the match
            // count is muted. The status strip and the change bar's
            // label keep the panel treatment.
            let control = lparam as HWND;
            let is_find_count = control == app.find_count;
            let on_find_bar =
                is_find_count || control == app.find_label || control == app.replace_label;
            // SAFETY: colouring our status / find-bar labels from the
            // active theme; returns the background brush.
            unsafe {
                SetTextColor(
                    hdc,
                    colorref(if is_find_count {
                        app.theme.muted
                    } else {
                        app.theme.text
                    }),
                );
                SetBkColor(
                    hdc,
                    colorref(if on_find_bar {
                        app.theme.bg
                    } else {
                        app.theme.panel
                    }),
                );
                SetBkMode(hdc, OPAQUE as i32);
            }
            if on_find_bar {
                app.edit_brush as LRESULT
            } else {
                app.panel_brush as LRESULT
            }
        }
        WM_CTLCOLOREDIT => {
            let hdc = wparam as HDC;
            // SAFETY: colouring our find/replace edits; returns the
            // editor-background brush.
            unsafe {
                SetTextColor(hdc, colorref(app.theme.text));
                SetBkColor(hdc, colorref(app.theme.bg));
                SetBkMode(hdc, OPAQUE as i32);
            }
            app.edit_brush as LRESULT
        }
        WM_DRAWITEM => {
            // SAFETY: lparam is a DRAWITEMSTRUCT for one of our
            // owner-drawn buttons.
            let dis = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
            let id = dis.CtlID as usize;
            // ODS_SELECTED (0x1), ODS_DISABLED (0x4), ODS_FOCUS (0x10).
            let pressed = dis.itemState & 0x1 != 0;
            let disabled = dis.itemState & 0x4 != 0;
            let focused = dis.itemState & 0x10 != 0;
            // The Text / Hex pair is one segmented control (polish
            // pass): the segment for the current mode is filled
            // with the accent colour and labelled in white. Every
            // other "on" toggle — the find bar's "In selection" —
            // keeps the accent text and border treatment the mode
            // pair used before, as in the mockup.
            let seg_active = (id == CMD_MODE_TEXT && app.editor.mode() == EditorMode::Text)
                || (id == CMD_MODE_HEX && app.editor.mode() == EditorMode::Hex);
            let active = seg_active || (id == CMD_FIND_SCOPE && app.find_in_selection);
            let brush = if seg_active {
                app.accent_brush
            } else if pressed {
                app.edit_brush
            } else {
                app.panel_brush
            };
            let label_colour = if disabled {
                app.theme.muted
            } else if seg_active {
                Rgba::rgb(0xFF, 0xFF, 0xFF)
            } else if active {
                app.theme.accent
            } else {
                app.theme.text
            };
            let mut rect = dis.rcItem;
            // SAFETY: GDI drawing on the button's own DC.
            unsafe {
                FillRect(dis.hDC, &rect, brush);
                if id == CMD_MODE_TEXT || id == CMD_MODE_HEX {
                    // One shared frame for the pair: top and bottom
                    // edges on both segments, the outer side edge
                    // on each, and the single divider drawn as the
                    // Text segment's right edge (the Hex segment's
                    // left edge stays undrawn, so the divider is
                    // one line, not two).
                    let (left, right) = if id == CMD_MODE_TEXT {
                        (true, true)
                    } else {
                        (false, true)
                    };
                    draw_frame_edges(dis.hDC, &rect, app.line_brush, true, true, left, right);
                } else {
                    FrameRect(
                        dis.hDC,
                        &rect,
                        if active {
                            app.accent_brush
                        } else {
                            app.line_brush
                        },
                    );
                }
                if focused {
                    // Keyboard focus (polish pass, accessibility):
                    // an accent frame inset by 2 px inside the
                    // normal border, so tabbing through the chrome
                    // is visible — owner-drawn buttons otherwise
                    // show no focus at all.
                    let mut inner = rect;
                    inner.left += 2;
                    inner.top += 2;
                    inner.right -= 2;
                    inner.bottom -= 2;
                    FrameRect(dis.hDC, &inner, app.accent_brush);
                }
                SetBkMode(dis.hDC, TRANSPARENT as i32);
                SetTextColor(dis.hDC, colorref(label_colour));
                let text = window_text(dis.hwndItem);
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                if !wide.is_empty() {
                    let has_icon = matches!(
                        id,
                        CMD_OPEN | CMD_SAVE | CMD_UNDO | CMD_REDO | CMD_FIND | CMD_REPLACE
                    );
                    if has_icon {
                        // Toolbar polish: the glyph sits left of
                        // the label, the pair centred as a group.
                        // The label is measured with the DC's own
                        // font (the button's), so the centring is
                        // exact at any DPI.
                        let mut extent = SIZE { cx: 0, cy: 0 };
                        GetTextExtentPoint32W(
                            dis.hDC,
                            wide.as_ptr(),
                            wide.len() as i32,
                            &mut extent,
                        );
                        let icon_px = app.px(13.0);
                        let gap = app.px(4.0);
                        let group_w = icon_px + gap + extent.cx;
                        let icon_x = rect.left + (rect.right - rect.left - group_w) / 2;
                        let icon_y = rect.top + (rect.bottom - rect.top - icon_px) / 2;
                        draw_toolbar_icon(
                            dis.hDC,
                            id,
                            icon_x,
                            icon_y,
                            icon_px,
                            colorref(label_colour),
                        );
                        let mut text_rect = RECT {
                            left: icon_x + icon_px + gap,
                            top: rect.top,
                            right: rect.right,
                            bottom: rect.bottom,
                        };
                        // DT_LEFT (0x0) | DT_VCENTER (0x4) | DT_SINGLELINE (0x20)
                        DrawTextW(
                            dis.hDC,
                            wide.as_mut_ptr(),
                            wide.len() as i32,
                            &mut text_rect,
                            0x4 | 0x20,
                        );
                    } else {
                        // DT_CENTER (0x1) | DT_VCENTER (0x4) | DT_SINGLELINE (0x20)
                        DrawTextW(
                            dis.hDC,
                            wide.as_mut_ptr(),
                            wide.len() as i32,
                            &mut rect,
                            0x1 | 0x4 | 0x20,
                        );
                    }
                }
            }
            1
        }
        WM_SETCURSOR => {
            if (lparam & 0xFFFF) == 1 {
                // HTCLIENT: I-beam over the editor surface.
                // SAFETY: plain cursor position reads.
                let mut pt = POINT { x: 0, y: 0 };
                // SAFETY: as above.
                unsafe {
                    GetCursorPos(&mut pt);
                    ScreenToClient(hwnd, &mut pt);
                }
                let layout = compute_layout(app);
                let y = app.dip(pt.y);
                let x = app.dip(pt.x);
                if y >= layout.editor_top
                    && y < layout.editor_bottom
                    && x < layout.editor_right - SCROLLBAR_W
                {
                    // SAFETY: loading the shared I-beam cursor.
                    let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_IBEAM) };
                    // SAFETY: as above.
                    unsafe { SetCursor(cursor) };
                    return 1;
                }
            }
            // SAFETY: default handling elsewhere.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CLOSE => {
            if !prompt_save_if_dirty(app) {
                return 0;
            }
            // Persist settings (always, even on a clean close).
            // SAFETY: geometry reads on our window.
            if unsafe { IsZoomed(hwnd) } == 0 {
                let mut rc: RECT = unsafe { std::mem::zeroed() };
                // SAFETY: as above.
                if unsafe { GetWindowRect(hwnd, &mut rc) } != 0 {
                    app.settings.window = Some((
                        rc.left,
                        rc.top,
                        (rc.right - rc.left).max(1),
                        (rc.bottom - rc.top).max(1),
                    ));
                }
            }
            save_settings(app);
            delete_recovery_files();
            // SAFETY: our window; destruction is the confirmed outcome.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            release_target(app);
            drop_format(app);
            // SAFETY: the app-owned GDI brushes and COM factories are
            // released exactly once, here.
            unsafe {
                if !app.panel_brush.is_null() {
                    DeleteObject(app.panel_brush as HGDIOBJ);
                    app.panel_brush = std::ptr::null_mut();
                }
                if !app.edit_brush.is_null() {
                    DeleteObject(app.edit_brush as HGDIOBJ);
                    app.edit_brush = std::ptr::null_mut();
                }
                if !app.line_brush.is_null() {
                    DeleteObject(app.line_brush as HGDIOBJ);
                    app.line_brush = std::ptr::null_mut();
                }
                if !app.accent_brush.is_null() {
                    DeleteObject(app.accent_brush as HGDIOBJ);
                    app.accent_brush = std::ptr::null_mut();
                }
                com_release(app.dwrite);
                app.dwrite = std::ptr::null_mut();
                com_release(app.d2d);
                app.d2d = std::ptr::null_mut();
                KillTimer(hwnd, TIMER_BLINK);
                KillTimer(hwnd, TIMER_TICK);
            }
            // SAFETY: end the message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Entry point for the GUI (called from `crate::cli::run` on Windows
/// when a file — or no file — is given without a headless operation).
/// Returns the process exit code.
pub fn run_gui(options: &CliOptions) -> i32 {
    // SAFETY: Per-Monitor V2 DPI awareness must be set before any
    // window exists in the process (SPEC.md section 4.4); a failure
    // (already set) is harmless.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let settings = Settings::load_from(&crate::settings::settings_path());

    let (d2d, d2d_hr) = d2d_create_factory();
    let (dwrite, dwrite_hr) = dwrite_create_factory();
    if d2d.is_null() || dwrite.is_null() {
        let report = format!(
            "VeloEdit could not start Direct2D / DirectWrite, which the editor window needs.\n\nError codes: Direct2D 0x{:08X}, DirectWrite 0x{:08X}\nThe headless tools (VeloEdit --help) still work.",
            d2d_hr as u32, dwrite_hr as u32
        );
        eprintln!("{report}");
        message_box(
            std::ptr::null_mut(),
            &report,
            "VeloEdit",
            MB_OK | MB_ICONERROR,
        );
        // SAFETY: release whichever factory succeeded (null-checked).
        unsafe {
            com_release(d2d);
            com_release(dwrite);
        }
        return 1;
    }

    // The startup document: an explicit file wins; then a recovery
    // record (skipped in read-only viewing); otherwise a blank page.
    let mut index_source: Option<Arc<MmapFile>> = None;
    let mut startup_eol = "LF";
    // A delta recovery hands back the original file's fingerprint
    // alongside the editor; the tracking setup below adopts it.
    let mut recovered_fingerprint: Option<crate::recovery::FileFingerprint> = None;
    let mut editor = if let Some(file) = &options.file {
        let path = Path::new(file);
        match MmapFile::open(path) {
            Ok(mmap) => {
                let mmap = Arc::new(mmap);
                let sample_len = (mmap.len() as usize).min(64 * 1024);
                let detection = encoding::detect(&mmap.as_slice()[..sample_len]);
                startup_eol = detect_eol(&mmap.as_slice()[..sample_len]);
                let table = if mmap.is_empty() {
                    PieceTable::new(0)
                } else {
                    PieceTable::from_mapped(mmap.clone())
                };
                index_source = Some(mmap);
                Editor::from_table(
                    table,
                    detection,
                    Some(path.to_path_buf()),
                    options.read_only,
                )
            }
            Err(err) => {
                message_box(
                    std::ptr::null_mut(),
                    &format!("Could not open {}:\n{err}", path.display()),
                    "VeloEdit",
                    MB_OK | MB_ICONERROR,
                );
                Editor::new_empty()
            }
        }
    } else if !options.read_only {
        match recovery_startup_editor() {
            Some((recovered, fingerprint)) => {
                recovered_fingerprint = fingerprint;
                recovered
            }
            None => Editor::new_empty(),
        }
    } else {
        Editor::new_empty()
    };
    if options.mode == CliMode::Hex {
        editor.set_mode(EditorMode::Hex);
    }
    // Roadmap item 6: the owner's configured undo limits, applied
    // before any edit can be recorded.
    editor.set_undo_limits(
        settings.undo_limit,
        settings.undo_byte_budget_mb * 1024 * 1024,
    );
    // Staged open, as in open_document: only the head region is
    // indexed before the window exists; the background thread
    // spawned below completes the index in stages.
    editor.seed_line_index();
    if options.file.is_none() {
        // Recovery snapshot (or a blank page): sample what was loaded.
        let sample = editor.table().read(0, editor.total_len().min(64 * 1024));
        startup_eol = detect_eol(&sample);
    }

    let theme = Theme::resolve(&settings.theme, settings.accent_override.as_deref());
    // SAFETY: plain GDI factory calls; the app owns the brushes.
    let panel_brush = unsafe { CreateSolidBrush(colorref(theme.panel)) };
    let edit_brush = unsafe { CreateSolidBrush(colorref(theme.bg)) };
    let line_brush = unsafe { CreateSolidBrush(colorref(theme.line)) };
    let accent_brush = unsafe { CreateSolidBrush(colorref(theme.accent)) };
    let mut app = App {
        hwnd: std::ptr::null_mut(),
        editor,
        settings,
        theme,
        read_only_flag: options.read_only,
        d2d,
        dwrite,
        target: std::ptr::null_mut(),
        brushes: None,
        text_format: std::ptr::null_mut(),
        font_family: String::new(),
        dpi: 96,
        char_w: 8.0,
        line_h: 18.0,
        panel_brush,
        edit_brush,
        line_brush,
        accent_brush,
        toolbar: Vec::new(),
        find_controls: Vec::new(),
        find_edit: std::ptr::null_mut(),
        replace_edit: std::ptr::null_mut(),
        find_count: std::ptr::null_mut(),
        find_label: std::ptr::null_mut(),
        replace_label: std::ptr::null_mut(),
        find_scope_check: std::ptr::null_mut(),
        find_in_selection: false,
        find_scope_range: None,
        last_scope_range: None,
        last_find_kind: FindKind::Plain,
        doc_mmap: None,
        find_slot: Arc::new(Mutex::new(None)),
        find_gen: 0,
        bg_active: false,
        bg_matches: Vec::new(),
        bg_done: false,
        bg_current: None,
        bg_needle_len: 0,
        find_sync_once: false,
        bookmark_store: crate::bookmarks::BookmarkStore::load_from(
            &crate::bookmarks::bookmarks_path(),
        ),
        bookmarks: crate::bookmarks::BookmarkList::new(),
        doc_stamp: None,
        file_stamp: None,
        change_label: std::ptr::null_mut(),
        change_reload_btn: std::ptr::null_mut(),
        change_ignore_btn: std::ptr::null_mut(),
        change_visible: false,
        change_shown_for: None,
        change_ignored_for: None,
        fingerprint_slot: Arc::new(Mutex::new(None)),
        doc_epoch: 0,
        doc_fingerprint: None,
        status: std::ptr::null_mut(),
        status_right: std::ptr::null_mut(),
        eol: startup_eol,
        scroll_drag: None,
        help_hwnd: std::ptr::null_mut(),
        file_menu: std::ptr::null_mut(),
        view_menu: std::ptr::null_mut(),
        recent_menu: std::ptr::null_mut(),
        theme_menu: std::ptr::null_mut(),
        accent_menu: std::ptr::null_mut(),
        hex_cols_menu: std::ptr::null_mut(),
        hex_group_menu: std::ptr::null_mut(),
        save_options_menu: std::ptr::null_mut(),
        rel_offsets: false,
        rel_base: 0,
        find_visible: false,
        has_focus: false,
        caret_on: true,
        dragging: false,
        wheel_rem: 0,
        wheel_zoom_rem: 0,
        menu_preview: None,
        pending_high_surrogate: None,
        hex_nibble: None,
        index_slot: Arc::new(Mutex::new(None)),
        indexing: None,
        doc_generation: 0,
        open_generation: 0,
        last_edit: None,
        custom_colors: [0; 16],
        index_epoch: 0,
        last_find_text: String::new(),
        pending_goto: if options.file.is_some() {
            options.line
        } else {
            None
        },
    };
    // Per-document tracking for the startup document (bookmarks,
    // external-change baseline, recovery fingerprint): the same
    // setup open_document performs for every later open.
    app.doc_mmap = index_source.clone();
    begin_document_tracking(&mut app);
    if let Some(fingerprint) = recovered_fingerprint {
        app.doc_fingerprint = Some(fingerprint);
    }
    if let Some(mmap) = index_source {
        spawn_index_thread(&mut app, mmap);
    }

    // SAFETY: the instance handle identifies this process's module.
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if !register_classes(hinstance) {
        // SAFETY: no window exists; release what we own and give up.
        unsafe {
            com_release(dwrite);
            com_release(d2d);
        }
        return 1;
    }

    let (x, y, w, h) = match app.settings.window {
        Some((x, y, w, h))
            if (-10_000..=10_000).contains(&x)
                && (-10_000..=10_000).contains(&y)
                && (200..=20_000).contains(&w)
                && (150..=20_000).contains(&h) =>
        {
            (x, y, w, h)
        }
        _ => (CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT),
    };
    let class = wstr(MAIN_CLASS);
    let boxed = Box::into_raw(Box::new(app));
    // SAFETY: the class is registered; the box pointer travels in
    // lpCreateParams and is adopted by WM_CREATE (freed at WM_NCDESTROY).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            wstr("VeloEdit").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            boxed as *const c_void,
        )
    };
    if hwnd.is_null() {
        // SAFETY: the window never adopted the box; reclaim and release.
        unsafe {
            let app = Box::from_raw(boxed);
            com_release(app.dwrite);
            com_release(app.d2d);
            drop(app);
        }
        return 1;
    }
    // SAFETY: our fresh window.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
    }

    // The message loop.
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error.
        let result = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            return msg.wParam as i32;
        }
        // SAFETY: standard message pumping for our windows.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn register_classes(hinstance: HINSTANCE) -> bool {
    // CS_HREDRAW (0x2) | CS_VREDRAW (0x1) | CS_DBLCLKS (0x8).
    let style = 0x2 | 0x1 | 0x8;
    // SAFETY: loading the shared arrow cursor for both classes.
    let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };
    // The program icon: resource id 1, embedded by build.rs from
    // assets/veloedit.ico; the id is passed as a pointer (the
    // documented MAKEINTRESOURCEW form). Null if the resource were
    // somehow absent — the classes then get the default icon; never
    // fatal.
    // SAFETY: `hinstance` is our own module handle.
    let icon = unsafe { LoadIconW(hinstance, 1 as *const u16) };
    let main = wstr(MAIN_CLASS);
    let main_class = WNDCLASSW {
        style,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: main.as_ptr(),
    };
    // SAFETY: the class struct outlives the call; a 0 atom is failure
    // (or the class is already registered, which is also fine).
    if unsafe { RegisterClassW(&main_class) } == 0 {
        return false;
    }
    let help = wstr(HELP_CLASS);
    let help_class = WNDCLASSW {
        style: 0x2 | 0x1,
        lpfnWndProc: Some(help_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: help.as_ptr(),
    };
    // SAFETY: as above.
    if unsafe { RegisterClassW(&help_class) } == 0 {
        return false;
    }
    let split = wstr(SPLIT_CLASS);
    let split_class = WNDCLASSW {
        style: 0x2 | 0x1,
        lpfnWndProc: Some(split_dialog_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: split.as_ptr(),
    };
    // SAFETY: as above.
    if unsafe { RegisterClassW(&split_class) } == 0 {
        return false;
    }
    let hash = wstr(HASH_CLASS);
    let hash_class = WNDCLASSW {
        style: 0x2 | 0x1,
        lpfnWndProc: Some(hash_dialog_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: hash.as_ptr(),
    };
    // SAFETY: as above.
    (unsafe { RegisterClassW(&hash_class) }) != 0
}

/// The registry path of the Explorer context-menu item (roadmap
/// item 38), relative to HKEY_CURRENT_USER: per-user, so installing
/// it needs no administrator rights and touches no other account.
pub fn shell_menu_key_path() -> &'static str {
    r"Software\Classes\*\shell\VeloEdit"
}

/// The command Explorer runs for the menu item: the program's path
/// in quotes, then the clicked file as `"%1"`.
pub fn shell_menu_command(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Write one REG_SZ value under an already-open key. `name` is None
/// for the key's default value.
fn reg_set_sz(key: HKEY, name: Option<&str>, value: &str) -> Result<(), String> {
    let wide = wstr(value);
    let name_wide = name.map(wstr);
    // SAFETY: `key` is a live key handle opened by the caller with
    // KEY_WRITE; the name (or null for the default value) and the
    // NUL-terminated UTF-16 data outlive the call.
    let status = unsafe {
        RegSetValueExW(
            key,
            name_wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
            0,
            REG_SZ,
            wide.as_ptr() as *const u8,
            (wide.len() * std::mem::size_of::<u16>()) as u32,
        )
    };
    if status != 0 {
        return Err(format!(
            "could not write the Explorer menu value (registry error {status})"
        ));
    }
    Ok(())
}

/// Create (or open) a key under HKCU with write access; the caller
/// closes the returned handle.
fn reg_create_key(path: &str) -> Result<HKEY, String> {
    let wide = wstr(path);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: the path is NUL-terminated and outlives the call; the
    // out-slot is writable; a null class name and the non-volatile
    // option are the standard create-or-open form.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(format!(
            "could not create the Explorer menu key (registry error {status})"
        ));
    }
    Ok(key)
}

/// Add the "Open in VeloEdit" Explorer menu item for this program
/// (roadmap item 38): the MUIVerb and Icon on the item key, the
/// launch command on its `command` subkey — all under the current
/// user, nothing else in the registry is touched.
pub fn shell_menu_install() -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("could not find the program file: {e}"))?;
    let key = reg_create_key(shell_menu_key_path())?;
    let verb = reg_set_sz(key, Some("MUIVerb"), "Open in VeloEdit");
    let icon = verb.and_then(|()| reg_set_sz(key, Some("Icon"), &exe.to_string_lossy()));
    // SAFETY: `key` came from reg_create_key above and is closed
    // exactly once here, after the value writes.
    unsafe { RegCloseKey(key) };
    icon?;
    let command_key = reg_create_key(&format!("{}\\command", shell_menu_key_path()))?;
    let command = reg_set_sz(command_key, None, &shell_menu_command(&exe));
    // SAFETY: as above — the command subkey, closed exactly once.
    unsafe { RegCloseKey(command_key) };
    command
}

/// Remove the Explorer menu item (roadmap item 38). Returns whether
/// anything was installed; removing when nothing is installed is a
/// success, not an error.
pub fn shell_menu_uninstall() -> Result<bool, String> {
    let wide = wstr(shell_menu_key_path());
    // SAFETY: the path is NUL-terminated and outlives the call;
    // RegDeleteTreeW removes the key and its subkeys (here just the
    // command subkey) in one call.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide.as_ptr()) };
    match status {
        0 => Ok(true),
        2 => Ok(false), // ERROR_FILE_NOT_FOUND: nothing was installed
        other => Err(format!(
            "could not remove the Explorer menu item (registry error {other})"
        )),
    }
}

/// Whether the Explorer menu item is currently installed.
pub fn shell_menu_is_installed() -> bool {
    let wide = wstr(shell_menu_key_path());
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: the path is NUL-terminated and outlives the call; the
    // out-slot is writable; a handle handed back is closed below.
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide.as_ptr(), 0, KEY_READ, &mut key) };
    if status != 0 {
        return false;
    }
    // SAFETY: `key` was just opened by us and is closed exactly once.
    unsafe { RegCloseKey(key) };
    true
}

/// Attach to the parent process's console, if it has one.
///
/// The binary uses the Windows (GUI) subsystem so the editor window
/// opens without a console of its own (see src/main.rs). When the
/// program is instead started from a terminal, the headless tools
/// still need somewhere to print: attaching to the terminal's console
/// gives them exactly that. A launch with no parent console (a
/// double-click) makes the call fail, which is harmless — the result
/// is ignored either way. Called from `crate::cli::run` before
/// anything prints.
///
/// After attaching, the console code pages are set to UTF-8 (65001,
/// roadmap item 46) so the headless reports — file names especially —
/// print correctly in the terminal instead of through the legacy
/// OEM/ANSI code page.
pub fn attach_parent_console() {
    // SAFETY: ATTACH_PARENT_PROCESS (u32::MAX) names the parent
    // process's console; the call takes no pointers and its failure
    // is benign by design here. The code-page setters take a plain
    // integer (65001 = UTF-8) and likewise fail benignly when there
    // is no console; all three results are ignored on purpose.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
        windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
        windows_sys::Win32::System::Console::SetConsoleCP(65001);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The factory IIDs, byte for byte as the SDK headers define them.
    /// The first field run failed with E_NOINTERFACE because one
    /// nibble of the DirectWrite IID was wrong while the Direct2D IID
    /// was right — pin both so a typo can never ship silently again.
    #[test]
    fn factory_iids_match_the_sdk() {
        assert_eq!(IID_ID2D1_FACTORY.data1, 0x0615_2247);
        assert_eq!(IID_ID2D1_FACTORY.data2, 0x6f50);
        assert_eq!(IID_ID2D1_FACTORY.data3, 0x465a);
        assert_eq!(
            IID_ID2D1_FACTORY.data4,
            [0x92, 0x45, 0x11, 0x8b, 0xfd, 0x3b, 0x60, 0x07]
        );
        assert_eq!(IID_IDWRITE_FACTORY.data1, 0xb859_ee5a);
        assert_eq!(IID_IDWRITE_FACTORY.data2, 0xd838);
        assert_eq!(IID_IDWRITE_FACTORY.data3, 0x4b5b);
        assert_eq!(
            IID_IDWRITE_FACTORY.data4,
            [0xa2, 0xe8, 0x1a, 0xdc, 0x7d, 0x93, 0xdb, 0x48]
        );
    }

    /// The vtable slots that were wrong in v1.0.0, pinned to the
    /// SDK-verified values (see the constants block comment).
    #[test]
    fn vtable_slots_match_the_sdk() {
        assert_eq!(SLOT_RT_CLEAR, 47);
        assert_eq!(SLOT_RT_BEGIN_DRAW, 48);
        assert_eq!(SLOT_RT_END_DRAW, 49);
        assert_eq!(SLOT_RT_SET_DPI, 51);
        assert_eq!(SLOT_RT_RESIZE, 58);
        assert_eq!(SLOT_LAYOUT_GET_METRICS, 60);
        assert_eq!(SLOT_RT_DRAW_RECTANGLE, 16);
    }

    /// The custom scrollbar's thumb geometry: hidden when everything
    /// fits, proportional otherwise, clamped to a grabbable minimum.
    #[test]
    fn scrollbar_geometry_covers_the_track() {
        // Everything fits (or there is nothing): no bar.
        assert_eq!(scrollbar_geometry(200.0, 10, 10, 0), None);
        assert_eq!(scrollbar_geometry(200.0, 0, 5, 0), None);
        // Half the document visible: the thumb is half the track, and
        // the last page leaves it flush with the bottom.
        assert_eq!(scrollbar_geometry(200.0, 100, 50, 0), Some((0.0, 100.0)));
        assert_eq!(scrollbar_geometry(200.0, 100, 50, 25), Some((50.0, 100.0)));
        assert_eq!(scrollbar_geometry(200.0, 100, 50, 50), Some((100.0, 100.0)));
        // A huge file: the thumb height clamps to the minimum and it
        // still reaches the bottom on the last page.
        let (y, h) = scrollbar_geometry(200.0, 1_000_000, 50, 999_950).expect("bar drawn");
        assert_eq!(h, SCROLL_MIN_THUMB);
        assert!((y - (200.0 - SCROLL_MIN_THUMB)).abs() < 0.01);
    }

    /// Dragging converts a thumb position back to a first row; the
    /// two helpers must round-trip.
    #[test]
    fn scrollbar_first_row_inverts_the_geometry() {
        for first in [0u64, 25, 50, 75] {
            let (y, _) = scrollbar_geometry(1000.0, 100, 25, first).expect("bar drawn");
            assert_eq!(scrollbar_first_row(1000.0, 100, 25, y), first);
        }
        // Past either end clamps to the document ends.
        assert_eq!(scrollbar_first_row(1000.0, 100, 25, -50.0), 0);
        assert_eq!(scrollbar_first_row(1000.0, 100, 25, 99_999.0), 75);
        // Nothing to scroll: always the first row.
        assert_eq!(scrollbar_first_row(1000.0, 10, 25, 500.0), 0);
    }

    /// Scrollbar ticks: offsets bucket by their fraction of the
    /// document, runs landing on one row collapse into it, and
    /// degenerate inputs produce no ticks.
    #[test]
    fn tick_rows_buckets_by_document_fraction() {
        assert!(tick_rows(&[], 100, 10).is_empty());
        assert!(tick_rows(&[0, 50], 0, 10).is_empty());
        assert!(tick_rows(&[0, 50], 100, 0).is_empty());
        // Fractions land on proportional rows; the last byte is on
        // the last row.
        assert_eq!(tick_rows(&[0, 25, 50, 99], 100, 10), vec![0, 2, 5, 9]);
        // A dense run collapses to one tick per row.
        let dense: Vec<u64> = (0..100).collect();
        assert_eq!(tick_rows(&dense, 100, 4), vec![0, 1, 2, 3]);
        // Huge offsets stay exact through the u128 maths.
        let big = u64::MAX / 2;
        assert_eq!(tick_rows(&[big], u64::MAX, 1000), vec![499]);
    }

    /// The status bar's zoom readout: the settings default is
    /// 100%, and the percentage scales with the font size.
    #[test]
    fn zoom_percent_centres_on_the_default_size() {
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE), 100);
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE * 2), 200);
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE / 2), 50);
        // One step up from the default (the menu/Ctrl+wheel step
        // is 2 points) rounds to the nearest whole percent.
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE + 2), 114);
    }

    /// The polish pass's two derived tones are exact 50/50 blends.
    #[test]
    fn blend_rgba_halves_each_channel() {
        let bg = Rgba::rgb(0x10, 0x20, 0x30);
        let panel = Rgba::rgb(0x20, 0x40, 0x60);
        assert_eq!(blend_rgba(bg, panel, 0.5), Rgba::rgb(0x18, 0x30, 0x48));
        // The endpoints are the inputs themselves.
        assert_eq!(blend_rgba(bg, panel, 0.0), bg);
        assert_eq!(blend_rgba(bg, panel, 1.0), panel);
    }

    /// The status bar's size format: exact bytes, one decimal above.
    #[test]
    fn human_size_formats_like_the_status_bar() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(5), "5 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GB");
        // The design mockup's file size.
        let gb104 = (104.2 * 1024.0 * 1024.0 * 1024.0) as u64;
        assert_eq!(human_size(gb104), "104.2 GB");
    }

    /// The Explorer menu item (roadmap item 38) lives at one fixed
    /// per-user key, and its command quotes the program path and the
    /// clicked file exactly as Explorer expects.
    #[test]
    fn shell_menu_paths_and_command_are_exact() {
        assert_eq!(shell_menu_key_path(), r"Software\Classes\*\shell\VeloEdit");
        assert_eq!(
            shell_menu_command(Path::new(r"C:\Tools\VeloEdit.exe")),
            "\"C:\\Tools\\VeloEdit.exe\" \"%1\""
        );
        assert_eq!(
            shell_menu_command(Path::new(r"D:\My Apps\VeloEdit.exe")),
            "\"D:\\My Apps\\VeloEdit.exe\" \"%1\"",
            "a path with spaces stays one quoted argument"
        );
    }

    /// The armed-save suffix (roadmap items 31/32): empty while
    /// both choices are Keep, and every non-Keep combination names
    /// exactly the conversion that will run on the next save.
    #[test]
    fn save_armed_suffix_names_the_pending_conversion() {
        use crate::save::{BomChoice, EolChoice};
        assert_eq!(save_armed_suffix(EolChoice::Keep, BomChoice::Keep), "");
        assert_eq!(
            save_armed_suffix(EolChoice::Lf, BomChoice::Keep),
            " \u{00B7} \u{2192} LF"
        );
        assert_eq!(
            save_armed_suffix(EolChoice::Crlf, BomChoice::Keep),
            " \u{00B7} \u{2192} CRLF"
        );
        assert_eq!(
            save_armed_suffix(EolChoice::Keep, BomChoice::Add),
            " \u{00B7} +BOM"
        );
        assert_eq!(
            save_armed_suffix(EolChoice::Keep, BomChoice::Remove),
            " \u{00B7} \u{2212}BOM"
        );
        assert_eq!(
            save_armed_suffix(EolChoice::Crlf, BomChoice::Remove),
            " \u{00B7} \u{2192} CRLF \u{00B7} \u{2212}BOM"
        );
        assert_eq!(
            save_armed_suffix(EolChoice::Lf, BomChoice::Add),
            " \u{00B7} \u{2192} LF \u{00B7} +BOM"
        );
    }

    /// The hash dialog's verdict (roadmap item 37): blank for an
    /// empty box, patient while hashing, and it names the digest a
    /// pasted value matches (case-insensitively, whitespace ignored)
    /// or reports no match.
    #[test]
    fn hash_verdict_matches_each_digest() {
        let h = crate::hash::hash_bytes(b"veloedit");
        assert_eq!(hash_verdict("", Some(&h)), "");
        assert_eq!(hash_verdict("   ", Some(&h)), "");
        assert_eq!(hash_verdict(&h.md5, None), "Still hashing\u{2026}");
        assert_eq!(
            hash_verdict(&h.sha256.to_uppercase(), Some(&h)),
            "Matches SHA-256"
        );
        assert_eq!(
            hash_verdict(&format!("  {}  ", h.sha1), Some(&h)),
            "Matches SHA-1"
        );
        assert_eq!(hash_verdict(&h.md5, Some(&h)), "Matches MD5");
        assert_eq!(hash_verdict("deadbeef", Some(&h)), "No match");
    }

    /// The inspector panel's rows (roadmap item 15): one labelled
    /// row per reading, in display order, with the no-reading
    /// marker where the slice is too short.
    #[test]
    fn inspector_rows_cover_every_reading() {
        let bytes: [u8; 16] = [
            0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        let report = crate::inspector::inspect(&bytes);
        let rows = inspector_rows(&report, 0x100);
        let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Offset",
                "Hex",
                "Binary",
                "Int8",
                "UInt8",
                "Int16 LE",
                "Int16 BE",
                "UInt16 LE",
                "UInt16 BE",
                "Int32 LE",
                "Int32 BE",
                "UInt32 LE",
                "UInt32 BE",
                "Float32 LE",
                "Float32 BE",
                "Int64 LE",
                "Int64 BE",
                "UInt64 LE",
                "UInt64 BE",
                "Float64 LE",
                "Float64 BE",
                "Unix s (u32)",
                "Unix s (u64)",
                "Unix ms (u64)",
                "GUID",
                "FILETIME",
                "Text",
            ]
        );
        assert_eq!(rows[0].1, "256 (0x100)");
        assert_eq!(rows[24].1, "00112233-4455-6677-8899-AABBCCDDEEFF");
        // A two-byte slice: the short readings become the marker.
        let report = crate::inspector::inspect(&bytes[..2]);
        let rows = inspector_rows(&report, 0);
        assert_eq!(rows[9].1, "\u{2014}"); // Int32 LE
        assert_eq!(rows[24].1, "\u{2014}"); // GUID
    }
}
