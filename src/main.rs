// Keep a console for debug logs. Release builds run without a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod logging;
// Legacy diagnostics keep their call sites and enter the bounded file sink.
// Performance/capture-only output explicitly uses std::eprintln instead.
macro_rules! eprintln {
    ($($argument:tt)*) => { $crate::logging::diagnostic(module_path!(), format_args!($($argument)*)) };
}

mod animation;
mod app;
mod calc;
mod channel;
mod commands;
mod config;
mod remote_mode;
mod remote_session;

mod dock;
mod drag;
mod editor;
mod files;
mod history;
mod hotkey;
mod indexer;
mod launch;
mod morph;
mod notch;
mod render;
mod search;
mod surface;
mod system;
mod taskbar;
mod theme;
mod tray;
mod tray_host;
mod util;
mod worker;

fn main() {
    let watchdog = std::env::args_os().any(|argument| argument == "--taskbar-watchdog");
    if let Err(error) = logging::init(if watchdog { "watchdog" } else { "launcher" }) {
        #[cfg(debug_assertions)]
        std::eprintln!("Wisp file diagnostics unavailable: {error}");
        #[cfg(not(debug_assertions))]
        let _ = error;
    }
    if taskbar::dispatch_watchdog() {
        logging::finish("watchdog-returned");
        return;
    }
    if let Err(e) = app::run() {
        logging::error(
            "app.run",
            format_args!("code={:#010X} message={}", e.code().0, e.message()),
        );
        util::error_box(&format!("Wisp could not start:\n{}", e.message()));
        logging::finish("entry-error");
    } else {
        logging::finish("clean-exit");
    }
}
