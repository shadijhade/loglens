//! LogLens: a desktop viewer and analyzer for Serilog text logs.

#![windows_subsystem = "windows"]

mod app;
mod logs;

use std::path::PathBuf;

use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;

fn main() {
    // `loglens.exe <folder or files…>` opens them straight away.
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1500.), px(940.)), cx)),
            window_min_size: Some(size(px(960.), px(600.))),
            titlebar: Some(TitlebarOptions { title: Some("LogLens".into()), ..TitleBar::title_bar_options() }),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| app::LogLens::new(paths, window, cx)))
            .expect("failed to open window");
        cx.activate(true);
    });
}
