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
    GlobalFree, COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect,
    ScreenToClient, SetBkColor, SetBkMode, SetTextColor, UpdateWindow, HBRUSH, HDC, HGDIOBJ,
    OPAQUE, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetOpenFileNameW, GetSaveFileNameW, CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT,
    CHOOSECOLORW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{
    SetScrollInfo, DRAWITEMSTRUCT, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTTOOLINFOW,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::cli::{CliOptions, Mode as CliMode};
use crate::editor::{Editor, EditorMode};
use crate::encoding;
use crate::hexview;
use crate::line_index::LineIndex;
use crate::mmap_win::MmapFile;
use crate::piece_table::PieceTable;
use crate::search::SearchOptions;
use crate::settings::Settings;
use crate::theme::{Theme, ACCENT_PRESETS};

// ---------------------------------------------------------------------------
// Command ids (menus, toolbar buttons and find-bar buttons share them, so a
// menu item and its toolbar button can never drift apart).
// ---------------------------------------------------------------------------

const CMD_OPEN: usize = 1001;
const CMD_SAVE: usize = 1002;
const CMD_SAVE_AS: usize = 1003;
const CMD_EXIT: usize = 1004;
const CMD_UNDO: usize = 1011;
const CMD_REDO: usize = 1012;
const CMD_CUT: usize = 1013;
const CMD_COPY: usize = 1014;
const CMD_PASTE: usize = 1015;
const CMD_FIND: usize = 1016;
const CMD_REPLACE: usize = 1017;
const CMD_SELECT_ALL: usize = 1018;
const CMD_MODE_TEXT: usize = 1021;
const CMD_MODE_HEX: usize = 1022;
const CMD_MODE_TOGGLE: usize = 1025;
const CMD_FONT_BIGGER: usize = 1023;
const CMD_FONT_SMALLER: usize = 1024;
const CMD_GUIDE: usize = 1031;
const CMD_ABOUT: usize = 1032;
const CMD_FIND_NEXT: usize = 1041;
const CMD_FIND_PREV: usize = 1042;
const CMD_DO_REPLACE: usize = 1043;
const CMD_DO_REPLACE_ALL: usize = 1044;
const CMD_FIND_CLOSE: usize = 1045;
const CMD_THEME_BASE: usize = 3000; // + theme index (18 themes)
const CMD_ACCENT_BASE: usize = 3100; // 0 = theme default, 1..=8 presets, 9 = Custom…
const CMD_ACCENT_CUSTOM: usize = CMD_ACCENT_BASE + 9;
const CMD_RECENT_BASE: usize = 3200; // + recent index (max 8)

const ID_FIND_EDIT: usize = 2001;
const ID_REPLACE_EDIT: usize = 2002;
const ID_FIND_COUNT: usize = 2003;

const TIMER_BLINK: usize = 1;
const TIMER_TICK: usize = 2;

/// Scrollbar resolution: the window scrollbar is a 32-bit control, so a
/// 64-bit document is mapped onto this many track positions with the
/// ratio helpers in [`crate::hexview`] (SPEC.md section 3.3).
const SCROLL_TRACK: i32 = 100_000;

/// Recovery snapshots are only written for documents up to this size;
/// larger documents rely on the atomic save alone (documented in the
/// README and QUALITY.md).
const RECOVERY_MAX_BYTES: u64 = 16 * 1024 * 1024;

// Layout constants (in DIPs; scaled to physical pixels for child windows).
const TOOLBAR_H: f32 = 30.0;
const FINDBAR_H: f32 = 34.0;
const STATUS_H: f32 = 22.0;
const TEXT_PAD: f32 = 6.0;

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

/// Human-readable byte size for the status bar.
fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Theme colour as a GDI COLORREF (0x00BBGGRR) for window chrome.
fn colorref(c: crate::theme::Rgba) -> COLORREF {
    (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16)
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
shows the raw bytes: each row shows 16 bytes as numbers plus the same\r\n\
bytes as characters on the right. Press F6 (or View ▸ Hex/Text) to\r\n\
switch. Typing a hex digit (0-9, A-F) in hex mode changes the byte\r\n\
under the caret.\r\n\
\r\n\
KEYS\r\n\
Ctrl+O open · Ctrl+S save · Ctrl+Z undo · Ctrl+Y redo\r\n\
Ctrl+X / Ctrl+C / Ctrl+V cut, copy, paste · Ctrl+A select all\r\n\
Ctrl+F find · Ctrl+H find and replace · F6 switch text/hex\r\n\
F1 this guide · Esc closes the find bar\r\n\
Arrows, Home, End, Page Up and Page Down move the caret; hold Shift\r\n\
to select while you move. You can also drop a file onto the window\r\n\
to open it.\r\n\
\r\n\
SAVING IS SAFE\r\n\
Saving writes the new content to a temporary file first, checks it,\r\n\
and only then swaps it into place, so a crash or power cut cannot\r\n\
leave a half-written file. Undo history is cleared when you save.\r\n\
For files up to 16 MB, VeloEdit also keeps a recovery snapshot next\r\n\
to the program while you edit, and offers it back if the program\r\n\
closes unexpectedly. Larger files rely on the safe save alone.\r\n\
\r\n\
THEMES\r\n\
View ▸ Theme offers 18 built-in colour themes; View ▸ Accent recolours\r\n\
the highlight colour on top of any theme. Your choices are stored in\r\n\
the data folder next to the program — VeloEdit is fully portable and\r\n\
works completely offline.\r\n";

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

/// IID_IDWriteFactory from dwrite.h.
const IID_IDWRITE_FACTORY: Guid = Guid {
    data1: 0xb859_ee5a,
    data2: 0xd838,
    data3: 0x4b5b,
    data4: [0xa2, 0xee, 0x1a, 0xdc, 0x7d, 0x93, 0xdb, 0x48],
};

const D2DERR_RECREATE_TARGET: i32 = 0x8899_000Cu32 as i32;

// Vtable slots (0-based, IUnknown included), per d2d1.h / dwrite.h.
const SLOT_RELEASE: usize = 2; // IUnknown::Release — every interface
const SLOT_D2D_CREATE_HWND_RT: usize = 14; // ID2D1Factory::CreateHwndRenderTarget
const SLOT_RT_CREATE_SOLID_BRUSH: usize = 8; // ID2D1RenderTarget::CreateSolidColorBrush
const SLOT_RT_FILL_RECTANGLE: usize = 17; // ID2D1RenderTarget::FillRectangle
const SLOT_RT_DRAW_TEXT: usize = 27; // ID2D1RenderTarget::DrawText
const SLOT_RT_CLEAR: usize = 46; // ID2D1RenderTarget::Clear
const SLOT_RT_BEGIN_DRAW: usize = 47; // ID2D1RenderTarget::BeginDraw
const SLOT_RT_END_DRAW: usize = 48; // ID2D1RenderTarget::EndDraw
const SLOT_RT_SET_DPI: usize = 50; // ID2D1RenderTarget::SetDpi
const SLOT_RT_RESIZE: usize = 57; // ID2D1HwndRenderTarget::Resize
const SLOT_DW_GET_FONT_COLLECTION: usize = 3; // IDWriteFactory::GetSystemFontCollection
const SLOT_DW_CREATE_TEXT_FORMAT: usize = 15; // IDWriteFactory::CreateTextFormat
const SLOT_DW_CREATE_TEXT_LAYOUT: usize = 18; // IDWriteFactory::CreateTextLayout
const SLOT_FC_FIND_FAMILY_NAME: usize = 5; // IDWriteFontCollection::FindFamilyName
const SLOT_LAYOUT_GET_METRICS: usize = 45; // IDWriteTextLayout::GetMetrics

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

/// Create the single-threaded Direct2D factory (or null on failure).
fn d2d_create_factory() -> *mut c_void {
    let mut factory: *mut c_void = std::ptr::null_mut();
    // SAFETY: the IID is the documented ID2D1Factory GUID; the options
    // pointer may be null; `factory` is a writable out-slot. The
    // returned pointer is checked before use and owned by the caller.
    let hr = unsafe { D2D1CreateFactory(0, &IID_ID2D1_FACTORY, std::ptr::null(), &mut factory) };
    if hr < 0 {
        return std::ptr::null_mut();
    }
    factory
}

/// Create the shared DirectWrite factory (or null on failure).
fn dwrite_create_factory() -> *mut c_void {
    let mut factory: *mut c_void = std::ptr::null_mut();
    // SAFETY: the IID is the documented IDWriteFactory GUID; `factory`
    // is a writable out-slot; the result is checked and owned.
    let hr = unsafe { DWriteCreateFactory(0, &IID_IDWRITE_FACTORY, &mut factory) };
    if hr < 0 {
        return std::ptr::null_mut();
    }
    factory
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
    current: *mut c_void,
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
            &mut self.current,
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
    toolbar: Vec<HWND>,
    find_controls: Vec<HWND>,
    find_edit: HWND,
    replace_edit: HWND,
    find_count: HWND,
    status: HWND,
    help_hwnd: HWND,
    view_menu: HMENU,
    recent_menu: HMENU,
    theme_menu: HMENU,
    accent_menu: HMENU,
    find_visible: bool,
    has_focus: bool,
    caret_on: bool,
    dragging: bool,
    wheel_rem: i32,
    pending_high_surrogate: Option<u16>,
    hex_nibble: Option<u8>,
    index_slot: Arc<Mutex<Option<(u64, LineIndex)>>>,
    doc_generation: u64,
    open_generation: u64,
    last_edit: Option<Instant>,
    custom_colors: [u32; 16],
    last_find_text: String,
    /// A `--line` target waiting for the background line index.
    pending_goto: Option<u64>,
    /// Incremented on every document open; index threads tag their
    /// result with the epoch they were spawned for.
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
        current: make_brush(target, t.current_line),
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
    // SAFETY: slot 45 is IDWriteTextLayout::GetMetrics per dwrite.h;
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
}

fn compute_layout(app: &mut App) -> Layout {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: our window plus a writable RECT.
    unsafe { GetClientRect(app.hwnd, &mut client) };
    let width = app.dip(client.right - client.left);
    let height = app.dip(client.bottom - client.top);
    let top = TOOLBAR_H + if app.find_visible { FINDBAR_H } else { 0.0 };
    let gutter_w = match app.editor.mode() {
        EditorMode::Text => {
            let digits = app.editor.line_count().max(1).to_string().len() as f32;
            (digits + 2.0) * app.char_w + TEXT_PAD
        }
        EditorMode::Hex => 18.0 * app.char_w + TEXT_PAD,
    };
    Layout {
        editor_top: top,
        editor_bottom: (height - STATUS_H).max(top),
        width,
        gutter_w,
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
    // Toolbar buttons: fixed DIP widths in creation order.
    let widths: [f32; 7] = [58.0, 58.0, 58.0, 58.0, 58.0, 74.0, 92.0];
    let mut x = app.px(4.0);
    for (i, button) in app.toolbar.iter().enumerate() {
        let w = app.px(widths.get(i).copied().unwrap_or(58.0));
        // SAFETY: all handles are our child windows.
        unsafe { MoveWindow(*button, x, app.px(3.0), w, toolbar_h - app.px(6.0), 1) };
        x += w + app.px(4.0);
    }
    // Find bar (second row when visible).
    if app.find_visible {
        let y = toolbar_h + app.px(3.0);
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
        place(app, find_edit, &mut fx, 190.0);
        // Buttons follow in find_controls order after the two edits:
        // [find_edit, prev, next, count, replace_edit, replace, replace_all, close]
        let order: [(HWND, f32); 6] = [
            (app.find_controls[1], 52.0),
            (app.find_controls[2], 52.0),
            (find_count, 96.0),
            (replace_edit, 190.0),
            (app.find_controls[5], 68.0),
            (app.find_controls[6], 82.0),
        ];
        for (hwnd, w) in order {
            place(app, hwnd, &mut fx, w);
        }
        let close_btn = app.find_controls[7];
        place(app, close_btn, &mut fx, 30.0);
    }
    // Status strip along the bottom.
    let status_h = app.px(STATUS_H);
    // SAFETY: our child window.
    unsafe { MoveWindow(app.status, 0, ch - status_h, cw, status_h, 1) };
}

fn invalidate(app: &App) {
    // SAFETY: our window; a null rect invalidates the whole client area
    // and FALSE keeps the background (Direct2D repaints everything).
    unsafe { InvalidateRect(app.hwnd, std::ptr::null(), 0) };
}

// ---------------------------------------------------------------------------
// Title, status bar, scrollbar
// ---------------------------------------------------------------------------

fn refresh_title(app: &App) {
    let mut title = String::from("VeloEdit");
    if let Some(path) = app.editor.path() {
        title.push_str(" \u{2014} ");
        title.push_str(&path.display().to_string());
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
    let mut text = format!(
        "Ln {}, Col {} \u{00B7} Offset {} \u{00B7} {} \u{00B7} {} \u{00B7} {}",
        st.line,
        st.col,
        st.offset,
        st.encoding.name(),
        fmt_size(st.size),
        app.theme.name,
    );
    if !st.encoding_confident {
        text.push_str(" (detected, not certain)");
    }
    if st.selection_len > 0 {
        text.push_str(&format!(
            " \u{00B7} {} selected",
            fmt_size(st.selection_len)
        ));
    }
    if st.read_only {
        text.push_str(" \u{00B7} Read-only");
    }
    if st.dirty {
        text.push_str(" \u{00B7} Unsaved changes");
    }
    let wide = wstr(&text);
    // SAFETY: our status control plus a NUL-terminated string.
    unsafe { SetWindowTextW(app.status, wide.as_ptr()) };
}

fn scrollbar_update(app: &mut App) {
    let total = match app.editor.mode() {
        EditorMode::Text => app.editor.line_count(),
        EditorMode::Hex => hexview::row_count(app.editor.total_len()),
    };
    let first = match app.editor.mode() {
        EditorMode::Text => app.editor.first_line(),
        EditorMode::Hex => app.editor.first_row(),
    };
    let page = page_rows(app);
    let mut info = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: SCROLL_TRACK - 1,
        nPage: 0,
        nPos: 0,
        nTrackPos: 0,
    };
    if total <= page {
        info.nPage = SCROLL_TRACK as u32; // thumb fills the track: nothing to scroll
        info.nPos = 0;
    } else {
        info.nPage = ((SCROLL_TRACK as u128 * page as u128 / total as u128) as u32).max(1);
        info.nPos = hexview::thumb_position(first, total, SCROLL_TRACK as u64) as i32;
    }
    // SAFETY: our window; `info` outlives the call.
    unsafe { SetScrollInfo(app.hwnd, SB_VERT, &info, 1) };
}

// ---------------------------------------------------------------------------
// Rendering (Direct2D). All editor data is gathered first (the editor
// needs &mut), then the draw calls run off the gathered snapshot.
// ---------------------------------------------------------------------------

type RtFillRect = unsafe extern "system" fn(*mut c_void, *const D2dRectF, *mut c_void);
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
    let current_match = app.editor.current_match();
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
    // SAFETY: slots 47/48/46 are BeginDraw/EndDraw/Clear per d2d1.h,
    // resolved from the live render target.
    let begin: unsafe extern "system" fn(*mut c_void) = unsafe { vfn(target, SLOT_RT_BEGIN_DRAW) };
    let end: unsafe extern "system" fn(*mut c_void, *mut u64, *mut u64) -> i32 =
        unsafe { vfn(target, SLOT_RT_END_DRAW) };
    let clear: unsafe extern "system" fn(*mut c_void, *const D2dColorF) =
        unsafe { vfn(target, SLOT_RT_CLEAR) };
    let fill: RtFillRect = unsafe { vfn(target, SLOT_RT_FILL_RECTANGLE) };
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
                let row_rect = D2dRectF {
                    left: 0.0,
                    top: y,
                    right: width,
                    bottom: y + app.line_h,
                };
                if row.line_no == caret_line {
                    fill_rect(fill, target, row_rect, brushes.current);
                }
                // Find-match highlight for the current match.
                if let Some((ms, me)) = current_match {
                    if ms >= row.start && ms < row.end {
                        let c0 = col_of_byte(table, enc, row.start, ms);
                        let c1 = col_of_byte(table, enc, row.start, me.min(row.end));
                        fill_rect(
                            fill,
                            target,
                            D2dRectF {
                                left: layout.gutter_w + TEXT_PAD + c0 * app.char_w,
                                top: y,
                                right: layout.gutter_w + TEXT_PAD + c1 * app.char_w,
                                bottom: y + app.line_h,
                            },
                            brushes.match_bg,
                        );
                    }
                }
                // Selection highlight.
                if let Some((ss, se)) = selection {
                    if se > row.start && ss <= row.end {
                        let from = ss.max(row.start);
                        let c0 = col_of_byte(table, enc, row.start, from);
                        let x1 = if se > row.end {
                            width
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
                    brushes.muted
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
                // The line's text.
                draw_text(
                    draw,
                    target,
                    format,
                    &row.text,
                    D2dRectF {
                        left: layout.gutter_w + TEXT_PAD,
                        top: y,
                        right: width,
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
            let hex_x = layout.gutter_w + TEXT_PAD;
            let ascii_x = hex_x + 51.0 * app.char_w;
            for (i, row) in hex_rows.iter().enumerate() {
                let y = top + i as f32 * app.line_h;
                if caret >= row.address && caret < row.address + row.len {
                    fill_rect(
                        fill,
                        target,
                        D2dRectF {
                            left: 0.0,
                            top: y,
                            right: width,
                            bottom: y + app.line_h,
                        },
                        brushes.current,
                    );
                }
                // Selection: one background cell per selected byte.
                if let Some((ss, se)) = selection {
                    let from = ss.max(row.address);
                    let to = se.min(row.address + row.len);
                    let mut byte = from;
                    while byte < to {
                        let idx = (byte - row.address) as f32;
                        let cell = idx * 3.0 + if idx >= 8.0 { 1.0 } else { 0.0 };
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
                                left: ascii_x + idx * app.char_w,
                                top: y,
                                right: ascii_x + (idx + 1.0) * app.char_w,
                                bottom: y + app.line_h,
                            },
                            brushes.selection,
                        );
                        byte += 1;
                    }
                }
                let address = format!("{:016X}", row.address);
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
                    brushes.muted,
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
                // Caret: an outline box around the current byte.
                if app.has_focus
                    && app.caret_on
                    && caret >= row.address
                    && caret <= row.address + row.len
                {
                    let idx =
                        (caret.min(row.address + row.len.saturating_sub(1)) - row.address) as f32;
                    if row.len > 0 {
                        let cell = idx * 3.0 + if idx >= 8.0 { 1.0 } else { 0.0 };
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
            let total_rows = hexview::row_count(app.editor.total_len());
            if total_rows == 0 {
                return 0;
            }
            let row = (app.editor.first_row() + row_in_view as u64).min(total_rows - 1);
            let row_start = hexview::row_offset(row);
            let row_len = hexview::BYTES_PER_ROW.min(app.editor.total_len() - row_start);
            let hex_x = layout.gutter_w + TEXT_PAD;
            let ascii_x = hex_x + 51.0 * app.char_w;
            let byte = if x >= ascii_x {
                ((x - ascii_x) / app.char_w.max(1.0)).floor().max(0.0) as u64
            } else {
                let cell = ((x - hex_x) / app.char_w.max(1.0)).floor().max(0.0) as u64;
                if cell < 24 {
                    cell / 3
                } else {
                    8 + (cell.saturating_sub(25)) / 3
                }
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

/// Build the line index for a freshly opened mapping on a background
/// thread (SPEC.md section 3.1: the secondary indexing thread). The
/// result lands in the shared slot; the UI thread picks it up on its
/// tick and the editor only accepts it if the document is untouched.
fn spawn_index_thread(app: &App, mmap: Arc<MmapFile>) {
    let slot = app.index_slot.clone();
    let epoch = app.index_epoch;
    if let Ok(mut guard) = slot.lock() {
        *guard = None;
    }
    std::thread::spawn(move || {
        let index = LineIndex::build(mmap.len(), |offset, out| {
            let bytes = mmap.as_slice();
            let start = offset as usize;
            if start >= bytes.len() {
                return 0;
            }
            let n = out.len().min(bytes.len() - start);
            out[..n].copy_from_slice(&bytes[start..start + n]);
            n
        });
        if let Ok(mut guard) = slot.lock() {
            *guard = Some((epoch, index));
        }
    });
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
    app.doc_generation = 0;
    app.open_generation = 0;
    app.last_edit = None;
    app.hex_nibble = None;
    spawn_index_thread(app, mmap);
    delete_recovery_files();
    let path_string = path.to_string_lossy().into_owned();
    app.settings.add_recent(&path_string);
    save_settings(app);
    refresh_title(app);
    refresh_status(app);
    scrollbar_update(app);
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
fn do_save(app: &mut App) -> bool {
    if app.editor.path().is_none() {
        return do_save_as(app);
    }
    match app.editor.save() {
        Ok(_) => {
            delete_recovery_files();
            app.last_edit = None;
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
    match app.editor.save_as(&path) {
        Ok(_) => {
            delete_recovery_files();
            app.last_edit = None;
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

// ---------------------------------------------------------------------------
// Find bar
// ---------------------------------------------------------------------------

fn show_find_bar(app: &mut App, focus_replace: bool) {
    app.find_visible = true;
    for hwnd in &app.find_controls {
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
    for hwnd in &app.find_controls {
        // SAFETY: our child windows.
        unsafe { ShowWindow(*hwnd, SW_HIDE) };
    }
    layout_children(app);
    invalidate(app);
    // SAFETY: our window.
    unsafe { SetFocus(app.hwnd) };
}

/// Run (or re-run) the find from the find-bar text and update the count
/// label. `advance`: 1 = next match, -1 = previous, 0 = stay.
fn find_from_bar(app: &mut App, advance: i32) {
    // SAFETY: our edit controls.
    let text = unsafe { window_text(app.find_edit) };
    if text.is_empty() {
        let wide = wstr("");
        // SAFETY: our static control.
        unsafe { SetWindowTextW(app.find_count, wide.as_ptr()) };
        app.last_find_text.clear();
        return;
    }
    if text != app.last_find_text || app.editor.matches().is_empty() {
        app.last_find_text = text.clone();
        match app.editor.set_find(&text, SearchOptions::default()) {
            Ok(_) => {}
            Err(err) => {
                let wide = wstr(&format!("{err}"));
                // SAFETY: our static control.
                unsafe { SetWindowTextW(app.find_count, wide.as_ptr()) };
                return;
            }
        }
    } else if advance > 0 {
        app.editor.find_next();
    } else if advance < 0 {
        app.editor.find_prev();
    }
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
    let page = page_rows(app);
    app.editor.ensure_caret_visible(page);
    refresh_status(app);
    scrollbar_update(app);
    invalidate(app);
}

fn do_replace(app: &mut App, all: bool) {
    // SAFETY: our edit controls.
    let replacement = unsafe { window_text(app.replace_edit) };
    // Make sure the needle is current before replacing.
    find_from_bar(app, 0);
    let result = if all {
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
    scrollbar_update(app);
    invalidate(app);
}

// ---------------------------------------------------------------------------
// Settings + theme application
// ---------------------------------------------------------------------------

fn save_settings(app: &App) {
    let path = crate::settings::settings_path();
    let _ = app.settings.save_to(&path);
}

/// Apply a new resolved theme: GDI chrome brushes are recreated, the
/// Direct2D brushes are dropped (recreated against the live target on
/// the next paint), and the choice is persisted.
fn apply_theme(app: &mut App) {
    app.theme = Theme::resolve(&app.settings.theme, app.settings.accent_override.as_deref());
    // SAFETY: the GDI brushes are owned by the app; DeleteObject on a
    // brush not currently selected into any DC is safe.
    unsafe {
        if !app.panel_brush.is_null() {
            DeleteObject(app.panel_brush as HGDIOBJ);
        }
        if !app.edit_brush.is_null() {
            DeleteObject(app.edit_brush as HGDIOBJ);
        }
    }
    // SAFETY: CreateSolidBrush is a plain factory call; the brushes
    // are owned by the app and deleted in apply_theme / WM_DESTROY.
    unsafe {
        app.panel_brush = CreateSolidBrush(colorref(app.theme.panel));
        app.edit_brush = CreateSolidBrush(colorref(app.theme.bg));
    }
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
            current: make_brush(target, t.current_line),
            selection: make_brush(target, t.selection),
            match_bg: make_brush(target, t.match_bg),
            accent: make_brush(target, t.accent),
        });
    }
    save_settings(app);
    refresh_status(app);
    invalidate(app);
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
// Recovery (never lose the user's work): a debounced snapshot of the
// edited document in data/recovery.bin + data/recovery.json, written
// only for documents up to RECOVERY_MAX_BYTES.
// ---------------------------------------------------------------------------

fn recovery_paths() -> (PathBuf, PathBuf) {
    let dir = crate::settings::data_dir();
    (dir.join("recovery.bin"), dir.join("recovery.json"))
}

fn delete_recovery_files() {
    let (bin, json) = recovery_paths();
    let _ = std::fs::remove_file(bin);
    let _ = std::fs::remove_file(json);
}

fn write_recovery(app: &App) {
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

/// Startup recovery check: if a snapshot exists and the file on disk
/// does not match it, ask whether to open the recovered copy. Returns
/// the editor to start with, or `None` for the normal startup path.
fn recovery_startup_editor() -> Option<Editor> {
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
    // with the menus.
    let toolbar_defs: [(usize, &str, &str); 7] = [
        (CMD_OPEN, "Open", "Open a file (Ctrl+O)"),
        (CMD_SAVE, "Save", "Save the file (Ctrl+S)"),
        (CMD_UNDO, "Undo", "Undo the last change (Ctrl+Z)"),
        (CMD_REDO, "Redo", "Redo the last undone change (Ctrl+Y)"),
        (CMD_FIND, "Find", "Find text (Ctrl+F)"),
        (CMD_REPLACE, "Replace", "Find and replace (Ctrl+H)"),
        (
            CMD_MODE_TOGGLE,
            "Hex",
            "Switch between text and hex mode (F6)",
        ),
    ];
    for (id, label, tip) in toolbar_defs {
        let button = create_child(app, "Button", label, WS_CHILD | WS_VISIBLE | 0xB, 0, id);
        app.toolbar.push(button);
        add_tooltip(tooltip, app.hwnd, button, tip);
    }

    // Find bar (created hidden; shown by Ctrl+F / Ctrl+H). The order
    // in find_controls is fixed — layout_children indexes into it.
    let edit_style = WS_CHILD | 0x80; // ES_AUTOHSCROLL
    let button_style = WS_CHILD | 0xB; // BS_OWNERDRAW
    app.find_edit = create_child(app, "Edit", "", edit_style, WS_EX_CLIENTEDGE, ID_FIND_EDIT);
    let prev = create_child(app, "Button", "Prev", button_style, 0, CMD_FIND_PREV);
    let next = create_child(app, "Button", "Next", button_style, 0, CMD_FIND_NEXT);
    app.find_count = create_child(app, "Static", "", WS_CHILD, 0, ID_FIND_COUNT);
    app.replace_edit = create_child(
        app,
        "Edit",
        "",
        edit_style,
        WS_EX_CLIENTEDGE,
        ID_REPLACE_EDIT,
    );
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
    subclass_edit(app.find_edit, app.hwnd);
    subclass_edit(app.replace_edit, app.hwnd);

    // Status strip: a static control, themed via WM_CTLCOLORSTATIC.
    app.status = create_child(app, "Static", "", WS_CHILD | WS_VISIBLE, 0, 0);
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
        menu_item(file, CMD_OPEN, "Open\tCtrl+O");
        menu_item(file, CMD_SAVE, "Save\tCtrl+S");
        menu_item(file, CMD_SAVE_AS, "Save As\u{2026}");
        app.recent_menu = CreatePopupMenu();
        menu_popup(file, app.recent_menu, "Recent");
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

        let view = CreatePopupMenu();
        app.view_menu = view;
        menu_item(view, CMD_MODE_TEXT, "Text Mode");
        menu_item(view, CMD_MODE_HEX, "Hex Mode\tF6");
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

        let help = CreatePopupMenu();
        menu_item(help, CMD_GUIDE, "Guide\tF1");
        menu_item(help, CMD_ABOUT, "About VeloEdit");

        menu_popup(bar, file, "File");
        menu_popup(bar, edit, "Edit");
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
    }
    // The toolbar mode button shows the *other* mode's name as the
    // action it performs.
    if let Some(button) = app.toolbar.get(6) {
        let label = match app.editor.mode() {
            EditorMode::Text => "Hex",
            EditorMode::Hex => "Text",
        };
        let wide = wstr(label);
        // SAFETY: our toolbar button.
        unsafe { SetWindowTextW(*button, wide.as_ptr()) };
    }
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
    scrollbar_update(app);
    invalidate(app);
}

fn set_mode(app: &mut App, mode: EditorMode) {
    if app.editor.mode() == mode {
        return;
    }
    app.editor.set_mode(mode);
    refresh_menu_checks(app);
    refresh_status(app);
    scrollbar_update(app);
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

fn on_vscroll(app: &mut App, wparam: WPARAM) {
    let code = (wparam & 0xFFFF) as i32;
    let page = page_rows(app) as i64;
    // SB_LINEUP=0, SB_LINEDOWN=1, SB_PAGEUP=2, SB_PAGEDOWN=3,
    // SB_THUMBPOSITION=4, SB_THUMBTRACK=5, SB_TOP=6, SB_BOTTOM=7.
    match code {
        0 => scroll_by(app, -1),
        1 => scroll_by(app, 1),
        2 => scroll_by(app, -page),
        3 => scroll_by(app, page),
        6 => scroll_by(app, i64::MIN / 2),
        7 => scroll_by(app, i64::MAX / 2),
        4 | 5 => {
            // SAFETY: SCROLLINFO is a plain C struct; SIF_TRACKPOS
            // (0x10) asks for the drag position.
            let mut info: SCROLLINFO = unsafe { std::mem::zeroed() };
            info.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
            info.fMask = 0x10;
            // SAFETY: our window; `info` outlives the call.
            if unsafe { GetScrollInfo(app.hwnd, SB_VERT, &mut info) } != 0 {
                let total = match app.editor.mode() {
                    EditorMode::Text => app.editor.line_count(),
                    EditorMode::Hex => hexview::row_count(app.editor.total_len()),
                };
                let first = hexview::offset_for_thumb(
                    info.nTrackPos.max(0) as u64,
                    total.max(1),
                    SCROLL_TRACK as u64,
                );
                match app.editor.mode() {
                    EditorMode::Text => app.editor.set_first_line(first),
                    EditorMode::Hex => app.editor.set_first_row(first),
                }
                invalidate(app);
            }
        }
        _ => {}
    }
}

fn on_wheel(app: &mut App, wparam: WPARAM) {
    let delta = ((wparam >> 16) as u16) as i16 as i32;
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
        CMD_SELECT_ALL => {
            app.editor.select_all();
            after_caret_move(app);
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
                scrollbar_update(app);
                refresh_status(app);
                invalidate(app);
            }
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
            let name = crate::theme::names()[id - CMD_THEME_BASE];
            app.settings.theme = name.to_string();
            apply_theme(app);
            refresh_menu_checks(app);
        }
        _ if id == CMD_ACCENT_BASE => {
            app.settings.accent_override = None;
            apply_theme(app);
            refresh_menu_checks(app);
        }
        _ if (CMD_ACCENT_BASE + 1..CMD_ACCENT_BASE + 1 + ACCENT_PRESETS.len()).contains(&id) => {
            let (_, hex) = ACCENT_PRESETS[id - CMD_ACCENT_BASE - 1];
            app.settings.accent_override = Some(hex.to_string());
            apply_theme(app);
            refresh_menu_checks(app);
        }
        _ if id == CMD_ACCENT_CUSTOM => {
            if let Some(color) = choose_accent_color(app, app.theme.accent) {
                app.settings.accent_override = Some(color.to_hex());
                apply_theme(app);
                refresh_menu_checks(app);
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
        scrollbar_update(app);
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
                // SAFETY: slot 57 is ID2D1HwndRenderTarget::Resize per
                // d2d1.h; on failure the target is dropped and the next
                // paint recreates it.
                let resize: unsafe extern "system" fn(*mut c_void, *const D2dSizeU) -> i32 =
                    unsafe { vfn(app.target, SLOT_RT_RESIZE) };
                // SAFETY: as above.
                if unsafe { resize(app.target, &size) } < 0 {
                    release_target(app);
                }
            }
            layout_children(app);
            scrollbar_update(app);
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
        WM_VSCROLL => {
            on_vscroll(app, wparam);
            0
        }
        WM_MOUSEWHEEL => {
            on_wheel(app, wparam);
            0
        }
        WM_LBUTTONDOWN => {
            on_mouse_down(app, lparam, false);
            0
        }
        WM_LBUTTONDBLCLK => {
            on_mouse_down(app, lparam, true);
            0
        }
        WM_MOUSEMOVE => {
            if app.dragging {
                let (x, y) = mouse_xy(lparam);
                let offset = pos_to_offset(app, x, y);
                app.editor.move_caret_to(offset, true);
                refresh_status(app);
                invalidate(app);
            }
            0
        }
        WM_LBUTTONUP => {
            if app.dragging {
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
                    set(edit_menu, CMD_UNDO, app.editor.can_undo());
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
        WM_TIMER => {
            match wparam {
                TIMER_BLINK => {
                    if app.has_focus {
                        app.caret_on = !app.caret_on;
                        invalidate(app);
                    }
                }
                TIMER_TICK => {
                    // Pick up the background line index (only when the
                    // document is untouched since it was opened).
                    if app.doc_generation == app.open_generation {
                        let ready = match app.index_slot.try_lock() {
                            Ok(mut guard) => guard.take(),
                            Err(_) => None,
                        };
                        if let Some((epoch, index)) = ready {
                            if epoch == app.index_epoch {
                                app.editor.attach_line_index(index);
                                scrollbar_update(app);
                                invalidate(app);
                            }
                        }
                    }
                    if app.pending_goto.is_some() && app.editor.has_current_index() {
                        let line = app.pending_goto.take().expect("checked above");
                        app.editor.goto_line(line);
                        after_caret_move(app);
                    }
                    // Recovery: one second after the last edit, snapshot.
                    if let Some(when) = app.last_edit {
                        if when.elapsed() >= std::time::Duration::from_secs(1) {
                            if app.editor.is_dirty() {
                                write_recovery(app);
                            }
                            app.last_edit = None;
                        }
                    }
                }
                _ => {}
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
                // SAFETY: slot 50 is ID2D1RenderTarget::SetDpi per
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
            scrollbar_update(app);
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
            // SAFETY: colouring our status / find-bar labels from the
            // active theme; returns the panel brush.
            unsafe {
                SetTextColor(hdc, colorref(app.theme.text));
                SetBkColor(hdc, colorref(app.theme.panel));
                SetBkMode(hdc, OPAQUE as i32);
            }
            app.panel_brush as LRESULT
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
            let pressed = dis.itemState & 0x1 != 0; // ODS_SELECTED
            let brush = if pressed {
                app.edit_brush
            } else {
                app.panel_brush
            };
            let mut rect = dis.rcItem;
            // SAFETY: GDI drawing on the button's own DC.
            unsafe {
                FillRect(dis.hDC, &rect, brush);
                SetBkMode(dis.hDC, TRANSPARENT as i32);
                SetTextColor(dis.hDC, colorref(app.theme.text));
                let text = window_text(dis.hwndItem);
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                if !wide.is_empty() {
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
                if y >= layout.editor_top && y < layout.editor_bottom {
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

    let d2d = d2d_create_factory();
    let dwrite = dwrite_create_factory();
    if d2d.is_null() || dwrite.is_null() {
        message_box(
            std::ptr::null_mut(),
            "VeloEdit could not start Direct2D / DirectWrite, which the editor window needs.\nThe headless tools (VeloEdit --help) still work.",
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
    // snapshot (skipped in read-only viewing); otherwise a blank page.
    let mut index_source: Option<Arc<MmapFile>> = None;
    let mut editor = if let Some(file) = &options.file {
        let path = Path::new(file);
        match MmapFile::open(path) {
            Ok(mmap) => {
                let mmap = Arc::new(mmap);
                let sample_len = (mmap.len() as usize).min(64 * 1024);
                let detection = encoding::detect(&mmap.as_slice()[..sample_len]);
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
        recovery_startup_editor().unwrap_or_else(Editor::new_empty)
    } else {
        Editor::new_empty()
    };
    if options.mode == CliMode::Hex {
        editor.set_mode(EditorMode::Hex);
    }

    let theme = Theme::resolve(&settings.theme, settings.accent_override.as_deref());
    // SAFETY: plain GDI factory calls; the app owns both brushes.
    let panel_brush = unsafe { CreateSolidBrush(colorref(theme.panel)) };
    let edit_brush = unsafe { CreateSolidBrush(colorref(theme.bg)) };
    let app = App {
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
        toolbar: Vec::new(),
        find_controls: Vec::new(),
        find_edit: std::ptr::null_mut(),
        replace_edit: std::ptr::null_mut(),
        find_count: std::ptr::null_mut(),
        status: std::ptr::null_mut(),
        help_hwnd: std::ptr::null_mut(),
        view_menu: std::ptr::null_mut(),
        recent_menu: std::ptr::null_mut(),
        theme_menu: std::ptr::null_mut(),
        accent_menu: std::ptr::null_mut(),
        find_visible: false,
        has_focus: false,
        caret_on: true,
        dragging: false,
        wheel_rem: 0,
        pending_high_surrogate: None,
        hex_nibble: None,
        index_slot: Arc::new(Mutex::new(None)),
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
    if let Some(mmap) = index_source {
        spawn_index_thread(&app, mmap);
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
            WS_OVERLAPPEDWINDOW | WS_VSCROLL | WS_CLIPCHILDREN,
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
    let main = wstr(MAIN_CLASS);
    let main_class = WNDCLASSW {
        style,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
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
        hIcon: std::ptr::null_mut(),
        hCursor: cursor,
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: help.as_ptr(),
    };
    // SAFETY: as above.
    (unsafe { RegisterClassW(&help_class) }) != 0
}
