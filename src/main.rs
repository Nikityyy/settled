#![windows_subsystem = "windows"]

mod panel;
mod t3;
mod tray;
mod window;

use t3::Event;
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

fn main() {
    unsafe {
        // One copy per user session; a second launch exits at once.
        let _instance = CreateMutexW(None, true, w!("Local\\settled"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    tray::init_autostart();
    std::thread::spawn(|| {
        t3::Watcher::new().run(|event| match event {
            Event::Offline => tray::set_status("T3 Code is not running"),
            Event::Watching { working: 0 } => tray::set_status("Nothing running"),
            Event::Watching { working: 1 } => tray::set_status("1 thread working"),
            Event::Watching { working } => tray::set_status(&format!("{working} threads working")),
            Event::Finished { link } => {
                if !tray::paused() {
                    window::reveal_if_minimized(link);
                }
            }
        })
    });
    tray::run();
}
