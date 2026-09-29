#![allow(unsafe_code)]
// Win32 interop fights several pedantic lints (prelude glob imports, &ref as
// raw-pointer args, i32/u32 handle math, hex COLORREFs) — allowed module-wide.
#![allow(
    clippy::wildcard_imports,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::unreadable_literal,
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::min_ident_chars,
    clippy::many_single_char_names,
    clippy::struct_excessive_bools
)]
//! GUI installer front-end — pure Win32/GDI.
//!
//! Deliberately NOT WinUI 3: the installer's job is to run on machines that
//! do not yet have WinAppRuntime, so the UI must be plain USER32+GDI.
//! Fluent-dark styling is applied manually: #202020 surface, #60CDFF accent,
//! dark title bar (DWMWA_USE_IMMERSIVE_DARK_MODE), `DarkMode_Explorer`-themed
//! controls, Per-Monitor-V2 DPI.
//!
//! Layout is one grid in DIPs (`CLIENT_W` wide, 32-DIP margins, header band,
//! body, footer band with the buttons right-aligned to the same margin). The
//! window is sized from its *client* area at the target DPI and every child is
//! placed by `layout`, which also runs on `WM_DPICHANGED`.
//!
//! Work runs on a std::thread; the worker updates the status/progress child
//! windows via SendMessage (marshalled to the UI thread) and posts a final
//! WM_APP to swap the window to its Done state.

use crate::{APP, UninstallPlan, VER, human_size, install_steps, uninstall_plan, uninstall_steps};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus::{
    FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath,
    GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
    GdipDeletePen, GdipDrawLine, GdipDrawLines, GdipDrawPath, GdipFillEllipse, GdipFillPath,
    GdipSetSmoothingMode, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpGraphics, GpPath,
    PointF, SmoothingModeAntiAlias, UnitPixel,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

// COLORREF is 0x00BBGGRR.
const BG: u32 = 0x00202020;
const CARD: u32 = 0x002B2B2B;
const CARD_EDGE: u32 = 0x003A3A3A;
const TEXT: u32 = 0x00E9E9E9;
const SUBTLE: u32 = 0x009C9C9C;
const ACCENT: u32 = 0x00FFCD60; // #60CDFF
const ACCENT_HOT: u32 = 0x00FFDD94;
const DANGER: u32 = 0x007D6AF9; // #F96A7D
const OK: u32 = 0x005FCB6C; // #6CCB5F
const LINE: u32 = 0x00363636;
const ON_ACCENT: u32 = 0x00151515;

const IDC_EDIT: i32 = 100;
const IDC_BROWSE: i32 = 101;
const IDC_CHK_SHORTCUT: i32 = 102;
const IDC_CHK_PATH: i32 = 103;
const IDC_CHK_PURGE: i32 = 104;
const IDC_PRIMARY: i32 = 110;
const IDC_CANCEL: i32 = 111;
const IDC_STATUS: i32 = 112;
const IDC_PROG: i32 = 113;

const WM_APP_DONE: u32 = WM_APP + 1;
/// lparam = Box<Option<PathBuf>> from the detached picker thread.
const WM_APP_PICKED: u32 = WM_APP + 2;

// ---------------------------------------------------------------- layout (DIPs)

const CLIENT_W: i32 = 620;
const MARGIN: i32 = 32;
/// Hairline under the header band.
const HEADER_LINE: i32 = 96;
/// Height of the footer band (hairline on top, buttons inside).
const FOOT_H: i32 = 76;
const BTN_H: i32 = 36;
const BTN_W: i32 = 132;
const CANCEL_W: i32 = 104;
const BTN_GAP: i32 = 8;

fn client_h(mode: Mode) -> i32 {
    match mode {
        Mode::Install => 372,
        Mode::Uninstall => 424,
    }
}

/// Y of the footer hairline.
fn footer_y(mode: Mode) -> i32 {
    client_h(mode) - FOOT_H
}

// ---------------------------------------------------------------- GDI+ AA helpers
// GDI RoundRect/paths are aliased (visible burrs on rounded corners); GDI+
// draws them anti-aliased. gdiplus.dll ships with Windows — no dependency.

/// COLORREF (0x00BBGGRR) → GDI+ ARGB (0xAARRGGBB).
fn argb(c: u32) -> u32 {
    0xFF00_0000 | ((c & 0xFF) << 16) | (c & 0xFF00) | (c >> 16)
}

fn gp_graphics(hdc: HDC) -> *mut GpGraphics {
    unsafe {
        let mut g = std::ptr::null_mut();
        let _ = GdipCreateFromHDC(hdc, &mut g);
        if !g.is_null() {
            let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        }
        g
    }
}

fn rr_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> *mut GpPath {
    unsafe {
        let mut p = std::ptr::null_mut();
        let _ = GdipCreatePath(FillModeAlternate, &mut p);
        if p.is_null() {
            return p;
        }
        let d = (r * 2.0).min(w).min(h);
        let _ = GdipAddPathArc(p, x, y, d, d, 180.0, 90.0);
        let _ = GdipAddPathArc(p, x + w - d, y, d, d, 270.0, 90.0);
        let _ = GdipAddPathArc(p, x + w - d, y + h - d, d, d, 0.0, 90.0);
        let _ = GdipAddPathArc(p, x, y + h - d, d, d, 90.0, 90.0);
        let _ = GdipClosePathFigure(p);
        p
    }
}

fn fill_rr(hdc: HDC, rc: &RECT, r: f32, color: u32) {
    unsafe {
        let g = gp_graphics(hdc);
        if g.is_null() {
            return;
        }
        let p = rr_path(
            rc.left as f32,
            rc.top as f32,
            (rc.right - rc.left) as f32,
            (rc.bottom - rc.top) as f32,
            r,
        );
        let mut br = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(argb(color), &mut br);
        let _ = GdipFillPath(g, br.cast(), p);
        let _ = GdipDeleteBrush(br.cast());
        let _ = GdipDeletePath(p);
        let _ = GdipDeleteGraphics(g);
    }
}

fn stroke_rr(hdc: HDC, rc: &RECT, r: f32, color: u32, width: f32) {
    unsafe {
        let g = gp_graphics(hdc);
        if g.is_null() {
            return;
        }
        // Stroke centered on the path — inset by half the pen width so the
        // border lands inside the rect rather than bleeding out.
        let i = width / 2.0;
        let p = rr_path(
            rc.left as f32 + i,
            rc.top as f32 + i,
            (rc.right - rc.left) as f32 - width,
            (rc.bottom - rc.top) as f32 - width,
            r,
        );
        let mut pen = std::ptr::null_mut();
        let _ = GdipCreatePen1(argb(color), width, UnitPixel, &mut pen);
        let _ = GdipDrawPath(g, pen, p);
        let _ = GdipDeletePen(pen);
        let _ = GdipDeletePath(p);
        let _ = GdipDeleteGraphics(g);
    }
}

fn draw_check(hdc: HDC, x: f32, y: f32, s: f32, color: u32) {
    unsafe {
        let g = gp_graphics(hdc);
        if g.is_null() {
            return;
        }
        let mut pen = std::ptr::null_mut();
        let _ = GdipCreatePen1(argb(color), s * 0.12, UnitPixel, &mut pen);
        let pts = [
            PointF {
                X: x + s * 0.20,
                Y: y + s * 0.55,
            },
            PointF {
                X: x + s * 0.42,
                Y: y + s * 0.76,
            },
            PointF {
                X: x + s * 0.82,
                Y: y + s * 0.26,
            },
        ];
        let _ = GdipDrawLines(g, pen, pts.as_ptr(), 3);
        let _ = GdipDeletePen(pen);
        let _ = GdipDeleteGraphics(g);
    }
}

/// Filled disc; `alpha` 0–255 (tinted badges use ~0x38, the success mark 0xFF).
fn fill_circle(hdc: HDC, cx: f32, cy: f32, r: f32, color: u32, alpha: u8) {
    unsafe {
        let g = gp_graphics(hdc);
        if g.is_null() {
            return;
        }
        let mut br = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(
            (argb(color) & 0x00FF_FFFF) | (u32::from(alpha) << 24),
            &mut br,
        );
        let _ = GdipFillEllipse(g, br.cast(), cx - r, cy - r, r * 2.0, r * 2.0);
        let _ = GdipDeleteBrush(br.cast());
        let _ = GdipDeleteGraphics(g);
    }
}

fn draw_minus(hdc: HDC, x: f32, y: f32, s: f32, color: u32) {
    unsafe {
        let g = gp_graphics(hdc);
        if g.is_null() {
            return;
        }
        let mut pen = std::ptr::null_mut();
        let _ = GdipCreatePen1(argb(color), s * 0.12, UnitPixel, &mut pen);
        let _ = GdipDrawLine(g, pen, x + s * 0.28, y + s * 0.5, x + s * 0.72, y + s * 0.5);
        let _ = GdipDeletePen(pen);
        let _ = GdipDeleteGraphics(g);
    }
}

#[derive(Clone, Copy)]
enum Glyph {
    Remove,
    Keep,
}

/// Tinted disc with a minus (goes away) or a check (stays), `s` px square.
fn draw_glyph(hdc: HDC, x: i32, y: i32, s: i32, kind: Glyph) {
    let c = match kind {
        Glyph::Remove => DANGER,
        Glyph::Keep => OK,
    };
    fill_circle(
        hdc,
        x as f32 + s as f32 / 2.0,
        y as f32 + s as f32 / 2.0,
        s as f32 / 2.0,
        c,
        0x38,
    );
    match kind {
        Glyph::Remove => draw_minus(hdc, x as f32, y as f32, s as f32, c),
        Glyph::Keep => draw_check(hdc, x as f32, y as f32, s as f32, c),
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Install,
    Uninstall,
}

#[derive(Default)]
struct Shared {
    ok: bool,
    err: String,
    launch: Option<PathBuf>,
}

/// Fonts for one DPI — built once per `layout`, not per paint.
struct Fonts {
    title: HFONT,
    sub: HFONT,
    body: HFONT,
    small: HFONT,
    label: HFONT,
    btn: HFONT,
    btn_bold: HFONT,
    done: HFONT,
}

impl Fonts {
    fn new(dpi: u32) -> Self {
        Self {
            title: font(20.0, true, dpi),
            sub: font(11.0, false, dpi),
            body: font(10.5, false, dpi),
            small: font(9.5, false, dpi),
            label: font(10.5, false, dpi),
            btn: font(11.0, false, dpi),
            btn_bold: font(11.0, true, dpi),
            done: font(17.0, true, dpi),
        }
    }

    fn free(&self) {
        for f in [
            self.title,
            self.sub,
            self.body,
            self.small,
            self.label,
            self.btn,
            self.btn_bold,
            self.done,
        ] {
            unsafe {
                let _ = DeleteObject(f.into());
            }
        }
    }
}

struct Gui {
    mode: Mode,
    dir: PathBuf,
    /// Some(v) when an existing install is being upgraded — copy reads
    /// "更新"; picking a different dir migrates (old dir auto-cleaned).
    update_from: Option<String>,
    /// Recorded InstallLocation of the existing install, if any.
    prior_dir: Option<PathBuf>,
    /// Uninstall only: what will be removed / kept (read once at startup).
    plan: Option<UninstallPlan>,
    /// Uninstall only: also delete the user-data directory (off by default).
    purge: bool,
    /// After a purge: the data directory is still there (locked file, …).
    data_left: bool,
    hinst: HINSTANCE,
    dpi: u32,
    fonts: Fonts,
    hicon: HICON,
    prog: HWND,
    status: HWND,
    primary: HWND,
    cancel: HWND,
    edit: HWND,
    browse: HWND,
    chk1: HWND,
    chk2: HWND,
    chk_purge: HWND,
    edit_focus: bool,
    // Owner-drawn checkboxes: BS_AUTOCHECKBOX|BS_OWNERDRAW collapses to
    // plain owner-draw (0x03|0x0B=0x0B) so Windows never toggles the check —
    // state lives here and clicks flip it manually.
    chk_shortcut: bool,
    chk_path: bool,
    // The brush is returned to Windows on every WM_CTLCOLOREDIT — must be
    // pre-allocated, not created per-message (GDI object leak).
    field_brush: HBRUSH,
    working: bool,
    done_ok: bool,
    failed: bool,
    /// A folder-pick dialog is in flight — the button stays disabled until
    /// WM_APP_PICKED lands (picked/cancelled) so slow dialogs can't stack.
    pick_pending: bool,
    shared: Arc<Mutex<Shared>>,
}

impl Gui {
    /// The uninstall summary lists a data card only when there is data.
    fn has_data(&self) -> bool {
        self.plan.as_ref().is_some_and(|p| p.data_dir.is_some())
    }
}

/// UTF-16 with trailing NUL — bind the Vec in a `let` so the pointer outlives
/// the API call (a `PCWSTR(v.as_ptr())` from a temporary would dangle).
fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Logical-unit scaler for a DPI value.
fn sc(x: i32, dpi: u32) -> i32 {
    x * dpi as i32 / 96
}

/// DPI of a window. Debug builds honor `GTT_SETUP_DPI` so the layout can be
/// checked at other scales without another monitor.
fn dpi_of(hwnd: HWND) -> u32 {
    #[cfg(debug_assertions)]
    if let Some(d) = std::env::var("GTT_SETUP_DPI")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        return d;
    }
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        d => d,
    }
}

/// DPI to size the window for before it exists (primary monitor).
fn system_dpi() -> u32 {
    #[cfg(debug_assertions)]
    if let Some(d) = std::env::var("GTT_SETUP_DPI")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        return d;
    }
    match unsafe { GetDpiForSystem() } {
        0 => 96,
        d => d,
    }
}

fn font(pt: f32, semibold: bool, dpi: u32) -> HFONT {
    unsafe {
        CreateFontW(
            -(pt * dpi as f32 / 72.0) as i32,
            0,
            0,
            0,
            (if semibold { FW_SEMIBOLD } else { FW_NORMAL }).0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            u32::from(DEFAULT_PITCH.0),
            w!("Segoe UI"),
        )
    }
}

fn hmenu_id(id: i32) -> HMENU {
    HMENU(id as isize as *mut _)
}

fn make_btn(parent: HWND, text: &[u16], id: i32, owner_drawn: bool, hinst: HINSTANCE) -> HWND {
    unsafe {
        let style = WS_CHILD
            | WS_VISIBLE
            | WINDOW_STYLE(WS_TABSTOP.0)
            | WINDOW_STYLE(if owner_drawn {
                BS_OWNERDRAW as u32
            } else {
                BS_PUSHBUTTON as u32
            });
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(text.as_ptr()),
            style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(hmenu_id(id)),
            Some(hinst),
            None,
        )
        .unwrap_or_default();
        let _ = SetWindowTheme(h, w!("DarkMode_Explorer"), None);
        h
    }
}

fn set_prog(hwnd: HWND, pct: u32) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, pct.clamp(0, 1000) as isize);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

extern "system" fn prog_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let h = (rc.bottom - rc.top) as f32;
            fill_rr(hdc, &rc, h / 2.0, CARD);
            let pct = GetWindowLongPtrW(hwnd, GWLP_USERDATA).clamp(0, 1000) as i32;
            if pct > 0 {
                let w = (rc.right - rc.left).max(1) * pct / 1000;
                let fill = RECT {
                    left: rc.left,
                    top: rc.top,
                    right: rc.left + w.max(h as i32),
                    bottom: rc.bottom,
                };
                fill_rr(hdc, &fill, h / 2.0, ACCENT);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

fn gui(hwnd: HWND) -> *mut Gui {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Gui }
}

/// Store the status text and repaint (callable from the worker thread — the
/// text lives in a hidden STATIC, which marshals `SetWindowTextW` for us).
fn set_status(hwnd: HWND, text: &str) {
    let s = w(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(s.as_ptr()));
        if let Ok(parent) = GetParent(hwnd) {
            let _ = InvalidateRect(Some(parent), None, false);
        }
    }
}

// ---------------------------------------------------------------- layout

fn place(h: HWND, x: i32, y: i32, wd: i32, ht: i32) {
    if h.0.is_null() {
        return;
    }
    unsafe {
        let _ = SetWindowPos(h, None, x, y, wd, ht, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn set_font(h: HWND, f: HFONT) {
    if h.0.is_null() {
        return;
    }
    unsafe {
        let _ = SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
    }
}

fn show(h: HWND, visible: bool) {
    if h.0.is_null() {
        return;
    }
    unsafe {
        let _ = ShowWindow(h, if visible { SW_SHOW } else { SW_HIDE });
    }
}

const BROWSE_W: i32 = 100;

/// Where the status line is painted: left of the buttons, above the progress
/// bar. (A STATIC control rendered CJK text ~20% larger than the same font
/// drawn by the parent, so the text is painted here; the STATIC stays hidden
/// as the thread-safe text store the worker writes to.)
fn status_rect(dpi: u32, mode: Mode) -> RECT {
    let sx = |v: i32| sc(v, dpi);
    let btn_y = sx(footer_y(mode) + (FOOT_H - BTN_H) / 2);
    let cancel_x = sx(CLIENT_W - MARGIN - BTN_W) - sx(BTN_GAP + CANCEL_W);
    RECT {
        left: sx(MARGIN),
        top: btn_y + sx(3),
        right: cancel_x - sx(20),
        bottom: btn_y + sx(3) + sx(18),
    }
}

/// Frame of the install-path field (the "Browse" button follows it).
fn edit_frame(dpi: u32) -> RECT {
    let sx = |v: i32| sc(v, dpi);
    let x = sx(MARGIN);
    let w = sx(CLIENT_W - 2 * MARGIN) - sx(BROWSE_W) - sx(8);
    RECT {
        left: x,
        top: sx(140),
        right: x + w,
        bottom: sx(140) + sx(30),
    }
}

/// Position every child and (re)build DPI-dependent resources. Runs once
/// after creation and again on WM_DPICHANGED.
fn layout(hwnd: HWND, g: &mut Gui) {
    let dpi = dpi_of(hwnd);
    g.dpi = dpi;
    let sx = |v: i32| sc(v, dpi);

    let fonts = Fonts::new(dpi);
    set_font(g.edit, fonts.body);
    set_font(g.chk1, fonts.body);
    set_font(g.chk2, fonts.body);
    set_font(g.chk_purge, fonts.body);
    set_font(g.browse, fonts.btn);
    set_font(g.primary, fonts.btn);
    set_font(g.cancel, fonts.btn);
    g.fonts.free();
    g.fonts = fonts;

    // Icon at the size the header draws it (44 DIPs).
    unsafe {
        if !g.hicon.0.is_null() {
            let _ = DestroyIcon(g.hicon);
        }
        g.hicon = LoadImageW(
            Some(g.hinst),
            PCWSTR(std::ptr::without_provenance::<u16>(1)),
            IMAGE_ICON,
            sx(44),
            sx(44),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
    }

    let full = sx(CLIENT_W - 2 * MARGIN);
    let foot = footer_y(g.mode);
    // Footer: buttons right-aligned to the margin; status + progress fill the
    // space to their left (status over the bar, both centred on the buttons).
    let btn_y = sx(foot + (FOOT_H - BTN_H) / 2);
    let prim_x = sx(CLIENT_W - MARGIN - BTN_W);
    let cancel_x = prim_x - sx(BTN_GAP + CANCEL_W);
    place(g.primary, prim_x, btn_y, sx(BTN_W), sx(BTN_H));
    place(g.cancel, cancel_x, btn_y, sx(CANCEL_W), sx(BTN_H));
    let left_w = cancel_x - sx(MARGIN) - sx(20);
    place(g.prog, sx(MARGIN), btn_y + sx(BTN_H - 8), left_w, sx(6));

    if g.mode == Mode::Install {
        // The edit is inset inside a frame the parent paints, so its single
        // text line sits vertically centred (a bare Win32 edit hugs the top).
        let fr = edit_frame(dpi);
        place(
            g.edit,
            fr.left + sx(10),
            fr.top + sx(5),
            fr.right - fr.left - sx(20),
            sx(20),
        );
        place(
            g.browse,
            fr.right + sx(8),
            fr.top,
            sx(BROWSE_W),
            fr.bottom - fr.top,
        );
        place(g.chk1, sx(MARGIN), sx(190), full, sx(22));
        place(g.chk2, sx(MARGIN), sx(218), full, sx(22));
    } else {
        place(g.chk_purge, sx(MARGIN), sx(312), full, sx(22));
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

// ---------------------------------------------------------------- painting

fn rect(x: i32, y: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: r,
        bottom: b,
    }
}

fn put(hdc: HDC, text: &str, rc: RECT, font: HFONT, color: u32, flags: DRAW_TEXT_FORMAT) {
    unsafe {
        let old = SelectObject(hdc, font.into());
        let _ = SetTextColor(hdc, COLORREF(color));
        let mut buf = w(text);
        let n = buf.len() - 1;
        let mut r = rc;
        DrawTextW(hdc, &mut buf[..n], &mut r, flags);
        SelectObject(hdc, old);
    }
}

fn hline(hdc: HDC, x0: i32, x1: i32, y: i32) {
    unsafe {
        let ln = CreateSolidBrush(COLORREF(LINE));
        FillRect(hdc, &rect(x0, y, x1, y + 1), ln);
        let _ = DeleteObject(ln.into());
    }
}

const LEFT_1: DRAW_TEXT_FORMAT =
    DRAW_TEXT_FORMAT(DT_LEFT.0 | DT_SINGLELINE.0 | DT_VCENTER.0 | DT_END_ELLIPSIS.0);

/// One card row: glyph, label (left) and an optional right-aligned value.
fn card_row(
    hdc: HDC,
    g: &Gui,
    x: i32,
    right: i32,
    y: i32,
    kind: Glyph,
    label: &str,
    value: Option<&str>,
) {
    let sx = |v: i32| sc(v, g.dpi);
    draw_glyph(hdc, x + sx(16), y + sx(3), sx(16), kind);
    let text_x = x + sx(42);
    if let Some(v) = value {
        put(
            hdc,
            v,
            rect(text_x, y, right - sx(16), y + sx(22)),
            g.fonts.small,
            SUBTLE,
            DRAW_TEXT_FORMAT(DT_RIGHT.0 | DT_SINGLELINE.0 | DT_VCENTER.0),
        );
    }
    put(
        hdc,
        label,
        rect(
            text_x,
            y,
            right - sx(16) - if value.is_some() { sx(80) } else { 0 },
            y + sx(22),
        ),
        g.fonts.body,
        TEXT,
        LEFT_1,
    );
}

fn paint(hdc: HDC, rc: RECT, g: &Gui) {
    let sx = |v: i32| sc(v, g.dpi);
    let f = &g.fonts;
    let m = sx(MARGIN);
    let right = rc.right - m;

    // ---- header: app icon, title, subtitle
    unsafe {
        if !g.hicon.0.is_null() {
            let _ = DrawIconEx(hdc, m, sx(26), g.hicon, sx(44), sx(44), 0, None, DI_NORMAL);
        }
    }
    put(
        hdc,
        APP,
        rect(sx(88), sx(22), right, sx(54)),
        f.title,
        TEXT,
        LEFT_1,
    );
    let sub = match g.mode {
        Mode::Uninstall => format!("卸载程序 — v{VER}"),
        Mode::Install => match &g.update_from {
            Some(old) if old != VER => format!("已安装 v{old} — 更新至 v{VER}"),
            Some(_) => format!("已安装 v{VER} — 重装修复"),
            None => format!("本地 AI 编码工具用量统计 — v{VER}"),
        },
    };
    put(
        hdc,
        &sub,
        rect(sx(88), sx(56), right, sx(78)),
        f.sub,
        SUBTLE,
        LEFT_1,
    );
    hline(hdc, m, right, sx(HEADER_LINE));
    hline(hdc, m, right, sx(footer_y(g.mode)));

    if g.done_ok {
        paint_done(hdc, rc, g);
        return;
    }
    match g.mode {
        Mode::Install => {
            // Rounded field around the path edit: card fill (the edit paints
            // the same colour), accent stroke while focused, hairline otherwise.
            let frame = edit_frame(g.dpi);
            fill_rr(hdc, &frame, sx(6) as f32, CARD);
            stroke_rr(
                hdc,
                &frame,
                sx(6) as f32,
                if g.edit_focus { ACCENT } else { 0x004A4A4A },
                1.0,
            );
            put(
                hdc,
                "安装位置",
                rect(m, sx(116), m + sx(200), sx(136)),
                f.label,
                SUBTLE,
                LEFT_1,
            );
            put(
                hdc,
                "无需管理员权限 · 用户数据保存在 %USERPROFILE%\\.globaltokentracker",
                rect(m, sx(262), right, sx(282)),
                f.small,
                SUBTLE,
                LEFT_1,
            );
        }
        Mode::Uninstall => paint_uninstall(hdc, rc, g),
    }
    // Status line (progress narrative / failure) above the progress bar.
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(g.status, &mut buf) }.max(0) as usize;
    if n > 0 {
        put(
            hdc,
            &String::from_utf16_lossy(&buf[..n]),
            status_rect(g.dpi, g.mode),
            f.label,
            if g.failed { DANGER } else { SUBTLE },
            LEFT_1,
        );
    }
}

/// Uninstall confirmation: what goes away (left) and what stays (right).
fn paint_uninstall(hdc: HDC, rc: RECT, g: &Gui) {
    let sx = |v: i32| sc(v, g.dpi);
    let f = &g.fonts;
    let m = sx(MARGIN);
    let right = rc.right - m;
    let Some(plan) = &g.plan else { return };

    // Install location — the thing being removed, on one line.
    put(
        hdc,
        "安装位置",
        rect(m, sx(110), m + sx(64), sx(130)),
        f.small,
        SUBTLE,
        LEFT_1,
    );
    put(
        hdc,
        &g.dir.display().to_string(),
        rect(m + sx(70), sx(110), right, sx(130)),
        f.small,
        ACCENT,
        DRAW_TEXT_FORMAT(DT_LEFT.0 | DT_SINGLELINE.0 | DT_VCENTER.0 | DT_PATH_ELLIPSIS.0),
    );

    let gap = sx(12);
    let cw = (right - m - gap) / 2;
    let (y0, ch) = (sx(140), sx(160));
    let left_card = rect(m, y0, m + cw, y0 + ch);
    let right_card = rect(m + cw + gap, y0, right, y0 + ch);
    let rad = sx(8) as f32;

    // ---- left: removed
    fill_rr(hdc, &left_card, rad, CARD);
    stroke_rr(hdc, &left_card, rad, CARD_EDGE, 1.0);
    put(
        hdc,
        "将被移除",
        rect(
            left_card.left + sx(16),
            y0 + sx(12),
            left_card.right - sx(16),
            y0 + sx(32),
        ),
        f.small,
        SUBTLE,
        LEFT_1,
    );
    let mut y = y0 + sx(42);
    let program = human_size(plan.program_bytes);
    card_row(
        hdc,
        g,
        left_card.left,
        left_card.right,
        y,
        Glyph::Remove,
        "程序文件",
        Some(&program),
    );
    y += sx(26);
    for (present, label) in [
        (plan.shortcuts, "开始菜单快捷方式"),
        (plan.on_path, "用户 PATH 中的安装目录"),
        (plan.registered, "“应用和功能”中的卸载项"),
    ] {
        if present {
            card_row(
                hdc,
                g,
                left_card.left,
                left_card.right,
                y,
                Glyph::Remove,
                label,
                None,
            );
            y += sx(26);
        }
    }

    // ---- right: kept (or, when the box is ticked, removed too)
    fill_rr(hdc, &right_card, rad, CARD);
    stroke_rr(
        hdc,
        &right_card,
        rad,
        if g.purge { DANGER } else { CARD_EDGE },
        1.0,
    );
    let title = if !g.has_data() {
        "用户数据"
    } else if g.purge {
        "将一并删除"
    } else {
        "将保留"
    };
    put(
        hdc,
        title,
        rect(
            right_card.left + sx(16),
            y0 + sx(12),
            right_card.right - sx(16),
            y0 + sx(32),
        ),
        f.small,
        if g.purge { DANGER } else { SUBTLE },
        LEFT_1,
    );
    let y = y0 + sx(42);
    if g.has_data() {
        let size = human_size(plan.data_bytes);
        card_row(
            hdc,
            g,
            right_card.left,
            right_card.right,
            y,
            if g.purge { Glyph::Remove } else { Glyph::Keep },
            "用量账本与设置",
            Some(&size),
        );
        let tx = right_card.left + sx(42);
        put(
            hdc,
            "%USERPROFILE%\\.globaltokentracker",
            rect(tx, y + sx(28), right_card.right - sx(16), y + sx(48)),
            f.small,
            SUBTLE,
            LEFT_1,
        );
        let (note, c) = if g.purge {
            ("此操作不可恢复", DANGER)
        } else {
            ("重新安装后自动沿用", SUBTLE)
        };
        put(
            hdc,
            note,
            rect(tx, y + sx(50), right_card.right - sx(16), y + sx(70)),
            f.small,
            c,
            LEFT_1,
        );
    } else {
        put(
            hdc,
            "未发现用户数据",
            rect(
                right_card.left + sx(16),
                y,
                right_card.right - sx(16),
                y + sx(22),
            ),
            f.body,
            SUBTLE,
            LEFT_1,
        );
    }
}

/// Finished state: a success mark and what happened, centred in the body.
fn paint_done(hdc: HDC, rc: RECT, g: &Gui) {
    let sx = |v: i32| sc(v, g.dpi);
    let f = &g.fonts;
    let m = sx(MARGIN);
    let (body_top, body_bot) = (sx(HEADER_LINE), sx(footer_y(g.mode)));
    let block = sx(150);
    let top = body_top + (body_bot - body_top - block) / 2;
    let cx = (rc.right / 2) as f32;
    fill_circle(hdc, cx, (top + sx(28)) as f32, sx(28) as f32, OK, 0xFF);
    let s = sx(40) as f32;
    draw_check(
        hdc,
        cx - s / 2.0,
        (top + sx(28)) as f32 - s / 2.0,
        s,
        ON_ACCENT,
    );

    let (title, line1, line2, line2_color) = match g.mode {
        Mode::Install => (
            if g.update_from.is_some() {
                "更新完成"
            } else {
                "安装完成"
            },
            format!("{APP} v{VER} 已就绪"),
            g.dir.display().to_string(),
            ACCENT,
        ),
        Mode::Uninstall => (
            "卸载完成",
            format!("{APP} 已从此电脑移除"),
            if g.purge && !g.data_left {
                "用户数据已一并删除".to_string()
            } else if g.purge {
                "用户数据未能完全删除，可手动删除 %USERPROFILE%\\.globaltokentracker".to_string()
            } else if g.has_data() {
                "用户数据保留在 %USERPROFILE%\\.globaltokentracker（重新安装后自动沿用）"
                    .to_string()
            } else {
                String::new()
            },
            if g.purge && g.data_left {
                DANGER
            } else {
                SUBTLE
            },
        ),
    };
    let center = DRAW_TEXT_FORMAT(DT_CENTER.0 | DT_SINGLELINE.0 | DT_VCENTER.0);
    put(
        hdc,
        title,
        rect(0, top + sx(68), rc.right, top + sx(100)),
        f.done,
        TEXT,
        center,
    );
    put(
        hdc,
        &line1,
        rect(m, top + sx(104), rc.right - m, top + sx(126)),
        f.body,
        TEXT,
        center,
    );
    put(
        hdc,
        &line2,
        rect(m, top + sx(130), rc.right - m, top + sx(150)),
        f.small,
        line2_color,
        DRAW_TEXT_FORMAT(DT_CENTER.0 | DT_SINGLELINE.0 | DT_VCENTER.0 | DT_PATH_ELLIPSIS.0),
    );
}

// ---------------------------------------------------------------- window proc

/// Owner-drawn checkbox — same rounded/flat vocabulary as the buttons so the
/// glyph and text match the dark surface. The purge box turns red when armed.
fn draw_checkbox(di: &DRAWITEMSTRUCT, g: &Gui, checked: bool, danger: bool) {
    let sx = |v: i32| sc(v, g.dpi);
    let pressed = di.itemState.0 & ODS_SELECTED.0 != 0;
    let enabled = di.itemState.0 & ODS_DISABLED.0 == 0;
    unsafe {
        let mut rc = di.rcItem;
        let bg = CreateSolidBrush(COLORREF(BG));
        FillRect(di.hDC, &rc, bg);
        let _ = DeleteObject(bg.into());
        let bs = sx(17);
        let bx = rc.left;
        let by = rc.top + (rc.bottom - rc.top - bs) / 2;
        let brc = rect(bx, by, bx + bs, by + bs);
        let on = if danger { DANGER } else { ACCENT };
        let face = if checked {
            on
        } else if pressed {
            0x003A3A3A
        } else {
            CARD
        };
        fill_rr(di.hDC, &brc, sx(4) as f32, face);
        let edge = if checked {
            on
        } else if enabled {
            0x005A5A5A
        } else {
            0x00404040
        };
        stroke_rr(di.hDC, &brc, sx(4) as f32, edge, 1.0);
        if checked {
            draw_check(di.hDC, bx as f32, by as f32, bs as f32, ON_ACCENT);
        }
        let _ = SetBkMode(di.hDC, TRANSPARENT);
        let _ = SetTextColor(di.hDC, COLORREF(if enabled { TEXT } else { SUBTLE }));
        let old = SelectObject(di.hDC, g.fonts.body.into());
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(di.hwndItem, &mut buf).max(0) as usize;
        rc.left = bx + bs + sx(9);
        DrawTextW(
            di.hDC,
            &mut buf[..n],
            &mut rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(di.hDC, old);
    }
}

fn draw_button(di: &DRAWITEMSTRUCT, g: &Gui) {
    let sx = |v: i32| sc(v, g.dpi);
    let pressed = di.itemState.0 & ODS_SELECTED.0 != 0;
    let enabled = di.itemState.0 & ODS_DISABLED.0 == 0;
    let is_primary = di.CtlID == IDC_PRIMARY as u32;
    let (fill, txt) = if is_primary {
        // The uninstall button is red until the run is done; it goes deeper
        // red when the purge box is armed.
        let accent = if g.mode == Mode::Uninstall && !g.done_ok {
            DANGER
        } else {
            ACCENT
        };
        let f = if !enabled {
            0x00484848
        } else if pressed {
            ACCENT_HOT
        } else {
            accent
        };
        (f, if enabled { ON_ACCENT } else { 0x00AAAAAA })
    } else {
        let f = if pressed { 0x003A3A3A } else { 0x00333333 };
        (f, TEXT)
    };
    unsafe {
        let mut rc = di.rcItem;
        let rad = sx(6) as f32;
        fill_rr(di.hDC, &rc, rad, fill);
        if !is_primary {
            stroke_rr(di.hDC, &rc, rad, 0x00555555, 1.0);
        }
        let _ = SetBkMode(di.hDC, TRANSPARENT);
        let _ = SetTextColor(di.hDC, COLORREF(txt));
        let old = SelectObject(
            di.hDC,
            if is_primary {
                g.fonts.btn_bold
            } else {
                g.fonts.btn
            }
            .into(),
        );
        let mut buf = [0u16; 64];
        let n = GetWindowTextW(di.hwndItem, &mut buf).max(0) as usize;
        DrawTextW(
            di.hDC,
            &mut buf[..n],
            &mut rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(di.hDC, old);
    }
}

/// Swap the window to its Done state (install / uninstall finished).
fn finish_ok(hwnd: HWND, g: &mut Gui) {
    g.done_ok = true;
    g.failed = false;
    g.data_left = g.purge
        && g.plan
            .as_ref()
            .and_then(|p| p.data_dir.as_deref())
            .is_some_and(Path::exists);
    set_status(g.status, "");
    set_prog(g.prog, 1000);
    for h in [g.edit, g.browse, g.chk1, g.chk2, g.chk_purge, g.prog] {
        show(h, false);
    }
    let lbl = w(if g.mode == Mode::Install {
        "启动并关闭"
    } else {
        "关闭"
    });
    unsafe {
        let _ = SetWindowTextW(g.primary, PCWSTR(lbl.as_ptr()));
        let _ = ShowWindow(g.cancel, SW_HIDE);
        let _ = EnableWindow(g.primary, true);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wpar: WPARAM, lpar: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => unsafe {
            let cs = lpar.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            LRESULT(0)
        },
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => unsafe {
            let g = &*gui(hwnd);
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            // Double-buffered full paint — no flicker.
            let mem = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
            let old = SelectObject(mem, bmp.into());
            let bg = CreateSolidBrush(COLORREF(BG));
            FillRect(mem, &rc, bg);
            let _ = DeleteObject(bg.into());
            let _ = SetBkMode(mem, TRANSPARENT);
            paint(mem, rc, g);
            // Restore BEFORE deleting bmp — and crucially AFTER the BitBlt,
            // otherwise the blit sources from the 1x1 stock bitmap and the
            // client stays blank.
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        },
        WM_DPICHANGED => unsafe {
            let g = &mut *gui(hwnd);
            let r = &*(lpar.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            layout(hwnd, g);
            LRESULT(0)
        },
        WM_CTLCOLOREDIT => unsafe {
            let g = &*gui(hwnd);
            let hdc = HDC(wpar.0 as *mut _);
            let _ = SetTextColor(hdc, COLORREF(TEXT));
            let _ = SetBkColor(hdc, COLORREF(CARD));
            LRESULT(g.field_brush.0 as isize)
        },
        WM_CTLCOLORBTN => unsafe {
            // Checkboxes: themed face + transparent text background.
            let hdc = HDC(wpar.0 as *mut _);
            let _ = SetTextColor(hdc, COLORREF(TEXT));
            let _ = SetBkMode(hdc, TRANSPARENT);
            LRESULT(GetStockObject(NULL_BRUSH).0 as isize)
        },
        WM_DRAWITEM => unsafe {
            let di = &*(lpar.0 as *const DRAWITEMSTRUCT);
            if di.CtlType != ODT_BUTTON {
                return LRESULT(0);
            }
            let g = &*gui(hwnd);
            match di.CtlID as i32 {
                IDC_CHK_SHORTCUT => draw_checkbox(di, g, g.chk_shortcut, false),
                IDC_CHK_PATH => draw_checkbox(di, g, g.chk_path, false),
                IDC_CHK_PURGE => draw_checkbox(di, g, g.purge, true),
                _ => draw_button(di, g),
            }
            LRESULT(1)
        },
        WM_COMMAND => unsafe {
            let id = (wpar.0 & 0xFFFF) as i32;
            let g = &mut *gui(hwnd);
            // Edit focus notifications → repaint so its frame turns accent.
            if id == IDC_EDIT
                && matches!((wpar.0 >> 16) as u32, x if x == EN_SETFOCUS || x == EN_KILLFOCUS)
            {
                g.edit_focus = (wpar.0 >> 16) as u32 == EN_SETFOCUS;
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            match id {
                IDC_BROWSE => {
                    if !g.pick_pending {
                        g.pick_pending = true;
                        set_status(g.status, "打开文件夹选择器…（若无响应请直接输入路径）");
                        let _ = EnableWindow(g.browse, false);
                        pick_folder(hwnd);
                    }
                    LRESULT(0)
                }
                IDC_PRIMARY => {
                    if g.working {
                        return LRESULT(0);
                    }
                    if g.done_ok {
                        let launch = g.shared.lock().unwrap().launch.clone();
                        if let Some(exe) = launch {
                            let s = w(&exe.display().to_string());
                            ShellExecuteW(
                                Some(hwnd),
                                w!("open"),
                                PCWSTR(s.as_ptr()),
                                PCWSTR::null(),
                                PCWSTR::null(),
                                SW_SHOW,
                            );
                        }
                        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                        return LRESULT(0);
                    }
                    if g.mode == Mode::Install {
                        let mut buf = vec![0u16; 1024];
                        let n = GetWindowTextW(g.edit, &mut buf).max(0) as usize;
                        let d = PathBuf::from(String::from_utf16_lossy(&buf[..n]).trim());
                        if d.as_os_str().is_empty() {
                            let t = w("安装目录不能为空");
                            let c = w(APP);
                            MessageBoxW(
                                Some(hwnd),
                                PCWSTR(t.as_ptr()),
                                PCWSTR(c.as_ptr()),
                                MB_ICONWARNING,
                            );
                            return LRESULT(0);
                        }
                        g.dir = d;
                    } else if g.purge {
                        // Irreversible — ask once more, defaulting to "No".
                        let bytes = g.plan.as_ref().map_or(0, |p| p.data_bytes);
                        let t = w(&format!(
                            "将永久删除用量账本、设置与备份（{}），无法恢复。\n\n仍要继续卸载吗？",
                            human_size(bytes)
                        ));
                        let c = w("确认删除用户数据");
                        let r = MessageBoxW(
                            Some(hwnd),
                            PCWSTR(t.as_ptr()),
                            PCWSTR(c.as_ptr()),
                            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                        );
                        if r != IDYES {
                            return LRESULT(0);
                        }
                    }
                    start_work(hwnd);
                    LRESULT(0)
                }
                IDC_CANCEL => {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    LRESULT(0)
                }
                IDC_CHK_SHORTCUT => {
                    g.chk_shortcut = !g.chk_shortcut;
                    let _ = InvalidateRect(Some(g.chk1), None, false);
                    LRESULT(0)
                }
                IDC_CHK_PATH => {
                    g.chk_path = !g.chk_path;
                    let _ = InvalidateRect(Some(g.chk2), None, false);
                    LRESULT(0)
                }
                IDC_CHK_PURGE => {
                    g.purge = !g.purge;
                    // The right-hand card flips between "kept" and "deleted".
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wpar, lpar),
            }
        },
        WM_APP_PICKED => unsafe {
            let g = &mut *gui(hwnd);
            let res = Box::from_raw(lpar.0 as *mut Option<PathBuf>);
            if let Some(p) = *res {
                let s = w(&p.display().to_string());
                let _ = SetWindowTextW(g.edit, PCWSTR(s.as_ptr()));
            }
            set_status(g.status, "");
            g.pick_pending = false;
            let _ = EnableWindow(g.browse, true);
            LRESULT(0)
        },
        WM_APP_DONE => unsafe {
            let g = &mut *gui(hwnd);
            g.working = false;
            let (ok, err) = {
                let s = g.shared.lock().unwrap();
                (s.ok, s.err.clone())
            };
            if ok {
                finish_ok(hwnd, g);
            } else {
                g.failed = true;
                set_status(g.status, &format!("失败：{err}"));
                let _ = EnableWindow(g.primary, true);
                let _ = EnableWindow(g.cancel, true);
                // Re-enable inputs so the user can fix the path and retry.
                for h in [g.edit, g.browse, g.chk1, g.chk2, g.chk_purge] {
                    if !h.0.is_null() {
                        let _ = EnableWindow(h, true);
                    }
                }
            }
            LRESULT(0)
        },
        WM_CLOSE => unsafe {
            let g = &*gui(hwnd);
            if g.working {
                // Closing now would kill the worker mid-install and leave a
                // half-written program dir — defer the close instead.
                set_status(g.status, "正在执行，完成后可关闭");
                return LRESULT(0);
            }
            if g.mode == Mode::Uninstall && g.done_ok {
                // Our exe lives inside the dir being deleted — only schedule
                // the deferred rmdir now that the window is really closing.
                let _ = crate::schedule_dir_delete(&g.dir);
            }
            DefWindowProcW(hwnd, msg, wpar, lpar)
        },
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wpar, lpar) },
    }
}

/// Folder picker on a detached STA helper thread — the common dialog
/// enumerates shell network locations and can take seconds (or hang) on
/// machines with a dead mapped drive. Never block the installer UI: the
/// result (or cancel) arrives later as WM_APP_PICKED; a hung dialog just
/// leaks the helper until process exit and the user can type the path.
fn pick_folder(hwnd: HWND) {
    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let out: Option<PathBuf> = pick_folder_inner(HWND(raw as *mut _));
        // The window reads Box<Option<PathBuf>> in lparam and frees it.
        let boxed = Box::new(out);
        let _ = unsafe {
            PostMessageW(
                Some(HWND(raw as *mut _)),
                WM_APP_PICKED,
                WPARAM(0),
                LPARAM(Box::into_raw(boxed) as isize),
            )
        };
    });
}

fn pick_folder_inner(hwnd: HWND) -> Option<PathBuf> {
    unsafe {
        // Helper thread needs its own STA.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        dlg.SetOptions(dlg.GetOptions().ok()? | FOS_PICKFOLDERS)
            .ok()?;
        if dlg.Show(Some(hwnd)).is_err() {
            return None;
        }
        let item = dlg.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        Some(PathBuf::from(path.to_string().ok()?))
    }
}

fn start_work(hwnd: HWND) {
    let g = unsafe { &mut *gui(hwnd) };
    g.working = true;
    g.failed = false;
    unsafe {
        let _ = EnableWindow(g.primary, false);
        let _ = EnableWindow(g.cancel, false);
        for h in [g.edit, g.browse, g.chk1, g.chk2, g.chk_purge] {
            if !h.0.is_null() {
                let _ = EnableWindow(h, false);
            }
        }
        let _ = ShowWindow(g.prog, SW_SHOW);
    }
    let want_shortcut = g.mode == Mode::Install && g.chk_shortcut;
    let want_path = g.mode == Mode::Install && g.chk_path;
    let purge = g.mode == Mode::Uninstall && g.purge;
    let dir = g.dir.clone();
    let prior_dir = g.prior_dir.clone();
    let mode = g.mode;
    let shared = g.shared.clone();
    // HWNDs are raw pointers in windows 0.62 → !Send. Cross the thread
    // boundary as usize, rebuild inside the worker.
    let status = g.status.0 as usize;
    let prog = g.prog.0 as usize;
    let hwnd_ptr = hwnd.0 as usize;
    std::thread::spawn(move || {
        let status = HWND(status as *mut _);
        let prog = HWND(prog as *mut _);
        let hwnd = HWND(hwnd_ptr as *mut _);
        let mut step = |pct: u32, msg: &str| {
            set_status(status, msg);
            set_prog(prog, pct);
        };
        // No console in GUI mode — println! on an invalid stdout panics and
        // would kill this worker mid-install. Detail lines are dropped; step
        // labels + the final error message carry the GUI narrative.
        let log = |_msg: String| {};
        let res = match mode {
            Mode::Install => {
                install_steps(&dir, want_shortcut, want_path, &mut step, &log).map(|()| {
                    // Update that moved dirs — retire the old program dir.
                    if let Some(old) = &prior_dir {
                        crate::cleanup_prior_install(old, &dir, &log);
                    }
                })
            }
            Mode::Uninstall => uninstall_steps(&dir, purge, &mut step, &log),
        };
        {
            let mut s = shared.lock().unwrap();
            match res {
                Ok(()) => {
                    s.ok = true;
                    if mode == Mode::Install {
                        s.launch = Some(dir.join("globaltokentracker-ui.exe"));
                    }
                }
                Err(e) => {
                    s.err = e.to_string();
                }
            }
        }
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_DONE, WPARAM(0), LPARAM(0));
        }
    });
}

/// Debug-build screenshot hooks: `GTT_SETUP_PREVIEW=fresh,purge,work,done,fail`
/// puts the window into that state without running anything.
#[cfg(debug_assertions)]
fn apply_preview(hwnd: HWND, g: &mut Gui) {
    let Ok(spec) = std::env::var("GTT_SETUP_PREVIEW") else {
        return;
    };
    for tok in spec.split(',') {
        match tok.trim() {
            "fresh" => {
                g.update_from = None;
                if g.mode == Mode::Install {
                    let t = w("安装");
                    unsafe {
                        let _ = SetWindowTextW(g.primary, PCWSTR(t.as_ptr()));
                    }
                }
            }
            "purge" => g.purge = true,
            "work" => {
                g.working = true;
                unsafe {
                    let _ = EnableWindow(g.primary, false);
                    let _ = EnableWindow(g.cancel, false);
                    let _ = ShowWindow(g.prog, SW_SHOW);
                }
                set_status(g.status, "释放程序文件…");
                set_prog(g.prog, 450);
            }
            "fail" => {
                g.failed = true;
                set_status(g.status, "失败：无法写入目标目录（拒绝访问）");
            }
            "done" => finish_ok(hwnd, g),
            "dataleft" => g.data_left = true,
            _ => {}
        }
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

pub fn run(
    mode: Mode,
    initial_dir: &Path,
    update_from: Option<&str>,
    prior_dir: Option<PathBuf>,
) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // GDI+ for anti-aliased rounded drawing (system DLL, always present).
        let mut gdip_token = 0usize;
        let gdip_in = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: FALSE,
            SuppressExternalCodecs: FALSE,
        };
        let _ = GdiplusStartup(&mut gdip_token, &gdip_in, std::ptr::null_mut());
        let hinst: HINSTANCE = GetModuleHandleW(PCWSTR::null())?.into();
        // Icon resource #1 is embedded by build.rs (assets/icon.ico) —
        // MAKEINTRESOURCEW(1) = a non-provenance pointer carrying the ordinal.
        let hicon = LoadIconW(Some(hinst), PCWSTR(std::ptr::without_provenance::<u16>(1)))
            .unwrap_or_default();
        let wc = WNDCLASSW {
            hInstance: hinst,
            lpszClassName: w!("GttSetupWnd"),
            lpfnWndProc: Some(wnd_proc),
            hIcon: hicon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: CreateSolidBrush(COLORREF(BG)),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let pc = WNDCLASSW {
            hInstance: hinst,
            lpszClassName: w!("GttProgress"),
            lpfnWndProc: Some(prog_proc),
            ..Default::default()
        };
        RegisterClassW(&pc);

        let dpi0 = system_dpi();
        let state = Box::new(Gui {
            mode,
            dir: initial_dir.to_path_buf(),
            update_from: update_from.map(str::to_string),
            prior_dir,
            plan: (mode == Mode::Uninstall).then(|| uninstall_plan(initial_dir)),
            purge: false,
            data_left: false,
            hinst,
            dpi: dpi0,
            fonts: Fonts::new(dpi0),
            hicon: HICON::default(),
            prog: HWND::default(),
            status: HWND::default(),
            primary: HWND::default(),
            cancel: HWND::default(),
            edit: HWND::default(),
            browse: HWND::default(),
            chk1: HWND::default(),
            chk2: HWND::default(),
            chk_purge: HWND::default(),
            edit_focus: false,
            chk_shortcut: true,
            chk_path: true,
            field_brush: CreateSolidBrush(COLORREF(CARD)),
            working: false,
            done_ok: false,
            failed: false,
            pick_pending: false,
            shared: Arc::new(Mutex::new(Shared::default())),
        });
        let state_ptr = Box::into_raw(state);

        let title = w(match mode {
            Mode::Uninstall => "GlobalTokenTracker 卸载",
            Mode::Install => {
                if update_from.is_some() {
                    "GlobalTokenTracker 更新"
                } else {
                    "GlobalTokenTracker 安装"
                }
            }
        });
        // Size the window from its CLIENT area at the target DPI (the frame
        // adds its own share), and centre it on the primary work area.
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut wr = RECT {
            left: 0,
            top: 0,
            right: sc(CLIENT_W, dpi0),
            bottom: sc(client_h(mode), dpi0),
        };
        let _ = AdjustWindowRectExForDpi(&mut wr, style, false, WINDOW_EX_STYLE(0), dpi0);
        let (ow, oh) = (wr.right - wr.left, wr.bottom - wr.top);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut work).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let x = work.left + ((work.right - work.left) - ow) / 2;
        let y = work.top + ((work.bottom - work.top) - oh) / 2;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("GttSetupWnd"),
            PCWSTR(title.as_ptr()),
            style,
            x,
            y,
            ow,
            oh,
            None,
            None,
            Some(hinst),
            Some(state_ptr.cast_const().cast()),
        )?;
        let dark = TRUE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&dark).cast(),
            4,
        );

        let g = &mut *state_ptr;

        if mode == Mode::Install {
            let ed_t = w(&initial_dir.display().to_string());
            // No WS_EX_CLIENTEDGE — the 3D sunken edge fights the flat style;
            // the parent paints a rounded frame around it instead. The edit
            // text is vertically centred by the control's own margins.
            g.edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                PCWSTR(ed_t.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(WS_TABSTOP.0 | ES_AUTOHSCROLL as u32),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(hmenu_id(IDC_EDIT)),
                Some(hinst),
                None,
            )?;
            let _ = SetWindowTheme(g.edit, w!("DarkMode_Explorer"), None);
            let br_t = w("浏览…");
            g.browse = make_btn(hwnd, &br_t, IDC_BROWSE, true, hinst);
            // Plain owner-drawn buttons (not AUTOCHECKBOX — the style bits
            // collide); clicks arrive as WM_COMMAND and flip Gui state.
            let c1t = w("创建开始菜单快捷方式");
            g.chk1 = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                PCWSTR(c1t.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(WS_TABSTOP.0 | BS_OWNERDRAW as u32),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(hmenu_id(IDC_CHK_SHORTCUT)),
                Some(hinst),
                None,
            )?;
            let c2t = w("加入用户 PATH（终端可直接运行 CLI）");
            g.chk2 = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                PCWSTR(c2t.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(WS_TABSTOP.0 | BS_OWNERDRAW as u32),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(hmenu_id(IDC_CHK_PATH)),
                Some(hinst),
                None,
            )?;
        } else if g.has_data() {
            let ct = w("同时删除用户数据（用量账本、设置与备份，不可恢复）");
            g.chk_purge = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                PCWSTR(ct.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(WS_TABSTOP.0 | BS_OWNERDRAW as u32),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(hmenu_id(IDC_CHK_PURGE)),
                Some(hinst),
                None,
            )?;
        }

        // Hidden: only holds the status text (see `status_rect`).
        g.status = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            WS_CHILD,
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(hmenu_id(IDC_STATUS)),
            Some(hinst),
            None,
        )?;
        g.prog = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("GttProgress"),
            w!(""),
            WS_CHILD,
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(hmenu_id(IDC_PROG)),
            Some(hinst),
            None,
        )?;
        let ptxt = w(match mode {
            Mode::Uninstall => "卸载",
            Mode::Install => {
                if update_from.is_some() {
                    "更新"
                } else {
                    "安装"
                }
            }
        });
        g.primary = make_btn(hwnd, &ptxt, IDC_PRIMARY, true, hinst);
        let ctxt = w("取消");
        g.cancel = make_btn(hwnd, &ctxt, IDC_CANCEL, true, hinst);

        layout(hwnd, g);
        #[cfg(debug_assertions)]
        apply_preview(hwnd, g);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).into() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
        GdiplusShutdown(gdip_token);
        // state_ptr intentionally leaked — it lives as long as the process.
        Ok(())
    }
}
