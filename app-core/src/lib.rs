#![no_std]
#![forbid(unsafe_code)]

use display::font::{FontFamily, FontSize, FontWeight, LineSpacing, TypeSettings};
use display::{epd::RefreshMode, Rect};

pub const SETTINGS_ITEMS: u8 = 7;
pub const MAX_SD_CHAPTERS: usize = 128;
pub const FIRST_SD_BOOK_ID: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderSource {
    BuiltIn { book_id: u32 },
    Sd { index: u16 },
}

impl ReaderSource {
    pub fn from_book_id(book_id: u32) -> Self {
        if book_id >= FIRST_SD_BOOK_ID {
            Self::Sd {
                index: book_id
                    .saturating_sub(FIRST_SD_BOOK_ID)
                    .min(u16::MAX as u32) as u16,
            }
        } else {
            Self::BuiltIn { book_id }
        }
    }

    pub const fn sd(index: u16) -> Self {
        Self::Sd { index }
    }

    pub const fn book_id(self) -> u32 {
        match self {
            Self::BuiltIn { book_id } => book_id,
            Self::Sd { index } => FIRST_SD_BOOK_ID + index as u32,
        }
    }

    pub const fn sd_index(self) -> Option<u16> {
        match self {
            Self::BuiltIn { .. } => None,
            Self::Sd { index } => Some(index),
        }
    }

    pub const fn is_sd(self) -> bool {
        matches!(self, Self::Sd { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Power,
    Back,
    Confirm,
    Previous,
    Next,
    PagePrevious,
    PageNext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Sample {
        button: Option<Button>,
        aux_raw: u16,
        nav_raw: u16,
        page_raw: u16,
        battery_mv: u16,
        battery_percent: u8,
    },
}

impl InputEvent {
    pub const fn button(button: Button) -> Self {
        Self::Sample {
            button: Some(button),
            aux_raw: 2000,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 4000,
            battery_percent: 77,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderKind {
    Boot,
    Page,
    Battery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppView {
    Home,
    Library,
    Reading,
    Chapters,
    Wireless,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeAction {
    Read,
    Files,
    Wireless,
    Settings,
}

/// Where the front page-turn pair sits. The front row is two pairs —
/// back/confirm and previous/next. `PagesLeft` exchanges the pairs whole,
/// keeping each pair's internal order, for readers whose thumb rests on
/// the other end of the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontButtons {
    PagesRight,
    PagesLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshPolicy {
    FastOnly,
    FullOnWake,
    FullEveryTen,
}

pub const DEFAULT_FULL_REFRESH_INTERVAL: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshPlanner {
    screen_on: bool,
    fast_refreshes: u8,
    last_request: Option<RenderRequest>,
    fast_refresh_enabled: bool,
    full_refresh_interval: u8,
    panel_shows_sleep_screen: bool,
}

impl Default for RefreshPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl RefreshPlanner {
    pub const fn new() -> Self {
        Self {
            screen_on: false,
            fast_refreshes: 0,
            last_request: None,
            fast_refresh_enabled: true,
            full_refresh_interval: DEFAULT_FULL_REFRESH_INTERVAL,
            panel_shows_sleep_screen: false,
        }
    }

    pub const fn with_fast_refresh_enabled(mut self, enabled: bool) -> Self {
        self.fast_refresh_enabled = enabled;
        self
    }

    /// Seeds a fresh planner with the knowledge that the panel already shows
    /// the sleep screen. Deep sleep is terminal — waking reboots the chip and
    /// builds a new planner — but the only deep-sleep entry path draws the
    /// sleep screen and waits for the panel to settle before cutting power,
    /// so on a deep-sleep wake the panel contents are known by construction
    /// and the first render can take the one-flicker clean instead of the
    /// multi-flash full waveform. Callers must gate the seed strictly on the
    /// deep-sleep wake cause *and* on a persisted record that the sleep
    /// frame actually settled: the sleep path still powers down when its
    /// final flush fails, and any other cold boot (battery pull, crash,
    /// software reset) leaves unknown pixels that only `Full` clears.
    pub const fn with_panel_shows_sleep_screen(mut self, shows_sleep_screen: bool) -> Self {
        self.panel_shows_sleep_screen = shows_sleep_screen;
        self
    }

    pub const fn screen_on(&self) -> bool {
        self.screen_on
    }

    pub const fn last_request(&self) -> Option<RenderRequest> {
        self.last_request
    }

    pub fn mode_for(&self, request: RenderRequest) -> RefreshMode {
        let Some(last) = self.last_request else {
            // Cold boot leaves unknown pixels on the panel; only the deep
            // full waveform reliably clears them. After a display sleep the
            // panel still shows the sleep screen this firmware drew, so the
            // one-flicker clean is enough to wake.
            return if self.fast_refresh_enabled && self.panel_shows_sleep_screen {
                RefreshMode::FastClean
            } else {
                RefreshMode::Full
            };
        };
        if !self.fast_refresh_enabled || !self.screen_on {
            return RefreshMode::Full;
        }
        // Context changes need ghost cleanup, but the panel state is known
        // (the frame just shown), so the one-flicker clean suffices and the
        // multi-flash full waveform stays reserved for boot and sleep.
        if last.kind == RenderKind::Boot
            || request.view != last.view
            || request.book_id != last.book_id
            || request.orientation != last.orientation
            // A type-settings change redraws whole text columns; the clean
            // pass avoids fast-diff ghosting across the page.
            || request.font_size != last.font_size
            || request.line_spacing != last.line_spacing
            || request.font_weight != last.font_weight
            || request.font_family != last.font_family
            || Self::needs_clean_library_refresh(request, last)
        {
            return RefreshMode::FastClean;
        }
        match request.refresh_policy {
            RefreshPolicy::FastOnly | RefreshPolicy::FullOnWake => RefreshMode::Fast,
            RefreshPolicy::FullEveryTen if self.fast_refreshes >= self.full_refresh_interval => {
                RefreshMode::FastClean
            }
            RefreshPolicy::FullEveryTen => RefreshMode::Fast,
        }
    }

    pub fn record_render(&mut self, request: RenderRequest, mode: RefreshMode) {
        self.screen_on = true;
        self.last_request = Some(request);
        self.panel_shows_sleep_screen = false;
        if mode == RefreshMode::Fast {
            self.fast_refreshes = self.fast_refreshes.saturating_add(1);
        } else {
            self.fast_refreshes = 0;
        }
    }

    /// Records the panel powering down at the end of the sleep handshake.
    /// Clearing `last_request` is what makes the next render re-init the
    /// panel, so this must run whenever the panel actually slept — even if
    /// the sleep-frame flush failed. `panel_shows_sleep_screen` carries that
    /// flush outcome: `true` lets the wake render take the one-flicker
    /// clean, `false` (stale pixels under a failed flush) keeps the deep
    /// full waveform that unknown panel contents require.
    pub fn record_sleep(&mut self, panel_shows_sleep_screen: bool) {
        self.screen_on = false;
        self.fast_refreshes = 0;
        self.last_request = None;
        self.panel_shows_sleep_screen = panel_shows_sleep_screen;
    }

    fn needs_clean_library_refresh(request: RenderRequest, last: RenderRequest) -> bool {
        // Only the library list actually redraws when the scan count moves;
        // other views repaint identical pixels and can ride the partial.
        request.view == AppView::Library && request.library_count != last.library_count
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub kind: RenderKind,
    pub view: AppView,
    pub page: u32,
    pub page_count: u32,
    pub chapter: u8,
    pub selection: u16,
    pub book_id: u32,
    pub orientation: DisplayOrientation,
    pub front_buttons: FrontButtons,
    /// Portrait reading's summoned key sheet is up; renderers draw it
    /// over the page's bottom band.
    pub reading_sheet: bool,
    pub refresh_policy: RefreshPolicy,
    pub font_size: FontSize,
    pub line_spacing: LineSpacing,
    pub font_weight: FontWeight,
    pub font_family: FontFamily,
    pub last_button: Option<Button>,
    pub aux_raw: u16,
    pub nav_raw: u16,
    pub page_raw: u16,
    pub battery_mv: u16,
    pub battery_percent: u8,
    pub library_count: u16,
    pub sync_status: SyncStatus,
    /// Saved network name for the Wireless screen; len 0 when none.
    pub wifi_ssid: [u8; 32],
    pub wifi_ssid_len: u8,
    pub dirty: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayCommand {
    Render(RenderRequest),
    Sleep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageCommand {
    LoadCatalogCache,
    RefreshCatalog,
    OpenBook {
        request_id: u32,
        book_id: u32,
        index: u16,
        chapter: u8,
        target_pages: u16,
        type_settings: TypeSettings,
        /// Paginate into the portrait page box. Rides beside the type
        /// settings because it changes wrap points the same way.
        portrait: bool,
    },
    ExtendSection {
        request_id: u32,
        book_id: u32,
        index: u16,
        chapter: u8,
        target_pages: u16,
        type_settings: TypeSettings,
        portrait: bool,
    },
    /// Load the full chapter list (TOC.BIN) into the reader's section buffer
    /// for the Chapters overview. The reading section reloads on exit.
    LoadChapters {
        request_id: u32,
        book_id: u32,
        index: u16,
    },
    /// Jump to a chapter from the overview. The display task resolves the
    /// chapter's start page from the on-disk TOC (the reducer's chapter-page
    /// map is capped at 128) and loads that section.
    JumpChapter {
        request_id: u32,
        book_id: u32,
        index: u16,
        chapter: u8,
        type_settings: TypeSettings,
        portrait: bool,
    },
    StoreProgress(PersistedAppState),
    /// Hand the EPUB scratch to the wifi task as sync-session heap. One
    /// way: after this the display task refuses scratch-using commands
    /// until the session's software reset reboots the reader.
    LoanSyncMemory,
    /// Persist the credentials captured by the onboarding portal to
    /// /XTEINK/WIFI.BIN. Allowed during a sync session: it is the portal
    /// that sends it.
    StoreWifiCredentials(WifiCredentials),
    /// Delete /XTEINK/WIFI.BIN. Sent when the user confirms "forget" on
    /// the Wireless screen, which is only reachable before the radio
    /// starts, so it never runs during a sync session.
    ForgetWifiCredentials,
    /// Enter the upload session: the display task parks on the upload
    /// channels and writes browser-sent books to /BOOKS until the
    /// session's reset. Sent by the wifi task at the first upload.
    ReceiveUpload,
}

/// The sync session's storage-admission rules. Granting the loan is one-way:
/// the EPUB scratch becomes radio heap, so every scratch-using storage command
/// is refused from then on and only the session-ending software reset brings
/// the reader pipeline back. Progress writes stay alive (they are cheap and
/// harmless), the portal stores credentials, and uploads only make sense
/// while the browser shelf is being served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SyncSession {
    /// The reader pipeline owns the scratch; ordinary storage work runs.
    #[default]
    Idle,
    /// The scratch is loaned to the radio until the session-ending reset.
    Loaned,
}

impl SyncSession {
    /// Whether a storage command may run in the current session state.
    pub fn admits(&self, command: &StorageCommand) -> bool {
        match self {
            // Uploads arrive from the browser shelf, which only exists once
            // the session is serving; outside it the command is a stray.
            SyncSession::Idle => !matches!(command, StorageCommand::ReceiveUpload),
            SyncSession::Loaned => matches!(
                command,
                StorageCommand::StoreProgress(_)
                    | StorageCommand::StoreWifiCredentials(_)
                    | StorageCommand::ReceiveUpload
            ),
        }
    }

    /// Whether the session is running, i.e. the loan has been granted.
    /// Render-path catalog reads stop once this is true: the browser shelf
    /// may be rewriting the card underneath, and the visible surface is the
    /// Sync screen anyway.
    pub fn active(&self) -> bool {
        matches!(self, SyncSession::Loaned)
    }

    /// One-way transition: the display task has dismantled the scratch and
    /// shipped it to the wifi task.
    pub fn loan_granted(&mut self) {
        *self = SyncSession::Loaned;
    }
}

/// Station credentials as a bounded Copy message: what the onboarding
/// portal captures and what `/XTEINK/WIFI.BIN` stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCredentials {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub password: [u8; 64],
    pub password_len: u8,
}

impl WifiCredentials {
    pub fn from_strs(ssid: &str, password: &str) -> Option<Self> {
        if ssid.is_empty() || ssid.len() > 32 || password.len() > 64 {
            return None;
        }
        let mut record = Self {
            ssid: [0; 32],
            ssid_len: ssid.len() as u8,
            password: [0; 64],
            password_len: password.len() as u8,
        };
        record.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());
        record.password[..password.len()].copy_from_slice(password.as_bytes());
        Some(record)
    }

    pub fn ssid(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.ssid_len.min(32) as usize]).unwrap_or("")
    }

    pub fn password(&self) -> &str {
        core::str::from_utf8(&self.password[..self.password_len.min(64) as usize]).unwrap_or("")
    }

    pub fn ssid_message(&self) -> WifiSsid {
        WifiSsid {
            bytes: self.ssid,
            len: self.ssid_len,
        }
    }
}

/// A network name alone, as a bounded Copy message: what the Wireless
/// screen shows. Events carry this instead of `WifiCredentials` so the
/// password never travels further than the radio and the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiSsid {
    pub bytes: [u8; 32],
    pub len: u8,
}

impl WifiSsid {
    pub fn new(ssid: &str) -> Option<Self> {
        if ssid.is_empty() || ssid.len() > 32 {
            return None;
        }
        let mut message = Self {
            bytes: [0; 32],
            len: ssid.len() as u8,
        };
        message.bytes[..ssid.len()].copy_from_slice(ssid.as_bytes());
        Some(message)
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len.min(32) as usize]).unwrap_or("")
    }
}

/// The onboarding hotspot's WPA2 PSK, minted fresh from the hardware RNG
/// each time the portal starts. It rides `SyncEvent::PortalUp` into
/// `SyncStatus` so the Wireless screen can render the join QR and the
/// manual-join password text — the display is the only channel that
/// carries it, so nothing secret lives in the repo or the release binary.
/// Always exactly [`PortalPsk::LEN`] ASCII characters from
/// [`PSK_ALPHABET`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PortalPsk {
    bytes: [u8; PortalPsk::LEN],
}

impl core::fmt::Debug for PortalPsk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PortalPsk")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

/// Alphabet for the per-session portal PSK: ASCII alphanumerics minus
/// the hand-typing-ambiguous 0/O/1/I/l/i/o (phones that cannot scan
/// type it from the screen) and nothing the `WIFI:` QR payload needs
/// escaped (`\ ; , : "`). 55 characters. Lives here rather than in the
/// firmware's minting code so [`PortalPsk::EMULATOR_DEMO`] is
/// host-testable against it.
pub const PSK_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz";

impl PortalPsk {
    pub const LEN: usize = 16;

    /// Fixed value for the emulators' synthetic portal flow, so golden
    /// frames render deterministically. Sixteen characters from the same
    /// unambiguous alphabet the firmware mints from; never used on
    /// hardware.
    pub const EMULATOR_DEMO: Self = Self {
        bytes: *b"emudemqpsk234567",
    };

    /// Constructs a PSK, refusing any byte outside [`PSK_ALPHABET`] —
    /// which also rules out non-ASCII bytes and the characters the
    /// `WIFI:` QR payload would need escaped.
    pub fn new(bytes: [u8; Self::LEN]) -> Option<Self> {
        if bytes.iter().all(|b| PSK_ALPHABET.contains(b)) {
            Some(Self { bytes })
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        // PSK_ALPHABET is pure ASCII, so validated bytes are always UTF-8.
        core::str::from_utf8(&self.bytes).unwrap_or("")
    }

    pub const fn bytes(&self) -> [u8; Self::LEN] {
        self.bytes
    }
}

// Bounded Copy messages by design: chapter_pages rides inside the event
// because firmware has no heap to box large variants into.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayEvent {
    Settled,
    Asleep,
    Library(LibraryEvent),
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryEvent {
    Scanned {
        count: u16,
    },
    Loaded {
        book_id: u32,
        pages: u32,
        chapters: u8,
        /// The chapter the reading page currently sits in, computed by the
        /// firmware over the whole book. Unlike `chapter_pages` (capped at
        /// `MAX_SD_CHAPTERS`), this tracks position into a long book so the
        /// colophon and chapter cursor do not stick past the cap.
        current_chapter: u16,
        chapter_pages: [u16; MAX_SD_CHAPTERS],
    },
    ChapterPage {
        book_id: u32,
        chapter: u8,
        page: u32,
    },
    /// The firmware re-resolved the current chapter for the page just rendered,
    /// over the whole-book (uncapped) map. Sent on reading renders so the cursor
    /// keeps tracking past `MAX_SD_CHAPTERS` between section loads, where the
    /// reducer's own `sd_chapter_for_page` saturates.
    ChapterCursor {
        book_id: u32,
        current_chapter: u16,
    },
    CustomFont {
        available: bool,
    },
    Restored {
        book_id: u32,
        chapter: u8,
        page: u32,
        /// The book's total page count, read from the cache index header at
        /// restore so the Home progress bar has a denominator before the book
        /// is opened. 0 when unavailable (the bar keeps its fallback).
        page_count: u32,
        reading_orientation: u8,
        refresh_policy: u8,
        font_size: u8,
        line_spacing: u8,
        font_weight: u8,
        font_family: u8,
        front_buttons: u8,
    },
}

/// Wi-Fi session lifecycle as shown on the Wireless screen. The wifi task
/// owns the radio and reports transitions back as `SyncEvent`s; the reducer
/// only records what the screen should say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    /// No Wi-Fi network is saved; Confirm starts the onboarding hotspot.
    NotConfigured,
    /// A network is saved and the radio is untouched; Confirm connects.
    Idle,
    /// "Forget this network" awaits its confirmation: Confirm deletes the
    /// saved credentials, Back cancels. Only reachable from Idle, so the
    /// radio is still untouched.
    ForgetPending,
    /// Confirm was pressed: the app shell must emit `SyncCommand::Start`.
    Starting,
    Connecting,
    /// Joined and DHCP-configured with this IPv4 address.
    Connected([u8; 4]),
    /// The onboarding hotspot is up; the screen renders the join QR and
    /// manual-join password from this session's PSK.
    PortalUp(PortalPsk),
    /// Connected and the book server answers at this address until the
    /// session ends.
    Serving([u8; 4]),
    /// The portal captured and stored credentials; a fresh session will
    /// use them after the reset.
    CredentialsSaved,
    Error(SyncError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncError {
    RadioInit,
    Join,
    Dhcp,
    /// The pre-loan flush of the coalesced reading position failed, so the
    /// display task refused to dismantle the reader scratch over an unsaved
    /// position. Nothing was loaned; Confirm retries the session.
    Storage,
}

/// wifi task -> app task progress reports for the Wireless screen. The
/// display task also sends `NetworkSaved` once at boot, after reading
/// /XTEINK/WIFI.BIN, so the screen can name the saved network before any
/// session starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    /// A saved network exists (on the card or compiled in); the screen
    /// shows its name and offers connect/forget.
    NetworkSaved(WifiSsid),
    Connecting,
    Connected([u8; 4]),
    /// The onboarding hotspot is up, secured with this session's PSK.
    PortalUp(PortalPsk),
    Serving([u8; 4]),
    CredentialsSaved(WifiSsid),
    Failed(SyncError),
}

/// app task -> wifi task session control. Starting a session loans reader
/// memory to the radio irrevocably; Exit therefore maps to a software reset
/// on hardware once a session has started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncCommand {
    Start,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    /// User input landed; carries the view the input left the app in so the
    /// power task can tier its idle timeout (long leash while Reading,
    /// short on the shell views).
    Activity(AppView),
    DisplaySettled,
    DisplayAsleep,
    SleepNow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistedAppState {
    pub book_id: u32,
    pub chapter: u16,
    pub screen: u32,
    pub shell_orientation: u8,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
    pub font_size: u8,
    pub line_spacing: u8,
    pub font_weight: u8,
    pub font_family: u8,
    pub front_buttons: u8,
    pub source_hash: u32,
    pub source_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReducerContext {
    pub builtin_book_count: u8,
    pub builtin_chapter_count: u8,
}

impl ReducerContext {
    pub const fn new(builtin_book_count: u8, builtin_chapter_count: u8) -> Self {
        Self {
            builtin_book_count,
            builtin_chapter_count,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderState {
    pub view: AppView,
    pub page: u32,
    pub selection: u16,
    pub chapter: u8,
    pub book_id: u32,
    pub orientation: DisplayOrientation,
    pub front_buttons: FrontButtons,
    pub refresh_policy: RefreshPolicy,
    pub font_size: FontSize,
    pub line_spacing: LineSpacing,
    pub font_weight: FontWeight,
    pub font_family: FontFamily,
    pub custom_font_available: bool,
    pub last_button: Option<Button>,
    pub aux_raw: u16,
    pub nav_raw: u16,
    pub page_raw: u16,
    pub battery_mv: u16,
    pub battery_percent: u8,
    pub library_count: u16,
    pub sd_page_count: u32,
    pub sd_chapter_count: u8,
    pub sd_chapter_pages: [u16; MAX_SD_CHAPTERS],
    pub read_request_pending: bool,
    /// Portrait reading's summoned key sheet is up: the next named-key
    /// press acts on the label it revealed instead of summoning again.
    pub reading_sheet: bool,
    pub sync_status: SyncStatus,
    /// The saved Wi-Fi network's name; len 0 means none is saved. Fed by
    /// `SyncEvent::NetworkSaved` at boot and `CredentialsSaved` from the
    /// portal, cleared by the forget flow.
    pub wifi_ssid: [u8; 32],
    pub wifi_ssid_len: u8,
    pub dirty: Rect,
}

impl ReaderState {
    pub const fn boot() -> Self {
        Self {
            view: AppView::Home,
            page: 0,
            selection: 0,
            chapter: 0,
            book_id: 1,
            orientation: DisplayOrientation::LandscapeButtonsBottom,
            front_buttons: FrontButtons::PagesRight,
            refresh_policy: RefreshPolicy::FullOnWake,
            font_size: FontSize::Medium,
            line_spacing: LineSpacing::Normal,
            font_weight: FontWeight::Normal,
            font_family: FontFamily::Literata,
            custom_font_available: false,
            last_button: None,
            aux_raw: 0,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 0,
            battery_percent: 100,
            library_count: 0,
            sd_page_count: 1,
            sd_chapter_count: 1,
            sd_chapter_pages: [0; MAX_SD_CHAPTERS],
            read_request_pending: false,
            reading_sheet: false,
            sync_status: SyncStatus::NotConfigured,
            wifi_ssid: [0; 32],
            wifi_ssid_len: 0,
            dirty: Rect::FULL,
        }
    }

    /// The saved network's name; empty when none is saved.
    pub fn wifi_ssid(&self) -> &str {
        core::str::from_utf8(&self.wifi_ssid[..self.wifi_ssid_len.min(32) as usize]).unwrap_or("")
    }

    pub fn wifi_network_saved(&self) -> bool {
        self.wifi_ssid_len > 0
    }

    pub fn apply_input(self, ctx: ReducerContext, event: InputEvent) -> Self {
        let InputEvent::Sample {
            button: raw_button,
            aux_raw,
            nav_raw,
            page_raw,
            battery_mv,
            battery_percent,
        } = event;
        let button = orient_button(
            self.orientation,
            swap_front_pairs(self.front_buttons, raw_button),
        );
        // Home is positional, not grammatical: its four actions direct-map
        // the physical key column, with the ordering itself ranked (continue
        // second from the top). The front-pair swap moves roles to the
        // resting thumb, but Home has no roles to move -- riding the swap
        // would demote continue to the far end and put wireless and settings
        // on the comfortable pair. So Home maps from the un-swapped keys and
        // reads the same for every user.
        let home_button = orient_button(self.orientation, raw_button);
        let mut next = self;
        next.last_button = button;
        next.aux_raw = aux_raw;
        next.nav_raw = nav_raw;
        next.page_raw = page_raw;
        next.battery_mv = battery_mv;
        next.battery_percent = battery_percent;
        next.dirty = Rect::FULL;

        // Portrait reading is full-bleed: the first named-key press summons
        // the key sheet above the buttons (the margin appears when called
        // for); the second press acts on the label it revealed. Page turns
        // never wait on the sheet -- reading momentum would make a turn a
        // second press -- so the browse pair acts at once, dismissing it.
        // Landscape keeps its direct mapping.
        if self.view == AppView::Reading && is_portrait(self.orientation) {
            match button {
                Some(Button::Confirm | Button::Back) if !self.reading_sheet => {
                    next.reading_sheet = true;
                    return next;
                }
                Some(Button::Power) | None => {}
                Some(_) => next.reading_sheet = false,
            }
        }

        match (self.view, button) {
            (_, None) => {}
            (_, Some(Button::Power)) => {}
            (AppView::Home, Some(_)) => {
                if let Some(home_button) = home_button {
                    next = apply_home_action(next, home_action_for_button(home_button));
                }
            }
            (AppView::Library, Some(Button::Next | Button::PageNext)) => {
                next.selection = wrap_next(self.selection, self.library_item_count(ctx));
            }
            (AppView::Library, Some(Button::Previous | Button::PagePrevious)) => {
                next.selection = wrap_prev(self.selection, self.library_item_count(ctx));
            }
            // Imprint key grammar: Back always zooms out one level,
            // Confirm always affirms the screen's primary action.
            (AppView::Library, Some(Button::Confirm)) => {
                if self.selection < self.library_count {
                    next.book_id = ReaderSource::sd(self.selection).book_id();
                    next.view = AppView::Reading;
                    next.chapter = 0;
                    next.selection = 0;
                    next.page = 0;
                    next.sd_page_count = 1;
                    next.sd_chapter_count = 1;
                    next.sd_chapter_pages = [0; MAX_SD_CHAPTERS];
                    next.read_request_pending = false;
                }
            }
            (AppView::Library, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 0;
                next.read_request_pending = false;
            }
            (AppView::Reading, Some(Button::Next | Button::PageNext)) => {
                if ReaderSource::from_book_id(self.book_id).is_sd() {
                    if self.page + 1 < self.sd_page_count {
                        next.page = self.page + 1;
                    } else {
                        next.page = self.sd_page_count.saturating_sub(1);
                    }
                    next.chapter = next.sd_chapter_for_page(next.page);
                    next.selection = next.chapter as u16;
                } else {
                    next.chapter = wrap_next(
                        self.chapter as u16,
                        (ctx.builtin_chapter_count as u16).max(1),
                    ) as u8;
                    next.selection = next.chapter as u16;
                    next.page = 0;
                }
            }
            (AppView::Reading, Some(Button::Previous | Button::PagePrevious)) => {
                if ReaderSource::from_book_id(self.book_id).is_sd() {
                    if self.page > 0 {
                        next.page = self.page - 1;
                    }
                    next.chapter = next.sd_chapter_for_page(next.page);
                    next.selection = next.chapter as u16;
                } else {
                    next.chapter = wrap_prev(
                        self.chapter as u16,
                        (ctx.builtin_chapter_count as u16).max(1),
                    ) as u8;
                    next.selection = next.chapter as u16;
                    next.page = 0;
                }
            }
            (AppView::Reading, Some(Button::Confirm)) => {
                next.view = AppView::Chapters;
                // `chapter` already tracks the reading position (kept current
                // by the firmware's Loaded event, un-capped); opening the list
                // lands the cursor there rather than on the saturated guess.
                next.selection = self.chapter as u16;
            }
            (AppView::Reading, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 0;
            }
            (AppView::Chapters, Some(Button::Next | Button::PageNext)) => {
                next.selection = wrap_next(self.selection, self.chapter_item_count(ctx) as u16);
            }
            (AppView::Chapters, Some(Button::Previous | Button::PagePrevious)) => {
                next.selection = wrap_prev(self.selection, self.chapter_item_count(ctx) as u16);
            }
            (AppView::Chapters, Some(Button::Confirm)) => {
                next.chapter = self.selection as u8;
                next.page = if ReaderSource::from_book_id(self.book_id).is_sd() {
                    u32::from(
                        self.sd_chapter_pages
                            .get(self.selection as usize)
                            .copied()
                            .unwrap_or(0),
                    )
                } else {
                    0
                };
                next.view = AppView::Reading;
            }
            (AppView::Chapters, Some(Button::Back)) => {
                next.view = AppView::Reading;
            }
            (AppView::Wireless, Some(Button::Confirm)) => match self.sync_status {
                // NotConfigured starts too: with no stored or built-in
                // credentials the wifi task answers with the onboarding
                // portal instead of a station join.
                SyncStatus::NotConfigured | SyncStatus::Idle | SyncStatus::Error(_) => {
                    next.sync_status = SyncStatus::Starting;
                }
                // Confirm affirms the forget: the app shell deletes
                // /XTEINK/WIFI.BIN on this transition.
                SyncStatus::ForgetPending => {
                    next.wifi_ssid = [0; 32];
                    next.wifi_ssid_len = 0;
                    next.sync_status = SyncStatus::NotConfigured;
                }
                SyncStatus::CredentialsSaved | SyncStatus::Serving(_) => {
                    next.view = AppView::Home;
                    next.selection = 0;
                    next.sync_status = next.wireless_entry_status();
                }
                // An in-flight session ignores Confirm until it lands in
                // Serving, CredentialsSaved, or Error.
                _ => {}
            },
            (AppView::Wireless, Some(Button::Back)) => {
                // Back zooms out one level: a pending forget falls back to
                // the idle screen rather than leaving the view.
                if self.sync_status == SyncStatus::ForgetPending {
                    next.sync_status = SyncStatus::Idle;
                } else {
                    // Leaving after the radio started maps to
                    // SyncCommand::Exit in the app shell, which resets the
                    // device; the reducer still returns Home so the
                    // emulator stays navigable.
                    next.view = AppView::Home;
                    next.selection = 0;
                    next.sync_status = next.wireless_entry_status();
                }
            }
            (AppView::Wireless, Some(Button::Previous | Button::PagePrevious)) => {
                // The browse key doubles as "forget" while idle; the
                // destructive step still needs its Confirm.
                if self.sync_status == SyncStatus::Idle {
                    next.sync_status = SyncStatus::ForgetPending;
                }
            }
            (AppView::Wireless, Some(Button::Next | Button::PageNext)) => {}
            (AppView::Settings, Some(Button::Next | Button::PageNext)) => {
                next.selection = wrap_next(self.selection, SETTINGS_ITEMS as u16);
            }
            (AppView::Settings, Some(Button::Previous | Button::PagePrevious)) => {
                next.selection = wrap_prev(self.selection, SETTINGS_ITEMS as u16);
            }
            (AppView::Settings, Some(Button::Confirm)) => {
                next = apply_setting(next);
            }
            (AppView::Settings, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 0;
            }
        }

        // The sheet is a reading-surface state; leaving the page (or the
        // posture that summons it) always drops it.
        if next.view != AppView::Reading || !is_portrait(next.orientation) {
            next.reading_sheet = false;
        }

        next
    }

    pub fn apply_library_event(mut self, ctx: ReducerContext, event: LibraryEvent) -> Self {
        match event {
            LibraryEvent::Scanned { count } => {
                self.library_count = count;
                // Boot points at the built-in demo book until the scan
                // proves the card has real books; the title page then
                // adopts the first catalog entry instead of the
                // placeholder. Saved progress (Restored) arrives after
                // and overrides this default, and a demo book that is
                // actually open stays put.
                if count > 0
                    && !ReaderSource::from_book_id(self.book_id).is_sd()
                    && !matches!(self.view, AppView::Reading | AppView::Chapters)
                {
                    self.book_id = ReaderSource::sd(0).book_id();
                    self.chapter = 0;
                    self.page = 0;
                    self.dirty = Rect::FULL;
                }
                if self.view == AppView::Library {
                    if count == 0 {
                        self.selection = 0;
                    } else if self.selection >= count {
                        self.selection = count - 1;
                    }
                    self.dirty = Rect::FULL;
                    if self.read_request_pending {
                        self.read_request_pending = false;
                    }
                }
            }
            LibraryEvent::Loaded {
                book_id,
                pages,
                chapters,
                current_chapter,
                chapter_pages,
            } => {
                if self.book_id == book_id {
                    self.sd_page_count = pages.max(1);
                    self.sd_chapter_count = chapters.max(1);
                    self.sd_chapter_pages = chapter_pages;
                    self.page = self.page.min(self.sd_page_count.saturating_sub(1));
                    // The firmware owns the true current chapter over the whole
                    // book; adopt it so the cursor tracks past the cap that the
                    // page-turn recompute (sd_chapter_for_page) saturates at.
                    self.chapter = current_chapter.min(u8::MAX as u16) as u8;
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::ChapterPage {
                book_id,
                chapter,
                page,
            } => {
                if self.book_id == book_id {
                    if let Some(slot) = self.sd_chapter_pages.get_mut(chapter as usize) {
                        *slot = page.min(u16::MAX as u32) as u16;
                    }
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::ChapterCursor {
                book_id,
                current_chapter,
            } => {
                if self.book_id == book_id {
                    // Adopt the firmware's uncapped chapter silently. The Reading
                    // view shows page-within-chapter, not the chapter itself, so no
                    // repaint is needed -- Home/sleep/Chapters and the persisted
                    // position pick up the corrected value when next used.
                    self.chapter = current_chapter.min(u8::MAX as u16) as u8;
                }
            }
            LibraryEvent::CustomFont { available } => {
                self.custom_font_available = available;
                if !available && self.font_family == FontFamily::Custom {
                    self.font_family = FontFamily::Literata;
                }
                self.dirty = Rect::FULL;
            }
            LibraryEvent::Restored {
                book_id,
                chapter,
                page,
                page_count,
                reading_orientation,
                refresh_policy,
                font_size,
                line_spacing,
                font_weight,
                font_family,
                front_buttons,
            } => {
                self.book_id = book_id;
                self.chapter = chapter;
                self.page = page;
                // Give the Home progress bar a real denominator on wake, before
                // the book opens; the Loaded event refreshes it once read.
                if page_count > 0 {
                    self.sd_page_count = page_count;
                }
                if self.read_request_pending {
                    self.view = AppView::Reading;
                    self.selection = chapter as u16;
                } else if self.view == AppView::Library {
                    let restored_index =
                        ReaderSource::from_book_id(book_id).sd_index().unwrap_or(0);
                    self.selection = restored_index.min(self.library_count.saturating_sub(1));
                } else if self.view == AppView::Chapters {
                    // Home/Settings keep their own key selection; only the
                    // chapter list tracks the restored chapter cursor.
                    self.selection = chapter as u16;
                }
                self.read_request_pending = false;
                if let Some(orientation) = display_orientation_from_u8(reading_orientation) {
                    self.orientation = orientation;
                }
                if let Some(policy) = refresh_policy_from_u8(refresh_policy) {
                    self.refresh_policy = policy;
                }
                if let Some(size) = FontSize::from_u8(font_size) {
                    self.font_size = size;
                }
                if let Some(spacing) = LineSpacing::from_u8(line_spacing) {
                    self.line_spacing = spacing;
                }
                if let Some(weight) = FontWeight::from_u8(font_weight) {
                    self.font_weight = weight;
                }
                if let Some(family) = FontFamily::from_u8(font_family) {
                    self.font_family =
                        if family == FontFamily::Custom && !self.custom_font_available {
                            FontFamily::Literata
                        } else {
                            family
                        };
                }
                if let Some(front) = front_buttons_from_u8(front_buttons) {
                    self.front_buttons = front;
                }
                self.dirty = Rect::FULL;
            }
        }
        if self.view == AppView::Library {
            self.selection = self
                .selection
                .min(self.library_item_count(ctx).saturating_sub(1));
            self.dirty = Rect::FULL;
        }
        self
    }

    pub fn apply_sync_event(mut self, event: SyncEvent) -> Self {
        self.sync_status = match event {
            // The boot-time probe names the saved network without touching
            // the session; it only upgrades an untouched screen.
            SyncEvent::NetworkSaved(ssid) => {
                self.wifi_ssid = ssid.bytes;
                self.wifi_ssid_len = ssid.len;
                match self.sync_status {
                    SyncStatus::NotConfigured => SyncStatus::Idle,
                    status => status,
                }
            }
            SyncEvent::Connecting => SyncStatus::Connecting,
            SyncEvent::Connected(ip) => SyncStatus::Connected(ip),
            SyncEvent::PortalUp(psk) => SyncStatus::PortalUp(psk),
            SyncEvent::Serving(ip) => SyncStatus::Serving(ip),
            SyncEvent::CredentialsSaved(ssid) => {
                self.wifi_ssid = ssid.bytes;
                self.wifi_ssid_len = ssid.len;
                SyncStatus::CredentialsSaved
            }
            SyncEvent::Failed(error) => SyncStatus::Error(error),
        };
        self.dirty = Rect::FULL;
        self
    }

    /// What the Wireless screen shows on entry, before any session: the
    /// connect offer when a network is saved, the set-up offer otherwise.
    pub fn wireless_entry_status(&self) -> SyncStatus {
        if self.wifi_network_saved() {
            SyncStatus::Idle
        } else {
            SyncStatus::NotConfigured
        }
    }

    pub fn render_request(self, kind: RenderKind) -> RenderRequest {
        RenderRequest {
            kind,
            view: self.view,
            page: self.page,
            page_count: self.sd_page_count,
            chapter: self.chapter,
            selection: self.selection,
            book_id: self.book_id,
            orientation: self.orientation,
            front_buttons: self.front_buttons,
            reading_sheet: self.reading_sheet,
            refresh_policy: self.refresh_policy,
            font_size: self.font_size,
            line_spacing: self.line_spacing,
            font_weight: self.font_weight,
            font_family: self.font_family,
            last_button: self.last_button,
            aux_raw: self.aux_raw,
            nav_raw: self.nav_raw,
            page_raw: self.page_raw,
            battery_mv: self.battery_mv,
            battery_percent: self.battery_percent,
            library_count: self.library_count,
            sync_status: self.sync_status,
            wifi_ssid: self.wifi_ssid,
            wifi_ssid_len: self.wifi_ssid_len,
            dirty: self.dirty,
        }
    }

    pub fn persisted(self) -> PersistedAppState {
        PersistedAppState {
            book_id: self.book_id,
            chapter: self.chapter as u16,
            screen: self.page,
            shell_orientation: DisplayOrientation::PortraitButtonsLeft as u8,
            reading_orientation: self.orientation as u8,
            refresh_policy: self.refresh_policy as u8,
            font_size: self.font_size as u8,
            line_spacing: self.line_spacing as u8,
            font_weight: self.font_weight as u8,
            font_family: self.font_family as u8,
            front_buttons: self.front_buttons as u8,
            source_hash: 0,
            source_size: 0,
        }
    }

    pub fn type_settings(self) -> TypeSettings {
        TypeSettings {
            size: self.font_size,
            spacing: self.line_spacing,
            weight: self.font_weight,
            family: self.font_family,
        }
    }

    pub fn library_item_count(self, ctx: ReducerContext) -> u16 {
        self.library_count.max(ctx.builtin_book_count as u16).max(1)
    }

    pub fn chapter_item_count(self, ctx: ReducerContext) -> u8 {
        if ReaderSource::from_book_id(self.book_id).is_sd() {
            self.sd_chapter_count.max(1)
        } else {
            ctx.builtin_chapter_count.max(1)
        }
    }

    pub fn sd_chapter_for_page(self, page: u32) -> u8 {
        let mut selected = 0u8;
        for index in 0..self.sd_chapter_count.min(MAX_SD_CHAPTERS as u8) {
            if u32::from(self.sd_chapter_pages[index as usize]) <= page {
                selected = index;
            } else {
                break;
            }
        }
        selected
    }
}

/// Whether an orientation stands the panel's long axis upright. The two
/// portrait variants share one page geometry, so reading layout keys off
/// this rather than the exact variant.
pub fn is_portrait(orientation: DisplayOrientation) -> bool {
    matches!(
        orientation,
        DisplayOrientation::PortraitButtonsLeft | DisplayOrientation::PortraitButtonsRight
    )
}

pub fn display_orientation_from_u8(value: u8) -> Option<DisplayOrientation> {
    match value {
        0 => Some(DisplayOrientation::LandscapeButtonsBottom),
        1 => Some(DisplayOrientation::LandscapeButtonsTop),
        2 => Some(DisplayOrientation::PortraitButtonsLeft),
        3 => Some(DisplayOrientation::PortraitButtonsRight),
        _ => None,
    }
}

pub fn front_buttons_from_u8(value: u8) -> Option<FrontButtons> {
    match value {
        0 => Some(FrontButtons::PagesRight),
        1 => Some(FrontButtons::PagesLeft),
        _ => None,
    }
}

pub fn refresh_policy_from_u8(value: u8) -> Option<RefreshPolicy> {
    match value {
        0 => Some(RefreshPolicy::FastOnly),
        1 => Some(RefreshPolicy::FullOnWake),
        2 => Some(RefreshPolicy::FullEveryTen),
        _ => None,
    }
}

fn wrap_next(value: u16, len: u16) -> u16 {
    if value + 1 >= len {
        0
    } else {
        value + 1
    }
}

fn wrap_prev(value: u16, len: u16) -> u16 {
    if value == 0 {
        len - 1
    } else {
        value - 1
    }
}

fn home_action_for_button(button: Button) -> HomeAction {
    match button {
        // Home direct-maps the left-edge key column (top to bottom:
        // Back, Confirm, Previous, Next). Back zooms out of the book
        // onto the shelf; Confirm affirms continuing to read.
        Button::Back => HomeAction::Files,
        Button::Confirm => HomeAction::Read,
        Button::Previous | Button::PagePrevious => HomeAction::Wireless,
        Button::Next | Button::PageNext | Button::Power => HomeAction::Settings,
    }
}

fn apply_home_action(mut state: ReaderState, action: HomeAction) -> ReaderState {
    state.selection = 0;
    state.read_request_pending = false;
    match action {
        HomeAction::Read => {
            if ReaderSource::from_book_id(state.book_id).is_sd() {
                state.view = AppView::Reading;
                state.selection = state.chapter as u16;
            } else if state.library_count > 0 {
                state.view = AppView::Library;
            } else {
                state.view = AppView::Reading;
                state.book_id = 1;
            }
        }
        HomeAction::Files => {
            state.view = AppView::Library;
        }
        HomeAction::Wireless => {
            state.view = AppView::Wireless;
            state.sync_status = state.wireless_entry_status();
        }
        HomeAction::Settings => {
            state.view = AppView::Settings;
        }
    }
    state
}

/// Positional button semantics: after the device rotates, the button
/// sitting where Back used to be still acts as Back. The 180-degree flip
/// reverses both the front column and the side pair. The quarter turn to
/// portrait keeps everything: the front column reads left-to-right along
/// the bottom bezel in its natural order, and the side pair stands on end
/// with the forward key already at its natural end (hardware-walked
/// July 9 2026 — a swap here came out inverted on the device).
/// `PagesLeft` exchanges the two front pairs whole (back/confirm with
/// previous/next) before the orientation map, so the swap is a fact about
/// the physical buttons rather than the current hold. The side page rail
/// is untouched.
fn swap_front_pairs(front_buttons: FrontButtons, button: Option<Button>) -> Option<Button> {
    if front_buttons == FrontButtons::PagesRight {
        return button;
    }
    Some(match button? {
        Button::Back => Button::Previous,
        Button::Confirm => Button::Next,
        Button::Previous => Button::Back,
        Button::Next => Button::Confirm,
        other => other,
    })
}

fn orient_button(orientation: DisplayOrientation, button: Option<Button>) -> Option<Button> {
    let button = button?;
    Some(match orientation {
        DisplayOrientation::LandscapeButtonsTop => match button {
            Button::Power => Button::Power,
            Button::Back => Button::Next,
            Button::Confirm => Button::Previous,
            Button::Previous => Button::Confirm,
            Button::Next => Button::Back,
            Button::PagePrevious => Button::PageNext,
            Button::PageNext => Button::PagePrevious,
        },
        _ => button,
    })
}

/// Settings rows, top to bottom: the type block first (typeface, then its
/// size, weight, and spacing — broadest choice to finest adjustment), then
/// the set-and-forget display rows.
fn apply_setting(mut state: ReaderState) -> ReaderState {
    match state.selection {
        0 => {
            state.font_family = next_font_family(state.font_family, state.custom_font_available);
        }
        1 => {
            state.font_size = match state.font_size {
                FontSize::Small => FontSize::Medium,
                FontSize::Medium => FontSize::Large,
                FontSize::Large => FontSize::Small,
            };
        }
        2 => {
            state.font_weight = match state.font_weight {
                FontWeight::Normal => FontWeight::Heavy,
                FontWeight::Heavy => FontWeight::Normal,
            };
        }
        3 => {
            state.line_spacing = match state.line_spacing {
                LineSpacing::Compact => LineSpacing::Normal,
                LineSpacing::Normal => LineSpacing::Relaxed,
                LineSpacing::Relaxed => LineSpacing::Compact,
            };
        }
        4 => {
            state.refresh_policy = match state.refresh_policy {
                RefreshPolicy::FastOnly => RefreshPolicy::FullOnWake,
                RefreshPolicy::FullOnWake => RefreshPolicy::FullEveryTen,
                RefreshPolicy::FullEveryTen => RefreshPolicy::FastOnly,
            };
        }
        5 => {
            // Three holds are offered: the two landscapes and the one
            // portrait (front buttons below the screen). The buttons-above
            // portrait variant stays in the enum for the persistence format
            // but has no use case, so the cycle skips it.
            state.orientation = match state.orientation {
                DisplayOrientation::LandscapeButtonsBottom => {
                    DisplayOrientation::LandscapeButtonsTop
                }
                DisplayOrientation::LandscapeButtonsTop => DisplayOrientation::PortraitButtonsLeft,
                _ => DisplayOrientation::LandscapeButtonsBottom,
            };
        }
        6 => {
            state.front_buttons = match state.front_buttons {
                FrontButtons::PagesRight => FrontButtons::PagesLeft,
                FrontButtons::PagesLeft => FrontButtons::PagesRight,
            };
        }
        _ => {}
    }
    state
}

fn next_font_family(family: FontFamily, custom_available: bool) -> FontFamily {
    match (family, custom_available) {
        (FontFamily::Literata, _) => FontFamily::Merriweather,
        (FontFamily::Merriweather, true) => FontFamily::Custom,
        (FontFamily::Merriweather, false) => FontFamily::Literata,
        (FontFamily::Custom, _) => FontFamily::Literata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: ReducerContext = ReducerContext::new(1, 3);

    #[test]
    fn emulator_demo_psk_stays_within_the_mintable_alphabet() {
        let bytes = PortalPsk::EMULATOR_DEMO.bytes();
        assert_eq!(bytes.len(), PortalPsk::LEN);
        for b in bytes {
            assert!(
                PSK_ALPHABET.contains(&b),
                "EMULATOR_DEMO byte {:?} is outside PSK_ALPHABET",
                b as char
            );
        }
    }

    #[test]
    fn portal_psk_construction_refuses_bytes_outside_the_alphabet() {
        let valid = PortalPsk::EMULATOR_DEMO.bytes();
        assert_eq!(PortalPsk::new(valid), Some(PortalPsk::EMULATOR_DEMO));
        for bad in [b'0', b';', 0xFF] {
            let mut bytes = valid;
            bytes[0] = bad;
            assert_eq!(
                PortalPsk::new(bytes),
                None,
                "byte {bad:#04x} must be refused"
            );
        }
    }

    fn press(state: ReaderState, button: Button) -> ReaderState {
        state.apply_input(CTX, InputEvent::button(button))
    }

    #[test]
    fn home_navigation_opens_primary_views() {
        assert_eq!(
            press(ReaderState::boot(), Button::Confirm).view,
            AppView::Reading
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Back).view,
            AppView::Library
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Previous).view,
            AppView::Wireless
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Next).view,
            AppView::Settings
        );
    }

    fn with_saved_network(state: ReaderState) -> ReaderState {
        state.apply_sync_event(SyncEvent::NetworkSaved(
            WifiSsid::new("latent.space").unwrap(),
        ))
    }

    #[test]
    fn wireless_without_saved_network_starts_the_portal_flow() {
        let state = press(ReaderState::boot(), Button::Previous);
        assert_eq!(state.view, AppView::Wireless);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        let state = press(state, Button::Confirm);
        assert_eq!(state.sync_status, SyncStatus::Starting);
        let state = state.apply_sync_event(SyncEvent::PortalUp(PortalPsk::EMULATOR_DEMO));
        assert_eq!(
            state.sync_status,
            SyncStatus::PortalUp(PortalPsk::EMULATOR_DEMO)
        );
        // Confirm is inert while the portal serves.
        let state = press(state, Button::Confirm);
        assert_eq!(
            state.sync_status,
            SyncStatus::PortalUp(PortalPsk::EMULATOR_DEMO)
        );
        let state = state.apply_sync_event(SyncEvent::CredentialsSaved(
            WifiSsid::new("latent.space").unwrap(),
        ));
        assert_eq!(state.sync_status, SyncStatus::CredentialsSaved);
        // The portal's capture names the network for the rest of the boot.
        assert_eq!(state.wifi_ssid(), "latent.space");
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn sync_serving_state_follows_connect_and_back_exits() {
        let state = with_saved_network(ReaderState::boot());
        let state = press(press(state, Button::Previous), Button::Confirm)
            .apply_sync_event(SyncEvent::Connected([192, 168, 0, 233]))
            .apply_sync_event(SyncEvent::Serving([192, 168, 0, 233]));
        assert_eq!(state.sync_status, SyncStatus::Serving([192, 168, 0, 233]));
        // The screen labels Confirm "done" while serving, so it must exit
        // exactly like Back does (the wifi task defers the reset past any
        // in-flight transfer either way).
        let confirmed = press(state, Button::Confirm);
        assert_eq!(confirmed.view, AppView::Home);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn boot_network_probe_names_the_saved_network() {
        let state = with_saved_network(ReaderState::boot());
        assert_eq!(state.wifi_ssid(), "latent.space");
        let state = press(state, Button::Previous);
        assert_eq!(state.view, AppView::Wireless);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn boot_network_probe_upgrades_an_open_wireless_screen() {
        // The probe races screen entry only when the user opens Wireless
        // within the first seconds of boot; the screen upgrades in place.
        let state = press(ReaderState::boot(), Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        let state = with_saved_network(state);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn forget_needs_its_confirm_and_clears_the_network() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        let state = press(state, Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::ForgetPending);
        // Back cancels without leaving the screen or the network.
        let cancelled = press(state, Button::Back);
        assert_eq!(cancelled.view, AppView::Wireless);
        assert_eq!(cancelled.sync_status, SyncStatus::Idle);
        assert!(cancelled.wifi_network_saved());
        // Confirm forgets: the screen falls back to the set-up offer.
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Wireless);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        assert!(!state.wifi_network_saved());
    }

    #[test]
    fn forget_is_unreachable_without_a_saved_network() {
        let state = press(ReaderState::boot(), Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        let state = press(state, Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
    }

    #[test]
    fn wifi_credentials_round_trip_strs() {
        let creds = WifiCredentials::from_strs("latent.space", "a&b c/9").unwrap();
        assert_eq!(creds.ssid(), "latent.space");
        assert_eq!(creds.password(), "a&b c/9");
        assert!(WifiCredentials::from_strs("", "x").is_none());
        assert!(WifiCredentials::from_strs("123456789012345678901234567890123", "x").is_none());
    }

    #[test]
    fn sync_with_saved_network_starts_on_confirm_and_tracks_events() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::Idle);
        let state = press(state, Button::Confirm);
        assert_eq!(state.sync_status, SyncStatus::Starting);

        let state = state.apply_sync_event(SyncEvent::Connecting);
        assert_eq!(state.sync_status, SyncStatus::Connecting);
        // In-flight Confirm presses are ignored.
        let held = press(state, Button::Confirm);
        assert_eq!(held.sync_status, SyncStatus::Connecting);
        let state = state.apply_sync_event(SyncEvent::Connected([192, 168, 1, 23]));
        assert_eq!(state.sync_status, SyncStatus::Connected([192, 168, 1, 23]));
        let state = state.apply_sync_event(SyncEvent::Serving([192, 168, 1, 23]));

        // The done press returns Home with the entry status restored.
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Home);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn sync_error_can_be_retried_with_confirm() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        let state =
            press(state, Button::Confirm).apply_sync_event(SyncEvent::Failed(SyncError::Join));
        assert_eq!(state.sync_status, SyncStatus::Error(SyncError::Join));
        let state = press(state, Button::Confirm);
        assert_eq!(state.sync_status, SyncStatus::Starting);
    }

    #[test]
    fn sync_back_returns_home_and_resets_status() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        let state = press(state, Button::Confirm).apply_sync_event(SyncEvent::Connecting);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn reader_source_maps_sd_catalog_indices_to_book_ids() {
        assert_eq!(
            ReaderSource::from_book_id(1),
            ReaderSource::BuiltIn { book_id: 1 }
        );
        assert_eq!(ReaderSource::sd(0).book_id(), 2);
        assert_eq!(ReaderSource::sd(7).book_id(), 9);
        assert_eq!(ReaderSource::from_book_id(9).sd_index(), Some(7));
    }

    #[test]
    fn library_open_key_opens_sd_book() {
        let state = press(ReaderState::boot(), Button::Back)
            .apply_library_event(CTX, LibraryEvent::Scanned { count: 2 });
        let state = press(press(state, Button::Next), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.book_id, 3);
    }

    #[test]
    fn library_back_key_returns_home_without_opening() {
        let state = press(ReaderState::boot(), Button::Back)
            .apply_library_event(CTX, LibraryEvent::Scanned { count: 2 });
        let state = press(press(state, Button::Next), Button::Back);
        assert_eq!(state.view, AppView::Home);
        // Browsing did not open anything: the active book is still the
        // scan-time default (first catalog entry), not the browsed row.
        assert_eq!(state.book_id, ReaderSource::sd(0).book_id());
    }

    #[test]
    fn reading_next_previous_bounds_sd_pages() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = 2;
        state.sd_page_count = 2;
        assert_eq!(press(state, Button::Next).page, 1);
        assert_eq!(press(press(state, Button::Next), Button::Next).page, 1);
        assert_eq!(press(press(state, Button::Next), Button::Previous).page, 0);
    }

    #[test]
    fn chapter_selection_changes_reading_chapter() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = 1;
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Chapters);
        let state = press(press(state, Button::Next), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.chapter, 1);
    }

    #[test]
    fn sd_chapter_selection_uses_toc_page_target() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(0).book_id();
        state.sd_page_count = 40;
        state.sd_chapter_count = 3;
        state.sd_chapter_pages[0] = 0;
        state.sd_chapter_pages[1] = 12;
        state.sd_chapter_pages[2] = 24;

        let state = press(state, Button::Confirm);
        let state = press(press(state, Button::Next), Button::Confirm);

        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.chapter, 1);
        assert_eq!(state.page, 12);
    }

    #[test]
    fn sd_page_navigation_tracks_chapter_without_wrapping_pages() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(0).book_id();
        state.page = 11;
        state.sd_page_count = 40;
        state.sd_chapter_count = 3;
        state.sd_chapter_pages[0] = 0;
        state.sd_chapter_pages[1] = 12;
        state.sd_chapter_pages[2] = 24;

        let state = press(state, Button::Next);

        assert_eq!(state.page, 12);
        assert_eq!(state.chapter, 1);
    }

    #[test]
    fn sd_page_navigation_tracks_chapters_past_first_screen() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(0).book_id();
        state.sd_page_count = 400;
        state.sd_chapter_count = 40;
        for index in 0..40 {
            state.sd_chapter_pages[index] = (index as u16) * 10;
        }
        state.page = 249;
        state.chapter = 23;

        let state = press(state, Button::Next);

        assert_eq!(state.page, 250);
        assert_eq!(state.chapter, 25);
        assert_eq!(state.selection, 25);
    }

    #[test]
    fn catalog_scan_does_not_auto_open_from_files() {
        let state = press(ReaderState::boot(), Button::Back);
        assert_eq!(state.view, AppView::Library);
        assert!(!state.read_request_pending);

        let state = state.apply_library_event(CTX, LibraryEvent::Scanned { count: 2 });
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.library_count, 2);
        assert!(!state.read_request_pending);
    }

    #[test]
    fn scan_defaults_home_to_first_sd_book_until_restore() {
        let state = ReaderState::boot();
        assert_eq!(state.book_id, 1);

        let state = state.apply_library_event(CTX, LibraryEvent::Scanned { count: 3 });
        assert_eq!(state.book_id, ReaderSource::sd(0).book_id());
        assert_eq!(state.chapter, 0);
        assert_eq!(state.page, 0);

        // Saved progress arriving after the scan wins over the default.
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: ReaderSource::sd(2).book_id(),
                chapter: 4,
                page: 12,
                page_count: 0,
                reading_orientation: DisplayOrientation::LandscapeButtonsBottom as u8,
                refresh_policy: RefreshPolicy::FullOnWake as u8,
                font_size: FontSize::Medium as u8,
                line_spacing: LineSpacing::Normal as u8,
                font_weight: FontWeight::Normal as u8,
                font_family: FontFamily::Literata as u8,
                front_buttons: FrontButtons::PagesRight as u8,
            },
        );
        assert_eq!(state.book_id, ReaderSource::sd(2).book_id());

        // A later rescan must not yank the restored book back.
        let state = state.apply_library_event(CTX, LibraryEvent::Scanned { count: 3 });
        assert_eq!(state.book_id, ReaderSource::sd(2).book_id());
    }

    #[test]
    fn scan_keeps_an_open_builtin_book() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        let state = state.apply_library_event(CTX, LibraryEvent::Scanned { count: 3 });
        assert_eq!(state.book_id, 1);
    }

    #[test]
    fn restore_keeps_home_key_selection() {
        let state = ReaderState::boot();
        let home_selection = state.selection;
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: ReaderSource::sd(1).book_id(),
                chapter: 9,
                page: 70,
                page_count: 0,
                reading_orientation: DisplayOrientation::LandscapeButtonsBottom as u8,
                refresh_policy: RefreshPolicy::FullOnWake as u8,
                font_size: FontSize::Medium as u8,
                line_spacing: LineSpacing::Normal as u8,
                font_weight: FontWeight::Normal as u8,
                font_family: FontFamily::Literata as u8,
                front_buttons: FrontButtons::PagesRight as u8,
            },
        );
        assert_eq!(state.selection, home_selection);
        assert_eq!(state.chapter, 9);
        assert_eq!(state.page, 70);
    }

    #[test]
    fn library_open_before_scan_stays_in_files() {
        let state = press(ReaderState::boot(), Button::Back);
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.book_id, 1);

        let state = state.apply_library_event(CTX, LibraryEvent::Scanned { count: 2 });
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.library_count, 2);
    }

    #[test]
    fn settings_change_key_cycles_refresh_policy() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state.selection = 4;
        let state = press(state, Button::Confirm);
        assert_eq!(state.refresh_policy, RefreshPolicy::FullEveryTen);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn settings_change_key_cycles_type_size_spacing_and_weight() {
        let state = press(ReaderState::boot(), Button::Next);
        assert_eq!(state.selection, 0);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Merriweather);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Literata);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 1);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_size, FontSize::Large);
        let state = press(press(state, Button::Confirm), Button::Confirm);
        assert_eq!(state.font_size, FontSize::Medium);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 2);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_weight, FontWeight::Heavy);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_weight, FontWeight::Normal);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 3);
        let state = press(state, Button::Confirm);
        assert_eq!(state.line_spacing, LineSpacing::Relaxed);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 4);
        let state = press(state, Button::Next);
        assert_eq!(state.selection, 5);
        let state = press(state, Button::Next);
        assert_eq!(state.selection, 6);
        let state = press(state, Button::Next);
        assert_eq!(state.selection, 0, "selection wraps after the last row");
    }

    #[test]
    fn settings_change_key_toggles_front_buttons() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state.selection = 6;

        let state = press(state, Button::Confirm);
        assert_eq!(state.front_buttons, FrontButtons::PagesLeft);

        // With the pairs swapped, the physical Next key (second key of the
        // pair now holding back/confirm) carries Confirm's change action,
        // and toggles the setting back.
        let state = press(state, Button::Next);
        assert_eq!(state.front_buttons, FrontButtons::PagesRight);
    }

    #[test]
    fn pages_left_swaps_the_front_pairs_whole() {
        let mut state = ReaderState::boot();
        state.front_buttons = FrontButtons::PagesLeft;
        state.view = AppView::Reading;

        // Reading: the physical back/confirm pair now turns pages (order
        // kept within the pair), and the old page pair carries back/confirm.
        state.book_id = ReaderSource::sd(0).book_id();
        state.sd_page_count = 10;
        state.page = 5;
        assert_eq!(press(state, Button::Back).page, 4);
        assert_eq!(press(state, Button::Confirm).page, 6);
        assert_eq!(press(state, Button::Previous).view, AppView::Home);
        assert_eq!(press(state, Button::Next).view, AppView::Chapters);

        // The side page rail is untouched.
        assert_eq!(press(state, Button::PageNext).page, 6);
    }

    #[test]
    fn home_ignores_the_front_pair_swap() {
        // Home is positional: the same physical key opens the same view
        // whether or not the pairs are swapped, so the title page reads
        // identically for every user.
        let mut state = ReaderState::boot();
        state.front_buttons = FrontButtons::PagesLeft;

        assert_eq!(press(state, Button::Back).view, AppView::Library);
        assert_eq!(press(state, Button::Confirm).view, AppView::Reading);
        assert_eq!(press(state, Button::Previous).view, AppView::Wireless);
        assert_eq!(press(state, Button::Next).view, AppView::Settings);
    }

    #[test]
    fn settings_change_key_cycles_the_three_offered_orientations() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state.selection = 5;

        let state = press(state, Button::Confirm);
        assert_eq!(state.orientation, DisplayOrientation::LandscapeButtonsTop);

        // Rotated 180 degrees, the physical Previous key sits where Confirm
        // was, so it carries the change action.
        let state = press(state, Button::Previous);
        assert_eq!(state.orientation, DisplayOrientation::PortraitButtonsLeft);

        // Portrait keeps the front column's order: Confirm stays Confirm,
        // and the cycle wraps back to the default hold (skipping the
        // unoffered buttons-above portrait).
        let state = press(state, Button::Confirm);
        assert_eq!(
            state.orientation,
            DisplayOrientation::LandscapeButtonsBottom
        );
    }

    #[test]
    fn portrait_keeps_all_physical_buttons() {
        let mut state = ReaderState::boot();
        state.orientation = DisplayOrientation::PortraitButtonsLeft;

        assert_eq!(press(state, Button::Back).view, AppView::Library);
        assert_eq!(press(state, Button::Confirm).view, AppView::Reading);

        // The side pair keeps its physical sense: the hardware walk showed
        // the forward key already lands at its natural end in portrait.
        let mut library = press(state, Button::Back);
        library.library_count = 3;
        let next = press(library, Button::PageNext);
        assert_eq!(next.selection, 1);
        let previous = press(next, Button::PagePrevious);
        assert_eq!(previous.selection, 0);
    }

    #[test]
    fn portrait_reading_summons_the_sheet_before_acting() {
        let mut state = press(ReaderState::boot(), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        state.orientation = DisplayOrientation::PortraitButtonsLeft;

        // First named-key press summons; the second acts on its label.
        let state = press(state, Button::Confirm);
        assert!(state.reading_sheet);
        assert_eq!(state.view, AppView::Reading, "summoning is not an action");
        let state = press(state, Button::Confirm);
        assert!(!state.reading_sheet);
        assert_eq!(state.view, AppView::Chapters);

        // Back out of Chapters returns to a sheetless page; Back then
        // summons, and a second Back leaves for Home.
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Reading);
        assert!(!state.reading_sheet);
        let state = press(state, Button::Back);
        assert!(state.reading_sheet);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
        assert!(!state.reading_sheet);
    }

    #[test]
    fn portrait_page_turns_never_wait_on_the_sheet() {
        let mut state = press(ReaderState::boot(), Button::Confirm);
        state.orientation = DisplayOrientation::PortraitButtonsLeft;

        // The browse pair pages immediately — no summon toll.
        let chapter_before = state.chapter;
        let state = press(state, Button::Next);
        assert!(!state.reading_sheet);
        assert_ne!(state.chapter, chapter_before, "next paged immediately");

        // And a page turn dismisses an up sheet.
        let state = press(state, Button::Confirm);
        assert!(state.reading_sheet);
        let state = press(state, Button::Next);
        assert!(!state.reading_sheet);
        assert_eq!(state.view, AppView::Reading);
    }

    #[test]
    fn landscape_reading_keeps_direct_key_mappings() {
        let state = press(ReaderState::boot(), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);

        // No sheet in landscape: Confirm opens Chapters on the first press.
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Chapters, "no sheet in landscape");
        assert!(!state.reading_sheet);
    }

    #[test]
    fn landscape_top_rotates_front_button_mapping() {
        let mut state = ReaderState::boot();
        state.orientation = DisplayOrientation::LandscapeButtonsTop;

        assert_eq!(press(state, Button::Back).view, AppView::Settings);
        assert_eq!(press(state, Button::Confirm).view, AppView::Wireless);
        assert_eq!(press(state, Button::Previous).view, AppView::Reading);
        assert_eq!(press(state, Button::Next).view, AppView::Library);
    }

    #[test]
    fn landscape_top_swaps_page_buttons() {
        let mut state = press(ReaderState::boot(), Button::Back);
        state.library_count = 3;
        state.orientation = DisplayOrientation::LandscapeButtonsTop;

        let next = press(state, Button::PagePrevious);
        assert_eq!(next.selection, 1);

        let previous = press(next, Button::PageNext);
        assert_eq!(previous.selection, 0);
    }

    #[test]
    fn settings_typeface_cycles_custom_only_when_available() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state = state.apply_library_event(CTX, LibraryEvent::CustomFont { available: true });
        assert_eq!(state.selection, 0);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Merriweather);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Custom);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Literata);
    }

    #[test]
    fn removing_custom_font_falls_back_to_literata() {
        let mut state = ReaderState::boot();
        state.custom_font_available = true;
        state.font_family = FontFamily::Custom;
        let state = state.apply_library_event(CTX, LibraryEvent::CustomFont { available: false });
        assert_eq!(state.font_family, FontFamily::Literata);
        assert!(!state.custom_font_available);
    }

    #[test]
    fn library_restore_updates_progress_and_preferences() {
        let state = ReaderState::boot().apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: 2,
                chapter: 4,
                page: 12,
                page_count: 0,
                reading_orientation: DisplayOrientation::PortraitButtonsRight as u8,
                refresh_policy: RefreshPolicy::FastOnly as u8,
                font_size: FontSize::Large as u8,
                line_spacing: LineSpacing::Compact as u8,
                font_weight: FontWeight::Normal as u8,
                font_family: FontFamily::Literata as u8,
                front_buttons: FrontButtons::PagesRight as u8,
            },
        );
        assert_eq!(state.book_id, 2);
        assert_eq!(state.chapter, 4);
        assert_eq!(state.page, 12);
        assert_eq!(state.orientation, DisplayOrientation::PortraitButtonsRight);
        assert_eq!(state.refresh_policy, RefreshPolicy::FastOnly);
        assert_eq!(state.font_size, FontSize::Large);
        assert_eq!(state.line_spacing, LineSpacing::Compact);
    }

    #[test]
    fn refresh_plan_cleans_after_type_settings_change() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Settings;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        state.font_size = FontSize::Large;
        assert_eq!(
            planner.mode_for(state.render_request(RenderKind::Page)),
            RefreshMode::FastClean
        );
    }

    #[test]
    fn refresh_plan_cleans_after_orientation_change() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Settings;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        state.orientation = DisplayOrientation::LandscapeButtonsTop;
        assert_eq!(
            planner.mode_for(state.render_request(RenderKind::Page)),
            RefreshMode::FastClean
        );
    }

    #[test]
    fn refresh_plan_uses_fast_clean_for_context_changes_and_fast_for_selection() {
        let mut planner = RefreshPlanner::new();
        let mut request = ReaderState::boot().render_request(RenderKind::Boot);

        // Cold boot is the only render where panel contents are unknown,
        // so it keeps the deep multi-flash full waveform.
        assert_eq!(planner.mode_for(request), RefreshMode::Full);
        planner.record_render(request, RefreshMode::Full);

        request.kind = RenderKind::Page;
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);

        request.view = AppView::Settings;
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
        planner.record_render(request, RefreshMode::FastClean);

        // Cursor moves inside Settings ride the fast differential refresh
        // against the prestaged previous frame; leaving the view is a view
        // change, which gets the one-flicker cleaning refresh.
        request.selection = 1;
        assert_eq!(planner.mode_for(request), RefreshMode::Fast);
        planner.record_render(request, RefreshMode::Fast);

        request.view = AppView::Home;
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
    }

    #[test]
    fn refresh_plan_keeps_library_selection_fast() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Library;
        state.library_count = 3;
        let mut request = state.render_request(RenderKind::Page);

        planner.record_render(request, RefreshMode::Full);
        request.selection = 1;

        assert_eq!(planner.mode_for(request), RefreshMode::Fast);
    }

    #[test]
    fn refresh_plan_keeps_chapter_selection_fast() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Chapters;
        let mut request = state.render_request(RenderKind::Page);

        planner.record_render(request, RefreshMode::Full);
        request.selection = 1;

        assert_eq!(planner.mode_for(request), RefreshMode::Fast);
    }

    #[test]
    fn refresh_plan_counts_fast_refreshes_and_resets_on_sleep() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.refresh_policy = RefreshPolicy::FullEveryTen;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        for _ in 0..DEFAULT_FULL_REFRESH_INTERVAL {
            assert_eq!(planner.mode_for(request), RefreshMode::Fast);
            planner.record_render(request, RefreshMode::Fast);
        }
        // Periodic mid-reading cleanup uses the one-flicker clean instead
        // of the jarring multi-flash full waveform.
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);

        // After a display sleep the panel shows the sleep screen the
        // firmware drew, so wake also needs only the one-flicker clean.
        planner.record_sleep(true);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
    }

    #[test]
    fn refresh_plan_keeps_deep_full_for_cold_boot_only() {
        let mut planner = RefreshPlanner::new();
        let request = ReaderState::boot().render_request(RenderKind::Boot);

        // Cold boot: unknown panel contents, deep full waveform.
        assert_eq!(planner.mode_for(request), RefreshMode::Full);
        planner.record_render(request, RefreshMode::Full);

        // Wake after sleep: known sleep-screen contents, one-flicker clean.
        planner.record_sleep(true);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
        planner.record_render(request, RefreshMode::FastClean);
        planner.record_sleep(true);

        // A sleep whose final flush failed still powers the panel down, so
        // the screen is off and the next render re-inits — but the pixels
        // underneath are stale, and the wake render must pay the deep full
        // waveform instead of fast-cleaning over them.
        let mut failed_flush = RefreshPlanner::new();
        failed_flush.record_render(request, RefreshMode::Full);
        failed_flush.record_sleep(false);
        assert!(!failed_flush.screen_on());
        assert_eq!(failed_flush.last_request(), None);
        assert_eq!(failed_flush.mode_for(request), RefreshMode::Full);

        // Disabling fast refresh falls back to the deep full everywhere.
        let conservative = RefreshPlanner::new().with_fast_refresh_enabled(false);
        assert_eq!(conservative.mode_for(request), RefreshMode::Full);
    }

    #[test]
    fn refresh_plan_seeded_deep_sleep_wake_uses_fast_clean() {
        let request = ReaderState::boot().render_request(RenderKind::Boot);

        // A deep-sleep wake is a cold boot with a fresh planner, but the
        // panel still shows the sleep screen the firmware drew before
        // powering down; the seed lets the wake render take the one-flicker
        // clean instead of the multi-flash full waveform.
        let mut planner = RefreshPlanner::new().with_panel_shows_sleep_screen(true);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);

        // The seed is consumed by the first render: from here the planner
        // behaves exactly like an in-session one — the post-boot cleanup
        // pass, then fast differentials for same-context turns.
        planner.record_render(request, RefreshMode::FastClean);
        let mut page = request;
        page.kind = RenderKind::Page;
        assert_eq!(planner.mode_for(page), RefreshMode::FastClean);
        planner.record_render(page, RefreshMode::FastClean);
        page.selection = 1;
        assert_eq!(planner.mode_for(page), RefreshMode::Fast);

        // An unseeded cold boot (battery pull, crash, software reset) still
        // pays the deep full waveform — panel contents are unknown.
        assert_eq!(
            RefreshPlanner::new()
                .with_panel_shows_sleep_screen(false)
                .mode_for(request),
            RefreshMode::Full
        );

        // With fast refresh disabled the seed is ignored.
        let conservative = RefreshPlanner::new()
            .with_panel_shows_sleep_screen(true)
            .with_fast_refresh_enabled(false);
        assert_eq!(conservative.mode_for(request), RefreshMode::Full);
    }

    /// One of every StorageCommand variant, so the admission table below is
    /// exhaustive by construction: a new variant fails the count assertion
    /// until it is classified here.
    fn every_storage_command() -> [StorageCommand; 10] {
        let persisted = PersistedAppState {
            book_id: 0,
            chapter: 0,
            screen: 0,
            shell_orientation: 0,
            reading_orientation: 0,
            refresh_policy: 0,
            font_size: 0,
            line_spacing: 0,
            font_weight: 0,
            font_family: 0,
            front_buttons: 0,
            source_hash: 0,
            source_size: 0,
        };
        let credentials = WifiCredentials::from_strs("ssid", "pass").unwrap();
        [
            StorageCommand::LoadCatalogCache,
            StorageCommand::RefreshCatalog,
            StorageCommand::OpenBook {
                request_id: 1,
                book_id: 1,
                index: 0,
                chapter: 0,
                target_pages: 0,
                type_settings: TypeSettings::DEFAULT,
                portrait: false,
            },
            StorageCommand::ExtendSection {
                request_id: 1,
                book_id: 1,
                index: 0,
                chapter: 0,
                target_pages: 0,
                type_settings: TypeSettings::DEFAULT,
                portrait: false,
            },
            StorageCommand::LoadChapters {
                request_id: 1,
                book_id: 1,
                index: 0,
            },
            StorageCommand::JumpChapter {
                request_id: 1,
                book_id: 1,
                index: 0,
                chapter: 0,
                type_settings: TypeSettings::DEFAULT,
                portrait: false,
            },
            StorageCommand::StoreProgress(persisted),
            StorageCommand::LoanSyncMemory,
            StorageCommand::StoreWifiCredentials(credentials),
            StorageCommand::ForgetWifiCredentials,
        ]
    }

    #[test]
    fn idle_sync_session_admits_everything_but_upload() {
        let session = SyncSession::Idle;
        assert!(!session.active());
        for command in every_storage_command() {
            assert!(session.admits(&command), "refused idle: {command:?}");
        }
        // Uploads only exist while the browser shelf is being served.
        assert!(!session.admits(&StorageCommand::ReceiveUpload));
    }

    #[test]
    fn loaned_sync_session_admits_only_loan_safe_commands() {
        let mut session = SyncSession::default();
        session.loan_granted();
        assert!(session.active());
        let mut admitted = 0;
        for command in every_storage_command() {
            let loan_safe = matches!(
                command,
                StorageCommand::StoreProgress(_) | StorageCommand::StoreWifiCredentials(_)
            );
            assert_eq!(
                session.admits(&command),
                loan_safe,
                "wrong admission while loaned: {command:?}"
            );
            admitted += usize::from(loan_safe);
        }
        assert_eq!(admitted, 2);
        assert!(session.admits(&StorageCommand::ReceiveUpload));
        // Notably refused: a second loan of memory that is already gone.
        assert!(!session.admits(&StorageCommand::LoanSyncMemory));
    }
}
