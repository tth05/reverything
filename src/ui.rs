use glium::glutin::event::{Event, WindowEvent};
use glium::glutin::event_loop::{ControlFlow, EventLoop};
use glium::Surface;
use imgui::{InputTextCallback, InputTextCallbackHandler, TextCallbackData};
use crate::ntfs::index::NtfsVolumeIndex;

pub struct ReverythingUI {
    search_text: String,
    index: NtfsVolumeIndex,
    results: Vec<String>
}

impl ReverythingUI {
    fn new(index: NtfsVolumeIndex) -> Self {
        Self {
            search_text: String::new(),
            index,
            results: Vec::new()
        }
    }

    fn render(&mut self, ui: &imgui::Ui) {
        ui.text("Reverything");

        ui.text("Search: ");
        ui.same_line();
        let mut did_edit = false;
        ui.input_text(" ", &mut self.search_text).auto_select_all(true).callback(
            InputTextCallback::EDIT,
            SearchCallbackHandler::new(&mut did_edit),
        ).build();

        if did_edit && self.search_text.len() > 3 {
            self.results.clear();
            self.index.iter().filter(|f| f.name.contains(&self.search_text)).for_each(|f| {
                self.results.push(self.index.compute_full_path(f));
            });
        }

        ui.separator();

        for result in &self.results {
            ui.text(result);
        }
    }
}

struct SearchCallbackHandler<'a> {
    did_edit: &'a mut bool,
}

impl<'a> SearchCallbackHandler<'a> {
    fn new(did_edit: &'a mut bool) -> Self {
        Self { did_edit }
    }
}

impl<'a> InputTextCallbackHandler for SearchCallbackHandler<'a> {
    fn on_edit(&mut self, _: TextCallbackData) {
        *self.did_edit = true;
    }
}

pub fn start_ui(index: NtfsVolumeIndex) {
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

    // unsafe { imgui::sys::ImGuiStyle_ScaleAllSizes(imgui::sys::igGetStyle(), 2.5); }
    imgui_context.style_mut().window_rounding = 0.0;

    let mut reverything = ReverythingUI::new(index);

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
            let mut ui = imgui_context.frame();

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
                    reverything.render(ui);
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
        imgui_winit_support::HiDpiMode::Locked(1.0),
    );

    imgui_context
        .fonts()
        .add_font(&[imgui::FontSource::DefaultFontData {
            config: Some(imgui::FontConfig {
                size_pixels: 26.0,
                ..Default::default()
            }),
        }]);

    // imgui_context.io_mut().font_global_scale = 1.5 as f32;

    (winit_platform, imgui_context)
}
