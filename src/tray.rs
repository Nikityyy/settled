//! The tray icon, the shared settings, and the message loop.

use crate::panel;
use crate::window::wide;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_ANY,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const APP_KEY: PCWSTR = w!(r"Software\settled");

/// The green that means "done": the icon, and every switch that is on.
pub const GREEN: (u8, u8, u8) = (0x1E, 0x94, 0x61);
/// Paused. At least 3.3:1 against the white check and against light and dark surfaces.
pub const GREY: (u8, u8, u8) = (0x8A, 0x8A, 0x8A);

static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The icon while watching, and the grey one while paused.
static ICON: AtomicIsize = AtomicIsize::new(0);
static ICON_PAUSED: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<String> = Mutex::new(String::new());

pub fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
    notify(NIM_MODIFY);
}

pub fn status() -> String {
    STATUS.lock().unwrap().clone()
}

/// Shows `status` in the tooltip and in the panel. Safe to call from any thread.
pub fn set_status(status: &str) {
    let mut current = STATUS.lock().unwrap();
    if *current != status {
        *current = status.to_owned();
        drop(current);
        notify(NIM_MODIFY);
        panel::refresh();
    }
}

/// Creates the tray icon and runs the message loop until the user quits.
pub fn run() {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("settled");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        });
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE::default(), class, class, WINDOW_STYLE::default(),
            0, 0, 0, 0, None, None, instance, None,
        ) else {
            return;
        };
        WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
        let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem());
        ICON.store(make_icon(size, GREEN).0 as isize, Ordering::Relaxed);
        ICON_PAUSED.store(make_icon(size, GREY).0 as isize, Ordering::Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        notify(NIM_ADD);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Removes the tray icon and ends the message loop.
pub fn quit() {
    notify(NIM_DELETE);
    unsafe { PostQuitMessage(0) };
}

fn tray_hwnd() -> HWND {
    HWND(WINDOW.load(Ordering::Relaxed) as *mut _)
}

fn notify(action: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) {
    let hwnd = tray_hwnd();
    if hwnd.is_invalid() {
        return;
    }
    let status = status();
    let tip = if status.is_empty() { "settled".to_owned() } else { format!("settled: {status}") };
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: HICON(if paused() { &ICON_PAUSED } else { &ICON }.load(Ordering::Relaxed) as *mut _),
        ..Default::default()
    };
    for (slot, unit) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *slot = unit;
    }
    unsafe {
        let _ = Shell_NotifyIconW(action, &data);
    }
}

/// The tray icon's rectangle on screen, in physical pixels.
fn icon_rect() -> Option<RECT> {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: tray_hwnd(),
        uID: 1,
        ..Default::default()
    };
    unsafe { Shell_NotifyIconGetRect(&id).ok() }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam.0 & 0xffff) as u32;
            if event == WM_LBUTTONUP || event == WM_RBUTTONUP {
                panel::toggle(icon_rect());
            }
            LRESULT(0)
        }
        _ if msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer restarted and lost every tray icon.
            notify(NIM_ADD);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn autostart() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, w!("settled"), RRF_RT_ANY, None, None, None).is_ok() }
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let command = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("settled"), REG_SZ.0,
                Some(command.as_ptr().cast()), (command.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("settled"));
        }
    }
}

/// Turns autostart on at the first launch, and keeps its path current if the exe moved.
pub fn init_autostart() {
    let first_launch =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, APP_KEY, w!("initialized"), RRF_RT_ANY, None, None, None).is_err() };
    if first_launch || autostart() {
        set_autostart(true);
    }
    if first_launch {
        let one: u32 = 1;
        unsafe {
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, APP_KEY, w!("initialized"), REG_DWORD.0,
                Some((&one as *const u32).cast()), 4);
        }
    }
}

/// Draws the tray icon at `size` pixels: a disc in `fill` with a white check mark.
unsafe fn make_icon(size: i32, fill: (u8, u8, u8)) -> HICON {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let Ok(color) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
        return HICON::default();
    };
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
    let s = size as f32 / 32.0;
    let check = [(9.0 * s, 16.5 * s), (14.0 * s, 21.5 * s), (23.0 * s, 11.5 * s)];
    for y in 0..size {
        for x in 0..size {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            let disc = (16.0 * s - 0.5 - ((p.0 - 16.0 * s).powi(2) + (p.1 - 16.0 * s).powi(2)).sqrt()).clamp(0.0, 1.0);
            let line = segment_distance(p, check[0], check[1]).min(segment_distance(p, check[1], check[2]));
            let mark = (2.6 * s - line + 0.5).clamp(0.0, 1.0);
            // Premultiplied by the disc's coverage, as Windows expects for 32-bit icons.
            let mix = |base: u8| ((base as f32 + (255.0 - base as f32) * mark) * disc) as u32;
            pixels[(y * size + x) as usize] =
                ((disc * 255.0) as u32) << 24 | mix(fill.0) << 16 | mix(fill.1) << 8 | mix(fill.2);
        }
    }
    let mask_bits = vec![0u8; (size * size / 8) as usize];
    let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));
    let icon = CreateIconIndirect(&ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color })
        .unwrap_or_default();
    let _ = DeleteObject(mask);
    let _ = DeleteObject(color);
    icon
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}
