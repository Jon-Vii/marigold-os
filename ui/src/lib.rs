#![no_std]
#![forbid(unsafe_code)]

pub mod app_render;
pub mod layout;
pub mod qr_generated;
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

/// Sync screen lifecycle, mirrored from app-core so the renderer stays
/// decoupled from reducer types the way UiView mirrors AppView.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiSyncStatus {
    NotConfigured,
    Idle,
    Starting,
    Connecting,
    Connected([u8; 4]),
    Syncing,
    Done { pushed: bool, pulled: bool },
    PortalUp,
    Serving([u8; 4]),
    CredentialsSaved,
    Error(&'static str),
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
    pub font_size: display::font::FontSize,
    pub line_spacing: display::font::LineSpacing,
    pub selection: u16,
    pub chapter: u8,
    /// The current chapter's title resolved over the whole book (past the
    /// resident `chapters` cap). When non-empty the colophon prefers it over
    /// `chapters[chapter]`; empty falls back to the list or a numeral.
    pub chapter_title: &'a str,
    pub page: u32,
    pub page_count: u32,
    pub battery_percent: u8,
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    /// The resident slice of the on-disk catalog the Library list draws from:
    /// `library_entries[i]` is the book at absolute index
    /// `library_window_start + i`. The full catalog is streamed from the card,
    /// so this window holds only the rows around the selection.
    pub library_entries: &'a [&'a str],
    pub library_window_start: u16,
    /// Total book count across the whole catalog, independent of the resident
    /// window — drives the "x of N" footer and the scroll math.
    pub library_total: u16,
    pub chapters: &'a [UiTocItem<'a>],
    pub sync_status: UiSyncStatus,
}
