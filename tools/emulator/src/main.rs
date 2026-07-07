#[cfg(not(feature = "device-x3"))]
mod panel;
#[cfg(feature = "device-x3")]
#[path = "panel_uc8253.rs"]
mod panel;
mod render;
mod scenario;

use app_core::{
    AppView, Button, InputEvent, LibraryEvent, ReaderSource, RefreshPlanner, StorageCommand,
};
#[cfg(feature = "gui")]
use display::{HEIGHT, WIDTH};
#[cfg(feature = "gui")]
use eframe::egui;
use panel::PanelModel;
#[cfg(feature = "gui")]
use render::framebuffer_to_color_image;
use render::{write_png, write_presented_png};
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
    present_dump: Option<PathBuf>,
    check: Option<PathBuf>,
    gui: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut scenario = None;
        let mut sd_root = None;
        let mut dump = None;
        let mut present_dump = None;
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
                "--present-dump" => {
                    present_dump = Some(PathBuf::from(
                        iter.next().ok_or("--present-dump needs a path")?,
                    ))
                }
                "--check" => {
                    check = Some(PathBuf::from(iter.next().ok_or("--check needs a path")?))
                }
                "--gui" => gui = true,
                "--help" | "-h" => {
                    return Err(
                        "usage: x4-emulator [--gui] [--scenario PATH] [--sd-root PATH] [--dump out.png] [--present-dump out.png] [--check golden.png]"
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
            present_dump,
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
        if let Some(path) = args.present_dump {
            write_presented_png(&path, emu.framebuffer())?;
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
        if let Some(dump) = &args.present_dump {
            let path = output_path(dump, &path)?;
            write_presented_png(&path, emu.framebuffer())?;
            println!("presented {}", path.display());
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
    refresh_planner: RefreshPlanner,
    panel: PanelModel,
    fb: display::fb::Framebuffer,
    prev_fb: display::fb::Framebuffer,
    prev_prestaged: bool,
    sleeping: bool,
    _sd_root: Option<PathBuf>,
    library_entries: Vec<String>,
    last_storage: Option<StorageCommand>,
    sd_reader_status: EmulatedReaderStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmulatedReaderStatus {
    Empty,
    Loading,
    Ready,
}

impl Emulator {
    pub fn boot(sd_root: Option<PathBuf>) -> Self {
        let mut emu = Self {
            state: app_core::ReaderState::boot(),
            ctx: app_core::ReducerContext::new(1, 4),
            refresh_planner: RefreshPlanner::new(),
            panel: PanelModel::new(),
            fb: display::fb::Framebuffer::new(),
            prev_fb: display::fb::Framebuffer::new(),
            prev_prestaged: false,
            sleeping: false,
            _sd_root: sd_root,
            library_entries: Vec::new(),
            last_storage: None,
            sd_reader_status: EmulatedReaderStatus::Empty,
        };
        emu.panel.init_sequence().expect("panel init");
        // A saved network, like the firmware's boot probe of WIFI.BIN:
        // scenarios must be able to walk the whole connect flow. The
        // no-network screen stays reachable through the forget flow (or
        // pinned by app-core unit tests).
        emu.state = emu
            .state
            .apply_sync_event(app_core::SyncEvent::NetworkSaved(
                app_core::WifiSsid::new("HOME-WIFI").unwrap(),
            ));
        emu.render(app_core::RenderKind::Boot);
        emu
    }

    pub fn input(&mut self, button: Button) {
        if button == Button::Power {
            if self.sleeping {
                self.sleeping = false;
                self.panel.init_sequence().expect("panel wake init");
                self.prev_prestaged = false;
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
        let previous = self.state;
        self.state = self.state.apply_input(self.ctx, InputEvent::button(button));
        self.render(app_core::RenderKind::Page);
        if let Some(command) = storage_command_for_transition(previous, self.state) {
            if matches!(command, StorageCommand::OpenBook { .. }) {
                self.sd_reader_status = EmulatedReaderStatus::Loading;
                self.render(app_core::RenderKind::Page);
            }
            self.last_storage = Some(command);
        }
    }

    pub fn library_event(&mut self, event: LibraryEvent) {
        if let LibraryEvent::Scanned { count } = event {
            self.library_entries.clear();
            self.library_entries
                .extend((0..count).map(|index| format!("SD Book {}", index + 1)));
        }
        if matches!(event, LibraryEvent::Loaded { .. }) {
            self.sd_reader_status = EmulatedReaderStatus::Ready;
        }
        self.state = self.state.apply_library_event(self.ctx, event);
        self.render(app_core::RenderKind::Page);
    }

    pub fn sync_event(&mut self, event: app_core::SyncEvent) {
        self.state = self.state.apply_sync_event(event);
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

    pub fn pending_storage_name(&self) -> Option<&'static str> {
        match self.last_storage {
            Some(StorageCommand::LoadCatalogCache) => Some("LoadCatalogCache"),
            Some(StorageCommand::RefreshCatalog) => Some("RefreshCatalog"),
            Some(StorageCommand::OpenBook { .. }) => Some("OpenBook"),
            Some(StorageCommand::ExtendSection { .. }) => Some("ExtendSection"),
            Some(StorageCommand::StoreProgress(_)) => Some("StoreProgress"),
            Some(StorageCommand::LoanSyncMemory) => Some("LoanSyncMemory"),
            Some(StorageCommand::StoreWifiCredentials(_)) => Some("StoreWifiCredentials"),
            Some(StorageCommand::ForgetWifiCredentials) => Some("ForgetWifiCredentials"),
            Some(StorageCommand::ReceiveUpload) => Some("ReceiveUpload"),
            Some(StorageCommand::LoadChapters { .. }) => Some("LoadChapters"),
            Some(StorageCommand::JumpChapter { .. }) => Some("JumpChapter"),
            None => None,
        }
    }

    pub fn reader_status_name(&self) -> &'static str {
        match self.sd_reader_status {
            EmulatedReaderStatus::Empty => "Empty",
            EmulatedReaderStatus::Loading => "Loading",
            EmulatedReaderStatus::Ready => "Ready",
        }
    }

    fn render(&mut self, kind: app_core::RenderKind) {
        let request = self.state.render_request(kind);
        if request.view == AppView::Reading
            && ReaderSource::from_book_id(request.book_id).is_sd()
            && self.sd_reader_status != EmulatedReaderStatus::Ready
        {
            self.fb.clear(true);
            display::render::draw_ascii(&mut self.fb, "OPENING EPUB", 20, 72, false);
        } else {
            crate::render::render_request(&mut self.fb, request, &self.library_entries);
        }
        let mode = self.refresh_planner.mode_for(request);
        let effective_mode = self
            .panel
            .flush(&self.fb, &self.prev_fb, mode, self.prev_prestaged)
            .expect("panel flush");
        self.refresh_planner.record_render(request, mode);
        self.prev_fb.copy_from(&self.fb);
        // A Full flush already writes the old/RED plane with the current
        // frame, so staging it again here would just repeat that write.
        self.prev_prestaged = effective_mode == display::epd::RefreshMode::Full
            || self.panel.prestage_previous(&self.fb).is_ok();
    }

    fn sleep_panel(&mut self) {
        crate::render::render_sleep(
            &mut self.fb,
            self.state.render_request(app_core::RenderKind::Page),
            &self.library_entries,
        );
        self.panel
            .flush(
                &self.fb,
                &self.prev_fb,
                display::epd::RefreshMode::Full,
                self.prev_prestaged,
            )
            .expect("panel sleep flush");
        self.prev_fb.copy_from(&self.fb);
        self.prev_prestaged = false;
        self.panel.deep_sleep().expect("panel deep sleep");
        self.refresh_planner.record_sleep();
    }
}

fn storage_command_for_transition(
    previous: app_core::ReaderState,
    next: app_core::ReaderState,
) -> Option<StorageCommand> {
    let index = ReaderSource::from_book_id(next.book_id).sd_index()?;
    if next.view != AppView::Reading {
        return None;
    }

    if previous.book_id != next.book_id
        || previous.chapter != next.chapter
        || previous.view != AppView::Reading
    {
        return Some(StorageCommand::OpenBook {
            request_id: 0,
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: 5,
            type_settings: next.type_settings(),
        });
    }

    if next.page.saturating_add(2) >= next.sd_page_count {
        return Some(StorageCommand::ExtendSection {
            request_id: 0,
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: next.page.saturating_add(5).min(u16::MAX as u32) as u16,
            type_settings: next.type_settings(),
        });
    }

    if previous.page != next.page {
        return Some(StorageCommand::StoreProgress(next.persisted()));
    }

    None
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
            ui.label(format!(
                "Refresh: {:?}",
                self.emulator.panel().last_refresh()
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_scenario_contracts_pass_for_selected_panel() {
        let scenarios = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/scenarios");
        for path in scenario_paths(Some(&scenarios)).expect("list scenarios") {
            let mut emulator = Emulator::boot(None);
            let scenario = Scenario::load(&path).expect("load scenario");
            scenario
                .run(&mut emulator)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            scenario
                .assert(&emulator)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        }
    }
}
