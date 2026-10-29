//! Finding the Teams window and grabbing its pixels with GDI.
//!
//! Only the Teams window is captured by default (new Teams `ms-teams.exe`,
//! classic `Teams.exe`, or a browser tab titled "Microsoft Teams"), so other
//! windows on the screen never end up in the notes.

use image::RgbImage;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetSystemMetrics, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, SM_CXSCREEN, SM_CYSCREEN,
};

/// PW_RENDERFULLCONTENT: needed for Chromium/WebView2 windows such as new Teams.
const PW_RENDERFULLCONTENT: u32 = 2;

#[derive(Debug, Clone)]
pub struct TeamsWindow {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
    /// The title looks like a call or meeting rather than chat/calendar.
    pub in_meeting: bool,
    pub area: i64,
}

/// Window rectangles in physical pixels; call once at start-up.
pub fn init_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

const MEETING_WORDS: [&str; 10] = [
    "meeting", "call", "σύσκεψη", "κλήση", "besprechung", "réunion", "reunión", "riunione", "anruf", "appel",
];

fn exe_name(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or("").to_lowercase()
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<TeamsWindow>);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buf[..n as usize]);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let exe = exe_name(pid);
    let native = exe == "ms-teams.exe" || exe == "teams.exe";
    let lower = title.to_lowercase();
    if !native && !lower.contains("microsoft teams") {
        return BOOL(1);
    }
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    let area = ((r.right - r.left) as i64) * ((r.bottom - r.top) as i64);
    if area < 200 * 150 {
        return BOOL(1);
    }
    let in_meeting = MEETING_WORDS.iter().any(|w| lower.contains(w));
    list.push(TeamsWindow { hwnd: hwnd.0 as isize, title, exe, in_meeting, area });
    BOOL(1)
}

/// The best Teams window: a meeting window first, then the largest one.
pub fn find_teams() -> Option<TeamsWindow> {
    let mut list: Vec<TeamsWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize));
    }
    list.into_iter().max_by_key(|w| (w.in_meeting, w.area))
}

fn bgra_to_rgb(bgra: &[u8], w: u32, h: u32) -> RgbImage {
    let mut img = RgbImage::new(w, h);
    for (px, src) in img.pixels_mut().zip(bgra.chunks_exact(4)) {
        *px = image::Rgb([src[2], src[1], src[0]]);
    }
    img
}

/// Copies a device context region into an RGB image, using `fill` to draw into
/// the memory bitmap.
unsafe fn grab(w: i32, h: i32, fill: impl FnOnce(windows::Win32::Graphics::Gdi::HDC) -> bool) -> Option<RgbImage> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(mem, bmp);
    let ok = fill(mem);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(mem, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    if !ok || lines == 0 {
        return None;
    }
    Some(bgra_to_rgb(&buf, w as u32, h as u32))
}

/// The Teams window's content, even when other windows cover it.
pub fn capture_window(hwnd: isize) -> Option<RgbImage> {
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        if IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        let img = grab(r.right - r.left, r.bottom - r.top, |dc| {
            PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool()
        })?;
        // A window that cannot be rendered off-screen comes back black.
        let lit = img.as_raw().iter().step_by(97).any(|&b| b > 8);
        lit.then_some(img)
    }
}

/// The primary monitor.
pub fn capture_screen() -> Option<RgbImage> {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        grab(w, h, |dc| {
            let screen = GetDC(None);
            let ok = BitBlt(dc, 0, 0, w, h, screen, 0, 0, SRCCOPY).is_ok();
            ReleaseDC(None, screen);
            ok
        })
    }
}
