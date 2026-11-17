//! Nera Cleaner — desktop cleaner for macOS and Windows built with GPUI
//! (gpui-kit).

// Release builds on Windows are GUI programs: no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod screens;

use gpui_kit::*;

fn main() {
    gpui_kit::platform::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx: &mut App| {
            gpui_kit::component::init(cx);
            nera_ui::init(cx);

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.), px(640.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Nera Cleaner".into()),
                    appears_transparent: app::CUSTOM_TITLEBAR,
                    // Centred on the 64 px header, level with the app name.
                    traffic_light_position: Some(point(px(14.), px(26.))),
                }),
                app_id: Some(nera_core::paths::BUNDLE_ID.into()),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                nera_ui::state::apply_theme(Some(window), cx);
                cx.new(|cx| app::NeraApp::new(window, cx))
            })
            .expect("failed to open window");

            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
