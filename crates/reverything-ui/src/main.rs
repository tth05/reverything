//! The Reverything search window. It talks to the Reverything service over a named pipe and
//! runs as the logged on user.
//!
//! `--background` starts hidden in the tray, which is how it is started with Windows.

#![windows_subsystem = "windows"]

use std::time::Instant;

use gpui_kit::*;

mod client;
mod desktop;
mod drives;
mod format;
mod icons;
mod log;
mod results;
mod selection;
mod settings;
mod shell;
mod update;
mod view;

// Icons beyond the ones the components bundle
gpui_kit::assets::icon_assets!(ExtraIcons, [CircleQuestionMark]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn main() {
    // The window has no console, without this a panic would close it without a trace
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::write(&format!("Panic: {}\n{}", info, backtrace));
    }));
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
    application().with_assets(AppAssets).run(move |cx| {
        stage("run");
        init(cx);
        stage("init");
        // The app keeps running in the tray without a window
        cx.set_quit_mode(QuitMode::Explicit);
        cx.set_global(settings::Settings::load());
        view::apply_theme(None, cx);
        cx.bind_keys(view::key_bindings());

        // Started with Windows, the window is only opened when it is first needed
        let window = (!background).then(|| open_main_window(started, cx));
        stage("window opened");

        desktop::Desktop::install(window, reopen_main_window, second_instance, cx);
        stage("tray and hotkey");
    });
}

/// Where the window was last time, if that is still on a connected display.
fn saved_bounds(cx: &App) -> Option<(Bounds<Pixels>, bool)> {
    let p = cx.global::<settings::Settings>().window?;
    let bounds = Bounds::new(
        point(px(p.x), px(p.y)),
        size(px(p.width.max(640.)), px(p.height.max(360.))),
    );
    cx.displays()
        .iter()
        .any(|display| display.bounds().intersects(&bounds))
        .then_some((bounds, p.maximized))
}

/// Opens the search window, shown and focused.
fn open_main_window(started: Instant, cx: &mut App) -> AnyWindowHandle {
    let (bounds, maximized) = saved_bounds(cx)
        .unwrap_or_else(|| (Bounds::centered(None, size(px(1280.), px(780.)), cx), false));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Shown below, after moving it to the monitor with the mouse
        show: false,
        focus: false,
        // The window draws its own title bar, see `view::MainView::render_title_bar`
        titlebar: Some(TitlebarOptions {
            title: Some("Reverything".into()),
            ..gpui_kit::component::TitleBar::title_bar_options()
        }),
        app_owns_titlebar_drag: true,
        window_min_size: Some(size(px(640.), px(360.))),
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
                desktop::hide(window, cx);
                false
            } else {
                desktop::remember_bounds(window, cx);
                cx.quit();
                true
            }
        });
        view
    })
    .expect("Failed to open the window");
    let _ = window.update(cx, |_, window, cx| desktop::present(window, cx, maximized));
    window
}

/// Opens the window again after it was closed while hidden.
fn reopen_main_window(cx: &mut App) -> AnyWindowHandle {
    open_main_window(Instant::now(), cx)
}
