//! Finds T3 Code's windows and brings a finished thread back to the screen.

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, ShowWindow, GW_OWNER, SW_RESTORE, SW_SHOWNORMAL,
};

/// If every T3 Code window is minimized, opens `link` in T3 Code and restores the window.
/// Does nothing when T3 Code is on screen or closed.
pub fn reveal_if_minimized(link: &str) {
    let windows = t3_windows();
    if windows.is_empty() || !windows.iter().all(|&w| unsafe { IsIconic(w) }.as_bool()) {
        return;
    }
    let link = wide(link);
    unsafe {
        // T3 Code handles its own link: it switches to the thread and restores itself.
        ShellExecuteW(HWND::default(), w!("open"), PCWSTR(link.as_ptr()), None, None, SW_SHOWNORMAL);
        // Restore directly as well, in case the link cannot reach the app.
        let _ = ShowWindow(windows[0], SW_RESTORE);
    }
}

/// Visible, titled top-level windows of a `T3 Code*.exe` process.
fn t3_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, found: LPARAM) -> BOOL {
        let found = &mut *(found.0 as *mut Vec<HWND>);
        if IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_err()
            && GetWindowTextLengthW(hwnd) > 0
            && is_t3_process(hwnd)
        {
            found.push(hwnd);
        }
        true.into()
    }
    let mut found: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

unsafe fn is_t3_process(hwnd: HWND) -> bool {
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false;
    };
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len).is_ok();
    let _ = CloseHandle(process);
    if !ok {
        return false;
    }
    let path = String::from_utf16_lossy(&path[..len as usize]).to_lowercase();
    let name = path.rsplit('\\').next().unwrap_or_default();
    name.starts_with("t3 code") && name.ends_with(".exe")
}

/// A null-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
