use crate::Emulator;
use app_core::{AppView, Button, DisplayOrientation, LibraryEvent, RefreshPolicy};
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
    count: Option<u8>,
    book_id: Option<u32>,
    pages: Option<u32>,
    chapters: Option<u8>,
    chapter: Option<u8>,
    page: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct Expect {
    view: Option<String>,
    book_id: Option<u32>,
    chapter: Option<u8>,
    page: Option<u32>,
    selection: Option<u8>,
    orientation: Option<String>,
    refresh_policy: Option<String>,
    sleeping: Option<bool>,
    library_count: Option<u8>,
    last_button: Option<String>,
    last_refresh: Option<String>,
    panel_sleeping: Option<bool>,
    history_contains: Option<String>,
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
        if let Some(policy) = &self.expect.refresh_policy {
            let expected = parse_refresh_policy(policy)?;
            if state.refresh_policy != expected {
                return Err(format!(
                    "expected refresh policy {expected:?}, got {:?}",
                    state.refresh_policy
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
        Ok(())
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
        _ => Err(format!("unknown button: {value}")),
    }
}

fn parse_view(value: &str) -> Result<AppView, String> {
    match value {
        "Home" | "home" => Ok(AppView::Home),
        "Library" | "library" => Ok(AppView::Library),
        "Reading" | "reading" => Ok(AppView::Reading),
        "Chapters" | "chapters" => Ok(AppView::Chapters),
        "Sync" | "sync" => Ok(AppView::Sync),
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

fn parse_refresh_policy(value: &str) -> Result<RefreshPolicy, String> {
    match value {
        "FastOnly" | "fast" => Ok(RefreshPolicy::FastOnly),
        "FullOnWake" | "wake" => Ok(RefreshPolicy::FullOnWake),
        "FullEveryTen" | "ten" => Ok(RefreshPolicy::FullEveryTen),
        _ => Err(format!("unknown refresh policy: {value}")),
    }
}

fn parse_refresh_mode(value: &str) -> Result<RefreshMode, String> {
    match value {
        "Full" | "full" => Ok(RefreshMode::Full),
        "Fast" | "fast" => Ok(RefreshMode::Fast),
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
