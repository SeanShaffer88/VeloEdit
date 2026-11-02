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
    CloseHandle, GlobalFree, SetHandleInformation, COLORREF, HANDLE, HINSTANCE, HWND, LPARAM,
    LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Globalization::{MultiByteToWideChar, WideCharToMultiByte};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
    EndPaint, FillRect, FrameRect, GetStockObject, GetTextExtentPoint32W, InvalidateRect, LineTo,
    MoveToEx, Rectangle, ScreenToClient, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    UpdateWindow, HBRUSH, HDC, HFONT, HGDIOBJ, HPEN, NULL_BRUSH, OPAQUE, PAINTSTRUCT, PS_SOLID,
    TRANSPARENT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION,
    STARTUPINFOW,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetOpenFileNameW, GetSaveFileNameW, CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT,
    CHOOSECOLORW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_TRACKACTIVATE, TTM_TRACKPOSITION,
    TTTOOLINFOW,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, ShellExecuteW, HDROP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::cli::{CliOptions, Mode as CliMode};
use crate::editor::{Editor, EditorMode};
use crate::encoding;
use crate::filter::FilterView;
use crate::hexview;
use crate::line_index::{LineIndex, LineIndexBuilder};
use crate::mmap_win::MmapFile;
use crate::piece_table::{OriginalBuffer, PieceTable};
use crate::pointers::JumpHistory;
use crate::save::SaveOptions;
use crate::search::{self, SearchOptions};
use crate::settings::{Settings, DEFAULT_FONT_SIZE};
use crate::strings::FoundString;
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
// Roadmap slice R4b (view wiring): Follow Tail / Ghost Tail
// (item 22), structural filter (item 18), word wrap (item 19),
// strings pane, delimiter grid, entropy strip, time-delta gutter,
// freeze pane, focus dimming, ANSI colours + control-character
// tags (item 30 expansion), jump history (item 11 wiring).
const CMD_FOLLOW_TAIL: usize = 1030;
const CMD_FILTER: usize = 1033;
const CMD_WORD_WRAP: usize = 1034;
const CMD_STRINGS: usize = 1035;
const CMD_GRID: usize = 1036;
const CMD_ENTROPY: usize = 1037;
const CMD_TIME_DELTAS: usize = 1038;
const CMD_FREEZE: usize = 1039;
const CMD_DIM: usize = 1040;
const CMD_ANSI: usize = 1047;
const CMD_CTL_CHARS: usize = 1048;
const CMD_JUMP_BACK: usize = 1049;
const CMD_JUMP_FORWARD: usize = 1050;
// Slice R4c (systems wiring) commands:
const CMD_EXTRACT_RANGES: usize = 1051;
const CMD_EXPORT_PATCH: usize = 1052;
const CMD_APPLY_PATCH: usize = 1053;
const CMD_COMPARE_WITH: usize = 1054;
const CMD_OPEN_ADMIN: usize = 1055;
const CMD_INSERT_ZEROES: usize = 1056;
const CMD_CLEAR_FILTER: usize = 1057;
const CMD_BOOKMARK_EDIT: usize = 1058;
const CMD_BOOKMARK_TOGGLE: usize = 1059;
const CMD_BOOKMARK_NEXT: usize = 1060;
const CMD_BOOKMARK_PREV: usize = 1061;
// Tabs (roadmap item 39, slice R5b): the File-menu and keyboard
// commands, then the tab strip's right-click menu commands (they
// act on `app.tab_context`, set before the menu opens).
const CMD_NEW_TAB: usize = 1062;
const CMD_CLOSE_TAB: usize = 1063;
const CMD_NEXT_TAB: usize = 1064;
const CMD_PREV_TAB: usize = 1065;
const CMD_NEW_WINDOW: usize = 1066;
const CMD_OPEN_IN_NEW_WINDOW: usize = 1067;
const CMD_TAB_CLOSE: usize = 1070;
const CMD_TAB_CLOSE_OTHERS: usize = 1071;
const CMD_TAB_CLOSE_ALL: usize = 1072;
const CMD_TAB_COPY_PATH: usize = 1073;
const CMD_TAB_NEW_WINDOW: usize = 1074;
// Split view and side-by-side diff (roadmap item 28 and the R5
// line diff, slice R5c).
const CMD_SPLIT_VIEW: usize = 1075;
const CMD_COMPARE_SIDE: usize = 1076;
const CMD_DIFF_NEXT: usize = 1077;
const CMD_DIFF_PREV: usize = 1078;
const CMD_DIFF_RECOMPUTE: usize = 1079;
// Slice R5d command ids.
const CMD_FIND_REGEX: usize = 1080;
const CMD_APPLY_TEMPLATE: usize = 1081;
const CMD_DISASM: usize = 1082;
const CMD_TOOLS_RELOAD: usize = 1083;
const CMD_TOOL_BASE: usize = 3410; // + tool index (data/tools.json order)
/// The split/compare divider's painted width, in DIPs.
const DIVIDER_W: f32 = 5.0;
/// The narrowest either pane of a split/compare may become.
const MIN_PANE_W: f32 = 180.0;
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
/// Encoding menu (slice R4c, item 29): the standard encodings as
/// radio items, `CMD_ENC_BASE + index` into the menu's order
/// (UTF-8, UTF-16 LE, UTF-16 BE, ANSI).
const CMD_ENC_BASE: usize = 3380;
/// The Code Pages submenu: `CMD_CODEPAGE_BASE + 0` is "None
/// (detected)"; `+ 1 + i` selects `CodePage::ALL[i]`.
const CMD_CODEPAGE_BASE: usize = 3390;

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
/// The tab strip's height (roadmap item 39): one row of document
/// tabs between the accent strip and the toolbar.
const TABBAR_H: f32 = 28.0;
/// Tab width bounds (DIPs): a tab never renders narrower or wider
/// than these; overflow past the minimum scrolls the strip.
const TAB_MIN_W: f32 = 76.0;
const TAB_MAX_W: f32 = 208.0;
/// Space reserved inside every tab for its close ×, so tabs never
/// change width when the × appears on the active/hovered tab.
const TAB_CLOSE_W: f32 = 18.0;
/// The find bar row matches the toolbar row's height, so the find
/// bar's controls come out the same size as the toolbar buttons
/// (both rows place their controls at `row height − 6` DIPs).
const FINDBAR_H: f32 = 30.0;
/// Height of the external-change notice bar (roadmap item 34) when
/// it is showing, directly under the toolbar.
const CHANGEBAR_H: f32 = 30.0;
const STATUS_H: f32 = 26.0;
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
TABS\r\n\
One window can hold many documents, one tab each, in the strip\r\n\
under the top edge. Each tab keeps its own place — caret, scroll,\r\n\
text or hex mode, undo history, find and filter work, even a\r\n\
search or filter still running — so switching tabs never loses\r\n\
anything. A filled square on a tab means unsaved changes; a\r\n\
hollow square means the file is read-only. Rest the pointer on\r\n\
a tab to see its full path. The + button opens a new empty tab.\r\n\
File \u{25B8} Open, the Recent list, and files dropped on the window\r\n\
open into a new tab when the current tab already holds a file\r\n\
or has unsaved changes; an untouched empty tab is used instead,\r\n\
so opening a file never disturbs what you are doing or asks\r\n\
about its changes. Ctrl+T opens a new tab, Ctrl+W closes the\r\n\
current one, Ctrl+Tab and Ctrl+Shift+Tab move between tabs,\r\n\
and Ctrl+1 to Ctrl+8 jump to a tab by number (Ctrl+9 jumps to\r\n\
the last tab). Click a tab to switch to it, middle-click to\r\n\
close it, drag it sideways to reorder it, and right-click it\r\n\
for Close, Close Others, Close All, Copy Path, and Open in New\r\n\
Window. When the tabs no longer fit, they shrink and then the\r\n\
strip scrolls — turn the mouse wheel over the strip to scroll\r\n\
it. Closing a tab with unsaved changes asks whether to save\r\n\
it, like closing the window always has; closing the last tab\r\n\
leaves a fresh empty document, and closing the window asks\r\n\
about every document with unsaved changes in turn. File \u{25B8} New\r\n\
Window starts a separate VeloEdit window (its own program\r\n\
copy), and Open in New Window opens the current file in one.\r\n\
When the window closes cleanly, the list of open tabs is\r\n\
remembered and reopened next time, on the tab you left active;\r\n\
files that have gone missing are skipped without a fuss.\r\n\
Untitled documents are not remembered this way — an unsaved\r\n\
one still relies on the recovery record described above.\r\n\
\r\n\
SPLIT VIEW\r\n\
View \u{25B8} Split Text/Hex (Ctrl+\\) shows the same document twice,\r\n\
side by side: the text on the left, the bytes as hex on the\r\n\
right. The two panes are tied together by the bytes they\r\n\
show — scroll one and the other follows, because the byte at\r\n\
the top of one pane is the byte at the top of the other. The\r\n\
caret's byte is marked in the pane you are not using, so you\r\n\
can always see where you are in both. Click a pane (or turn\r\n\
the mouse wheel over it) to work in it; edits happen in\r\n\
whichever pane you clicked, and both panes show the result.\r\n\
The Text and Hex buttons both light up while split, and\r\n\
clicking one moves the keyboard to that pane instead of\r\n\
changing the view. Drag the divider between the panes to\r\n\
make one wider; where you leave it is remembered. A few\r\n\
things belong to the single view and step aside while split:\r\n\
the filter view and frozen lines cannot start (the status\r\n\
bar says why), and the delimiter grid waits until split is\r\n\
off. Turning split off returns to whichever pane you were\r\n\
using last.\r\n\
\r\n\
COMPARE SIDE BY SIDE\r\n\
File \u{25B8} Compare Side by Side\u{2026} opens a second file in a\r\n\
pane on the right, next to your document. The second file is\r\n\
only ever read, never changed — its pane takes no typing at\r\n\
all. The two panes scroll together, line by line in text\r\n\
mode or row by row in hex mode, so the same place in both\r\n\
files stays side by side. In text mode VeloEdit also works\r\n\
out where the two files differ, in the background: differing\r\n\
stretches are shaded in both panes, the status bar counts\r\n\
them, and Search \u{25B8} Next Difference (F7) and Previous\r\n\
Difference (Shift+F7) jump between them, moving both panes\r\n\
together. If you edit your document after the differences\r\n\
were worked out, the shading stays but the status bar marks\r\n\
it stale; Search \u{25B8} Compare Differences works it out\r\n\
again. In hex mode there is no shading — the panes simply\r\n\
scroll together, and File \u{25B8} Compare With\u{2026} remains the\r\n\
tool for byte-level differences. Press Esc (or choose the\r\n\
menu item again) to close the compare; switching tabs closes\r\n\
it too. The Data Inspector steps aside while a compare is\r\n\
open and returns when it closes.\r\n\
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
out the MD5, SHA-1, SHA-256, CRC32 and xxHash64 checksums of\r\n\
the file on disk, and checks any checksum you paste in\r\n\
against them.\r\n\
\r\n\
THEMES\r\n\
View ▸ Theme offers 20 built-in colour themes; View ▸ Accent recolours\r\n\
the highlight colour on top of any theme. Moving through those menus\r\n\
previews each choice in the window — click to keep it, or close the\r\n\
menu to go back to what you had. Your choices are stored in\r\n\
the data folder next to the program — VeloEdit is fully portable and\r\n\
works completely offline.\r\n\
\r\n\
MORE WAYS TO LOOK\r\n\
View \u{25B8} Follow Tail keeps an eye on a file that is still being\r\n\
written (a log, for instance): when the file grows and you have\r\n\
not edited it, the new lines simply arrive — nothing reloads and\r\n\
your place is kept. The status bar reads Following while it is on.\r\n\
Search \u{25B8} Filter\u{2026} shows only the lines that contain a word\r\n\
or phrase, each with its real line number. On a large file the\r\n\
filter opens at once and fills in as the search works through\r\n\
the file — the status bar reads Filtering\u{2026} with a percentage\r\n\
until it is done. Walk the rows with the arrow keys; Enter\r\n\
jumps back to the full file at that line, and Esc goes back\r\n\
without moving. Confirming the Filter prompt with the text\r\n\
left empty leaves the filter, and Search \u{25B8} Clear Filter does\r\n\
the same. You cannot type while the filter is up — the status\r\n\
bar says what is happening.\r\n\
View \u{25B8} Word Wrap folds long lines at the edge of the window\r\n\
instead of letting them run off it. Wrapping is only how lines\r\n\
are shown; the file never changes.\r\n\
View \u{25B8} Strings Pane lists the readable text found inside a\r\n\
binary file along the bottom of the window; double-click one to\r\n\
jump to where it sits. View \u{25B8} Entropy Strip draws a thin\r\n\
rainbow-like bar under the toolbar — packed or encrypted parts\r\n\
of a file glow in the accent colour, plain text stays dark — and\r\n\
clicking the bar jumps the view to that part of the file.\r\n\
View \u{25B8} Time Deltas is for log files: when lines begin with a\r\n\
time, the gutter shows how long passed between one line and the\r\n\
next. View \u{25B8} Delimiter Grid lines up comma, tab, semicolon\r\n\
or pipe separated values into columns — display only: positions\r\n\
and copying still use the file's real text. View \u{25B8} Freeze\r\n\
Top Lines\u{2026} pins the first lines (a header row, say) at the top\r\n\
while the rest scrolls; frozen lines can be looked at but not\r\n\
edited. View \u{25B8} Dim Inactive Paragraphs fades everything\r\n\
except the block of lines you are working in.\r\n\
View \u{25B8} ANSI Colours paints the colour codes found in\r\n\
terminal logs as real colours, and View \u{25B8} Show Control\r\n\
Characters shows invisible control bytes as small tags like ^A.\r\n\
With either on, the window may paint text wider or narrower than\r\n\
the byte columns the status bar reports — the status bar always\r\n\
tells the truth about the file.\r\n\
Resting the mouse pointer on a scrollbar tick (a match or a\r\n\
bookmark) shows which line it belongs to. In hex mode,\r\n\
Ctrl+click a byte — or put the caret on it and press Ctrl+Enter —\r\n\
to follow a stored pointer to where it points, and Alt+Left /\r\n\
Alt+Right walk back and forward through your jumps. The Data\r\n\
Inspector also grows a Bits row in hex mode: eight boxes for the\r\n\
byte at the caret, and clicking a box flips that bit (undo works\r\n\
as usual).\r\n\
\r\n\
READING THE WINDOW\r\n\
The toolbar buttons carry small icons beside their words, and\r\n\
the Text / Hex switch is one joined control with the side in\r\n\
use filled in the accent colour. If you move through the\r\n\
controls with the keyboard, the control you are on is outlined\r\n\
inside its border in the accent colour, so you can always see\r\n\
where you are. The status bar shows the text size as a\r\n\
percentage beside the file facts — 100% is the standard size.\r\n\
\r\n\
MORE TOOLS\r\n\
File \u{25B8} Extract Ranges\u{2026} copies chosen parts of the file into a\r\n\
new file. Type the ranges in ascending order, separated by ; —\r\n\
byte ranges count both ends in (0-1023, or hex 0x1000-0x1FFF),\r\n\
and line ranges take whole lines with their endings (L10-L20).\r\n\
File \u{25B8} Export Delta Patch\u{2026} writes a small patch file holding\r\n\
only your changes compared with the file on disk; File \u{25B8} Apply\r\n\
Patch\u{2026} applies such a patch to the file in the window and\r\n\
writes the result to a new file — the original is never\r\n\
overwritten. Edit \u{25B8} Insert Zeroes\u{2026} adds a run of zero bytes at\r\n\
the caret (type a number, or a size like 4K, 2M or 1G); undo\r\n\
removes the whole run at once. File \u{25B8} Compare With\u{2026} compares\r\n\
the document with another file block by block and lists the\r\n\
regions that differ; Go to jumps the caret to a region.\r\n\
Bookmarks can carry a colour and a short note: right-click a\r\n\
bookmark's gutter square, or use Search \u{25B8} Bookmarks \u{25B8} Edit\r\n\
Bookmark\u{2026}. The gutter square and the scrollbar tick draw in\r\n\
the bookmark's colour, and jumping to a bookmark with F2 shows\r\n\
its note in the status bar.\r\n\
The Encoding menu re-reads the document under a different\r\n\
character encoding without changing its bytes, including the\r\n\
Code Pages submenu (Shift-JIS, GBK, EUC-KR, ISO-8859-1,\r\n\
ISO-8859-15, Windows-1250, Windows-1251) for files written by\r\n\
older or regional programs. The status bar shows the page in\r\n\
use, like CP932. Text typed or pasted is converted back\r\n\
through the same page when you save. A character the page\r\n\
cannot show becomes a replacement mark on screen, and one it\r\n\
cannot store is written as ? — pages are a viewing choice;\r\n\
nothing is detected or changed until you pick one.\r\n\
A file marked read-only opens with (read-only) in the title\r\n\
and Read-only file in the status bar. Saving then offers to\r\n\
save a copy under a different name instead of failing. File \u{25B8}\r\n\
Open as Administrator opens a new window with full rights on\r\n\
the same file, for the rare case a file truly needs it —\r\n\
Windows asks you to approve the step first.\r\n\
The Regex tick on the find bar searches with a regular\r\n\
expression instead of plain text: the whole document is\r\n\
searched, matches can be any length, and the count is exact.\r\n\
Replace and Replace All put the replacement in literally (no\r\n\
$1 tricks), and Replace All is still a single undo. Regex\r\n\
works in UTF-8 and ANSI files; for UTF-16 and code-page files\r\n\
the find bar says so instead of guessing. A pattern that does\r\n\
not parse shows its error in the find bar and searches\r\n\
nothing.\r\n\
Tools \u{25B8} Apply Template\u{2026} reads a binary file's structure\r\n\
through a template: plain-text descriptions of structures\r\n\
(field names, types like u32le, arrays), kept as .txt files in\r\n\
a templates folder inside the data folder next to the\r\n\
program. Pick a file, pick a structure, say where to apply it\r\n\
(the caret is the usual start) and the fields appear in a\r\n\
list under the document: name, offset, size, value.\r\n\
Double-click a row to jump to that field. Two ready-made\r\n\
examples, for PE and PNG headers, ship in the examples folder\r\n\
in the source repository — copy them into data/templates to\r\n\
use them. If the folder or the files are missing, the dialog\r\n\
says so and names the folder to create.\r\n\
The Tools menu can also run your own programs. Put a\r\n\
tools.json file in the data folder: a list of tools, each\r\n\
with a name, a command and its arguments, what to feed it\r\n\
(input: the selection, the caret's line, the file, or\r\n\
nothing) and what to do with what it prints (output: replace\r\n\
the selection, insert at the caret, write to a new file, or\r\n\
just show it). {file} and {offset} in the arguments are\r\n\
replaced with the document's path and the caret's offset. A\r\n\
run never freezes the window: the tool works in the\r\n\
background, the status bar shows its name, Esc stops it, and\r\n\
a run is stopped after 30 seconds. A tool that prints more\r\n\
than 16 MB, fails to start, or exits with an error says so\r\n\
plainly. If the document changes while a tool runs, its\r\n\
output is shown instead of applied, so nothing lands in the\r\n\
wrong place. Tools \u{25B8} Reload Tools re-reads the file.\r\n\
In text mode you can edit in several places at once.\r\n\
Alt+click adds a caret (Alt+click one again to remove it);\r\n\
Ctrl+Alt+Up and Ctrl+Alt+Down add a caret on the line above\r\n\
or below. Typing, Backspace, Delete, Enter and Tab apply at\r\n\
every caret as one undo. The status bar counts the carets\r\n\
while there is more than one. A plain click, Esc, or any\r\n\
movement key collapses back to one caret. Alt+drag selects a\r\n\
rectangle of columns across lines: Copy and Cut take its\r\n\
text (one line per row), and Delete or typing replaces it.\r\n\
View \u{25B8} Disassembly opens a listing, in hex mode only, of\r\n\
the machine instructions starting at the caret — offset,\r\n\
bytes, instruction. It follows the caret, double-clicking a\r\n\
row moves the caret there, and its heading always names the\r\n\
offset it decoded from, so a stale listing never pretends to\r\n\
be current. While it is open the Data Inspector steps aside\r\n\
and returns when it closes.\r\n\
Right-clicking VeloEdit's taskbar button shows the recent\r\n\
files and a New Window entry. For screen readers, the window\r\n\
reports the current line and caret position; that is a\r\n\
scoped step, not full document reading. Lines containing\r\n\
wide characters (such as CJK text) are measured exactly, so\r\n\
the caret, selections and clicking line up with the glyphs.\r\n\
\r\n\
KEYBOARD SHORTCUTS\r\n\
Every shortcut can be changed. Put a file named\r\n\
keybindings.json in the data folder next to the program: a\r\n\
flat list pairing an action name with the keys for it, like\r\n\
{ \"find\": \"Ctrl+Shift+F\", \"toggle_hex\": \"F6\" }. An empty\r\n\
string unbinds an action. If the file has mistakes, the\r\n\
actions concerned keep their defaults and the status bar says\r\n\
so once at startup. The defaults are: open Ctrl+O \u{00B7} save\r\n\
Ctrl+S \u{00B7} save_as Ctrl+Shift+S \u{00B7} undo Ctrl+Z \u{00B7} redo Ctrl+Y \u{00B7}\r\n\
cut Ctrl+X \u{00B7} copy Ctrl+C \u{00B7} paste Ctrl+V \u{00B7} select_all Ctrl+A \u{00B7}\r\n\
find Ctrl+F \u{00B7} replace Ctrl+H \u{00B7} find_next F3 \u{00B7} find_prev\r\n\
Shift+F3 \u{00B7} toggle_hex F6 \u{00B7} guide F1 \u{00B7} font_bigger Ctrl++ \u{00B7}\r\n\
font_smaller Ctrl+- \u{00B7} toggle_bookmark Ctrl+F2 \u{00B7} bookmark_next\r\n\
F2 \u{00B7} bookmark_prev Shift+F2 \u{00B7} jump_back Alt+Left \u{00B7}\r\n\
jump_forward Alt+Right \u{00B7} pointer_follow Ctrl+Enter \u{00B7} new_tab\r\n\
Ctrl+T \u{00B7} close_tab Ctrl+W \u{00B7} next_tab Ctrl+Tab \u{00B7} prev_tab\r\n\
Ctrl+Shift+Tab \u{00B7} tab_1 to tab_8 Ctrl+1 to Ctrl+8 \u{00B7} tab_last\r\n\
Ctrl+9 \u{00B7} toggle_split Ctrl+\\ \u{00B7} diff_next F7 \u{00B7} diff_prev\r\n\
Shift+F7 \u{00B7} caret_up Ctrl+Alt+Up \u{00B7} caret_down Ctrl+Alt+Down.\r\n\
follow_tail, filter and clear_filter start unbound.\r\n\
Two settings live only in settings.json in the data folder:\r\n\
\"map_window_mb\" (0, 512, 1024, 2048 or 4096; 0 maps the whole\r\n\
file, a number reads very large files through a moving window\r\n\
of that many megabytes) and \"fast_save_clone\" (true lets a\r\n\
save reuse the unchanged parts of the file already on disk\r\n\
where the drive supports it, falling back to the normal save\r\n\
whenever it cannot).\r\n";

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
const SLOT_LAYOUT_HIT_TEST_POINT: usize = 64; // IDWriteTextLayout::HitTestPoint
const SLOT_LAYOUT_HIT_TEST_TEXT_POSITION: usize = 65; // IDWriteTextLayout::HitTestTextPosition

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

/// DWRITE_HIT_TEST_METRICS (dwrite.h): where a hit-test against a
/// text layout landed — the character position and the exact
/// geometry of the glyph cluster there.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DwHitTestMetrics {
    text_position: u32,
    length: u32,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    bidi_level: u32,
    is_text: i32,
    is_trimmed: i32,
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
    /// The 16 ANSI SGR foreground colours (item 30 expansion),
    /// indices 0..8 = codes 30..37, 8..16 = codes 90..97.
    ansi: [*mut c_void; 16],
    /// The entropy strip's colour ramp (16 steps from the panel
    /// colour to the accent).
    ramp: [*mut c_void; 16],
    /// The bookmark annotation swatches (slice R4c, item 5): one
    /// brush per colour in `crate::bookmarks::BOOKMARK_SWATCHES`,
    /// built from its fixed RGB values (theme-independent by
    /// design — the swatches are chosen to read on every theme).
    swatches: [*mut c_void; 6],
    /// The side-by-side diff bands (slice R5c): a dedicated tint
    /// derived from the theme accent — the background blended a
    /// quarter of the way toward the accent, and halfway for the
    /// region the user is currently parked on. Accent-derived so
    /// the bands follow the (possibly overridden) accent like
    /// everything else in the palette.
    diff_bg: *mut c_void,
    diff_bg_hot: *mut c_void,
}

/// The ANSI SGR foreground palette: the standard terminal colours,
/// chosen bright enough to read on the dark themes. Codes 30–37
/// index 0..8, bright codes 90–97 index 8..16.
const ANSI_PALETTE: [Rgba; 16] = [
    Rgba::rgb(0x66, 0x66, 0x70),
    Rgba::rgb(0xE0, 0x52, 0x52),
    Rgba::rgb(0x4C, 0xC3, 0x8A),
    Rgba::rgb(0xE5, 0xC0, 0x7B),
    Rgba::rgb(0x5B, 0x8D, 0xEF),
    Rgba::rgb(0xC6, 0x78, 0xDD),
    Rgba::rgb(0x56, 0xB6, 0xC2),
    Rgba::rgb(0xD4, 0xD4, 0xD8),
    Rgba::rgb(0x8A, 0x8A, 0x93),
    Rgba::rgb(0xFF, 0x6B, 0x6B),
    Rgba::rgb(0x69, 0xDB, 0x7C),
    Rgba::rgb(0xFF, 0xD4, 0x3B),
    Rgba::rgb(0x74, 0xC0, 0xFC),
    Rgba::rgb(0xE5, 0x99, 0xF7),
    Rgba::rgb(0x66, 0xD9, 0xE8),
    Rgba::rgb(0xF8, 0xF9, 0xFA),
];

impl Brushes {
    /// Build the full brush set for a theme against a live target.
    fn build(target: *mut c_void, t: &Theme) -> Brushes {
        Brushes {
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
            ansi: std::array::from_fn(|i| make_brush(target, ANSI_PALETTE[i])),
            ramp: std::array::from_fn(|i| {
                make_brush(target, blend_rgba(t.panel, t.accent, i as f32 / 15.0))
            }),
            swatches: std::array::from_fn(|i| {
                let (_, (r, g, b)) = crate::bookmarks::BOOKMARK_SWATCHES[i];
                make_brush(target, Rgba::rgb(r, g, b))
            }),
            diff_bg: make_brush(target, blend_rgba(t.bg, t.accent, 0.25)),
            diff_bg_hot: make_brush(target, blend_rgba(t.bg, t.accent, 0.5)),
        }
    }

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
            &mut self.diff_bg,
            &mut self.diff_bg_hot,
        ]
        .into_iter()
        .chain(self.ansi.iter_mut())
        .chain(self.ramp.iter_mut())
        .chain(self.swatches.iter_mut())
        {
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
    /// Regular-expression find (roadmap item 9, slice R5d): the
    /// find bar's Regex toggle. Matches carry variable-length
    /// spans from the regex engine.
    Regex,
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
    /// Match END offsets, parallel to `matches` — populated only
    /// by a background regex find (slice R5d), whose matches vary
    /// in length; a plain find leaves it empty and every end is
    /// the start plus the needle length, as before.
    ends: Vec<u64>,
    done: bool,
}

/// Shared state for the background filter build (slice R4c, item
/// 13): the worker folds its document snapshot window by window
/// into a `crate::filter::FilterBuilder` and publishes view
/// snapshots here; the UI thread attaches them on its tick. The
/// generation tags follow the find discipline: `filter_gen` bumps
/// on every new build and every cancel, `doc_gen` must match the
/// document generation the build started from — a stale worker's
/// snapshots are simply never attached.
struct FilterProgress {
    filter_gen: u64,
    doc_gen: u64,
    view: std::sync::Arc<crate::filter::FilterView>,
    scanned: u64,
    done: bool,
}

/// The canvas tooltip tool's id (the scrollbar tick hover preview).
const TICK_TOOL_ID: usize = 0x7E57;

thread_local! {
    /// The tick hover preview's text buffer. The tooltip control
    /// keeps the pointer, so the buffer is a fixed-size scratch that
    /// is never reallocated: contents are rewritten in place before
    /// each activation (the same deliberate-lifetime trick
    /// `add_tooltip` uses, without leaking per control).
    static HOVER_TEXT_SCRATCH: std::cell::RefCell<Vec<u16>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Rewrite the tick hover preview's text in place.
fn set_hover_text(text: &str) {
    HOVER_TEXT_SCRATCH.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.is_empty() {
            buf.resize(512, 0);
        }
        let mut n = 0;
        for unit in text.encode_utf16() {
            if n >= 500 {
                break;
            }
            buf[n] = unit;
            n += 1;
        }
        buf[n] = 0;
    });
}

/// The structural filter view's shell state (roadmap item 18): the
/// core model plus the view's own scroll position and row cursor.
/// The document caret never moves while the filter is up — the
/// cursor is a row highlight, and Enter turns it into a real jump.
struct FilterState {
    view: FilterView,
    /// The pattern as typed, for the banner.
    pattern: String,
    /// Index of the first visible filtered row.
    first_row: usize,
    /// The row cursor (an index into the filtered rows).
    cursor_row: usize,
    /// A background build is still walking the document (slice
    /// R4c, item 13); `view` holds the matches found so far.
    building: bool,
    /// Bytes the running build has covered, for the banner's
    /// "Filtering… NN%".
    scanned: u64,
}

/// The strings worker's delivery slot: (generation, min length,
/// results).
type StringsSlot = Arc<Mutex<Option<(u64, u32, Vec<FoundString>)>>>;

/// The entropy worker's delivery slot: (generation, values).
type EntropySlot = Arc<Mutex<Option<(u64, Vec<f64>)>>>;

/// The strings pane's shell state: the worker's slot and the UI
/// mirror, epoch-guarded like the background find (the generation
/// bumps on every document change — open, edit, save).
struct StringsState {
    slot: StringsSlot,
    gen: u64,
    results: Vec<FoundString>,
    /// Index of the first visible result row in the pane.
    first: usize,
    /// The selected result row, drawn highlighted.
    selected: Option<usize>,
    searching: bool,
    /// The generation the held `results` belong to; a mismatch with
    /// `gen` means a rescan is owed.
    scanned_gen: u64,
    /// The minimum length the held results were scanned with.
    scanned_min_len: u32,
}

/// The entropy strip's shell state: the worker's slot and the UI
/// mirror, with its own generation (bumped wherever the strings
/// generation bumps).
struct EntropyState {
    slot: EntropySlot,
    gen: u64,
    values: Vec<f64>,
    computing: bool,
    scanned_gen: u64,
}

/// What the hover tooltip is previewing: a scrollbar tick (a match
/// or bookmark offset) or a document tab (its full path). One
/// tracking tooltip serves both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HoverKind {
    Tick(u64, bool),
    Tab(usize),
    /// A fixed explanation (slice R5c): the split/compare
    /// divider and the panes it separates.
    Note(&'static str),
}

/// One document's complete shell state (tabs, roadmap item 39,
/// slice R5b): everything that belongs to a document rather than
/// to the window — the editor itself (caret, scroll, selection,
/// mode, undo, encoding all live inside it), the find state and
/// its background worker, the filter view and its builder, the
/// staged line-index build, follow-tail, bookmarks, the
/// external-change bar, delta recovery, the inspector/hex
/// transients, the jump history, and the armed save options.
///
/// The ACTIVE document's state lives in `App`'s own fields of the
/// same names — every feature was written against those, and they
/// keep working unchanged. Inactive documents keep their state in
/// a `DocState` in `App::parked`; switching tabs swaps the two
/// field by field (`App::swap_state`). Background workers hold
/// clones of their document's delivery slots, so a worker started
/// for a parked document keeps publishing into that document's
/// state and its results are picked up when the tab returns.
struct DocState {
    /// Stable identity for the close-others/all sweeps (indices
    /// shift as tabs close; ids do not).
    id: u64,
    editor: Editor,
    find_in_selection: bool,
    /// The find bar's Regex toggle (slice R5d), parked per document
    /// exactly like `find_in_selection`.
    find_regex: bool,
    find_scope_range: Option<(u64, u64)>,
    last_scope_range: Option<(u64, u64)>,
    last_find_kind: FindKind,
    doc_mmap: Option<Arc<MmapFile>>,
    find_slot: Arc<Mutex<Option<FindProgress>>>,
    find_gen: u64,
    bg_active: bool,
    bg_matches: Vec<u64>,
    bg_done: bool,
    bg_current: Option<usize>,
    bg_needle_len: u64,
    bg_ends: Vec<u64>,
    find_sync_once: bool,
    bookmarks: crate::bookmarks::BookmarkList,
    doc_stamp: Option<crate::bookmarks::FileStamp>,
    file_stamp: Option<crate::bookmarks::FileStamp>,
    change_visible: bool,
    change_shown_for: Option<crate::bookmarks::FileStamp>,
    change_ignored_for: Option<crate::bookmarks::FileStamp>,
    fingerprint_slot: Arc<Mutex<Option<(u64, crate::recovery::FileFingerprint)>>>,
    doc_epoch: u64,
    doc_fingerprint: Option<crate::recovery::FileFingerprint>,
    eol: &'static str,
    rel_offsets: bool,
    rel_base: u64,
    find_visible: bool,
    pending_high_surrogate: Option<u16>,
    hex_nibble: Option<u8>,
    index_slot: Arc<Mutex<Option<(u64, IndexProgress)>>>,
    indexing: Option<(u64, u64)>,
    doc_generation: u64,
    open_generation: u64,
    last_edit: Option<Instant>,
    last_find_text: String,
    pending_goto: Option<u64>,
    index_epoch: u64,
    status_note: Option<String>,
    tail_pin_view: bool,
    filter: Option<FilterState>,
    strings: StringsState,
    entropy: EntropyState,
    entropy_on: bool,
    grid_on: bool,
    deltas_on: bool,
    dim_on: bool,
    ansi_on: bool,
    ctl_on: bool,
    jump_history: JumpHistory,
    filter_slot: Arc<Mutex<Option<FilterProgress>>>,
    filter_gen: u64,
    doc_read_only_file: bool,
    /// The armed File ▸ Save Options choices (roadmap items
    /// 31/32), per document: a fresh document starts from the
    /// persisted settings, and the menu writes both the document
    /// and the settings (so the choice stays the default for the
    /// next document and the next run).
    save_eol: crate::save::EolChoice,
    save_bom: crate::save::BomChoice,
    /// The find bar's control texts, which live in the child
    /// windows while the document is active: parked here so a tab
    /// switch restores the bar exactly as it was left.
    find_box: String,
    replace_box: String,
    find_count: String,
}

/// A tab drag in progress (slice R5b): the tab being dragged, the
/// pointer's DIP x at the grab and now, and whether the pointer
/// has moved far enough to count as a drag (rather than a click).
struct TabDrag {
    index: usize,
    start_x: f32,
    x: f32,
    moved: bool,
}

/// A finished side-by-side diff computation (slice R5c),
/// delivered by the background worker through the compare's
/// slot. `diff_gen` and `doc_gen` are the generations the
/// computation started from; a result whose generations no
/// longer match is discarded, exactly like the find worker's.
struct DiffProgress {
    diff_gen: u64,
    doc_gen: u64,
    report: crate::linediff::LineDiffReport,
    /// The run covered only the indexed prefix of one side
    /// (its line index was still building): the regions are
    /// honest for that prefix, and the compare re-runs once
    /// both indexes complete.
    prefix: bool,
}

/// The side-by-side compare session (slice R5c): a second file
/// shown read-only in a right-hand pane next to the active
/// document. Owned by the WINDOW, not by any tab — switching
/// tabs closes it. Its editor is a full `Editor` over its own
/// mapping (the same lightweight open path as a document,
/// marked read-only), with its own staged line-index build —
/// but no undo, no find, no recovery: the pane takes no text
/// input at all, and nothing here is ever written back.
struct CompareState {
    editor: Editor,
    path: std::path::PathBuf,
    mmap: Option<std::sync::Arc<crate::mmap_win::MmapFile>>,
    /// The staged index build's delivery slot (an
    /// `(epoch, progress)` pair, the document index's shape).
    index_slot: std::sync::Arc<std::sync::Mutex<Option<(u64, IndexProgress)>>>,
    index_epoch: u64,
    /// `(indexed_bytes, total)` while the right file's index is
    /// still building; `None` once complete.
    indexing: Option<(u64, u64)>,
    /// The diff worker's delivery slot.
    diff_slot: std::sync::Arc<std::sync::Mutex<Option<DiffProgress>>>,
    diff_gen: u64,
    /// The last completed difference report (text mode only).
    diff: Option<crate::linediff::LineDiffReport>,
    /// The held report covers only an indexed prefix.
    diff_prefix: bool,
    /// The left document changed after the report was computed:
    /// the bands stay painted but the status bar says they are
    /// stale and offers the recompute command.
    diff_stale: bool,
    /// A diff computation is running on the worker.
    diff_computing: bool,
    /// The region the user last jumped to.
    current: Option<usize>,
    /// The panes' scroll pairing: right first unit = left first
    /// unit + delta (lines in text mode, hex rows in hex mode).
    /// Zero until a difference jump pairs the panes.
    delta: i64,
}

/// The compare pane's gathered rows (slice R5c): the right
/// file's rows in the document's mode, painted plainly. The
/// text variant carries its gutter width, computed during the
/// gather (the editor's line count needs `&mut`).
enum CmpRows {
    Text {
        rows: Vec<crate::editor::TextRow>,
        gutter_w: f32,
    },
    Hex(Vec<crate::editor::HexRow>),
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
    /// The find bar's "Regex" toggle (slice R5d), an owner-drawn
    /// button in the same family as the scope toggle.
    find_regex_check: HWND,
    /// Whether the find bar searches with a regular expression
    /// (roadmap item 9, slice R5d). Parked per document like
    /// `find_in_selection`.
    find_regex: bool,
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
    /// Match END offsets while a background REGEX find owns the
    /// display (slice R5d), parallel to `bg_matches`; empty for a
    /// plain background find, whose ends are start + needle length.
    bg_ends: Vec<u64>,
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
    // --- Roadmap slice R4b (view wiring) -----------------------------
    /// One-shot status-bar note (a pointer jump's origin, a Ghost
    /// Tail growth summary): appended to the next status refresh,
    /// then cleared by the next caret move or edit.
    status_note: Option<String>,
    /// Ghost Tail: the view was at the end of the document when a
    /// growth was adopted; when the resumed index completes, pin
    /// the view to the new end once.
    tail_pin_view: bool,
    /// The structural filter view (item 18), while active.
    filter: Option<FilterState>,
    /// The strings pane's state.
    strings: StringsState,
    /// The entropy strip's state (worker + mirror).
    entropy: EntropyState,
    /// Entropy strip on/off (session-only, like relative offsets).
    entropy_on: bool,
    /// Delimiter grid on/off (session-only).
    grid_on: bool,
    /// Time-delta gutter on/off (session-only).
    deltas_on: bool,
    /// Focus dimming on/off (session-only).
    dim_on: bool,
    /// ANSI colour rendering on/off (session-only).
    ansi_on: bool,
    /// Control-character glyph tags on/off (session-only).
    ctl_on: bool,
    /// Pointer-follow jump history (item 11 wiring): pushed at the
    /// shell's jump points, cleared on open.
    jump_history: JumpHistory,
    /// The main tooltip control, kept so the scrollbar tick hover
    /// preview can drive its tracking tool.
    tooltip: HWND,
    /// The canvas tooltip tool was registered.
    hover_tool_ready: bool,
    /// The preview the hover tooltip currently shows (a
    /// scrollbar tick or a document tab).
    hover_shown: Option<HoverKind>,
    /// The hover candidate under the pointer and when it settled.
    hover_cand: Option<(HoverKind, Instant)>,
    /// The Data Inspector's bit boxes from the last render (DIPs),
    /// bit 7 first, plus the caret offset and byte they mirror.
    bit_boxes: Vec<(f32, f32, f32, f32)>,
    bit_mirror: Option<(u64, u8)>,
    /// The strings pane's min-length chips from the last render:
    /// `(min_len, rect in DIPs)`.
    strings_chips: Vec<(u32, (f32, f32, f32, f32))>,
    // --- Roadmap slice R4c (systems wiring) ------------------------
    /// The background filter build's delivery slot and generation
    /// (item 13).
    filter_slot: Arc<Mutex<Option<FilterProgress>>>,
    filter_gen: u64,
    /// The merged key bindings (item 48): the shipped defaults
    /// overlaid with `data/keybindings.json`. The accelerator
    /// dispatch and the menu chord labels read from this map.
    keybindings: crate::keybindings::KeyBindings,
    /// `keybindings.json` existed but had problems (bad JSON,
    /// unknown actions, unparseable chords); the status bar says
    /// so once at startup and the defaults are in force.
    keybindings_problem: bool,
    /// The current document's file carries the read-only
    /// attribute (item 33): the title and status bar say so, and
    /// Save offers a copy instead of writing.
    doc_read_only_file: bool,
    /// The Encoding menu and its Code Pages submenu, kept for the
    /// check-mark sync (item 29).
    encoding_menu: HMENU,
    codepage_menu: HMENU,
    // --- Tabs (roadmap item 39, slice R5b) -----------------------
    /// The live document's identity (its parked form is
    /// `DocState::id`).
    doc_id: u64,
    /// The live document's armed save-options choices; the parked
    /// form is `DocState::save_eol` / `save_bom`.
    save_eol: crate::save::EolChoice,
    save_bom: crate::save::BomChoice,
    /// Conceptual tab order (count + active index); the pure
    /// bookkeeping lives in `crate::tabs`.
    tabs: crate::tabs::TabOrder,
    /// The inactive documents' states, in conceptual order with
    /// the active document left out (the `crate::tabs`
    /// invariant): `parked.len() == tabs.count() - 1` always.
    parked: Vec<DocState>,
    /// Source of `DocState` identities.
    next_doc_id: u64,
    /// The tab strip's horizontal scroll offset in DIPs (only
    /// nonzero while the tabs overflow the window width).
    tab_scroll_x: f32,
    /// The tab under the pointer, for the close × and the hover
    /// fill; recomputed on every mouse move over the strip.
    tab_hover: Option<usize>,
    /// The pointer is over the strip's "+" button.
    tab_plus_hover: bool,
    /// A tab drag in progress (reorder by insertion point).
    tab_drag: Option<TabDrag>,
    /// The tab the strip's context menu acts on (set just before
    /// the menu opens).
    tab_context: Option<usize>,
    /// While session restore runs, `open_document` must not
    /// delete the crash-recovery files: an accepted recovery is
    /// sitting in the first tab, and its record is only replaced
    /// by the next edit's debounce, as before.
    suppress_recovery_delete: bool,
    // -- Split view and side-by-side diff (slice R5c) ----------
    /// The open compare session, if any. Window-owned: never
    /// parked with a tab; switching tabs closes it.
    compare: Option<CompareState>,
    /// The pointer is over the split/compare divider (its band
    /// paints in the accent while true).
    split_div_hover: bool,
    /// A divider drag in progress: the grab offset (pointer x
    /// minus divider x) so the divider tracks the pointer
    /// without jumping.
    divider_drag: Option<f32>,
    // -- Slice R5d ------------------------------------------------
    /// Binary templates (roadmap item 17): the results pane's
    /// state — the last applied struct, its header label, and the
    /// list's scroll/selection. Window-owned like the compare:
    /// switching tabs or opening a file clears it, since the
    /// offsets name the document it was applied to.
    tpl_open: bool,
    tpl_result: Option<crate::templates::AppliedStruct>,
    tpl_title: String,
    tpl_first: usize,
    tpl_selected: Option<usize>,
    /// The pane header's close box from the last render (DIPs).
    tpl_close_rect: (f32, f32, f32, f32),
    /// The external tools (roadmap item 49): the set loaded from
    /// `data/tools.json`, its menu, a load problem to show in the
    /// menu when the file exists but does not parse, and the
    /// in-flight run the tick services.
    tools: crate::exttools::ToolSet,
    tools_menu: HMENU,
    tools_error: Option<String>,
    tool_run: Option<ToolRun>,
    tool_gen: u64,
    /// The disassembly panel (hex mode): on/off, the base offset
    /// the held rows were decoded from, the rows themselves, and
    /// the stale flag an edit sets (the next paint recomputes).
    disasm_on: bool,
    disasm_base: u64,
    disasm_rows: Vec<DisasmRow>,
    disasm_stale: bool,
    /// Extra carets (multi-cursor editing): byte offsets, sorted,
    /// never containing the editor's own caret. Transient: a tab
    /// switch, a file open, a plain click, Esc, or any navigation
    /// collapses back to the one editor caret.
    extra_carets: Vec<u64>,
    /// The rectangular selection (byte columns over a line range),
    /// while one is up.
    rect_sel: Option<RectSel>,
    /// A rectangular drag in progress: the anchor corner and the
    /// current corner, in (line, byte column) pairs.
    rect_drag: Option<((u64, u64), (u64, u64))>,
    /// An Alt+press in the text area that has not yet become a
    /// drag: (line, byte column, offset). On release without a
    /// drag it toggles an extra caret; once the pointer moves to
    /// another line or column it becomes `rect_drag`.
    rect_press: Option<(u64, u64, u64)>,
    /// The screen-reader provider object (roadmap item 47),
    /// created lazily on the first WM_GETOBJECT and disconnected
    /// at destroy. A raw COM pointer owned by the window.
    uia_provider: *mut c_void,
}

/// One decoded instruction row in the disassembly panel.
struct DisasmRow {
    offset: u64,
    /// The instruction's full length in bytes.
    len: u64,
    /// Up to 8 bytes, hex, space-separated.
    bytes: String,
    text: String,
}

/// A rectangular selection: byte columns `[col_start, col_end)`
/// over lines `first_line..=last_line`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RectSel {
    first_line: u64,
    last_line: u64,
    col_start: u64,
    col_end: u64,
}

/// An in-flight external-tool run: the worker thread owns the
/// child process and delivers its outcome through the shared
/// slot; the tick applies it. Only one run at a time.
struct ToolRun {
    name: String,
    gen: u64,
    /// The document generation at launch. If the document changed
    /// while the tool ran, output that would edit it is shown
    /// instead of applied — the range it was computed against no
    /// longer names the same bytes.
    doc_gen: u64,
    /// The range a replace-selection applies to (the selection at
    /// launch, or the caret point), also the insert point.
    apply_range: (u64, u64),
    shared: Arc<ToolShared>,
}

/// What the UI thread and a tool worker share.
struct ToolShared {
    outcome: Mutex<Option<ToolOutcome>>,
    /// The child process handle (as a usize) while it runs, so
    /// Esc can stop it. The worker clears this before closing the
    /// handle, so a cancel never touches a dead handle.
    process: Mutex<Option<usize>>,
    cancelled: std::sync::atomic::AtomicBool,
}

/// A finished tool run's outcome, carried from the worker to the
/// tick: the captured stdout on success, or the plain failure
/// text (spawn error, exit code with stderr, timeout, oversize).
struct ToolOutcome {
    name: String,
    mode: crate::exttools::ToolOutput,
    result: Result<Vec<u8>, String>,
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
        self.status_note = None;
        self.tail_pin_view = false;
        // The strings pane and the entropy strip describe the
        // document's bytes; an edit makes their held results stale
        // (their workers are epoch-guarded on these generations).
        self.strings.gen += 1;
        self.entropy.gen += 1;
        // A filter view describes the pre-edit document; editing is
        // refused while it is up, and this drop is the belt to that
        // braces for programmatic edit paths.
        self.filter = None;
        // An edit invalidates a background find exactly like the
        // editor's own match list (which the editor clears itself):
        // drop the mirrored matches — and their scrollbar ticks —
        // and bump the generation so the worker's late results are
        // never picked up.
        cancel_bg_find(self);
        // An open compare's computed differences describe the
        // pre-edit document: mark them stale (the bands stay,
        // honestly labelled) and retire any in-flight run. The
        // recompute is offered, never automatic per keystroke.
        if let Some(cmp) = &mut self.compare {
            cmp.diff_stale = true;
            cmp.diff_gen += 1;
            cmp.diff_computing = false;
        }
        // Slice R5d: the template results name byte offsets in
        // the pre-edit document — close the pane rather than let
        // stale offsets invite a wrong jump. The disassembly
        // panel just goes stale; its next paint recomputes.
        self.tpl_open = false;
        self.tpl_result = None;
        self.tpl_selected = None;
        self.disasm_stale = true;
    }

    /// Exchange every per-document field with `other` (tabs,
    /// roadmap item 39): the parking primitive. After the call the
    /// live fields hold `other`'s document and `other` holds the
    /// one that was live. Window-global state — settings, theme,
    /// the child-window handles, the menus, the render targets —
    /// is never swapped; the field list below is exactly the
    /// `DocState` list, and the two must be kept in step.
    fn swap_state(&mut self, other: &mut DocState) {
        std::mem::swap(&mut self.doc_id, &mut other.id);
        std::mem::swap(&mut self.editor, &mut other.editor);
        std::mem::swap(&mut self.find_in_selection, &mut other.find_in_selection);
        std::mem::swap(&mut self.find_regex, &mut other.find_regex);
        std::mem::swap(&mut self.find_scope_range, &mut other.find_scope_range);
        std::mem::swap(&mut self.last_scope_range, &mut other.last_scope_range);
        std::mem::swap(&mut self.last_find_kind, &mut other.last_find_kind);
        std::mem::swap(&mut self.doc_mmap, &mut other.doc_mmap);
        std::mem::swap(&mut self.find_slot, &mut other.find_slot);
        std::mem::swap(&mut self.find_gen, &mut other.find_gen);
        std::mem::swap(&mut self.bg_active, &mut other.bg_active);
        std::mem::swap(&mut self.bg_matches, &mut other.bg_matches);
        std::mem::swap(&mut self.bg_done, &mut other.bg_done);
        std::mem::swap(&mut self.bg_current, &mut other.bg_current);
        std::mem::swap(&mut self.bg_needle_len, &mut other.bg_needle_len);
        std::mem::swap(&mut self.bg_ends, &mut other.bg_ends);
        std::mem::swap(&mut self.find_sync_once, &mut other.find_sync_once);
        std::mem::swap(&mut self.bookmarks, &mut other.bookmarks);
        std::mem::swap(&mut self.doc_stamp, &mut other.doc_stamp);
        std::mem::swap(&mut self.file_stamp, &mut other.file_stamp);
        std::mem::swap(&mut self.change_visible, &mut other.change_visible);
        std::mem::swap(&mut self.change_shown_for, &mut other.change_shown_for);
        std::mem::swap(&mut self.change_ignored_for, &mut other.change_ignored_for);
        std::mem::swap(&mut self.fingerprint_slot, &mut other.fingerprint_slot);
        std::mem::swap(&mut self.doc_epoch, &mut other.doc_epoch);
        std::mem::swap(&mut self.doc_fingerprint, &mut other.doc_fingerprint);
        std::mem::swap(&mut self.eol, &mut other.eol);
        std::mem::swap(&mut self.rel_offsets, &mut other.rel_offsets);
        std::mem::swap(&mut self.rel_base, &mut other.rel_base);
        std::mem::swap(&mut self.find_visible, &mut other.find_visible);
        std::mem::swap(
            &mut self.pending_high_surrogate,
            &mut other.pending_high_surrogate,
        );
        std::mem::swap(&mut self.hex_nibble, &mut other.hex_nibble);
        std::mem::swap(&mut self.index_slot, &mut other.index_slot);
        std::mem::swap(&mut self.indexing, &mut other.indexing);
        std::mem::swap(&mut self.doc_generation, &mut other.doc_generation);
        std::mem::swap(&mut self.open_generation, &mut other.open_generation);
        std::mem::swap(&mut self.last_edit, &mut other.last_edit);
        std::mem::swap(&mut self.last_find_text, &mut other.last_find_text);
        std::mem::swap(&mut self.pending_goto, &mut other.pending_goto);
        std::mem::swap(&mut self.index_epoch, &mut other.index_epoch);
        std::mem::swap(&mut self.status_note, &mut other.status_note);
        std::mem::swap(&mut self.tail_pin_view, &mut other.tail_pin_view);
        std::mem::swap(&mut self.filter, &mut other.filter);
        std::mem::swap(&mut self.strings, &mut other.strings);
        std::mem::swap(&mut self.entropy, &mut other.entropy);
        std::mem::swap(&mut self.entropy_on, &mut other.entropy_on);
        std::mem::swap(&mut self.grid_on, &mut other.grid_on);
        std::mem::swap(&mut self.deltas_on, &mut other.deltas_on);
        std::mem::swap(&mut self.dim_on, &mut other.dim_on);
        std::mem::swap(&mut self.ansi_on, &mut other.ansi_on);
        std::mem::swap(&mut self.ctl_on, &mut other.ctl_on);
        std::mem::swap(&mut self.jump_history, &mut other.jump_history);
        std::mem::swap(&mut self.filter_slot, &mut other.filter_slot);
        std::mem::swap(&mut self.filter_gen, &mut other.filter_gen);
        std::mem::swap(&mut self.doc_read_only_file, &mut other.doc_read_only_file);
        std::mem::swap(&mut self.save_eol, &mut other.save_eol);
        std::mem::swap(&mut self.save_bom, &mut other.save_bom);
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
    app.brushes = Some(Brushes::build(target, &app.theme));
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

#[derive(Clone, Copy)]
struct Layout {
    editor_top: f32,
    editor_bottom: f32,
    width: f32,
    gutter_w: f32,
    /// X of the content origin (slice R5c): 0 for the main
    /// canvas; the hex pane of the split view starts its gutter
    /// at `split_div + DIVIDER_W`, and its paint/hit-test copy of
    /// the layout carries that here.
    origin_x: f32,
    /// The TEXT gutter's width (slice R5c), computed on every
    /// layout whatever the mode: the split view's left pane
    /// needs it while the editor's mode (the focus) is hex, and
    /// `gutter_w` above follows the mode.
    text_gutter: f32,
    /// The split view's divider x while the split is active
    /// (slice R5c); `None` when it is not.
    split_div: Option<f32>,
    /// The compare's divider x while a compare is open
    /// (slice R5c); the left document's `editor_right` already
    /// ends here in that case.
    compare_div: Option<f32>,
    /// Right edge of the editor area: the client width minus the
    /// inspector panel while it is open (roadmap item 15). Mouse
    /// maths, the scrollbar, and the content columns all bound to
    /// this; the chrome (accent strip, row washes, status bar) uses
    /// the full `width`.
    editor_right: f32,
    /// Top of the first text/hex row: below the entropy strip and
    /// the filter banner when either is showing (R4b).
    rows_top: f32,
    /// Bottom of the rows region: above the strings pane when it is
    /// docked open (R4b).
    rows_bottom: f32,
    /// Width of the time-delta gutter column inside the gutter
    /// (0 while the time-delta gutter is off or in hex mode).
    delta_w: f32,
    /// Height of the strings pane (0 while closed).
    strings_h: f32,
    /// Height of the template results pane (0 while closed,
    /// slice R5d). The template pane stacks ABOVE the strings
    /// pane: rows end at `rows_bottom`, the template pane fills
    /// `[rows_bottom, rows_bottom + tpl_h)`, and the strings pane
    /// fills the rest down to the status bar.
    tpl_h: f32,
}

/// The canvas top: the full chrome stack the rows must clear —
/// accent strip, tab bar (roadmap item 39), toolbar, then the
/// change bar and find bar while they show. This MUST stay the
/// same stack `layout_children` positions the child windows
/// against (they hang from STRIP_H + TABBAR_H): when the tab
/// bar landed in R5b the canvas top was left one bar short, so
/// the first rows painted underneath the toolbar buttons until
/// a field screenshot caught it (2026-11-01). The test below
/// pins the sum.
fn chrome_stack_top(change_visible: bool, find_visible: bool) -> f32 {
    STRIP_H
        + TABBAR_H
        + TOOLBAR_H
        + if change_visible { CHANGEBAR_H } else { 0.0 }
        + if find_visible { FINDBAR_H } else { 0.0 }
}

fn compute_layout(app: &mut App) -> Layout {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let width = app.dip(client.right - client.left);
    let height = app.dip(client.bottom - client.top);
    let top = chrome_stack_top(app.change_visible, app.find_visible);
    // Time-delta gutter (R4b): an extra column inside the text
    // gutter, wide enough for "+999.9s"-shaped deltas. In the
    // split view the text pane carries it whatever the focus.
    let delta_w = if app.deltas_on && (app.editor.mode() == EditorMode::Text || split_active(app)) {
        DELTA_GUTTER_CHARS * app.char_w
    } else {
        0.0
    };
    // The text gutter's width at this layout (slice R5c).
    let text_digits = app.editor.line_count().max(1).to_string().len() as f32;
    let text_gutter = (text_digits + 2.0) * app.char_w + TEXT_PAD + delta_w;
    let gutter_w = match app.editor.mode() {
        EditorMode::Text => {
            let digits = app.editor.line_count().max(1).to_string().len() as f32;
            (digits + 2.0) * app.char_w + TEXT_PAD + delta_w
        }
        EditorMode::Hex => 18.0 * app.char_w + TEXT_PAD,
    };
    // The compare (slice R5c) takes the right half of the
    // canvas for its pane: the document's area ends at the
    // compare divider, and the inspector steps aside for the
    // duration (its open state is preserved, just not painted).
    let compare_div = if app.compare.is_some() {
        Some(crate::panes::divider_x(
            0.0,
            width,
            app.settings.split_ratio_permille as f32 / 1000.0,
            MIN_PANE_W,
            MIN_PANE_W,
        ))
    } else {
        None
    };
    let editor_right = if let Some(div) = compare_div {
        div
    } else {
        // The disassembly panel docks on the right in hex mode;
        // while it shows, the Data Inspector steps aside (its
        // open state is preserved), the same rule the compare
        // pane uses. A compare owns the right side outright,
        // above.
        let mut right = width;
        if disasm_showing(app) {
            right = (right - DISASM_W).max(gutter_w + TEXT_PAD + SCROLLBAR_W + 40.0);
        } else if app.settings.inspector_open {
            right = (right - INSPECTOR_W).max(gutter_w + TEXT_PAD + SCROLLBAR_W + 40.0);
        }
        right
    };
    // The split view's divider spans the document's own area.
    let split_div = if split_active(app) {
        Some(crate::panes::divider_x(
            0.0,
            editor_right,
            app.settings.split_ratio_permille as f32 / 1000.0,
            MIN_PANE_W,
            MIN_PANE_W,
        ))
    } else {
        None
    };
    let bottom = (height - STATUS_H).max(top);
    let strings_h = if app.settings.strings_open {
        STRINGS_HEADER_H + STRINGS_ROWS as f32 * app.line_h
    } else {
        0.0
    };
    // The template results pane (slice R5d) stacks above the
    // strings pane.
    let tpl_h = if app.tpl_open && app.tpl_result.is_some() {
        TPL_HEADER_H + TPL_ROWS as f32 * app.line_h
    } else {
        0.0
    };
    // The entropy strip and the filter banner sit between the
    // chrome bars and the first row.
    let rows_top = top
        + if app.entropy_on { ENTROPY_H } else { 0.0 }
        + if app.filter.is_some() {
            app.line_h
        } else {
            0.0
        };
    Layout {
        editor_top: top,
        editor_bottom: bottom,
        width,
        gutter_w,
        origin_x: 0.0,
        text_gutter,
        split_div,
        compare_div,
        editor_right,
        rows_top,
        rows_bottom: (bottom - strings_h - tpl_h).max(rows_top),
        delta_w,
        strings_h,
        tpl_h,
    }
}

/// Whether the disassembly panel is actually painted (slice
/// R5d): toggled on, hex mode (it decodes bytes; text mode has
/// no use for it), and no compare open (a compare owns the
/// right side, as it does over the inspector).
fn disasm_showing(app: &App) -> bool {
    app.disasm_on && app.editor.mode() == EditorMode::Hex && app.compare.is_none()
}

/// Whether the text/hex split view is actually in effect
/// (roadmap item 28, slice R5c): the setting is on, no filter
/// view is up (the two decline each other — see the commands),
/// no compare is open (a compare owns the right side), and no
/// lines are frozen (the frozen block belongs to the single
/// canvas; freezing and splitting decline each other too).
fn split_active(app: &App) -> bool {
    app.settings.split_view
        && app.filter.is_none()
        && app.compare.is_none()
        && app.settings.freeze_lines == 0
}

/// Re-tie the split's unfocused pane to the focused pane's top
/// through the shared byte-offset anchor: the offset at the top
/// of the focused pane maps to the line (text) or row (hex) at
/// the top of the other. Called from `render` — the one funnel
/// every scroll path passes through before painting — so the
/// stored tops are never more than one paint apart.
fn split_sync_from_focus(app: &mut App) {
    if !split_active(app) {
        return;
    }
    let width = app.editor.hex_layout().bytes_per_row.max(1);
    match app.editor.mode() {
        EditorMode::Text => {
            let anchor = app
                .editor
                .line_start_byte(app.editor.first_line())
                .unwrap_or(0);
            let row = crate::panes::hex_row_for_offset(anchor, width);
            app.editor.set_first_row(row);
        }
        EditorMode::Hex => {
            let anchor = crate::panes::offset_for_hex_row(app.editor.first_row(), width);
            let (line, _) = app.editor.line_col_of(anchor);
            app.editor.set_first_line(line);
        }
    }
}

/// Move keyboard focus to one pane of the split (slice R5c):
/// the mode becomes the pane's representation — the mode IS the
/// focus while split — with both panes' tops preserved across
/// the switch and the anchor re-tied. Reached by clicking a
/// pane and by F6's toggle; the Text/Hex commands themselves
/// leave the split instead (see `mode_command`). Outside the
/// split this is exactly `set_mode`.
/// A compare opened in hex mode has no differences yet
/// (phase 2 is text-only): start its diff when text mode
/// arrives (slice R5c).
fn maybe_start_compare_diff(app: &mut App) {
    if app.editor.mode() != EditorMode::Text {
        return;
    }
    let needs = matches!(&app.compare, Some(cmp)
        if cmp.diff.is_none() && !cmp.diff_computing && !cmp.diff_stale);
    if needs {
        start_diff(app);
        refresh_status(app);
    }
}

fn split_set_focus(app: &mut App, hex: bool) {
    let target = if hex {
        EditorMode::Hex
    } else {
        EditorMode::Text
    };
    if !split_active(app) {
        set_mode(app, target);
        maybe_start_compare_diff(app);
        return;
    }
    if app.editor.mode() == target {
        return;
    }
    let first_line = app.editor.first_line();
    let first_row = app.editor.first_row();
    set_mode(app, target);
    // set_mode dropped the filter (never up while split) and
    // re-derived the incoming top from the caret; put both
    // tops back, then let the anchor tie them together.
    app.editor.set_first_line(first_line);
    app.editor.set_first_row(first_row);
    split_sync_from_focus(app);
    invalidate(app);
    maybe_start_compare_diff(app);
}

/// The Text/Hex commands (toolbar pair, View menu): they name a
/// single view. With no split this is a plain mode switch.
/// While split, the command LEAVES the split for the named
/// view, opening exactly where that pane was — the field found
/// no way back to one view from these commands when they only
/// moved the focus. (Focus inside the split still moves by
/// clicking a pane or pressing F6.)
fn mode_command(app: &mut App, target: EditorMode) {
    if !split_active(app) {
        set_mode(app, target);
        maybe_start_compare_diff(app);
        return;
    }
    let first_line = app.editor.first_line();
    let first_row = app.editor.first_row();
    app.settings.split_view = false;
    save_settings(app);
    set_mode(app, target);
    // set_mode translated the focused pane's top into the
    // target's scroll unit; put both stored tops back so the
    // single view opens exactly where its own pane was.
    app.editor.set_first_line(first_line);
    app.editor.set_first_row(first_row);
    refresh_menu_checks(app);
    sync_hex_layout(app);
    refresh_status(app);
    invalidate(app);
    maybe_start_compare_diff(app);
}

/// The fit-to-window hex width for the current window (roadmap
/// item 13): the character cells available to the hex area,
/// counted in cells (the monospace grid makes this exact).
fn fit_hex_width(app: &mut App) -> u64 {
    let layout = compute_layout(app);
    // While the split is active the hex pane is what must fit:
    // its content runs from the divider (past its own gutter)
    // to the editor's right edge.
    let content_left = match layout.split_div {
        Some(div) => div + DIVIDER_W + hex_gutter_w(app) + TEXT_PAD,
        None => layout.gutter_w + TEXT_PAD,
    };
    let avail = ((layout.editor_right - SCROLLBAR_W - content_left) / app.char_w.max(1.0))
        .floor()
        .max(0.0) as usize;
    hexview::fit_bytes_per_row(avail, app.settings.hex_group as usize)
}

/// The hex gutter's width at the current font: the address
/// column ("0000000000000000: " worth of cells) plus padding.
/// `compute_layout` uses the same formula for the hex mode's
/// gutter; the split view needs it for the right pane while the
/// layout's own gutter describes the text pane.
fn hex_gutter_w(app: &App) -> f32 {
    18.0 * app.char_w + TEXT_PAD
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
    // The compare's editor mirrors the document's layout
    // (slice R5c): identical bytes-per-row keeps hex rows 1:1
    // between the panes, which the sync-scroll pairing assumes.
    if let Some(cmp) = &mut app.compare {
        if cmp.editor.hex_layout() != layout {
            cmp.editor.set_hex_layout(layout);
        }
    }
}

fn page_rows(app: &mut App) -> u64 {
    let layout = compute_layout(app);
    let rows = ((layout.rows_bottom - layout.rows_top) / app.line_h.max(1.0)).floor();
    (rows as u64).max(1)
}

/// Width of the time-delta gutter column, in character cells.
const DELTA_GUTTER_CHARS: f32 = 10.0;
/// Height of the entropy strip (DIPs).
const ENTROPY_H: f32 = 6.0;
/// Strings pane: header height (DIPs) and result rows shown.
const STRINGS_HEADER_H: f32 = 22.0;
const STRINGS_ROWS: u64 = 7;
/// Template results pane geometry (slice R5d): the strings
/// pane's shape, one dock up.
const TPL_HEADER_H: f32 = 22.0;
const TPL_ROWS: u64 = 6;
/// Disassembly panel width in DIPs (slice R5d).
const DISASM_W: f32 = 350.0;
/// Instructions decoded into the disassembly panel per fill.
const DISASM_COUNT: usize = 48;

/// One painted row of the text view (R4b): a whole line, or one
/// wrapped segment of a line, or one filtered line. All text-mode
/// painting and text-mode pointer maths run over these rows, so
/// word wrap, the freeze pane and the filter view share one exact
/// model instead of three near-copies.
struct DisplayRow {
    /// The document line this row shows (0-based).
    line_no: u64,
    /// Byte offset where the line's content starts.
    line_start: u64,
    /// Byte range of THIS segment within the line's content.
    start: u64,
    end: u64,
    /// Character column where this segment starts in the line
    /// (0 for whole-line rows and continuation anchor).
    char_start: u64,
    /// The segment's display text (already truncated like the
    /// editor's own rows).
    text: String,
    /// Part of the frozen block (painted from the top lines).
    frozen: bool,
}

/// The character ranges a wrapped line of `char_count` characters
/// breaks into at `cols` characters per row. An empty line still
/// gets one (empty) row; `cols` is clamped to at least 1.
fn wrap_char_ranges(char_count: u64, cols: u64) -> Vec<(u64, u64)> {
    let cols = cols.max(1);
    if char_count == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < char_count {
        let end = (at + cols).min(char_count);
        out.push((at, end));
        at = end;
    }
    out
}

/// The characters of `text` in the char range `from..to`, as a new
/// string (char-boundary safe by construction).
fn char_slice(text: &str, from: u64, to: u64) -> String {
    text.chars()
        .skip(from as usize)
        .take((to - from) as usize)
        .collect()
}

/// The display text for one line's content range, with exactly the
/// editor's row truncation rules (fetch cap, character cap,
/// truncation marker). Used for rows the editor's batch fetcher
/// cannot address (filtered lines).
fn line_row_text(ed: &Editor, start: u64, end: u64) -> String {
    let fetch_len = (end - start).min(256 * 1024);
    let bytes = ed.table().read(start, fetch_len);
    let mut text = crate::editor::decode_lossy_codec(&bytes, ed.codec());
    let truncated = fetch_len < end - start || text.chars().count() > crate::editor::MAX_ROW_CHARS;
    if text.chars().count() > crate::editor::MAX_ROW_CHARS {
        text = text.chars().take(crate::editor::MAX_ROW_CHARS).collect();
    }
    if truncated {
        text.push_str(crate::editor::TRUNCATION_MARKER);
    }
    text
}

/// Whether word wrap is in effect for the text view right now:
/// the setting is on, the view is the plain text view (never the
/// filter view and never the delimiter grid, which has its own
/// column model).
fn wrap_active(app: &App) -> bool {
    app.settings.word_wrap && app.filter.is_none() && !grid_active(app)
}

/// Whether the delimiter grid is in effect: toggled on, text mode,
/// no filter up. (Detection itself happens per paint over the
/// visible lines; a document with no consistent delimiter simply
/// paints as plain text.)
fn grid_active(app: &App) -> bool {
    // The grid is a single-canvas reading of the whole text
    // width; the split's text pane is narrower than the grid's
    // detection assumes, so the grid steps aside while split
    // (slice R5c — the toggle stays on and returns with the
    // split off; see PARITY).
    app.grid_on
        && app.filter.is_none()
        && app.editor.mode() == EditorMode::Text
        && !split_active(app)
}

/// Characters that fit across the text area at the current layout —
/// the wrap column count.
fn wrap_columns(app: &mut App) -> u64 {
    let layout = compute_layout(app);
    // In the split view the text pane ends at the divider.
    let right = layout.split_div.unwrap_or(layout.editor_right);
    let text_w = (right - SCROLLBAR_W - layout.gutter_w - TEXT_PAD).max(app.char_w);
    ((text_w / app.char_w.max(1.0)).floor() as u64).max(4)
}

/// Build the display rows for the text view, at most `max_rows`:
/// frozen block first (when freezing is on), then the scrolled
/// region. In the filter view, rows are the filtered lines from the
/// filter's scroll position.
fn display_rows(app: &mut App, max_rows: usize) -> Vec<DisplayRow> {
    if app.editor.mode() != EditorMode::Text {
        return Vec::new();
    }
    display_rows_model(app, max_rows)
}

/// The text display-row model without the mode guard: the split
/// view's text pane needs text rows while the editor's mode (the
/// split's focus) is hex (slice R5c). Everything else about the
/// rows — filter, wrap, freeze — is the ordinary model's.
fn display_rows_model(app: &mut App, max_rows: usize) -> Vec<DisplayRow> {
    let mut out: Vec<DisplayRow> = Vec::new();
    if max_rows == 0 {
        return out;
    }
    if let Some(filter) = &app.filter {
        let first = filter.first_row;
        let total = filter.view.len();
        for row_idx in first..total.min(first + max_rows) {
            let Some(line) = filter.view.line_at_row(row_idx) else {
                break;
            };
            let text = line_row_text(&app.editor, line.start, line.end);
            out.push(DisplayRow {
                line_no: line.line_no,
                line_start: line.start,
                start: line.start,
                end: line.end,
                char_start: 0,
                text,
                frozen: false,
            });
        }
        return out;
    }
    let wrap = wrap_active(app);
    let cols = if wrap { wrap_columns(app) } else { 0 };
    let freeze = app.settings.freeze_lines as u64;
    let push_line =
        |app: &mut App, out: &mut Vec<DisplayRow>, row: crate::editor::TextRow, frozen: bool| {
            if wrap {
                let total_chars = row.text.chars().count() as u64;
                for (cs, ce) in wrap_char_ranges(total_chars, cols) {
                    let start = app
                        .editor
                        .offset_of_line_col(row.line_no, cs)
                        .unwrap_or(row.start);
                    let end = app
                        .editor
                        .offset_of_line_col(row.line_no, ce)
                        .unwrap_or(row.end);
                    out.push(DisplayRow {
                        line_no: row.line_no,
                        line_start: row.start,
                        start,
                        end,
                        char_start: cs,
                        text: char_slice(&row.text, cs, ce),
                        frozen,
                    });
                    if out.len() >= max_rows {
                        return;
                    }
                }
            } else {
                out.push(DisplayRow {
                    line_no: row.line_no,
                    line_start: row.start,
                    start: row.start,
                    end: row.end,
                    char_start: 0,
                    text: row.text,
                    frozen,
                });
            }
        };
    if freeze > 0 {
        let frozen_rows = app.editor.visible_text_rows(0, freeze as usize);
        for row in frozen_rows {
            if out.len() >= max_rows {
                return out;
            }
            push_line(app, &mut out, row, true);
        }
    }
    let first_line = app.editor.first_line().max(freeze);
    let rest = app
        .editor
        .visible_text_rows(first_line, max_rows - out.len().min(max_rows));
    for row in rest {
        if out.len() >= max_rows {
            break;
        }
        push_line(app, &mut out, row, false);
    }
    out
}

/// One painted run of a text row when the segment painter is
/// active (ANSI colours and/or control-character tags): a run of
/// ordinary characters in one SGR state, or one control character's
/// glyph tag.
struct PaintSeg {
    /// What to draw (for a tag, the two-character caret notation).
    text: String,
    /// Character column (in the line) where this run starts.
    char_from: u64,
    /// Line characters this run covers (a tag covers 1).
    char_len: u64,
    /// Display width in character cells (a tag is 2 cells wide).
    cells: u64,
    /// ANSI palette index for the foreground, or `None` for the
    /// row's ordinary brush.
    fg: Option<usize>,
    /// This run is a control-character glyph tag.
    tag: bool,
}

/// Split one display row's text into painted runs (item 30
/// expansion). With ANSI on, CSI escape sequences are consumed —
/// SGR sequences update the colour state, other sequences are
/// simply not painted — and the text between them becomes runs.
/// With control tags on, a control character (other than tab)
/// becomes a tag run with its caret notation (`^A`, `^?` for DEL).
/// With both off the result is a single run, and callers skip this
/// entirely. Character columns are counted in decoded characters,
/// matching the editor's column model exactly.
fn build_segments(text: &str, char_start: u64, ansi_on: bool, ctl_on: bool) -> Vec<PaintSeg> {
    let chars: Vec<char> = text.chars().collect();
    let mut segs: Vec<PaintSeg> = Vec::new();
    let mut fg: Option<usize> = None;
    let mut bold = false;
    let mut run = String::new();
    let mut run_from = char_start;
    let mut col = char_start;
    let flush = |segs: &mut Vec<PaintSeg>, run: &mut String, from: u64, fg: Option<usize>| {
        if !run.is_empty() {
            let len = run.chars().count() as u64;
            segs.push(PaintSeg {
                text: std::mem::take(run),
                char_from: from,
                char_len: len,
                cells: len,
                fg,
                tag: false,
            });
        }
    };
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ansi_on && ch == '\u{1B}' && i + 1 < chars.len() && chars[i + 1] == '[' {
            // A CSI sequence: parameter/intermediate bytes, then a
            // final byte in 0x40..=0x7E.
            let mut j = i + 2;
            let mut params = String::new();
            while j < chars.len() && ('\u{20}'..='\u{3F}').contains(&chars[j]) {
                if ('\u{30}'..='\u{3F}').contains(&chars[j]) {
                    params.push(chars[j]);
                }
                j += 1;
            }
            if j < chars.len() && ('\u{40}'..='\u{7E}').contains(&chars[j]) {
                if chars[j] == 'm' {
                    flush(&mut segs, &mut run, run_from, fg);
                    for part in params.split(';') {
                        let code: u32 = if part.is_empty() {
                            0
                        } else {
                            part.parse().unwrap_or(u32::MAX)
                        };
                        match code {
                            0 => {
                                fg = None;
                                bold = false;
                            }
                            1 => bold = true,
                            22 => bold = false,
                            30..=37 => fg = Some((code - 30) as usize + if bold { 8 } else { 0 }),
                            39 => fg = None,
                            90..=97 => fg = Some((code - 90) as usize + 8),
                            _ => {}
                        }
                    }
                    run_from = col;
                }
                i = j + 1;
                continue;
            }
            // Unterminated sequence: fall through and treat the ESC
            // as an ordinary control character below.
        }
        if ctl_on && ((ch as u32) < 0x20 && ch != '\t' || ch == '\u{7F}') {
            flush(&mut segs, &mut run, run_from, fg);
            let notation = match ch {
                '\u{7F}' => "^?".to_string(),
                _ => format!("^{}", (b'@' + (ch as u8)) as char),
            };
            segs.push(PaintSeg {
                text: notation,
                char_from: col,
                char_len: 1,
                cells: 2,
                fg: None,
                tag: true,
            });
            col += 1;
            run_from = col;
            i += 1;
            continue;
        }
        run.push(ch);
        col += 1;
        i += 1;
    }
    flush(&mut segs, &mut run, run_from, fg);
    segs
}

/// The display cell (counted from the first segment's start) of a
/// line character column, under a segment list: ordinary
/// characters are one cell each, a tag is two cells for its one
/// character.
fn seg_cell_of(segs: &[PaintSeg], col: u64) -> u64 {
    let mut cells = 0u64;
    for seg in segs {
        if col <= seg.char_from {
            break;
        }
        if col >= seg.char_from + seg.char_len {
            cells += seg.cells;
        } else {
            cells += col - seg.char_from;
        }
    }
    cells
}

/// The line character column at display cell `cell` under a segment
/// list (the inverse of `seg_cell_of`, for pointer maths): a cell
/// inside a tag maps to the tag's character.
fn seg_col_of_cell(segs: &[PaintSeg], cell: u64) -> u64 {
    let mut cells = 0u64;
    let mut col = segs.first().map(|s| s.char_from).unwrap_or(0);
    for seg in segs {
        if cell < cells + seg.cells {
            if seg.tag {
                return seg.char_from;
            }
            return seg.char_from + (cell - cells).min(seg.char_len);
        }
        cells += seg.cells;
        col = seg.char_from + seg.char_len;
    }
    col
}

/// Format a log time delta for the delta gutter: always signed,
/// milliseconds under a second, seconds (two decimals, trimmed)
/// under a minute and a half, minutes beyond.
fn format_delta(ms: i64) -> String {
    let sign = if ms < 0 { "-" } else { "+" };
    let abs = ms.unsigned_abs();
    if abs < 1000 {
        format!("{sign}{abs}ms")
    } else if abs < 90_000 {
        let secs = abs as f64 / 1000.0;
        let text = format!("{secs:.2}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{sign}{text}s")
    } else {
        let mins = abs as f64 / 60_000.0;
        let text = format!("{mins:.1}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{sign}{text}m")
    }
}

/// Post-move fixup for word wrap: the editor's own visibility rule
/// is line-based, but with wrap a visible line's later segments can
/// still sit below the fold. Advance the first line until the
/// caret's segment is inside the display rows. A no-op whenever
/// wrap is off (the common case costs one flag test).
fn ensure_caret_segment_visible(app: &mut App) {
    if !wrap_active(app) || app.editor.mode() != EditorMode::Text {
        return;
    }
    // The editor's line-based pass (which always runs first) owns
    // the caret-above-the-view case; this fixup only ever scrolls
    // DOWN to a wrapped segment below the fold.
    let (caret_line_now, _) = app.editor.line_col_of(app.editor.caret());
    if caret_line_now < app.editor.first_line() {
        return;
    }
    let page = page_rows(app).max(1) as usize;
    for _ in 0..64 {
        let rows = display_rows(app, page + 1);
        let (caret_line, caret_col) = app.editor.line_col_of(app.editor.caret());
        let visible = rows.iter().any(|row| {
            row.line_no == caret_line
                && caret_col >= row.char_start
                && (caret_col < row.char_start + row.text.chars().count() as u64
                    || row.char_start + row.text.chars().count() as u64
                        == app.editor.line_char_count(caret_line))
        });
        if visible {
            return;
        }
        // The caret sits below the visible segments: scroll one
        // line further and re-check (bounded; each step is exact).
        let first = app.editor.first_line();
        app.editor.set_first_line(first + 1);
        if app.editor.first_line() == first {
            return;
        }
    }
}

/// Position every child window for the current client size / DPI /
/// find-bar visibility.
fn layout_children(app: &mut App) {
    // The tab strip's scroll offset is valid only for the current
    // width; keep it clamped whenever the chrome is laid out
    // (resize, tab switch, open/close).
    clamp_tab_scroll(app);
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let (cw, ch) = (client.right - client.left, client.bottom - client.top);
    let toolbar_h = app.px(TOOLBAR_H);
    let row_y = app.px(STRIP_H + TABBAR_H + 3.0);
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
        let y = app.px(STRIP_H) + app.px(TABBAR_H) + toolbar_h + app.px(3.0);
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
            + app.px(TABBAR_H)
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
        // The Regex toggle (slice R5d) follows the scope toggle.
        if app.find_controls.len() > 9 {
            let regex_btn = app.find_controls[9];
            place(app, regex_btn, &mut fx, 74.0);
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
    let armed = !save_armed_suffix(app.save_eol, app.save_bom).is_empty();
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
    if app.doc_read_only_file {
        // Protected file (slice R4c, item 33): the file itself is
        // marked read-only on disk.
        title.push_str(" (read-only)");
    }
    let wide = wstr(&title);
    // SAFETY: our window plus a NUL-terminated string.
    unsafe { SetWindowTextW(app.hwnd, wide.as_ptr()) };
}

fn refresh_status(app: &mut App) {
    let st = app.editor.status();
    // While the structural filter is up, the left segment describes
    // the filter, not the (frozen) document caret: the view is a
    // read-only line picker, and the status bar says so.
    if let Some(filter) = &app.filter {
        let mut left = format!(
            "Filter: {} \u{00B7} {} lines \u{00B7} row {} \u{00B7} read-only \u{00B7} Enter opens the line \u{00B7} Esc closes",
            filter.pattern,
            filter.view.len(),
            if filter.view.is_empty() {
                0
            } else {
                filter.cursor_row + 1
            },
        );
        if filter.building {
            // The background build is still walking the document
            // (slice R4c, item 13): the rows on screen are the
            // matches found so far, and the banner says how far
            // the walk has got.
            let total = filter.view.total_length().max(1);
            let pct = filter.scanned.saturating_mul(100) / total;
            left.push_str(&format!(" \u{00B7} Filtering\u{2026} {pct}%"));
        } else if !filter.view.is_complete() {
            left.push_str(" \u{00B7} indexing\u{2026} (indexed prefix only)");
        }
        let wide = wstr(&left);
        // SAFETY: our status control plus a NUL-terminated string.
        unsafe { SetWindowTextW(app.status, wide.as_ptr()) };
        refresh_status_right(app, &st);
        return;
    }
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
    if app.doc_read_only_file {
        // Protected file (slice R4c, item 33) — distinct wording
        // from the --read-only launch flag below: this one is the
        // file's own attribute.
        left.push_str(" \u{00B7} Read-only file");
    } else if st.read_only {
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
    if app.settings.follow_tail && app.editor.path().is_some() {
        // Follow Tail (Ghost Tail, item 22) is armed: say so
        // quietly whenever the document is file-backed.
        left.push_str(" \u{00B7} Following");
    }
    if let Some(note) = &app.status_note {
        left.push_str(&format!(" \u{00B7} {note}"));
    }
    if let Some(run) = &app.tool_run {
        // External tool running (item 49): the wait happens on a
        // worker; the status bar says which tool and Esc stops it.
        left.push_str(&format!(" \u{00B7} Running {}\u{2026}", run.name));
    }
    if !app.extra_carets.is_empty() {
        // Multi-caret (slice R5d): the caret count while more
        // than one caret is live.
        left.push_str(&format!(" \u{00B7} {} carets", app.extra_carets.len() + 1));
    }
    // Side-by-side compare (slice R5c): what is being compared,
    // and the state of the difference computation — running,
    // stale (with the way to recompute), or the region count
    // with the parked position.
    if let Some(cmp) = &app.compare {
        let name = cmp
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cmp.path.display().to_string());
        left.push_str(&format!(" \u{00B7} Comparing with {name}"));
        if cmp.diff_computing {
            left.push_str(" \u{00B7} Comparing\u{2026}");
        } else if cmp.diff_stale {
            left.push_str(
                " \u{00B7} Differences stale \u{2014} Search \u{25B8} Compare Differences recomputes",
            );
        } else if let Some(report) = &cmp.diff {
            if report.regions.is_empty() {
                left.push_str(" \u{00B7} No differences");
            } else {
                left.push_str(&format!(
                    " \u{00B7} {} difference{}",
                    report.regions.len(),
                    if report.regions.len() == 1 { "" } else { "s" }
                ));
                if let Some(cur) = cmp.current {
                    left.push_str(&format!(
                        " \u{00B7} {} of {}",
                        cur + 1,
                        report.regions.len()
                    ));
                }
            }
            if cmp.diff_prefix {
                left.push_str(" \u{00B7} indexing\u{2026} (indexed prefix so far)");
            }
        }
        if let Some((indexed, total)) = cmp.indexing {
            if let Some(suffix) = crate::line_index::indexing_status_suffix(indexed, total) {
                left.push_str(&suffix);
            }
        }
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
    refresh_status_right(app, &st);
}

/// The status bar's right segment: zoom readout, then the document
/// facts. Split out of `refresh_status` because the filter view
/// replaces the left segment but keeps these facts.
fn refresh_status_right(app: &App, st: &crate::editor::StatusInfo) {
    // The zoom readout (polish pass — 100% is the default font
    // size; this refresh runs on every font change, menu or
    // Ctrl+wheel alike, so the percentage is always live).
    // A user-chosen code page (slice R4c, item 29) replaces the
    // detected encoding's name here — "CP932" and friends — and
    // the detection-confidence suffix does not apply to it.
    let encoding_label = match st.page {
        Some(page) => page.label(),
        None => encoding_display(st.encoding),
    };
    let mut right = format!(
        "{}% \u{00B7} {} \u{00B7} {} \u{00B7} {}",
        zoom_percent(app.settings.font_size),
        encoding_label,
        app.eol,
        human_size(st.size)
    );
    if st.page.is_none() && !st.encoding_confident {
        right.push_str(" (detected, not certain)");
    }
    // Armed save conversions (roadmap items 31/32) are visible
    // here before anything is saved.
    right.push_str(save_armed_suffix(app.save_eol, app.save_bom));
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
    // The filter view scrolls its own row list (R4b).
    if let Some(filter) = &app.filter {
        return (
            filter.view.len() as u64,
            filter.first_row as u64,
            page_rows(app),
        );
    }
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

/// The brush a bookmark's gutter marker and scrollbar tick draw
/// in (slice R4c, item 5): its annotation swatch when it has one,
/// otherwise the theme accent — the colour bookmarks have always
/// used.
fn bookmark_brush(brushes: &Brushes, bookmark: &crate::bookmarks::Bookmark) -> *mut c_void {
    match bookmark.color {
        Some(i) if (i as usize) < brushes.swatches.len() => brushes.swatches[i as usize],
        _ => brushes.accent,
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

/// Create a single-line DirectWrite layout for `text` in the
/// app's current format (the caller releases it with
/// [`com_release`]). Null on any failure — measurement is an
/// enhancement, never a requirement.
fn create_line_layout(app: &App, text: &str) -> *mut c_void {
    if app.dwrite.is_null() || app.text_format.is_null() || text.is_empty() {
        return std::ptr::null_mut();
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut layout: *mut c_void = std::ptr::null_mut();
    // SAFETY: slot 18 is IDWriteFactory::CreateTextLayout per
    // dwrite.h; the string and format are live, the out-slot
    // writable. A generous max width keeps the text on one line.
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
            wide.as_ptr(),
            wide.len() as u32,
            app.text_format,
            1_000_000.0,
            app.line_h.max(1.0),
            &mut layout,
        )
    };
    if hr < 0 {
        return std::ptr::null_mut();
    }
    layout
}

/// The x (DIPs from the text origin) of the caret boundary before
/// character `pos` of the laid-out text, measured by DirectWrite
/// itself (IDWriteTextLayout::HitTestTextPosition). `None` when
/// the call fails.
fn layout_caret_x(layout: *mut c_void, pos: u32, text_len: u32) -> Option<f32> {
    if layout.is_null() {
        return None;
    }
    // The end boundary is the trailing edge of the last character.
    let (at, trailing) = if pos >= text_len && text_len > 0 {
        (text_len - 1, 1)
    } else {
        (pos, 0)
    };
    let mut px = 0.0f32;
    let mut py = 0.0f32;
    let mut metrics = DwHitTestMetrics::default();
    // SAFETY: slot 65 is IDWriteTextLayout::HitTestTextPosition per
    // dwrite.h; the out slots are writable locals.
    let hit: unsafe extern "system" fn(
        *mut c_void,
        u32,
        i32,
        *mut f32,
        *mut f32,
        *mut DwHitTestMetrics,
    ) -> i32 = unsafe { vfn(layout, SLOT_LAYOUT_HIT_TEST_TEXT_POSITION) };
    // SAFETY: as above.
    let hr = unsafe { hit(layout, at, trailing, &mut px, &mut py, &mut metrics) };
    if hr < 0 {
        return None;
    }
    Some(px)
}

/// The character position at x `x` (DIPs from the text origin) in
/// the laid-out text (IDWriteTextLayout::HitTestPoint): the
/// position of the glyph under the point, advanced past it when
/// the point falls on its trailing half.
fn layout_char_at(layout: *mut c_void, x: f32) -> Option<u32> {
    if layout.is_null() {
        return None;
    }
    let mut trailing = 0i32;
    let mut inside = 0i32;
    let mut metrics = DwHitTestMetrics::default();
    // SAFETY: slot 64 is IDWriteTextLayout::HitTestPoint per
    // dwrite.h; the out slots are writable locals.
    let hit: unsafe extern "system" fn(
        *mut c_void,
        f32,
        f32,
        *mut i32,
        *mut i32,
        *mut DwHitTestMetrics,
    ) -> i32 = unsafe { vfn(layout, SLOT_LAYOUT_HIT_TEST_POINT) };
    // SAFETY: as above.
    let hr = unsafe { hit(layout, x, 0.0, &mut trailing, &mut inside, &mut metrics) };
    if hr < 0 {
        return None;
    }
    Some(if trailing != 0 {
        metrics.text_position + metrics.length
    } else {
        metrics.text_position
    })
}

/// Per-row text measurement for the painters (roadmap item 25,
/// slice R5d). The canvas draws each row's text with DirectWrite,
/// which lays glyphs out by their real advances — for pure-ASCII
/// text in the app's monospace fonts that advance is exactly one
/// cell per character, so `column × char_w` is already exact and
/// stays the fast path. A row containing anything else (wide CJK
/// glyphs, combining marks, a proportional fallback font) is
/// measured through a layout of that row instead, created lazily
/// on first use and released with the helper. If layout creation
/// fails, the cell maths remains as the fallback.
struct RowMeasure<'a> {
    app: &'a App,
    text: &'a str,
    layout: *mut c_void,
    tried: bool,
}

impl<'a> RowMeasure<'a> {
    fn new(app: &'a App, text: &'a str) -> Self {
        RowMeasure {
            app,
            text,
            layout: std::ptr::null_mut(),
            tried: false,
        }
    }

    /// Whether this row needs real measurement at all.
    fn needs(&self) -> bool {
        !self.text.is_ascii()
    }

    fn layout(&mut self) -> *mut c_void {
        if !self.tried {
            self.tried = true;
            if self.needs() {
                self.layout = create_line_layout(self.app, self.text);
            }
        }
        self.layout
    }

    /// The x (DIPs from the text origin) of the boundary before
    /// the character at segment-relative position `pos`.
    fn x_of(&mut self, pos: u64) -> f32 {
        if pos == 0 {
            return 0.0;
        }
        let layout = self.layout();
        if !layout.is_null() {
            // HitTestTextPosition counts in UTF-16 code units, not
            // chars: translate through the string when they differ.
            let units = utf16_pos(self.text, pos);
            if let Some(x) = layout_caret_x(layout, units, utf16_len(self.text)) {
                return x;
            }
        }
        pos as f32 * self.app.char_w
    }
}

impl Drop for RowMeasure<'_> {
    fn drop(&mut self) {
        if !self.layout.is_null() {
            // SAFETY: created by create_line_layout above, owned
            // here, released exactly once.
            unsafe { com_release(self.layout) };
        }
    }
}

/// UTF-16 code-unit count of `text`.
fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// The char position of the boundary at UTF-16 code-unit
/// position `units` in `text` (clamped to the end; a position
/// inside a surrogate pair rounds up to the pair's end).
fn char_pos_of_utf16(text: &str, units: u32) -> u64 {
    let mut used = 0u32;
    for (i, c) in text.chars().enumerate() {
        if used >= units {
            return i as u64;
        }
        used += c.len_utf16() as u32;
    }
    text.chars().count() as u64
}

/// The UTF-16 code-unit position of the boundary before char
/// `char_pos` in `text` (clamped to the end).
fn utf16_pos(text: &str, char_pos: u64) -> u32 {
    let mut units = 0u32;
    for (i, c) in text.chars().enumerate() {
        if i as u64 >= char_pos {
            break;
        }
        units += c.len_utf16() as u32;
    }
    units
}

/// Character column of `byte` within a row starting at `row_start`,
/// decoding the prefix (rows are visited rarely and prefixes are short,
/// except on very long lines, where the row-fetch cap applies).
fn col_of_byte(
    table: &PieceTable,
    enc: crate::encoding::TextCodec,
    row_start: u64,
    byte: u64,
) -> f32 {
    if byte <= row_start {
        return 0.0;
    }
    let bytes = table.read(row_start, byte - row_start);
    crate::editor::decode_lossy_codec(&bytes, enc)
        .chars()
        .count() as f32
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

/// The facts one text row's painting needs beyond the row itself,
/// gathered once per paint by `render` and shared by the plain,
/// wrapped, filtered and grid row painters.
struct RowPaint<'a> {
    caret_line: u64,
    caret_col: u64,
    caret_line_chars: u64,
    selection: Option<(u64, u64)>,
    /// Every displayed match as a byte span (slice R5d): regex
    /// matches vary in length, so the painters work from spans,
    /// never from starts plus a shared length.
    match_spans: &'a [(u64, u64)],
    current_match: Option<(u64, u64)>,
    /// The document's bookmarks, annotations included: the gutter
    /// markers draw in each bookmark's colour (slice R4c, item 5).
    bookmarks: &'a [crate::bookmarks::Bookmark],
    deltas: &'a std::collections::HashMap<u64, i64>,
    dim_range: Option<(u64, u64)>,
    seg_mode: bool,
    in_filter: bool,
    /// The compare's difference regions over the LEFT document
    /// (slice R5c), while a compare report is held; rows inside
    /// a region paint the diff band. `diff_current` is the
    /// region the user last jumped to (painted hotter).
    diff: Option<&'a [crate::linediff::LineDiffRegion]>,
    diff_current: Option<usize>,
    /// Whether the live text caret paints in these rows (slice
    /// R5c): in the split view the text pane shows the caret
    /// only while it has focus; the other pane's position is a
    /// passive marker drawn by the split painter instead.
    caret_live: bool,
    /// Extra multi-carets (slice R5d): byte offsets; each paints
    /// as a thin accent bar on its row.
    extra_carets: &'a [u64],
    /// The byte-column rectangle (slice R5d), washed per line.
    rect: Option<RectSel>,
}

/// Whether a line's content is blank for the paragraph model
/// (spaces, tabs and carriage returns only; a line that cannot be
/// read counts as blank — a separator, never content).
fn line_is_blank(ed: &mut Editor, line: u64) -> bool {
    match ed.line_content_range(line) {
        Some((start, end)) => {
            let bytes = ed.table().read(start, (end - start).min(512));
            bytes.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r')
        }
        None => true,
    }
}

/// The caret's paragraph for focus dimming (R4b): the run of
/// non-blank lines containing `line`, bounded to 5000 lines each
/// way so a pathological unbroken file costs a bounded scan. A
/// caret on a blank line is its own paragraph.
fn paragraph_range(app: &mut App, line: u64) -> Option<(u64, u64)> {
    if line_is_blank(&mut app.editor, line) {
        return Some((line, line));
    }
    let mut lo = line;
    while lo > 0 && lo > line.saturating_sub(5000) && !line_is_blank(&mut app.editor, lo - 1) {
        lo -= 1;
    }
    let count = app.editor.line_count();
    let mut hi = line;
    while hi + 1 < count && hi < line + 5000 && !line_is_blank(&mut app.editor, hi + 1) {
        hi += 1;
    }
    Some((lo, hi))
}

/// The delimiter grid model over the visible display rows (R4b):
/// the detected delimiter and the widest field per column, in
/// characters (capped so one rogue field cannot push the rest of
/// the grid off-screen).
fn grid_model(
    app: &mut App,
    disp: &[DisplayRow],
) -> Option<(crate::delimited::Delimiter, Vec<u64>)> {
    let enc = app.editor.codec();
    let contents: Vec<Vec<u8>> = disp
        .iter()
        .filter(|r| r.char_start == 0)
        .map(|r| {
            app.editor
                .table()
                .read(r.line_start, (r.end - r.line_start).min(GRID_FETCH_BYTES))
        })
        .collect();
    let refs: Vec<&[u8]> = contents.iter().map(|c| c.as_slice()).collect();
    let delim = crate::delimited::detect_delimiter(&refs)?;
    let mut widths: Vec<u64> = Vec::new();
    for content in &contents {
        for (fi, (fs, fe)) in crate::delimited::split_line(content, delim)
            .into_iter()
            .enumerate()
        {
            let n = crate::editor::decode_lossy_codec(&content[fs as usize..fe as usize], enc)
                .chars()
                .count() as u64;
            if widths.len() <= fi {
                widths.resize(fi + 1, 0);
            }
            widths[fi] = widths[fi].max(n).min(GRID_COL_MAX_CHARS);
        }
    }
    Some((delim, widths))
}

/// Field byte fetches for the grid are capped like row fetches.
const GRID_FETCH_BYTES: u64 = 256 * 1024;
/// Gap between grid columns, in character cells.
const GRID_GAP_CHARS: u64 = 2;
/// Widest grid column, in characters.
const GRID_COL_MAX_CHARS: u64 = 120;

/// Time-delta gutter values for the visible lines (R4b): each
/// line's delta from the line before it, computed over the visible
/// range plus one line of look-behind (the first visible line's
/// delta is real too).
fn row_deltas_map(app: &mut App, disp: &[DisplayRow]) -> std::collections::HashMap<u64, i64> {
    let mut out = std::collections::HashMap::new();
    let mut lines: Vec<u64> = disp
        .iter()
        .filter(|r| r.char_start == 0)
        .map(|r| r.line_no)
        .collect();
    lines.dedup();
    let Some(&first) = lines.first() else {
        return out;
    };
    let Some(&last) = lines.last() else {
        return out;
    };
    let from = first.saturating_sub(1);
    let mut contents: Vec<Vec<u8>> = Vec::new();
    let mut present: Vec<u64> = Vec::new();
    for line in from..=last {
        // The timestamp sits at the line's start, so a prefix is
        // everything the parser needs.
        if let Some((start, end)) = app.editor.line_content_range(line) {
            contents.push(app.editor.table().read(start, (end - start).min(4096)));
            present.push(line);
        }
    }
    let refs: Vec<&[u8]> = contents.iter().map(|c| c.as_slice()).collect();
    let deltas = crate::timedeltas::line_time_deltas(refs.iter().copied());
    for (i, &line) in present.iter().enumerate() {
        if let Some(Some(delta)) = deltas.get(i) {
            out.insert(line, *delta);
        }
    }
    out
}

fn render(app: &mut App) {
    ensure_format(app);
    ensure_target(app);
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let layout = compute_layout(app);
    // Split/compare scroll ties (slice R5c) run before any rows
    // are gathered: the split re-ties the unfocused pane's top
    // to the focused pane's through the shared byte anchor, and
    // the compare re-ties the right pane's top to the left's
    // through their pairing delta.
    split_sync_from_focus(app);
    compare_sync_scroll(app);
    let visible =
        (((layout.rows_bottom - layout.rows_top) / app.line_h.max(1.0)).ceil() as usize).max(1);
    let selection = app.editor.selection_range();
    let current_match = display_current_match(app);
    // Every match start (all matches share the needle's byte length),
    // plus the numbers the custom scrollbar is drawn from. While a
    // background find owns the display these come from its mirror,
    // not the editor's own list (see display_matches).
    let all_matches: Vec<u64> = display_matches(app).to_vec();
    // The same matches as spans for the row painters (slice R5d).
    let all_spans: Vec<(u64, u64)> = display_spans(app);
    // Bookmark offsets (sorted) for the scrollbar ticks, and the
    // full entries for the gutter markers (which draw in each
    // bookmark's annotation colour, slice R4c item 5).
    let bookmark_offsets: Vec<u64> = app.bookmarks.iter().map(|b| b.offset).collect();
    let bookmark_entries: Vec<crate::bookmarks::Bookmark> = app.bookmarks.iter().cloned().collect();
    let (sb_total, sb_first, sb_page) = scroll_metrics(app);
    let (caret_line, caret_col) = app.editor.line_col_of(app.editor.caret());
    let caret = app.editor.caret();
    let caret_line_chars = app.editor.line_char_count(caret_line);
    let in_filter = app.filter.is_some();
    let seg_mode = app.ansi_on || app.ctl_on;

    // Gather rows first; the draw pass below uses no &mut editor calls.
    enum RowData {
        Text(Vec<DisplayRow>),
        Hex(Vec<crate::editor::HexRow>),
    }
    let rows = match app.editor.mode() {
        EditorMode::Text => RowData::Text(display_rows(app, visible)),
        EditorMode::Hex => {
            RowData::Hex(app.editor.visible_hex_rows(app.editor.first_row(), visible))
        }
    };
    // Split view (slice R5c): both representations gathered —
    // the focused pane's rows are `rows` above; these serve the
    // other pane (and the text extras, which follow the text
    // pane even while hex has focus).
    let split_rows: Option<(Vec<DisplayRow>, Vec<crate::editor::HexRow>)> =
        if layout.split_div.is_some() {
            Some((
                display_rows_model(app, visible),
                app.editor.visible_hex_rows(app.editor.first_row(), visible),
            ))
        } else {
            None
        };
    let cmp_mode = app.editor.mode();
    let cmp_rows: Option<CmpRows> = match (&mut app.compare, cmp_mode) {
        (Some(cmp), EditorMode::Text) => {
            let digits = cmp.editor.line_count().max(1).to_string().len() as f32;
            Some(CmpRows::Text {
                rows: cmp
                    .editor
                    .visible_text_rows(cmp.editor.first_line(), visible),
                gutter_w: (digits + 2.0) * app.char_w + TEXT_PAD,
            })
        }
        (Some(cmp), EditorMode::Hex) => Some(CmpRows::Hex(
            cmp.editor.visible_hex_rows(cmp.editor.first_row(), visible),
        )),
        _ => None,
    };
    // Delimiter grid model over the visible lines (R4b).
    let grid = match &rows {
        RowData::Text(disp) if grid_active(app) => grid_model(app, disp),
        _ => None,
    };
    // Time-delta gutter values per visible line (R4b). In the
    // split view the values follow the TEXT pane's rows, which
    // are the split gather while hex has focus.
    let row_deltas: std::collections::HashMap<u64, i64> = if app.deltas_on && !in_filter {
        match (&split_rows, &rows) {
            (Some((trows, _)), _) => row_deltas_map(app, trows),
            (None, RowData::Text(disp)) => row_deltas_map(app, disp),
            _ => std::collections::HashMap::new(),
        }
    } else {
        std::collections::HashMap::new()
    };
    // Focus dimming (R4b): the caret's paragraph.
    let dim_range: Option<(u64, u64)> =
        if app.dim_on && (matches!(rows, RowData::Text(_)) || split_rows.is_some()) && !in_filter {
            paragraph_range(app, caret_line)
        } else {
            None
        };
    // The compare's difference bands over the left document
    // (slice R5c), while a report is held.
    let diff_regions: Option<&[crate::linediff::LineDiffRegion]> = app
        .compare
        .as_ref()
        .and_then(|c| c.diff.as_ref())
        .map(|r| r.regions.as_slice());
    let diff_current = app.compare.as_ref().and_then(|c| c.current);
    let paint = RowPaint {
        caret_line,
        caret_col,
        caret_line_chars,
        selection,
        match_spans: &all_spans,
        current_match,
        bookmarks: &bookmark_entries,
        deltas: &row_deltas,
        dim_range,
        seg_mode,
        in_filter,
        diff: diff_regions,
        diff_current,
        caret_live: true,
        extra_carets: &app.extra_carets,
        rect: app.rect_sel,
    };

    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let format = app.text_format;
    let table = app.editor.table();
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
    let rtop = layout.rows_top;
    let rbottom = layout.rows_bottom;
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

    // The tab strip (roadmap item 39): a panel band holding one
    // tab per open document. The active tab is filled with the
    // editor background and outlined; a hovered tab takes the
    // halfway band colour; a filled accent square marks unsaved
    // changes and a hollow square a read-only file; the close ×
    // shows on the active and hovered tabs. When the tabs
    // overflow, they shrink to a minimum width and then the
    // strip scrolls (the wheel over the strip scrolls it).
    {
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: 0.0,
                top: STRIP_H,
                right: width,
                bottom: STRIP_H + TABBAR_H,
            },
            brushes.panel,
        );
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: 0.0,
                top: STRIP_H + TABBAR_H - 1.0,
                right: width,
                bottom: STRIP_H + TABBAR_H,
            },
            brushes.line,
        );
        let (geoms, plus) = tab_strip_layout(app);
        let tab_top = STRIP_H + 3.0;
        let tab_bottom = STRIP_H + TABBAR_H;
        let text_y = tab_top + ((tab_bottom - tab_top - app.line_h) / 2.0).max(0.0);
        let mid_y = (tab_top + tab_bottom) / 2.0;
        for geom in &geoms {
            if geom.x + geom.w < 0.0 || geom.x > width {
                continue;
            }
            let active = geom.index == app.tabs.active();
            let hovered = app.tab_hover == Some(geom.index);
            let rect = D2dRectF {
                left: geom.x,
                top: tab_top,
                right: geom.x + geom.w,
                bottom: tab_bottom,
            };
            if active {
                fill_rect(fill, target, rect, brushes.bg);
                outline_rect(draw_rect, target, rect, brushes.line);
            } else if hovered {
                fill_rect(fill, target, rect, brushes.band);
            }
            let name = tab_display_name(app, geom.index);
            let mut tx = geom.x + 8.0;
            if tab_is_dirty(app, geom.index) {
                fill_rect(
                    fill,
                    target,
                    D2dRectF {
                        left: tx,
                        top: mid_y - 3.0,
                        right: tx + 6.0,
                        bottom: mid_y + 3.0,
                    },
                    brushes.accent,
                );
                tx += 10.0;
            }
            if tab_is_read_only_file(app, geom.index) {
                outline_rect(
                    draw_rect,
                    target,
                    D2dRectF {
                        left: tx,
                        top: mid_y - 3.0,
                        right: tx + 6.0,
                        bottom: mid_y + 3.0,
                    },
                    brushes.muted,
                );
                tx += 10.0;
            }
            let close_x = geom.x + geom.w - TAB_CLOSE_W - 2.0;
            draw_text(
                draw,
                target,
                format,
                &name,
                D2dRectF {
                    left: tx,
                    top: text_y,
                    right: close_x - 2.0,
                    bottom: text_y + app.line_h,
                },
                if active || hovered {
                    brushes.text
                } else {
                    brushes.muted
                },
            );
            if active || hovered {
                draw_text(
                    draw,
                    target,
                    format,
                    "\u{2715}",
                    D2dRectF {
                        left: close_x,
                        top: text_y,
                        right: geom.x + geom.w - 2.0,
                        bottom: text_y + app.line_h,
                    },
                    brushes.muted,
                );
            }
        }
        // The "+" new-tab button at the end of the strip (pinned
        // to the right edge while the tabs overflow).
        if app.tab_plus_hover {
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: plus.0,
                    top: tab_top,
                    right: plus.1,
                    bottom: tab_bottom,
                },
                brushes.band,
            );
        }
        draw_text(
            draw,
            target,
            format,
            "+",
            D2dRectF {
                left: plus.0 + 8.0,
                top: text_y,
                right: plus.1,
                bottom: text_y + app.line_h,
            },
            if app.tab_plus_hover {
                brushes.text
            } else {
                brushes.muted
            },
        );
        // A drag in progress paints its insertion marker.
        if let Some(drag) = &app.tab_drag {
            if drag.moved {
                let (_, marker_x) = tab_drop_at(app, drag.index, drag.x);
                fill_rect(
                    fill,
                    target,
                    D2dRectF {
                        left: marker_x - 1.0,
                        top: tab_top,
                        right: marker_x + 1.0,
                        bottom: tab_bottom,
                    },
                    brushes.accent,
                );
            }
        }
    }
    // Gutter background for the rows strip. In the split view
    // the strip is the TEXT pane's gutter, whatever the focus
    // (the layout's own gutter follows the focused mode).
    let gutter_fill_w = if layout.split_div.is_some() {
        layout.text_gutter
    } else {
        layout.gutter_w
    };
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: 0.0,
            top,
            right: gutter_fill_w,
            bottom: rbottom,
        },
        brushes.gutter,
    );

    // The entropy strip (R4b): one colour step per block across the
    // whole file, panel colour (low) to accent (high). Adjacent
    // columns at the same ramp level merge into one fill.
    if app.entropy_on {
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: 0.0,
                top,
                right: layout.editor_right,
                bottom: top + ENTROPY_H,
            },
            brushes.panel,
        );
        if !app.entropy.values.is_empty() && layout.editor_right > 0.0 {
            let n = app.entropy.values.len();
            let w = layout.editor_right;
            let level_at = |x: f32| -> usize {
                let idx = ((x / w) * n as f32) as usize;
                let v = app.entropy.values[idx.min(n - 1)];
                ((v / 8.0) * 15.0).round().clamp(0.0, 15.0) as usize
            };
            let mut run_start = 0.0f32;
            let mut run_level = level_at(0.0);
            let mut x = 1.0f32;
            while x <= w {
                let level = if x >= w { usize::MAX } else { level_at(x) };
                if level != run_level {
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: run_start,
                            top,
                            right: x.min(w),
                            bottom: top + ENTROPY_H,
                        },
                        brushes.ramp[run_level],
                    );
                    run_start = x;
                    run_level = level;
                }
                x += 1.0;
            }
        }
    }

    // The filter banner (R4b): replaces the ruler row while the
    // filter view is up, naming the pattern and the row count.
    if let Some(filter) = &app.filter {
        let by = rtop - app.line_h;
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: 0.0,
                top: by,
                right: width,
                bottom: by + app.line_h,
            },
            brushes.panel,
        );
        let mut label = format!("Filter: {} ({} lines)", filter.pattern, filter.view.len());
        if !filter.view.is_complete() {
            label.push_str(&format!(
                " \u{00B7} still indexing \u{00B7} matches in the first {} so far",
                human_size(filter.view.indexed_bytes())
            ));
        }
        draw_text(
            draw,
            target,
            format,
            &label,
            D2dRectF {
                left: TEXT_PAD,
                top: by,
                right: width - TEXT_PAD,
                bottom: by + app.line_h,
            },
            brushes.text,
        );
    }

    match &rows {
        RowData::Text(disp) => {
            let frozen_count = disp.iter().take_while(|r| r.frozen).count();
            if let Some((delim, widths)) = &grid {
                paint_grid_rows(app, disp, *delim, widths, &layout, &paint);
            } else {
                for (i, row) in disp.iter().enumerate() {
                    let y = rtop + i as f32 * app.line_h;
                    if y >= rbottom {
                        break;
                    }
                    let banded = if in_filter {
                        app.filter
                            .as_ref()
                            .map(|f| f.first_row + i == f.cursor_row)
                            .unwrap_or(false)
                    } else {
                        row.line_no == caret_line
                    };
                    paint_text_row(app, row, y, &layout, banded, &paint);
                }
            }
            // Freeze divider (R4b): a rule under the pinned block.
            if frozen_count > 0 && !in_filter && grid.is_none() {
                let fy = rtop + frozen_count as f32 * app.line_h;
                if fy < rbottom {
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: 0.0,
                            top: fy,
                            right: layout.editor_right,
                            bottom: fy + 1.0,
                        },
                        brushes.line,
                    );
                }
            }
        }
        RowData::Hex(hex_rows) => {
            paint_hex_rows(
                app,
                hex_rows,
                0.0,
                layout.gutter_w,
                layout.editor_right,
                rtop,
                rbottom,
                &HexRowPaint {
                    selection,
                    bookmarks: &bookmark_entries,
                    caret,
                    mark: CaretMark::Active,
                },
            );
        }
    }

    // Split view and compare panes (slice R5c): painted over
    // the main pass — the split repaints both panes inside their
    // own bounds (the main pass painted the focused pane across
    // the full canvas), and the compare paints its right pane.
    if layout.split_div.is_some() {
        if let Some((trows, hrows)) = &split_rows {
            paint_split(app, trows, hrows, &layout, &paint);
        }
    }
    if layout.compare_div.is_some() {
        if let Some(crows) = &cmp_rows {
            paint_compare_pane(app, crows, &layout);
        }
    }

    // The column ruler (roadmap item 21): a quiet 1-DIP vertical
    // guideline at the configured character column, text mode only.
    // With wrap, the filter, or the grid up, character columns no
    // longer map to one fixed x, so the ruler steps aside. In the
    // split view `paint_split` draws it inside the text pane.
    if app.settings.ruler_column > 0
        && matches!(rows, RowData::Text(_))
        && !wrap_active(app)
        && !in_filter
        && grid.is_none()
        && layout.split_div.is_none()
    {
        let x = layout.gutter_w + TEXT_PAD + app.settings.ruler_column as f32 * app.char_w;
        if x > layout.gutter_w && x < layout.editor_right - SCROLLBAR_W {
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x,
                    top: rtop,
                    right: x + 1.0,
                    bottom: rbottom,
                },
                brushes.line,
            );
        }
    }

    // The custom scrollbar, drawn over the editor's right edge: a
    // panel-coloured track with an accent thumb. Only drawn when the
    // document overflows the visible rows.
    if let Some((thumb_y, thumb_h)) =
        scrollbar_geometry(rbottom - rtop, sb_total, sb_page, sb_first)
    {
        let track = D2dRectF {
            left: layout.editor_right - SCROLLBAR_W,
            top: rtop,
            right: layout.editor_right,
            bottom: rbottom,
        };
        fill_rect(fill, target, track, brushes.panel);
        fill_rect(
            fill,
            target,
            D2dRectF {
                top: rtop + thumb_y,
                bottom: rtop + thumb_y + thumb_h,
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
            rtop,
            rbottom - rtop,
            &all_matches,
            table.len(),
            brushes.accent,
        );
        draw_offset_ticks(
            fill,
            target,
            track.left,
            track.right,
            rtop,
            rbottom - rtop,
            &bookmark_offsets,
            table.len(),
            brushes.text,
        );
        // Coloured bookmarks (slice R4c, item 5) redraw their
        // ticks in their swatch colour, over the plain pass above.
        for (i, swatch) in brushes.swatches.iter().enumerate() {
            let offsets: Vec<u64> = bookmark_entries
                .iter()
                .filter(|b| b.color == Some(i as u8))
                .map(|b| b.offset)
                .collect();
            if !offsets.is_empty() {
                draw_offset_ticks(
                    fill,
                    target,
                    track.left,
                    track.right,
                    rtop,
                    rbottom - rtop,
                    &offsets,
                    table.len(),
                    *swatch,
                );
            }
        }
    }

    // The data inspector panel (roadmap item 15): docked on the
    // right while open, drawn in the theme's panel colour with the
    // same monospace format as the editor. The readings are taken
    // from the 16 bytes at the caret on every paint, so caret moves
    // and edits refresh it with no extra plumbing.
    app.bit_boxes.clear();
    app.bit_mirror = None;
    // The inspector steps aside while a compare is open (slice
    // R5c): the compare pane owns the right side. Its open state
    // is preserved and it returns when the compare closes.
    if app.settings.inspector_open && app.compare.is_none() && !disasm_showing(app) {
        let panel_left = layout.editor_right;
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: panel_left,
                top,
                right: width,
                bottom: rbottom,
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
                bottom: rbottom,
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
            if y + app.line_h > rbottom {
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
                if y + app.line_h > rbottom {
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
        // The bit row (R4b hex bit tooling): eight toggle boxes for
        // the byte at the caret, bits 7..0. Clicking a box flips
        // the bit through the editor's undoable byte-replace path;
        // the boxes' rects are remembered for the click test. Text
        // mode and a caret at the very end (no byte) hide the row.
        if app.editor.mode() == EditorMode::Hex {
            if let Some(byte) = table.byte_at(caret) {
                let bits_y = y + 4.0;
                if bits_y + 2.0 * app.line_h <= rbottom {
                    draw_text(
                        draw,
                        target,
                        format,
                        "Bits",
                        D2dRectF {
                            left: panel_left + TEXT_PAD,
                            top: bits_y + app.line_h,
                            right: value_x - 4.0,
                            bottom: bits_y + 2.0 * app.line_h,
                        },
                        brushes.muted,
                    );
                    let box_w = app.char_w * 2.0;
                    let gap = 4.0;
                    for i in 0..8usize {
                        let bit = 7 - i as u32;
                        let bx = value_x + i as f32 * (box_w + gap);
                        let digit_x = bx + (box_w - app.char_w) / 2.0;
                        draw_text(
                            draw,
                            target,
                            format,
                            &bit.to_string(),
                            D2dRectF {
                                left: digit_x,
                                top: bits_y,
                                right: bx + box_w,
                                bottom: bits_y + app.line_h,
                            },
                            brushes.muted,
                        );
                        let rect = D2dRectF {
                            left: bx,
                            top: bits_y + app.line_h,
                            right: bx + box_w,
                            bottom: bits_y + 2.0 * app.line_h,
                        };
                        let is_set = (byte >> bit) & 1 == 1;
                        if is_set {
                            fill_rect(fill, target, rect, brushes.accent);
                        } else {
                            outline_rect(draw_rect, target, rect, brushes.line);
                        }
                        draw_text(
                            draw,
                            target,
                            format,
                            if is_set { "1" } else { "0" },
                            D2dRectF {
                                left: digit_x,
                                top: rect.top,
                                right: bx + box_w,
                                bottom: rect.bottom,
                            },
                            if is_set { brushes.bg } else { brushes.text },
                        );
                        app.bit_boxes
                            .push((rect.left, rect.top, rect.right, rect.bottom));
                    }
                    app.bit_mirror = Some((caret, byte));
                }
            }
        }
    }

    // The disassembly panel (slice R5d): hex mode only, decoded
    // from the caret. refresh_disasm re-decodes whenever the
    // caret left the decoded range or the document changed, so
    // what is painted is never stale; the header names the base.
    if disasm_showing(app) {
        // Re-decode inline (field-wise, so the brushes borrow
        // above is undisturbed) when the rows went stale.
        if !disasm_rows_current(app) {
            let rows = decode_disasm_rows(table, caret);
            app.disasm_base = caret;
            app.disasm_rows = rows;
            app.disasm_stale = false;
        }
        let panel_left = layout.editor_right;
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: panel_left,
                top,
                right: width,
                bottom: rbottom,
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
                bottom: rbottom,
            },
            brushes.line,
        );
        draw_text(
            draw,
            target,
            format,
            &format!("Disassembly \u{2014} from 0x{:X}", app.disasm_base),
            D2dRectF {
                left: panel_left + TEXT_PAD,
                top: top + 4.0,
                right: width - TEXT_PAD,
                bottom: top + 4.0 + app.line_h,
            },
            brushes.muted,
        );
        let mut y = top + 4.0 + app.line_h;
        for row in &app.disasm_rows {
            if y + app.line_h > rbottom {
                break;
            }
            if caret >= row.offset && caret < row.offset + row.len {
                fill_rect(
                    fill,
                    target,
                    D2dRectF {
                        left: panel_left + 1.0,
                        top: y,
                        right: width,
                        bottom: y + app.line_h,
                    },
                    brushes.band,
                );
            }
            draw_text(
                draw,
                target,
                format,
                &format!("{:08X}  {:<23}  {}", row.offset, row.bytes, row.text),
                D2dRectF {
                    left: panel_left + TEXT_PAD,
                    top: y,
                    right: width - TEXT_PAD,
                    bottom: y + app.line_h,
                },
                brushes.text,
            );
            y += app.line_h;
        }
    }

    // The strings pane (R4b): docked along the bottom while open.
    // The template pane (slice R5d) stacks above it, so its top is
    // the rows' bottom plus the template pane's height.
    if layout.strings_h > 0.0 {
        let pane_top = rbottom + layout.tpl_h;
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: 0.0,
                top: pane_top,
                right: width,
                bottom,
            },
            brushes.panel,
        );
        fill_rect(
            fill,
            target,
            D2dRectF {
                left: 0.0,
                top: pane_top,
                right: width,
                bottom: pane_top + 1.0,
            },
            brushes.line,
        );
        let header = if app.strings.searching {
            "Strings \u{00B7} scanning\u{2026}".to_string()
        } else {
            format!("Strings ({})", app.strings.results.len())
        };
        draw_text(
            draw,
            target,
            format,
            &header,
            D2dRectF {
                left: TEXT_PAD,
                top: pane_top + 3.0,
                right: width - TEXT_PAD,
                bottom: pane_top + STRINGS_HEADER_H,
            },
            brushes.text,
        );
        // Min-length chips at the header's right edge.
        app.strings_chips.clear();
        let mut cx = width - TEXT_PAD;
        for min_len in [8u32, 6, 4] {
            let label = format!("min {min_len}");
            let chip_w = (label.len() as f32 + 2.0) * app.char_w;
            cx -= chip_w + 4.0;
            let rect = (
                cx,
                pane_top + 2.0,
                cx + chip_w,
                pane_top + STRINGS_HEADER_H - 2.0,
            );
            let active = min_len == app.settings.strings_min_len;
            if active {
                outline_rect(
                    draw_rect,
                    target,
                    D2dRectF {
                        left: rect.0,
                        top: rect.1,
                        right: rect.2,
                        bottom: rect.3,
                    },
                    brushes.accent,
                );
            }
            draw_text(
                draw,
                target,
                format,
                &label,
                D2dRectF {
                    left: cx + app.char_w,
                    top: pane_top + 3.0,
                    right: rect.2,
                    bottom: pane_top + STRINGS_HEADER_H,
                },
                if active {
                    brushes.accent
                } else {
                    brushes.muted
                },
            );
            app.strings_chips.push((min_len, rect));
        }
        if app.strings.results.is_empty() && !app.strings.searching {
            draw_text(
                draw,
                target,
                format,
                "No strings found",
                D2dRectF {
                    left: TEXT_PAD,
                    top: pane_top + STRINGS_HEADER_H,
                    right: width - TEXT_PAD,
                    bottom: pane_top + STRINGS_HEADER_H + app.line_h,
                },
                brushes.muted,
            );
        }
        for i in 0..STRINGS_ROWS as usize {
            let idx = app.strings.first + i;
            let Some(found) = app.strings.results.get(idx) else {
                break;
            };
            let y = pane_top + STRINGS_HEADER_H + i as f32 * app.line_h;
            if app.strings.selected == Some(idx) {
                fill_rect(
                    fill,
                    target,
                    D2dRectF {
                        left: 0.0,
                        top: y,
                        right: width,
                        bottom: y + app.line_h,
                    },
                    brushes.selection,
                );
            }
            let line = format!("0x{:X}  {}", found.offset, found.text);
            draw_text(
                draw,
                target,
                format,
                &line,
                D2dRectF {
                    left: TEXT_PAD,
                    top: y,
                    right: width - TEXT_PAD,
                    bottom: y + app.line_h,
                },
                brushes.text,
            );
        }
    }

    // The template results pane (slice R5d): docked directly
    // below the rows, above the strings pane.
    if layout.tpl_h > 0.0 {
        // The close box is recorded before the results borrow.
        app.tpl_close_rect = (
            width - TEXT_PAD - 16.0,
            rbottom + 2.0,
            width - TEXT_PAD,
            rbottom + TPL_HEADER_H - 2.0,
        );
        if let Some(result) = &app.tpl_result {
            let pane_top = rbottom;
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: 0.0,
                    top: pane_top,
                    right: width,
                    bottom: pane_top + layout.tpl_h,
                },
                brushes.panel,
            );
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: 0.0,
                    top: pane_top,
                    right: width,
                    bottom: pane_top + 1.0,
                },
                brushes.line,
            );
            let header = if result.complete {
                app.tpl_title.clone()
            } else {
                format!("{} \u{00B7} incomplete (ran past the data)", app.tpl_title)
            };
            draw_text(
                draw,
                target,
                format,
                &header,
                D2dRectF {
                    left: TEXT_PAD,
                    top: pane_top + 3.0,
                    right: width - TEXT_PAD - 24.0,
                    bottom: pane_top + TPL_HEADER_H,
                },
                brushes.text,
            );
            // The close box at the header's right edge.
            let close = app.tpl_close_rect;
            draw_text(
                draw,
                target,
                format,
                "\u{2715}",
                D2dRectF {
                    left: close.0,
                    top: pane_top + 3.0,
                    right: close.2,
                    bottom: pane_top + TPL_HEADER_H,
                },
                brushes.muted,
            );
            for i in 0..TPL_ROWS as usize {
                let idx = app.tpl_first + i;
                let Some(field) = result.fields.get(idx) else {
                    break;
                };
                let y = pane_top + TPL_HEADER_H + i as f32 * app.line_h;
                if app.tpl_selected == Some(idx) {
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: 0.0,
                            top: y,
                            right: width,
                            bottom: y + app.line_h,
                        },
                        brushes.selection,
                    );
                }
                let line = format!(
                    "{}  \u{00B7}  0x{:X}  \u{00B7}  {} B  \u{00B7}  {}",
                    field.path, field.offset, field.size, field.display
                );
                draw_text(
                    draw,
                    target,
                    format,
                    &line,
                    D2dRectF {
                        left: TEXT_PAD,
                        top: y,
                        right: width - TEXT_PAD,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
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

/// Paint one display row of the text view (R4b): the band,
/// find-match and selection washes, the gutter (line number,
/// time delta, bookmark marker), the text itself (plain, or in
/// ANSI/tag segments), and the caret. Shared by the plain,
/// wrapped, frozen and filtered views; `banded` is precomputed by
/// the caller (the caret's line normally, the cursor row in the
/// filter view).
#[allow(clippy::too_many_arguments)]
/// How the caret byte shows in a hex pane (slice R5c): the
/// live caret (blinking, accent, focus-gated — the single-pane
/// behaviour), or a passive marker (the same box, always drawn,
/// in the muted tone: the caret's byte as seen from the split's
/// other pane).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaretMark {
    Active,
    Passive,
}

/// The facts one hex pane's painting needs beyond its rows.
struct HexRowPaint<'a> {
    selection: Option<(u64, u64)>,
    bookmarks: &'a [crate::bookmarks::Bookmark],
    caret: u64,
    mark: CaretMark,
}

/// Paint hex rows into the pane whose gutter starts at `x0`
/// (DIPs), whose gutter is `gutter_w` wide and whose content
/// ends at `right`, rows running from `rows_top` to
/// `rows_bottom`. This is the single hex renderer: the
/// single-pane view calls it over the whole canvas, the split
/// view over the right pane (slice R5c) — the rows themselves
/// come from whichever editor owns the pane.
#[allow(clippy::too_many_arguments)]
fn paint_hex_rows(
    app: &App,
    hex_rows: &[crate::editor::HexRow],
    x0: f32,
    gutter_w: f32,
    right: f32,
    rows_top: f32,
    rows_bottom: f32,
    paint: &HexRowPaint,
) {
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let format = app.text_format;
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    let draw: RtDrawText = unsafe { vfn(target, SLOT_RT_DRAW_TEXT) };
    let selection = paint.selection;
    let bookmark_entries = paint.bookmarks;
    let caret = paint.caret;
    // Column geometry follows the current hex layout
    // (roadmap items 13/14): the ASCII column starts after
    // the formatted hex text plus a two-cell gap.
    let hex_layout = app.editor.hex_layout();
    let hex_x = x0 + gutter_w + TEXT_PAD;
    let ascii_x = hex_x
        + (hexview::hex_text_len(hex_layout.bytes_per_row as usize, &hex_layout) + 2) as f32
            * app.char_w;
    let mark_caret = match paint.mark {
        CaretMark::Active => app.has_focus && app.caret_on,
        CaretMark::Passive => true,
    };
    let mark_brush = if paint.mark == CaretMark::Passive {
        brushes.muted
    } else {
        brushes.accent
    };
    for (i, row) in hex_rows.iter().enumerate() {
        let y = rows_top + i as f32 * app.line_h;
        if y >= rows_bottom {
            break;
        }
        if caret >= row.address && caret < row.address + row.len {
            // The same current-line band as text mode
            // (polish pass), text area only, drawn before
            // the selection cells and the row's text.
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x0 + gutter_w,
                    top: y,
                    right,
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
                let cell = hexview::byte_char_col(idx, row.len as usize, &hex_layout) as f32;
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
        // square as text mode, at the gutter's right edge
        // here — the address text starts at the left. Its
        // colour is the bookmark's annotation colour, or
        // the accent (slice R4c, item 5).
        let bm_at = bookmark_entries.partition_point(|b| b.offset < row.address);
        if matches!(bookmark_entries.get(bm_at), Some(b) if b.offset < row.address + row.len) {
            let size = 6.0;
            let my = y + (app.line_h - size) / 2.0;
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x0 + gutter_w - 2.0 - size,
                    top: my,
                    right: x0 + gutter_w - 2.0,
                    bottom: my + size,
                },
                bookmark_brush(brushes, &bookmark_entries[bm_at]),
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
                left: x0 + TEXT_PAD,
                top: y,
                right: x0 + gutter_w,
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
                right,
                bottom: y + app.line_h,
            },
            brushes.text,
        );
        // Caret: an outline box around the current byte (the
        // passive marker draws the same box, unconditionally
        // and muted).
        if mark_caret && caret >= row.address && caret <= row.address + row.len {
            let idx = (caret.min(row.address + row.len.saturating_sub(1)) - row.address) as usize;
            if row.len > 0 {
                let cell = hexview::byte_char_col(idx, row.len as usize, &hex_layout) as f32;
                let cx0 = hex_x + cell * app.char_w;
                let cx1 = cx0 + 2.0 * app.char_w;
                let t = 1.5f32;
                for r in [
                    D2dRectF {
                        left: cx0,
                        top: y,
                        right: cx1,
                        bottom: y + t,
                    },
                    D2dRectF {
                        left: cx0,
                        top: y + app.line_h - t,
                        right: cx1,
                        bottom: y + app.line_h,
                    },
                    D2dRectF {
                        left: cx0,
                        top: y,
                        right: cx0 + t,
                        bottom: y + app.line_h,
                    },
                    D2dRectF {
                        left: cx1 - t,
                        top: y,
                        right: cx1,
                        bottom: y + app.line_h,
                    },
                ] {
                    fill_rect(fill, target, r, mark_brush);
                }
            }
        }
    }
}

/// Tie the compare pane's scroll to the document's (slice
/// R5c): the right editor's first line (text) or row (hex) is
/// the left's plus the pairing delta, clamped to the right
/// document. Runs in `render`'s prologue, before rows gather.
fn compare_sync_scroll(app: &mut App) {
    let Some(cmp) = &mut app.compare else {
        return;
    };
    match app.editor.mode() {
        EditorMode::Text => {
            let total = cmp.editor.line_count();
            let first = crate::panes::follow_first(app.editor.first_line(), cmp.delta, total);
            cmp.editor.set_first_line(first);
        }
        EditorMode::Hex => {
            let total = hexview::row_count_w(
                cmp.editor.total_len(),
                cmp.editor.hex_layout().bytes_per_row,
            );
            let first = crate::panes::follow_first(app.editor.first_row(), cmp.delta, total);
            cmp.editor.set_first_row(first);
        }
    }
}

/// Paint the split view's two panes (slice R5c): the text
/// representation on the left of the divider, the hex on the
/// right, both from the one editor. The main pass already
/// painted the focused pane across the full canvas, so this
/// first re-clears the rows strip, then repaints each pane
/// inside its own bounds with the ordinary renderers — the
/// text rows through `paint_text_row` with a pane layout, the
/// hex rows through `paint_hex_rows`. The unfocused pane's
/// caret position shows as a passive marker.
fn paint_split(
    app: &App,
    trows: &[DisplayRow],
    hrows: &[crate::editor::HexRow],
    layout: &Layout,
    paint: &RowPaint,
) {
    let Some(div) = layout.split_div else {
        return;
    };
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    let hex_focused = app.editor.mode() == EditorMode::Hex;
    let rtop = layout.rows_top;
    let rbottom = layout.rows_bottom;
    let tg = layout.text_gutter;
    let hg = hex_gutter_w(app);
    let hex_x0 = div + DIVIDER_W;
    // Re-clear the rows strip, then the two gutter backgrounds.
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: 0.0,
            top: rtop,
            right: layout.editor_right,
            bottom: rbottom,
        },
        brushes.bg,
    );
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: 0.0,
            top: rtop,
            right: tg,
            bottom: rbottom,
        },
        brushes.gutter,
    );
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: hex_x0,
            top: rtop,
            right: hex_x0 + hg,
            bottom: rbottom,
        },
        brushes.gutter,
    );
    // The text pane, through the ordinary text renderer with a
    // pane-bounded layout.
    let mut pane_layout = *layout;
    pane_layout.gutter_w = tg;
    pane_layout.editor_right = div;
    let text_paint = RowPaint {
        caret_live: !hex_focused,
        ..*paint
    };
    for (i, row) in trows.iter().enumerate() {
        let y = rtop + i as f32 * app.line_h;
        if y >= rbottom {
            break;
        }
        paint_text_row(
            app,
            row,
            y,
            &pane_layout,
            row.line_no == paint.caret_line,
            &text_paint,
        );
    }
    // The passive caret marker in the text pane while hex has
    // focus: a muted bar at the caret's cell.
    if hex_focused {
        for (i, row) in trows.iter().enumerate() {
            if row.line_no != paint.caret_line {
                continue;
            }
            let y = rtop + i as f32 * app.line_h;
            if y >= rbottom {
                break;
            }
            let row_chars = row.text.chars().count() as u64;
            if paint.caret_col < row.char_start || paint.caret_col > row.char_start + row_chars {
                continue;
            }
            let col = if paint.seg_mode {
                seg_cell_of(
                    &build_segments(&row.text, row.char_start, app.ansi_on, app.ctl_on),
                    paint.caret_col,
                )
            } else {
                paint.caret_col - row.char_start
            };
            let x = tg + TEXT_PAD + col as f32 * app.char_w;
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x,
                    top: y,
                    right: x + 2.0,
                    bottom: y + app.line_h,
                },
                brushes.muted,
            );
        }
    }
    // The column ruler inside the text pane, under the same
    // conditions as the single-pane ruler.
    if app.settings.ruler_column > 0 && !wrap_active(app) {
        let x = tg + TEXT_PAD + app.settings.ruler_column as f32 * app.char_w;
        if x > tg && x < div {
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x,
                    top: rtop,
                    right: x + 1.0,
                    bottom: rbottom,
                },
                brushes.line,
            );
        }
    }
    // The hex pane, through the shared hex renderer.
    paint_hex_rows(
        app,
        hrows,
        hex_x0,
        hg,
        layout.editor_right,
        rtop,
        rbottom,
        &HexRowPaint {
            selection: paint.selection,
            bookmarks: paint.bookmarks,
            caret: app.editor.caret(),
            mark: if hex_focused {
                CaretMark::Active
            } else {
                CaretMark::Passive
            },
        },
    );
    // The divider and the focus bar.
    paint_divider(app, div, layout);
    let (fx0, fx1) = if hex_focused {
        (hex_x0, layout.editor_right)
    } else {
        (0.0, div)
    };
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: fx0,
            top: rtop,
            right: fx1,
            bottom: rtop + 2.0,
        },
        brushes.accent,
    );
}

/// The split/compare divider: a panel band with a one-DIP
/// centre line, in the accent while hovered or dragged.
fn paint_divider(app: &App, div: f32, layout: &Layout) {
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: div,
            top: layout.editor_top,
            right: div + DIVIDER_W,
            bottom: layout.rows_bottom,
        },
        brushes.panel,
    );
    let line_brush = if app.split_div_hover || app.divider_drag.is_some() {
        brushes.accent
    } else {
        brushes.line
    };
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: div + 2.0,
            top: layout.editor_top,
            right: div + 3.0,
            bottom: layout.rows_bottom,
        },
        line_brush,
    );
}

/// Paint the compare's right pane (slice R5c): the second
/// file's rows, plainly — line numbers or addresses in the
/// gutter tone, text in the text colour, difference bands in
/// text mode. No caret, no selection, no editing affordance of
/// any kind: the pane is a reading surface.
fn paint_compare_pane(app: &App, rows: &CmpRows, layout: &Layout) {
    let Some(div) = layout.compare_div else {
        return;
    };
    let Some(cmp) = &app.compare else {
        return;
    };
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let format = app.text_format;
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    let draw: RtDrawText = unsafe { vfn(target, SLOT_RT_DRAW_TEXT) };
    let x0 = div + DIVIDER_W;
    let rtop = layout.rows_top;
    let rbottom = layout.rows_bottom;
    let width = layout.width;
    fill_rect(
        fill,
        target,
        D2dRectF {
            left: x0,
            top: rtop,
            right: width,
            bottom: rbottom,
        },
        brushes.bg,
    );
    match rows {
        CmpRows::Text {
            rows: trows,
            gutter_w: tg,
        } => {
            let tg = *tg;
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x0,
                    top: rtop,
                    right: x0 + tg,
                    bottom: rbottom,
                },
                brushes.gutter,
            );
            let regions = cmp.diff.as_ref().map(|r| r.regions.as_slice());
            for (i, row) in trows.iter().enumerate() {
                let y = rtop + i as f32 * app.line_h;
                if y >= rbottom {
                    break;
                }
                if let Some(regions) = regions {
                    if let Some(idx) = crate::panes::region_index_at_right(regions, row.line_no) {
                        let brush = if cmp.current == Some(idx) {
                            brushes.diff_bg_hot
                        } else {
                            brushes.diff_bg
                        };
                        fill_rect(
                            fill,
                            target,
                            D2dRectF {
                                left: x0,
                                top: y,
                                right: width,
                                bottom: y + app.line_h,
                            },
                            brush,
                        );
                    }
                }
                draw_text(
                    draw,
                    target,
                    format,
                    &format!("{}", row.line_no + 1),
                    D2dRectF {
                        left: x0 + TEXT_PAD,
                        top: y,
                        right: x0 + tg - TEXT_PAD,
                        bottom: y + app.line_h,
                    },
                    brushes.gutter_num,
                );
                draw_text(
                    draw,
                    target,
                    format,
                    &row.text,
                    D2dRectF {
                        left: x0 + tg + TEXT_PAD,
                        top: y,
                        right: width,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
            }
        }
        CmpRows::Hex(hrows) => {
            let hg = hex_gutter_w(app);
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: x0,
                    top: rtop,
                    right: x0 + hg,
                    bottom: rbottom,
                },
                brushes.gutter,
            );
            let hex_layout = cmp.editor.hex_layout();
            let hex_x = x0 + hg + TEXT_PAD;
            let ascii_x = hex_x
                + (hexview::hex_text_len(hex_layout.bytes_per_row as usize, &hex_layout) + 2)
                    as f32
                    * app.char_w;
            for (i, row) in hrows.iter().enumerate() {
                let y = rtop + i as f32 * app.line_h;
                if y >= rbottom {
                    break;
                }
                draw_text(
                    draw,
                    target,
                    format,
                    &format!("{:016X}", row.address),
                    D2dRectF {
                        left: x0 + TEXT_PAD,
                        top: y,
                        right: x0 + hg,
                        bottom: y + app.line_h,
                    },
                    brushes.gutter_num,
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
                        right: width,
                        bottom: y + app.line_h,
                    },
                    brushes.text,
                );
            }
        }
    }
    paint_divider(app, div, layout);
}

fn paint_text_row(
    app: &App,
    row: &DisplayRow,
    y: f32,
    layout: &Layout,
    banded: bool,
    paint: &RowPaint,
) {
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let format = app.text_format;
    let table = app.editor.table();
    let enc = app.editor.codec();
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    let draw_rect: RtDrawRectangle = unsafe { vfn(target, SLOT_RT_DRAW_RECTANGLE) };
    let draw: RtDrawText = unsafe { vfn(target, SLOT_RT_DRAW_TEXT) };
    let text_x = layout.gutter_w + TEXT_PAD;
    let seg_chars = row.text.chars().count() as u64;
    let segs: Vec<PaintSeg> = if paint.seg_mode {
        build_segments(&row.text, row.char_start, app.ansi_on, app.ctl_on)
    } else {
        Vec::new()
    };
    // Line character column → display cell within this row.
    let cell_of = |col: u64| -> f32 {
        if paint.seg_mode {
            seg_cell_of(&segs, col) as f32
        } else {
            col.saturating_sub(row.char_start) as f32
        }
    };
    // Wide-glyph measurement (roadmap item 25, slice R5d): in the
    // plain path, x positions come from the row's own DirectWrite
    // layout whenever its glyphs are not one-cell ASCII; segment
    // mode keeps its cell maths because its runs ARE cell-quantised
    // by construction (control tags occupy two cells, ANSI runs
    // are drawn cell by cell).
    let mut measure = RowMeasure::new(app, &row.text);
    let x_of = |measure: &mut RowMeasure, col: u64| -> f32 {
        if paint.seg_mode {
            cell_of(col) * app.char_w
        } else {
            measure.x_of(col.saturating_sub(row.char_start))
        }
    };

    // Current-line band (polish pass): background blended halfway
    // toward the panel colour, text area only. In the filter view
    // the band marks the cursor row instead.
    if banded {
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
    // Difference band (slice R5c): while a compare report is
    // held, a row inside a differing region of the left document
    // takes the diff tint across the whole pane width, gutter
    // included — the region reads as one block, paired with the
    // band in the compare pane. The current region is hotter.
    if let Some(regions) = paint.diff {
        if let Some(idx) = crate::panes::region_index_at_left(regions, row.line_no) {
            let brush = if paint.diff_current == Some(idx) {
                brushes.diff_bg_hot
            } else {
                brushes.diff_bg
            };
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: layout.origin_x,
                    top: y,
                    right: layout.editor_right,
                    bottom: y + app.line_h,
                },
                brush,
            );
        }
    }
    // Find-match highlights (never in the filter view: the document
    // caret and its matches are frozen there and painting them
    // would mislead). Matches are spans now (slice R5d): they are
    // sorted and non-overlapping, so at most one span can straddle
    // the row's start — the scan begins one span early to catch it.
    if !paint.in_filter {
        let begin = paint.match_spans.partition_point(|&(s, _)| s < row.start);
        let scan_from = begin.saturating_sub(1);
        for &(ms, me) in &paint.match_spans[scan_from..] {
            if ms >= row.end {
                break;
            }
            if me <= row.start || me <= ms {
                continue;
            }
            let c0 = col_of_byte(table, enc, row.line_start, ms) as u64;
            let c1 = col_of_byte(table, enc, row.line_start, me.min(row.end)) as u64;
            let seg_lo = row.char_start;
            let seg_hi = row.char_start + seg_chars;
            if c1 <= seg_lo || c0 >= seg_hi {
                continue;
            }
            let rect = D2dRectF {
                left: text_x + x_of(&mut measure, c0.max(seg_lo)),
                top: y,
                right: text_x + x_of(&mut measure, c1.min(seg_hi)),
                bottom: y + app.line_h,
            };
            fill_rect(fill, target, rect, brushes.match_bg);
            if paint.current_match == Some((ms, me)) {
                outline_rect(draw_rect, target, rect, brushes.accent);
            }
        }
    }
    // Selection highlight.
    if !paint.in_filter {
        if let Some((ss, se)) = paint.selection {
            if se > row.start && ss <= row.end {
                let from = ss.max(row.start);
                let c0 = col_of_byte(table, enc, row.line_start, from) as u64;
                if c0 > row.char_start + seg_chars {
                    // Starts past this segment: a later segment
                    // (or none) owns it.
                } else {
                    let x0 = text_x + x_of(&mut measure, c0.max(row.char_start));
                    let x1 = if se > row.end {
                        layout.editor_right
                    } else {
                        let c1 = col_of_byte(table, enc, row.line_start, se) as u64;
                        text_x + x_of(&mut measure, c1.min(row.char_start + seg_chars))
                    };
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: x0,
                            top: y,
                            right: x1,
                            bottom: y + app.line_h,
                        },
                        brushes.selection,
                    );
                }
            }
        }
    }
    // Rectangle selection wash (slice R5d): the covered segment
    // of this row in the selection colour. The intersection is
    // pure offset maths against the segment — bytes past the
    // line's content end exist in no segment, so the column
    // clamp needs no index lookup here.
    if !paint.in_filter {
        if let Some(rect) = paint.rect {
            if row.line_no >= rect.first_line && row.line_no <= rect.last_line {
                let rs = row.line_start + rect.col_start;
                let re = row.line_start + rect.col_end;
                let from = rs.max(row.start);
                let to = re.min(row.end);
                if from < to {
                    let c0 = col_of_byte(table, enc, row.line_start, from) as u64;
                    let c1 = col_of_byte(table, enc, row.line_start, to) as u64;
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: text_x + x_of(&mut measure, c0.max(row.char_start)),
                            top: y,
                            right: text_x + x_of(&mut measure, c1.min(row.char_start + seg_chars)),
                            bottom: y + app.line_h,
                        },
                        brushes.selection,
                    );
                }
            }
        }
        // Extra carets (slice R5d): a thin accent bar at each one
        // that falls on this row segment.
        for &off in paint.extra_carets {
            if off >= row.start && off <= row.end {
                let c = col_of_byte(table, enc, row.line_start, off) as u64;
                let x = text_x + x_of(&mut measure, c);
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
    // Gutter contents paint once per line, on its first segment.
    if row.char_start == 0 {
        // Time delta (R4b): right-aligned in its own gutter column.
        if layout.delta_w > 0.0 {
            if let Some(delta) = paint.deltas.get(&row.line_no) {
                let text = format_delta(*delta);
                draw_text(
                    draw,
                    target,
                    format,
                    &text,
                    D2dRectF {
                        left: TEXT_PAD,
                        top: y,
                        right: TEXT_PAD + layout.delta_w - 4.0,
                        bottom: y + app.line_h,
                    },
                    brushes.muted,
                );
            }
        }
        // Line number, right-aligned in the gutter. In the filter
        // view this is the ORIGINAL line number — the whole point
        // of the view is knowing where each line lives.
        let number = (row.line_no + 1).to_string();
        let num_w = number.len() as f32 * app.char_w;
        let num_brush = if banded {
            brushes.accent
        } else {
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
        // Bookmark marker (roadmap item 20): a small accent square
        // at the gutter's left edge — or just right of the delta
        // column when that is showing.
        let bm_at = paint.bookmarks.partition_point(|b| b.offset < row.start);
        if matches!(paint.bookmarks.get(bm_at), Some(b) if b.offset < row.end || b.offset == row.start)
        {
            let size = 6.0;
            let my = y + (app.line_h - size) / 2.0;
            let bx = if layout.delta_w > 0.0 {
                TEXT_PAD + layout.delta_w + 2.0
            } else {
                2.0
            };
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: bx,
                    top: my,
                    right: bx + size,
                    bottom: my + size,
                },
                bookmark_brush(brushes, &paint.bookmarks[bm_at]),
            );
        }
    }
    // The row's text: one run normally, or the segment runs when
    // ANSI colours / control tags are on. Focus dimming paints
    // other paragraphs in the muted brush.
    let dimmed = matches!(paint.dim_range, Some((lo, hi)) if row.line_no < lo || row.line_no > hi);
    let text_brush = if dimmed { brushes.muted } else { brushes.text };
    if paint.seg_mode {
        let mut cx = text_x;
        for seg in &segs {
            if seg.tag {
                // A small inverted tag: line-coloured box, muted
                // notation, two cells wide for one character.
                fill_rect(
                    fill,
                    target,
                    D2dRectF {
                        left: cx + 0.5,
                        top: y + 1.0,
                        right: cx + 2.0 * app.char_w - 0.5,
                        bottom: y + app.line_h - 1.0,
                    },
                    brushes.line,
                );
                draw_text(
                    draw,
                    target,
                    format,
                    &seg.text,
                    D2dRectF {
                        left: cx,
                        top: y,
                        right: cx + 2.0 * app.char_w,
                        bottom: y + app.line_h,
                    },
                    brushes.muted,
                );
            } else {
                let brush = seg.fg.map(|ix| brushes.ansi[ix]).unwrap_or(text_brush);
                draw_text(
                    draw,
                    target,
                    format,
                    &seg.text,
                    D2dRectF {
                        left: cx,
                        top: y,
                        right: cx + seg.cells as f32 * app.char_w,
                        bottom: y + app.line_h,
                    },
                    brush,
                );
            }
            cx += seg.cells as f32 * app.char_w;
        }
    } else {
        draw_text(
            draw,
            target,
            format,
            &row.text,
            D2dRectF {
                left: text_x,
                top: y,
                right: layout.editor_right,
                bottom: y + app.line_h,
            },
            text_brush,
        );
    }
    // Caret: a 2px accent bar at the caret's cell, on the segment
    // that holds it (the line-end caret belongs to the last one).
    if app.has_focus
        && app.caret_on
        && paint.caret_live
        && !paint.in_filter
        && row.line_no == paint.caret_line
    {
        let in_segment = paint.caret_col >= row.char_start
            && (paint.caret_col < row.char_start + seg_chars
                || (paint.caret_col == paint.caret_line_chars
                    && row.char_start + seg_chars == paint.caret_line_chars));
        if in_segment {
            let x = text_x + x_of(&mut measure, paint.caret_col);
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

/// Paint the visible rows in delimiter-grid form (R4b): each
/// line's fields aligned into columns by padding in the PAINT step
/// only — the file's bytes, the caret columns and copy all stay
/// byte-true. Find and selection washes clip per field.
#[allow(clippy::too_many_arguments)]
fn paint_grid_rows(
    app: &App,
    disp: &[DisplayRow],
    delim: crate::delimited::Delimiter,
    widths: &[u64],
    layout: &Layout,
    paint: &RowPaint,
) {
    if app.target.is_null() || app.brushes.is_none() {
        return;
    }
    let target = app.target;
    let brushes = app.brushes.as_ref().expect("checked above");
    let format = app.text_format;
    let table = app.editor.table();
    let enc = app.editor.codec();
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
    let draw_rect: RtDrawRectangle = unsafe { vfn(target, SLOT_RT_DRAW_RECTANGLE) };
    let draw: RtDrawText = unsafe { vfn(target, SLOT_RT_DRAW_TEXT) };
    let text_x = layout.gutter_w + TEXT_PAD;
    let rtop = layout.rows_top;
    let field_x = |fi: usize| -> f32 {
        let mut cells: u64 = 0;
        for (i, w) in widths.iter().enumerate() {
            if i >= fi {
                break;
            }
            cells += w + GRID_GAP_CHARS;
        }
        text_x + cells as f32 * app.char_w
    };
    for (i, row) in disp.iter().enumerate() {
        let y = rtop + i as f32 * app.line_h;
        if y >= layout.rows_bottom {
            break;
        }
        // Current-line band, as in the plain painter.
        if row.line_no == paint.caret_line {
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
        // Gutter: delta, number, bookmark — the plain painter's
        // rules for whole-line rows.
        if layout.delta_w > 0.0 {
            if let Some(delta) = paint.deltas.get(&row.line_no) {
                let text = format_delta(*delta);
                draw_text(
                    draw,
                    target,
                    format,
                    &text,
                    D2dRectF {
                        left: TEXT_PAD,
                        top: y,
                        right: TEXT_PAD + layout.delta_w - 4.0,
                        bottom: y + app.line_h,
                    },
                    brushes.muted,
                );
            }
        }
        let number = (row.line_no + 1).to_string();
        let num_w = number.len() as f32 * app.char_w;
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
            if row.line_no == paint.caret_line {
                brushes.accent
            } else {
                brushes.gutter_num
            },
        );
        let bm_at = paint.bookmarks.partition_point(|b| b.offset < row.start);
        if matches!(paint.bookmarks.get(bm_at), Some(b) if b.offset < row.end || b.offset == row.start)
        {
            let size = 6.0;
            let my = y + (app.line_h - size) / 2.0;
            let bx = if layout.delta_w > 0.0 {
                TEXT_PAD + layout.delta_w + 2.0
            } else {
                2.0
            };
            fill_rect(
                fill,
                target,
                D2dRectF {
                    left: bx,
                    top: my,
                    right: bx + size,
                    bottom: my + size,
                },
                bookmark_brush(brushes, &paint.bookmarks[bm_at]),
            );
        }
        // The fields themselves.
        let content = table.read(
            row.line_start,
            (row.end - row.line_start).min(GRID_FETCH_BYTES),
        );
        let fields = crate::delimited::split_line(&content, delim);
        let dimmed =
            matches!(paint.dim_range, Some((lo, hi)) if row.line_no < lo || row.line_no > hi);
        let text_brush = if dimmed { brushes.muted } else { brushes.text };
        let mut caret_x: Option<f32> = None;
        for (fi, &(fs, fe)) in fields.iter().enumerate() {
            let fx = field_x(fi);
            let f_abs = row.line_start + fs;
            let f_end = row.line_start + fe;
            // Find-match wash inside this field (spans, slice
            // R5d — the scan starts one span early for a match
            // straddling the field's start).
            {
                let begin = paint.match_spans.partition_point(|&(s, _)| s < f_abs);
                let scan_from = begin.saturating_sub(1);
                for &(ms, me) in &paint.match_spans[scan_from..] {
                    if ms >= f_end {
                        break;
                    }
                    if me <= f_abs || me <= ms {
                        continue;
                    }
                    let c0 = col_of_byte(table, enc, f_abs, ms.max(f_abs));
                    let c1 = col_of_byte(table, enc, f_abs, me.min(f_end));
                    let rect = D2dRectF {
                        left: fx + c0 * app.char_w,
                        top: y,
                        right: fx + c1 * app.char_w,
                        bottom: y + app.line_h,
                    };
                    fill_rect(fill, target, rect, brushes.match_bg);
                    if paint.current_match == Some((ms, me)) {
                        outline_rect(draw_rect, target, rect, brushes.accent);
                    }
                }
            }
            // Selection wash inside this field.
            if let Some((ss, se)) = paint.selection {
                if se > f_abs && ss < f_end {
                    let c0 = col_of_byte(table, enc, f_abs, ss.max(f_abs));
                    let c1 = col_of_byte(table, enc, f_abs, se.min(f_end));
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: fx + c0 * app.char_w,
                            top: y,
                            right: fx + c1 * app.char_w,
                            bottom: y + app.line_h,
                        },
                        brushes.selection,
                    );
                }
            }
            let text = crate::editor::decode_lossy_codec(&content[fs as usize..fe as usize], enc);
            draw_text(
                draw,
                target,
                format,
                &text,
                D2dRectF {
                    left: fx,
                    top: y,
                    right: layout.editor_right,
                    bottom: y + app.line_h,
                },
                text_brush,
            );
            // The caret lives in exactly one field (or at the
            // line's end, in the last one).
            let caret = app.editor.caret();
            if caret >= f_abs && caret <= f_end {
                caret_x = Some(fx + col_of_byte(table, enc, f_abs, caret) * app.char_w);
            }
        }
        if app.has_focus && app.caret_on && row.line_no == paint.caret_line {
            if let Some(x) = caret_x {
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
}

/// Map a mouse position (physical pixels) to a caret byte offset using
/// monospace cell maths (the SPEC.md section 4 font is fixed-pitch, so
/// cell maths is exact; documented in docs/PARITY.md).
/// Pointer maths for the delimiter grid: find the field under the
/// pointer using the same column widths the painter computed, then
/// the character inside the field. The returned offset is a real
/// document position — padding cells past a short field's text land
/// on the field's end, never inside the next field.
fn pos_to_offset_grid(app: &mut App, x: f32, row_in_view: usize) -> u64 {
    let page = page_rows(app) as usize + 1;
    let disp = display_rows(app, page);
    if disp.is_empty() {
        return 0;
    }
    let idx = row_in_view.min(disp.len() - 1);
    let (line_no, line_start, line_end) = {
        let row = &disp[idx];
        (row.line_no, row.line_start, row.end)
    };
    let Some((delim, widths)) = grid_model(app, &disp) else {
        return app.editor.offset_of_line_col(line_no, 0).unwrap_or(0);
    };
    let layout = compute_layout(app);
    let text_x = layout.gutter_w + TEXT_PAD;
    let enc = app.editor.codec();
    let content = app
        .editor
        .table()
        .read(line_start, (line_end - line_start).min(GRID_FETCH_BYTES));
    let fields = crate::delimited::split_line(&content, delim);
    if fields.is_empty() {
        return line_start;
    }
    let mut fx = text_x;
    for (fi, &(fs, fe)) in fields.iter().enumerate() {
        let col_w = widths.get(fi).copied().unwrap_or(0);
        let field_w = (col_w + GRID_GAP_CHARS) as f32 * app.char_w;
        let last = fi + 1 == fields.len();
        if x < fx + field_w || last {
            let own = crate::editor::decode_lossy_codec(&content[fs as usize..fe as usize], enc)
                .chars()
                .count() as u64;
            let within = (((x - fx) / app.char_w.max(1.0)).floor().max(0.0) as u64).min(own);
            let f_abs = line_start + fs;
            let field_chars = col_of_byte(app.editor.table(), enc, line_start, f_abs) as u64;
            return app
                .editor
                .offset_of_line_col(line_no, field_chars + within)
                .unwrap_or(line_start);
        }
        fx += field_w;
    }
    line_start
}

fn pos_to_offset(app: &mut App, x_px: i32, y_px: i32) -> u64 {
    let layout = compute_layout(app);
    let x = app.dip(x_px);
    // In the split view the pane under the pointer decides the
    // representation the click resolves through (slice R5c):
    // the same maths, run against the pane's own layout.
    if let Some(div) = layout.split_div {
        if x >= div + DIVIDER_W {
            let mut pane = layout;
            pane.origin_x = div + DIVIDER_W;
            pane.gutter_w = hex_gutter_w(app);
            return pos_to_offset_in(app, x_px, y_px, &pane, EditorMode::Hex);
        }
        let mut pane = layout;
        pane.gutter_w = layout.text_gutter;
        return pos_to_offset_in(app, x_px, y_px, &pane, EditorMode::Text);
    }
    pos_to_offset_in(app, x_px, y_px, &layout, app.editor.mode())
}

/// The click→offset maths against an explicit pane layout and
/// representation (the single-pane path passes the canvas
/// layout and the editor's mode).
fn pos_to_offset_in(app: &mut App, x_px: i32, y_px: i32, layout: &Layout, mode: EditorMode) -> u64 {
    let x = app.dip(x_px);
    let y = app.dip(y_px);
    let row_in_view = ((y - layout.rows_top) / app.line_h.max(1.0)).floor();
    if row_in_view < 0.0 {
        return app.editor.caret();
    }
    if x > layout.editor_right {
        // A press on the inspector panel (roadmap item 15) is not
        // an editor click: keep the caret where it is.
        return app.editor.caret();
    }
    match mode {
        EditorMode::Text => {
            // Text clicks resolve through the same display rows the
            // painter uses (R4b), so wrap segments, frozen lines and
            // filtered lines all hit-test exactly as drawn. In the
            // filter view the returned offset names the clicked
            // filtered line's position — callers use it only to
            // place the filter cursor, never to edit.
            if grid_active(app) {
                return pos_to_offset_grid(app, x, row_in_view as usize);
            }
            let page = page_rows(app) as usize + 1;
            // The model directly (not the mode-guarded
            // `display_rows`): a split text-pane click resolves
            // while the editor's mode is hex.
            let disp = display_rows_model(app, page);
            if disp.is_empty() {
                return 0;
            }
            let idx = (row_in_view as usize).min(disp.len() - 1);
            let (line_no, char_start, text) = {
                let row = &disp[idx];
                (row.line_no, row.char_start, row.text.clone())
            };
            let text_x = layout.origin_x + layout.gutter_w + TEXT_PAD;
            // Wide-glyph hit-testing (roadmap item 25, slice R5d):
            // a row whose glyphs are not one-cell ASCII resolves
            // the click through the row's own DirectWrite layout,
            // exactly as the painter measures it; anything else
            // keeps the cell maths (exact for ASCII, and the
            // segment mode's runs are cell-quantised by design).
            let measured_col = if !(app.ansi_on || app.ctl_on) && !text.is_ascii() {
                let layout_ptr = create_line_layout(app, &text);
                let hit = if layout_ptr.is_null() {
                    None
                } else {
                    layout_char_at(layout_ptr, x - text_x)
                };
                if !layout_ptr.is_null() {
                    // SAFETY: created just above, owned here,
                    // released exactly once.
                    unsafe { com_release(layout_ptr) };
                }
                hit.map(|units| char_start + char_pos_of_utf16(&text, units))
            } else {
                None
            };
            let col = if let Some(col) = measured_col {
                col
            } else {
                let cell = ((x - text_x) / app.char_w.max(1.0)).floor().max(0.0) as u64;
                if app.ansi_on || app.ctl_on {
                    let segs = build_segments(&text, char_start, app.ansi_on, app.ctl_on);
                    seg_col_of_cell(&segs, cell)
                } else {
                    char_start + cell
                }
            };
            app.editor
                .offset_of_line_col(line_no, col)
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
            let hex_x = layout.origin_x + layout.gutter_w + TEXT_PAD;
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
    spawn_index_thread_resume(app, mmap, None);
}

/// Spawn the background line-index build. With `resume`, the
/// builder starts from that snapshot's frontier instead of from
/// zero (Ghost Tail: the index held over the old length describes
/// the grown document's prefix exactly, so the build continues
/// where it stopped). A resume snapshot whose total disagrees with
/// the mapping is ignored rather than trusted.
fn spawn_index_thread_resume(app: &mut App, mmap: Arc<MmapFile>, resume: Option<LineIndex>) {
    app.index_epoch += 1;
    let epoch = app.index_epoch;
    let slot = app.index_slot.clone();
    if let Ok(mut guard) = slot.lock() {
        *guard = None;
    }
    let resume = resume.filter(|index| index.total_length() == mmap.len());
    let start = resume
        .as_ref()
        .map(|index| index.indexed_bytes())
        .unwrap_or(0);
    app.indexing = Some((start, mmap.len()));
    std::thread::spawn(move || {
        let total = mmap.len();
        let mut builder = match &resume {
            Some(index) => LineIndexBuilder::resume_from(index),
            None => LineIndexBuilder::new(total),
        };
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
    // Clone the full stored entries (slice R4c: colour and note
    // annotations ride along, not just offsets and names).
    let entries: Vec<crate::bookmarks::Bookmark> =
        app.bookmark_store.bookmarks(&path, stamp).to_vec();
    for entry in entries {
        app.bookmarks.add(entry);
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
    // R4b view state belongs to the previous document: the filter
    // view is dropped, the jump history clears on open (item 11),
    // and the strings / entropy mirrors are staled so their panes
    // rescan the new document.
    app.filter = None;
    cancel_filter_build(app);
    app.jump_history.clear();
    app.status_note = None;
    app.tail_pin_view = false;
    app.strings.gen += 1;
    app.strings.results.clear();
    app.strings.first = 0;
    app.strings.selected = None;
    app.strings.searching = false;
    app.entropy.gen += 1;
    app.entropy.values.clear();
    app.entropy.computing = false;
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
            app.doc_read_only_file = crate::save::path_is_read_only(&path);
            load_session_bookmarks(app);
            app.doc_fingerprint = None;
            spawn_fingerprint_thread(app, &path);
        }
        None => {
            app.doc_stamp = None;
            app.file_stamp = None;
            app.doc_read_only_file = false;
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
    app.doc_read_only_file = crate::save::path_is_read_only(&path);
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
        record_jump(app, caret, offset);
        after_caret_move(app);
        // A bookmark note (slice R4c, item 5) rides the status bar
        // for this jump — set after after_caret_move, which clears
        // one-shot notes.
        if let Some(bookmark) = app.bookmarks.get(offset) {
            if !bookmark.note.is_empty() {
                let note = bookmark.note.clone();
                app.status_note = Some(note);
                refresh_status(app);
            }
        }
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
    app.bg_ends.clear();
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
    let needle = match app.editor.encode_text(text) {
        Ok(needle) => needle,
        Err(_) => return false,
    };
    let snapshot = snapshot_table(app);
    let len = snapshot.len();
    let options = SearchOptions::default();
    // The editor filters UTF-16 matches to code-unit-aligned
    // offsets; the worker applies the same filter so the two paths
    // never disagree. (Under a code page there is no alignment to
    // filter — the codec check covers both cases.)
    let utf16_bom = if app.editor.codec_is_utf16() {
        Some(app.editor.bom_skip())
    } else {
        None
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
            ends: Vec::new(),
            done: false,
        });
    }
    app.bg_active = true;
    app.bg_matches.clear();
    app.bg_ends.clear();
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

/// Start the background REGEX find for `text` (slice R5d): the
/// pattern is compiled once here — a pattern that fails to compile
/// returns false and the caller runs the synchronous find, which
/// reports the error — and the worker then streams the engine over
/// a snapshot of the document, publishing spans in batches so the
/// count climbs while a huge document is still being walked. The
/// ends travel with the starts: regex matches vary in length.
fn start_bg_regex_find(app: &mut App, text: &str) -> bool {
    let re = match crate::regex::Regex::new(text) {
        Ok(re) => re,
        Err(_) => return false,
    };
    let snapshot = snapshot_table(app);
    let total = snapshot.len();
    app.find_gen += 1;
    let find_gen = app.find_gen;
    let doc_gen = app.doc_generation;
    let slot = app.find_slot.clone();
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(FindProgress {
            find_gen,
            doc_gen,
            matches: Vec::new(),
            ends: Vec::new(),
            done: false,
        });
    }
    app.bg_active = true;
    app.bg_matches.clear();
    app.bg_ends.clear();
    app.bg_current = None;
    app.bg_done = false;
    app.bg_needle_len = 0;
    std::thread::spawn(move || {
        let mut iter = re.find_iter(total, |offset, buf| snapshot.read_into(offset, buf));
        loop {
            let mut batch: Vec<(u64, u64)> = Vec::with_capacity(8192);
            let mut exhausted = false;
            for _ in 0..8192 {
                match iter.next() {
                    Some(span) => batch.push(span),
                    None => {
                        exhausted = true;
                        break;
                    }
                }
            }
            let done = exhausted;
            if let Ok(mut guard) = slot.lock() {
                match guard.as_mut() {
                    Some(progress)
                        if progress.find_gen == find_gen && progress.doc_gen == doc_gen =>
                    {
                        progress.matches.extend(batch.iter().map(|&(s, _)| s));
                        progress.ends.extend(batch.iter().map(|&(_, e)| e));
                        progress.done = done;
                    }
                    // Superseded or cancelled: stop quietly.
                    _ => return,
                }
            }
            if done {
                return;
            }
        }
    });
    true
}

/// The end offset of the background find's match at `idx`: from
/// the span ends for a regex find, start + needle length for a
/// plain one.
fn bg_match_end(app: &App, idx: usize) -> u64 {
    let start = app.bg_matches.get(idx).copied().unwrap_or(0);
    match app.bg_ends.get(idx) {
        Some(&end) => end,
        None => start + app.bg_needle_len,
    }
}

/// Select the background find's match at `idx`: the selection is
/// placed with the editor's ordinary caret moves (anchor at the
/// match start, caret at its end — the same shape the editor's own
/// match selection produces).
fn select_bg_match(app: &mut App, idx: usize) {
    let Some(&start) = app.bg_matches.get(idx) else {
        return;
    };
    let end = bg_match_end(app, idx);
    app.bg_current = Some(idx);
    app.editor.move_caret_to(start, false);
    app.editor.move_caret_to(end, true);
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

/// Every displayed match as a byte span, from whichever find owns
/// the display (slice R5d). Plain finds share one needle length —
/// taken from the current match, whose length is the needle's —
/// while regex finds carry per-match ends (the editor's span list,
/// or the background mirror's ends). Sorted by start, as every
/// underlying list is.
fn display_spans(app: &App) -> Vec<(u64, u64)> {
    if app.bg_active {
        if !app.bg_ends.is_empty() {
            return app
                .bg_matches
                .iter()
                .copied()
                .zip(app.bg_ends.iter().copied())
                .collect();
        }
        return app
            .bg_matches
            .iter()
            .map(|&s| (s, s + app.bg_needle_len))
            .collect();
    }
    if !app.editor.match_spans().is_empty() {
        return app.editor.match_spans().to_vec();
    }
    let len = app.editor.current_match().map(|(s, e)| e - s).unwrap_or(0);
    app.editor.matches().iter().map(|&s| (s, s + len)).collect()
}

/// The currently selected match's byte range, from whichever find
/// owns the display.
fn display_current_match(app: &App) -> Option<(u64, u64)> {
    if app.bg_active {
        let idx = app.bg_current?;
        let start = *app.bg_matches.get(idx)?;
        Some((start, bg_match_end(app, idx)))
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
    // Ghost Tail (roadmap item 22): with Follow Tail on and an
    // unedited document, a file that only GREW is adopted silently
    // — no bar, no reload, no disturbance. A shrink, a same-size
    // rewrite, or an edited document never takes this path; a growth
    // the core refuses (for example, history left behind by an edit
    // that was undone) falls through to the bar as before.
    if app.settings.follow_tail
        && now.size > base.size
        && !app.editor.is_dirty()
        && try_follow_growth(app, &path, now)
    {
        return;
    }
    if app.change_shown_for == Some(now) || app.change_ignored_for == Some(now) {
        return;
    }
    app.change_shown_for = Some(now);
    set_change_bar_visible(app, true);
}

/// Adopt an on-disk growth into the open document (Ghost Tail,
/// roadmap item 22): re-map the grown file, hand the new mapping to
/// the editor's growth adoption, and resume the line index from the
/// frontier the editor already holds. The caret, selection, scroll
/// position and mode are untouched — except the two "at the end"
/// cases, which follow the tail: a caret at the very end rides to
/// the new end at once, and a view scrolled to the end is pinned to
/// the new end when the resumed index completes. Returns false when
/// the growth cannot be adopted (the caller then shows the
/// external-change bar).
fn try_follow_growth(
    app: &mut App,
    path: &std::path::Path,
    now: crate::bookmarks::FileStamp,
) -> bool {
    let (sb_total, sb_first, sb_page) = scroll_metrics(app);
    let view_at_end = sb_first.saturating_add(sb_page) >= sb_total;
    let caret_at_end = app.editor.caret() >= app.editor.total_len();
    let Ok(mmap) = MmapFile::open(path) else {
        return false;
    };
    if mmap.len() != now.size {
        // The file moved again between the stamp and the map; leave
        // this state to the change bar.
        return false;
    }
    let mmap = Arc::new(mmap);
    let old_len = app.editor.total_len();
    if app
        .editor
        .adopt_grown_backing(now.size, OriginalBuffer::Mapped(mmap.clone()))
        .is_err()
    {
        return false;
    }
    app.doc_mmap = Some(mmap.clone());
    app.file_stamp = Some(now);
    app.change_shown_for = None;
    if app.change_visible {
        set_change_bar_visible(app, false);
    }
    // Resume indexing from the snapshot the editor now holds (the
    // held index was extended over the grown length by the core).
    let resume = app.editor.line_index_snapshot();
    spawn_index_thread_resume(app, mmap, resume);
    // A filtered view was built over the old length; drop it rather
    // than let it describe a document that no longer matches.
    app.filter = None;
    app.tail_pin_view = view_at_end && !caret_at_end;
    if caret_at_end {
        let end = app.editor.total_len();
        let page = page_rows(app);
        app.editor.move_caret_to(end, false);
        app.editor.ensure_caret_visible(page);
    }
    app.status_note = Some(format!(
        "File grew by {} — following",
        human_size(now.size.saturating_sub(old_len))
    ));
    refresh_status(app);
    invalidate(app);
    true
}

/// Open `path` in the window: memory-mapped, encoding detected, index
/// building in the background. Errors are reported, never fatal.
fn open_document(app: &mut App, path: &Path) {
    // A fresh document replaces the compared one (slice R5c):
    // the compare session closes rather than pairing against a
    // document it never diffed.
    close_compare(app);
    // Multi-caret / rectangle / template state names the old
    // document's bytes (slice R5d): all collapse on an open.
    clear_multi(app);
    app.tpl_open = false;
    app.tpl_result = None;
    app.tpl_selected = None;
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
    // Mapping windows (slice R4c, item 1): with map_window_mb set,
    // the detection sample is read through a remapped window
    // instead of the whole-file view, exercising the windowed
    // reader on the open path. The document backing itself stays
    // whole-file in this slice (see PARITY) — the windowed reader
    // is the seam the backing migrates to later. Any windowed
    // failure falls back to the whole-file sample.
    let windowed_sample = if app.settings.map_window_mb > 0 {
        crate::mmap_win::WindowedMap::open(
            path,
            crate::mapwindow::window_bytes(app.settings.map_window_mb),
        )
        .ok()
        .and_then(|map| {
            let mut buf = vec![0u8; sample_len];
            if map.read_at(0, &mut buf) == sample_len {
                Some(buf)
            } else {
                None
            }
        })
    } else {
        None
    };
    let (detection, eol) = match &windowed_sample {
        Some(sample) => (encoding::detect(sample), detect_eol(sample)),
        None => {
            let sample = &mmap.as_slice()[..sample_len];
            (encoding::detect(sample), detect_eol(sample))
        }
    };
    app.eol = eol;
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
    // The crash-recovery record belongs to the document it was
    // written for; opening during a session restore (roadmap
    // item 39) must not discard the startup document's accepted
    // recovery.
    if !app.suppress_recovery_delete {
        delete_recovery_files();
    }
    begin_document_tracking(app);
    let path_string = path.to_string_lossy().into_owned();
    app.settings.add_recent(&path_string);
    publish_jump_list(app);
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

// ---------------------------------------------------------------------------
// Tabs (roadmap item 39, slice R5b): several documents in one
// window. The active document's state lives in the App fields
// (see DocState); inactive documents are parked in `app.parked`.
// Every operation below is built from two primitives — parking
// the live state and swapping a parked state in — so the tab
// order bookkeeping in `crate::tabs` and the parked list can be
// reasoned about (and tested) separately from the window.
// ---------------------------------------------------------------------------

/// A fresh empty document's state: what a new tab starts with —
/// an untitled editor under the owner's undo limits, the
/// persisted save options as its armed choices, and every worker
/// slot new. Mirrors the initial document's defaults in
/// `run_gui`.
fn fresh_doc_state(app: &mut App) -> DocState {
    app.next_doc_id += 1;
    let mut editor = Editor::new_empty();
    editor.set_undo_limits(
        app.settings.undo_limit,
        app.settings.undo_byte_budget_mb * 1024 * 1024,
    );
    DocState {
        id: app.next_doc_id,
        editor,
        find_in_selection: false,
        find_regex: false,
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
        bg_ends: Vec::new(),
        find_sync_once: false,
        bookmarks: crate::bookmarks::BookmarkList::new(),
        doc_stamp: None,
        file_stamp: None,
        change_visible: false,
        change_shown_for: None,
        change_ignored_for: None,
        fingerprint_slot: Arc::new(Mutex::new(None)),
        doc_epoch: 0,
        doc_fingerprint: None,
        eol: "LF",
        rel_offsets: false,
        rel_base: 0,
        find_visible: false,
        pending_high_surrogate: None,
        hex_nibble: None,
        index_slot: Arc::new(Mutex::new(None)),
        indexing: None,
        doc_generation: 0,
        open_generation: 0,
        last_edit: None,
        last_find_text: String::new(),
        pending_goto: None,
        index_epoch: 0,
        status_note: None,
        tail_pin_view: false,
        filter: None,
        strings: StringsState {
            slot: Arc::new(Mutex::new(None)),
            gen: 0,
            results: Vec::new(),
            first: 0,
            selected: None,
            searching: false,
            scanned_gen: 0,
            scanned_min_len: 0,
        },
        entropy: EntropyState {
            slot: Arc::new(Mutex::new(None)),
            gen: 0,
            values: Vec::new(),
            computing: false,
            scanned_gen: 0,
        },
        entropy_on: false,
        grid_on: false,
        deltas_on: false,
        dim_on: false,
        ansi_on: false,
        ctl_on: false,
        jump_history: JumpHistory::new(),
        filter_slot: Arc::new(Mutex::new(None)),
        filter_gen: 0,
        doc_read_only_file: false,
        save_eol: app.settings.save_eol_choice(),
        save_bom: app.settings.save_bom_choice(),
        find_box: String::new(),
        replace_box: String::new(),
        find_count: String::new(),
    }
}

/// Set a child control's text (null-safe).
fn set_control_text(hwnd: HWND, text: &str) {
    if hwnd.is_null() {
        return;
    }
    let wide = wstr(text);
    // SAFETY: our child window plus a NUL-terminated string.
    unsafe { SetWindowTextW(hwnd, wide.as_ptr()) };
}

/// Park the live document: swap its state into its tab slot,
/// leaving a fresh placeholder document live. Afterwards
/// `app.parked` holds EVERY document in conceptual order (the
/// arrangement `crate::tabs` documents), ready for one to be
/// swapped back in or removed.
fn park_live(app: &mut App) {
    // The find bar's texts live in the child windows, not in the
    // state: capture them for the outgoing document first.
    // SAFETY: reading our own child windows' text.
    let find_box = unsafe { window_text(app.find_edit) };
    // SAFETY: as above.
    let replace_box = unsafe { window_text(app.replace_edit) };
    // SAFETY: as above.
    let find_count = unsafe { window_text(app.find_count) };
    let mut fresh = fresh_doc_state(app);
    app.swap_state(&mut fresh);
    // `fresh` now holds the document that was live; give it its
    // find-bar texts before it joins the parked list.
    fresh.find_box = find_box;
    fresh.replace_box = replace_box;
    fresh.find_count = find_count;
    let pos = app.tabs.park_position();
    app.parked.insert(pos, fresh);
}

/// Swap a parked document in as the live one (its previous
/// contents — a fresh placeholder — are dropped by the caller).
/// The document's parked find-bar texts go back into the
/// controls; everything else arrives with the state swap itself.
fn swap_in(app: &mut App, state: &mut DocState) {
    let find_box = state.find_box.clone();
    let replace_box = state.replace_box.clone();
    let find_count = state.find_count.clone();
    app.swap_state(state);
    set_control_text(app.find_edit, &find_box);
    set_control_text(app.replace_edit, &replace_box);
    set_control_text(app.find_count, &find_count);
}

/// Clear the find bar's control texts (a brand-new document has
/// no find state of its own yet).
fn clear_find_bar_texts(app: &mut App) {
    set_control_text(app.find_edit, "");
    set_control_text(app.replace_edit, "");
    set_control_text(app.find_count, "");
}

/// Reconnect the window chrome to the document that just became
/// active: the find bar and the change bar take the document's
/// own visibility, the title / status / menu checks refresh, the
/// hex layout re-syncs, interaction transients reset, and the
/// strip scrolls the active tab into view.
fn after_tab_switch(app: &mut App) {
    // The compare belongs to the window, not the tab (slice
    // R5c): switching tabs closes it.
    close_compare(app);
    // Multi-caret / rectangle / template state names the old
    // document's bytes (slice R5d): all collapse on a tab switch.
    clear_multi(app);
    app.tpl_open = false;
    app.tpl_result = None;
    app.tpl_selected = None;
    let find_show = if app.find_visible { SW_SHOW } else { SW_HIDE };
    for hwnd in app
        .find_controls
        .iter()
        .chain([&app.find_label, &app.replace_label])
    {
        if !hwnd.is_null() {
            // SAFETY: our child windows.
            unsafe { ShowWindow(*hwnd, find_show) };
        }
    }
    // The find scope toggle's label follows the document's state
    // (the draw pass adds the accent state while it is on).
    set_control_text(
        app.find_scope_check,
        if app.find_in_selection {
            "\u{2713} In selection"
        } else {
            "In selection"
        },
    );
    // The Regex toggle's label likewise (slice R5d).
    set_control_text(
        app.find_regex_check,
        if app.find_regex {
            "\u{2713} Regex"
        } else {
            "Regex"
        },
    );
    let change_show = if app.change_visible { SW_SHOW } else { SW_HIDE };
    for hwnd in [
        app.change_label,
        app.change_reload_btn,
        app.change_ignore_btn,
    ] {
        if !hwnd.is_null() {
            // SAFETY: our child windows.
            unsafe { ShowWindow(hwnd, change_show) };
        }
    }
    // If the incoming document has no find bar but the keyboard
    // focus is sitting in one of its (now hidden) controls, hand
    // the focus back to the window so keystrokes keep working.
    if !app.find_visible {
        // SAFETY: a plain focus query.
        let focus = unsafe { GetFocus() };
        if focus == app.find_edit || focus == app.replace_edit {
            // SAFETY: our window.
            unsafe { SetFocus(app.hwnd) };
        }
    }
    app.scroll_drag = None;
    app.dragging = false;
    app.hover_cand = None;
    hide_hover(app);
    app.tab_hover = None;
    app.tab_plus_hover = false;
    sync_hex_layout(app);
    refresh_title(app);
    refresh_status(app);
    refresh_menu_checks(app);
    ensure_active_tab_visible(app);
    layout_children(app);
    invalidate(app);
}

/// Make conceptual tab `index` the active document. Switching
/// preserves everything about both documents: each keeps its own
/// caret, scroll, mode, filter (and any in-flight filter or find
/// or index build, which keeps running against its own state),
/// undo history, and armed save options.
fn switch_tab(app: &mut App, index: usize) {
    if index >= app.tabs.count() || index == app.tabs.active() {
        return;
    }
    park_live(app);
    let mut state = app.parked.remove(index);
    swap_in(app, &mut state);
    drop(state);
    app.tabs.activate(index);
    after_tab_switch(app);
}

/// Open a fresh empty document in a new tab and switch to it.
fn new_tab(app: &mut App) {
    park_live(app);
    app.tabs.opened_appended();
    // The live fields already hold the fresh state park_live
    // swapped in — that IS the new document.
    clear_find_bar_texts(app);
    after_tab_switch(app);
}

/// Open `path` in a new tab and switch to it.
fn open_path_in_new_tab(app: &mut App, path: &Path) {
    park_live(app);
    app.tabs.opened_appended();
    clear_find_bar_texts(app);
    open_document(app, path);
    after_tab_switch(app);
}

/// The File ▸ Open rule with tabs: an untouched empty tab (no
/// file, no unsaved changes) is replaced in place; anything else
/// opens in a new tab, so opening a file never disturbs the
/// document in front of the user or asks about its changes.
fn open_path_from_ui(app: &mut App, path: &Path) {
    if app.editor.path().is_none() && !app.editor.is_dirty() {
        open_document(app, path);
    } else {
        open_path_in_new_tab(app, path);
    }
}

/// Close the active tab (its unsaved changes already dealt
/// with). When it is the only tab, a fresh empty document takes
/// its place in the same tab — the window never has zero
/// documents.
fn close_active_tab(app: &mut App) {
    if app.tabs.count() == 1 {
        let mut fresh = fresh_doc_state(app);
        app.swap_state(&mut fresh);
        drop(fresh);
        app.tabs.reset_single();
        clear_find_bar_texts(app);
        after_tab_switch(app);
        return;
    }
    let closing = app.tabs.active();
    // The replacement: the tab that slides into the closed tab's
    // place, or the previous tab when closing the last one.
    let replacement = if closing + 1 < app.tabs.count() {
        closing + 1
    } else {
        closing - 1
    };
    let pi = app
        .tabs
        .parked_index(replacement)
        .expect("the replacement tab is parked");
    let mut state = app.parked.remove(pi);
    swap_in(app, &mut state);
    drop(state); // the closed document's state
    app.tabs.closed(closing);
    after_tab_switch(app);
}

/// Close conceptual tab `index`, asking about unsaved changes
/// first (the existing prompt). A dirty parked tab is brought to
/// the front to ask about it. Returns false when the user
/// cancelled.
fn request_close_tab(app: &mut App, index: usize) -> bool {
    if index >= app.tabs.count() {
        return false;
    }
    if index != app.tabs.active() {
        if !tab_is_dirty(app, index) {
            let pi = app
                .tabs
                .parked_index(index)
                .expect("a parked tab has a parked index");
            app.parked.remove(pi);
            app.tabs.closed(index);
            invalidate(app);
            return true;
        }
        switch_tab(app, index);
    }
    if !prompt_save_if_dirty(app) {
        return false;
    }
    close_active_tab(app);
    true
}

/// Close every tab except the one identified by `keep_id`,
/// asking about each dirty one; a Cancel stops the sweep where
/// it stands. Ends on the kept tab.
fn close_other_tabs(app: &mut App, keep_id: u64) {
    while let Some(index) = (0..app.tabs.count()).find(|&i| tab_doc_id(app, i) != keep_id) {
        if !request_close_tab(app, index) {
            break;
        }
    }
    if let Some(keep) = tab_index_of_id(app, keep_id) {
        switch_tab(app, keep);
    }
}

/// Close all tabs (asking about each dirty one); the last one
/// leaves a fresh empty document behind, per the close rule.
fn close_all_tabs(app: &mut App) {
    loop {
        if app.tabs.count() == 1 {
            if app.editor.path().is_none() && !app.editor.is_dirty() {
                break; // already the fresh empty document
            }
            request_close_tab(app, app.tabs.active());
            break;
        }
        if !request_close_tab(app, 0) {
            break;
        }
    }
}

/// The window-close sweep: every dirty document in turn is
/// brought to the front and asked about (the existing prompt).
/// Cancel anywhere aborts the close. A document answered with
/// "Don't Save" stays dirty, so answered identities are
/// remembered and nobody is asked twice.
fn prompt_all_dirty(app: &mut App) -> bool {
    // The tab the user was on when they asked to close: the
    // sweep returns to it, so the session snapshot taken after
    // the sweep names the tab they actually left active.
    let start_id = app.doc_id;
    let mut answered: Vec<u64> = Vec::new();
    loop {
        let Some(index) = (0..app.tabs.count())
            .find(|&i| tab_is_dirty(app, i) && !answered.contains(&tab_doc_id(app, i)))
        else {
            if let Some(start) = tab_index_of_id(app, start_id) {
                switch_tab(app, start);
            }
            return true;
        };
        switch_tab(app, index);
        if !prompt_save_if_dirty(app) {
            return false;
        }
        answered.push(app.doc_id);
    }
}

/// The path of conceptual tab `index`'s document, if it has one.
fn tab_path(app: &App, index: usize) -> Option<PathBuf> {
    if index == app.tabs.active() {
        return app.editor.path().map(|p| p.to_path_buf());
    }
    let pi = app.tabs.parked_index(index)?;
    app.parked[pi].editor.path().map(|p| p.to_path_buf())
}

/// Whether conceptual tab `index`'s document has unsaved changes.
fn tab_is_dirty(app: &App, index: usize) -> bool {
    if index == app.tabs.active() {
        return app.editor.is_dirty();
    }
    match app.tabs.parked_index(index) {
        Some(pi) => app.parked[pi].editor.is_dirty(),
        None => false,
    }
}

/// Whether conceptual tab `index`'s file carries the read-only
/// attribute (the tab strip's hollow marker).
fn tab_is_read_only_file(app: &App, index: usize) -> bool {
    if index == app.tabs.active() {
        return app.doc_read_only_file;
    }
    match app.tabs.parked_index(index) {
        Some(pi) => app.parked[pi].doc_read_only_file,
        None => false,
    }
}

/// The stable identity of conceptual tab `index`'s document.
fn tab_doc_id(app: &App, index: usize) -> u64 {
    if index == app.tabs.active() {
        return app.doc_id;
    }
    match app.tabs.parked_index(index) {
        Some(pi) => app.parked[pi].id,
        None => 0,
    }
}

/// The conceptual index of the document with identity `id`.
fn tab_index_of_id(app: &App, id: u64) -> Option<usize> {
    (0..app.tabs.count()).find(|&i| tab_doc_id(app, i) == id)
}

/// The conceptual index of the tab showing `path` (compared as
/// the lossy string the session list stores), if any.
fn tab_index_with_path(app: &App, path: &str) -> Option<usize> {
    (0..app.tabs.count()).find(|&i| {
        tab_path(app, i).map(|p| p.to_string_lossy().into_owned()) == Some(path.to_string())
    })
}

/// A tab's display name: the file's name, or "Untitled".
fn tab_display_name(app: &App, index: usize) -> String {
    match tab_path(app, index) {
        Some(path) => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => "Untitled".to_string(),
    }
}

/// The tab's hover tooltip text: the full path (or "Untitled"),
/// with its state named.
fn tab_tooltip_text(app: &App, index: usize) -> String {
    let mut text = match tab_path(app, index) {
        Some(path) => path.display().to_string(),
        None => "Untitled".to_string(),
    };
    if tab_is_dirty(app, index) {
        text.push_str(" \u{00B7} unsaved changes");
    }
    if tab_is_read_only_file(app, index) {
        text.push_str(" \u{00B7} read-only");
    }
    text
}

/// Snapshot the session (the open tabs' paths and the active
/// index within them) into the settings, ready for the
/// close-time save. Documents without a path are not persisted.
fn snapshot_session(app: &mut App) {
    let docs: Vec<Option<String>> = (0..app.tabs.count())
        .map(|i| tab_path(app, i).map(|p| p.to_string_lossy().into_owned()))
        .collect();
    let (paths, active) = crate::tabs::session_from_docs(&docs, app.tabs.active());
    app.settings.session_tabs = paths;
    app.settings.session_active = active;
}

/// Reopen the tabs the last clean exit left behind: the saved
/// paths that still open, in order, with the saved active tab
/// restored. A document the startup already produced (an
/// accepted crash recovery) covers its own path and is not
/// opened twice; when the startup document is an untouched
/// empty tab, the first restored file takes its place. Missing
/// or unreadable files are skipped silently (the restore plan
/// probes each path by opening its mapping, which is also what
/// the open itself does first).
fn restore_session(app: &mut App) {
    let saved = app.settings.session_tabs.clone();
    if saved.is_empty() {
        return;
    }
    let (paths, want_active) =
        crate::tabs::plan_restore(&saved, app.settings.session_active, |p| {
            MmapFile::open(Path::new(p)).is_ok()
        });
    if paths.is_empty() {
        return;
    }
    let startup_path = app.editor.path().map(|p| p.to_string_lossy().into_owned());
    let startup_untouched = startup_path.is_none() && !app.editor.is_dirty();
    app.suppress_recovery_delete = true;
    let mut rest = paths.iter().filter(|p| Some(*p) != startup_path.as_ref());
    if startup_untouched {
        if let Some(first) = rest.next() {
            open_document(app, Path::new(first));
        }
    }
    for path in rest {
        open_path_in_new_tab(app, Path::new(path));
    }
    app.suppress_recovery_delete = false;
    if let Some(index) = tab_index_with_path(app, &paths[want_active]) {
        switch_tab(app, index);
    }
}

/// Start a separate VeloEdit process — File ▸ New Window, or
/// Open in New Window with a file. There is no single-instance
/// design in the program (no mutex, no forwarding): every launch
/// is its own process, so a new window is simply our own exe
/// started again, with the file as its argument when one is
/// given. The Explorer "Open in VeloEdit" entry and command-line
/// file arguments take the same route they always have — a fresh
/// process per invocation.
fn spawn_new_window(app: &mut App, path: Option<&Path>) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let verb = wstr("open");
    let file = wstr(&exe.to_string_lossy());
    let params = path.map(|p| wstr(&format!("\"{}\"", p.display())));
    // SAFETY: the strings are NUL-terminated and outlive the
    // call; a null parent and directory are permitted. A return
    // value of 32 or less is the documented failure signal.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params
                .as_ref()
                .map(|p| p.as_ptr())
                .unwrap_or(std::ptr::null()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result as usize) <= 32 {
        message_box(
            app.hwnd,
            "Windows did not open the new window.",
            "New Window",
            MB_OK | MB_ICONERROR,
        );
    }
}

// ---------------------------------------------------------------------------
// The tab strip: geometry, painting, and pointer handling. The
// strip is drawn on the canvas with the rest of the chrome (the
// toolbar family), between the accent strip and the toolbar.
// Paint and hit-testing share `tab_strip_layout`, so they can
// never disagree about where a tab is.
// ---------------------------------------------------------------------------

/// One tab's strip geometry (DIPs, client coordinates; `x` may be
/// negative while the strip is scrolled).
struct TabGeom {
    index: usize,
    x: f32,
    w: f32,
}

/// The "+" (new tab) button's width in the strip.
const TAB_PLUS_W: f32 = 26.0;
/// Horizontal gap between tabs, and the strip's edge padding.
const TAB_GAP: f32 = 4.0;

/// The strip's layout: every tab's position (in tab order, after
/// the scroll offset) and the "+" button's span. Tab widths come
/// from the tab names at the editor's character width, fitted by
/// `crate::tabs::fit_widths`; the "+" button is pinned at the
/// right edge when the tabs overflow.
/// The rendered width (DIPs) of a tab label in the app's text
/// format, measured with a real text layout. The estimate this
/// replaces (character count × the cell width from a ten-digit
/// probe) is wrong whenever the machine's actual font metrics
/// differ from the probe's — and the painter clips the label to
/// the tab's rect, so an under-estimate cost the last character
/// of every label in the field ("Untitled" painted as "Untitle").
/// Falls back to the estimate when measurement is unavailable.
fn tab_name_width(app: &App, name: &str) -> f32 {
    let layout = create_line_layout(app, name);
    if !layout.is_null() {
        let mut metrics = DWriteTextMetrics::default();
        // SAFETY: slot 60 is IDWriteTextLayout::GetMetrics per
        // dwrite.h; `metrics` is a writable struct of exactly the
        // header's layout; the layout is ours and released below.
        let get_metrics: unsafe extern "system" fn(*mut c_void, *mut DWriteTextMetrics) -> i32 =
            unsafe { vfn(layout, SLOT_LAYOUT_GET_METRICS) };
        // SAFETY: as above.
        let hr = unsafe { get_metrics(layout, &mut metrics) };
        // SAFETY: the layout is owned by us, released exactly once.
        unsafe { com_release(layout) };
        if hr >= 0 && metrics.width > 0.0 {
            return metrics.width.ceil();
        }
    }
    name.chars().count() as f32 * app.char_w
}

fn tab_strip_layout(app: &App) -> (Vec<TabGeom>, (f32, f32)) {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let width = app.dip(client.right - client.left);
    let count = app.tabs.count();
    let avail = (width - TAB_GAP * 2.0 - TAB_PLUS_W).max(TAB_MIN_W);
    let naturals: Vec<f32> = (0..count)
        .map(|i| {
            let name = tab_display_name(app, i);
            let text_w = tab_name_width(app, &name);
            // The label's measured width plus exactly the chrome
            // the painter spends (tabs::natural_width) — a smaller
            // budget clips the label's last character, because the
            // painter clips the text to the tab's rect.
            let markers =
                usize::from(tab_is_dirty(app, i)) + usize::from(tab_is_read_only_file(app, i));
            crate::tabs::natural_width(text_w, markers).clamp(TAB_MIN_W, TAB_MAX_W)
        })
        .collect();
    let widths = crate::tabs::fit_widths(&naturals, TAB_MIN_W, avail);
    let mut geoms = Vec::with_capacity(count);
    let mut x = TAB_GAP - app.tab_scroll_x;
    for (i, w) in widths.iter().enumerate() {
        geoms.push(TabGeom { index: i, x, w: *w });
        x += w + TAB_GAP;
    }
    let content_right = x - TAB_GAP + app.tab_scroll_x;
    let plus_x = if content_right + TAB_GAP + TAB_PLUS_W <= width - TAB_GAP {
        content_right + TAB_GAP
    } else {
        (width - TAB_GAP - TAB_PLUS_W).max(TAB_GAP)
    };
    (geoms, (plus_x, plus_x + TAB_PLUS_W))
}

/// The strip content's right edge in unscrolled coordinates
/// (the last tab's right edge) — the scroll range derives from
/// it.
fn tab_content_width(app: &App) -> f32 {
    let (geoms, _) = tab_strip_layout(app);
    match geoms.last() {
        Some(last) => last.x + last.w + app.tab_scroll_x,
        None => 0.0,
    }
}

/// Clamp the strip's scroll offset to the content.
fn clamp_tab_scroll(app: &mut App) {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let width = app.dip(client.right - client.left);
    let max_scroll = (tab_content_width(app) + TAB_GAP * 2.0 + TAB_PLUS_W - width).max(0.0);
    app.tab_scroll_x = app.tab_scroll_x.clamp(0.0, max_scroll);
}

/// Scroll the strip just enough that the active tab is fully
/// visible (called on every switch).
fn ensure_active_tab_visible(app: &mut App) {
    let (geoms, _) = tab_strip_layout(app);
    let Some(geom) = geoms.iter().find(|g| g.index == app.tabs.active()) else {
        return;
    };
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let width = app.dip(client.right - client.left);
    let view_right = width - TAB_PLUS_W - TAB_GAP * 2.0;
    if geom.x < TAB_GAP {
        app.tab_scroll_x -= TAB_GAP - geom.x;
    } else if geom.x + geom.w > view_right {
        app.tab_scroll_x += geom.x + geom.w - view_right;
    }
    clamp_tab_scroll(app);
}

/// What a point in the tab strip hits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TabHit {
    Tab(usize),
    Close(usize),
    New,
    None,
}

/// Hit-test a point (DIPs) against the strip. The close × is
/// live only on the active or hovered tab — exactly the tabs it
/// is painted on.
fn tab_hit(app: &App, x: f32, y: f32) -> TabHit {
    if y < STRIP_H || y >= STRIP_H + TABBAR_H {
        return TabHit::None;
    }
    let (geoms, plus) = tab_strip_layout(app);
    if x >= plus.0 && x < plus.1 {
        return TabHit::New;
    }
    for geom in &geoms {
        if x >= geom.x && x < geom.x + geom.w {
            let close_shown = geom.index == app.tabs.active() || app.tab_hover == Some(geom.index);
            if close_shown && x >= geom.x + geom.w - TAB_CLOSE_W - 2.0 {
                return TabHit::Close(geom.index);
            }
            return TabHit::Tab(geom.index);
        }
    }
    TabHit::None
}

/// The drop position for a tab drag: the final index the dragged
/// tab would take if released at DIP x, and the x to paint the
/// insertion marker at. Computed over the OTHER tabs closing
/// ranks, so dropping a tab in its own neighbourhood is a no-op.
fn tab_drop_at(app: &App, drag_index: usize, x: f32) -> (usize, f32) {
    let (geoms, _) = tab_strip_layout(app);
    let mut cx = TAB_GAP - app.tab_scroll_x;
    let mut slot = 0;
    for geom in &geoms {
        if geom.index == drag_index {
            continue;
        }
        if x < cx + geom.w / 2.0 {
            return (slot, cx - TAB_GAP / 2.0);
        }
        cx += geom.w + TAB_GAP;
        slot += 1;
    }
    (slot, cx - TAB_GAP / 2.0)
}

/// Move a tab by drag: reorder the parked list and the order
/// bookkeeping together, keeping the same document active (the
/// active marker follows its document).
fn move_tab(app: &mut App, from: usize, to: usize) {
    if from >= app.tabs.count() || to >= app.tabs.count() || from == to {
        return;
    }
    park_live(app);
    // The parked list is now the full conceptual list.
    let state = app.parked.remove(from);
    app.parked.insert(to, state);
    app.tabs.moved(from, to);
    let active = app.tabs.active();
    let mut state = app.parked.remove(active);
    swap_in(app, &mut state);
    drop(state);
    after_tab_switch(app);
}

/// Left button down in the strip: switch (and arm a drag), close
/// on the ×, or open a new tab on "+". Returns true when the
/// point was the strip's to handle.
fn tab_mouse_down(app: &mut App, lparam: LPARAM) -> bool {
    let (x_px, y_px) = mouse_xy(lparam);
    let (x, y) = (app.dip(x_px), app.dip(y_px));
    if y < STRIP_H || y >= STRIP_H + TABBAR_H {
        return false;
    }
    // The hover state must reflect this exact point before the
    // close-box test (the × is live only where it is painted).
    app.tab_hover = match tab_hit(app, x, y) {
        TabHit::Tab(i) | TabHit::Close(i) => Some(i),
        _ => None,
    };
    match tab_hit(app, x, y) {
        TabHit::Close(i) => {
            request_close_tab(app, i);
            true
        }
        TabHit::New => {
            new_tab(app);
            true
        }
        TabHit::Tab(i) => {
            switch_tab(app, i);
            app.tab_drag = Some(TabDrag {
                index: i,
                start_x: x,
                x,
                moved: false,
            });
            // SAFETY: capture the pointer for the drag.
            unsafe { SetCapture(app.hwnd) };
            true
        }
        TabHit::None => false,
    }
}

/// Middle button down in the strip closes the tab under the
/// pointer (the browser habit).
fn tab_middle_down(app: &mut App, lparam: LPARAM) -> bool {
    let (x_px, y_px) = mouse_xy(lparam);
    let (x, y) = (app.dip(x_px), app.dip(y_px));
    match tab_hit(app, x, y) {
        TabHit::Tab(i) | TabHit::Close(i) => {
            request_close_tab(app, i);
            true
        }
        _ => false,
    }
}

/// Right button up in the strip: the tab's context menu (Close,
/// Close Others, Close All, Copy Path, Open in New Window). The
/// commands arrive through WM_COMMAND; `app.tab_context` names
/// the tab they act on.
fn tab_context_menu(app: &mut App, lparam: LPARAM) -> bool {
    let (x_px, y_px) = mouse_xy(lparam);
    let (x, y) = (app.dip(x_px), app.dip(y_px));
    let index = match tab_hit(app, x, y) {
        TabHit::Tab(i) | TabHit::Close(i) => i,
        _ => return false,
    };
    app.tab_context = Some(index);
    // SAFETY: building and showing a popup menu of our own; it is
    // destroyed after the call.
    unsafe {
        let menu = CreatePopupMenu();
        menu_item(menu, CMD_TAB_CLOSE, "Close");
        menu_item(menu, CMD_TAB_CLOSE_OTHERS, "Close Others");
        menu_item(menu, CMD_TAB_CLOSE_ALL, "Close All");
        menu_sep(menu);
        menu_item(menu, CMD_TAB_COPY_PATH, "Copy Path");
        menu_item(menu, CMD_TAB_NEW_WINDOW, "Open in New Window");
        if tab_path(app, index).is_none() {
            EnableMenuItem(menu, CMD_TAB_COPY_PATH as u32, MF_BYCOMMAND | MF_GRAYED);
            EnableMenuItem(menu, CMD_TAB_NEW_WINDOW as u32, MF_BYCOMMAND | MF_GRAYED);
        }
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            app.hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
    }
    true
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
        // Under a user-chosen code page (slice R4c, item 29) the
        // save pipeline sees a unit-width-1, BOM-less encoding:
        // the document bytes already ARE page bytes (the codec
        // only translates at the display/typing edges), so the
        // streamed save is a pass-through either way — what the
        // page changes is how text converts, not how bytes move.
        encoding: match app.editor.code_page() {
            Some(_) => encoding::TextEncoding::Ansi,
            None => app.editor.encoding(),
        },
        eol: app.save_eol,
        bom: app.save_bom,
        fast_clone: app.settings.fast_save_clone,
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

/// Block-clone fast save (slice R4c, item 4): when the owner
/// turned the setting on but the clone path could not serve this
/// save, the status bar says so once. The save itself used the
/// normal streamed path and is fully correct either way — the
/// note is information, never an error.
fn note_fast_clone_fallback(app: &mut App, report: &crate::save::SaveReport) {
    if app.settings.fast_save_clone && !report.fast_clone_used {
        app.status_note =
            Some("Fast save was not available for this file; the normal save was used".to_string());
    }
}

/// After a successful save (slice R5c): a save with an armed
/// line-ending or byte-order-mark conversion rewrites the
/// document's bytes, which stales an open compare's computed
/// differences exactly like an edit does. (A plain save changes
/// nothing in the document, so its regions stay valid.)
fn note_saved(app: &mut App) {
    if app.compare.is_some()
        && (app.save_eol != crate::save::EolChoice::Keep
            || app.save_bom != crate::save::BomChoice::Keep)
    {
        if let Some(cmp) = &mut app.compare {
            cmp.diff_stale = true;
            cmp.diff_gen += 1;
            cmp.diff_computing = false;
        }
    }
}

fn do_save(app: &mut App) -> bool {
    if app.editor.path().is_none() {
        return do_save_as(app);
    }
    // Protected file (slice R4c, item 33): the file on disk is
    // marked read-only, so saving over it would fail with a raw
    // OS error. Explain in plain words and offer a copy instead.
    if app.doc_read_only_file {
        let answer = message_box(
            app.hwnd,
            "This file is marked read-only, so VeloEdit cannot save over it.\n\nSave a copy under a different name instead?",
            "VeloEdit",
            MB_YESNO | MB_ICONINFORMATION,
        );
        if answer == IDYES {
            return do_save_as(app);
        }
        return false;
    }
    // Roadmap item 34: surface an external change before we
    // overwrite the file. The notice is non-modal — the save still
    // proceeds; the bar simply stays visible until the save lands
    // (after_save_tracking then retires it).
    check_external_change(app);
    match app.editor.save_with_options(&gui_save_options(app)) {
        Ok(report) => {
            delete_recovery_files();
            app.last_edit = None;
            after_save_tracking(app, false);
            note_fast_clone_fallback(app, &report);
            note_saved(app);
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
        Ok(report) => {
            delete_recovery_files();
            app.last_edit = None;
            after_save_tracking(app, true);
            note_fast_clone_fallback(app, &report);
            note_saved(app);
            let path_string = path.to_string_lossy().into_owned();
            app.settings.add_recent(&path_string);
            publish_jump_list(app);
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
// Slice R4c handlers: bookmark annotations, Extract Ranges, delta
// patches, Insert Zeroes, Compare With, Open as Administrator.
// ---------------------------------------------------------------------------

/// Owner-drawn button paint shared by the slice R4c dialogs (the
/// older dialog procs carry their own copies; the new ones use
/// this). `lparam` is the WM_DRAWITEM payload.
fn draw_dialog_button(app: &App, lparam: LPARAM) -> LRESULT {
    // SAFETY: lparam is a DRAWITEMSTRUCT for one of our buttons.
    let dis = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
    let mut rect = dis.rcItem;
    // SAFETY: GDI drawing on the button's own DC.
    unsafe {
        FillRect(dis.hDC, &rect, app.panel_brush);
        FrameRect(dis.hDC, &rect, app.line_brush);
        if dis.itemState & 0x10 != 0 {
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

/// The session bookmark sitting on document line `line`, if any
/// (slice R4c, item 5).
fn bookmark_on_line(app: &mut App, line: u64) -> Option<u64> {
    let offsets: Vec<u64> = app.bookmarks.iter().map(|b| b.offset).collect();
    offsets
        .into_iter()
        .find(|&offset| app.editor.line_col_of(offset).0 == line)
}

/// Right-click in the text gutter (slice R4c, item 5): when the
/// click lands on a line that carries a bookmark, open that
/// bookmark's annotation editor. Returns true when a dialog
/// opened (the click is consumed); anything else falls through
/// to the default right-click behaviour.
fn gutter_bookmark_click(app: &mut App, lparam: LPARAM) -> bool {
    if app.filter.is_some() || app.editor.mode() != EditorMode::Text {
        return false;
    }
    let (x_px, y_px) = mouse_xy(lparam);
    let layout = compute_layout(app);
    if app.dip(x_px) >= layout.gutter_w {
        return false;
    }
    let offset = pos_to_offset(app, x_px, y_px);
    let line = app.editor.line_col_of(offset).0;
    match bookmark_on_line(app, line) {
        Some(bookmark_offset) => {
            edit_bookmark_dialog(app, bookmark_offset);
            true
        }
        None => false,
    }
}

// ---- Bookmark annotation editor (slice R4c, item 5) ----------

const BMEDIT_CLASS: &str = "VeloEditBookmarkEdit";
const ID_BMEDIT_NOTE: usize = 5501;
const ID_BMEDIT_COLOR_BASE: usize = 5502; // +0 = Default, +1..=+6 = swatches
const ID_BMEDIT_OK: usize = 5509;
const ID_BMEDIT_CANCEL: usize = 5510;

struct BookmarkEditState {
    app: *mut App,
    note_edit: HWND,
    /// -1 = the default (theme accent); 0..=5 = a swatch index.
    chosen: i32,
    result: Option<(Option<u8>, String)>,
}

unsafe extern "system" fn bookmark_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's BookmarkEditState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut BookmarkEditState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (caller's frame).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        let place = |child: HWND, x: f32, y: f32, w: f32, h: f32| {
            // SAFETY: reparent and position our fresh child window.
            unsafe {
                SetParent(child, hwnd);
                MoveWindow(child, px(x), px(y), px(w), px(h), 1);
            }
        };
        let label = create_child(app, "Static", "Colour:", WS_CHILD | WS_VISIBLE, 0, 0);
        place(label, 12.0, 12.0, 404.0, 18.0);
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW
        let mut names: Vec<String> = vec!["Default".to_string()];
        for (name, _) in crate::bookmarks::BOOKMARK_SWATCHES {
            names.push(name.to_string());
        }
        for (i, name) in names.iter().enumerate() {
            let button = create_child(
                app,
                "Button",
                name,
                button_style,
                0,
                ID_BMEDIT_COLOR_BASE + i,
            );
            let row = (i / 4) as f32;
            let col = (i % 4) as f32;
            place(button, 12.0 + col * 104.0, 34.0 + row * 30.0, 98.0, 24.0);
        }
        let note_label = create_child(app, "Static", "Note:", WS_CHILD | WS_VISIBLE, 0, 0);
        place(note_label, 12.0, 100.0, 404.0, 18.0);
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        let edit = create_child(
            app,
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_BMEDIT_NOTE,
        );
        place(edit, 12.0, 122.0, 404.0, 24.0);
        state.note_edit = edit;
        let ok = create_child(app, "Button", "OK", button_style, 0, ID_BMEDIT_OK);
        let cancel = create_child(app, "Button", "Cancel", button_style, 0, ID_BMEDIT_CANCEL);
        place(ok, 268.0, 158.0, 70.0, 24.0);
        place(cancel, 346.0, 158.0, 70.0, 24.0);
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut BookmarkEditState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            if (ID_BMEDIT_COLOR_BASE..ID_BMEDIT_COLOR_BASE + 7).contains(&id) {
                state.chosen = (id - ID_BMEDIT_COLOR_BASE) as i32 - 1;
                // SAFETY: repaint so the selection frame moves.
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 1) };
            } else if id == ID_BMEDIT_OK {
                let color = if state.chosen < 0 {
                    None
                } else {
                    Some(state.chosen as u8)
                };
                // SAFETY: our edit control.
                let note = unsafe { window_text(state.note_edit) };
                state.result = Some((color, note.trim().to_string()));
                // SAFETY: our dialog window.
                unsafe { DestroyWindow(hwnd) };
            } else if id == ID_BMEDIT_CANCEL {
                // SAFETY: our dialog window.
                unsafe { DestroyWindow(hwnd) };
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: closing is cancelling.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DRAWITEM => {
            // SAFETY: lparam is a DRAWITEMSTRUCT for one of our buttons.
            let dis = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
            let id = dis.CtlID as usize;
            if (ID_BMEDIT_COLOR_BASE..ID_BMEDIT_COLOR_BASE + 7).contains(&id) {
                // SAFETY: the App is alive (modal dialog, UI thread).
                let app = unsafe { &*state.app };
                let rect = dis.rcItem;
                let index = id - ID_BMEDIT_COLOR_BASE;
                let selected = state.chosen == index as i32 - 1;
                // SAFETY: GDI drawing on the button's own DC.
                unsafe {
                    FillRect(dis.hDC, &rect, app.panel_brush);
                    FrameRect(dis.hDC, &rect, app.line_brush);
                    if selected {
                        let mut inner = rect;
                        inner.left += 2;
                        inner.top += 2;
                        inner.right -= 2;
                        inner.bottom -= 2;
                        FrameRect(dis.hDC, &inner, app.accent_brush);
                    }
                    // The colour block: the swatch itself, or the
                    // theme accent for "Default".
                    let rgb = if index == 0 {
                        app.theme.accent
                    } else {
                        let (r, g, b) = crate::bookmarks::BOOKMARK_SWATCHES[index - 1].1;
                        crate::theme::Rgba::rgb(r, g, b)
                    };
                    let swatch_brush = CreateSolidBrush(colorref(rgb));
                    let mut block = rect;
                    block.left += 5;
                    block.top += 5;
                    block.right = block.left + 16;
                    block.bottom -= 5;
                    FillRect(dis.hDC, &block, swatch_brush);
                    DeleteObject(swatch_brush);
                    SetBkMode(dis.hDC, TRANSPARENT as i32);
                    SetTextColor(dis.hDC, colorref(app.theme.text));
                    let text = window_text(dis.hwndItem);
                    let mut wide: Vec<u16> = text.encode_utf16().collect();
                    let mut text_rect = rect;
                    text_rect.left += 26;
                    if !wide.is_empty() {
                        DrawTextW(
                            dis.hDC,
                            wide.as_mut_ptr(),
                            wide.len() as i32,
                            &mut text_rect,
                            0x4 | 0x20,
                        );
                    }
                }
                1
            } else {
                // SAFETY: the App is alive (modal dialog, UI thread).
                let app = unsafe { &*state.app };
                draw_dialog_button(app, lparam)
            }
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Show the bookmark annotation editor for the bookmark at
/// `offset`, prefilled with its current colour and note. Returns
/// the confirmed (colour, note), or `None` on cancel.
fn show_bookmark_edit_dialog(
    app: &mut App,
    bookmark: &crate::bookmarks::Bookmark,
) -> Option<(Option<u8>, String)> {
    let mut state = BookmarkEditState {
        app: app as *mut App,
        note_edit: std::ptr::null_mut(),
        chosen: bookmark.color.map(|c| c as i32).unwrap_or(-1),
        result: None,
    };
    let note_prefill = bookmark.note.clone();
    let class = wstr(BMEDIT_CLASS);
    let title = wstr("Edit Bookmark");
    // SAFETY: the class is registered in run_gui; the state
    // travels in lpCreateParams and lives in this stack frame
    // until the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(440.0),
            app.px(228.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut BookmarkEditState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: prefill the note and focus it; the edit was created
    // in WM_CREATE.
    unsafe {
        let wide = wstr(&note_prefill);
        SetWindowTextW(state.note_edit, wide.as_ptr());
        SetFocus(state.note_edit);
        SendMessageW(state.note_edit, 0xB1, 0, -1isize); // EM_SETSEL all
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error.
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

/// Open the annotation editor for the bookmark at `offset` and,
/// on confirm, apply it to the session list and the central
/// store (slice R4c, item 5 — the store is the one portable
/// `data/bookmarks.json`; no sidecar files, by decision).
fn edit_bookmark_dialog(app: &mut App, offset: u64) {
    let Some(bookmark) = app.bookmarks.get(offset).cloned() else {
        return;
    };
    let Some((color, note)) = show_bookmark_edit_dialog(app, &bookmark) else {
        return;
    };
    app.bookmarks.set_annotation(offset, color, &note);
    if let (Some(path), Some(stamp)) = (
        app.editor.path().map(|p| p.to_string_lossy().into_owned()),
        app.doc_stamp,
    ) {
        if app
            .bookmark_store
            .set_annotation(&path, stamp, offset, color, &note)
        {
            let _ = app
                .bookmark_store
                .save_to(&crate::bookmarks::bookmarks_path());
            load_session_bookmarks(app);
        }
    }
    invalidate(app);
}

// ---- Extract Ranges dialog (slice R4c, item 2) ---------------

const SLICE_CLASS: &str = "VeloEditSlice";
const ID_SLICE_EDIT: usize = 5401;
const ID_SLICE_OK: usize = 5402;
const ID_SLICE_CANCEL: usize = 5403;

struct SliceDialogState {
    app: *mut App,
    edit: HWND,
    result: Option<String>,
}

const PROP_SLICE_OLD: &str = "VeloEdit.SliceOldProc";
const PROP_SLICE_DLG: &str = "VeloEdit.SliceDlg";

/// Subclass the slice dialog's edit so Enter confirms and Esc
/// cancels — the same property-based trick as the prompt dialog.
fn subclass_slice_edit(edit: HWND, dlg: HWND) {
    let name_old = wstr(PROP_SLICE_OLD);
    let name_dlg = wstr(PROP_SLICE_DLG);
    // SAFETY: `edit` is our child window; the old proc is stored
    // in a window property before the new proc is installed.
    unsafe {
        let old = SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            slice_edit_proc as *const () as usize as isize,
        );
        SetPropW(edit, name_old.as_ptr(), old as HANDLE);
        SetPropW(edit, name_dlg.as_ptr(), dlg as HANDLE);
    }
}

unsafe extern "system" fn slice_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let name_old = wstr(PROP_SLICE_OLD);
    let name_dlg = wstr(PROP_SLICE_DLG);
    // SAFETY: the props were stored by subclass_slice_edit before
    // this proc was installed, and are removed at WM_NCDESTROY.
    let old = unsafe { GetPropW(hwnd, name_old.as_ptr()) } as isize;
    let dlg = unsafe { GetPropW(hwnd, name_dlg.as_ptr()) } as HWND;
    if msg == WM_KEYDOWN {
        let cmd = match wparam as u32 {
            0x0D => Some(ID_SLICE_OK),     // Enter: confirm
            0x1B => Some(ID_SLICE_CANCEL), // Esc: cancel
            _ => None,
        };
        if let Some(cmd) = cmd {
            // SAFETY: `dlg` is the slice dialog stored at subclass
            // time; the command is handled there.
            unsafe { PostMessageW(dlg, WM_COMMAND, cmd, 0) };
            return 0;
        }
    }
    if msg == WM_NCDESTROY {
        // SAFETY: restore the original proc and drop our props.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
            RemovePropW(hwnd, name_old.as_ptr());
            RemovePropW(hwnd, name_dlg.as_ptr());
        }
    }
    let proc_fn: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: proc_fn is the edit control's original window
    // procedure, retrieved from our own property above.
    unsafe { CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn slice_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's SliceDialogState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut SliceDialogState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (caller's frame).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        let place = |child: HWND, x: f32, y: f32, w: f32, h: f32| {
            // SAFETY: reparent and position our fresh child window.
            unsafe {
                SetParent(child, hwnd);
                MoveWindow(child, px(x), px(y), px(w), px(h), 1);
            }
        };
        let hint = create_child(
            app,
            "Static",
            "Ranges to extract, in ascending order, separated by ;\nByte ranges count both ends in: 0-1023  ·  0x1000-0x1FFF\nLine ranges take whole lines with endings: L10-L20",
            WS_CHILD | WS_VISIBLE,
            0,
            0,
        );
        place(hint, 12.0, 10.0, 424.0, 48.0);
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        let edit = create_child(app, "Edit", "", edit_style, WS_EX_CLIENTEDGE, ID_SLICE_EDIT);
        place(edit, 12.0, 62.0, 424.0, 24.0);
        state.edit = edit;
        subclass_slice_edit(edit, hwnd);
        // SAFETY: focus the spec field.
        unsafe { SetFocus(edit) };
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW
        let ok = create_child(app, "Button", "Extract", button_style, 0, ID_SLICE_OK);
        let cancel = create_child(app, "Button", "Cancel", button_style, 0, ID_SLICE_CANCEL);
        place(ok, 268.0, 96.0, 80.0, 24.0);
        place(cancel, 356.0, 96.0, 80.0, 24.0);
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut SliceDialogState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_COMMAND => {
            match wparam & 0xFFFF {
                ID_SLICE_OK => {
                    // SAFETY: our edit control.
                    state.result = Some(unsafe { window_text(state.edit) });
                    // SAFETY: our dialog window.
                    unsafe { DestroyWindow(hwnd) };
                }
                ID_SLICE_CANCEL => {
                    // SAFETY: our dialog window.
                    unsafe { DestroyWindow(hwnd) };
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: closing is cancelling.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DRAWITEM => {
            // SAFETY: the App is alive (modal dialog, UI thread).
            let app = unsafe { &*state.app };
            draw_dialog_button(app, lparam)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Show the Extract Ranges spec dialog (the Split dialog's modal
/// pattern). Returns the confirmed spec text, or `None` on cancel.
fn show_slice_dialog(app: &mut App) -> Option<String> {
    let mut state = SliceDialogState {
        app: app as *mut App,
        edit: std::ptr::null_mut(),
        result: None,
    };
    let class = wstr(SLICE_CLASS);
    let title = wstr("Extract Ranges");
    // SAFETY: the slice class is registered in run_gui; the state
    // travels in lpCreateParams and lives in this stack frame
    // until the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(460.0),
            app.px(168.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut SliceDialogState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: modal behaviour, exactly like the Split dialog.
    unsafe {
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error.
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

/// The `data/templates/` folder beside the program (roadmap item
/// 17, slice R5d): the user's binary-template files live here.
fn templates_dir() -> PathBuf {
    crate::settings::data_dir().join("templates")
}

/// The template files on offer: every `.txt` file in
/// [`templates_dir`], sorted by path.
fn template_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(templates_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_txt = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("txt"))
                .unwrap_or(false);
            if is_txt {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Parse an offset typed into a dialog: decimal, or hex with a
/// `0x` prefix.
fn parse_offset_text(text: &str) -> Option<u64> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>().ok()
    }
}

const TPLDLG_CLASS: &str = "VeloEditTemplate";
const ID_TPL_FILES: usize = 5501;
const ID_TPL_STRUCTS: usize = 5502;
const ID_TPL_OFFSET: usize = 5503;
const ID_TPL_APPLY: usize = 5504;
const ID_TPL_CANCEL: usize = 5505;

// ListBox messages and the selection-change notification
// (winuser.h), written literally like the other control constants.
const LB_ADDSTRING: u32 = 0x0180;
const LB_RESETCONTENT: u32 = 0x0184;
const LB_SETCURSEL: u32 = 0x018E;
const LB_GETCURSEL: u32 = 0x0188;
const LBN_SELCHANGE: usize = 1;

struct TemplateDialogState {
    app: *mut App,
    files: Vec<PathBuf>,
    set: Option<crate::templates::TemplateSet>,
    file_list: HWND,
    struct_list: HWND,
    offset_edit: HWND,
    error_label: HWND,
}

const PROP_TPL_OLD: &str = "VeloEdit.TplOldProc";
const PROP_TPL_DLG: &str = "VeloEdit.TplDlg";

/// Subclass the offset edit so Enter applies and Esc cancels —
/// the slice dialog's property-based trick.
fn subclass_tpl_edit(edit: HWND, dlg: HWND) {
    let name_old = wstr(PROP_TPL_OLD);
    let name_dlg = wstr(PROP_TPL_DLG);
    // SAFETY: `edit` is our child window; the old proc is stored
    // in a window property before the new proc is installed.
    unsafe {
        let old = SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            tpl_edit_proc as *const () as usize as isize,
        );
        SetPropW(edit, name_old.as_ptr(), old as HANDLE);
        SetPropW(edit, name_dlg.as_ptr(), dlg as HANDLE);
    }
}

unsafe extern "system" fn tpl_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let name_old = wstr(PROP_TPL_OLD);
    let name_dlg = wstr(PROP_TPL_DLG);
    // SAFETY: the props were stored by subclass_tpl_edit before
    // this proc was installed, and are removed at WM_NCDESTROY.
    let old = unsafe { GetPropW(hwnd, name_old.as_ptr()) } as isize;
    let dlg = unsafe { GetPropW(hwnd, name_dlg.as_ptr()) } as HWND;
    if msg == WM_KEYDOWN {
        let cmd = match wparam as u32 {
            0x0D => Some(ID_TPL_APPLY),  // Enter: apply
            0x1B => Some(ID_TPL_CANCEL), // Esc: cancel
            _ => None,
        };
        if let Some(cmd) = cmd {
            // SAFETY: `dlg` is the template dialog stored at
            // subclass time; the command is handled there.
            unsafe { PostMessageW(dlg, WM_COMMAND, cmd, 0) };
            return 0;
        }
    }
    if msg == WM_NCDESTROY {
        // SAFETY: restore the original proc and drop our props.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
            RemovePropW(hwnd, name_old.as_ptr());
            RemovePropW(hwnd, name_dlg.as_ptr());
        }
    }
    let proc_fn: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: proc_fn is the edit control's original window
    // procedure, retrieved from our own property above.
    unsafe { CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam) }
}

/// Send a string to a listbox (its lifetime only needs to cover
/// the call — the list copies it).
fn listbox_add(hwnd: HWND, text: &str) {
    let wide = wstr(text);
    // SAFETY: our listbox; the string outlives the call.
    unsafe { SendMessageW(hwnd, LB_ADDSTRING, 0, wide.as_ptr() as LPARAM) };
}

fn listbox_clear(hwnd: HWND) {
    // SAFETY: our listbox.
    unsafe { SendMessageW(hwnd, LB_RESETCONTENT, 0, 0) };
}

fn listbox_select(hwnd: HWND, index: usize) {
    // SAFETY: our listbox.
    unsafe { SendMessageW(hwnd, LB_SETCURSEL, index, 0) };
}

fn listbox_current(hwnd: HWND) -> Option<usize> {
    // SAFETY: our listbox; LB_ERR (-1) means no selection.
    let sel = unsafe { SendMessageW(hwnd, LB_GETCURSEL, 0, 0) };
    if sel < 0 {
        None
    } else {
        Some(sel as usize)
    }
}

/// Load the template file at `index` in the dialog: parse it, and
/// either fill the struct list or show the parse error verbatim.
fn tpl_dialog_load(state: &mut TemplateDialogState, index: usize) {
    listbox_clear(state.struct_list);
    state.set = None;
    let Some(path) = state.files.get(index) else {
        return;
    };
    let parsed = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))
        .and_then(|text| {
            crate::templates::TemplateSet::parse(&text).map_err(|err| format!("{err}"))
        });
    match parsed {
        Ok(set) => {
            for name in set.struct_names() {
                listbox_add(state.struct_list, name);
            }
            if set.struct_names().next().is_some() {
                listbox_select(state.struct_list, 0);
            }
            state.set = Some(set);
            set_control_text(state.error_label, "");
        }
        Err(message) => {
            set_control_text(state.error_label, &message);
        }
    }
}

/// Apply the dialog's chosen struct at the chosen offset: the
/// results land in the docked pane and the dialog closes. An
/// apply error shows verbatim and the dialog stays open.
fn tpl_dialog_apply(state: &mut TemplateDialogState, hwnd: HWND) {
    // SAFETY: the App is alive and exclusively ours on this thread
    // (modal dialog).
    let app = unsafe { &mut *state.app };
    let Some(set) = &state.set else {
        set_control_text(state.error_label, "Pick a template file first.");
        return;
    };
    let Some(sel) = listbox_current(state.struct_list) else {
        set_control_text(state.error_label, "Pick a struct first.");
        return;
    };
    let name = set.struct_names().nth(sel).unwrap_or_default().to_string();
    // SAFETY: our edit control.
    let offset_text = unsafe { window_text(state.offset_edit) };
    let Some(offset) = parse_offset_text(&offset_text) else {
        set_control_text(
            state.error_label,
            "The offset is not a number — decimal, or hex like 0x40.",
        );
        return;
    };
    match crate::op_apply_template(app.editor.table(), set, &name, offset) {
        Ok(applied) => {
            let file_label = state
                .files
                .get(listbox_current(state.file_list).unwrap_or(0))
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            app.tpl_title = format!(
                "Template: {name} \u{00B7} {file_label} \u{00B7} at 0x{offset:X} \u{00B7} {} fields",
                applied.fields.len()
            );
            app.tpl_result = Some(applied);
            app.tpl_open = true;
            app.tpl_first = 0;
            app.tpl_selected = None;
            // SAFETY: our dialog window.
            unsafe { DestroyWindow(hwnd) };
        }
        Err(err) => {
            set_control_text(state.error_label, &format!("{err}"));
        }
    }
}

unsafe extern "system" fn template_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's TemplateDialogState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut TemplateDialogState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (caller's frame).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        let place = |child: HWND, x: f32, y: f32, w: f32, h: f32| {
            // SAFETY: reparent and position our fresh child window.
            unsafe {
                SetParent(child, hwnd);
                MoveWindow(child, px(x), px(y), px(w), px(h), 1);
            }
        };
        let hint_text = if state.files.is_empty() {
            format!(
                "No template files found.\nCreate the folder {} and copy .txt template files into it \u{2014} the program's examples/templates folder has two to start with.",
                templates_dir().display()
            )
        } else {
            "Pick a template file and one of its structs, set the offset, then Apply.\nThe fields appear in a list under the editor; double-click one to jump to it."
                .to_string()
        };
        let hint = create_child(app, "Static", &hint_text, WS_CHILD | WS_VISIBLE, 0, 0);
        place(hint, 12.0, 10.0, 446.0, 34.0);
        // LBS_NOTIFY (0x1) so selection changes reach WM_COMMAND;
        // WS_VSCROLL (0x100000) for long lists.
        let list_style = WS_CHILD | WS_VISIBLE | 0x1 | 0x100000;
        state.file_list = create_child(
            app,
            "ListBox",
            "",
            list_style,
            WS_EX_CLIENTEDGE,
            ID_TPL_FILES,
        );
        place(state.file_list, 12.0, 48.0, 218.0, 150.0);
        state.struct_list = create_child(
            app,
            "ListBox",
            "",
            list_style,
            WS_EX_CLIENTEDGE,
            ID_TPL_STRUCTS,
        );
        place(state.struct_list, 240.0, 48.0, 218.0, 150.0);
        let offset_label = create_child(
            app,
            "Static",
            "Apply at offset:",
            WS_CHILD | WS_VISIBLE,
            0,
            0,
        );
        place(offset_label, 12.0, 206.0, 110.0, 20.0);
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        state.offset_edit = create_child(
            app,
            "Edit",
            &app.editor.caret().to_string(),
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_TPL_OFFSET,
        );
        place(state.offset_edit, 124.0, 204.0, 140.0, 24.0);
        subclass_tpl_edit(state.offset_edit, hwnd);
        state.error_label = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE, 0, 0);
        place(state.error_label, 12.0, 234.0, 446.0, 34.0);
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW
        let apply_btn = create_child(app, "Button", "Apply", button_style, 0, ID_TPL_APPLY);
        let cancel_btn = create_child(app, "Button", "Cancel", button_style, 0, ID_TPL_CANCEL);
        place(apply_btn, 290.0, 276.0, 80.0, 24.0);
        place(cancel_btn, 378.0, 276.0, 80.0, 24.0);
        for path in &state.files {
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            listbox_add(state.file_list, &label);
        }
        if !state.files.is_empty() {
            listbox_select(state.file_list, 0);
            tpl_dialog_load(state, 0);
        }
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TemplateDialogState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let notify = (wparam >> 16) & 0xFFFF;
            match id {
                ID_TPL_FILES if notify == LBN_SELCHANGE => {
                    if let Some(sel) = listbox_current(state.file_list) {
                        tpl_dialog_load(state, sel);
                    }
                }
                ID_TPL_APPLY => tpl_dialog_apply(state, hwnd),
                ID_TPL_CANCEL => {
                    // SAFETY: our dialog window.
                    unsafe { DestroyWindow(hwnd) };
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: closing is cancelling.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DRAWITEM => {
            // SAFETY: the App is alive (modal dialog, UI thread).
            let app = unsafe { &*state.app };
            draw_dialog_button(app, lparam)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Show the Apply Template dialog (the Slice dialog's modal
/// pattern). Applying fills the template results pane.
fn show_template_dialog(app: &mut App) {
    let mut state = TemplateDialogState {
        app: app as *mut App,
        files: template_files(),
        set: None,
        file_list: std::ptr::null_mut(),
        struct_list: std::ptr::null_mut(),
        offset_edit: std::ptr::null_mut(),
        error_label: std::ptr::null_mut(),
    };
    let class = wstr(TPLDLG_CLASS);
    let title = wstr("Apply Template");
    // SAFETY: the template class is registered in run_gui; the
    // state travels in lpCreateParams and lives in this stack
    // frame until the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(482.0),
            app.px(348.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut TemplateDialogState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return;
    }
    // SAFETY: modal behaviour, exactly like the Slice dialog.
    unsafe {
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error.
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
    if app.tpl_open {
        invalidate(app);
    }
}

/// File ▸ Extract Ranges… (slice R4c, item 2): ask for a spec,
/// then an output path, then stream the ranges out through the
/// slicing core. Line ranges need the complete line index: while
/// it is still building, the core's own error says so, verbatim.
fn do_extract_ranges(app: &mut App) {
    let Some(spec) = show_slice_dialog(app) else {
        return;
    };
    let Some(out_path) = file_dialog(app, true, Some("extract.bin")) else {
        return;
    };
    if app.editor.path() == Some(out_path.as_path()) {
        message_box(
            app.hwnd,
            "The extract cannot overwrite the file it reads from. Pick a different output name.",
            "Extract Ranges",
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    let index = app.editor.line_index_snapshot();
    let result = (|| -> Result<crate::slicing::ExtractReport, crate::slicing::SliceError> {
        let mut file = std::fs::File::create(&out_path).map_err(crate::slicing::SliceError::Io)?;
        crate::slicing::extract_ranges(app.editor.table(), index.as_ref(), &spec, &mut file)
    })();
    match result {
        Ok(report) => {
            app.status_note = Some(format!(
                "Extracted {} range(s) \u{2014} {} bytes written to {}",
                report.ranges,
                report.bytes_written,
                out_path.display()
            ));
            refresh_status(app);
        }
        Err(err) => {
            message_box(
                app.hwnd,
                &format!("{err}"),
                "Extract Ranges",
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

/// File ▸ Export Delta Patch… (slice R4c, item 3): write the
/// VELOPAT1 patch for the current edits against the on-disk
/// original. The menu greys this unless the document is dirty
/// and backed; the checks repeat here for safety.
fn do_export_patch(app: &mut App) {
    if app.editor.path().is_none() || !app.editor.is_dirty() {
        return;
    }
    let Some(dest) = file_dialog(app, true, Some("delta.velopat")) else {
        return;
    };
    let Some(mmap) = &app.doc_mmap else {
        message_box(
            app.hwnd,
            "The original file is not available, so no patch can be built.",
            "Export Delta Patch",
            MB_OK | MB_ICONERROR,
        );
        return;
    };
    let original = mmap.as_slice();
    let read_original = |offset: u64, out: &mut [u8]| -> usize {
        let start = offset as usize;
        if start >= original.len() {
            return 0;
        }
        let n = out.len().min(original.len() - start);
        out[..n].copy_from_slice(&original[start..start + n]);
        n
    };
    let patch =
        match crate::op_build_patch(original.len() as u64, read_original, app.editor.table()) {
            Ok(patch) => patch,
            Err(err) => {
                message_box(
                    app.hwnd,
                    &format!("The patch could not be built: {err}"),
                    "Export Delta Patch",
                    MB_OK | MB_ICONERROR,
                );
                return;
            }
        };
    match std::fs::write(&dest, patch.to_bytes()) {
        Ok(()) => {
            app.status_note = Some(format!(
                "Delta patch written: {} hunk(s), original {} bytes \u{2192} result {} bytes",
                patch.hunks.len(),
                patch.original_len,
                patch.result_len
            ));
            refresh_status(app);
        }
        Err(err) => {
            message_box(
                app.hwnd,
                &format!("The patch could not be written: {err}"),
                "Export Delta Patch",
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

/// File ▸ Apply Patch… (slice R4c, item 3): apply a VELOPAT1
/// patch whose base is the current document's file, writing the
/// result to a NEW file (Save As semantics — the base is never
/// overwritten in place).
fn do_apply_patch(app: &mut App) {
    let Some(base) = app.editor.path().map(|p| p.to_path_buf()) else {
        message_box(
            app.hwnd,
            "Open a file first \u{2014} a patch applies to the file of the document in the window.",
            "Apply Patch",
            MB_OK | MB_ICONERROR,
        );
        return;
    };
    let Some(patch_path) = file_dialog(app, false, None) else {
        return;
    };
    let patch = match std::fs::read(&patch_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| crate::patch::Patch::from_bytes(&bytes).map_err(|e| e.to_string()))
    {
        Ok(patch) => patch,
        Err(message) => {
            message_box(
                app.hwnd,
                &format!("That is not a usable VeloEdit patch: {message}"),
                "Apply Patch",
                MB_OK | MB_ICONERROR,
            );
            return;
        }
    };
    let Some(out_path) = file_dialog(app, true, Some("patched.bin")) else {
        return;
    };
    if out_path == base {
        message_box(
            app.hwnd,
            "The result cannot overwrite the file the patch is based on. Pick a different name.",
            "Apply Patch",
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    let result = (|| -> Result<u64, String> {
        let mut base_file = std::fs::File::open(&base).map_err(|e| e.to_string())?;
        let base_len = base_file.metadata().map_err(|e| e.to_string())?.len();
        let mut out_file = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
        crate::patch::apply_patch(
            base_len,
            |offset: u64, out: &mut [u8]| -> usize {
                use std::io::{Read, Seek, SeekFrom};
                if base_file.seek(SeekFrom::Start(offset)).is_err() {
                    return 0;
                }
                base_file.read(out).unwrap_or(0)
            },
            &patch,
            &mut out_file,
        )
        .map_err(|e| e.to_string())
    })();
    match result {
        Ok(written) => {
            app.status_note = Some(format!(
                "Patch applied \u{2014} {written} bytes written to {}",
                out_path.display()
            ));
            refresh_status(app);
        }
        Err(message) => {
            message_box(
                app.hwnd,
                &format!("The patch could not be applied: {message}"),
                "Apply Patch",
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

/// Edit ▸ Insert Zeroes… (slice R4c, item 4): a run of zero
/// bytes at the caret, one undo entry, via the editor op.
fn do_insert_zeroes(app: &mut App) {
    let Some(text) = show_prompt(
        app,
        "Insert Zeroes",
        "Zero bytes to insert at the caret (a number, or a K / M / G suffix):",
        "",
    ) else {
        return;
    };
    match crate::cli::parse_size(text.trim()) {
        Ok(count) if count > 0 => match app.editor.insert_zeroes(count) {
            Ok(()) => after_edit(app),
            Err(err) => {
                message_box(
                    app.hwnd,
                    &format!("{err}"),
                    "Insert Zeroes",
                    MB_OK | MB_ICONERROR,
                );
            }
        },
        _ => {
            message_box(
                app.hwnd,
                "Enter a whole number of bytes, optionally with a K, M or G suffix \u{2014} for example 4096 or 4K.",
                "Insert Zeroes",
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

/// File ▸ Open as Administrator (slice R4c, item 8): relaunch
/// elevated on the same file. The plain-words explanation comes
/// BEFORE the UAC prompt, per the house rule — the message box
/// is the explanation; only an OK reaches ShellExecuteW.
fn do_open_admin(app: &mut App) {
    let Some(path) = app.editor.path().map(|p| p.to_path_buf()) else {
        return;
    };
    let answer = message_box(
        app.hwnd,
        "This opens a new VeloEdit window as administrator on the same file.\n\nWindows will ask you to approve it with a User Account Control prompt. The new window runs with full rights, so only do this when a file truly needs them \u{2014} working around a read-only mark usually does not.\n\nOpen the administrator window?",
        "Open as Administrator",
        MB_OKCANCEL | MB_ICONINFORMATION,
    );
    if answer != IDOK {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let verb = wstr("runas");
    let file = wstr(&exe.to_string_lossy());
    let params = wstr(&format!("\"{}\"", path.display()));
    // SAFETY: the strings are NUL-terminated and outlive the
    // call; a null parent and directory are permitted. A return
    // value of 32 or less is the documented failure signal.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result as usize) <= 32 {
        message_box(
            app.hwnd,
            "Windows did not open the administrator window.",
            "Open as Administrator",
            MB_OK | MB_ICONERROR,
        );
    }
}

// ---- Compare With dialog (slice R4c, item 11) ----------------

const COMPARE_CLASS: &str = "VeloEditCompare";
const ID_CMP_LIST: usize = 5451;
const ID_CMP_GOTO: usize = 5452;
const ID_CMP_CLOSE: usize = 5453;
const TIMER_CMP: usize = 8;

/// The compare worker's one-shot result slot.
type CompareSlot =
    std::sync::Arc<std::sync::Mutex<Option<Result<crate::structdiff::StructDiffReport, String>>>>;

struct CompareDialogState {
    app: *mut App,
    slot: CompareSlot,
    note: HWND,
    list: HWND,
    report: Option<crate::structdiff::StructDiffReport>,
    chosen: Option<u64>,
    done: bool,
}

unsafe extern "system" fn compare_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's CompareDialogState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut CompareDialogState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (caller's frame).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        let place = |child: HWND, x: f32, y: f32, w: f32, h: f32| {
            // SAFETY: reparent and position our fresh child window.
            unsafe {
                SetParent(child, hwnd);
                MoveWindow(child, px(x), px(y), px(w), px(h), 1);
            }
        };
        let note = create_child(
            app,
            "Static",
            "Comparing\u{2026}",
            WS_CHILD | WS_VISIBLE,
            0,
            0,
        );
        place(note, 12.0, 10.0, 432.0, 20.0);
        state.note = note;
        // ListBox: LBS_NOTIFY (0x1) | WS_VSCROLL | WS_BORDER.
        let list_style = WS_CHILD | WS_VISIBLE | 0x1 | 0x0020_0000 | 0x0080_0000;
        let list = create_child(app, "ListBox", "", list_style, 0, ID_CMP_LIST);
        place(list, 12.0, 34.0, 432.0, 196.0);
        state.list = list;
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW
        let goto = create_child(app, "Button", "Go to", button_style, 0, ID_CMP_GOTO);
        let close = create_child(app, "Button", "Close", button_style, 0, ID_CMP_CLOSE);
        place(goto, 268.0, 240.0, 80.0, 24.0);
        place(close, 356.0, 240.0, 88.0, 24.0);
        // SAFETY: poll for the worker's report.
        unsafe { SetTimer(hwnd, TIMER_CMP, 100, None) };
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut CompareDialogState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_TIMER if wparam == TIMER_CMP => {
            if !state.done {
                let outcome = match state.slot.lock() {
                    Ok(mut guard) => guard.take(),
                    Err(_) => None,
                };
                if let Some(outcome) = outcome {
                    state.done = true;
                    // SAFETY: our timer; the report has arrived.
                    unsafe { KillTimer(hwnd, TIMER_CMP) };
                    match outcome {
                        Ok(report) => {
                            let add_line = |text: &str| {
                                let wide = wstr(text);
                                // SAFETY: LB_ADDSTRING (0x0180) on
                                // our list box; the string outlives
                                // the call.
                                unsafe {
                                    SendMessageW(state.list, 0x0180, 0, wide.as_ptr() as LPARAM);
                                }
                            };
                            if report.equal() {
                                // SAFETY: our static control.
                                unsafe {
                                    SetWindowTextW(
                                        state.note,
                                        wstr("The two files are identical.").as_ptr(),
                                    );
                                }
                                add_line("No differing regions");
                            } else {
                                let first = report
                                    .first_divergence
                                    .map(|o| o.to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                // SAFETY: our static control.
                                unsafe {
                                    SetWindowTextW(
                                        state.note,
                                        wstr(&format!(
                                            "{} region(s) differ \u{00B7} first difference at byte {first}",
                                            report.regions.len()
                                        ))
                                        .as_ptr(),
                                    );
                                }
                                for region in &report.regions {
                                    add_line(&format!(
                                        "Offset {} \u{00B7} {} bytes",
                                        region.offset, region.length
                                    ));
                                }
                            }
                            state.report = Some(report);
                        }
                        Err(message) => {
                            // SAFETY: our static control.
                            unsafe {
                                SetWindowTextW(state.note, wstr(&message).as_ptr());
                            }
                        }
                    }
                }
            }
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let code = (wparam >> 16) & 0xFFFF;
            // LBN_DBLCLK (1) on the list acts as Go to.
            if id == ID_CMP_GOTO || (id == ID_CMP_LIST && code == 1) {
                // SAFETY: LB_GETCURSEL (0x0188) on our list box.
                let sel = unsafe { SendMessageW(state.list, 0x0188, 0, 0) };
                if sel >= 0 {
                    if let Some(report) = &state.report {
                        if let Some(region) = report.regions.get(sel as usize) {
                            state.chosen = Some(region.offset);
                            // SAFETY: our dialog window.
                            unsafe { DestroyWindow(hwnd) };
                        }
                    }
                }
            } else if id == ID_CMP_CLOSE {
                // SAFETY: our dialog window.
                unsafe { DestroyWindow(hwnd) };
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: closing is cancelling.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DRAWITEM => {
            // SAFETY: the App is alive (modal dialog, UI thread).
            let app = unsafe { &*state.app };
            draw_dialog_button(app, lparam)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Show the Compare With results dialog; returns the region
/// offset chosen via Go to, if any.
fn show_compare_dialog(app: &mut App, slot: CompareSlot) -> Option<u64> {
    let mut state = CompareDialogState {
        app: app as *mut App,
        slot,
        note: std::ptr::null_mut(),
        list: std::ptr::null_mut(),
        report: None,
        chosen: None,
        done: false,
    };
    let class = wstr(COMPARE_CLASS);
    let title = wstr("Compare With");
    // SAFETY: the compare class is registered in run_gui; the
    // state travels in lpCreateParams and lives in this stack
    // frame until the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(480.0),
            app.px(312.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut CompareDialogState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: modal behaviour, exactly like the Split dialog.
    unsafe {
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error.
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
    state.chosen
}

/// File ▸ Compare With… (slice R4c, item 11): pick a second
/// file; the structural-diff core compares block by block on a
/// worker (1 MB blocks, hand-written SHA-256), and the results
/// dialog's Go to jumps the caret to a differing region.
/// Spawn the compare file's staged line-index build (slice
/// R5c): the same resumable builder and slot discipline as the
/// document's own index, against the compare's own slot/epoch.
fn spawn_compare_index_thread(
    slot: std::sync::Arc<std::sync::Mutex<Option<(u64, IndexProgress)>>>,
    epoch: u64,
    mmap: std::sync::Arc<MmapFile>,
) {
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

/// Open the side-by-side compare (slice R5c): the chosen file
/// becomes a read-only pane on the right of the active
/// document. Declined — with a status note, the shell's way —
/// while the split view or a filter owns the canvas.
fn open_compare(app: &mut App) {
    if app.compare.is_some() {
        close_compare(app);
        return;
    }
    if app.settings.split_view {
        app.status_note = Some(
            "Turn off Split Text/Hex to compare side by side \u{2014} both need the right side."
                .to_string(),
        );
        refresh_status(app);
        invalidate(app);
        return;
    }
    if app.filter.is_some() {
        app.status_note = Some("Exit the filter view to compare side by side.".to_string());
        refresh_status(app);
        invalidate(app);
        return;
    }
    let Some(path) = file_dialog(app, false, None) else {
        return;
    };
    let mmap = match MmapFile::open(&path) {
        Ok(mmap) => std::sync::Arc::new(mmap),
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
    let table = if mmap.is_empty() {
        PieceTable::new(0)
    } else {
        PieceTable::from_mapped(mmap.clone())
    };
    let mut editor = Editor::from_table(table, detection, Some(path.clone()), true);
    editor.seed_line_index();
    // Identical bytes-per-row from the start, so hex rows pair
    // 1:1 between the panes.
    editor.set_hex_layout(app.editor.hex_layout());
    let index_slot: std::sync::Arc<std::sync::Mutex<Option<(u64, IndexProgress)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let indexing = if editor.has_complete_index() {
        None
    } else {
        // The seeded head is in; the worker's publications keep
        // this current from the first step on.
        Some((0, mmap.len()))
    };
    spawn_compare_index_thread(index_slot.clone(), 1, mmap.clone());
    app.compare = Some(CompareState {
        editor,
        path,
        mmap: Some(mmap),
        index_slot,
        index_epoch: 1,
        indexing,
        diff_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
        diff_gen: 0,
        diff: None,
        diff_prefix: false,
        diff_stale: false,
        diff_computing: false,
        current: None,
        delta: 0,
    });
    if app.editor.mode() == EditorMode::Text {
        start_diff(app);
    }
    refresh_menu_checks(app);
    refresh_status(app);
    invalidate(app);
}

/// Close the compare (slice R5c): the state drops; any worker
/// still running publishes into the orphaned slots, which is
/// harmless. The inspector, if open, takes its side back on the
/// next paint.
fn close_compare(app: &mut App) {
    if app.compare.take().is_none() {
        return;
    }
    refresh_menu_checks(app);
    refresh_status(app);
    invalidate(app);
}

/// Run the line diff over the two documents on a background
/// worker (slice R5c): snapshots of both tables and whatever
/// line indexes exist at launch go in; a side whose index is
/// missing or dirty gets one built inside the worker from its
/// snapshot, so the run is always over the documents as they
/// stood when it started. A side whose index is still partial
/// yields a prefix run, labelled as such.
fn start_diff(app: &mut App) {
    if app.compare.is_none() {
        return;
    }
    let left_table = snapshot_table(app);
    let left_index = app.editor.line_index_snapshot();
    let doc_gen = app.doc_generation;
    let (slot, right_table, right_index) = {
        let cmp = app.compare.as_ref().expect("checked above");
        let right_table = match &cmp.mmap {
            Some(mmap) if !mmap.is_empty() => PieceTable::from_mapped(mmap.clone()),
            _ => PieceTable::new(0),
        };
        (
            cmp.diff_slot.clone(),
            right_table,
            cmp.editor.line_index_snapshot(),
        )
    };
    let diff_gen = {
        let cmp = app.compare.as_mut().expect("checked above");
        cmp.diff_gen += 1;
        cmp.diff_computing = true;
        cmp.diff_stale = false;
        cmp.diff_gen
    };
    if let Ok(mut guard) = slot.lock() {
        *guard = None;
    }
    std::thread::spawn(move || {
        let left = match left_index {
            Some(index) => index,
            None => {
                let total = left_table.len();
                LineIndex::build(total, |off, out| left_table.read_into(off, out))
            }
        };
        let right = match right_index {
            Some(index) => index,
            None => {
                let total = right_table.len();
                LineIndex::build(total, |off, out| right_table.read_into(off, out))
            }
        };
        let prefix = !left.is_complete() || !right.is_complete();
        let (left_count, left_reader) =
            crate::linediff::indexed_line_reader(&left, |off, out| left_table.read_into(off, out));
        let (right_count, right_reader) =
            crate::linediff::indexed_line_reader(&right, |off, out| {
                right_table.read_into(off, out)
            });
        let report =
            crate::linediff::compare_lines(left_count, left_reader, right_count, right_reader);
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(DiffProgress {
                diff_gen,
                doc_gen,
                report,
                prefix,
            });
        }
    });
}

/// Service the compare on the tick (slice R5c): attach the
/// right file's index publications, attach a finished diff
/// whose generations still match, and — once, when a prefix
/// run's indexes have both completed — rerun the diff over the
/// whole documents.
fn service_compare(app: &mut App) {
    if app.compare.is_none() {
        return;
    }
    let mut changed = false;
    // Index publications for the compare file.
    let publication = {
        let cmp = app.compare.as_ref().expect("checked above");
        match cmp.index_slot.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        }
    };
    if let Some((epoch, progress)) = publication {
        let cmp = app.compare.as_mut().expect("checked above");
        if epoch == cmp.index_epoch {
            let total = cmp.editor.total_len();
            cmp.editor.attach_line_index(progress.snapshot);
            cmp.indexing = if cmp.editor.has_complete_index() {
                None
            } else {
                Some((progress.indexed_bytes, total))
            };
            changed = true;
        }
    }
    // A finished diff whose generations still match.
    let result = {
        let cmp = app.compare.as_ref().expect("checked above");
        match cmp.diff_slot.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        }
    };
    let doc_gen = app.doc_generation;
    if let Some(done) = result {
        let cmp = app.compare.as_mut().expect("checked above");
        if done.diff_gen == cmp.diff_gen && done.doc_gen == doc_gen {
            cmp.diff = Some(done.report);
            cmp.diff_prefix = done.prefix;
            cmp.diff_computing = false;
            if let Some(cur) = cmp.current {
                let len = cmp.diff.as_ref().map(|r| r.regions.len()).unwrap_or(0);
                if cur >= len {
                    cmp.current = None;
                }
            }
            changed = true;
        }
    }
    // The one automatic rerun: a prefix result whose indexes
    // have since completed.
    let rerun = {
        let cmp = app.compare.as_ref().expect("checked above");
        cmp.diff_prefix
            && !cmp.diff_stale
            && !cmp.diff_computing
            && app.editor.mode() == EditorMode::Text
            && app.editor.has_complete_index()
            && cmp.editor.has_complete_index()
    };
    if rerun {
        start_diff(app);
        changed = true;
    }
    if changed {
        refresh_status(app);
        invalidate(app);
    }
}

/// Next/Previous Difference (slice R5c): jump the caret to the
/// region's first line and pair the panes on it, so both scroll
/// there together. Text mode only — hex compare is sync-scroll
/// plus the structural dialog.
fn diff_jump(app: &mut App, next: bool) {
    if app.editor.mode() != EditorMode::Text {
        app.status_note =
            Some("Differences are computed in text mode; hex compare scrolls in step.".to_string());
        refresh_status(app);
        invalidate(app);
        return;
    }
    let Some(cmp) = &app.compare else {
        return;
    };
    let Some(report) = &cmp.diff else {
        return;
    };
    if report.regions.is_empty() {
        app.status_note = Some("No differences.".to_string());
        refresh_status(app);
        invalidate(app);
        return;
    }
    let caret_line = app.editor.line_col_of(app.editor.caret()).0;
    let current = cmp.current;
    let regions = &report.regions;
    let idx = if next {
        crate::panes::next_region(regions, caret_line, current)
    } else {
        crate::panes::prev_region(regions, caret_line, current)
    };
    let Some(idx) = idx else {
        return;
    };
    let region = regions[idx];
    let offset = app.editor.line_start_byte(region.left_start).unwrap_or(0);
    let from = app.editor.caret();
    record_jump(app, from, offset);
    app.editor.move_caret_to(offset, false);
    app.editor.set_first_line(region.left_start);
    app.caret_on = true;
    if let Some(cmp) = &mut app.compare {
        cmp.delta = crate::panes::jump_delta(region.left_start, region.right_start);
        cmp.current = Some(idx);
    }
    refresh_status(app);
    invalidate(app);
}

fn do_compare_with(app: &mut App) {
    let Some(other_path) = file_dialog(app, false, None) else {
        return;
    };
    let table = snapshot_table(app);
    let slot: CompareSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let worker_slot = std::sync::Arc::clone(&slot);
    std::thread::spawn(move || {
        use std::io::{Read, Seek, SeekFrom};
        let outcome = (|| -> Result<crate::structdiff::StructDiffReport, String> {
            let mut file = std::fs::File::open(&other_path)
                .map_err(|e| format!("The other file could not be opened: {e}"))?;
            let len_b = file
                .metadata()
                .map_err(|e| format!("The other file could not be read: {e}"))?
                .len();
            let read_a = |offset: u64, out: &mut [u8]| table.read_into(offset, out);
            let read_b = |offset: u64, out: &mut [u8]| -> usize {
                if file.seek(SeekFrom::Start(offset)).is_err() {
                    return 0;
                }
                file.read(out).unwrap_or(0)
            };
            Ok(crate::structdiff::compare_sources(
                table.len(),
                read_a,
                len_b,
                read_b,
                crate::structdiff::DEFAULT_BLOCK_SIZE,
            ))
        })();
        if let Ok(mut guard) = worker_slot.lock() {
            *guard = Some(outcome);
        }
    });
    if let Some(offset) = show_compare_dialog(app, slot) {
        let from = app.editor.caret();
        app.editor.move_caret_to(offset, false);
        record_jump(app, from, offset);
        after_caret_move(app);
    }
}

// ---------------------------------------------------------------------------
// File Hashes dialog (roadmap item 37): a small modal window in the
// Split dialog's pattern. A worker thread streams the file ON DISK
// through the hash core (the file can be huge — nothing is loaded
// into memory) and delivers the five digests (MD5, SHA-1, SHA-256,
// CRC32, xxHash64 — one worker pass, slice R4c item 6) through a
// slot the dialog polls on a timer. The compare box checks pasted
// text against whichever digest it equals.
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
    /// The five digest value controls, MD5 / SHA-1 / SHA-256 /
    /// CRC32 / xxHash64 (slice R4c, item 6: the two R4a-core
    /// checksums ride the same worker pass).
    values: [HWND; 5],
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
    } else if pasted == format!("{:08x}", h.crc32) {
        "Matches CRC32".to_string()
    } else if pasted == format!("{:016x}", h.xxhash64) {
        "Matches xxHash64".to_string()
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
        let labels = ["MD5", "SHA-1", "SHA-256", "CRC32", "xxHash64"];
        for (i, label) in labels.iter().enumerate() {
            let y = 62.0 + i as f32 * 28.0;
            let lab = create_child(app, "Static", label, WS_CHILD | WS_VISIBLE, 0, 0);
            place(lab, 12.0, y, 76.0, 22.0);
            let val = create_child(
                app,
                "Static",
                "Computing\u{2026}",
                WS_CHILD | WS_VISIBLE,
                0,
                0,
            );
            place(val, 94.0, y, 454.0, 22.0);
            state.values[i] = val;
        }
        let cmp_label = create_child(app, "Static", "Compare:", WS_CHILD | WS_VISIBLE, 0, 0);
        place(cmp_label, 12.0, 208.0, 56.0, 22.0);
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        let compare = create_child(
            app,
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_HASH_COMPARE,
        );
        place(compare, 74.0, 206.0, 474.0, 24.0);
        state.compare = compare;
        let verdict = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE, 0, 0);
        place(verdict, 12.0, 238.0, 536.0, 22.0);
        state.verdict = verdict;
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW, like the Split dialog
        let close = create_child(app, "Button", "Close", button_style, 0, ID_HASH_CLOSE);
        place(close, 468.0, 268.0, 80.0, 26.0);
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
                    let crc32 = format!("{:08X}", h.crc32);
                    let xxhash64 = format!("{:016X}", h.xxhash64);
                    for (control, digest) in state.values.iter().zip([
                        h.md5.as_str(),
                        h.sha1.as_str(),
                        h.sha256.as_str(),
                        crc32.as_str(),
                        xxhash64.as_str(),
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
        values: [std::ptr::null_mut(); 5],
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
            app.px(342.0),
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

/// Which kind of find the find-bar state asks for right now. The
/// Regex toggle is an explicit user choice, so it wins over the
/// masked-pattern heuristic (a regex may legitimately contain a
/// space or a `?`); the scope toggle never combines with regex —
/// turning Regex on turns the scope off (see the toggle's command).
fn find_kind_for(app: &App, text: &str) -> FindKind {
    if app.find_regex {
        FindKind::Regex
    } else if masked_pattern_for(app, text).is_some() {
        FindKind::Masked
    } else if app.find_in_selection {
        FindKind::Scoped
    } else {
        FindKind::Plain
    }
}

/// Whether the document's codec lets a regex pattern run over its
/// bytes as-is (slice R5d): UTF-8 and ANSI are byte-compatible with
/// the pattern text; UTF-16 and the legacy code pages are not, and
/// regex declines there rather than guessing at a transcoding.
fn regex_codec_supported(app: &App) -> bool {
    matches!(
        app.editor.codec(),
        crate::encoding::TextCodec::Standard(
            crate::encoding::TextEncoding::Utf8 | crate::encoding::TextEncoding::Ansi
        )
    )
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
    // find is a whole-document find (plain or regex) for this text.
    // Any other kind — or a one-shot synchronous request from
    // Replace, which needs the editor's complete match state —
    // cancels it.
    let sync_once = std::mem::take(&mut app.find_sync_once);
    if !matches!(kind, FindKind::Plain | FindKind::Regex) || sync_once {
        cancel_bg_find(app);
    }
    let rerun = if matches!(kind, FindKind::Plain | FindKind::Regex) && app.bg_active {
        // The editor's own list stays empty while the background
        // find owns the display; only a text or kind change re-runs.
        text != app.last_find_text || kind != app.last_find_kind
    } else {
        text != app.last_find_text || kind != app.last_find_kind || app.editor.matches().is_empty()
    };
    // A label the match arms insist on (a regex compile error, the
    // codec decline): the generic count at the tail must not
    // overwrite it with "No matches".
    let mut label_override: Option<String> = None;
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
        FindKind::Regex => {
            if !regex_codec_supported(app) {
                // Declined, plainly: the pattern cannot run over
                // this document's bytes as-is (UTF-16 / code page).
                // Clear any older find so Next never advances a
                // stale list under a Regex label.
                if rerun {
                    let _ = app.editor.set_find("", SearchOptions::default());
                }
                label_override = Some("Regex works in UTF-8 and ANSI files".to_string());
            } else if app.bg_active && !rerun {
                if advance != 0 {
                    bg_advance(app, advance);
                }
            } else if rerun {
                cancel_bg_find(app);
                if !sync_once
                    && app.editor.total_len() > BG_FIND_THRESHOLD
                    && start_bg_regex_find(app, &text)
                {
                    if advance != 0 {
                        bg_advance(app, advance);
                    }
                } else {
                    match app.editor.set_find_regex(&text) {
                        Ok(_) => {}
                        Err(err) => {
                            label_override = Some(format!("{err}"));
                        }
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
    if let Some(text) = label_override {
        set_find_label(app, &text);
    } else if matches!(kind, FindKind::Plain | FindKind::Regex) && app.bg_active {
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
    if !app.find_regex && masked_pattern_for(app, &find_text).is_some() {
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
    let result = if all && app.last_find_kind == FindKind::Regex {
        // Regex replace-all (roadmap item 9, slice R5d): the
        // replacement is literal text, and the whole pass is one
        // undo entry (the editor recomputes the match spans
        // itself, so this is exact even when the find ran in the
        // background or the list was cleared by an edit).
        app.editor.replace_all_regex(&replacement).map(|n| n > 0)
    } else if all && app.find_in_selection {
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
// Screen-reader support, roadmap-scoped (roadmap item 47, slice
// R5d): WM_GETOBJECT on the editor canvas returns a hand-rolled
// IRawElementProviderSimple exposing the caret's line — Name is
// "<file> — Ln <n>, Col <n>: <the current line's text>". This is
// deliberately NOT a full TextPattern: no pattern providers are
// implemented, and the Guide/PARITY say exactly that. The
// interface is hand-declared like Direct2D's (windows-sys 0.61
// has no UIA bindings); vtable slots per UIAutomationCore.h:
// QueryInterface 0, AddRef 1, Release 2, get_ProviderOptions 3,
// GetPatternProvider 4, GetPropertyValue 5,
// get_HostRawElementProvider 6. IID_IRawElementProviderSimple:
// d6dd68d1-86fd-4332-8666-9abedea2d24c. Property IDs (plain
// values): Name 30005, ClassName 30012, ControlType 30003 (50004
// = Edit), IsKeyboardFocusable 30009, HasKeyboardFocus 30008,
// NativeWindowHandle 30020. Compile-verified only.
// ---------------------------------------------------------------------------

const IID_RAW_ELEMENT_PROVIDER_SIMPLE: Guid = Guid {
    data1: 0xd6dd_68d1,
    data2: 0x86fd,
    data3: 0x4332,
    data4: [0x86, 0x66, 0x9a, 0xbe, 0xde, 0xa2, 0xd2, 0x4c],
};
const IID_IUNKNOWN_UIA: Guid = Guid {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

// The UIA entry points are resolved from uiautomationcore.dll
// at first use instead of linked: some toolchains ship no
// import library for it, and resolving at runtime also means a
// machine without UIA could never break startup over it.
type UiaReturnFn = unsafe extern "system" fn(HWND, WPARAM, LPARAM, *mut c_void) -> LRESULT;
type UiaDisconnectFn = unsafe extern "system" fn(*mut c_void) -> i32;
type UiaHostFn = unsafe extern "system" fn(HWND, *mut *mut c_void) -> i32;

fn uia_proc(name: &str) -> Option<usize> {
    static LIB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // SAFETY: LoadLibrary/GetProcAddress on a system DLL; the
    // handle is kept for the process's life (never freed), so the
    // resolved addresses stay valid.
    unsafe {
        let lib = *LIB.get_or_init(|| {
            let wide = wstr("uiautomationcore.dll");
            LoadLibraryW(wide.as_ptr()) as usize
        });
        if lib == 0 {
            return None;
        }
        let cname = std::ffi::CString::new(name).ok()?;
        let proc = GetProcAddress(lib as HINSTANCE, cname.as_ptr() as *const u8);
        proc.map(|f| f as usize)
    }
}

/// UiaReturnRawElementProvider, when UIA is available.
fn uia_return_provider(
    hwnd: HWND,
    wparam: WPARAM,
    lparam: LPARAM,
    provider: *mut c_void,
) -> Option<LRESULT> {
    let addr = uia_proc("UiaReturnRawElementProvider")?;
    // SAFETY: the resolved address is the named export of
    // uiautomationcore.dll, whose signature matches UiaReturnFn.
    let f: UiaReturnFn = unsafe { std::mem::transmute(addr) };
    // SAFETY: as above.
    Some(unsafe { f(hwnd, wparam, lparam, provider) })
}

/// UiaHostProviderFromHwnd, when UIA is available.
fn call_uia_host_provider(hwnd: HWND, out: *mut *mut c_void) -> i32 {
    match uia_proc("UiaHostProviderFromHwnd") {
        Some(addr) => {
            // SAFETY: as uia_return_provider.
            let f: UiaHostFn = unsafe { std::mem::transmute(addr) };
            // SAFETY: as above.
            unsafe { f(hwnd, out) }
        }
        None => 0x8000_4005u32 as i32, // E_FAIL
    }
}

/// UiaDisconnectProvider, when UIA is available.
fn uia_disconnect(provider: *mut c_void) {
    if let Some(addr) = uia_proc("UiaDisconnectProvider") {
        // SAFETY: as uia_return_provider.
        let f: UiaDisconnectFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: as above.
        unsafe { f(provider) };
    }
}

#[link(name = "oleaut32")]
unsafe extern "system" {
    fn SysAllocStringLen(s: *const u16, len: u32) -> *mut u16;
}

/// A COM VARIANT, laid out by hand (24 bytes on x64): the value
/// union begins at offset 8.
#[repr(C)]
struct UiaVariant {
    vt: u16,
    reserved1: u16,
    reserved2: u16,
    reserved3: u16,
    data: [u64; 2],
}

impl UiaVariant {
    fn empty() -> Self {
        UiaVariant {
            vt: 0, // VT_EMPTY
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            data: [0, 0],
        }
    }
    fn bstr(text: &str) -> Self {
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: SysAllocStringLen copies the text into a new
        // BSTR owned by the caller (UIA frees it).
        let ptr = unsafe { SysAllocStringLen(wide.as_ptr(), wide.len() as u32) };
        let mut v = Self::empty();
        v.vt = 8; // VT_BSTR
        v.data[0] = ptr as u64;
        v
    }
    fn bool(value: bool) -> Self {
        let mut v = Self::empty();
        v.vt = 11; // VT_BOOL
        v.data[0] = if value { 0xFFFF } else { 0 };
        v
    }
    fn i4(value: i32) -> Self {
        let mut v = Self::empty();
        v.vt = 3; // VT_I4
        v.data[0] = (value as i64) as u64;
        v
    }
}

#[repr(C)]
struct UiaVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    provider_options: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
    get_pattern_provider: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32,
    get_property_value: unsafe extern "system" fn(*mut c_void, i32, *mut UiaVariant) -> i32,
    host_provider: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct UiaProvider {
    vtbl: *const UiaVtbl,
    refs: std::sync::atomic::AtomicU32,
    hwnd: HWND,
}

fn guid_eq(a: &Guid, b: &Guid) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

unsafe extern "system" fn uia_query_interface(
    this: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> i32 {
    // SAFETY: `this` is our provider (or null-checked by the
    // caller contract); `out` is written on every path.
    unsafe {
        if out.is_null() {
            return 0x8000_4003u32 as i32; // E_POINTER
        }
        *out = std::ptr::null_mut();
        if iid.is_null() {
            return 0x8000_4003u32 as i32;
        }
        let iid = &*iid;
        if guid_eq(iid, &IID_IUNKNOWN_UIA) || guid_eq(iid, &IID_RAW_ELEMENT_PROVIDER_SIMPLE) {
            *out = this;
            uia_add_ref(this);
            0
        } else {
            0x8000_4002u32 as i32 // E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn uia_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is our provider while it lives.
    let provider = unsafe { &*(this as *const UiaProvider) };
    provider
        .refs
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1
}

unsafe extern "system" fn uia_release(this: *mut c_void) -> u32 {
    // SAFETY: pairs with the AddRef calls; the last release drops
    // the box.
    let provider = unsafe { &*(this as *const UiaProvider) };
    let now = provider
        .refs
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
        - 1;
    if now == 0 {
        // SAFETY: the box was created by uia_create_provider.
        unsafe { drop(Box::from_raw(this as *mut UiaProvider)) };
    }
    now
}

unsafe extern "system" fn uia_provider_options(this: *mut c_void, out: *mut i32) -> i32 {
    let _ = this;
    // SAFETY: `out` is the caller's slot.
    unsafe {
        if !out.is_null() {
            *out = 1; // ProviderOptions_ServerSideProvider
        }
    }
    0
}

unsafe extern "system" fn uia_get_pattern_provider(
    this: *mut c_void,
    _pattern: i32,
    out: *mut *mut c_void,
) -> i32 {
    let _ = this;
    // No patterns are implemented (the scoped item 47 surface):
    // every pattern request answers "not available".
    // SAFETY: `out` is the caller's slot.
    unsafe {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
    }
    0
}

unsafe extern "system" fn uia_get_property_value(
    this: *mut c_void,
    property: i32,
    out: *mut UiaVariant,
) -> i32 {
    // SAFETY: `this` is our provider; the App is reached through
    // the window handle, and this callback only ever runs on the
    // UI thread while the window lives.
    unsafe {
        if out.is_null() {
            return 0x8000_4003u32 as i32; // E_POINTER
        }
        let provider = &*(this as *const UiaProvider);
        let app_ptr = app_ptr(provider.hwnd);
        if app_ptr.is_null() {
            *out = UiaVariant::empty();
            return 0;
        }
        let app = &mut *app_ptr;
        *out = match property {
            30005 => UiaVariant::bstr(&uia_name(app)),     // Name
            30012 => UiaVariant::bstr("VeloEditCanvas"),   // ClassName
            30003 => UiaVariant::i4(50004),                // ControlType: Edit
            30009 => UiaVariant::bool(true),               // IsKeyboardFocusable
            30008 => UiaVariant::bool(app.has_focus),      // HasKeyboardFocus
            30020 => UiaVariant::i4(provider.hwnd as i32), // NativeWindowHandle
            _ => UiaVariant::empty(),
        };
        0
    }
}

unsafe extern "system" fn uia_host_provider(this: *mut c_void, out: *mut *mut c_void) -> i32 {
    // SAFETY: `this` is our provider; the host provider comes
    // from UIA itself for our window.
    let provider = unsafe { &*(this as *const UiaProvider) };
    call_uia_host_provider(provider.hwnd, out)
}

static UIA_VTBL: UiaVtbl = UiaVtbl {
    query_interface: uia_query_interface,
    add_ref: uia_add_ref,
    release: uia_release,
    provider_options: uia_provider_options,
    get_pattern_provider: uia_get_pattern_provider,
    get_property_value: uia_get_property_value,
    host_provider: uia_host_provider,
};

/// Create the window's UIA provider (one reference owned by the
/// window; UIA AddRefs for its own holds).
fn uia_create_provider(hwnd: HWND) -> *mut c_void {
    let provider = Box::new(UiaProvider {
        vtbl: &UIA_VTBL,
        refs: std::sync::atomic::AtomicU32::new(1),
        hwnd,
    });
    Box::into_raw(provider) as *mut c_void
}

/// The provider's Name: "<file> — Ln <n>, Col <n>: <the caret's
/// line, trimmed, at most ~120 characters>".
fn uia_name(app: &mut App) -> String {
    let name = app
        .editor
        .path()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Untitled".to_string());
    let caret = app.editor.caret();
    let (line, col) = app.editor.line_col_of(caret);
    let text = app
        .editor
        .line_text(line)
        .map(|t| {
            let trimmed = t.trim();
            let capped: String = trimmed.chars().take(120).collect();
            if trimmed.chars().count() > 120 {
                format!("{capped}\u{2026}")
            } else {
                capped
            }
        })
        .unwrap_or_default();
    if text.is_empty() {
        format!("{name} \u{2014} Ln {}, Col {}", line + 1, col + 1)
    } else {
        format!("{name} \u{2014} Ln {}, Col {}: {text}", line + 1, col + 1)
    }
}

// ---------------------------------------------------------------------------
// Taskbar jump list (roadmap item 40, slice R5d): publish the
// recent files as a "Recent" category plus a "New Window" task
// on the taskbar button's right-click menu. The interfaces are
// hand-declared like Direct2D's — ICustomDestinationList is not
// in windows-sys 0.61 either. Vtable slots per shobjidl.h:
// ICustomDestinationList (after IUnknown): SetAppID 3, BeginList
// 4, AppendCategory 5, AppendKnownCategory 6, AddUserTasks 7,
// CommitList 8, GetRemovedDestinations 9, DeleteList 10,
// AbortList 11. IObjectCollection adds AddObject at slot 5 over
// IObjectArray's GetCount 3 / GetAt 4. IShellLinkW: SetPath 20,
// SetArguments 11, SetDescription 7 (full table verified against
// the SDK). GUIDs: CLSID_DestinationList
// 77f10cf0-3db5-4966-b520-b7c54fd35ed6, IID_ICustomDestinationList
// 6332debf-87b5-4670-90c0-5e57b408a49e,
// CLSID_EnumerableObjectCollection
// 2d3468c1-36a7-43b6-ac24-d3f02fd9607a, IID_IObjectCollection
// 5632b1a4-e38a-400a-928a-d4cd63230295, IID_IObjectArray
// 92ca9dcd-5622-4bba-a805-5e9f541bd8c9, CLSID_ShellLink
// 00021401-0000-0000-c000-000000000046, IID_IShellLinkW
// 000214f9-0000-0000-c000-000000000046.
// Compile-verified only. EVERY call is best-effort: any failure
// abandons the publish silently — a jump list must never break
// startup or opening a file.
// ---------------------------------------------------------------------------

const CLSID_DESTINATION_LIST: Guid = Guid {
    data1: 0x77f1_0cf0,
    data2: 0x3db5,
    data3: 0x4966,
    data4: [0xb5, 0x20, 0xb7, 0xc5, 0x4f, 0xd3, 0x5e, 0xd6],
};
const IID_CUSTOM_DESTINATION_LIST: Guid = Guid {
    data1: 0x6332_debf,
    data2: 0x87b5,
    data3: 0x4670,
    data4: [0x90, 0xc0, 0x5e, 0x57, 0xb4, 0x08, 0xa4, 0x9e],
};
const CLSID_ENUMERABLE_OBJECT_COLLECTION: Guid = Guid {
    data1: 0x2d34_68c1,
    data2: 0x36a7,
    data3: 0x43b6,
    data4: [0xac, 0x24, 0xd3, 0xf0, 0x2f, 0xd9, 0x60, 0x7a],
};
const IID_OBJECT_COLLECTION: Guid = Guid {
    data1: 0x5632_b1a4,
    data2: 0xe38a,
    data3: 0x400a,
    data4: [0x92, 0x8a, 0xd4, 0xcd, 0x63, 0x23, 0x02, 0x95],
};
const IID_OBJECT_ARRAY: Guid = Guid {
    data1: 0x92ca_9dcd,
    data2: 0x5622,
    data3: 0x4bba,
    data4: [0xa8, 0x05, 0x5e, 0x9f, 0x54, 0x1b, 0xd8, 0xc9],
};
const CLSID_SHELL_LINK: Guid = Guid {
    data1: 0x0002_1401,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};
const IID_SHELL_LINK_W: Guid = Guid {
    data1: 0x0002_14f9,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const SLOT_CDL_BEGIN_LIST: usize = 4;
const SLOT_CDL_APPEND_CATEGORY: usize = 5;
const SLOT_CDL_ADD_USER_TASKS: usize = 7;
const SLOT_CDL_COMMIT_LIST: usize = 8;
const SLOT_OC_ADD_OBJECT: usize = 5;
const SLOT_SL_SET_DESCRIPTION: usize = 7;
const SLOT_SL_SET_ARGUMENTS: usize = 11;
const SLOT_SL_SET_PATH: usize = 20;

type CdlBeginList =
    unsafe extern "system" fn(*mut c_void, *mut u32, *const Guid, *mut *mut c_void) -> i32;
type CdlAppendCategory = unsafe extern "system" fn(*mut c_void, *const u16, *mut c_void) -> i32;
type CdlPtrCall = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type CdlCommitList = unsafe extern "system" fn(*mut c_void) -> i32;
type OcAddObject = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type SlSetText = unsafe extern "system" fn(*mut c_void, *const u16) -> i32;

/// This file's Guid layout as a windows-sys GUID (same fields,
/// same order — the COM calls take the windows-sys spelling).
fn sys_guid(g: &Guid) -> windows_sys::core::GUID {
    windows_sys::core::GUID {
        data1: g.data1,
        data2: g.data2,
        data3: g.data3,
        data4: g.data4,
    }
}

/// Create one shell link: our exe, optional arguments, and the
/// display description. Null on any failure.
unsafe fn make_shell_link(exe: &[u16], args: Option<&[u16]>, desc: &[u16]) -> *mut c_void {
    // SAFETY: every step is checked; on failure whatever was
    // created is released and null comes back.
    unsafe {
        let mut link: *mut c_void = std::ptr::null_mut();
        let clsid = sys_guid(&CLSID_SHELL_LINK);
        let iid = sys_guid(&IID_SHELL_LINK_W);
        let hr = CoCreateInstance(
            &clsid,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &iid,
            &mut link,
        );
        if hr < 0 || link.is_null() {
            return std::ptr::null_mut();
        }
        let set_path: SlSetText = vfn(link, SLOT_SL_SET_PATH);
        let set_desc: SlSetText = vfn(link, SLOT_SL_SET_DESCRIPTION);
        set_path(link, exe.as_ptr());
        set_desc(link, desc.as_ptr());
        if let Some(args) = args {
            let set_args: SlSetText = vfn(link, SLOT_SL_SET_ARGUMENTS);
            set_args(link, args.as_ptr());
        }
        link
    }
}

/// Create an empty object collection. Null on any failure.
unsafe fn make_object_collection() -> *mut c_void {
    // SAFETY: as make_shell_link.
    unsafe {
        let mut coll: *mut c_void = std::ptr::null_mut();
        let clsid = sys_guid(&CLSID_ENUMERABLE_OBJECT_COLLECTION);
        let iid = sys_guid(&IID_OBJECT_COLLECTION);
        let hr = CoCreateInstance(
            &clsid,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &iid,
            &mut coll,
        );
        if hr < 0 || coll.is_null() {
            return std::ptr::null_mut();
        }
        coll
    }
}

/// Publish the jump list from the current recent-files list.
/// Called at startup and whenever that list changes. Silent on
/// any failure, by design (see the section header).
fn publish_jump_list(app: &App) {
    // SAFETY: the whole publish is best-effort COM plumbing; see
    // the section header. Every object created is released once.
    unsafe {
        // COINIT_APARTMENTTHREADED = 2. S_FALSE (1) still counts
        // as initialised-by-us for the uninitialise pairing;
        // RPC_E_CHANGED_MODE (negative) means COM was already up
        // in another model — proceed without uninitialising.
        let init = CoInitializeEx(std::ptr::null(), 2);
        let uninit = init == 0 || init == 1;
        publish_jump_list_inner(app);
        if uninit {
            CoUninitialize();
        }
    }
}

unsafe fn publish_jump_list_inner(app: &App) {
    // SAFETY: caller established the COM apartment; every step
    // bails quietly on failure.
    unsafe {
        let Ok(exe_path) = std::env::current_exe() else {
            return;
        };
        let exe = wstr(&exe_path.to_string_lossy());
        let mut list: *mut c_void = std::ptr::null_mut();
        let clsid = sys_guid(&CLSID_DESTINATION_LIST);
        let iid = sys_guid(&IID_CUSTOM_DESTINATION_LIST);
        let hr = CoCreateInstance(
            &clsid,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &iid,
            &mut list,
        );
        if hr < 0 || list.is_null() {
            return;
        }
        let mut slots: u32 = 0;
        let mut removed: *mut c_void = std::ptr::null_mut();
        let begin: CdlBeginList = vfn(list, SLOT_CDL_BEGIN_LIST);
        let hr = begin(list, &mut slots, &IID_OBJECT_ARRAY, &mut removed);
        if !removed.is_null() {
            com_release(removed);
        }
        if hr >= 0 {
            // The Recent category: one link per recent file,
            // launching our exe with the file as its argument —
            // activation arrives as an ordinary file argument.
            if !app.settings.recent.is_empty() {
                let coll = make_object_collection();
                if !coll.is_null() {
                    let add: OcAddObject = vfn(coll, SLOT_OC_ADD_OBJECT);
                    for path in &app.settings.recent {
                        let name = std::path::Path::new(path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.clone());
                        let args = wstr(&format!("\"{path}\""));
                        let desc = wstr(&name);
                        let link = make_shell_link(&exe, Some(&args), &desc);
                        if !link.is_null() {
                            add(coll, link);
                            com_release(link);
                        }
                    }
                    let append: CdlAppendCategory = vfn(list, SLOT_CDL_APPEND_CATEGORY);
                    let label = wstr("Recent");
                    append(list, label.as_ptr(), coll);
                    com_release(coll);
                }
            }
            // One user task: a fresh window of our own exe.
            let tasks = make_object_collection();
            if !tasks.is_null() {
                let add: OcAddObject = vfn(tasks, SLOT_OC_ADD_OBJECT);
                let desc = wstr("New Window");
                let link = make_shell_link(&exe, None, &desc);
                if !link.is_null() {
                    add(tasks, link);
                    com_release(link);
                }
                let add_tasks: CdlPtrCall = vfn(list, SLOT_CDL_ADD_USER_TASKS);
                add_tasks(list, tasks);
                com_release(tasks);
            }
            let commit: CdlCommitList = vfn(list, SLOT_CDL_COMMIT_LIST);
            commit(list);
        }
        com_release(list);
    }
}

// ---------------------------------------------------------------------------
// Disassembly panel (slice R5d): a hex-mode-only docked listing
// decoded from the caret with the src/disasm.rs length-accurate
// decoder. Recompute is cheap (48 instructions), so it happens
// on demand: whenever the caret leaves the decoded range or the
// document changed since the decode (note_edit marks it stale),
// the next paint re-decodes from the caret — the panel never
// presents stale instructions as current, and its header names
// the base offset it decoded from.
// ---------------------------------------------------------------------------

/// Decode up to DISASM_COUNT instructions from `caret` over a
/// table snapshot. An undecodable or truncated tail simply ends
/// the listing; the header still names the base honestly.
fn decode_disasm_rows(table: &PieceTable, caret: u64) -> Vec<DisasmRow> {
    let total = table.len();
    let want = ((DISASM_COUNT * 16) as u64).min(total.saturating_sub(caret));
    let bytes = table.read(caret, want);
    let mut rows = Vec::new();
    let mut pos = 0usize;
    while rows.len() < DISASM_COUNT && pos < bytes.len() {
        match crate::disasm::decode_one(&bytes[pos..]) {
            Ok(ins) => {
                let shown = ins.length.min(8).min(bytes.len() - pos);
                let mut byte_text = bytes[pos..pos + shown]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                if ins.length > 8 {
                    byte_text.push_str(" \u{2026}");
                }
                rows.push(DisasmRow {
                    offset: caret + pos as u64,
                    len: ins.length as u64,
                    bytes: byte_text,
                    text: ins.text.clone(),
                });
                pos += ins.length;
            }
            Err(_) => break,
        }
    }
    rows
}

/// Whether the panel's rows still cover the caret and were
/// decoded from the current document.
fn disasm_rows_current(app: &App) -> bool {
    let caret = app.editor.caret();
    let covered_end = app
        .disasm_rows
        .last()
        .map(|r| r.offset + r.len)
        .unwrap_or(0);
    !app.disasm_stale
        && !app.disasm_rows.is_empty()
        && caret >= app.disasm_base
        && caret < covered_end
}

/// Re-decode the panel's rows when they no longer cover the
/// caret or the document changed under them.
fn refresh_disasm(app: &mut App) {
    if !disasm_showing(app) || disasm_rows_current(app) {
        return;
    }
    let caret = app.editor.caret();
    let rows = decode_disasm_rows(app.editor.table(), caret);
    app.disasm_base = caret;
    app.disasm_rows = rows;
    app.disasm_stale = false;
}

/// A press in the disassembly panel: a double-click on a row
/// moves the caret to that instruction's offset; single presses
/// are inert (the panel follows the caret, not the other way).
fn disasm_click(app: &mut App, _x: f32, y: f32, double: bool, layout: &Layout) {
    if !double {
        return;
    }
    refresh_disasm(app);
    let row = ((y - layout.rows_top - 4.0 - app.line_h) / app.line_h.max(1.0)).floor();
    if row < 0.0 {
        return;
    }
    if let Some(r) = app.disasm_rows.get(row as usize) {
        let off = r.offset;
        app.editor.move_caret_to(off, false);
        after_caret_move(app);
    }
}

// ---------------------------------------------------------------------------
// External tools (roadmap item 49, slice R5d): the approved
// replacement for plugins. Tools are declared in data/tools.json
// (the src/exttools.rs core parses and validates them); the Tools
// menu runs them. A run never blocks the UI thread: a worker owns
// the child process and its pipes, and the 200 ms tick applies
// the outcome. The wait is bounded at 30 seconds; Esc stops a
// running tool; stdout is capped at 16 MB.
// ---------------------------------------------------------------------------

const TOOL_TIMEOUT_MS: u32 = 30_000;
const TOOL_STDOUT_CAP: usize = 16 * 1024 * 1024;
const TOOL_STDERR_CAP: usize = 64 * 1024;

/// Load the tool set from `data/tools.json`. A missing file is an
/// empty set and no error; a file that exists but does not parse
/// is an empty set plus the verbatim problem, which the Tools
/// menu shows as a greyed item.
fn load_tool_set() -> (crate::exttools::ToolSet, Option<String>) {
    let path = crate::settings::data_dir().join("tools.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => match crate::exttools::ToolSet::from_json(&text) {
            Ok(set) => (set, None),
            Err(err) => (crate::exttools::ToolSet::default(), Some(err)),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            (crate::exttools::ToolSet::default(), None)
        }
        Err(err) => (
            crate::exttools::ToolSet::default(),
            Some(format!("could not read tools.json: {err}")),
        ),
    }
}

/// Rebuild the Tools menu's contents from the loaded set.
fn rebuild_tools_menu(app: &App) {
    let menu = app.tools_menu;
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
    }
    menu_item(menu, CMD_APPLY_TEMPLATE, "Apply Template\u{2026}");
    menu_sep(menu);
    if let Some(err) = &app.tools_error {
        let label = wstr(&format!("(tools.json: {err})"));
        // SAFETY: our menu; the string outlives the call.
        unsafe { AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr()) };
    } else if app.tools.tools.is_empty() {
        let label = wstr("(No tools \u{2014} add data/tools.json)");
        // SAFETY: as above.
        unsafe { AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr()) };
    } else {
        for (i, tool) in app.tools.tools.iter().take(64).enumerate() {
            menu_item(menu, CMD_TOOL_BASE + i, &tool.name);
        }
    }
    menu_sep(menu);
    menu_item(menu, CMD_TOOLS_RELOAD, "Reload Tools");
}

/// Reload the tool set from disk and rebuild the menu.
fn reload_tools(app: &mut App) {
    let (set, error) = load_tool_set();
    let count = set.tools.len();
    app.tools = set;
    app.tools_error = error;
    rebuild_tools_menu(app);
    app.status_note = Some(match &app.tools_error {
        Some(err) => format!("Tools not loaded: {err}"),
        None => format!("Tools reloaded \u{2014} {count} tool(s)"),
    });
    refresh_status(app);
}

/// The byte range a tool's `range` input feeds: the caret's line
/// content in text mode, the caret's hex row in hex mode.
fn tool_range(app: &mut App) -> (u64, u64) {
    let caret = app.editor.caret();
    match app.editor.mode() {
        EditorMode::Text => {
            let (line, _) = app.editor.line_col_of(caret);
            app.editor
                .line_content_range(line)
                .unwrap_or((caret, caret))
        }
        EditorMode::Hex => {
            let width = app.editor.hex_layout().bytes_per_row.max(1);
            let start = hexview::row_offset_w(caret / width, width);
            (start, (start + width).min(app.editor.total_len()))
        }
    }
}

/// Start a run of `tool`. Refusals (a run already in flight, the
/// filter view up, a file-fed tool with no file) are explained in
/// the status bar or a message box, never silently dropped.
fn run_tool(app: &mut App, tool: &crate::exttools::ExternalTool) {
    if let Some(run) = &app.tool_run {
        app.status_note = Some(format!(
            "{} is still running \u{2014} Esc stops it",
            run.name
        ));
        refresh_status(app);
        return;
    }
    if app.filter.is_some() {
        app.status_note = Some("Tools cannot run while the filter view is up".to_string());
        refresh_status(app);
        return;
    }
    let caret = app.editor.caret();
    if tool.input == crate::exttools::ToolInput::File && app.editor.path().is_none() {
        message_box(
            app.hwnd,
            "This tool works on the document's file ({file}), but the document has not been saved yet. Save it first, then run the tool.",
            "External Tool",
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    let feed: Option<(PieceTable, u64, u64)> = match tool.input {
        crate::exttools::ToolInput::Selection => {
            let range = app.editor.selection_range().unwrap_or((caret, caret));
            Some((snapshot_table(app), range.0, range.1))
        }
        crate::exttools::ToolInput::Range => {
            let range = tool_range(app);
            Some((snapshot_table(app), range.0, range.1))
        }
        crate::exttools::ToolInput::File | crate::exttools::ToolInput::None => None,
    };
    let apply_range = app.editor.selection_range().unwrap_or((caret, caret));
    let file_arg = app
        .editor
        .path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args = crate::exttools::expand_args(&tool.args, &file_arg, caret);
    let command_line = crate::exttools::quote_command_line(&tool.command, &args);
    app.tool_gen += 1;
    let shared = Arc::new(ToolShared {
        outcome: Mutex::new(None),
        process: Mutex::new(None),
        cancelled: std::sync::atomic::AtomicBool::new(false),
    });
    let worker_shared = shared.clone();
    let mode = tool.output;
    let name = tool.name.clone();
    std::thread::spawn(move || {
        let result = tool_worker(&command_line, feed, &worker_shared);
        let outcome = ToolOutcome {
            name: name.clone(),
            mode,
            result,
        };
        if let Ok(mut slot) = worker_shared.outcome.lock() {
            *slot = Some(outcome);
        }
    });
    app.tool_run = Some(ToolRun {
        name: tool.name.clone(),
        gen: app.tool_gen,
        doc_gen: app.doc_generation,
        apply_range,
        shared,
    });
    refresh_status(app);
    invalidate(app);
}

/// Stop the running tool (Esc): flag the cancellation and
/// terminate the child; the worker reports it as cancelled.
fn cancel_tool_run(app: &mut App) {
    let Some(run) = &app.tool_run else {
        return;
    };
    run.shared
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let handle = run.shared.process.lock().ok().and_then(|slot| *slot);
    if let Some(handle) = handle {
        // SAFETY: the handle value was registered by the worker,
        // which clears the slot before closing the handle; while
        // it is registered the handle is open.
        unsafe { TerminateProcess(handle as HANDLE, 1) };
    }
    app.status_note = Some(format!("Stopping {}\u{2026}", run.name));
    refresh_status(app);
}

/// The tool worker: spawn the child with piped stdin/stdout/
/// stderr, feed it, drain it, wait with the timeout, and report.
/// Runs entirely on its own thread.
fn tool_worker(
    command_line: &str,
    feed: Option<(PieceTable, u64, u64)>,
    shared: &Arc<ToolShared>,
) -> Result<Vec<u8>, String> {
    // SAFETY: the whole body is Win32 process plumbing; every
    // handle created here is closed exactly once on every path,
    // and the pipe ends the child does not inherit are made
    // non-inheritable before the spawn.
    unsafe {
        let mut sa: SECURITY_ATTRIBUTES = std::mem::zeroed();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1;
        let (mut in_read, mut in_write) = (std::ptr::null_mut(), std::ptr::null_mut());
        let (mut out_read, mut out_write) = (std::ptr::null_mut(), std::ptr::null_mut());
        let (mut err_read, mut err_write) = (std::ptr::null_mut(), std::ptr::null_mut());
        if CreatePipe(&mut in_read, &mut in_write, &sa, 0) == 0
            || CreatePipe(&mut out_read, &mut out_write, &sa, 0) == 0
            || CreatePipe(&mut err_read, &mut err_write, &sa, 0) == 0
        {
            for h in [in_read, in_write, out_read, out_write, err_read, err_write] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
            return Err(format!(
                "could not create the tool's pipes: {}",
                std::io::Error::last_os_error()
            ));
        }
        // The parent's ends must not leak into the child.
        SetHandleInformation(in_write, 1, 0); // HANDLE_FLAG_INHERIT = 1
        SetHandleInformation(out_read, 1, 0);
        SetHandleInformation(err_read, 1, 0);
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = 0x100; // STARTF_USESTDHANDLES
        startup.hStdInput = in_read;
        startup.hStdOutput = out_write;
        startup.hStdError = err_write;
        let mut cmdline: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            1,
            0x0800_0000, // CREATE_NO_WINDOW
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut info,
        );
        // The child's ends are its own now, whatever happened.
        CloseHandle(in_read);
        CloseHandle(out_write);
        CloseHandle(err_write);
        if created == 0 {
            for h in [in_write, out_read, err_read] {
                CloseHandle(h);
            }
            return Err(format!(
                "could not start the tool: {}",
                std::io::Error::last_os_error()
            ));
        }
        CloseHandle(info.hThread);
        if let Ok(mut slot) = shared.process.lock() {
            *slot = Some(info.hProcess as usize);
        }
        // The feeder: stream the input range in chunks, then
        // close stdin so the tool sees its end of input. A child
        // that exits early breaks the pipe; that write error is
        // expected and ignored. (Handles travel as usize: raw
        // handles are not Send.)
        let in_write_usize = in_write as usize;
        let feeder = std::thread::spawn(move || {
            let in_write = in_write_usize as HANDLE;
            if let Some((table, start, end)) = feed {
                let mut pos = start;
                let mut buf = vec![0u8; 64 * 1024];
                while pos < end {
                    let want = ((end - pos).min(buf.len() as u64)) as usize;
                    let got = table.read_into(pos, &mut buf[..want]);
                    if got == 0 {
                        break;
                    }
                    let mut written = 0u32;
                    if WriteFile(
                        in_write,
                        buf.as_ptr(),
                        got as u32,
                        &mut written,
                        std::ptr::null_mut(),
                    ) == 0
                    {
                        break;
                    }
                    pos += got as u64;
                }
            }
            CloseHandle(in_write);
        });
        // The stderr drainer: read to the cap, keep draining past
        // it so the child never blocks on a full pipe.
        let err_read_usize = err_read as usize;
        let drainer = std::thread::spawn(move || {
            let err_read = err_read_usize as HANDLE;
            let mut kept: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let mut read = 0u32;
                if ReadFile(
                    err_read,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                ) == 0
                    || read == 0
                {
                    break;
                }
                if kept.len() < TOOL_STDERR_CAP {
                    let take = (read as usize).min(TOOL_STDERR_CAP - kept.len());
                    kept.extend_from_slice(&buf[..take]);
                }
            }
            CloseHandle(err_read);
            kept
        });
        // Stdout is read here: append to the cap, discard past it.
        let mut out: Vec<u8> = Vec::new();
        let mut overflow = false;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            if ReadFile(
                out_read,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
                || read == 0
            {
                break;
            }
            if out.len() + (read as usize) <= TOOL_STDOUT_CAP {
                out.extend_from_slice(&buf[..read as usize]);
            } else {
                let take = TOOL_STDOUT_CAP - out.len();
                if take > 0 {
                    out.extend_from_slice(&buf[..take]);
                }
                overflow = true;
            }
        }
        CloseHandle(out_read);
        // The wait is bounded; a timeout stops the child (its
        // pipes then close, ending the feeder and drainer too).
        let waited = WaitForSingleObject(info.hProcess, TOOL_TIMEOUT_MS);
        let timed_out = waited == 0x102; // WAIT_TIMEOUT
        if timed_out {
            TerminateProcess(info.hProcess, 1);
            WaitForSingleObject(info.hProcess, 5_000);
        }
        let mut exit_code = 0u32;
        GetExitCodeProcess(info.hProcess, &mut exit_code);
        if let Ok(mut slot) = shared.process.lock() {
            *slot = None;
        }
        CloseHandle(info.hProcess);
        let _ = feeder.join();
        let stderr = drainer.join().unwrap_or_default();
        if shared.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        if timed_out {
            return Err(format!(
                "the tool did not finish within {} seconds and was stopped",
                TOOL_TIMEOUT_MS / 1000
            ));
        }
        if overflow {
            return Err(
                "the tool printed more than 16 MB of output \u{2014} too much to use".to_string(),
            );
        }
        if exit_code != 0 {
            let err_text = String::from_utf8_lossy(&stderr).trim().to_string();
            return Err(if err_text.is_empty() {
                format!("the tool exited with code {exit_code}")
            } else {
                format!("the tool exited with code {exit_code}: {err_text}")
            });
        }
        Ok(out)
    }
}

/// Pick up a finished tool run on the tick and apply its output.
fn service_tool_run(app: &mut App) {
    let Some(run) = &app.tool_run else {
        return;
    };
    let outcome = match run.shared.outcome.try_lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    let Some(outcome) = outcome else {
        return;
    };
    if outcome.name != run.name || run.gen != app.tool_gen {
        app.tool_run = None;
        return;
    }
    let apply_range = run.apply_range;
    let doc_changed = run.doc_gen != app.doc_generation;
    let mode = outcome.mode;
    let name = outcome.name.clone();
    app.tool_run = None;
    match outcome.result {
        Err(message) => {
            app.status_note = Some(format!("{name}: {message}"));
            refresh_status(app);
            invalidate(app);
        }
        Ok(bytes) => {
            let would_edit = matches!(
                mode,
                crate::exttools::ToolOutput::ReplaceSelection | crate::exttools::ToolOutput::Insert
            );
            if would_edit && doc_changed {
                // The document moved under the tool: show the
                // output instead of applying it to stale offsets.
                app.status_note = Some(format!(
                    "{name} finished, but the document changed while it ran \u{2014} output shown, not applied"
                ));
                show_tool_output(app, &name, &bytes);
            } else {
                apply_tool_output(app, &name, mode, apply_range, bytes);
            }
            refresh_status(app);
            invalidate(app);
        }
    }
}

/// Apply a successful tool run's output by its mode.
fn apply_tool_output(
    app: &mut App,
    name: &str,
    mode: crate::exttools::ToolOutput,
    apply_range: (u64, u64),
    bytes: Vec<u8>,
) {
    match mode {
        crate::exttools::ToolOutput::ReplaceSelection | crate::exttools::ToolOutput::Insert => {
            if app.editor.read_only() {
                app.status_note = Some(format!(
                    "{name} finished, but the document is read-only \u{2014} output shown, not applied"
                ));
                show_tool_output(app, name, &bytes);
                return;
            }
            let (start, end) = if mode == crate::exttools::ToolOutput::Insert {
                (apply_range.1, apply_range.1)
            } else {
                apply_range
            };
            app.editor
                .move_caret_to(start.min(app.editor.total_len()), false);
            app.editor
                .move_caret_to(end.min(app.editor.total_len()), true);
            match app.editor.insert_bytes(&bytes) {
                Ok(()) => {
                    app.status_note = Some(format!(
                        "{name}: {} bytes {}",
                        bytes.len(),
                        if mode == crate::exttools::ToolOutput::Insert {
                            "inserted"
                        } else {
                            "applied"
                        }
                    ));
                    after_edit(app);
                }
                Err(err) => {
                    app.status_note = Some(format!("{name}: {err}"));
                }
            }
        }
        crate::exttools::ToolOutput::NewFile => {
            let Some(dest) = file_dialog(app, true, Some("tool-output.bin")) else {
                app.status_note = Some(format!("{name}: output discarded (no file chosen)"));
                return;
            };
            let table = PieceTable::from_original(bytes);
            match crate::save::save_atomic(&table, &dest, &crate::save::SaveOptions::default()) {
                Ok(_) => {
                    app.status_note = Some(format!("{name}: output written to {}", dest.display()));
                    open_path_in_new_tab(app, &dest);
                }
                Err(err) => {
                    message_box(
                        app.hwnd,
                        &format!("{name}'s output could not be written: {err}"),
                        "External Tool",
                        MB_OK | MB_ICONERROR,
                    );
                }
            }
        }
        crate::exttools::ToolOutput::Discard => {
            show_tool_output(app, name, &bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// Tool output dialog (item 49): a read-only view of what a tool
// printed, for discard-mode tools and for output that could not
// be applied (the document changed, or it is read-only).
// ---------------------------------------------------------------------------

const TOOLOUT_CLASS: &str = "VeloEditToolOut";
const ID_TOOLOUT_TEXT: usize = 5601;
const ID_TOOLOUT_CLOSE: usize = 5602;
const ES_MULTILINE: u32 = 0x0004;
const ES_READONLY: u32 = 0x0800;
const ES_AUTOVSCROLL: u32 = 0x0040;

struct ToolOutState {
    app: *mut App,
    text: String,
}

/// The tool-output dialog's edit subclass: Esc closes.
unsafe extern "system" fn toolout_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: props set by the dialog before any message can
    // arrive; the dialog outlives its controls.
    let old = unsafe { GetPropW(hwnd, wstr("VeloEdit.ToolOutOld").as_ptr()) } as isize;
    if msg == WM_KEYDOWN && wparam == 0x1B {
        // SAFETY: the parent is our dialog.
        let parent = unsafe { GetParent(hwnd) };
        // SAFETY: standard window call.
        unsafe { PostMessageW(parent, WM_CLOSE, 0, 0) };
        return 0;
    }
    if msg == WM_NCDESTROY {
        // SAFETY: restore the original proc and drop our prop.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
            RemovePropW(hwnd, wstr("VeloEdit.ToolOutOld").as_ptr());
        }
    }
    let proc_fn: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: proc_fn is the edit control's original window
    // procedure, retrieved from our own property above.
    unsafe { CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam) }
}

/// Show a tool's output read-only (long output is cut for
/// display only, with a note saying so).
fn show_tool_output(app: &mut App, name: &str, bytes: &[u8]) {
    let raw = String::from_utf8_lossy(bytes).into_owned();
    let mut text: String = raw.chars().take(64_000).collect();
    if raw.chars().count() > 64_000 {
        text.push_str("\n\u{2026} (output truncated for display)");
    }
    let mut state = ToolOutState {
        app,
        text: text.replace('\n', "\r\n"),
    };
    let class = wstr(TOOLOUT_CLASS);
    let title = wstr(&format!("{name} \u{2014} output"));
    // SAFETY: the standard modal-dialog dance (see show_slice_dialog);
    // `state` outlives the loop.
    unsafe {
        let parent = (*state.app).hwnd;
        let dlg = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            560,
            360,
            parent,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut ToolOutState as *mut c_void,
        );
        if dlg.is_null() {
            return;
        }
        EnableWindow(parent, 0);
        ShowWindow(dlg, SW_SHOW);
        SetForegroundWindow(dlg);
        let mut msg: MSG = std::mem::zeroed();
        while IsWindow(dlg) != 0 {
            if GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) <= 0 {
                PostQuitMessage(0);
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        EnableWindow(parent, 1);
        SetForegroundWindow(parent);
    }
}

/// Window procedure for the tool-output dialog.
unsafe extern "system" fn tool_out_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the boilerplate every dialog in this file uses; the
    // state pointer arrives with WM_CREATE and is stashed in
    // GWLP_USERDATA.
    unsafe {
        if msg == WM_CREATE {
            let create = lparam as *const CREATESTRUCTW;
            let state = (*create).lpCreateParams as *mut ToolOutState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            let app = &mut *(*state).app;
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                app.px(560.0),
                app.px(360.0),
                0x2, // SWP_NOMOVE
            );
            let place = |ctl: HWND, x: f32, y: f32, w: f32, h: f32| {
                SetWindowPos(
                    ctl,
                    std::ptr::null_mut(),
                    app.px(x),
                    app.px(y),
                    app.px(w),
                    app.px(h),
                    SWP_NOZORDER,
                );
            };
            // The output: one read-only multiline edit.
            let text_box = create_child(
                app,
                "EDIT",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | ES_MULTILINE
                    | ES_READONLY
                    | ES_AUTOVSCROLL
                    | WS_EX_CLIENTEDGE,
                0,
                ID_TOOLOUT_TEXT,
            );
            SetWindowLongPtrW(text_box, GWLP_HWNDPARENT, hwnd as isize);
            place(text_box, 12.0, 12.0, 536.0, 296.0);
            set_control_text(text_box, &(*state).text.clone());
            // SAFETY: subclass for Esc-to-close; the old proc is
            // kept in a prop for the control's whole life.
            let old = SetWindowLongPtrW(
                text_box,
                GWLP_WNDPROC,
                toolout_edit_proc as *const () as usize as isize,
            );
            SetPropW(
                text_box,
                wstr("VeloEdit.ToolOutOld").as_ptr(),
                old as *mut c_void,
            );
            let close = create_child(
                app,
                "BUTTON",
                "Close",
                WS_CHILD | WS_VISIBLE | 0xB, // BS_OWNERDRAW
                0,
                ID_TOOLOUT_CLOSE,
            );
            SetWindowLongPtrW(close, GWLP_HWNDPARENT, hwnd as isize);
            place(close, 464.0, 318.0, 84.0, 28.0);
            SetFocus(text_box);
            return 0;
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ToolOutState;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *(*state).app;
        match msg {
            WM_COMMAND => {
                if wparam & 0xFFFF == ID_TOOLOUT_CLOSE {
                    DestroyWindow(hwnd);
                }
                0
            }
            WM_DRAWITEM => draw_dialog_button(app, lparam),
            WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let theme = &app.theme;
                SetBkColor(wparam as _, colorref(theme.panel));
                SetTextColor(wparam as _, colorref(theme.text));
                app.panel_brush as LRESULT
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
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
        app.brushes = Some(Brushes::build(target, &app.theme));
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
// Roadmap slice R4b — view wiring helpers: the generic prompt
// dialog (filter pattern, freeze count), the structural filter
// view, the jump history and pointer following, the strings and
// entropy workers, and the scrollbar tick hover preview.
// ---------------------------------------------------------------------------

const PROMPT_CLASS: &str = "VeloEditPrompt";
const ID_PROMPT_EDIT: usize = 5301;
const ID_PROMPT_OK: usize = 5302;
const ID_PROMPT_CANCEL: usize = 5303;

struct PromptDialogState {
    app: *mut App,
    edit: HWND,
    label: String,
    initial: String,
    result: Option<String>,
}

const PROP_PROMPT_OLD: &str = "VeloEdit.PromptOldProc";
const PROP_PROMPT_DLG: &str = "VeloEdit.PromptDlg";

/// Subclass the prompt's edit so Enter confirms and Esc cancels
/// (the modal loop never sees the edit's keys otherwise) — the
/// same property-based trick as the find-bar edits.
fn subclass_prompt_edit(edit: HWND, dlg: HWND) {
    let name_old = wstr(PROP_PROMPT_OLD);
    let name_dlg = wstr(PROP_PROMPT_DLG);
    // SAFETY: `edit` is our child window; the old proc is stored in
    // a window property before the new proc is installed.
    unsafe {
        let old = SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            prompt_edit_proc as *const () as usize as isize,
        );
        SetPropW(edit, name_old.as_ptr(), old as HANDLE);
        SetPropW(edit, name_dlg.as_ptr(), dlg as HANDLE);
    }
}

unsafe extern "system" fn prompt_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let name_old = wstr(PROP_PROMPT_OLD);
    let name_dlg = wstr(PROP_PROMPT_DLG);
    // SAFETY: the props were stored by subclass_prompt_edit before
    // this proc was installed, and are removed at WM_NCDESTROY.
    let old = unsafe { GetPropW(hwnd, name_old.as_ptr()) } as isize;
    let dlg = unsafe { GetPropW(hwnd, name_dlg.as_ptr()) } as HWND;
    if msg == WM_KEYDOWN {
        let cmd = match wparam as u32 {
            0x0D => Some(ID_PROMPT_OK),     // Enter: confirm
            0x1B => Some(ID_PROMPT_CANCEL), // Esc: cancel
            _ => None,
        };
        if let Some(cmd) = cmd {
            // SAFETY: `dlg` is the prompt dialog stored at subclass
            // time; the command is handled there.
            unsafe { PostMessageW(dlg, WM_COMMAND, cmd, 0) };
            return 0;
        }
    }
    if msg == WM_NCDESTROY {
        // SAFETY: restore the original proc and drop our props.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
            RemovePropW(hwnd, name_old.as_ptr());
            RemovePropW(hwnd, name_dlg.as_ptr());
        }
    }
    let proc_fn: WNDPROC = unsafe { std::mem::transmute(old) };
    // SAFETY: proc_fn is the edit control's original window
    // procedure, retrieved from our own property above.
    unsafe { CallWindowProcW(proc_fn, hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn prompt_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        // SAFETY: lparam is the CREATESTRUCTW from CreateWindowExW,
        // whose lpCreateParams is the caller's PromptDialogState.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state_ptr = create.lpCreateParams as *mut PromptDialogState;
        // SAFETY: storing the state pointer for the dialog's life.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };
        // SAFETY: the state outlives the dialog (caller's frame).
        let state = unsafe { &mut *state_ptr };
        // SAFETY: the App is alive and exclusively ours on this thread.
        let app = unsafe { &*state.app };
        let scale = app.scale();
        let px = |dip: f32| (dip * scale).round() as i32;
        let label_text = state.label.clone();
        let initial = state.initial.clone();
        let label = create_child(app, "Static", &label_text, WS_CHILD | WS_VISIBLE, 0, 0);
        // SAFETY: reparent and position our fresh child window.
        unsafe {
            SetParent(label, hwnd);
            MoveWindow(label, px(12.0), px(10.0), px(404.0), px(20.0), 1);
        }
        let edit_style = WS_CHILD | WS_VISIBLE | 0x80; // ES_AUTOHSCROLL
        let edit = create_child(
            app,
            "Edit",
            &initial,
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_PROMPT_EDIT,
        );
        // SAFETY: as above.
        unsafe {
            SetParent(edit, hwnd);
            MoveWindow(edit, px(12.0), px(34.0), px(404.0), px(24.0), 1);
            SetFocus(edit);
            // Select the prefill so typing replaces it.
            SendMessageW(edit, 0xB1, 0, -1isize); // EM_SETSEL all
        }
        state.edit = edit;
        subclass_prompt_edit(edit, hwnd);
        let button_style = WS_CHILD | WS_VISIBLE | 0xB; // BS_OWNERDRAW
        let ok = create_child(app, "Button", "OK", button_style, 0, ID_PROMPT_OK);
        let cancel = create_child(app, "Button", "Cancel", button_style, 0, ID_PROMPT_CANCEL);
        // SAFETY: as above.
        unsafe {
            SetParent(ok, hwnd);
            MoveWindow(ok, px(268.0), px(66.0), px(70.0), px(24.0), 1);
            SetParent(cancel, hwnd);
            MoveWindow(cancel, px(346.0), px(66.0), px(70.0), px(24.0), 1);
        }
        return 0;
    }
    // SAFETY: the state pointer was stored at WM_CREATE.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut PromptDialogState;
    if state_ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the state outlives the dialog (see WM_CREATE).
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_COMMAND => {
            match wparam & 0xFFFF {
                ID_PROMPT_OK => {
                    // SAFETY: our edit control.
                    state.result = Some(unsafe { window_text(state.edit) });
                    // SAFETY: our dialog window.
                    unsafe { DestroyWindow(hwnd) };
                }
                ID_PROMPT_CANCEL => {
                    // SAFETY: our dialog window.
                    unsafe { DestroyWindow(hwnd) };
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: closing is cancelling.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        // Owner-drawn buttons, drawn like the Split dialog's (the
        // dialog procs cannot share the main window's draw code).
        WM_DRAWITEM => {
            // SAFETY: lparam is a DRAWITEMSTRUCT for one of our buttons.
            let dis = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
            // SAFETY: the App is alive (modal dialog, UI thread).
            let app = unsafe { &*state.app };
            let mut rect = dis.rcItem;
            // SAFETY: GDI drawing on the button's own DC.
            unsafe {
                FillRect(dis.hDC, &rect, app.panel_brush);
                FrameRect(dis.hDC, &rect, app.line_brush);
                if dis.itemState & 0x10 != 0 {
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

/// Show the generic one-line prompt dialog (the Split dialog's
/// modal pattern): a label, one edit prefilled with `initial`, OK /
/// Cancel. Returns the confirmed text, or `None` on cancel.
fn show_prompt(app: &mut App, title: &str, label: &str, initial: &str) -> Option<String> {
    let mut state = PromptDialogState {
        app: app as *mut App,
        edit: std::ptr::null_mut(),
        label: label.to_string(),
        initial: initial.to_string(),
        result: None,
    };
    let class = wstr(PROMPT_CLASS);
    let title_w = wstr(title);
    // SAFETY: the prompt class is registered in run_gui; the state
    // travels in lpCreateParams and lives in this stack frame until
    // the dialog is destroyed (the modal loop below).
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title_w.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            app.px(440.0),
            app.px(138.0),
            app.hwnd,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            &mut state as *mut PromptDialogState as *const c_void,
        )
    };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: modal behaviour, exactly like the Split dialog.
    unsafe {
        EnableWindow(app.hwnd, 0);
        ShowWindow(hwnd, SW_SHOW);
    }
    loop {
        // SAFETY: MSG is a plain C struct; GetMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: as above; 0 = WM_QUIT, -1 = error.
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

// ---- Structural filter view (roadmap item 18) --------------------

/// Search ▸ Filter…: prompt for a pattern, then show only the
/// lines that contain it.
fn open_filter_prompt(app: &mut App) {
    if app.editor.mode() != EditorMode::Text {
        app.status_note = Some("Filter works in Text mode".to_string());
        refresh_status(app);
        return;
    }
    let initial = if !app.last_find_text.is_empty() {
        app.last_find_text.clone()
    } else {
        app.editor.selected_text_lossy()
    };
    let Some(text) = show_prompt(app, "Filter Lines", "Show only lines containing:", &initial)
    else {
        return;
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        // Empty confirmation (slice R4c, item 12): with a filter
        // active it EXITS the filter view (the caret never moved
        // while filtering, so nothing needs restoring); with none
        // active it simply closes the prompt.
        if app.filter.is_some() {
            exit_filter(app);
        }
        return;
    }
    enter_filter(app, text);
}

/// Cancel a running background filter build (slice R4c, item
/// 13): bump the generation so the worker's next publication is
/// dropped, and clear the slot. Called on exit, on jump-to-line,
/// on a new build, and on document open.
fn cancel_filter_build(app: &mut App) {
    app.filter_gen += 1;
    if let Ok(mut guard) = app.filter_slot.lock() {
        *guard = None;
    }
}

/// Build the filter view for `pattern` and switch to it. The
/// cursor starts on the row at (or nearest after) the caret's line,
/// so Enter without moving returns to familiar ground.
///
/// Documents up to the background-find threshold build
/// synchronously, as they always have. Larger documents build on
/// a worker (slice R4c, item 13): the view opens immediately with
/// no rows and fills in as the worker's snapshots attach on the
/// tick — the window never freezes for a whole-document scan.
fn enter_filter(app: &mut App, pattern: String) {
    let needle = match app.editor.encode_text(&pattern) {
        Ok(needle) => needle,
        Err(_) => {
            message_box(
                app.hwnd,
                "That text cannot be written in this file's encoding.",
                "VeloEdit",
                MB_OK | MB_ICONERROR,
            );
            return;
        }
    };
    app.last_find_text = pattern.clone();
    cancel_filter_build(app);
    if app.editor.total_len() > BG_FIND_THRESHOLD {
        start_bg_filter(app, pattern, needle);
        return;
    }
    // Consume the needle before borrowing it for the query: the
    // editor borrows the table itself, so the query's lifetime is
    // satisfied by keeping `needle` alive across the call.
    let view = app.editor.filter_view(crate::filter::FilterQuery::literal(
        &needle,
        SearchOptions::default(),
    ));
    let Some(view) = view else {
        return;
    };
    let caret_line = app.editor.line_col_of(app.editor.caret()).0;
    let cursor_row = if view.is_empty() {
        0
    } else {
        view.row_of_line(caret_line).unwrap_or_else(|| {
            let after = view
                .lines()
                .partition_point(|line| line.line_no < caret_line);
            after.min(view.len() - 1)
        })
    };
    app.filter = Some(FilterState {
        view,
        pattern,
        first_row: 0,
        cursor_row,
        building: false,
        scanned: app.editor.total_len(),
    });
    filter_ensure_cursor_visible(app);
    refresh_status(app);
    invalidate(app);
}

/// Start the background filter build for a large document (slice
/// R4c, item 13): the filter view opens NOW with an empty
/// in-progress view; a worker walks a snapshot of the document in
/// [`BG_FIND_WINDOW`] windows, folding matches into a
/// `crate::filter::FilterBuilder` and publishing snapshots into
/// the filter slot, which the tick attaches (`service_filter_build`).
/// The caller has already bumped the build generation via
/// `cancel_filter_build`.
fn start_bg_filter(app: &mut App, pattern: String, needle: Vec<u8>) {
    let snapshot = snapshot_table(app);
    let total = snapshot.len();
    let doc_gen = app.doc_generation;
    let filter_gen = app.filter_gen;
    let slot = app.filter_slot.clone();
    let empty = crate::filter::FilterBuilder::new(total).snapshot(false);
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(FilterProgress {
            filter_gen,
            doc_gen,
            view: std::sync::Arc::new(empty.clone()),
            scanned: 0,
            done: false,
        });
    }
    app.filter = Some(FilterState {
        view: empty,
        pattern,
        first_row: 0,
        cursor_row: 0,
        building: true,
        scanned: 0,
    });
    refresh_status(app);
    invalidate(app);
    std::thread::spawn(move || {
        let options = SearchOptions::default();
        let mut builder = crate::filter::FilterBuilder::new(total);
        let mut end = 0u64;
        loop {
            end = (end + BG_FIND_WINDOW).min(total);
            builder.advance(&snapshot, end, &needle, &options);
            let done = builder.is_done();
            let view = std::sync::Arc::new(builder.snapshot(done));
            if let Ok(mut guard) = slot.lock() {
                match guard.as_mut() {
                    Some(progress)
                        if progress.filter_gen == filter_gen && progress.doc_gen == doc_gen =>
                    {
                        progress.view = view;
                        progress.scanned = builder.scanned();
                        progress.done = done;
                    }
                    // Superseded or cancelled: stop quietly.
                    _ => return,
                }
            }
            if done {
                return;
            }
        }
    });
}

/// The tick's filter-build service (slice R4c, item 13): attach
/// the worker's latest snapshot to the live filter view. The
/// cursor follows its line across snapshots (the first attach
/// positions it at the caret's line, like the synchronous build);
/// when the final snapshot lands, the banner's progress note
/// clears and the view is complete.
fn service_filter_build(app: &mut App) {
    let snapshot = match app.filter_slot.try_lock() {
        Ok(guard) => guard.as_ref().and_then(|progress| {
            if progress.filter_gen == app.filter_gen && progress.doc_gen == app.doc_generation {
                Some((progress.view.clone(), progress.scanned, progress.done))
            } else {
                None
            }
        }),
        Err(_) => None,
    };
    let Some((view, scanned, done)) = snapshot else {
        return;
    };
    let caret_line = app.editor.line_col_of(app.editor.caret()).0;
    let Some(filter) = &mut app.filter else {
        return;
    };
    if !filter.building {
        return;
    }
    let first_attach = filter.scanned == 0;
    let cursor_line = filter
        .view
        .line_at_row(filter.cursor_row)
        .map(|line| line.line_no);
    filter.view = (*view).clone();
    filter.scanned = scanned;
    if done {
        filter.building = false;
    }
    if first_attach {
        filter.cursor_row = if filter.view.is_empty() {
            0
        } else {
            filter.view.row_of_line(caret_line).unwrap_or_else(|| {
                let after = filter
                    .view
                    .lines()
                    .partition_point(|line| line.line_no < caret_line);
                after.min(filter.view.len() - 1)
            })
        };
    } else if let Some(line_no) = cursor_line {
        filter.cursor_row = filter
            .view
            .row_of_line(line_no)
            .unwrap_or(filter.cursor_row)
            .min(filter.view.len().saturating_sub(1));
    }
    filter_ensure_cursor_visible(app);
    refresh_status(app);
    invalidate(app);
}

/// Leave the filter view without moving the caret.
fn exit_filter(app: &mut App) {
    cancel_filter_build(app);
    if app.filter.take().is_some() {
        refresh_status(app);
        invalidate(app);
    }
}

/// Enter / double-click in the filter view: leave the filter and
/// land the caret at the start of the cursor row's line.
fn filter_jump_to_cursor(app: &mut App) {
    let Some(line) = app
        .filter
        .as_ref()
        .and_then(|f| f.view.line_at_row(f.cursor_row))
    else {
        exit_filter(app);
        return;
    };
    let from = app.editor.caret();
    app.filter = None;
    cancel_filter_build(app);
    app.editor.move_caret_to(line.start, false);
    record_jump(app, from, line.start);
    after_caret_move(app);
}

/// Move the filter's row cursor by `delta` rows (clamped), keeping
/// it visible.
fn filter_move_cursor(app: &mut App, delta: i64) {
    let Some(filter) = &mut app.filter else {
        return;
    };
    if filter.view.is_empty() {
        return;
    }
    let last = filter.view.len() - 1;
    let next = (filter.cursor_row as i64 + delta).clamp(0, last as i64);
    filter.cursor_row = next as usize;
    filter_ensure_cursor_visible(app);
    refresh_status(app);
    invalidate(app);
}

/// Keep the filter cursor inside the visible rows.
fn filter_ensure_cursor_visible(app: &mut App) {
    let page = page_rows(app).max(1) as usize;
    let Some(filter) = &mut app.filter else {
        return;
    };
    if filter.cursor_row < filter.first_row {
        filter.first_row = filter.cursor_row;
    }
    if filter.cursor_row >= filter.first_row + page {
        filter.first_row = filter.cursor_row + 1 - page;
    }
}

/// When the line index completes, an active filter view built over
/// the partial index is rebuilt over the whole document (the
/// banner's "still indexing" note then clears).
fn rebuild_filter_if_stale(app: &mut App) {
    let Some(filter) = &app.filter else {
        return;
    };
    if filter.building {
        // A background build owns this view (slice R4c, item 13);
        // its snapshots already cover the whole document when
        // they finish, index or no index.
        return;
    }
    if filter.view.is_complete() {
        return;
    }
    let pattern = filter.pattern.clone();
    let cursor_line = filter
        .view
        .line_at_row(filter.cursor_row)
        .map(|line| line.line_no);
    let Ok(needle) = app.editor.encode_text(&pattern) else {
        return;
    };
    let Some(view) = app.editor.filter_view(crate::filter::FilterQuery::literal(
        &needle,
        SearchOptions::default(),
    )) else {
        return;
    };
    let cursor_row = match cursor_line {
        Some(line_no) => view.row_of_line(line_no).unwrap_or(0),
        None => 0,
    };
    if let Some(filter) = &mut app.filter {
        filter.view = view;
        filter.cursor_row = cursor_row.min(filter.view.len().saturating_sub(1));
        filter_ensure_cursor_visible(app);
    }
    refresh_status(app);
    invalidate(app);
}

// ---- Jump history + pointer following (roadmap item 11) ----------

/// Record a deliberate jump in the shell's history: the core stack
/// holds destinations, so both ends are pushed (the origin push is
/// what Back lands on; pushing the position already current is a
/// no-op in the core).
fn record_jump(app: &mut App, from: u64, to: u64) {
    app.jump_history.push(from);
    app.jump_history.push(to);
}

/// Alt+Left / Alt+Right and their Search-menu items: walk the jump
/// history. Walking never pushes.
fn jump_history_move(app: &mut App, forward: bool) {
    let target = if forward {
        app.jump_history.forward()
    } else {
        app.jump_history.back()
    };
    if let Some(to) = target {
        let to = to.min(app.editor.total_len());
        app.editor.move_caret_to(to, false);
        after_caret_move(app);
    }
}

/// Ctrl+Enter (or Ctrl+click) pointer following: read the pointer
/// core at the caret and jump to the first reading that lands
/// inside the document — widest little-endian first, the order a
/// Windows binary's pointers almost always take.
fn pointer_jump(app: &mut App) {
    let at = app.editor.caret();
    let total = app.editor.total_len();
    let bytes = app.editor.table().read(at, (total - at).min(8));
    let targets = crate::pointers::pointer_targets(&bytes, total);
    let pick = [
        ("u64 LE", targets.u64_le),
        ("u32 LE", targets.u32_le),
        ("u64 BE", targets.u64_be),
        ("u32 BE", targets.u32_be),
    ]
    .into_iter()
    .find_map(|(label, reading)| reading.filter(|r| r.in_document).map(|r| (label, r.value)));
    match pick {
        Some((label, value)) => {
            app.editor.move_caret_to(value, false);
            record_jump(app, at, value);
            after_caret_move(app);
            app.status_note = Some(format!("Jumped via pointer 0x{value:X} ({label})"));
            refresh_status(app);
        }
        None => {
            app.status_note = Some(format!("No pointer at 0x{at:X} lands inside the document"));
            refresh_status(app);
        }
    }
}

/// Pin the view to the document's end (Ghost Tail completion, and
/// the scrollbar's End path shares the shape).
fn pin_view_to_end(app: &mut App) {
    let page = page_rows(app);
    match app.editor.mode() {
        EditorMode::Text => {
            let count = app.editor.line_count();
            app.editor.set_first_line(count.saturating_sub(page));
        }
        EditorMode::Hex => {
            let width = app.editor.hex_layout().bytes_per_row.max(1);
            let rows = hexview::row_count_w(app.editor.total_len(), width);
            app.editor.set_first_row(rows.saturating_sub(page));
        }
    }
}

// ---- Strings pane + entropy strip workers -------------------------

/// The strings pane's per-tick service: pick up a finished scan,
/// and start one when the pane is open and the held results are
/// stale (document changed, or the min length changed).
fn service_strings(app: &mut App) {
    let ready = match app.strings.slot.try_lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => None,
    };
    if let Some((gen, min_len, results)) = ready {
        if gen == app.strings.gen {
            app.strings.results = results;
            app.strings.scanned_gen = gen;
            app.strings.scanned_min_len = min_len;
            app.strings.searching = false;
            app.strings.first = 0;
            app.strings.selected = None;
            invalidate(app);
        }
    }
    if app.settings.strings_open
        && !app.strings.searching
        && (app.strings.scanned_gen != app.strings.gen
            || app.strings.scanned_min_len != app.settings.strings_min_len)
    {
        start_strings_scan(app);
    }
}

fn start_strings_scan(app: &mut App) {
    let gen = app.strings.gen;
    let min_len = app.settings.strings_min_len;
    let total = app.editor.total_len();
    if total <= BG_FIND_THRESHOLD {
        let snap = snapshot_table(app);
        app.strings.results = crate::strings::scan_strings_in_table(&snap, min_len as usize);
        app.strings.scanned_gen = gen;
        app.strings.scanned_min_len = min_len;
        app.strings.first = 0;
        app.strings.selected = None;
        invalidate(app);
        return;
    }
    app.strings.searching = true;
    let snap = snapshot_table(app);
    let slot = app.strings.slot.clone();
    std::thread::spawn(move || {
        let results = crate::strings::scan_strings_in_table(&snap, min_len as usize);
        if let Ok(mut guard) = slot.lock() {
            *guard = Some((gen, min_len, results));
        }
    });
}

/// Double-click / Enter on a strings result: select it and land the
/// caret on the string's offset (the current mode is kept — the
/// jump is a caret move, and it joins the jump history).
fn strings_jump_to(app: &mut App, idx: usize) {
    let Some(found) = app.strings.results.get(idx) else {
        return;
    };
    let offset = found.offset;
    app.strings.selected = Some(idx);
    let from = app.editor.caret();
    app.editor.move_caret_to(offset, false);
    record_jump(app, from, offset);
    after_caret_move(app);
}

/// The entropy strip's per-tick service: same shape as the strings
/// service. The block count targets about two blocks per DIP of
/// strip width, capped so the value list stays small.
fn service_entropy(app: &mut App) {
    let ready = match app.entropy.slot.try_lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => None,
    };
    if let Some((gen, values)) = ready {
        if gen == app.entropy.gen {
            app.entropy.values = values;
            app.entropy.scanned_gen = gen;
            app.entropy.computing = false;
            invalidate(app);
        }
    }
    if app.entropy_on && !app.entropy.computing && app.entropy.scanned_gen != app.entropy.gen {
        start_entropy_compute(app);
    }
}

fn start_entropy_compute(app: &mut App) {
    let total = app.editor.total_len();
    let gen = app.entropy.gen;
    if total == 0 {
        app.entropy.values = Vec::new();
        app.entropy.scanned_gen = gen;
        app.entropy.computing = false;
        return;
    }
    let width = compute_layout(app).editor_right.max(1.0);
    let target_blocks = ((width * 2.0) as u64).clamp(256, 8192);
    let block_size = (total / target_blocks).max(1);
    app.entropy.computing = true;
    if total <= BG_FIND_THRESHOLD {
        let snap = snapshot_table(app);
        app.entropy.values =
            crate::entropy::entropy_map(total, |off, out| snap.read_into(off, out), block_size);
        app.entropy.scanned_gen = gen;
        app.entropy.computing = false;
        invalidate(app);
        return;
    }
    let snap = snapshot_table(app);
    let slot = app.entropy.slot.clone();
    std::thread::spawn(move || {
        let values =
            crate::entropy::entropy_map(total, |off, out| snap.read_into(off, out), block_size);
        if let Ok(mut guard) = slot.lock() {
            *guard = Some((gen, values));
        }
    });
}

/// Clicking the entropy strip seeks the view proportionally (the
/// caret stays where it is — this is a scroll, not a jump).
fn entropy_seek(app: &mut App, x_dip: f32) {
    let layout = compute_layout(app);
    let total = app.editor.total_len();
    if total == 0 || layout.editor_right <= 0.0 {
        return;
    }
    let frac = (x_dip / layout.editor_right).clamp(0.0, 1.0) as f64;
    let byte = (frac * total as f64) as u64;
    match app.editor.mode() {
        EditorMode::Text => {
            let (line, _) = app.editor.line_col_of(byte);
            let page = page_rows(app);
            app.editor.set_first_line(line.saturating_sub(page / 2));
        }
        EditorMode::Hex => {
            let width = app.editor.hex_layout().bytes_per_row.max(1);
            app.editor.set_first_row(byte / width);
        }
    }
    invalidate(app);
}

// ---- Scrollbar tick hover previews (item 24 expansion) ------------

/// The tick nearest the pointer inside the scrollbar track, as
/// `(offset, is_bookmark)`: the same offsets the tick painter
/// buckets. `None` off the track or when nothing is within a
/// couple of DIPs along the track.
fn tick_candidate_at(app: &mut App, x_dip: f32, y_dip: f32) -> Option<(u64, bool)> {
    let layout = compute_layout(app);
    if x_dip < layout.editor_right - SCROLLBAR_W || x_dip > layout.editor_right {
        return None;
    }
    if y_dip < layout.rows_top || y_dip > layout.rows_bottom {
        return None;
    }
    let total = app.editor.total_len();
    if total == 0 {
        return None;
    }
    // No scrollbar is drawn when the document fits the page — and
    // no ticks are painted then either, so there is nothing to
    // preview.
    let (sb_total, _, sb_page) = scroll_metrics(app);
    if sb_total <= sb_page {
        return None;
    }
    let track_h = (layout.rows_bottom - layout.rows_top).max(1.0) as f64;
    let frac = (y_dip - layout.rows_top) as f64 / track_h;
    let threshold = 2.5 / track_h;
    let mut best: Option<(u64, bool, f64)> = None;
    let consider = |offset: u64, is_bookmark: bool, best: &mut Option<(u64, bool, f64)>| {
        let d = ((offset as f64 / total as f64) - frac).abs();
        if d <= threshold && best.map(|(_, _, bd)| d < bd).unwrap_or(true) {
            *best = Some((offset, is_bookmark, d));
        }
    };
    for &m in display_matches(app) {
        consider(m, false, &mut best);
    }
    for b in app.bookmarks.iter() {
        consider(b.offset, true, &mut best);
    }
    best.map(|(offset, is_bookmark, _)| (offset, is_bookmark))
}

/// The hover preview's per-tick service: a candidate that has
/// settled (350 ms) shows its tooltip; anything else hides it.
/// Two kinds of candidate share the one tracking tooltip:
/// scrollbar ticks (a line preview) and tabs (the document's
/// full path and state, roadmap item 39).
/// The pane/divider explanation under the pointer (slice R5c),
/// if any: the divider of the split view or the compare, the
/// compare's read-only pane, and the split's two panes each
/// carry a tooltip, like every other control.
fn note_candidate_at(app: &mut App, x_dip: f32, y_dip: f32) -> Option<HoverKind> {
    let layout = compute_layout(app);
    if y_dip < layout.editor_top || y_dip > layout.rows_bottom {
        return None;
    }
    if let Some(div) = layout.compare_div {
        if (x_dip - div - DIVIDER_W / 2.0).abs() <= 4.0 {
            return Some(HoverKind::Note("Drag to resize the panes"));
        }
        if x_dip >= div + DIVIDER_W {
            return Some(HoverKind::Note(
                "Compare pane \u{2014} read-only; it scrolls with the document",
            ));
        }
        return None;
    }
    if let Some(div) = layout.split_div {
        if (x_dip - div - DIVIDER_W / 2.0).abs() <= 4.0 {
            return Some(HoverKind::Note("Drag to resize the panes"));
        }
        if x_dip < div {
            return Some(HoverKind::Note("Text pane \u{2014} the document as text"));
        }
        return Some(HoverKind::Note("Hex pane \u{2014} the document as bytes"));
    }
    None
}

fn service_hover(app: &mut App) {
    if !app.hover_tool_ready || app.tooltip.is_null() {
        return;
    }
    // A shown preview survives only while the pointer still rests
    // on the same thing (the pointer may have left the window
    // entirely, which sends no mouse-move).
    if let Some(shown) = app.hover_shown {
        // SAFETY: POINT is a plain C struct; both calls are plain
        // coordinate queries/conversions.
        let mut pt: POINT = unsafe { std::mem::zeroed() };
        let still = unsafe {
            GetCursorPos(&mut pt);
            ScreenToClient(app.hwnd, &mut pt);
            match shown {
                HoverKind::Tick(..) => {
                    tick_candidate_at(app, app.dip(pt.x), app.dip(pt.y))
                        .map(|(o, b)| HoverKind::Tick(o, b))
                        == Some(shown)
                }
                HoverKind::Tab(index) => matches!(
                    tab_hit(app, app.dip(pt.x), app.dip(pt.y)),
                    TabHit::Tab(i) | TabHit::Close(i) if i == index
                ),
                HoverKind::Note(_) => {
                    note_candidate_at(app, app.dip(pt.x), app.dip(pt.y)) == Some(shown)
                }
            }
        };
        if !still {
            hide_hover(app);
        }
    }
    match app.hover_cand {
        Some((kind, since)) => {
            if app.hover_shown == Some(kind) {
                return;
            }
            if since.elapsed() < std::time::Duration::from_millis(350) {
                return;
            }
            let text = match kind {
                HoverKind::Tick(offset, is_bookmark) => {
                    let (line, _) = app.editor.line_col_of(offset);
                    let preview: String = app
                        .editor
                        .line_text(line)
                        .map(|text| text.chars().take(60).collect())
                        .unwrap_or_default();
                    if is_bookmark {
                        format!("Bookmark \u{00B7} Line {}: {}", line + 1, preview)
                    } else {
                        format!("Line {}: {}", line + 1, preview)
                    }
                }
                HoverKind::Tab(index) => tab_tooltip_text(app, index),
                HoverKind::Note(text) => text.to_string(),
            };
            set_hover_text(&text);
            show_hover_tooltip(app);
            app.hover_shown = Some(kind);
        }
        None => hide_hover(app),
    }
}

/// Activate the tracking tooltip at the cursor's position.
fn show_hover_tooltip(app: &mut App) {
    // SAFETY: POINT is a plain C struct; GetCursorPos fills it.
    let mut pt: POINT = unsafe { std::mem::zeroed() };
    // SAFETY: our tooltip window is live and the tool was
    // registered in create_children; TTTOOLINFOW is a plain C
    // struct — only the id fields are needed for activation.
    unsafe {
        GetCursorPos(&mut pt);
        let packed = ((pt.x as u32 & 0xFFFF) | ((pt.y as u32 & 0xFFFF) << 16)) as LPARAM;
        SendMessageW(app.tooltip, TTM_TRACKPOSITION, 0, packed);
        let mut info: TTTOOLINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        info.hwnd = app.hwnd;
        info.uId = TICK_TOOL_ID;
        SendMessageW(
            app.tooltip,
            TTM_TRACKACTIVATE,
            1,
            &info as *const TTTOOLINFOW as LPARAM,
        );
    }
}

/// Deactivate the tracking tooltip if it is showing.
fn hide_hover(app: &mut App) {
    if app.hover_shown.take().is_none() || app.tooltip.is_null() {
        return;
    }
    // SAFETY: our tooltip window is live; see show_hover_tooltip.
    unsafe {
        let mut info: TTTOOLINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        info.hwnd = app.hwnd;
        info.uId = TICK_TOOL_ID;
        SendMessageW(
            app.tooltip,
            TTM_TRACKACTIVATE,
            0,
            &info as *const TTTOOLINFOW as LPARAM,
        );
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
        // SAFETY: GetKeyState(0x11 = VK_CONTROL, 0x10 = VK_SHIFT)
        // is a plain state read.
        let ctrl = unsafe { GetKeyState(0x11) } as u16 & 0x8000 != 0;
        let shift = unsafe { GetKeyState(0x10) } as u16 & 0x8000 != 0;
        let cmd = match vk {
            0x0D => Some(CMD_FIND_NEXT),       // Enter: find next
            0x1B => Some(CMD_FIND_CLOSE),      // Esc: close the bar
            0x4F if ctrl => Some(CMD_OPEN),    // Ctrl+O
            0x53 if ctrl => Some(CMD_SAVE),    // Ctrl+S
            0x46 if ctrl => Some(CMD_FIND),    // Ctrl+F
            0x48 if ctrl => Some(CMD_REPLACE), // Ctrl+H
            // Tab commands work from the find bar too (roadmap
            // item 39): the find bar belongs to its document, so
            // switching tabs from here is ordinary.
            0x54 if ctrl => Some(CMD_NEW_TAB),           // Ctrl+T
            0x57 if ctrl => Some(CMD_CLOSE_TAB),         // Ctrl+W
            0x09 if ctrl && shift => Some(CMD_PREV_TAB), // Ctrl+Shift+Tab
            0x09 if ctrl => Some(CMD_NEXT_TAB),          // Ctrl+Tab
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
    // The canvas tracking tool for the scrollbar tick hover previews
    // (item 24 expansion): a tool on the main window itself, driven
    // manually (TTM_TRACKACTIVATE) so it only ever speaks for a tick
    // the pointer is resting on. Its text is swapped in place.
    app.tooltip = tooltip;
    set_hover_text("");
    {
        // SAFETY: TTTOOLINFOW is a plain C struct, zero-initialised
        // then filled; the text pointer stays fixed at the scratch
        // buffer for the process life (its contents are rewritten in
        // place by `set_hover_text` before each activation).
        let mut info: TTTOOLINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        info.uFlags = 0;
        info.hwnd = app.hwnd;
        info.uId = TICK_TOOL_ID;
        info.lpszText = HOVER_TEXT_SCRATCH.with(|s| s.borrow().as_ptr() as *mut u16);
        // SAFETY: the tooltip window is live; `info` outlives the call.
        let added = unsafe {
            SendMessageW(
                tooltip,
                TTM_ADDTOOLW,
                0,
                &info as *const TTTOOLINFOW as LPARAM,
            )
        };
        app.hover_tool_ready = added != 0;
    }

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
    // The "Regex" toggle (roadmap item 9, slice R5d): same
    // owner-drawn family, appended after the scope toggle in
    // find_controls (index 9; layout_children places it by index).
    app.find_regex_check = create_child(app, "Button", "Regex", button_style, 0, CMD_FIND_REGEX);
    app.find_controls.push(app.find_regex_check);
    add_tooltip(
        tooltip,
        app.hwnd,
        app.find_regex_check,
        "Search with a regular expression (whole document; UTF-8 and ANSI files)",
    );
    subclass_edit(app.find_edit, app.hwnd);
    subclass_edit(app.replace_edit, app.hwnd);

    // Status strip: two static controls, themed via WM_CTLCOLORSTATIC
    // — position on the left, document facts right-aligned (SS_RIGHT
    // = 0x2), both vertically centred (SS_CENTERIMAGE = 0x200).
    app.status = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE | 0x200, 0, 0);
    app.status_right = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE | 0x2 | 0x200, 0, 0);
    // The statics otherwise keep the window class's default font,
    // whose line height can exceed the strip; top-aligned, its
    // descenders shaved against the strip's bottom edge in the
    // field. Give both an app-sized font instead (a 13-DIP em at
    // the current scale). The handle is kept for the process life,
    // like the tooltip text buffers.
    let status_font: HFONT = {
        let face = wstr("Segoe UI");
        // SAFETY: plain creation parameters; the face name
        // outlives the call; weight 400, DEFAULT_CHARSET = 1,
        // CLEARTYPE_QUALITY = 5.
        unsafe {
            CreateFontW(
                -app.px(13.0),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                face.as_ptr(),
            )
        }
    };
    if !status_font.is_null() {
        // SAFETY: our controls; WM_SETFONT = 0x0030.
        unsafe {
            SendMessageW(app.status, 0x0030, status_font as WPARAM, 0);
            SendMessageW(app.status_right, 0x0030, status_font as WPARAM, 0);
        }
    }

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

/// A menu label with the effective chord for `action` appended as
/// the accelerator text (slice R4c, item 48): when the owner has
/// rebound or unbound the action in `data/keybindings.json`, the
/// menu shows the chord that actually works — or none.
fn menu_label(app: &App, base: &str, action: &str) -> String {
    match app.keybindings.label_for(action) {
        Some(label) => format!("{base}\t{label}"),
        None => base.to_string(),
    }
}

fn build_menus(app: &mut App) {
    // SAFETY: the whole menu tree is built from fresh popup menus and
    // installed on our window; nothing else holds these handles.
    unsafe {
        let bar = CreateMenu();

        let file = CreatePopupMenu();
        app.file_menu = file;
        // Tabs (roadmap item 39): New Tab opens a fresh empty
        // document tab; New Window starts a separate process;
        // Open in New Window does the same with the current file.
        menu_item(file, CMD_NEW_TAB, &menu_label(app, "New Tab", "new_tab"));
        menu_item(file, CMD_NEW_WINDOW, "New Window");
        menu_item(file, CMD_OPEN, &menu_label(app, "Open", "open"));
        menu_item(file, CMD_OPEN_IN_NEW_WINDOW, "Open in New Window");
        menu_sep(file);
        menu_item(file, CMD_SAVE, &menu_label(app, "Save", "save"));
        menu_item(file, CMD_SAVE_AS, "Save As\u{2026}");
        menu_item(file, CMD_SAVE_SELECTION, "Save Selection to File\u{2026}");
        menu_item(file, CMD_SPLIT_FILE, "Split File Copy\u{2026}");
        menu_item(file, CMD_FILE_HASHES, "File Hashes\u{2026}");
        // Slice R4c: slicing export, delta patches, structural
        // compare (items 2, 3 and 43).
        menu_item(file, CMD_EXTRACT_RANGES, "Extract Ranges\u{2026}");
        menu_item(file, CMD_EXPORT_PATCH, "Export Delta Patch\u{2026}");
        menu_item(file, CMD_APPLY_PATCH, "Apply Patch\u{2026}");
        menu_item(file, CMD_COMPARE_WITH, "Compare With\u{2026}");
        // Side-by-side visual diff (slice R5c): toggles the
        // compare pane; checked while one is open.
        menu_item(file, CMD_COMPARE_SIDE, "Compare Side by Side\u{2026}");
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
        // Protected files (item 33): relaunch elevated on the
        // current file (the handler explains before the UAC
        // prompt; greyed while no file is open — see
        // WM_INITMENUPOPUP).
        menu_item(file, CMD_OPEN_ADMIN, "Open as Administrator");
        menu_sep(file);
        menu_item(
            file,
            CMD_CLOSE_TAB,
            &menu_label(app, "Close Tab", "close_tab"),
        );
        menu_item(file, CMD_EXIT, "Exit");

        let edit = CreatePopupMenu();
        menu_item(edit, CMD_UNDO, &menu_label(app, "Undo", "undo"));
        menu_item(edit, CMD_REDO, &menu_label(app, "Redo", "redo"));
        menu_sep(edit);
        menu_item(edit, CMD_CUT, &menu_label(app, "Cut", "cut"));
        menu_item(edit, CMD_COPY, &menu_label(app, "Copy", "copy"));
        menu_item(edit, CMD_PASTE, &menu_label(app, "Paste", "paste"));
        menu_sep(edit);
        menu_item(edit, CMD_FIND, &menu_label(app, "Find", "find"));
        menu_item(edit, CMD_REPLACE, &menu_label(app, "Replace", "replace"));
        menu_sep(edit);
        menu_item(
            edit,
            CMD_SELECT_ALL,
            &menu_label(app, "Select All", "select_all"),
        );
        menu_item(edit, CMD_INSERT_ZEROES, "Insert Zeroes\u{2026}");
        menu_sep(edit);
        menu_item(edit, CMD_SORT_LINES, "Sort Lines in Selection");
        menu_item(edit, CMD_DEDUP_LINES, "Remove Duplicate Lines in Selection");

        let search = CreatePopupMenu();
        menu_item(search, CMD_FIND, &menu_label(app, "Find\u{2026}", "find"));
        menu_item(
            search,
            CMD_REPLACE,
            &menu_label(app, "Replace\u{2026}", "replace"),
        );
        menu_sep(search);
        menu_item(
            search,
            CMD_FIND_NEXT,
            &menu_label(app, "Find Next", "find_next"),
        );
        menu_item(
            search,
            CMD_FIND_PREV,
            &menu_label(app, "Find Previous", "find_prev"),
        );
        menu_sep(search);
        // Structural filter (roadmap item 18) and the jump history
        // (item 11 wiring). Clear Filter (slice R4c, item 12) is
        // enabled only while a filter is up (WM_INITMENUPOPUP).
        menu_item(search, CMD_FILTER, "Filter\u{2026}");
        menu_item(search, CMD_CLEAR_FILTER, "Clear Filter");
        menu_sep(search);
        // Bookmarks (roadmap item 20) with the annotation editor
        // (slice R4c, item 5).
        let bookmarks_menu = CreatePopupMenu();
        menu_item(
            bookmarks_menu,
            CMD_BOOKMARK_TOGGLE,
            &menu_label(app, "Toggle Bookmark", "toggle_bookmark"),
        );
        menu_item(
            bookmarks_menu,
            CMD_BOOKMARK_NEXT,
            &menu_label(app, "Next Bookmark", "bookmark_next"),
        );
        menu_item(
            bookmarks_menu,
            CMD_BOOKMARK_PREV,
            &menu_label(app, "Previous Bookmark", "bookmark_prev"),
        );
        menu_item(bookmarks_menu, CMD_BOOKMARK_EDIT, "Edit Bookmark\u{2026}");
        menu_popup(search, bookmarks_menu, "Bookmarks");
        menu_sep(search);
        menu_item(search, CMD_JUMP_BACK, &menu_label(app, "Back", "jump_back"));
        menu_item(
            search,
            CMD_JUMP_FORWARD,
            &menu_label(app, "Forward", "jump_forward"),
        );
        menu_sep(search);
        // Side-by-side diff (slice R5c): recompute the regions,
        // then walk them. All three grey unless a compare with
        // computed differences is open (WM_INITMENUPOPUP).
        menu_item(search, CMD_DIFF_RECOMPUTE, "Compare Differences");
        menu_item(
            search,
            CMD_DIFF_NEXT,
            &menu_label(app, "Next Difference", "diff_next"),
        );
        menu_item(
            search,
            CMD_DIFF_PREV,
            &menu_label(app, "Previous Difference", "diff_prev"),
        );

        let view = CreatePopupMenu();
        app.view_menu = view;
        menu_item(view, CMD_MODE_TEXT, "Text Mode");
        menu_item(
            view,
            CMD_MODE_HEX,
            &menu_label(app, "Hex Mode", "toggle_hex"),
        );
        menu_item(
            view,
            CMD_SPLIT_VIEW,
            &menu_label(app, "Split Text/Hex", "toggle_split"),
        );
        menu_sep(view);
        menu_item(view, CMD_INSPECTOR, "Data Inspector");
        menu_item(view, CMD_DISASM, "Disassembly");
        menu_item(view, CMD_STRINGS, "Strings Pane");
        menu_sep(view);
        // Roadmap slice R4b view features. Follow Tail and Word
        // Wrap persist; the rest are session view switches.
        menu_item(
            view,
            CMD_FOLLOW_TAIL,
            &menu_label(app, "Follow Tail", "follow_tail"),
        );
        menu_item(view, CMD_WORD_WRAP, "Word Wrap");
        menu_sep(view);
        menu_item(view, CMD_GRID, "Delimiter Grid");
        menu_item(view, CMD_ENTROPY, "Entropy Strip");
        menu_item(view, CMD_TIME_DELTAS, "Time Deltas");
        menu_item(view, CMD_FREEZE, "Freeze Top Lines\u{2026}");
        menu_item(view, CMD_DIM, "Dim Inactive Paragraphs");
        menu_sep(view);
        menu_item(view, CMD_ANSI, "ANSI Colours");
        menu_item(view, CMD_CTL_CHARS, "Show Control Characters");
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
        menu_item(
            view,
            CMD_FONT_BIGGER,
            &menu_label(app, "Font Size +", "font_bigger"),
        );
        menu_item(
            view,
            CMD_FONT_SMALLER,
            &menu_label(app, "Font Size \u{2212}", "font_smaller"),
        );
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

        // Encoding (slice R4c, item 29): the four standard
        // encodings as radios, plus the Code Pages submenu for
        // the legacy single/double-byte pages. Detection is
        // unchanged — a page is only ever active because the
        // owner picked it here.
        let encoding = CreatePopupMenu();
        app.encoding_menu = encoding;
        for (i, name) in ["UTF-8", "UTF-16 LE", "UTF-16 BE", "ANSI (Windows-1252)"]
            .iter()
            .enumerate()
        {
            menu_item(encoding, CMD_ENC_BASE + i, name);
        }
        menu_sep(encoding);
        app.codepage_menu = CreatePopupMenu();
        menu_item(app.codepage_menu, CMD_CODEPAGE_BASE, "None (detected)");
        menu_sep(app.codepage_menu);
        for (i, page) in crate::encoding::CodePage::ALL.iter().enumerate() {
            menu_item(
                app.codepage_menu,
                CMD_CODEPAGE_BASE + 1 + i,
                page.display_name(),
            );
        }
        menu_popup(encoding, app.codepage_menu, "Code Pages");

        let help = CreatePopupMenu();
        menu_item(help, CMD_GUIDE, &menu_label(app, "Built-in Guide", "guide"));
        menu_item(help, CMD_ABOUT, "About VeloEdit");

        menu_popup(bar, file, "File");
        menu_popup(bar, edit, "Edit");
        menu_popup(bar, search, "Search");
        menu_popup(bar, view, "View");
        menu_popup(bar, encoding, "Encoding");
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
        // Roadmap slice R4b view toggles: checked while on. Freeze
        // shows its check while any lines are frozen.
        for (id, on) in [
            (CMD_FOLLOW_TAIL, app.settings.follow_tail),
            (CMD_WORD_WRAP, app.settings.word_wrap),
            (CMD_STRINGS, app.settings.strings_open),
            (CMD_GRID, app.grid_on),
            (CMD_ENTROPY, app.entropy_on),
            (CMD_TIME_DELTAS, app.deltas_on),
            (CMD_FREEZE, app.settings.freeze_lines > 0),
            (CMD_DIM, app.dim_on),
            (CMD_ANSI, app.ansi_on),
            (CMD_CTL_CHARS, app.ctl_on),
            (CMD_DISASM, app.disasm_on),
        ] {
            CheckMenuItem(
                app.view_menu,
                id as u32,
                MF_BYCOMMAND | if on { MF_CHECKED } else { 0 },
            );
        }
        // Encoding radios and Code Pages checks (slice R4c, item
        // 29): the standard group reflects the active standard
        // encoding; while a code page is active no standard item
        // is checked and the page's own item is.
        if !app.encoding_menu.is_null() {
            let enc_idx = match app.editor.codec() {
                crate::encoding::TextCodec::Standard(crate::encoding::TextEncoding::Utf8) => {
                    Some(0)
                }
                crate::encoding::TextCodec::Standard(crate::encoding::TextEncoding::Utf16Le) => {
                    Some(1)
                }
                crate::encoding::TextCodec::Standard(crate::encoding::TextEncoding::Utf16Be) => {
                    Some(2)
                }
                crate::encoding::TextCodec::Standard(crate::encoding::TextEncoding::Ansi) => {
                    Some(3)
                }
                crate::encoding::TextCodec::Page(_) => None,
            };
            for i in 0..4 {
                CheckMenuItem(
                    app.encoding_menu,
                    (CMD_ENC_BASE + i) as u32,
                    MF_BYCOMMAND | if enc_idx == Some(i) { MF_CHECKED } else { 0 },
                );
            }
        }
        if !app.codepage_menu.is_null() {
            let page_idx = match app.editor.code_page() {
                None => 0,
                Some(page) => {
                    1 + crate::encoding::CodePage::ALL
                        .iter()
                        .position(|&p| p == page)
                        .unwrap_or(0)
                }
            };
            for i in 0..=crate::encoding::CodePage::ALL.len() {
                CheckMenuItem(
                    app.codepage_menu,
                    (CMD_CODEPAGE_BASE + i) as u32,
                    MF_BYCOMMAND | if i == page_idx { MF_CHECKED } else { 0 },
                );
            }
        }
        // Split view and compare checks (slice R5c): the split
        // item is checked while the split is actually in effect
        // (a filter or a compare suspends it, and the check
        // tells that truth); the compare item while one is open.
        CheckMenuItem(
            app.view_menu,
            CMD_SPLIT_VIEW as u32,
            MF_BYCOMMAND | if split_active(app) { MF_CHECKED } else { 0 },
        );
        if !app.file_menu.is_null() {
            CheckMenuItem(
                app.file_menu,
                CMD_COMPARE_SIDE as u32,
                MF_BYCOMMAND | if app.compare.is_some() { MF_CHECKED } else { 0 },
            );
        }
        // Save Options radios (roadmap items 31/32).
        if !app.save_options_menu.is_null() {
            let eol_idx = match app.save_eol {
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
            let bom_idx = match app.save_bom {
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
    app.status_note = None;
    let page = page_rows(app);
    app.editor.ensure_caret_visible(page);
    ensure_caret_segment_visible(app);
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
    // The filter view is a text-view concept; switching mode drops
    // it rather than leaving it describing the wrong view.
    app.filter = None;
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
    // The filter view scrolls its own rows (R4b).
    let page = page_rows(app) as usize;
    if let Some(filter) = &mut app.filter {
        let max_first = filter.view.len().saturating_sub(page);
        let next = (filter.first_row as i64 + delta).clamp(0, max_first as i64);
        filter.first_row = next as usize;
        invalidate(app);
        return;
    }
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

/// Set the status bar's transient note and repaint: the
/// shell's way of declining or explaining without a dialog.
fn set_status_note(app: &mut App, text: &str) {
    app.status_note = Some(text.to_string());
    refresh_status(app);
    invalidate(app);
}

fn on_command(app: &mut App, id: usize) {
    // The filter view is read-only: editing commands are refused
    // while it is up (typing itself is swallowed in on_char /
    // on_keydown, which never reach this far).
    if app.filter.is_some()
        && matches!(
            id,
            CMD_UNDO
                | CMD_REDO
                | CMD_CUT
                | CMD_PASTE
                | CMD_SORT_LINES
                | CMD_DEDUP_LINES
                | CMD_DO_REPLACE
                | CMD_DO_REPLACE_ALL
                | CMD_SAVE_SELECTION
                | CMD_INSERT_ZEROES
        )
    {
        app.status_note = Some("Close the filter (Esc) to edit".to_string());
        refresh_status(app);
        return;
    }
    // Find / Replace work on the document the filter froze: close
    // the filter first, then run the command as asked.
    if app.filter.is_some() && matches!(id, CMD_FIND | CMD_REPLACE | CMD_FIND_NEXT | CMD_FIND_PREV)
    {
        exit_filter(app);
    }
    // Freeze pane: paste and cut are refused at a frozen caret
    // like typed edits (typing itself checks in on_char /
    // on_keydown).
    if matches!(id, CMD_PASTE | CMD_CUT) && edit_blocked_by_freeze(app, false, false) {
        frozen_note(app);
        return;
    }
    match id {
        CMD_OPEN => {
            // Tabs (roadmap item 39): opening never asks about
            // the current document — an untouched empty tab is
            // replaced in place, everything else opens in a new
            // tab (see open_path_from_ui and the Guide).
            if let Some(path) = file_dialog(app, false, None) {
                open_path_from_ui(app, &path);
            }
        }
        CMD_NEW_TAB => new_tab(app),
        CMD_CLOSE_TAB => {
            request_close_tab(app, app.tabs.active());
        }
        CMD_NEXT_TAB => {
            let next = app.tabs.next();
            switch_tab(app, next);
        }
        CMD_PREV_TAB => {
            let prev = app.tabs.prev();
            switch_tab(app, prev);
        }
        CMD_NEW_WINDOW => spawn_new_window(app, None),
        CMD_OPEN_IN_NEW_WINDOW => {
            if let Some(path) = app.editor.path().map(|p| p.to_path_buf()) {
                spawn_new_window(app, Some(&path));
            }
        }
        CMD_TAB_CLOSE => {
            if let Some(index) = app.tab_context.take() {
                request_close_tab(app, index);
            }
        }
        CMD_TAB_CLOSE_OTHERS => {
            if let Some(index) = app.tab_context.take() {
                let keep = tab_doc_id(app, index);
                close_other_tabs(app, keep);
            }
        }
        CMD_TAB_CLOSE_ALL => {
            app.tab_context = None;
            close_all_tabs(app);
        }
        CMD_TAB_COPY_PATH => {
            if let Some(index) = app.tab_context.take() {
                if let Some(path) = tab_path(app, index) {
                    clipboard_set_text(app.hwnd, &path.display().to_string());
                }
            }
        }
        CMD_TAB_NEW_WINDOW => {
            if let Some(index) = app.tab_context.take() {
                if let Some(path) = tab_path(app, index) {
                    spawn_new_window(app, Some(&path));
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
            // A live rectangle cuts as its extracted text plus
            // one rect delete (slice R5d).
            if let Some(rect) = app.rect_sel {
                let eol = app.editor.line_ending();
                let text = app.editor.rect_extract_text(
                    rect.first_line,
                    rect.last_line,
                    rect.col_start,
                    rect.col_end,
                    eol,
                );
                clipboard_set_text(app.hwnd, &text);
                if app
                    .editor
                    .rect_delete(
                        rect.first_line,
                        rect.last_line,
                        rect.col_start,
                        rect.col_end,
                    )
                    .unwrap_or(false)
                {
                    app.rect_sel = None;
                    app.extra_carets.clear();
                    after_edit(app);
                }
            } else if let Some(text) = copy_text(&mut app.editor) {
                clipboard_set_text(app.hwnd, &text);
                if app.editor.delete_selection().unwrap_or(false) {
                    after_edit(app);
                }
            }
        }
        CMD_COPY => {
            if let Some(rect) = app.rect_sel {
                // Rectangle copy: the extracted columns, joined
                // with the document's line ending (slice R5d).
                let eol = app.editor.line_ending();
                let text = app.editor.rect_extract_text(
                    rect.first_line,
                    rect.last_line,
                    rect.col_start,
                    rect.col_end,
                    eol,
                );
                clipboard_set_text(app.hwnd, &text);
            } else if let Some(text) = copy_text(&mut app.editor) {
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
        CMD_FIND_REGEX => {
            // The "Regex" toggle (roadmap item 9, slice R5d):
            // state, ✓ prefix, accent draw state — the scope
            // toggle's exact pattern. Regex searches the whole
            // document, so turning it on turns the scope off
            // rather than letting the two silently combine.
            app.find_regex = !app.find_regex;
            if app.find_regex && app.find_in_selection {
                app.find_in_selection = false;
                app.find_scope_range = None;
                app.last_scope_range = None;
                set_control_text(app.find_scope_check, "In selection");
                // SAFETY: our find-bar button.
                unsafe { InvalidateRect(app.find_scope_check, std::ptr::null(), 1) };
            }
            set_control_text(
                app.find_regex_check,
                if app.find_regex {
                    "\u{2713} Regex"
                } else {
                    "Regex"
                },
            );
            // SAFETY: our find-bar button.
            unsafe {
                InvalidateRect(app.find_regex_check, std::ptr::null(), 1);
                SetFocus(app.find_edit);
            }
            find_from_bar(app, 0);
        }
        CMD_APPLY_TEMPLATE => show_template_dialog(app),
        CMD_TOOLS_RELOAD => reload_tools(app),
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
        // The Text/Hex commands name a single view: with no
        // split they switch mode; while split they LEAVE the
        // split for the named view (field report: there was no
        // way back to one view from them). Focus inside the
        // split still moves by clicking a pane or pressing F6,
        // which keeps its toggle meaning below.
        CMD_MODE_TEXT => mode_command(app, EditorMode::Text),
        CMD_MODE_HEX => mode_command(app, EditorMode::Hex),
        CMD_MODE_TOGGLE => {
            let hex = app.editor.mode() == EditorMode::Text;
            split_set_focus(app, hex);
        }
        CMD_SPLIT_VIEW => {
            // Text/hex split view (roadmap item 28, slice R5c).
            // The split, a filter, frozen lines, and a compare
            // each own the canvas in ways that do not compose;
            // the declines are status notes, never half-states.
            if app.compare.is_some() {
                set_status_note(
                    app,
                    "Close the compare to split text and hex \u{2014} both need the right side.",
                );
            } else if !app.settings.split_view && app.filter.is_some() {
                set_status_note(app, "Exit the filter view to split text and hex.");
            } else if !app.settings.split_view && app.settings.freeze_lines > 0 {
                set_status_note(
                    app,
                    "Unfreeze the top lines to split text and hex (Freeze Top Lines, 0 = off).",
                );
            } else {
                app.settings.split_view = !app.settings.split_view;
                save_settings(app);
                if app.settings.split_view && app.grid_on {
                    set_status_note(
                        app,
                        "The delimiter grid steps aside while split; it returns when split is off.",
                    );
                }
                refresh_menu_checks(app);
                sync_hex_layout(app);
                refresh_status(app);
                invalidate(app);
            }
        }
        CMD_COMPARE_SIDE => open_compare(app),
        CMD_DIFF_NEXT => diff_jump(app, true),
        CMD_DIFF_PREV => diff_jump(app, false),
        CMD_DIFF_RECOMPUTE => {
            if app.compare.is_some() && app.editor.mode() == EditorMode::Text {
                start_diff(app);
                refresh_status(app);
                invalidate(app);
            } else if app.compare.is_some() {
                set_status_note(
                    app,
                    "Differences are computed in text mode; hex compare scrolls in step.",
                );
            }
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
        CMD_DISASM => {
            // Disassembly panel (slice R5d): hex mode only. The
            // toggle is session state; in text mode a note says
            // where it will appear.
            app.disasm_on = !app.disasm_on;
            if app.disasm_on {
                app.disasm_stale = true;
                if app.editor.mode() == EditorMode::Text {
                    app.status_note = Some("Disassembly appears in hex mode".to_string());
                    refresh_status(app);
                }
            }
            refresh_menu_checks(app);
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
        CMD_FOLLOW_TAIL => {
            // Follow Tail / Ghost Tail (roadmap item 22): persisted.
            app.settings.follow_tail = !app.settings.follow_tail;
            save_settings(app);
            refresh_menu_checks(app);
            refresh_status(app);
        }
        CMD_WORD_WRAP => {
            // Word wrap (roadmap item 19): persisted, text mode
            // only, view-only.
            app.settings.word_wrap = !app.settings.word_wrap;
            save_settings(app);
            refresh_menu_checks(app);
            if app.settings.word_wrap {
                ensure_caret_segment_visible(app);
            }
            invalidate(app);
        }
        CMD_STRINGS => {
            app.settings.strings_open = !app.settings.strings_open;
            save_settings(app);
            refresh_menu_checks(app);
            layout_children(app);
            invalidate(app);
        }
        CMD_GRID => {
            app.grid_on = !app.grid_on;
            if app.grid_on && split_active(app) {
                set_status_note(
                    app,
                    "The delimiter grid steps aside while split; it returns when split is off.",
                );
            }
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_ENTROPY => {
            app.entropy_on = !app.entropy_on;
            refresh_menu_checks(app);
            layout_children(app);
            invalidate(app);
        }
        CMD_TIME_DELTAS => {
            app.deltas_on = !app.deltas_on;
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_DIM => {
            app.dim_on = !app.dim_on;
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_ANSI => {
            app.ansi_on = !app.ansi_on;
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_CTL_CHARS => {
            app.ctl_on = !app.ctl_on;
            refresh_menu_checks(app);
            invalidate(app);
        }
        CMD_FREEZE => {
            // Freeze Top Lines (freeze pane): prompt for the count;
            // 0 clears. Persisted, so the choice survives restarts.
            // The frozen block belongs to the single canvas: while
            // split, freezing declines with a note (slice R5c).
            if app.settings.split_view {
                set_status_note(
                    app,
                    "Turn off Split Text/Hex to freeze lines \u{2014} the frozen block belongs to the single view.",
                );
                return;
            }
            let current = app.settings.freeze_lines.to_string();
            if let Some(text) = show_prompt(
                app,
                "Freeze Top Lines",
                "Lines to pin at the top (0-50, 0 = off):",
                &current,
            ) {
                match text.trim().parse::<u32>() {
                    Ok(n) if n <= 50 => {
                        app.settings.freeze_lines = n;
                        save_settings(app);
                        refresh_menu_checks(app);
                        invalidate(app);
                    }
                    _ => {
                        message_box(
                            app.hwnd,
                            "Enter a whole number from 0 to 50.",
                            "Freeze Top Lines",
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
            }
        }
        CMD_FILTER => {
            // The filter view replaces the canvas; while split
            // it declines with a note (slice R5c), the mirror of
            // the split's own decline while a filter is up.
            if app.settings.split_view {
                set_status_note(app, "Turn off Split Text/Hex to use the filter view.");
            } else {
                open_filter_prompt(app);
            }
        }
        CMD_CLEAR_FILTER => {
            exit_filter(app);
        }
        CMD_BOOKMARK_TOGGLE => {
            toggle_bookmark(app);
        }
        CMD_BOOKMARK_NEXT => {
            jump_bookmark(app, true);
        }
        CMD_BOOKMARK_PREV => {
            jump_bookmark(app, false);
        }
        CMD_BOOKMARK_EDIT => {
            // Edit the annotation of the bookmark on the caret's
            // line (slice R4c, item 5).
            let line = app.editor.line_col_of(app.editor.caret()).0;
            match bookmark_on_line(app, line) {
                Some(offset) => edit_bookmark_dialog(app, offset),
                None => {
                    app.status_note = Some("No bookmark on this line to edit".to_string());
                    refresh_status(app);
                }
            }
        }
        CMD_INSERT_ZEROES => {
            do_insert_zeroes(app);
        }
        CMD_EXTRACT_RANGES => {
            do_extract_ranges(app);
        }
        CMD_EXPORT_PATCH => {
            do_export_patch(app);
        }
        CMD_APPLY_PATCH => {
            do_apply_patch(app);
        }
        CMD_COMPARE_WITH => {
            do_compare_with(app);
        }
        CMD_OPEN_ADMIN => {
            do_open_admin(app);
        }
        _ if (CMD_ENC_BASE..CMD_ENC_BASE + 4).contains(&id) => {
            // Encoding menu (slice R4c, item 29): reinterpret the
            // document under a standard encoding. The bytes do not
            // change — only how they decode for display, typing
            // and search, and what a save writes back.
            let enc = [
                encoding::TextEncoding::Utf8,
                encoding::TextEncoding::Utf16Le,
                encoding::TextEncoding::Utf16Be,
                encoding::TextEncoding::Ansi,
            ][id - CMD_ENC_BASE];
            app.editor.set_encoding(enc);
            refresh_menu_checks(app);
            refresh_status(app);
            invalidate(app);
        }
        _ if (CMD_CODEPAGE_BASE..=CMD_CODEPAGE_BASE + crate::encoding::CodePage::ALL.len())
            .contains(&id) =>
        {
            // Code Pages submenu (slice R4c, item 29): 0 clears
            // back to the detected standard encoding.
            let page = if id == CMD_CODEPAGE_BASE {
                None
            } else {
                Some(crate::encoding::CodePage::ALL[id - CMD_CODEPAGE_BASE - 1])
            };
            app.editor.set_code_page(page);
            refresh_menu_checks(app);
            refresh_status(app);
            invalidate(app);
        }
        CMD_JUMP_BACK => {
            jump_history_move(app, false);
        }
        CMD_JUMP_FORWARD => {
            jump_history_move(app, true);
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
            // The choice arms the ACTIVE document (roadmap item
            // 39: save options are per-tab) and is remembered in
            // the settings as the default for new documents.
            app.settings.save_eol = match id - CMD_SAVE_EOL_BASE {
                1 => crate::settings::SAVE_EOL_LF.to_string(),
                2 => crate::settings::SAVE_EOL_CRLF.to_string(),
                _ => crate::settings::SAVE_EOL_KEEP.to_string(),
            };
            app.save_eol = app.settings.save_eol_choice();
            save_settings(app);
            refresh_menu_checks(app);
            layout_children(app);
            refresh_status(app);
        }
        _ if (CMD_SAVE_BOM_BASE..CMD_SAVE_BOM_BASE + 3).contains(&id) => {
            // Save Options ▸ Byte-Order Mark (roadmap item 32):
            // per-document arming, as with line endings above.
            app.settings.save_bom = match id - CMD_SAVE_BOM_BASE {
                1 => crate::settings::SAVE_BOM_ADD.to_string(),
                2 => crate::settings::SAVE_BOM_REMOVE.to_string(),
                _ => crate::settings::SAVE_BOM_KEEP.to_string(),
            };
            app.save_bom = app.settings.save_bom_choice();
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
            // Recent files follow the same open-into-tab rule as
            // File ▸ Open (roadmap item 39).
            if let Some(path) = app.settings.recent.get(id - CMD_RECENT_BASE).cloned() {
                open_path_from_ui(app, Path::new(&path));
            }
        }
        _ if (CMD_TOOL_BASE..CMD_TOOL_BASE + 64).contains(&id) => {
            // One external tool from data/tools.json (item 49).
            if let Some(tool) = app.tools.tools.get(id - CMD_TOOL_BASE).cloned() {
                run_tool(app, &tool);
            }
        }
        _ => {}
    }
}

/// Handle WM_KEYDOWN for the main window. Returns true when handled.
/// Virtual-key codes are the stable Win32 numbers, written literally.
/// Whether an edit at the caret would touch the frozen block
/// (R4b freeze pane): the caret or selection starts inside it, or
/// the edit merges across its lower edge (Backspace at the first
/// unfrozen line's start, Delete at the last frozen line's end).
/// Freeze applies to the plain text view only.
fn edit_blocked_by_freeze(app: &mut App, backspace: bool, delete: bool) -> bool {
    let freeze = app.settings.freeze_lines as u64;
    if freeze == 0
        || app.editor.mode() != EditorMode::Text
        || app.filter.is_some()
        || grid_active(app)
    {
        return false;
    }
    let start = app
        .editor
        .selection_range()
        .map(|(s, _)| s)
        .unwrap_or_else(|| app.editor.caret());
    let (line, col) = app.editor.line_col_of(start);
    if line < freeze {
        return true;
    }
    if backspace && line == freeze && col == 0 {
        return true;
    }
    if delete && line + 1 == freeze {
        let chars = app.editor.line_char_count(line);
        if col >= chars {
            return true;
        }
    }
    false
}

/// Refuse an edit into the frozen block, saying why in the status
/// bar.
fn frozen_note(app: &mut App) {
    app.status_note =
        Some("That line is frozen \u{2014} View \u{25B8} Freeze Top Lines changes it".to_string());
    refresh_status(app);
}

/// After an Up / PageUp move, keep the caret out of the frozen
/// block: scrolling up stops at the first unfrozen line (the block
/// can still be entered by clicking or jumping — viewing is free;
/// only editing is refused there).
fn clamp_freeze_up(app: &mut App, before_line: u64) {
    let freeze = app.settings.freeze_lines as u64;
    if freeze == 0 || app.filter.is_some() || grid_active(app) {
        return;
    }
    let (now, col) = app.editor.line_col_of(app.editor.caret());
    if before_line >= freeze && now < freeze {
        if let Some(off) = app.editor.offset_of_line_col(freeze, col) {
            app.editor.move_caret_to(off, false);
        }
    }
}

/// Display-row Up/Down under word wrap (R4b): move the caret one
/// DISPLAY row, preserving the in-segment column. Returns false
/// when wrap is off or the caret's segment is not in view (the
/// caller then falls back to the editor's line move).
fn wrap_move(app: &mut App, up: bool, extend: bool) -> bool {
    if !wrap_active(app) || app.editor.mode() != EditorMode::Text {
        return false;
    }
    let page = page_rows(app) as usize + 1;
    let rows = display_rows(app, page);
    let (line, col) = app.editor.line_col_of(app.editor.caret());
    let line_chars = app.editor.line_char_count(line);
    let mut caret_idx: Option<usize> = None;
    for (i, row) in rows.iter().enumerate() {
        if row.line_no == line
            && col >= row.char_start
            && (col < row.char_start + row.text.chars().count() as u64
                || row.char_start + row.text.chars().count() as u64 == line_chars)
        {
            caret_idx = Some(i);
            break;
        }
    }
    let Some(i) = caret_idx else {
        return false;
    };
    let target = if up {
        i.checked_sub(1)
    } else if i + 1 < rows.len() {
        Some(i + 1)
    } else {
        None
    };
    let Some(t) = target else {
        return false;
    };
    // Scrolling up stops at the frozen block's edge (see
    // clamp_freeze_up for the line-move path).
    if rows[t].frozen && !rows[i].frozen && !extend {
        return true;
    }
    let in_seg = col - rows[i].char_start;
    let trow = &rows[t];
    let tcount = trow.text.chars().count() as u64;
    let tcol = trow.char_start + in_seg.min(tcount);
    let tline = trow.line_no;
    if let Some(off) = app.editor.offset_of_line_col(tline, tcol) {
        app.editor.move_caret_to(off, extend);
    }
    true
}

/// Translate a virtual-key + modifier state into a keybinding
/// chord (slice R4c, item 7). Keys outside the chord vocabulary
/// (Backspace, Tab, …) return `None` and keep their hard-wired
/// handling in [`on_keydown`].
fn chord_from_vk(vk: u32, ctrl: bool, shift: bool, alt: bool) -> Option<crate::keybindings::Chord> {
    use crate::keybindings::{Key, NamedKey};
    let key = match vk {
        0x41..=0x5A | 0x30..=0x39 => Key::Char(char::from_u32(vk).unwrap_or('?')),
        0x70..=0x87 => Key::Function((vk - 0x70 + 1) as u8),
        0x25 => Key::Named(NamedKey::Left),
        0x27 => Key::Named(NamedKey::Right),
        0x26 => Key::Named(NamedKey::Up),
        0x28 => Key::Named(NamedKey::Down),
        0x24 => Key::Named(NamedKey::Home),
        0x23 => Key::Named(NamedKey::End),
        0x21 => Key::Named(NamedKey::PageUp),
        0x22 => Key::Named(NamedKey::PageDown),
        0x2E => Key::Named(NamedKey::Delete),
        0x2D => Key::Named(NamedKey::Insert),
        0x1B => Key::Named(NamedKey::Escape),
        0x0D => Key::Named(NamedKey::Enter),
        0x20 => Key::Named(NamedKey::Space),
        0x09 => Key::Named(NamedKey::Tab), // VK_TAB: next/previous tab
        0xBB => Key::Char('+'),            // VK_OEM_PLUS
        0xBD => Key::Char('-'),            // VK_OEM_MINUS
        0xDC => Key::Char('\\'),           // VK_OEM_5: Ctrl+\ toggles the split
        _ => return None,
    };
    Some(crate::keybindings::Chord {
        ctrl,
        alt,
        shift,
        key,
    })
}

/// Run one rebindable action by name (slice R4c, item 7): the
/// keybinding dispatch table. Each arm is exactly the handler the
/// hard-wired shortcut used to call, so default bindings behave
/// identically to the pre-keybindings shell.
fn dispatch_action(app: &mut App, action: &str) {
    match action {
        "open" => on_command(app, CMD_OPEN),
        "save" => on_command(app, CMD_SAVE),
        "save_as" => on_command(app, CMD_SAVE_AS),
        "undo" => on_command(app, CMD_UNDO),
        "redo" => on_command(app, CMD_REDO),
        "cut" => on_command(app, CMD_CUT),
        "copy" => on_command(app, CMD_COPY),
        "paste" => on_command(app, CMD_PASTE),
        "select_all" => {
            app.editor.select_all();
            after_caret_move(app);
        }
        "find" => on_command(app, CMD_FIND),
        "replace" => on_command(app, CMD_REPLACE),
        "find_next" => on_command(app, CMD_FIND_NEXT),
        "find_prev" => on_command(app, CMD_FIND_PREV),
        "toggle_hex" => on_command(app, CMD_MODE_TOGGLE),
        "guide" => show_help(app),
        "font_bigger" => on_command(app, CMD_FONT_BIGGER),
        "font_smaller" => on_command(app, CMD_FONT_SMALLER),
        "toggle_bookmark" => toggle_bookmark(app),
        "bookmark_next" => jump_bookmark(app, true),
        "bookmark_prev" => jump_bookmark(app, false),
        "jump_back" => jump_history_move(app, false),
        "jump_forward" => jump_history_move(app, true),
        "pointer_follow" => {
            // Ctrl+Enter: pointer following in hex mode (roadmap
            // item 11 wiring).
            if app.editor.mode() == EditorMode::Hex {
                pointer_jump(app);
            }
        }
        "follow_tail" => on_command(app, CMD_FOLLOW_TAIL),
        "filter" => on_command(app, CMD_FILTER),
        "clear_filter" => on_command(app, CMD_CLEAR_FILTER),
        // Split view and diff navigation (slice R5c).
        "toggle_split" => on_command(app, CMD_SPLIT_VIEW),
        "caret_up" => add_caret_at_line(app, false),
        "caret_down" => add_caret_at_line(app, true),
        "diff_next" => on_command(app, CMD_DIFF_NEXT),
        "diff_prev" => on_command(app, CMD_DIFF_PREV),
        // Tabs (roadmap item 39, slice R5b).
        "new_tab" => on_command(app, CMD_NEW_TAB),
        "close_tab" => on_command(app, CMD_CLOSE_TAB),
        "next_tab" => on_command(app, CMD_NEXT_TAB),
        "prev_tab" => on_command(app, CMD_PREV_TAB),
        "tab_last" => {
            let last = app.tabs.count() - 1;
            switch_tab(app, last);
        }
        _ => {
            // Ctrl+1..8 jump to tab N (tab_1 .. tab_8).
            if let Some(n) = action
                .strip_prefix("tab_")
                .and_then(|rest| rest.parse::<usize>().ok())
            {
                if (1..=8).contains(&n) {
                    let target = (n - 1).min(app.tabs.count() - 1);
                    switch_tab(app, target);
                }
            }
        }
    }
}

fn on_keydown(app: &mut App, wparam: WPARAM) -> bool {
    let vk = wparam as u32;
    // SAFETY: plain key-state reads (VK_CONTROL = 0x11, VK_SHIFT = 0x10).
    let ctrl = unsafe { GetKeyState(0x11) } as u16 & 0x8000 != 0;
    let shift = unsafe { GetKeyState(0x10) } as u16 & 0x8000 != 0;
    let alt = unsafe { GetKeyState(0x12) } as u16 & 0x8000 != 0;
    let page = page_rows(app);
    // Keybindings (slice R4c, item 7): the merged chord map gets
    // first claim on every key in its vocabulary. Unbound chords
    // and keys outside the vocabulary fall through to the
    // hard-wired navigation handling below, unchanged.
    let chord = chord_from_vk(vk, ctrl, shift, alt);
    let action = chord.and_then(|c| app.keybindings.action_for(&c));
    // Jump history (roadmap item 11 wiring) works in every mode,
    // including the filter view — as Alt+Left / Alt+Right always
    // have. The second check preserves the pre-keybindings catch
    // for Alt+Arrow with extra modifiers held.
    if let Some(action) = action {
        if action == "jump_back" || action == "jump_forward" {
            dispatch_action(app, action);
            return true;
        }
    }
    if action.is_none() && alt && (vk == 0x25 || vk == 0x27) {
        jump_history_move(app, vk == 0x27);
        return true;
    }
    // Esc closes an open compare (slice R5c), whatever Esc is
    // bound to — before the filter model, though a compare and
    // a filter never coexist.
    if vk == 0x1B && app.compare.is_some() {
        close_compare(app);
        return true;
    }
    // Esc stops a running external tool (item 49): its worker
    // reports the run as cancelled on the next tick.
    if vk == 0x1B && app.tool_run.is_some() {
        cancel_tool_run(app);
        return true;
    }
    // The filter view has its own key model: Up/Down walk the row
    // cursor, Esc leaves without moving, and editing keys are
    // swallowed (the view is read-only). A few commands fall
    // through to the keybinding dispatch — Open and Save, the
    // Guide, and the find commands, which close the filter first
    // (see the guard at the top of on_command).
    if app.filter.is_some() {
        // Esc always leaves the filter, whatever it is bound to.
        if vk == 0x1B {
            exit_filter(app);
            return true;
        }
        if let Some(action) = action {
            // Tab commands fall through too (roadmap item 39):
            // switching away from a filtered tab is ordinary, and
            // the filter stays with its own document.
            if matches!(
                action,
                "open"
                    | "save"
                    | "find"
                    | "replace"
                    | "find_next"
                    | "find_prev"
                    | "guide"
                    | "new_tab"
                    | "close_tab"
                    | "next_tab"
                    | "prev_tab"
                    | "tab_last"
            ) || action.starts_with("tab_")
            {
                dispatch_action(app, action);
                return true;
            }
        }
        match vk {
            0x26 => {
                filter_move_cursor(app, -1);
                return true;
            }
            0x28 => {
                filter_move_cursor(app, 1);
                return true;
            }
            0x21 => {
                filter_move_cursor(app, -(page as i64));
                return true;
            }
            0x22 => {
                filter_move_cursor(app, page as i64);
                return true;
            }
            0x24 => {
                if let Some(filter) = &mut app.filter {
                    filter.cursor_row = 0;
                }
                filter_ensure_cursor_visible(app);
                refresh_status(app);
                invalidate(app);
                return true;
            }
            0x23 => {
                if let Some(filter) = &mut app.filter {
                    filter.cursor_row = filter.view.len().saturating_sub(1);
                }
                filter_ensure_cursor_visible(app);
                refresh_status(app);
                invalidate(app);
                return true;
            }
            0x70 => {
                // F1 shows the Guide even when the guide action
                // is unbound — the filter's own key model.
                show_help(app);
                return true;
            }
            _ => return true,
        }
    }
    if let Some(action) = action {
        dispatch_action(app, action);
        return true;
    }
    // Multi-caret (slice R5d): any navigation key collapses the
    // extra carets and the rectangle back to the single caret.
    // (The caret_up/caret_down chords were claimed by the dispatch
    // above and never reach here.)
    if matches!(vk, 0x21..=0x28) && (!app.extra_carets.is_empty() || app.rect_sel.is_some()) {
        clear_multi(app);
        refresh_status(app);
    }
    match vk {
        0x25 => app.editor.move_left(shift),  // VK_LEFT
        0x27 => app.editor.move_right(shift), // VK_RIGHT
        0x26 => {
            // VK_UP (wrap moves by display row; the freeze clamp
            // keeps scrolling up out of the pinned block)
            let before = app.editor.line_col_of(app.editor.caret()).0;
            if !wrap_move(app, true, shift) {
                app.editor.move_up(shift);
                if !shift {
                    clamp_freeze_up(app, before);
                }
            }
        }
        0x28 => {
            // VK_DOWN (wrap moves by display row)
            if !wrap_move(app, false, shift) {
                app.editor.move_down(shift);
            }
        }
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
        0x21 => {
            // VK_PRIOR
            let before = app.editor.line_col_of(app.editor.caret()).0;
            app.editor.move_page_up(page, shift);
            if !shift {
                clamp_freeze_up(app, before);
            }
        }
        0x22 => app.editor.move_page_down(page, shift), // VK_NEXT
        0x08 => {
            // VK_BACK: at multiple carets, one delete at each
            // (slice R5d, one undo entry).
            if multi_delete(app, true) {
                return true;
            }
            if edit_blocked_by_freeze(app, true, false) {
                frozen_note(app);
                return true;
            }
            if app.editor.backspace().unwrap_or(false) {
                after_edit(app);
            }
            return true;
        }
        0x2E => {
            // VK_DELETE: an unbound Delete deletes forward, as
            // always. (A Delete chord bound to an action was
            // claimed by the dispatch above.) At multiple carets
            // it deletes at each (slice R5d).
            if multi_delete(app, false) {
                return true;
            }
            if edit_blocked_by_freeze(app, false, true) {
                frozen_note(app);
                return true;
            }
            if app.editor.delete_forward().unwrap_or(false) {
                after_edit(app);
            }
            return true;
        }
        0x1B => {
            // VK_ESCAPE: extra carets and the rectangle collapse
            // first (slice R5d); then the find bar / selection as
            // before.
            if !app.extra_carets.is_empty() || app.rect_sel.is_some() {
                clear_multi(app);
                refresh_status(app);
                invalidate(app);
                return true;
            }
            if app.find_visible {
                hide_find_bar(app);
            } else {
                let caret = app.editor.caret();
                app.editor.move_caret_to(caret, false);
                invalidate(app);
            }
            return true;
        }
        _ => return false,
    }
    after_caret_move(app);
    true
}
// ---------------------------------------------------------------------------
// Multi-caret and rectangular editing (slice R5d, wiring the
// R5a undo core): extra carets are plain byte offsets beside the
// editor's primary caret; every edit at every caret goes through
// Editor::caret_edit_plan + apply_multi_replace as ONE undo
// entry. Text mode only — in hex mode the gestures decline with
// a status note. Plain click, Esc, and any navigation collapse
// back to the single caret.
// ---------------------------------------------------------------------------

/// Whether multi-caret or rectangle state is live (text mode,
/// no filter — the filter view is read-only and has its own keys).
fn multi_active(app: &App) -> bool {
    app.editor.mode() == EditorMode::Text
        && app.filter.is_none()
        && (!app.extra_carets.is_empty() || app.rect_sel.is_some())
}

/// Collapse all multi state back to the single primary caret.
fn clear_multi(app: &mut App) {
    app.extra_carets.clear();
    app.rect_sel = None;
    app.rect_drag = None;
    app.rect_press = None;
}

/// Whether a multi edit would touch the frozen block: the
/// primary caret is checked by the usual helper's line maths;
/// extras and the rectangle are checked by their lines.
fn multi_blocked_by_freeze(app: &mut App) -> bool {
    let freeze = app.settings.freeze_lines as u64;
    if freeze == 0 {
        return false;
    }
    if let Some(rect) = &app.rect_sel {
        if rect.first_line < freeze {
            return true;
        }
    }
    app.extra_carets
        .iter()
        .any(|&off| app.editor.line_col_of(off).0 < freeze)
}

/// Alt+click: add an extra caret at `offset`, or remove the one
/// already there. Clicking the primary caret does nothing.
fn toggle_extra_caret(app: &mut App, offset: u64) {
    if offset == app.editor.caret() {
        return;
    }
    if let Some(pos) = app.extra_carets.iter().position(|&o| o == offset) {
        app.extra_carets.remove(pos);
    } else {
        app.extra_carets.push(offset);
        app.extra_carets.sort_unstable();
    }
    refresh_status(app);
    invalidate(app);
}

/// Ctrl+Alt+Up/Down: add a caret on the line above or below the
/// outermost caret in that direction, at the same byte column
/// (clamped to the target line's content).
fn add_caret_at_line(app: &mut App, down: bool) {
    // Adding a caret switches the selection model from the
    // rectangle to carets.
    app.rect_sel = None;
    if app.editor.mode() == EditorMode::Hex {
        app.status_note = Some("Multiple carets are text-mode only".to_string());
        refresh_status(app);
        return;
    }
    if app.filter.is_some() {
        return;
    }
    let ed = &mut app.editor;
    let mut all: Vec<u64> = app.extra_carets.clone();
    all.push(ed.caret());
    let pick = if down {
        all.iter().max_by_key(|&&off| ed.line_col_of(off).0)
    } else {
        all.iter().min_by_key(|&&off| ed.line_col_of(off).0)
    };
    let Some(&base) = pick else { return };
    let (line, _) = ed.line_col_of(base);
    let byte_col = base - ed.line_start_byte(line).unwrap_or(0);
    let target_line = if down {
        if line + 1 >= ed.line_count() {
            return;
        }
        line + 1
    } else {
        if line == 0 {
            return;
        }
        line - 1
    };
    let Some(start) = ed.line_start_byte(target_line) else {
        return;
    };
    let end = ed
        .line_content_range(target_line)
        .map(|(_, e)| e)
        .unwrap_or(start);
    let offset = (start + byte_col).min(end);
    if offset == ed.caret() || app.extra_carets.contains(&offset) {
        return;
    }
    app.extra_carets.push(offset);
    app.extra_carets.sort_unstable();
    refresh_status(app);
    invalidate(app);
}

/// One character step from `off`, through the editor's own
/// line/column maths (so every encoding steps whole characters).
fn char_step(app: &mut App, off: u64, dir: i32) -> u64 {
    let ed = &mut app.editor;
    let (line, col) = ed.line_col_of(off);
    if dir < 0 {
        if col > 0 {
            ed.offset_of_line_col(line, col - 1).unwrap_or(off)
        } else if line > 0 {
            ed.line_content_range(line - 1)
                .map(|(_, e)| e)
                .unwrap_or(off)
        } else {
            off
        }
    } else {
        let end = ed.line_content_range(line).map(|(_, e)| e).unwrap_or(off);
        if off < end {
            ed.offset_of_line_col(line, col + 1).unwrap_or(end)
        } else if line + 1 < ed.line_count() {
            ed.line_start_byte(line + 1).unwrap_or(off)
        } else {
            off
        }
    }
}

/// The byte range a rectangle covers on one line (clamped).
fn rect_line_range(ed: &mut Editor, rect: &RectSel, line: u64) -> Option<(u64, u64)> {
    let (start, end) = ed.line_content_range(line)?;
    let len = end - start;
    Some((
        start + rect.col_start.min(len),
        start + rect.col_end.min(len),
    ))
}

/// Settle the carets after a multi edit: the primary takes its
/// planned offset; the extras take theirs, deduplicated and
/// never on top of the primary.
fn settle_carets(app: &mut App, news: &[u64]) {
    let primary_new = news[0];
    let mut extras: Vec<u64> = news[1..].to_vec();
    extras.retain(|&o| o != primary_new);
    extras.sort_unstable();
    extras.dedup();
    app.editor.move_caret_to(primary_new, false);
    app.extra_carets = extras;
    app.rect_sel = None;
}

/// Type `text` at every caret (or over the rectangle) as one
/// undo entry. Returns false when multi state is not active, so
/// the caller falls back to the ordinary single-caret insert.
fn multi_type(app: &mut App, text: &str) -> bool {
    if !multi_active(app) {
        return false;
    }
    if multi_blocked_by_freeze(app) {
        frozen_note(app);
        return true;
    }
    let bytes = match app.editor.encode_text(text) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    if let Some(rect) = app.rect_sel {
        // Typing over the rectangle replaces each line's segment
        // with the typed text — one undo entry through the same
        // multi path.
        let mut edits: Vec<crate::undo::RangeEdit> = Vec::new();
        for line in rect.first_line..=rect.last_line {
            if let Some((start, end)) = rect_line_range(&mut app.editor, &rect, line) {
                edits.push(crate::undo::RangeEdit {
                    offset: start,
                    delete_len: end - start,
                    replacement: &bytes,
                });
            }
        }
        let first_start = edits.first().map(|e| e.offset);
        match app.editor.apply_multi_replace(&edits) {
            Ok(true) => {
                app.rect_sel = None;
                app.extra_carets.clear();
                if let Some(start) = first_start {
                    let at = start + bytes.len() as u64;
                    app.editor.move_caret_to(at, false);
                }
                after_edit(app);
            }
            Ok(false) => {}
            Err(err) => {
                app.status_note = Some(err.to_string());
                refresh_status(app);
            }
        }
        return true;
    }
    let caret = app.editor.caret();
    let primary = app.editor.selection_range().unwrap_or((caret, caret));
    let mut carets: Vec<(u64, u64)> = vec![primary];
    for &off in &app.extra_carets {
        carets.push((off, off));
    }
    let (edits, news) = Editor::caret_edit_plan(&carets, &bytes);
    match app.editor.apply_multi_replace(&edits) {
        Ok(true) => {
            settle_carets(app, &news);
            after_edit(app);
        }
        Ok(false) => {}
        Err(err) => {
            app.status_note = Some(err.to_string());
            refresh_status(app);
        }
    }
    true
}

/// Backspace / Delete at every caret (or over the rectangle) as
/// one undo entry. Same contract as multi_type.
fn multi_delete(app: &mut App, backspace: bool) -> bool {
    if !multi_active(app) {
        return false;
    }
    if multi_blocked_by_freeze(app) {
        frozen_note(app);
        return true;
    }
    if let Some(rect) = app.rect_sel {
        match app.editor.rect_delete(
            rect.first_line,
            rect.last_line,
            rect.col_start,
            rect.col_end,
        ) {
            Ok(true) => {
                app.rect_sel = None;
                app.extra_carets.clear();
                after_edit(app);
            }
            Ok(false) => {}
            Err(err) => {
                app.status_note = Some(err.to_string());
                refresh_status(app);
            }
        }
        return true;
    }
    let caret = app.editor.caret();
    let primary = match app.editor.selection_range() {
        Some(range) => range,
        None if backspace => (char_step(app, caret, -1), caret),
        None => (caret, char_step(app, caret, 1)),
    };
    let mut carets: Vec<(u64, u64)> = vec![primary];
    let extras = app.extra_carets.clone();
    for &off in &extras {
        carets.push(if backspace {
            (char_step(app, off, -1), off)
        } else {
            (off, char_step(app, off, 1))
        });
    }
    let (edits, news) = Editor::caret_edit_plan(&carets, b"");
    match app.editor.apply_multi_replace(&edits) {
        Ok(true) => {
            settle_carets(app, &news);
            after_edit(app);
        }
        Ok(false) => {}
        Err(err) => {
            app.status_note = Some(err.to_string());
            refresh_status(app);
        }
    }
    true
}

fn insert_str(app: &mut App, text: &str) {
    if multi_type(app, text) {
        return;
    }
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
    // The filter view is read-only: Enter opens the cursor row's
    // line, and every other character is swallowed.
    if app.filter.is_some() {
        if ch == 0x0D {
            filter_jump_to_cursor(app);
        }
        return true;
    }
    // Freeze pane: typed input at a frozen caret is refused
    // (Enter and Tab edit too; the other control characters are
    // swallowed by the text branch anyway).
    if app.editor.mode() == EditorMode::Text
        && (matches!(ch, 0x09 | 0x0D) || ch >= 0x20)
        && edit_blocked_by_freeze(app, false, false)
    {
        frozen_note(app);
        return true;
    }
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
                    // Enter: at multiple carets, the document's
                    // line ending goes in at every caret (one
                    // undo entry, slice R5d).
                    if multi_type(app, app.editor.line_ending()) {
                        return true;
                    }
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

/// Flip one bit of the byte at the caret from the Data
/// Inspector's bit row (R4b): a normal undoable edit through the
/// editor's byte-replace path; the caret does not move.
fn flip_inspector_bit(app: &mut App, bit: u32) {
    let Some((offset, byte)) = app.bit_mirror else {
        return;
    };
    if app.editor.caret() != offset {
        return;
    }
    let new_byte = byte ^ (1u8 << bit);
    if app.editor.replace_byte_at(offset, new_byte).is_ok() {
        after_edit(app);
    }
}

/// A click in the strings pane (R4b): a min-length chip rescan, or
/// a result row (select; double-click jumps to the string).
fn strings_click(app: &mut App, x: f32, y: f32, double: bool, layout: &Layout) {
    for i in 0..app.strings_chips.len() {
        let (min_len, r) = app.strings_chips[i];
        if x >= r.0 && x < r.2 && y >= r.1 && y < r.3 {
            if app.settings.strings_min_len != min_len {
                app.settings.strings_min_len = min_len;
                save_settings(app);
                service_strings(app);
                invalidate(app);
            }
            return;
        }
    }
    let row =
        ((y - layout.rows_bottom - layout.tpl_h - STRINGS_HEADER_H) / app.line_h.max(1.0)).floor();
    if row < 0.0 {
        return;
    }
    let idx = app.strings.first + row as usize;
    if idx < app.strings.results.len() {
        app.strings.selected = Some(idx);
        if double {
            strings_jump_to(app, idx);
        } else {
            invalidate(app);
        }
    }
}

/// A click in the template results pane (slice R5d): the header's
/// close box closes the pane; a row selects, and a double-click
/// moves the caret to the field and selects its bytes.
fn tpl_click(app: &mut App, x: f32, y: f32, double: bool, layout: &Layout) {
    let close = app.tpl_close_rect;
    if x >= close.0 && x < close.2 && y >= close.1 && y < close.3 {
        app.tpl_open = false;
        invalidate(app);
        return;
    }
    let row = ((y - layout.rows_bottom - TPL_HEADER_H) / app.line_h.max(1.0)).floor();
    if row < 0.0 {
        return;
    }
    let idx = app.tpl_first + row as usize;
    let Some(result) = &app.tpl_result else {
        return;
    };
    if idx >= result.fields.len() {
        return;
    }
    app.tpl_selected = Some(idx);
    if double {
        let field = &result.fields[idx];
        let (offset, size) = (field.offset, field.size);
        app.editor.move_caret_to(offset, false);
        if size > 0 {
            app.editor.move_caret_to(offset + size, true);
        }
        app.caret_on = true;
        refresh_status(app);
    }
    invalidate(app);
}

/// Wheel over the template results pane scrolls its rows.
fn tpl_wheel(app: &mut App, wparam: WPARAM) {
    let delta = ((wparam >> 16) as u16) as i16 as i32;
    app.wheel_rem += delta;
    let total = app.tpl_result.as_ref().map(|r| r.fields.len()).unwrap_or(0);
    let max_first = total.saturating_sub(TPL_ROWS as usize);
    let mut changed = false;
    while app.wheel_rem >= 120 {
        app.tpl_first = app.tpl_first.saturating_sub(3);
        app.wheel_rem -= 120;
        changed = true;
    }
    while app.wheel_rem <= -120 {
        app.tpl_first = (app.tpl_first + 3).min(max_first);
        app.wheel_rem += 120;
        changed = true;
    }
    if changed {
        invalidate(app);
    }
}

/// Wheel over the strings pane scrolls the pane's rows (R4b),
/// sharing the window's wheel remainder like all its scrolling.
fn strings_wheel(app: &mut App, wparam: WPARAM) {
    let delta = ((wparam >> 16) as u16) as i16 as i32;
    app.wheel_rem += delta;
    let max_first = app
        .strings
        .results
        .len()
        .saturating_sub(STRINGS_ROWS as usize);
    let mut changed = false;
    while app.wheel_rem >= 120 {
        app.strings.first = app.strings.first.saturating_sub(3);
        app.wheel_rem -= 120;
        changed = true;
    }
    while app.wheel_rem <= -120 {
        app.strings.first = (app.strings.first + 3).min(max_first);
        app.wheel_rem += 120;
        changed = true;
    }
    if changed {
        invalidate(app);
    }
}

fn on_mouse_down(app: &mut App, lparam: LPARAM, double: bool) {
    // SAFETY: clicking the editor surface focuses the main window.
    unsafe { SetFocus(app.hwnd) };
    let (x, y) = mouse_xy(lparam);
    let layout = compute_layout(app);
    let xd = app.dip(x);
    let yd = app.dip(y);
    // Any press dismisses the tick hover preview.
    app.hover_cand = None;
    hide_hover(app);
    // Template results pane (slice R5d): directly below the rows
    // region, above the strings pane.
    if layout.tpl_h > 0.0 && yd >= layout.rows_bottom && yd < layout.rows_bottom + layout.tpl_h {
        tpl_click(app, xd, yd, double, &layout);
        return;
    }
    // Disassembly panel (slice R5d): a double-click on a row moves
    // the caret to the instruction; other presses there are inert.
    if disasm_showing(app)
        && xd >= layout.editor_right
        && yd >= layout.rows_top
        && yd < layout.rows_bottom
    {
        disasm_click(app, xd, yd, double, &layout);
        return;
    }
    // Strings pane (R4b): chips and rows live below the rows region
    // (below the template pane too, slice R5d).
    if layout.strings_h > 0.0 && yd >= layout.rows_bottom + layout.tpl_h {
        strings_click(app, xd, yd, double, &layout);
        return;
    }
    // Entropy strip (R4b): a click seeks proportionally.
    if app.entropy_on && yd >= layout.editor_top && yd < layout.editor_top + ENTROPY_H {
        entropy_seek(app, xd);
        return;
    }
    // Split/compare divider and panes (slice R5c): a press on
    // the divider starts its drag; a press in the compare pane
    // is swallowed whole (the pane takes no input at all); a
    // press in the split's unfocused pane focuses that pane
    // first, then lands as an ordinary press.
    if yd >= layout.editor_top && yd <= layout.rows_bottom {
        let active_div = layout.compare_div.or(layout.split_div);
        if let Some(div) = active_div {
            if (xd - div - DIVIDER_W / 2.0).abs() <= 4.0 {
                app.divider_drag = Some(xd - div);
                // SAFETY: capture the mouse for the drag; released on button up.
                unsafe { SetCapture(app.hwnd) };
                return;
            }
        }
        if let Some(div) = layout.compare_div {
            if xd >= div {
                return;
            }
        } else if let Some(div) = layout.split_div {
            let hex_side = xd >= div + DIVIDER_W;
            let focused_hex = app.editor.mode() == EditorMode::Hex;
            if hex_side != focused_hex {
                split_set_focus(app, hex_side);
            }
        }
    }
    // The filter banner and other chrome above the rows are not
    // editor clicks.
    if yd < layout.rows_top {
        return;
    }
    // Data Inspector bit boxes (R4b): a click flips the bit.
    if app.settings.inspector_open {
        let hit = app
            .bit_boxes
            .iter()
            .position(|r| xd >= r.0 && xd < r.2 && yd >= r.1 && yd < r.3);
        if let Some(i) = hit {
            flip_inspector_bit(app, 7 - i as u32);
            return;
        }
    }
    // The filter view (R4b): clicks place the row cursor; a double
    // click opens the line.
    if app.filter.is_some() {
        let row_in_view = ((yd - layout.rows_top) / app.line_h.max(1.0)).floor();
        if row_in_view >= 0.0 {
            let (first, len) = {
                let filter = app.filter.as_ref().expect("checked above");
                (filter.first_row, filter.view.len())
            };
            if len > 0 {
                let idx = (first + row_in_view as usize).min(len - 1);
                if let Some(filter) = &mut app.filter {
                    filter.cursor_row = idx;
                }
                if double {
                    filter_jump_to_cursor(app);
                    return;
                }
                refresh_status(app);
                invalidate(app);
            }
        }
        return;
    }
    let offset = pos_to_offset(app, x, y);
    // Alt gestures (slice R5d): Alt+click toggles an extra caret;
    // Alt+drag selects a byte-column rectangle. Text mode only —
    // in hex mode they decline with a note. (The split view keeps
    // its own click model; multi state just collapses there.)
    // SAFETY: plain key-state read (VK_MENU = 0x12).
    let alt = unsafe { GetKeyState(0x12) } as u16 & 0x8000 != 0;
    if alt && app.compare.is_none() && !split_active(app) {
        if app.editor.mode() == EditorMode::Hex {
            app.status_note = Some("Multiple carets and rectangles are text-mode only".to_string());
            refresh_status(app);
            return;
        }
        let (line, _) = app.editor.line_col_of(offset);
        let byte_col = offset - app.editor.line_start_byte(line).unwrap_or(0);
        app.rect_press = Some((line, byte_col, offset));
        // SAFETY: capture the mouse for the possible drag;
        // released on button up.
        unsafe { SetCapture(app.hwnd) };
        return;
    }
    // A plain press collapses multi state (slice R5d).
    if !app.extra_carets.is_empty() || app.rect_sel.is_some() {
        clear_multi(app);
        refresh_status(app);
    }
    // Ctrl+click in hex mode follows the pointer at the clicked
    // byte (roadmap item 11 wiring).
    // SAFETY: plain key-state read (VK_CONTROL = 0x11).
    let ctrl = unsafe { GetKeyState(0x11) } as u16 & 0x8000 != 0;
    if ctrl && app.editor.mode() == EditorMode::Hex {
        app.editor.move_caret_to(offset, false);
        pointer_jump(app);
        app.caret_on = true;
        invalidate(app);
        return;
    }
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
        || y < layout.rows_top
        || y >= layout.rows_bottom
    {
        return None;
    }
    let (total, first, visible) = scroll_metrics(app);
    let track_h = layout.rows_bottom - layout.rows_top;
    let (thumb_y, thumb_h) = scrollbar_geometry(track_h, total, visible, first)?;
    let rel = y - layout.rows_top;
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
    let track_h = layout.rows_bottom - layout.rows_top;
    let thumb_y = app.dip(y_px) - layout.rows_top - grab;
    let first = scrollbar_first_row(track_h, total, visible, thumb_y);
    // The filter view's drag sets its own first row (R4b).
    if app.filter.is_some() {
        let page = page_rows(app) as usize;
        if let Some(filter) = &mut app.filter {
            let max_first = filter.view.len().saturating_sub(page);
            filter.first_row = (first as usize).min(max_first);
        }
        invalidate(app);
        return true;
    }
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
        // The taskbar jump list (roadmap item 40) publishes at
        // startup and whenever the recent list changes.
        publish_jump_list(app);
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
                // The UIA provider (item 47) disconnects and the
                // window's reference is released before the App
                // goes away.
                let provider = (*ptr).uia_provider;
                if !provider.is_null() {
                    uia_disconnect(provider);
                    com_release(provider);
                }
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
        WM_GETOBJECT => {
            // UIA (roadmap item 47, scoped): for the root object
            // request (lparam = UiaRootObjectId = -25), hand UIA
            // our raw provider; every other object id gets the
            // default handling.
            if lparam == -25 {
                if app.uia_provider.is_null() {
                    app.uia_provider = uia_create_provider(hwnd);
                }
                if !app.uia_provider.is_null() {
                    // The provider is ours, kept alive by the
                    // window until WM_NCDESTROY.
                    if let Some(result) =
                        uia_return_provider(hwnd, wparam, lparam, app.uia_provider)
                    {
                        return result;
                    }
                }
            }
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
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
            // The wheel message carries SCREEN coordinates: over
            // the strings pane it scrolls the pane, anywhere else
            // the document (R4b).
            let (sx, sy) = mouse_xy(lparam);
            let mut pt = POINT { x: sx, y: sy };
            // SAFETY: our window; converts the point in place.
            unsafe { ScreenToClient(hwnd, &mut pt) };
            let layout = compute_layout(app);
            if app.dip(pt.y) >= STRIP_H && app.dip(pt.y) < STRIP_H + TABBAR_H {
                // Over the tab strip the wheel scrolls the strip
                // (roadmap item 39), never the document.
                let delta = ((wparam >> 16) as i16) as f32 / 120.0;
                app.tab_scroll_x = (app.tab_scroll_x - delta * 40.0).max(0.0);
                clamp_tab_scroll(app);
                invalidate(app);
            } else if layout.tpl_h > 0.0
                && wparam & 0xFFFF & 0x0008 == 0
                && app.dip(pt.y) >= layout.rows_bottom
                && app.dip(pt.y) < layout.rows_bottom + layout.tpl_h
            {
                tpl_wheel(app, wparam);
            } else if layout.strings_h > 0.0
                && wparam & 0xFFFF & 0x0008 == 0
                && app.dip(pt.y) >= layout.rows_bottom + layout.tpl_h
                && !app.strings.results.is_empty()
            {
                strings_wheel(app, wparam);
            } else {
                // The wheel scrolls the split pane under the
                // pointer (slice R5c): focus follows the pointer
                // first, as a click would focus it.
                if let Some(div) = layout.split_div {
                    let hex_side = app.dip(pt.x) >= div + DIVIDER_W;
                    if hex_side != (app.editor.mode() == EditorMode::Hex) {
                        split_set_focus(app, hex_side);
                    }
                }
                on_wheel(app, wparam);
            }
            0
        }
        WM_LBUTTONDOWN => {
            if !tab_mouse_down(app, lparam) && !on_scrollbar_down(app, lparam) {
                on_mouse_down(app, lparam, false);
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            if !tab_mouse_down(app, lparam) && !on_scrollbar_down(app, lparam) {
                on_mouse_down(app, lparam, true);
            }
            0
        }
        WM_MBUTTONDOWN => {
            // Middle-click closes the tab under the pointer
            // (roadmap item 39).
            tab_middle_down(app, lparam);
            0
        }
        WM_MOUSEMOVE => {
            let (mx, my) = mouse_xy(lparam);
            let (dx, dy) = (app.dip(mx), app.dip(my));
            if app.rect_press.is_some() || app.rect_drag.is_some() {
                // Rectangle drag (slice R5d): once the pointer
                // leaves the pressed line/column the press becomes
                // a drag; the selection is the normalised pair of
                // corners in (line, byte column) space.
                let offset = pos_to_offset(app, mx, my);
                let (line, _) = app.editor.line_col_of(offset);
                let byte_col = offset - app.editor.line_start_byte(line).unwrap_or(0);
                if app.rect_drag.is_none() {
                    let (pl, pc, _) = app.rect_press.unwrap();
                    if line != pl || byte_col != pc {
                        app.rect_drag = Some(((pl, pc), (line, byte_col)));
                    }
                }
                if let Some((_, corner)) = &mut app.rect_drag {
                    *corner = (line, byte_col);
                }
                if let Some(((l1, c1), (l2, c2))) = app.rect_drag {
                    app.rect_sel = Some(RectSel {
                        first_line: l1.min(l2),
                        last_line: l1.max(l2),
                        col_start: c1.min(c2),
                        col_end: c1.max(c2),
                    });
                }
                invalidate(app);
            } else if let Some(grab) = app.divider_drag {
                // Divider drag (slice R5c): the divider tracks
                // the pointer; the persisted ratio is clamped to
                // the usable range, and a fit-to-window hex
                // layout re-resolves against the new pane width.
                let layout = compute_layout(app);
                let right = if layout.compare_div.is_some() {
                    layout.width
                } else {
                    layout.editor_right
                };
                let ratio = crate::panes::ratio_for_x(0.0, right, dx - grab);
                let permille = (ratio * 1000.0).round().clamp(150.0, 850.0) as u16;
                if permille != app.settings.split_ratio_permille {
                    app.settings.split_ratio_permille = permille;
                    sync_hex_layout(app);
                }
                invalidate(app);
            } else if app.tab_drag.is_some() {
                // Tab drag (roadmap item 39): track the pointer;
                // past a small threshold the drag is live and the
                // insertion marker paints.
                if let Some(drag) = &mut app.tab_drag {
                    drag.x = dx;
                    if (dx - drag.start_x).abs() > 6.0 {
                        drag.moved = true;
                    }
                }
                invalidate(app);
            } else if !on_scrollbar_move(app, lparam) && app.dragging {
                let offset = pos_to_offset(app, mx, my);
                app.editor.move_caret_to(offset, true);
                refresh_status(app);
                invalidate(app);
            } else if app.scroll_drag.is_none() && !app.dragging {
                if (STRIP_H..STRIP_H + TABBAR_H).contains(&dy) {
                    // Over the tab strip: the hover state feeds
                    // the close ×, and the hover service shows
                    // the tab's tooltip (its full path).
                    let hit = tab_hit(app, dx, dy);
                    let new_hover = match hit {
                        TabHit::Tab(i) | TabHit::Close(i) => Some(i),
                        _ => None,
                    };
                    let new_plus = hit == TabHit::New;
                    if new_hover != app.tab_hover || new_plus != app.tab_plus_hover {
                        app.tab_hover = new_hover;
                        app.tab_plus_hover = new_plus;
                        invalidate(app);
                    }
                    match new_hover {
                        Some(i) => {
                            let settled = matches!(
                                app.hover_cand,
                                Some((HoverKind::Tab(o), _)) if o == i
                            );
                            if !settled {
                                app.hover_cand = Some((HoverKind::Tab(i), Instant::now()));
                            }
                        }
                        None => {
                            app.hover_cand = None;
                            hide_hover(app);
                        }
                    }
                } else {
                    if app.tab_hover.is_some() || app.tab_plus_hover {
                        app.tab_hover = None;
                        app.tab_plus_hover = false;
                        invalidate(app);
                    }
                    // Divider hover (slice R5c): the band
                    // lights up while the pointer rests on it.
                    let hover_layout = compute_layout(app);
                    let hover_div = hover_layout.compare_div.or(hover_layout.split_div);
                    let over_div = matches!(hover_div, Some(d)
                        if (dx - d - DIVIDER_W / 2.0).abs() <= 4.0
                            && dy >= hover_layout.editor_top
                            && dy <= hover_layout.rows_bottom);
                    if over_div != app.split_div_hover {
                        app.split_div_hover = over_div;
                        invalidate(app);
                    }
                    // Pane/divider explanations (slice R5c) win
                    // over the tick previews below.
                    match note_candidate_at(app, dx, dy) {
                        Some(kind) => {
                            let settled = matches!(app.hover_cand, Some((k, _)) if k == kind);
                            if !settled {
                                app.hover_cand = Some((kind, Instant::now()));
                            }
                        }
                        None => {
                            // Tick hover preview (item 24 expansion):
                            // track the candidate under the pointer; the
                            // tick service shows it once it settles.
                            match tick_candidate_at(app, dx, dy) {
                                Some((offset, is_bookmark)) => {
                                    let settled = matches!(
                                        app.hover_cand,
                                        Some((HoverKind::Tick(o, b), _)) if o == offset && b == is_bookmark
                                    );
                                    if !settled {
                                        app.hover_cand = Some((
                                            HoverKind::Tick(offset, is_bookmark),
                                            Instant::now(),
                                        ));
                                    }
                                }
                                None => {
                                    app.hover_cand = None;
                                    hide_hover(app);
                                }
                            }
                        }
                    }
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if app.rect_press.is_some() || app.rect_drag.is_some() {
                // Rectangle gesture ends (slice R5d): a press that
                // never became a drag toggles an extra caret at
                // the pressed offset; a drag leaves the rectangle.
                if app.rect_drag.take().is_none() {
                    if let Some((_, _, offset)) = app.rect_press.take() {
                        app.rect_sel = None;
                        toggle_extra_caret(app, offset);
                    }
                } else {
                    app.rect_press = None;
                    // The rectangle replaces the extra carets as
                    // the live selection model.
                    app.extra_carets.clear();
                    refresh_status(app);
                    invalidate(app);
                }
                // SAFETY: pairs with the SetCapture at mouse-down.
                unsafe { ReleaseCapture() };
            } else if app.divider_drag.take().is_some() {
                // The divider's resting place persists (slice
                // R5c), like the window's own geometry.
                save_settings(app);
                // SAFETY: pairs with the SetCapture at divider grab.
                unsafe { ReleaseCapture() };
            } else if let Some(drag) = app.tab_drag.take() {
                if drag.moved {
                    let (to, _) = tab_drop_at(app, drag.index, drag.x);
                    if to != drag.index {
                        move_tab(app, drag.index, to);
                    }
                }
                // SAFETY: pairs with the SetCapture at tab grab.
                unsafe { ReleaseCapture() };
            } else if app.scroll_drag.is_some() {
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
        WM_RBUTTONUP => {
            // Right-click on a tab opens its context menu
            // (roadmap item 39); right-click on a gutter bookmark
            // marker opens its annotation editor (slice R4c,
            // item 5). Anywhere else the right button keeps its
            // default behaviour.
            if tab_context_menu(app, lparam) || gutter_bookmark_click(app, lparam) {
                0
            } else {
                // SAFETY: unhandled clicks get default processing.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
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
                    // Export Delta Patch needs unsaved edits over
                    // a file-backed document (slice R4c, item 3);
                    // Open as Administrator needs a file at all
                    // (item 33).
                    set(
                        file_menu,
                        CMD_EXPORT_PATCH,
                        app.editor.is_dirty() && app.editor.path().is_some(),
                    );
                    set(file_menu, CMD_OPEN_ADMIN, app.editor.path().is_some());
                    // Open in New Window likewise needs a file
                    // (roadmap item 39).
                    set(
                        file_menu,
                        CMD_OPEN_IN_NEW_WINDOW,
                        app.editor.path().is_some(),
                    );
                    let search_menu = GetSubMenu(bar, 2);
                    // Clear Filter exists only while filtered
                    // (item 12); Edit Bookmark needs a bookmark.
                    set(search_menu, CMD_CLEAR_FILTER, app.filter.is_some());
                    set(search_menu, CMD_BOOKMARK_EDIT, !app.bookmarks.is_empty());
                    // Diff commands (slice R5c): recompute needs
                    // an open compare in text mode; the jumps
                    // need computed regions.
                    let cmp_open = app.compare.is_some();
                    let text_mode = app.editor.mode() == EditorMode::Text;
                    let has_diff = app
                        .compare
                        .as_ref()
                        .map(|c| c.diff.is_some())
                        .unwrap_or(false);
                    set(search_menu, CMD_DIFF_RECOMPUTE, cmp_open && text_mode);
                    set(
                        search_menu,
                        CMD_DIFF_NEXT,
                        cmp_open && text_mode && has_diff,
                    );
                    set(
                        search_menu,
                        CMD_DIFF_PREV,
                        cmp_open && text_mode && has_diff,
                    );
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
                                if complete && app.tail_pin_view {
                                    // Ghost Tail: the view was at
                                    // the end when the growth was
                                    // adopted — pin it to the new
                                    // end now that the index can
                                    // name it.
                                    app.tail_pin_view = false;
                                    pin_view_to_end(app);
                                }
                                if complete {
                                    rebuild_filter_if_stale(app);
                                }
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
                        let from = app.editor.caret();
                        app.editor.goto_line(line);
                        record_jump(app, from, app.editor.caret());
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
                                    Some((
                                        progress.matches.clone(),
                                        progress.ends.clone(),
                                        progress.done,
                                    ))
                                } else {
                                    None
                                }
                            }),
                            Err(_) => None,
                        };
                        if let Some((matches, ends, done)) = snapshot {
                            let changed =
                                matches.len() != app.bg_matches.len() || (done && !app.bg_done);
                            if changed {
                                app.bg_matches = matches;
                                app.bg_ends = ends;
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
                    // Background filter build (slice R4c, item
                    // 13): attach the snapshots the worker has
                    // published since the last tick.
                    service_filter_build(app);
                    // Side-by-side compare (slice R5c): attach
                    // the right file's index publications and any
                    // finished diff.
                    service_compare(app);
                    // Strings pane + entropy strip: start owed
                    // scans and pick up finished ones (both workers
                    // are epoch-guarded on their own generations).
                    service_strings(app);
                    service_entropy(app);
                    // External tools (item 49): apply a finished
                    // run's output.
                    service_tool_run(app);
                    // Scrollbar tick hover preview (item 24
                    // expansion): a settled candidate shows; a moved
                    // or vanished one hides.
                    service_hover(app);
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
            // Every dropped file opens (roadmap item 39: the
            // first takes an untouched empty tab if there is one,
            // the rest open as new tabs — the open-into-tab rule).
            // SAFETY: wparam is the drop handle; it is finished here.
            let hdrop = wparam as HDROP;
            // SAFETY: index 0xFFFF_FFFF asks for the file count.
            let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0) };
            let mut paths: Vec<String> = Vec::new();
            for i in 0..count {
                let mut buf = vec![0u16; 1024];
                // SAFETY: as above; index i is the i-th dropped file.
                let n = unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32) };
                if n > 0 {
                    paths.push(string_from_wide(&buf));
                }
            }
            // SAFETY: as above.
            unsafe { DragFinish(hdrop) };
            for path in &paths {
                open_path_from_ui(app, Path::new(path));
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
            // While the split view is on, BOTH segments are
            // active (slice R5c): both representations really
            // are showing, and the pair now switches which pane
            // has the keyboard rather than which view exists.
            let seg_active = (id == CMD_MODE_TEXT && app.editor.mode() == EditorMode::Text)
                || (id == CMD_MODE_HEX && app.editor.mode() == EditorMode::Hex)
                || (split_active(app) && (id == CMD_MODE_TEXT || id == CMD_MODE_HEX));
            let active = seg_active
                || (id == CMD_FIND_SCOPE && app.find_in_selection)
                || (id == CMD_FIND_REGEX && app.find_regex);
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
            // Tabs (roadmap item 39): every dirty document gets
            // its prompt in turn; Cancel anywhere aborts the
            // close and nothing is saved or discarded.
            if !prompt_all_dirty(app) {
                return 0;
            }
            // The session (open tabs + active tab) rides along
            // with the settings save below (roadmap item 39).
            snapshot_session(app);
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

    // Keybindings (slice R4c, item 7): data/keybindings.json
    // merged over the defaults. A problem with the file falls back
    // per action and is surfaced once in the status bar below.
    let keybindings =
        crate::keybindings::KeyBindings::load_from(&crate::keybindings::keybindings_path());
    let keybindings_problem = !keybindings.problems().is_empty();

    let theme = Theme::resolve(&settings.theme, settings.accent_override.as_deref());
    // The startup document's armed save options (roadmap item 39:
    // per-tab from here on; the settings are the default for new
    // documents).
    let startup_save_eol = settings.save_eol_choice();
    let startup_save_bom = settings.save_bom_choice();
    // SAFETY: plain GDI factory calls; the app owns the brushes.
    let panel_brush = unsafe { CreateSolidBrush(colorref(theme.panel)) };
    let edit_brush = unsafe { CreateSolidBrush(colorref(theme.bg)) };
    let line_brush = unsafe { CreateSolidBrush(colorref(theme.line)) };
    let accent_brush = unsafe { CreateSolidBrush(colorref(theme.accent)) };
    // The external tools load before the window exists so the
    // Tools menu is built from the real set (roadmap item 49).
    let (initial_tools, initial_tools_error) = load_tool_set();
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
        find_regex_check: std::ptr::null_mut(),
        find_regex: false,
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
        bg_ends: Vec::new(),
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
        status_note: None,
        tail_pin_view: false,
        filter: None,
        strings: StringsState {
            slot: Arc::new(Mutex::new(None)),
            gen: 0,
            results: Vec::new(),
            first: 0,
            selected: None,
            searching: false,
            scanned_gen: 0,
            scanned_min_len: 0,
        },
        entropy: EntropyState {
            slot: Arc::new(Mutex::new(None)),
            gen: 0,
            values: Vec::new(),
            computing: false,
            scanned_gen: 0,
        },
        entropy_on: false,
        grid_on: false,
        deltas_on: false,
        dim_on: false,
        ansi_on: false,
        ctl_on: false,
        jump_history: JumpHistory::new(),
        tooltip: std::ptr::null_mut(),
        hover_tool_ready: false,
        hover_shown: None,
        hover_cand: None,
        bit_boxes: Vec::new(),
        bit_mirror: None,
        strings_chips: Vec::new(),
        last_find_text: String::new(),
        pending_goto: if options.file.is_some() {
            options.line
        } else {
            None
        },
        filter_slot: Arc::new(Mutex::new(None)),
        filter_gen: 0,
        keybindings,
        keybindings_problem,
        doc_read_only_file: false,
        encoding_menu: std::ptr::null_mut(),
        codepage_menu: std::ptr::null_mut(),
        doc_id: 1,
        save_eol: startup_save_eol,
        save_bom: startup_save_bom,
        tabs: crate::tabs::TabOrder::new(),
        parked: Vec::new(),
        next_doc_id: 1,
        tab_scroll_x: 0.0,
        tab_hover: None,
        tab_plus_hover: false,
        tab_drag: None,
        tab_context: None,
        suppress_recovery_delete: false,
        compare: None,
        split_div_hover: false,
        divider_drag: None,
        tpl_open: false,
        tpl_result: None,
        tpl_title: String::new(),
        tpl_first: 0,
        tpl_selected: None,
        tpl_close_rect: (0.0, 0.0, 0.0, 0.0),
        tools: initial_tools,
        tools_menu: std::ptr::null_mut(),
        tools_error: initial_tools_error,
        tool_run: None,
        tool_gen: 0,
        disasm_on: false,
        disasm_base: 0,
        disasm_rows: Vec::new(),
        disasm_stale: true,
        extra_carets: Vec::new(),
        rect_sel: None,
        rect_drag: None,
        rect_press: None,
        uia_provider: std::ptr::null_mut(),
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

    // The keybindings problem note (slice R4c, item 7): shown
    // once at startup; the first caret move clears it like any
    // status note.
    // SAFETY: our window; app_ptr resolves the App it owns.
    let app = unsafe { &mut *app_ptr(hwnd) };
    if app.keybindings_problem {
        app.status_note = Some("keybindings.json had problems; defaults used".to_string());
        refresh_status(app);
    }

    // Session restore (roadmap item 39): reopen the tabs the
    // last clean exit left behind — unless this launch was given
    // a file to open (or is the read-only snapshot view), which
    // takes precedence exactly like the recovery prompts do.
    if options.file.is_none() && !options.read_only {
        restore_session(app);
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
    let prompt = wstr(PROMPT_CLASS);
    let prompt_class = WNDCLASSW {
        style: 0x2 | 0x1,
        lpfnWndProc: Some(prompt_dialog_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: icon,
        hCursor: cursor,
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: prompt.as_ptr(),
    };
    // SAFETY: as above.
    if unsafe { RegisterClassW(&prompt_class) } == 0 {
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
    if unsafe { RegisterClassW(&hash_class) } == 0 {
        return false;
    }
    // Slice R4c dialogs: Extract Ranges, Compare With, and the
    // bookmark annotation editor — same registration shape.
    let r4c_dialogs: [(&str, WNDPROC); 5] = [
        (SLICE_CLASS, Some(slice_dialog_proc)),
        (COMPARE_CLASS, Some(compare_dialog_proc)),
        (BMEDIT_CLASS, Some(bookmark_edit_proc)),
        (TPLDLG_CLASS, Some(template_dialog_proc)),
        (TOOLOUT_CLASS, Some(tool_out_proc)),
    ];
    for (name, proc_fn) in r4c_dialogs {
        let class_name = wstr(name);
        let class = WNDCLASSW {
            style: 0x2 | 0x1,
            lpfnWndProc: proc_fn,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: icon,
            hCursor: cursor,
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        // SAFETY: as above.
        if unsafe { RegisterClassW(&class) } == 0 {
            return false;
        }
    }
    true
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

/// Install the Win32 code-page converter the encoding core uses
/// for the legacy code pages (slice R4c, item 29). Called once
/// from `crate::cli::run`'s Windows prologue, before any decode
/// can happen. ISO-8859-1/-15 never reach these functions (the
/// core converts them with its own table); the double-byte and
/// Windows pages convert here, where the OS tables live.
pub fn install_page_converter() {
    crate::encoding::install_page_converter(crate::encoding::PageConverter {
        decode_units: win32_decode_units,
        encode_char: win32_encode_char,
    });
}

/// Decode bytes under a Windows code page to UTF-16 units
/// (`MultiByteToWideChar`): the page's own rules decide character
/// boundaries; invalid sequences become U+FFFD (the documented
/// lossy decode behaviour).
fn win32_decode_units(bytes: &[u8], page: u32) -> Vec<u16> {
    if bytes.is_empty() {
        return Vec::new();
    }
    // SAFETY: `bytes` is a live byte slice whose length is passed
    // exactly; the first call only measures (null output, zero
    // capacity), the second writes into a Vec sized from that
    // measurement, and its reported length bounds the result.
    unsafe {
        let needed = MultiByteToWideChar(
            page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if needed <= 0 {
            return Vec::new();
        }
        let mut units = vec![0u16; needed as usize];
        let written = MultiByteToWideChar(
            page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            units.as_mut_ptr(),
            needed,
        );
        if written < 0 {
            return Vec::new();
        }
        units.truncate(written as usize);
        units
    }
}

/// Encode one character under a Windows code page
/// (`WideCharToMultiByte`): `None` when the page has no mapping
/// for the character (the OS flags that it used a default), which
/// the core turns into its named encode error — saving never
/// silently writes a '?' the user did not choose.
fn win32_encode_char(ch: char, page: u32) -> Option<Vec<u8>> {
    let mut units = [0u16; 2];
    let n = ch.encode_utf16(&mut units).len();
    let mut used_default: i32 = 0;
    // SAFETY: `units[..n]` holds the character's UTF-16 form and
    // outlives the call; the first call only measures, the second
    // writes into a Vec sized from it; `used_default` is a
    // writable BOOL out-slot read only after success.
    unsafe {
        let needed = WideCharToMultiByte(
            page,
            0,
            units.as_ptr(),
            n as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut used_default,
        );
        if needed <= 0 {
            return None;
        }
        let mut bytes = vec![0u8; needed as usize];
        let written = WideCharToMultiByte(
            page,
            0,
            units.as_ptr(),
            n as i32,
            bytes.as_mut_ptr(),
            needed,
            std::ptr::null(),
            &mut used_default,
        );
        if written <= 0 || used_default != 0 {
            return None;
        }
        bytes.truncate(written as usize);
        Some(bytes)
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

    /// The canvas top clears the whole chrome stack — accent
    /// strip, TAB BAR, toolbar, and the optional bars. (The tab
    /// bar's arrival in R5b left this sum one bar short and the
    /// first rows painted under the toolbar until a field
    /// screenshot caught it; this pins the stack.)
    #[test]
    fn chrome_stack_top_clears_every_bar() {
        assert_eq!(
            chrome_stack_top(false, false),
            STRIP_H + TABBAR_H + TOOLBAR_H
        );
        assert_eq!(
            chrome_stack_top(true, false),
            STRIP_H + TABBAR_H + TOOLBAR_H + CHANGEBAR_H
        );
        assert_eq!(
            chrome_stack_top(false, true),
            STRIP_H + TABBAR_H + TOOLBAR_H + FINDBAR_H
        );
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
        // The R4c checksums compare too (displayed uppercase hex,
        // pasted in any case).
        assert_eq!(
            hash_verdict(&format!("{:08X}", h.crc32), Some(&h)),
            "Matches CRC32"
        );
        assert_eq!(
            hash_verdict(&format!("{:016x}", h.xxhash64), Some(&h)),
            "Matches xxHash64"
        );
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

    // ---- R4b pure helpers -----------------------------------------

    #[test]
    fn wrap_ranges_cover_the_line_exactly() {
        assert_eq!(wrap_char_ranges(0, 8), vec![(0, 0)]);
        assert_eq!(wrap_char_ranges(5, 8), vec![(0, 5)]);
        assert_eq!(wrap_char_ranges(8, 8), vec![(0, 8)]);
        assert_eq!(wrap_char_ranges(21, 8), vec![(0, 8), (8, 16), (16, 21)]);
        // A zero column count is clamped, never a hang.
        assert_eq!(wrap_char_ranges(3, 0), vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn segments_without_toggles_are_one_plain_run() {
        let segs = build_segments("plain text", 0, false, false);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "plain text");
        assert_eq!(segs[0].cells, 10);
        assert_eq!(segs[0].fg, None);
    }

    #[test]
    fn segments_apply_sgr_colours_and_consume_the_sequence() {
        let segs = build_segments("a\u{1B}[31mred\u{1B}[0m plain", 0, true, false);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "red", " plain"]);
        assert_eq!(segs[0].fg, None);
        assert_eq!(segs[1].fg, Some(1)); // red, code 31
        assert_eq!(segs[2].fg, None); // reset by code 0
                                      // The escape bytes occupy no character columns.
        assert_eq!(segs[1].char_from, 1);
        assert_eq!(segs[2].char_from, 4);
        // Bright + bold mappings.
        let segs = build_segments("\u{1B}[1;34mB\u{1B}[96mC", 0, true, false);
        assert_eq!(segs[0].fg, Some(12)); // bold blue -> bright blue
        assert_eq!(segs[1].fg, Some(14)); // bright cyan, code 96
    }

    #[test]
    fn segments_tag_control_characters_when_asked() {
        let segs = build_segments("a\u{1}b\u{7F}\t", 0, false, true);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "^A", "b", "^?", "\t"]);
        assert!(segs[1].tag && segs[3].tag);
        assert_eq!(segs[1].cells, 2); // a tag is two cells for one char
        assert_eq!(segs[1].char_len, 1);
        assert!(!segs[4].tag); // tab stays a plain character
    }

    #[test]
    fn segment_cell_and_column_maps_are_inverses() {
        let segs = build_segments("ab\u{2}cd", 0, false, true);
        // Cells: a=0, b=1, tag=2..4, c=4, d=5, end=6.
        assert_eq!(seg_cell_of(&segs, 0), 0);
        assert_eq!(seg_cell_of(&segs, 2), 2);
        assert_eq!(seg_cell_of(&segs, 3), 4);
        assert_eq!(seg_cell_of(&segs, 5), 6);
        assert_eq!(seg_col_of_cell(&segs, 0), 0);
        assert_eq!(seg_col_of_cell(&segs, 3), 2); // inside the tag
        assert_eq!(seg_col_of_cell(&segs, 4), 3);
        assert_eq!(seg_col_of_cell(&segs, 6), 5);
    }

    #[test]
    fn delta_formatting_reads_like_a_log() {
        assert_eq!(format_delta(0), "+0ms");
        assert_eq!(format_delta(45), "+45ms");
        assert_eq!(format_delta(-3), "-3ms");
        assert_eq!(format_delta(1000), "+1s");
        assert_eq!(format_delta(1250), "+1.25s");
        assert_eq!(format_delta(1500), "+1.5s");
        assert_eq!(format_delta(90_000), "+1.5m");
        assert_eq!(format_delta(-120_000), "-2m");
    }
}
