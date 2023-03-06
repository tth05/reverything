#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // Hide console window on Windows in release
#![feature(let_chains)]

use std::time::{Duration, Instant};

use eyre::{ContextCompat, Result};
use mimalloc::MiMalloc;

use crate::ntfs::index::NtfsVolumeIndex;
use crate::ntfs::journal::Journal;
use crate::ntfs::volume::get_volumes;

use glium::glutin::event::{Event, WindowEvent};
use glium::glutin::event_loop::{ControlFlow, EventLoop};
use glium::Surface;

mod ntfs;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Debug)]
pub struct FileInfo {
    name: String,
    parent: u64,
    is_directory: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    // Common setup for creating a winit window and imgui context, not specifc
    // to this renderer at all except that glutin is used to create the window
    // since it will give us access to a GL context
    let (event_loop, display) = create_window();
    let (mut winit_platform, mut imgui_context) = imgui_init(&display);

    // Create renderer from this crate
    let mut renderer = imgui_glium_renderer::Renderer::init(&mut imgui_context, &display)
        .expect("Failed to initialize renderer");

    // Timer for FPS calculation
    let mut last_frame = std::time::Instant::now();

    // Standard winit event loop
    event_loop.run(move |event, _, control_flow| match event {
        Event::NewEvents(_) => {
            let now = std::time::Instant::now();
            imgui_context.io_mut().update_delta_time(now - last_frame);
            last_frame = now;
        }
        Event::MainEventsCleared => {
            let gl_window = display.gl_window();
            winit_platform
                .prepare_frame(imgui_context.io_mut(), gl_window.window())
                .expect("Failed to prepare frame");
            gl_window.window().request_redraw();
        }
        Event::RedrawRequested(_) => {
            // Create frame for the all important `&imgui::Ui`
            let ui = imgui_context.frame();

            unsafe {
                imgui::sys::igSetNextWindowPos(
                    imgui::sys::ImVec2 { x: 0.0, y: 0.0 },
                    imgui::sys::ImGuiCond_Always as i32,
                    imgui::sys::ImVec2 { x: 0.0, y: 0.0 },
                );
                let display_size = (*imgui::sys::igGetIO()).DisplaySize;
                imgui::sys::igSetNextWindowSize(display_size, imgui::sys::ImGuiCond_Always as i32);
            }

            ui.window("Main")
                .collapsible(false)
                .scrollable(true)
                .movable(false)
                .resizable(false)
                .title_bar(false)
                .build(|| {
                    ui.text("Hello world!");
                    ui.text("This...is...imgui-rs!");
                    ui.separator();
                    for i in 0..100000 {
                        ui.text(format!("Line {}", i));
                    }
                });

            // Setup for drawing
            let gl_window = display.gl_window();
            let mut target = display.draw();

            // Renderer doesn't automatically clear window
            target.clear_color_srgb(1.0, 1.0, 1.0, 1.0);

            // Perform rendering
            winit_platform.prepare_render(ui, gl_window.window());
            let draw_data = imgui_context.render();
            renderer
                .render(&mut target, draw_data)
                .expect("Rendering failed");
            target.finish().expect("Failed to swap buffers");
        }
        Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } => *control_flow = ControlFlow::Exit,
        event => {
            let gl_window = display.gl_window();
            winit_platform.handle_event(imgui_context.io_mut(), gl_window.window(), &event);
        }
    });

    main_ntfs()
}

fn create_window() -> (EventLoop<()>, glium::Display) {
    let event_loop = EventLoop::new();
    let context = glium::glutin::ContextBuilder::new().with_vsync(true);
    let builder = glium::glutin::window::WindowBuilder::new()
        .with_title("Reverything")
        .with_inner_size(glium::glutin::dpi::LogicalSize::new(1024f64, 768f64));
    let display =
        glium::Display::new(builder, context, &event_loop).expect("Failed to initialize display");

    (event_loop, display)
}

fn imgui_init(display: &glium::Display) -> (imgui_winit_support::WinitPlatform, imgui::Context) {
    let mut imgui_context = imgui::Context::create();
    imgui_context.set_ini_filename(None);

    let mut winit_platform = imgui_winit_support::WinitPlatform::init(&mut imgui_context);
    winit_platform.attach_window(
        imgui_context.io_mut(),
        display.gl_window().window(),
        imgui_winit_support::HiDpiMode::Rounded,
    );

    imgui_context
        .fonts()
        .add_font(&[imgui::FontSource::DefaultFontData { config: None }]);

    // imgui_context.io_mut().font_global_scale = (1.0 / winit_platform.hidpi_factor()) as f32;

    (winit_platform, imgui_context)
}

fn main_ntfs() -> Result<()> {
    let t = Instant::now();
    let vol = get_volumes()
        .into_iter()
        .next()
        .with_context(|| "Cannot find first volume")?;

    if true {
        let mut j = Journal::new(vol)?;
        let mut i = 0;
        loop {
            let vec = j.read_entries()?;
            if vec.is_empty() {
                break;
            }
            println!("{} {:?}", i, vec);
            i += 1;
        }
        return Ok(());
    }

    let index = NtfsVolumeIndex::new(vol)?;

    let info = index.find_by_name("idea64.exe").unwrap();
    println!("{:?}", index.compute_full_path(info),);

    let mut s = 0usize;
    index.iter().for_each(|info| {
        s += index.compute_full_path(info).len();
    });

    println!("{}", s);
    println!("Elapsed: {:?}", t.elapsed());
    std::thread::sleep(Duration::from_secs(10));

    Ok(())
}
