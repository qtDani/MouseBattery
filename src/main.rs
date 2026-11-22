//! MouseBattery - a tiny tray battery indicator for Logitech mice (HID++), no G HUB needed.
#![windows_subsystem = "windows"]

mod hid;
mod hidpp;

use std::ffi::c_void;
use std::ptr::{addr_of_mut, null, null_mut};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use hidpp::{BatteryState, Charge, Msg, Shared};

const APP_NAME: &str = "MouseBattery";
const SETTINGS_KEY: &str = "Software\\MouseBattery";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_UPDATE: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const HOVER_TIMER: usize = 1;

const NIN_SELECT_: u32 = WM_USER;
const NIN_POPUPOPEN_: u32 = WM_USER + 6;

const ID_REFRESH: usize = 10;
const ID_NUMERIC: usize = 11;
const ID_AUTOSTART: usize = 12;
const ID_EXIT: usize = 13;

struct App {
    hwnd: HWND,
    popup: HWND,
    shared: Arc<Shared>,
    tx: std::sync::mpsc::Sender<Msg>,
    icon: HICON,
    numeric: bool,
    taskbar_created: u32,
    /// Area the cursor must stay in to keep the hover box open
    hover_area: RECT,
    hover_exact: bool,
    popup_text: Vec<u16>,
    popup_font: HFONT,
    popup_dpi: u32,
}

static mut APP: Option<App> = None;

fn app() -> &'static mut App {
    // Only touched from the UI thread
    unsafe { (*addr_of_mut!(APP)).as_mut().unwrap() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- registry

fn reg_get_dword(subkey: &str, name: &str) -> Option<u32> {
    let (k, n) = (wide(subkey), wide(name));
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut v as *mut u32 as *mut c_void, &mut size)
    };
    (r == ERROR_SUCCESS).then_some(v)
}

fn reg_get_string(subkey: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(subkey), wide(name));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size)
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn reg_set_dword(subkey: &str, name: &str, v: u32) {
    let (k, n) = (wide(subkey), wide(name));
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_DWORD, &v as *const u32 as *const c_void, 4);
    }
}

fn exe_command() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    format!("\"{}\"", exe)
}

fn autostart_enabled() -> bool {
    reg_get_string(RUN_KEY, APP_NAME).map(|v| v.eq_ignore_ascii_case(&exe_command())).unwrap_or(false)
}

fn set_autostart(on: bool) {
    let (k, n) = (wide(RUN_KEY), wide(APP_NAME));
    unsafe {
        if on {
            let v = wide(&exe_command());
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr() as *const c_void, (v.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr());
        }
    }
}

fn taskbar_is_light() -> bool {
    reg_get_dword("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize", "SystemUsesLightTheme") == Some(1)
}

// ---------------------------------------------------------------- text

fn status_lines(s: &BatteryState) -> String {
    if !s.found {
        return "Mouse battery\nLooking for your mouse...".into();
    }
    let battery = if s.percent < 0 {
        "Battery: reading...".to_string()
    } else {
        let state = match s.charge {
            Charge::Charging => "Charging",
            Charge::Full => "Fully charged",
            _ => "On battery",
        };
        format!("Battery: {}% - {}", s.percent, state)
    };
    if s.updated.is_some() && !s.online {
        return format!("{}\n{}\nMouse asleep or off", s.name, battery);
    }
    format!("{}\n{}", s.name, battery)
}

// ---------------------------------------------------------------- icon drawing

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Premultiplied BGRA pixel from a 0xRRGGBB colour and coverage 0-255
fn px(color: u32, a: u32) -> u32 {
    let r = ((color >> 16) & 0xFF) * a / 255;
    let g = ((color >> 8) & 0xFF) * a / 255;
    let b = (color & 0xFF) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

unsafe fn dib(size: i32) -> (HBITMAP, *mut u32) {
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = size;
    bi.bmiHeader.biHeight = -size; // top-down
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(null_mut(), &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    (bmp, bits as *mut u32)
}

fn make_icon(s: &BatteryState, numeric: bool) -> HICON {
    unsafe {
        let dpi = GetDpiForSystem();
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16);
        let light = taskbar_is_light();
        let fg = if light { rgb(0x1A, 0x1A, 0x1A) } else { rgb(0xFF, 0xFF, 0xFF) };
        let accent = if !s.found || s.percent < 0 {
            fg
        } else if s.charge == Charge::Charging || s.charge == Charge::Full {
            if light { rgb(0x10, 0x7C, 0x10) } else { rgb(0x6C, 0xCB, 0x5F) }
        } else if s.percent <= 15 {
            if light { rgb(0xC4, 0x2B, 0x1C) } else { rgb(0xFF, 0x63, 0x4F) }
        } else {
            fg
        };
        let dim = !s.found || !s.online;

        let (bmp, bits) = dib(size);
        let n = (size * size) as usize;
        let pixels = std::slice::from_raw_parts_mut(bits, n);

        if numeric {
            // Render white text with GDI, then use its brightness as coverage
            let text: Vec<u16> = if s.percent < 0 { "?".encode_utf16().collect() } else { s.percent.to_string().encode_utf16().collect() };
            let dc = CreateCompatibleDC(null_mut());
            let old_bmp = SelectObject(dc, bmp);
            let face = wide("Segoe UI");
            let mut h = size + 2;
            let mut font;
            loop {
                font = CreateFontW(-h, 0, 0, 0, 600, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, ANTIALIASED_QUALITY as u32, (DEFAULT_PITCH as u32) | (FF_SWISS as u32), face.as_ptr());
                let old = SelectObject(dc, font);
                let mut sz: SIZE = std::mem::zeroed();
                GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut sz);
                SelectObject(dc, old);
                if sz.cx <= size + 1 || h <= 6 {
                    break;
                }
                DeleteObject(font);
                h -= 1;
            }
            let old_font = SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FFFFFF);
            let mut rc = RECT { left: -1, top: 0, right: size + 1, bottom: size };
            DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            GdiFlush();
            for p in pixels.iter_mut() {
                let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
                let cov = if dim { cov * 55 / 100 } else { cov };
                *p = px(accent, cov);
            }
            SelectObject(dc, old_font);
            DeleteObject(font);
            SelectObject(dc, old_bmp);
            DeleteDC(dc);
        } else {
            // Battery glyph: outline + fill proportional to the charge
            let u = (size as f32 / 16.0).max(1.0);
            let t = u.round() as i32; // stroke
            let x0 = (0.5 * u) as i32;
            let x1 = size - (2.5 * u).round() as i32;
            let y0 = (size as f32 * 0.25).round() as i32;
            let y1 = size - y0;
            let set = |pixels: &mut [u32], x: i32, y: i32, c: u32| {
                if x >= 0 && y >= 0 && x < size && y < size {
                    pixels[(y * size + x) as usize] = c;
                }
            };
            let alpha = if dim { 140 } else { 255 };
            for y in y0..y1 {
                for x in x0..x1 {
                    let edge = x < x0 + t || x >= x1 - t || y < y0 + t || y >= y1 - t;
                    if edge {
                        set(pixels, x, y, px(fg, alpha));
                    }
                }
            }
            // terminal nub
            let ny0 = y0 + (y1 - y0) / 3;
            let ny1 = y1 - (y1 - y0) / 3;
            for y in ny0..ny1 {
                for x in x1..(x1 + (1.5 * u).round() as i32) {
                    set(pixels, x, y, px(fg, alpha));
                }
            }
            // fill
            let pct = s.percent.clamp(0, 100);
            let ix0 = x0 + t + 1;
            let ix1 = x1 - t - 1;
            let fill_w = ((ix1 - ix0) as f32 * pct as f32 / 100.0).round() as i32;
            let fill_w = if pct > 0 { fill_w.max(1) } else { 0 };
            for y in (y0 + t + 1)..(y1 - t - 1) {
                for x in ix0..(ix0 + fill_w) {
                    set(pixels, x, y, px(accent, alpha));
                }
            }
        }

        let mask_bits = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr() as *const c_void);
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: bmp };
        let icon = CreateIconIndirect(&info);
        DeleteObject(mask);
        DeleteObject(bmp);
        icon
    }
}

// ---------------------------------------------------------------- tray icon

fn tray_data(a: &App) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = a.hwnd;
    nid.uID = TRAY_ID;
    nid
}

fn set_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    let t: Vec<u16> = text.encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..t.len()].copy_from_slice(&t);
    nid.szTip[t.len()] = 0;
}

fn tray_add() {
    let a = app();
    let mut nid = tray_data(a);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP; // no NIF_SHOWTIP: we draw our own hover box
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = a.icon;
    set_tip(&mut nid, "Mouse battery");
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
}

fn refresh_ui() {
    let a = app();
    let state = a.shared.state.lock().unwrap().clone();
    let new_icon = make_icon(&state, a.numeric);
    let mut nid = tray_data(a);
    nid.uFlags = NIF_ICON | NIF_TIP;
    nid.hIcon = new_icon;
    let tip = status_lines(&state).replace('\n', " - ");
    set_tip(&mut nid, &tip);
    unsafe {
        Shell_NotifyIconW(NIM_MODIFY, &nid);
        if !a.icon.is_null() {
            DestroyIcon(a.icon);
        }
    }
    a.icon = new_icon;
    if unsafe { IsWindowVisible(a.popup) } != 0 {
        layout_popup(false);
    }
}

// ---------------------------------------------------------------- hover box

fn monitor_info(pt: POINT) -> (MONITORINFO, u32) {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) != 0 {
            dx = GetDpiForSystem();
        }
        (mi, dx)
    }
}

fn icon_rect() -> Option<RECT> {
    let a = app();
    unsafe {
        let mut id: NOTIFYICONIDENTIFIER = std::mem::zeroed();
        id.cbSize = std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32;
        id.hWnd = a.hwnd;
        id.uID = TRAY_ID;
        let mut r: RECT = std::mem::zeroed();
        if Shell_NotifyIconGetRect(&id, &mut r) == 0 && r.right > r.left && r.bottom > r.top {
            Some(r)
        } else {
            None
        }
    }
}

fn inside(r: &RECT, p: &POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// Measures the text, sizes the box and positions it centred on the tray icon.
fn layout_popup(reposition: bool) {
    let a = app();
    let state = a.shared.state.lock().unwrap().clone();
    let text = status_lines(&state);
    a.popup_text = text.encode_utf16().collect();

    unsafe {
        let mut cursor: POINT = std::mem::zeroed();
        GetCursorPos(&mut cursor);
        let anchor = if a.hover_exact { a.hover_area } else { RECT { left: cursor.x, top: cursor.y, right: cursor.x, bottom: cursor.y } };
        let center = POINT { x: (anchor.left + anchor.right) / 2, y: (anchor.top + anchor.bottom) / 2 };
        let (mi, dpi) = monitor_info(center);

        if a.popup_dpi != dpi || a.popup_font.is_null() {
            if !a.popup_font.is_null() {
                DeleteObject(a.popup_font);
            }
            let face = wide("Segoe UI");
            a.popup_font = CreateFontW(-(9 * dpi as i32 / 72), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, face.as_ptr());
            a.popup_dpi = dpi;
        }
        let dc = GetDC(a.popup);
        let old = SelectObject(dc, a.popup_font);
        let mut rc = RECT { left: 0, top: 0, right: 2000, bottom: 2000 };
        DrawTextW(dc, a.popup_text.as_ptr(), a.popup_text.len() as i32, &mut rc, DT_CALCRECT | DT_NOPREFIX);
        SelectObject(dc, old);
        ReleaseDC(a.popup, dc);

        let s = |v: i32| v * dpi as i32 / 96;
        let w = rc.right + s(10) * 2 + 2;
        let h = rc.bottom + s(6) * 2 + 2;

        if reposition || IsWindowVisible(a.popup) == 0 {
            let work = mi.rcWork;
            let gap = s(8);
            let y = if center.y >= work.bottom {
                work.bottom.min(anchor.top) - h - gap // taskbar at the bottom
            } else if center.y < work.top {
                work.top.max(anchor.bottom) + gap // taskbar at the top
            } else {
                anchor.top - h - gap // overflow flyout or side taskbar
            };
            let x = (center.x - w / 2).clamp(mi.rcMonitor.left + 4, mi.rcMonitor.right - w - 4);
            SetWindowPos(a.popup, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        } else {
            let mut wr: RECT = std::mem::zeroed();
            GetWindowRect(a.popup, &mut wr);
            // keep it centred on the same point when the text width changes
            let cx = (wr.left + wr.right) / 2;
            SetWindowPos(a.popup, HWND_TOPMOST, cx - w / 2, wr.bottom - h, w, h, SWP_NOACTIVATE);
        }
        InvalidateRect(a.popup, null(), 1);
    }
}

fn show_hover() {
    let a = app();
    unsafe {
        let mut cursor: POINT = std::mem::zeroed();
        GetCursorPos(&mut cursor);
        if IsWindowVisible(a.popup) != 0 {
            if !a.hover_exact {
                a.hover_area = RECT { left: cursor.x - 12, top: cursor.y - 12, right: cursor.x + 12, bottom: cursor.y + 12 };
            }
            return;
        }
        match icon_rect() {
            Some(r) if inside(&RECT { left: r.left - 4, top: r.top - 4, right: r.right + 4, bottom: r.bottom + 4 }, &cursor) => {
                a.hover_area = r;
                a.hover_exact = true;
            }
            _ => {
                // Taskbar replacements may not report the icon position: follow the cursor instead
                a.hover_area = RECT { left: cursor.x - 12, top: cursor.y - 12, right: cursor.x + 12, bottom: cursor.y + 12 };
                a.hover_exact = false;
            }
        }
        layout_popup(true);
        SetTimer(a.hwnd, HOVER_TIMER, 100, None);
    }
}

fn hide_hover() {
    let a = app();
    unsafe {
        KillTimer(a.hwnd, HOVER_TIMER);
        ShowWindow(a.popup, SW_HIDE);
    }
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let a = app();
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let light = taskbar_is_light();
            let (bg, border, fg) = if light {
                (0x00F9F9F9u32, 0x00CCCCCCu32, 0x00000000u32)
            } else {
                (0x002B2B2Bu32, 0x00505050u32, 0x00FFFFFFu32)
            };
            let bg_brush = CreateSolidBrush(bg);
            let border_brush = CreateSolidBrush(border);
            FillRect(dc, &rc, bg_brush);
            FrameRect(dc, &rc, border_brush);
            DeleteObject(bg_brush);
            DeleteObject(border_brush);
            let old = SelectObject(dc, a.popup_font);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, fg);
            let pad_x = 10 * a.popup_dpi as i32 / 96 + 1;
            let pad_y = 6 * a.popup_dpi as i32 / 96 + 1;
            let mut trc = RECT { left: rc.left + pad_x, top: rc.top + pad_y, right: rc.right - pad_x, bottom: rc.bottom - pad_y };
            DrawTextW(dc, a.popup_text.as_ptr(), a.popup_text.len() as i32, &mut trc, DT_NOPREFIX);
            SelectObject(dc, old);
            EndPaint(hwnd, &ps);
            0
        }
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- menu

fn show_menu() {
    hide_hover();
    let a = app();
    unsafe {
        let state = a.shared.state.lock().unwrap().clone();
        let menu = CreatePopupMenu();
        let header = wide(&status_lines(&state).lines().take(2).collect::<Vec<_>>().join("  -  "));
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, header.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        let items: [(usize, &str, bool); 3] = [
            (ID_REFRESH, "Refresh now", false),
            (ID_NUMERIC, "Show percentage number", a.numeric),
            (ID_AUTOSTART, "Start with Windows", autostart_enabled()),
        ];
        for (id, label, checked) in items {
            let l = wide(label);
            AppendMenuW(menu, MF_STRING | if checked { MF_CHECKED } else { 0 }, id, l.as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        let exit = wide("Exit");
        AppendMenuW(menu, MF_STRING, ID_EXIT, exit.as_ptr());

        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(a.hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, pt.x, pt.y, 0, a.hwnd, null());
        PostMessageW(a.hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

// ---------------------------------------------------------------- main window

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(a) = (*addr_of_mut!(APP)).as_mut() else { return DefWindowProcW(hwnd, msg, wparam, lparam) };
    if msg == a.taskbar_created && msg != 0 {
        tray_add();
        refresh_ui();
        return 0;
    }
    match msg {
        WM_APP_TRAY => {
            match (lparam as u32) & 0xFFFF {
                WM_MOUSEMOVE | NIN_POPUPOPEN_ | NIN_SELECT_ | WM_LBUTTONUP => show_hover(),
                WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(),
                _ => {}
            }
            0
        }
        WM_APP_UPDATE => {
            refresh_ui();
            0
        }
        WM_TIMER if wparam == HOVER_TIMER => {
            let mut p: POINT = std::mem::zeroed();
            GetCursorPos(&mut p);
            let r = a.hover_area;
            let grace = RECT { left: r.left - 2, top: r.top - 2, right: r.right + 2, bottom: r.bottom + 2 };
            if !inside(&grace, &p) {
                hide_hover();
            }
            0
        }
        WM_COMMAND => {
            match wparam & 0xFFFF {
                ID_REFRESH => {
                    let _ = a.tx.send(Msg::Refresh);
                }
                ID_NUMERIC => {
                    a.numeric = !a.numeric;
                    reg_set_dword(SETTINGS_KEY, "NumericIcon", a.numeric as u32);
                    refresh_ui();
                }
                ID_AUTOSTART => set_autostart(!autostart_enabled()),
                ID_EXIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED | WM_DISPLAYCHANGE => {
            refresh_ui();
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            let nid = tray_data(a);
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--probe") {
        let report = hidpp::probe_report();
        if let Some(path) = args.get(i + 1) {
            let _ = std::fs::write(path, &report);
        }
        return;
    }

    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let mutex_name = wide("Local\\MouseBatteryTray");
        CreateMutexW(null(), 1, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return; // already running
        }

        let hinst = GetModuleHandleW(null());
        let main_class = wide("MouseBatteryWnd");
        let popup_class = wide("MouseBatteryPopup");
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = main_class.as_ptr();
        RegisterClassExW(&wc);
        wc.lpfnWndProc = Some(popup_proc);
        wc.lpszClassName = popup_class.as_ptr();
        wc.style = CS_DROPSHADOW;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        RegisterClassExW(&wc);

        let title = wide(APP_NAME);
        let hwnd = CreateWindowExW(0, main_class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            popup_class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0, 0, 10, 10,
            null_mut(), null_mut(), hinst, null(),
        );

        let hwnd_val = hwnd as usize;
        let shared = Arc::new(Shared {
            state: Mutex::new(BatteryState::default()),
            notify: Box::new(move || {
                PostMessageW(hwnd_val as HWND, WM_APP_UPDATE, 0, 0);
            }),
        });
        let tx = hidpp::spawn(shared.clone());
        let tb = wide("TaskbarCreated");

        *addr_of_mut!(APP) = Some(App {
            hwnd,
            popup,
            shared,
            tx,
            icon: null_mut(),
            numeric: reg_get_dword(SETTINGS_KEY, "NumericIcon").map(|v| v != 0).unwrap_or(true),
            taskbar_created: RegisterWindowMessageW(tb.as_ptr()),
            hover_area: std::mem::zeroed(),
            hover_exact: false,
            popup_text: Vec::new(),
            popup_font: null_mut(),
            popup_dpi: 0,
        });
        let state = app().shared.state.lock().unwrap().clone();
        app().icon = make_icon(&state, app().numeric);
        tray_add();

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
