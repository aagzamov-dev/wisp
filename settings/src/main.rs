#![windows_subsystem = "windows"]

#[allow(dead_code)]
#[path = "../../src/logging.rs"]
mod logging;
// Shared with the launcher; this app uses only some of the names.
#[allow(dead_code)]
#[path = "../../src/channel.rs"]
mod channel;
mod config;
#[path = "../../src/remote_mode.rs"]
#[allow(dead_code)]
mod remote_mode;

mod hotkey;
#[path = "../../src/media_identity.rs"]
#[allow(dead_code)]
mod media_identity;
mod media_sources;
mod palette;
mod system;
mod ui;

use gpui_kit::{AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

fn main() {
    if let Err(error) = logging::init("settings") {
        #[cfg(debug_assertions)]
        std::eprintln!("Wisp Settings file diagnostics unavailable: {error}");
        #[cfg(not(debug_assertions))]
        let _ = error;
    }
    logging::event("settings.start", format_args!("ui_thread=true"));
    let args: Vec<String> = std::env::args().collect();
    let welcome = args.iter().any(|a| a == "--welcome");
    // `--page search` opens a page directly, for screenshots and testing.
    let page = args
        .iter()
        .position(|a| a == "--page")
        .and_then(|i| args.get(i + 1))
        .cloned();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            logging::event("settings.runtime.ready", format_args!("gpui=true"));
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let (title, width, height) = if welcome {
                ("Welcome to Wisp", 560., 460.)
            } else {
                ("Wisp Settings", 820., 580.)
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(width), px(height)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(520.), px(420.))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| ui::Settings::new(welcome, page.as_deref(), window, cx))
            })
            .expect("failed to open Wisp Settings");
            logging::event("settings.window.ready", format_args!("onboarding={welcome}"));
            cx.activate(true);
        });
    logging::finish("clean-exit");
}
