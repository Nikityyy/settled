//! The flyout that opens from the tray icon: one status line, two switches and Quit.
//!
//! Drawn with Direct2D and DirectWrite. Every size is in DIPs (1/96 inch) on a 4 DIP grid.
//! The window's corners have an 8 DIP radius (Windows 11 default) and rows sit 4 DIPs in
//! from the edge, so their highlight uses 8 - 4 = 4 and the two curves stay concentric.

use crate::tray;
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};
use windows::core::{w, Result};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, GetSysColor, InvalidateRect, MonitorFromPoint, COLOR_HIGHLIGHT,
    COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_SPACE,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// Layout, top to bottom: 16 padding, 36 header, 12 gap, three 40 rows with an 8 divider
// band before the last, 4 padding. 16 + 36 + 12 + 40 + 40 + 8 + 40 + 4 = 196.
// The 12 gap makes the space under the header a little larger than the space between
// two row labels (40 minus the text height), so the header reads as its own group.
const WIDTH: f32 = 288.0;
const HEIGHT: f32 = 196.0;
const PAD: f32 = 16.0;
const INSET: f32 = 4.0;
const HEADER_TOP: f32 = 16.0;
/// The header's two lines are 36 tall; the mark leaves 4 above and below.
const MARK: f32 = 28.0;
const ROW_HEIGHT: f32 = 40.0;
const ROW_TOPS: [f32; 3] = [64.0, 104.0, 152.0];
const DIVIDER: f32 = 148.0;
/// Windows 11 switch: a 40 x 20 track, knob 12 off and 14 on, 2 larger on hover.
const SWITCH_W: f32 = 40.0;
const SWITCH_H: f32 = 20.0;

const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Clone, Copy, PartialEq)]
enum Row {
    BringBack,
    Autostart,
    Quit,
}
const ORDER: [Row; 3] = [Row::BringBack, Row::Autostart, Row::Quit];
const LABELS: [&str; 3] = ["Bring T3 back when done", "Start with Windows", "Quit settled"];

static WINDOW: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
}

struct Panel {
    hwnd: HWND,
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: Option<ID2D1HwndRenderTarget>,
    dpi: f32,
    hover: Option<Row>,
    /// Keyboard focus. Shown as a ring only after a key press, as Windows does.
    focus: Option<Row>,
    hidden_at: Instant,
}

struct Palette {
    background: D2D1_COLOR_F,
    text: D2D1_COLOR_F,
    secondary: D2D1_COLOR_F,
    divider: D2D1_COLOR_F,
    hover: D2D1_COLOR_F,
    accent: D2D1_COLOR_F,
    on_accent: D2D1_COLOR_F,
    border: COLORREF,
    dark: bool,
}

/// Opens the panel above the tray icon, or closes it if it is open.
pub fn toggle(anchor: Option<RECT>) {
    PANEL.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = Panel::create().ok();
        }
        let Some(panel) = cell.as_mut() else { return };
        let visible = unsafe { IsWindowVisible(panel.hwnd) }.as_bool();
        // A click on the tray icon first deactivates the open panel, which hides it.
        // Without this check the same click would open it again.
        if visible || panel.hidden_at.elapsed() < Duration::from_millis(250) {
            panel.hide();
        } else {
            panel.show(anchor);
        }
    });
}

/// Redraws the panel if it is open. Safe to call from any thread.
pub fn refresh() {
    let hwnd = HWND(WINDOW.load(Ordering::Relaxed) as *mut _);
    if !hwnd.is_invalid() {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

impl Panel {
    fn create() -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("settled.panel");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: class,
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class, w!("settled"), WS_POPUP,
                0, 0, 0, 0, None, None, instance, None,
            )?;
            WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
            let corners = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, (&corners as *const DWM_WINDOW_CORNER_PREFERENCE).cast(), 4);
            Ok(Self {
                hwnd,
                d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
                target: None,
                dpi: 96.0,
                hover: None,
                focus: None,
                hidden_at: Instant::now() - Duration::from_secs(1),
            })
        }
    }

    fn show(&mut self, anchor: Option<RECT>) {
        unsafe {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let point = anchor.map_or(cursor, |r| POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 });
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut info);
            let (mut dpi_x, mut dpi_y) = (96, 96);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            self.dpi = dpi_x as f32;

            let px = |dips: f32| (dips * self.dpi / 96.0).round() as i32;
            let (w, h, gap) = (px(WIDTH), px(HEIGHT), px(12.0));
            let work = info.rcWork;
            // Centre on the icon and stay 12 from the taskbar and the screen edges, as
            // Windows' own flyouts do. The taskbar may sit on any edge.
            let x = if point.x >= work.right {
                work.right - gap - w
            } else if point.x < work.left {
                work.left + gap
            } else {
                (point.x - w / 2).clamp(work.left + gap, work.right - gap - w)
            };
            let y = if point.y >= work.bottom {
                work.bottom - gap - h
            } else if point.y < work.top {
                work.top + gap
            } else {
                (point.y - h / 2).clamp(work.top + gap, work.bottom - gap - h)
            };

            let palette = palette();
            let dark: BOOL = palette.dark.into();
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const BOOL).cast(), 4);
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_BORDER_COLOR, (&palette.border as *const COLORREF).cast(), 4);

            self.target = None;
            self.hover = None;
            self.focus = None;
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.hidden_at = Instant::now();
        self.target = None;
    }

    fn activate(&mut self, row: Row) {
        match row {
            Row::BringBack => tray::set_paused(!tray::paused()),
            Row::Autostart => tray::set_autostart(!tray::autostart()),
            Row::Quit => {
                self.hide();
                tray::quit();
                return;
            }
        }
        refresh();
    }

    fn row_at(&self, x: f32, y: f32) -> Option<Row> {
        if !(INSET..WIDTH - INSET).contains(&x) {
            return None;
        }
        ORDER.into_iter().zip(ROW_TOPS).find(|(_, top)| (*top..top + ROW_HEIGHT).contains(&y)).map(|(row, _)| row)
    }

    /// Rounds a DIP coordinate to the nearest physical pixel edge.
    fn snap(&self, dips: f32) -> f32 {
        let scale = self.dpi / 96.0;
        (dips * scale).round() / scale
    }

    fn paint(&mut self) -> Result<()> {
        if self.target.is_none() {
            let mut client = RECT::default();
            unsafe { GetClientRect(self.hwnd, &mut client)? };
            let target = unsafe {
                self.d2d.CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES::default(),
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd: self.hwnd,
                        pixelSize: D2D_SIZE_U { width: client.right as u32, height: client.bottom as u32 },
                        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                    },
                )?
            };
            unsafe { target.SetDpi(self.dpi, self.dpi) };
            self.target = Some(target);
        }
        let target = self.target.clone().unwrap();
        let p = palette();
        let one_px = 96.0 / self.dpi;
        unsafe {
            target.BeginDraw();
            target.Clear(Some(&p.background));
            let brush = |c: &D2D1_COLOR_F| target.CreateSolidColorBrush(c, None);
            let (text, secondary, divider, hover, accent, on_accent) =
                (brush(&p.text)?, brush(&p.secondary)?, brush(&p.divider)?, brush(&p.hover)?, brush(&p.accent)?, brush(&p.on_accent)?);
            let round = self.d2d.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )?;

            // Header: the mark, centred on the two lines of text beside it.
            let (mx, my) = (PAD + MARK / 2.0, HEADER_TOP + 18.0);
            let (r, g, b) = tray::GREY;
            let mark = if tray::paused() { brush(&rgb(r, g, b, 1.0))? } else { accent.clone() };
            target.FillEllipse(&ellipse(mx, my, MARK / 2.0), &mark);
            // On a 32 grid, the check spans 14 of the disc's 32 and sits 0.5 low, so its visual centre
            // lands on the disc's centre: the long arm rises above the short one.
            let s = MARK / 32.0;
            let check = [pt(mx - 7.0 * s, my + 0.5 * s), pt(mx - 2.0 * s, my + 5.5 * s), pt(mx + 7.0 * s, my - 4.5 * s)];
            target.DrawLine(check[0], check[1], &on_accent, 3.0 * s, &round);
            target.DrawLine(check[1], check[2], &on_accent, 3.0 * s, &round);

            let text_x = PAD + MARK + 12.0;
            let title = self.format(14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let body = self.format(14.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            let caption = self.format(12.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            draw_text(&target, "settled", &title, rect(text_x, HEADER_TOP, WIDTH - PAD, HEADER_TOP + 20.0), &text);
            let status = tray::status();
            let status = if status.is_empty() { "Connecting to T3 Code" } else { &status };
            draw_text(&target, status, &caption, rect(text_x, HEADER_TOP + 20.0, WIDTH - PAD, HEADER_TOP + 36.0), &secondary);

            // Rows.
            let states = [Some(!tray::paused()), Some(tray::autostart()), None];
            for (i, row) in ORDER.into_iter().enumerate() {
                let top = ROW_TOPS[i];
                let area = rect(INSET, top, WIDTH - INSET, top + ROW_HEIGHT);
                let hovered = self.hover == Some(row);
                if hovered {
                    target.FillRoundedRectangle(&rounded(area, 4.0), &hover);
                }
                if self.focus == Some(row) {
                    target.DrawRoundedRectangle(&rounded(inset(area, 1.0), 3.0), &text, 2.0, None);
                }
                draw_text(&target, LABELS[i], &body, rect(PAD, top, WIDTH - PAD - SWITCH_W - 12.0, top + ROW_HEIGHT), &text);
                if let Some(on) = states[i] {
                    let (x, cy) = (WIDTH - PAD - SWITCH_W, top + ROW_HEIGHT / 2.0);
                    let r = SWITCH_H / 2.0;
                    let grow = if hovered { 1.0 } else { 0.0 };
                    if on {
                        target.FillRoundedRectangle(&rounded(rect(x, cy - r, x + SWITCH_W, cy + r), r), &accent);
                        target.FillEllipse(&ellipse(x + SWITCH_W - r, cy, 7.0 + grow), &on_accent);
                    } else {
                        // The stroke runs half a pixel inside, so the outline's outer edge
                        // matches the filled track exactly.
                        let h = one_px / 2.0;
                        let track = rect(x + h, cy - r + h, x + SWITCH_W - h, cy + r - h);
                        target.DrawRoundedRectangle(&rounded(track, r - h), &secondary, one_px, None);
                        target.FillEllipse(&ellipse(x + r, cy, 6.0 + grow), &secondary);
                    }
                }
            }
            // A one-pixel hairline, snapped so it never smears across two rows of pixels.
            let y = self.snap(DIVIDER) + one_px / 2.0;
            target.DrawLine(pt(PAD, y), pt(WIDTH - PAD, y), &divider, one_px, None);

            let result = target.EndDraw(None, None);
            if result.is_err() {
                // The device was lost; build a new target on the next paint.
                self.target = None;
            }
            result
        }
    }

    fn format(&self, size: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextFormat> {
        unsafe {
            // Windows 11's UI font; DirectWrite falls back to Segoe UI on Windows 10.
            let format = self.dwrite.CreateTextFormat(
                w!("Segoe UI Variable Text"), None, weight, DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL, size, w!(""),
            )?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            Ok(format)
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = PANEL.with(|cell| {
        // Messages sent while the panel is busy (ShowWindow, SetWindowPos) go to the default.
        let Ok(mut cell) = cell.try_borrow_mut() else { return None };
        let panel = cell.as_mut()?;
        let scale = panel.dpi / 96.0;
        let mouse = || ((lparam.0 & 0xffff) as i16 as f32 / scale, ((lparam.0 >> 16) & 0xffff) as i16 as f32 / scale);
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = panel.paint();
                let _ = EndPaint(hwnd, &ps);
            }
            WM_ACTIVATE if (wparam.0 & 0xffff) as u32 == WA_INACTIVE => panel.hide(),
            WM_MOUSEMOVE => {
                let (x, y) = mouse();
                let hover = panel.row_at(x, y);
                if hover != panel.hover {
                    panel.hover = hover;
                    refresh();
                }
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
            }
            WM_MOUSELEAVE => {
                panel.hover = None;
                refresh();
            }
            WM_LBUTTONUP => {
                let (x, y) = mouse();
                if let Some(row) = panel.row_at(x, y) {
                    panel.focus = None;
                    panel.activate(row);
                }
            }
            WM_KEYDOWN => {
                let key = wparam.0 as u16;
                let index = panel.focus.and_then(|f| ORDER.iter().position(|r| *r == f));
                let step = |forward: bool| match index {
                    None if forward => 0,
                    None => ORDER.len() - 1,
                    Some(i) if forward => (i + 1) % ORDER.len(),
                    Some(i) => (i + ORDER.len() - 1) % ORDER.len(),
                };
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                if key == VK_ESCAPE.0 {
                    panel.hide();
                } else if key == VK_DOWN.0 || (key == VK_TAB.0 && !shift) {
                    panel.focus = Some(ORDER[step(true)]);
                    refresh();
                } else if key == VK_UP.0 || key == VK_TAB.0 {
                    panel.focus = Some(ORDER[step(false)]);
                    refresh();
                } else if key == VK_RETURN.0 || key == VK_SPACE.0 {
                    if let Some(row) = panel.focus {
                        panel.activate(row);
                    }
                }
            }
            WM_SETTINGCHANGE | WM_DPICHANGED => panel.hide(),
            _ => return None,
        }
        Some(LRESULT(0))
    });
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}

/// Colours follow the taskbar's light or dark setting, and Windows High Contrast.
/// The green is at least 3.6:1 against the white knob and both backgrounds.
fn palette() -> Palette {
    if let Some(p) = high_contrast() {
        return p;
    }
    let (r, g, b) = tray::GREEN;
    let mut light = 0u32;
    let mut size = 4u32;
    let found = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"), RRF_RT_REG_DWORD, None, Some((&mut light as *mut u32).cast()), Some(&mut size),
        )
        .is_ok()
    };
    if found && light == 1 {
        Palette {
            background: rgb(0xF9, 0xF9, 0xF9, 1.0),
            text: rgb(0x1A, 0x1A, 0x1A, 1.0),
            secondary: rgb(0x5D, 0x5D, 0x5D, 1.0),
            divider: rgb(0, 0, 0, 0.08),
            hover: rgb(0, 0, 0, 0.04),
            accent: rgb(r, g, b, 1.0),
            on_accent: rgb(0xFF, 0xFF, 0xFF, 1.0),
            border: COLORREF(0x00E5E5E5),
            dark: false,
        }
    } else {
        Palette {
            background: rgb(0x2C, 0x2C, 0x2C, 1.0),
            text: rgb(0xFF, 0xFF, 0xFF, 1.0),
            secondary: rgb(0xC5, 0xC5, 0xC5, 1.0),
            divider: rgb(0xFF, 0xFF, 0xFF, 0.08),
            hover: rgb(0xFF, 0xFF, 0xFF, 0.06),
            accent: rgb(r, g, b, 1.0),
            on_accent: rgb(0xFF, 0xFF, 0xFF, 1.0),
            border: COLORREF(0x00404040),
            dark: true,
        }
    }
}

fn high_contrast() -> Option<Palette> {
    let mut hc = HIGHCONTRASTW { cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    unsafe {
        SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, Some((&mut hc as *mut HIGHCONTRASTW).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
    }
    if !hc.dwFlags.contains(HCF_HIGHCONTRASTON) {
        return None;
    }
    let sys = |index| {
        let c = unsafe { GetSysColor(index) };
        rgb((c & 0xff) as u8, ((c >> 8) & 0xff) as u8, ((c >> 16) & 0xff) as u8, 1.0)
    };
    Some(Palette {
        background: sys(COLOR_WINDOW),
        text: sys(COLOR_WINDOWTEXT),
        secondary: sys(COLOR_WINDOWTEXT),
        divider: sys(COLOR_WINDOWTEXT),
        hover: sys(COLOR_HIGHLIGHT),
        accent: sys(COLOR_HIGHLIGHT),
        on_accent: sys(COLOR_HIGHLIGHTTEXT),
        border: COLORREF(unsafe { GetSysColor(COLOR_WINDOWTEXT) }),
        dark: false,
    })
}

fn rgb(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a }
}

fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

fn inset(r: D2D_RECT_F, by: f32) -> D2D_RECT_F {
    rect(r.left + by, r.top + by, r.right - by, r.bottom - by)
}

fn rounded(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect, radiusX: radius, radiusY: radius }
}

fn ellipse(x: f32, y: f32, radius: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE { point: pt(x, y), radiusX: radius, radiusY: radius }
}

unsafe fn draw_text(target: &ID2D1HwndRenderTarget, text: &str, format: &IDWriteTextFormat, area: D2D_RECT_F, brush: &ID2D1SolidColorBrush) {
    let text: Vec<u16> = text.encode_utf16().collect();
    target.DrawText(&text, format, &area, brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
}
