#![no_std]
#![forbid(unsafe_code)]

pub mod app_render;
pub mod layout;
pub mod reading;
pub mod render;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiView {
    Home,
    Library,
    Chapters,
    Sync,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiRefreshPolicy {
    FastOnly,
    FullOnWake,
    FullEveryTen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLibraryStatus {
    NotScanned,
    Scanning,
    Ready,
    Empty,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiCover<'a> {
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub bits: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiBook<'a> {
    pub title: &'a str,
    pub author: &'a str,
    pub progress_permille: u16,
    pub cover: Option<UiCover<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiTocItem<'a> {
    pub title: &'a str,
    pub level: u8,
    /// 1-based book page the chapter starts on; 0 when unknown.
    pub page: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiShell<'a> {
    pub view: UiView,
    pub orientation: UiOrientation,
    pub refresh_policy: UiRefreshPolicy,
    pub selection: u8,
    pub chapter: u8,
    pub page: u32,
    pub page_count: u32,
    pub battery_percent: u8,
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    pub library_entries: &'a [&'a str],
    pub chapters: &'a [UiTocItem<'a>],
}
