use crate::Emulator;
use app_core::{
    AppView, Button, DisplayOrientation, FrontButtons, LibraryEvent, RefreshPolicy, SyncError,
    SyncEvent, SyncStatus,
};
use display::epd::RefreshMode;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Scenario {
    #[serde(default)]
    steps: Vec<Step>,
    #[serde(default)]
    expect: Expect,
}

#[derive(Debug, Deserialize)]
struct Step {
    button: Option<String>,
    library: Option<String>,
    count: Option<u16>,
    book_id: Option<u32>,
    pages: Option<u32>,
    chapters: Option<u8>,
    chapter: Option<u8>,
    page: Option<u32>,
    sync: Option<String>,
    ip: Option<[u8; 4]>,
    error: Option<String>,
    ssid: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Expect {
    view: Option<String>,
    book_id: Option<u32>,
    chapter: Option<u8>,
    page: Option<u32>,
    selection: Option<u16>,
    orientation: Option<String>,
    front_buttons: Option<String>,
    refresh_policy: Option<String>,
    font_size: Option<String>,
    line_spacing: Option<String>,
    font_weight: Option<String>,
    font_family: Option<String>,
    sleeping: Option<bool>,
    reading_sheet: Option<bool>,
    library_count: Option<u16>,
    last_button: Option<String>,
    last_refresh: Option<String>,
    panel_sleeping: Option<bool>,
    history_contains: Option<String>,
    pending_storage: Option<String>,
    reader_status: Option<String>,
    sync_status: Option<String>,
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn run(&self, emu: &mut Emulator) -> Result<(), String> {
        for step in &self.steps {
            if let Some(button) = &step.button {
                emu.input(parse_button(button)?);
            }
            if let Some(library) = &step.library {
                emu.library_event(parse_library_event(library, step)?);
            }
            if let Some(sync) = &step.sync {
                emu.sync_event(parse_sync_event(sync, step)?);
            }
        }
        Ok(())
    }

    pub fn assert(&self, emu: &Emulator) -> Result<(), String> {
        let state = emu.state();
        if let Some(view) = &self.expect.view {
            let expected = parse_view(view)?;
            if state.view != expected {
                return Err(format!("expected view {expected:?}, got {:?}", state.view));
            }
        }
        if let Some(book_id) = self.expect.book_id {
            expect_eq("book_id", book_id, state.book_id)?;
        }
        if let Some(chapter) = self.expect.chapter {
            expect_eq("chapter", chapter, state.chapter)?;
        }
        if let Some(page) = self.expect.page {
            expect_eq("page", page, state.page)?;
        }
        if let Some(selection) = self.expect.selection {
            expect_eq("selection", selection, state.selection)?;
        }
        if let Some(orientation) = &self.expect.orientation {
            let expected = parse_orientation(orientation)?;
            if state.orientation != expected {
                return Err(format!(
                    "expected orientation {expected:?}, got {:?}",
                    state.orientation
                ));
            }
        }
        if let Some(reading_sheet) = self.expect.reading_sheet {
            expect_eq("reading_sheet", reading_sheet, state.reading_sheet)?;
        }
        if let Some(front_buttons) = &self.expect.front_buttons {
            let expected = parse_front_buttons(front_buttons)?;
            if state.front_buttons != expected {
                return Err(format!(
                    "expected front buttons {expected:?}, got {:?}",
                    state.front_buttons
                ));
            }
        }
        if let Some(policy) = &self.expect.refresh_policy {
            let expected = parse_refresh_policy(policy)?;
            if state.refresh_policy != expected {
                return Err(format!(
                    "expected refresh policy {expected:?}, got {:?}",
                    state.refresh_policy
                ));
            }
        }
        if let Some(size) = &self.expect.font_size {
            let expected = parse_font_size(size)?;
            if state.font_size != expected {
                return Err(format!(
                    "expected font size {expected:?}, got {:?}",
                    state.font_size
                ));
            }
        }
        if let Some(spacing) = &self.expect.line_spacing {
            let expected = parse_line_spacing(spacing)?;
            if state.line_spacing != expected {
                return Err(format!(
                    "expected line spacing {expected:?}, got {:?}",
                    state.line_spacing
                ));
            }
        }
        if let Some(weight) = &self.expect.font_weight {
            let expected = parse_font_weight(weight)?;
            if state.font_weight != expected {
                return Err(format!(
                    "expected font weight {expected:?}, got {:?}",
                    state.font_weight
                ));
            }
        }
        if let Some(family) = &self.expect.font_family {
            let expected = parse_font_family(family)?;
            if state.font_family != expected {
                return Err(format!(
                    "expected font family {expected:?}, got {:?}",
                    state.font_family
                ));
            }
        }
        if let Some(sleeping) = self.expect.sleeping {
            expect_eq("sleeping", sleeping, emu.sleeping())?;
        }
        if let Some(library_count) = self.expect.library_count {
            expect_eq("library_count", library_count, state.library_count)?;
        }
        if let Some(last_button) = &self.expect.last_button {
            let expected = parse_button(last_button)?;
            if state.last_button != Some(expected) {
                return Err(format!(
                    "expected last_button {expected:?}, got {:?}",
                    state.last_button
                ));
            }
        }
        if let Some(last_refresh) = &self.expect.last_refresh {
            let expected = parse_refresh_mode(last_refresh)?;
            if emu.panel().last_refresh() != Some(expected) {
                return Err(format!(
                    "expected last_refresh {expected:?}, got {:?}",
                    emu.panel().last_refresh()
                ));
            }
        }
        if let Some(panel_sleeping) = self.expect.panel_sleeping {
            expect_eq("panel_sleeping", panel_sleeping, emu.panel().is_deep_sleep())?;
        }
        if let Some(needle) = &self.expect.history_contains {
            if !emu.panel().history().iter().any(|entry| entry.contains(needle)) {
                return Err(format!("panel history does not contain {needle:?}"));
            }
        }
        if let Some(expected) = &self.expect.pending_storage {
            let actual = emu.pending_storage_name();
            if actual != Some(expected.as_str()) {
                return Err(format!(
                    "expected pending_storage {expected:?}, got {:?}",
                    actual
                ));
            }
        }
        if let Some(expected) = &self.expect.reader_status {
            let actual = emu.reader_status_name();
            if actual != expected {
                return Err(format!(
                    "expected reader_status {expected:?}, got {actual:?}"
                ));
            }
        }
        if let Some(expected) = &self.expect.sync_status {
            let actual = sync_status_name(emu.state().sync_status);
            if actual != expected {
                return Err(format!("expected sync_status {expected:?}, got {actual:?}"));
            }
        }
        Ok(())
    }
}

fn parse_sync_event(kind: &str, step: &Step) -> Result<SyncEvent, String> {
    match kind {
        "Connecting" | "connecting" => Ok(SyncEvent::Connecting),
        "Connected" | "connected" => Ok(SyncEvent::Connected(step.ip.unwrap_or([192, 168, 1, 2]))),
        "PortalUp" | "portal-up" => Ok(SyncEvent::PortalUp),
        "Serving" | "serving" => Ok(SyncEvent::Serving(step.ip.unwrap_or([192, 168, 0, 233]))),
        "NetworkSaved" | "network-saved" => Ok(SyncEvent::NetworkSaved(
            app_core::WifiSsid::new(step.ssid.as_deref().unwrap_or("HOME-WIFI"))
                .ok_or_else(|| "bad ssid".to_string())?,
        )),
        "CredentialsSaved" | "credentials-saved" => Ok(SyncEvent::CredentialsSaved(
            app_core::WifiSsid::new(step.ssid.as_deref().unwrap_or("HOME-WIFI"))
                .ok_or_else(|| "bad ssid".to_string())?,
        )),
        "Failed" | "failed" => Ok(SyncEvent::Failed(parse_sync_error(
            step.error.as_deref().unwrap_or("server"),
        )?)),
        _ => Err(format!("unknown sync event: {kind}")),
    }
}

fn parse_sync_error(value: &str) -> Result<SyncError, String> {
    match value {
        "radio" => Ok(SyncError::RadioInit),
        "join" => Ok(SyncError::Join),
        "dhcp" => Ok(SyncError::Dhcp),
        _ => Err(format!("unknown sync error: {value}")),
    }
}

fn sync_status_name(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::NotConfigured => "not-configured",
        SyncStatus::Idle => "idle",
        SyncStatus::ForgetPending => "forget-pending",
        SyncStatus::Starting => "starting",
        SyncStatus::Connecting => "connecting",
        SyncStatus::Connected(_) => "connected",
        SyncStatus::PortalUp => "portal-up",
        SyncStatus::Serving(_) => "serving",
        SyncStatus::CredentialsSaved => "credentials-saved",
        SyncStatus::Error(_) => "error",
    }
}

fn parse_library_event(kind: &str, step: &Step) -> Result<LibraryEvent, String> {
    match kind {
        "Scanned" | "scanned" => Ok(LibraryEvent::Scanned {
            count: step.count.unwrap_or(0),
        }),
        "Loaded" | "loaded" => Ok(LibraryEvent::Loaded {
            book_id: step.book_id.unwrap_or(2),
            pages: step.pages.unwrap_or(1),
            chapters: step.chapters.unwrap_or(1),
            current_chapter: u16::from(step.chapter.unwrap_or(0)),
            chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
        }),
        "ChapterPage" | "chapter-page" | "chapter_page" => Ok(LibraryEvent::ChapterPage {
            book_id: step.book_id.unwrap_or(2),
            chapter: step.chapter.unwrap_or(0),
            page: step.page.unwrap_or(0),
        }),
        _ => Err(format!("unknown library event: {kind}")),
    }
}

fn parse_button(value: &str) -> Result<Button, String> {
    match value {
        "Power" | "power" => Ok(Button::Power),
        "Back" | "back" => Ok(Button::Back),
        "Confirm" | "confirm" | "Ok" | "ok" => Ok(Button::Confirm),
        "Previous" | "previous" | "prev" => Ok(Button::Previous),
        "Next" | "next" => Ok(Button::Next),
        "PagePrevious" | "page-previous" | "page-prev" => Ok(Button::PagePrevious),
        "PageNext" | "page-next" => Ok(Button::PageNext),
        _ => Err(format!("unknown button: {value}")),
    }
}

fn parse_view(value: &str) -> Result<AppView, String> {
    match value {
        "Home" | "home" => Ok(AppView::Home),
        "Library" | "library" => Ok(AppView::Library),
        "Reading" | "reading" => Ok(AppView::Reading),
        "Chapters" | "chapters" => Ok(AppView::Chapters),
        "Wireless" | "wireless" | "Sync" | "sync" => Ok(AppView::Wireless),
        "Settings" | "settings" => Ok(AppView::Settings),
        _ => Err(format!("unknown view: {value}")),
    }
}

fn parse_orientation(value: &str) -> Result<DisplayOrientation, String> {
    match value {
        "LandscapeButtonsBottom" | "landscape-bottom" => {
            Ok(DisplayOrientation::LandscapeButtonsBottom)
        }
        "LandscapeButtonsTop" | "landscape-top" => Ok(DisplayOrientation::LandscapeButtonsTop),
        "PortraitButtonsLeft" | "portrait-left" => Ok(DisplayOrientation::PortraitButtonsLeft),
        "PortraitButtonsRight" | "portrait-right" => Ok(DisplayOrientation::PortraitButtonsRight),
        _ => Err(format!("unknown orientation: {value}")),
    }
}

fn parse_front_buttons(value: &str) -> Result<FrontButtons, String> {
    match value {
        "PagesRight" | "pages-right" => Ok(FrontButtons::PagesRight),
        "PagesLeft" | "pages-left" => Ok(FrontButtons::PagesLeft),
        _ => Err(format!("unknown front buttons: {value}")),
    }
}

fn parse_refresh_policy(value: &str) -> Result<RefreshPolicy, String> {
    match value {
        "FastOnly" | "fast" => Ok(RefreshPolicy::FastOnly),
        "FullOnWake" | "wake" => Ok(RefreshPolicy::FullOnWake),
        "FullEveryTen" | "ten" => Ok(RefreshPolicy::FullEveryTen),
        _ => Err(format!("unknown refresh policy: {value}")),
    }
}

fn parse_font_size(value: &str) -> Result<display::font::FontSize, String> {
    match value {
        "Small" | "small" => Ok(display::font::FontSize::Small),
        "Medium" | "medium" => Ok(display::font::FontSize::Medium),
        "Large" | "large" => Ok(display::font::FontSize::Large),
        _ => Err(format!("unknown font size: {value}")),
    }
}

fn parse_line_spacing(value: &str) -> Result<display::font::LineSpacing, String> {
    match value {
        "Compact" | "compact" => Ok(display::font::LineSpacing::Compact),
        "Normal" | "normal" => Ok(display::font::LineSpacing::Normal),
        "Relaxed" | "relaxed" => Ok(display::font::LineSpacing::Relaxed),
        _ => Err(format!("unknown line spacing: {value}")),
    }
}

fn parse_font_weight(value: &str) -> Result<display::font::FontWeight, String> {
    match value {
        "Normal" | "normal" => Ok(display::font::FontWeight::Normal),
        "Heavy" | "heavy" => Ok(display::font::FontWeight::Heavy),
        _ => Err(format!("unknown font weight: {value}")),
    }
}

fn parse_font_family(value: &str) -> Result<display::font::FontFamily, String> {
    match value {
        "Literata" | "literata" => Ok(display::font::FontFamily::Literata),
        "Merriweather" | "merriweather" => Ok(display::font::FontFamily::Merriweather),
        "Custom" | "custom" => Ok(display::font::FontFamily::Custom),
        _ => Err(format!("unknown font family: {value}")),
    }
}

fn parse_refresh_mode(value: &str) -> Result<RefreshMode, String> {
    match value {
        "Full" | "full" => Ok(RefreshMode::Full),
        "Fast" | "fast" => Ok(RefreshMode::Fast),
        "FastClean" | "fast-clean" | "fastclean" => Ok(RefreshMode::FastClean),
        "PowerDown" | "power-down" | "powerdown" => Ok(RefreshMode::PowerDown),
        _ => Err(format!("unknown refresh mode: {value}")),
    }
}

fn expect_eq<T: core::fmt::Debug + PartialEq>(
    name: &str,
    expected: T,
    actual: T,
) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(format!("expected {name} {expected:?}, got {actual:?}"))
    }
}
