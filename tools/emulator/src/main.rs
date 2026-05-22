mod panel;
mod render;
mod scenario;

use app_core::{Button, InputEvent, LibraryEvent};
#[cfg(feature = "gui")]
use display::{HEIGHT, WIDTH};
#[cfg(feature = "gui")]
use eframe::egui;
use panel::PanelModel;
use render::write_png;
#[cfg(feature = "gui")]
use render::framebuffer_to_color_image;
use scenario::Scenario;
use std::env;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;
    if args.gui {
        run_gui(args)?;
    } else {
        run_headless(args)?;
    }
    Ok(())
}

struct Args {
    scenario: Option<PathBuf>,
    sd_root: Option<PathBuf>,
    dump: Option<PathBuf>,
    check: Option<PathBuf>,
    gui: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut scenario = None;
        let mut sd_root = None;
        let mut dump = None;
        let mut check = None;
        let mut gui = false;
        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--scenario" => {
                    scenario = Some(PathBuf::from(iter.next().ok_or("--scenario needs a path")?))
                }
                "--sd-root" => {
                    sd_root = Some(PathBuf::from(iter.next().ok_or("--sd-root needs a path")?))
                }
                "--dump" => dump = Some(PathBuf::from(iter.next().ok_or("--dump needs a path")?)),
                "--check" => {
                    check = Some(PathBuf::from(iter.next().ok_or("--check needs a path")?))
                }
                "--gui" => gui = true,
                "--help" | "-h" => {
                    return Err(
                        "usage: x4-emulator [--gui] [--scenario PATH] [--sd-root PATH] [--dump out.png] [--check golden.png]"
                            .into(),
                    );
                }
                value => return Err(format!("unknown option: {value}")),
            }
        }
        Ok(Self {
            scenario,
            sd_root,
            dump,
            check,
            gui,
        })
    }
}

fn run_headless(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let scenarios = scenario_paths(args.scenario.as_deref())?;
    if scenarios.is_empty() {
        let emu = Emulator::boot(args.sd_root);
        if let Some(path) = args.dump {
            write_png(&path, emu.framebuffer())?;
        }
        if let Some(path) = args.check {
            compare_png(&path, emu.framebuffer())?;
        }
        return Ok(());
    }

    for path in scenarios {
        let mut emu = Emulator::boot(args.sd_root.clone());
        let scenario = Scenario::load(&path)?;
        scenario
            .run(&mut emu)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        scenario
            .assert(&emu)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        if let Some(dump) = &args.dump {
            let path = output_path(dump, &path)?;
            write_png(&path, emu.framebuffer())?;
            println!("dumped {}", path.display());
        }
        if let Some(check) = &args.check {
            let path = output_path(check, &path)?;
            compare_png(&path, emu.framebuffer())?;
            println!("matched {}", path.display());
        }
        println!("ok {}", path.display());
    }
    Ok(())
}

fn scenario_paths(path: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if path.is_file() {
        return Ok(vec![path.to_owned()]);
    }
    if !path.is_dir() {
        return Err(format!("scenario path does not exist: {}", path.display()));
    }
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(path).map_err(|err| format!("{}: {err}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn output_path(base: &Path, scenario: &Path) -> Result<PathBuf, String> {
    if base.extension().is_some_and(|ext| ext == "png") {
        return Ok(base.to_owned());
    }
    let name = scenario
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("scenario has no valid file stem: {}", scenario.display()))?;
    Ok(base.join(format!("{name}.png")))
}

fn compare_png(path: &Path, fb: &display::fb::Framebuffer) -> Result<(), String> {
    let mut actual = Vec::new();
    render::encode_png(&mut actual, fb).map_err(|err| err.to_string())?;
    let expected = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!("frame does not match {}", path.display()))
    }
}

fn run_gui(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(feature = "gui"))]
    {
        let _ = args;
        Err("--gui requires building with --features gui".into())
    }
    #[cfg(feature = "gui")]
    {
        let native_options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1100.0, 720.0])
                .with_min_inner_size([900.0, 560.0]),
            ..Default::default()
        };
        let sd_root = args.sd_root.clone();
        eframe::run_native(
            "Xteink X4 Emulator",
            native_options,
            Box::new(move |_cc| Ok(Box::new(EmulatorApp::new(sd_root)))),
        )?;
        Ok(())
    }
}

pub struct Emulator {
    state: app_core::ReaderState,
    ctx: app_core::ReducerContext,
    panel: PanelModel,
    fb: display::fb::Framebuffer,
    screen_on: bool,
    fast_refreshes: u8,
    sleeping: bool,
    sd_root: Option<PathBuf>,
    library_entries: Vec<String>,
    last_view: Option<app_core::AppView>,
    last_book_id: Option<u32>,
}

impl Emulator {
    pub fn boot(sd_root: Option<PathBuf>) -> Self {
        let mut emu = Self {
            state: app_core::ReaderState::boot(),
            ctx: app_core::ReducerContext::new(1, 4),
            panel: PanelModel::new(),
            fb: display::fb::Framebuffer::new(),
            screen_on: false,
            fast_refreshes: 0,
            sleeping: false,
            sd_root,
            library_entries: Vec::new(),
            last_view: None,
            last_book_id: None,
        };
        emu.panel.init_sequence().expect("panel init");
        emu.render(app_core::RenderKind::Boot);
        emu
    }

    pub fn input(&mut self, button: Button) {
        if button == Button::Power {
            if self.sleeping {
                self.sleeping = false;
                self.panel.init_sequence().expect("panel wake init");
                self.state.view = app_core::AppView::Home;
                self.render(app_core::RenderKind::Page);
            } else {
                self.sleeping = true;
                self.sleep_panel();
            }
            return;
        }
        if self.sleeping {
            return;
        }
        self.state = self.state.apply_input(self.ctx, InputEvent::button(button));
        self.maybe_autoscan();
        self.render(app_core::RenderKind::Page);
    }

    pub fn library_event(&mut self, event: LibraryEvent) {
        self.state = self.state.apply_library_event(self.ctx, event);
        self.render(app_core::RenderKind::Page);
    }

    pub fn state(&self) -> app_core::ReaderState {
        self.state
    }

    pub fn panel(&self) -> &PanelModel {
        &self.panel
    }

    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    pub fn framebuffer(&self) -> &display::fb::Framebuffer {
        &self.fb
    }

    fn maybe_autoscan(&mut self) {
        if self.state.view != app_core::AppView::Library || self.state.library_count != 0 {
            return;
        }
        let count = self.scan_sd_root();
        self.state = self
            .state
            .apply_library_event(self.ctx, LibraryEvent::Scanned { count });
    }

    fn scan_sd_root(&mut self) -> u8 {
        self.library_entries.clear();
        let Some(root) = &self.sd_root else {
            return 0;
        };
        for dir in [root.join("books"), root.clone()] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("epub"))
                {
                    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                        self.library_entries.push(name.to_owned());
                    }
                }
                if self.library_entries.len() >= 8 {
                    break;
                }
            }
        }
        self.library_entries.len().min(u8::MAX as usize) as u8
    }

    fn render(&mut self, kind: app_core::RenderKind) {
        let request = self.state.render_request(kind);
        crate::render::render_request(&mut self.fb, request, &self.library_entries);
        let mode = self.refresh_mode(request);
        self.panel
            .write_framebuffer_bw(&self.fb)
            .expect("panel framebuffer write");
        self.panel.refresh(mode).expect("panel refresh");
        self.screen_on = true;
        self.last_view = Some(request.view);
        self.last_book_id = Some(request.book_id);
        if mode == display::epd::RefreshMode::Fast {
            self.fast_refreshes = self.fast_refreshes.saturating_add(1);
        } else {
            self.fast_refreshes = 0;
        }
    }

    fn sleep_panel(&mut self) {
        crate::render::render_sleep(&mut self.fb, self.state.render_request(app_core::RenderKind::Page));
        self.panel
            .write_framebuffer_bw(&self.fb)
            .expect("panel sleep framebuffer write");
        self.panel
            .refresh(display::epd::RefreshMode::Full)
            .expect("panel sleep refresh");
        self.panel.deep_sleep().expect("panel deep sleep");
        self.screen_on = false;
        self.fast_refreshes = 0;
        self.last_view = None;
        self.last_book_id = None;
    }

    fn refresh_mode(&self, request: app_core::RenderRequest) -> display::epd::RefreshMode {
        if !self.screen_on
            || self.last_view != Some(request.view)
            || self.last_book_id != Some(request.book_id)
        {
            return display::epd::RefreshMode::Full;
        }
        match request.refresh_policy {
            app_core::RefreshPolicy::FastOnly | app_core::RefreshPolicy::FullOnWake => {
                display::epd::RefreshMode::Fast
            }
            app_core::RefreshPolicy::FullEveryTen if self.fast_refreshes >= 8 => {
                display::epd::RefreshMode::Full
            }
            app_core::RefreshPolicy::FullEveryTen => display::epd::RefreshMode::Fast,
        }
    }
}

#[cfg(feature = "gui")]
struct EmulatorApp {
    emulator: Emulator,
    texture: Option<egui::TextureHandle>,
}

#[cfg(feature = "gui")]
impl EmulatorApp {
    fn new(sd_root: Option<PathBuf>) -> Self {
        Self {
            emulator: Emulator::boot(sd_root),
            texture: None,
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let bindings = [
            (egui::Key::Q, Button::Power),
            (egui::Key::Escape, Button::Back),
            (egui::Key::Enter, Button::Confirm),
            (egui::Key::ArrowLeft, Button::Previous),
            (egui::Key::ArrowRight, Button::Next),
        ];
        for (key, button) in bindings {
            if ctx.input(|input| input.key_pressed(key)) {
                self.emulator.input(button);
            }
        }
    }
}

#[cfg(feature = "gui")]
impl eframe::App for EmulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_keys(ctx);
        let image = framebuffer_to_color_image(self.emulator.framebuffer());
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::NEAREST),
            None => {
                self.texture =
                    Some(ctx.load_texture("x4-framebuffer", image, egui::TextureOptions::NEAREST));
            }
        }

        egui::SidePanel::right("state").show(ctx, |ui| {
            let state = self.emulator.state();
            ui.heading("X4 Emulator");
            ui.label(format!("View: {:?}", state.view));
            ui.label(format!("Book: {}", state.book_id));
            ui.label(format!("Chapter: {}", state.chapter));
            ui.label(format!("Page: {}", state.page));
            ui.label(format!("Selection: {}", state.selection));
            ui.label(format!("Battery: {}%", state.battery_percent));
            ui.label(format!("Refresh: {:?}", self.emulator.panel().last_refresh()));
            ui.separator();
            ui.label("Keys");
            ui.label("Q Power");
            ui.label("Esc Back");
            ui.label("Enter Confirm");
            ui.label("Left Previous");
            ui.label("Right Next");
            ui.horizontal(|ui| {
                if ui.button("Back").clicked() {
                    self.emulator.input(Button::Back);
                }
                if ui.button("OK").clicked() {
                    self.emulator.input(Button::Confirm);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Prev").clicked() {
                    self.emulator.input(Button::Previous);
                }
                if ui.button("Next").clicked() {
                    self.emulator.input(Button::Next);
                }
            });
            if ui.button("Power").clicked() {
                self.emulator.input(Button::Power);
            }
            ui.separator();
            ui.label("Panel history");
            for entry in self.emulator.panel().history().iter().rev().take(12).rev() {
                ui.label(entry);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.texture {
                let available = ui.available_size();
                let scale = (available.x / WIDTH as f32)
                    .min(available.y / HEIGHT as f32)
                    .max(1.0);
                let size = egui::Vec2::new(WIDTH as f32 * scale, HEIGHT as f32 * scale);
                ui.image((texture.id(), size));
            }
        });
    }
}
