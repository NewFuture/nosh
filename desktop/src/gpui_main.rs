#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gpui_view;

use clap::Parser;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_view::{Desktop, Options};

fn main() {
    let options = Options::parse();
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(800.), px(540.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("nosh - GPUI desktop experiment".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Desktop::new(options, window, cx)),
        ) {
            Ok(_) => cx.activate(true),
            Err(error) => {
                eprintln!("nosh-gpui: cannot open native window: {error:#}");
                cx.quit();
            }
        }
    });
}
