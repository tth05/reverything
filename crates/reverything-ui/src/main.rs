//! The Reverything search window. It talks to the Reverything service over a named pipe and
//! runs as the logged on user.
//!
//! `--background` starts hidden in the tray, which is how it is started with Windows.

#![windows_subsystem = "windows"]

use std::time::Instant;

use gpui_kit::*;

mod client;
mod desktop;
mod format;
mod icons;
mod log;
mod results;
mod settings;
mod shell;
mod view;

fn main() {
    let started = Instant::now();
    // A second start just brings the running window to the front
    let Some(second_instance) = desktop::single_instance() else {
        return;
    };
    let background = std::env::args().any(|a| a == "--background");

    // REVERYTHING_LOG_STARTUP=1 writes the startup stages to the UI log
    let stage = move |name: &str| {
        if std::env::var_os("REVERYTHING_LOG_STARTUP").is_some() {
            log::write(&format!("{} after {:?}", name, started.elapsed()));
        }
    };
    stage("main");
    application().with_assets(assets::Assets).run(move |cx| {
        stage("run");
        init(cx);
        stage("init");
        cx.set_global(settings::Settings::load());
        view::apply_theme(None, cx);
        cx.bind_keys(view::key_bindings());

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1280.), px(780.)),
                cx,
            ))),
            // The window draws its own title bar, see `view::MainView::render_title_bar`
            titlebar: Some(TitlebarOptions {
                title: Some("Reverything".into()),
                ..gpui_kit::component::TitleBar::title_bar_options()
            }),
            app_owns_titlebar_drag: true,
            window_min_size: Some(size(px(640.), px(360.))),
            show: !background,
            focus: !background,
            ..Default::default()
        };

        let (window, _) = open_window(options, cx, |window, cx| {
            let view = cx.new(|cx| view::MainView::new(started, window, cx));

            // Follow the Windows light/dark setting
            window
                .observe_window_appearance(|window, cx| {
                    if cx.global::<settings::Settings>().theme == settings::ThemeChoice::System {
                        view::apply_theme(Some(window), cx);
                    }
                })
                .detach();

            // Closing hides the window to the tray unless that is turned off
            window.on_window_should_close(cx, |window, cx| {
                if cx.global::<settings::Settings>().close_to_tray {
                    desktop::hide(window);
                    false
                } else {
                    cx.quit();
                    true
                }
            });
            view
        })
        .expect("Failed to open the window");
        stage("window opened");

        desktop::Desktop::install(window, second_instance, cx);
        stage("tray and hotkey");
    });
}
