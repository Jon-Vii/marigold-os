//! The next page, drawn and loaded into the panel ahead of the press that
//! asks for it (X4 only).
//!
//! A page turn used to render the page (about 40 ms) and stream it into
//! BW RAM (16 ms) between the press and the start of the 421 ms waveform.
//! Once a reading page has settled, and RED RAM holds it again, the app
//! task predicts the render a Next press would send
//! (`ReaderState::next_page_request`) and offers it with
//! `DisplayCommand::StageNext`. The display task draws that page into `fb`
//! and writes it to BW RAM, the frame on screen staying in `prev_fb` and
//! RED RAM. When the press's render arrives and is for the same frame, the
//! task starts the refresh at once (`display_flush::flush_staged`).
//!
//! The same frame means three things, all checked when the render arrives:
//!
//! - The request's [`app_core::ReadingPageKey`] equals the staged one.
//!   Those are the only request fields the page reads; the rest differ
//!   from press to press.
//! - The store's [`ReaderStore::fingerprint`] equals the one taken when
//!   the page was staged, so nothing the page reads from the store has
//!   changed: not a page, block, text byte, layout, book index, seed run,
//!   status, or font pack. This is what makes every store mutation (an
//!   idle preparation step, an image decode, a catalog or chapter load, a
//!   relayout, a different book) invalidate the stage without having to be
//!   listed, and it is taken after the render's own window loads, so it
//!   sees the store that render would draw from.
//! - `fb` and BW RAM still hold what staging wrote. They are written only
//!   by renders, the loading plate and the sleep screen, each of which
//!   drops the stage ([`NextPage::forget`] or [`NextPage::take`]), and by
//!   staging itself.
//!
//! Given those, the press's render would draw the frame staging drew: it
//! is the same code (`views::render` draws a reading page through
//! `views::render_sd_reading_while`) over the same inputs, and a page is
//! only staged when that code draws it from RAM alone
//! (`views::page_can_be_staged`).
//!
//! Staging never holds up a press for long. It starts only when no
//! command is queued, and gives way to any press counted since the app
//! made its prediction: the draw asks before each line, the BW write before
//! each band (and also stops for any queued command). A press is therefore
//! handled within a line's draw, a band's transfer, or one store
//! fingerprint (`bench: stage digest_us`) of when it would be otherwise.
//! The input task runs at interrupt priority, so the count moves even while
//! the draw holds the thread executor. An abandoned stage leaves BW RAM
//! part written, which no flush relies on.
//!
//! Idle preparation changes the store. After each idle step the staged
//! page is checked against the store and dropped if the store moved, so a
//! press during preparation finds nothing staged and pays nothing for a
//! check that would fail. The page is drawn again once preparation has
//! nothing left to do, not after every step, each of which could undo it.

use crate::display_flush::{self, Epd};
use crate::reader_store::ReaderStore;
use crate::{BUTTON_PRESSES, DISPLAY_COMMANDS, STORAGE_COMMANDS};
use app_core::{ReadingPageKey, RenderRequest};
use core::sync::atomic::Ordering;
use display::fb::Framebuffer;
use display::BAND_BYTES;
use embassy_time::Instant;

/// The render the app last offered and the press count it was made at.
#[derive(Clone, Copy)]
struct Offer {
    request: RenderRequest,
    presses: Presses,
}

/// A `BUTTON_PRESSES` reading.
#[derive(Clone, Copy)]
struct Presses(u32);

impl Presses {
    /// No press since this reading.
    fn current(self) -> bool {
        BUTTON_PRESSES.load(Ordering::Relaxed) == self.0
    }
}

/// What `fb` and BW RAM hold while a page is staged.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Staged {
    page: ReadingPageKey,
    store: u64,
}

/// How a render used what was staged, for the `bench: render` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageUse {
    /// `fb` and BW RAM hold the frame: no render, no BW write.
    Hit,
    /// Nothing was staged.
    Nothing,
    /// Another page was staged.
    OtherPage,
    /// The page was staged, but the store has changed since.
    StoreChanged,
}

impl StageUse {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Nothing => "none",
            Self::OtherPage => "other_page",
            Self::StoreChanged => "store_changed",
        }
    }
}

pub(crate) struct NextPage {
    offer: Option<Offer>,
    staged: Option<Staged>,
    /// Idle preparation has run since the offer: stage again only once it
    /// has finished.
    after_idle: bool,
}

impl NextPage {
    pub(crate) const fn new() -> Self {
        Self {
            offer: None,
            staged: None,
            after_idle: false,
        }
    }

    /// `DisplayCommand::StageNext`.
    pub(crate) fn offer(&mut self, request: RenderRequest, presses: u32) {
        self.offer = request.reading_page_key().is_some().then_some(Offer {
            request,
            presses: Presses(presses),
        });
        self.after_idle = false;
    }

    /// `fb` or BW RAM now hold something else (the loading plate, the
    /// sleep screen), or the reader has left: drop the stage and the offer.
    pub(crate) fn forget(&mut self) {
        self.offer = None;
        self.staged = None;
    }

    /// An idle preparation step ran and may have changed the store. Drop
    /// the staged page if it did, now rather than on the press: a press
    /// then finds nothing staged and pays nothing for the check. A press
    /// already counted drops it without the check, which would only delay
    /// that press.
    pub(crate) fn idle_ran(&mut self, store: &ReaderStore) {
        self.after_idle = true;
        let Some(staged) = self.staged else {
            return;
        };
        if !self.offer.is_some_and(|offer| offer.presses.current()) {
            self.forget();
        } else if store.fingerprint() != staged.store {
            self.staged = None;
        }
    }

    /// Whether there is staging to do once the task is quiet: an offer not
    /// yet staged. Not while idle preparation that has begun since the
    /// offer still has steps to run, since each could undo it.
    pub(crate) fn due(&self, idle_pending: bool) -> bool {
        let Some(offer) = self.offer else {
            return false;
        };
        self.staged.map(|staged| staged.page) != offer.request.reading_page_key()
            && !(self.after_idle && idle_pending)
    }

    /// A render arrived: whether `fb` and BW RAM already hold the frame it
    /// would draw. Either way the stage and the offer are spent, since the
    /// render's flush (or its own draw) replaces them. `panel_ready` says
    /// the controller is initialised, so its RAM is what staging left.
    pub(crate) fn take(
        &mut self,
        request: RenderRequest,
        store: &ReaderStore,
        panel_ready: bool,
    ) -> StageUse {
        self.offer = None;
        let Some(staged) = self.staged.take().filter(|_| panel_ready) else {
            return StageUse::Nothing;
        };
        if request.reading_page_key() != Some(staged.page) {
            StageUse::OtherPage
        } else if store.fingerprint() != staged.store {
            StageUse::StoreChanged
        } else {
            StageUse::Hit
        }
    }

    /// Stage the offered page if it can be: draw it into `fb` and write it
    /// to BW RAM. `panel_ready` says the panel shows a settled frame that
    /// RED RAM and `prev_fb` both hold. False when the panel did not take
    /// the write, so the caller stops trusting RED RAM as well.
    pub(crate) async fn stage(
        &mut self,
        epd: &mut Epd,
        fb: &mut Framebuffer,
        tx_band: &mut [u8; BAND_BYTES],
        store: &ReaderStore,
        panel_ready: bool,
    ) -> bool {
        let Some(drawn) = self.draw(fb, store, panel_ready) else {
            return true;
        };
        let started = Instant::now();
        let written = display_flush::stage_frame(epd, fb, tx_band, || {
            drawn.presses.current() && DISPLAY_COMMANDS.is_empty() && STORAGE_COMMANDS.is_empty()
        })
        .await;
        let bw_ms = started.elapsed().as_millis();
        let ok = matches!(written, Ok(true));
        if ok {
            self.staged = Some(drawn.staged);
        } else if written.is_err() || !drawn.presses.current() {
            // A press moved the reader on, or the panel failed the write;
            // a command alone leaves the offer to be staged again once the
            // task is quiet.
            self.offer = None;
        }
        esp_println::println!(
            "bench: stage page={} ok={} step=bw render_ms={} digest_us={} bw_ms={} t_ms={}",
            drawn.staged.page.page,
            ok,
            drawn.render_ms,
            drawn.digest_us,
            bw_ms,
            Instant::now().as_millis(),
        );
        written.is_ok()
    }

    /// The synchronous half of [`Self::stage`]: the checks and the draw.
    /// Out of line, so the display task's future does not carry the page
    /// draw's frame or the offer across the BW write.
    #[inline(never)]
    fn draw(
        &mut self,
        fb: &mut Framebuffer,
        store: &ReaderStore,
        panel_ready: bool,
    ) -> Option<Drawn> {
        let offer = self.offer.take()?;
        let key = offer.request.reading_page_key()?;
        if !offer.presses.current() {
            return None;
        }
        self.offer = Some(offer);
        self.staged = None;
        let reason = if !panel_ready {
            Some("panel")
        } else if !crate::views::page_can_be_staged(offer.request, store) {
            Some("page")
        } else {
            None
        };
        if let Some(reason) = reason {
            // Not this page; the next settled render offers another.
            self.offer = None;
            esp_println::println!(
                "bench: stage page={} ok=false step=check reason={}",
                key.page,
                reason
            );
            return None;
        }
        let started = Instant::now();
        if !crate::views::render_sd_reading_while(fb, offer.request, store, || {
            offer.presses.current()
        }) {
            self.offer = None;
            esp_println::println!(
                "bench: stage page={} ok=false step=render render_ms={} t_ms={}",
                key.page,
                started.elapsed().as_millis(),
                Instant::now().as_millis(),
            );
            return None;
        }
        let render_ms = started.elapsed().as_millis();
        let digest_start = Instant::now();
        let fingerprint = store.fingerprint();
        Some(Drawn {
            presses: offer.presses,
            staged: Staged {
                page: key,
                store: fingerprint,
            },
            render_ms,
            digest_us: digest_start.elapsed().as_micros(),
        })
    }
}

/// A page drawn into `fb`, waiting for its BW write.
struct Drawn {
    presses: Presses,
    staged: Staged,
    render_ms: u64,
    /// What one store fingerprint took, the cost a press's check adds.
    digest_us: u64,
}

/// Bench builds: after a staged page is shown, draw the same request
/// fresh into `fb` (free once RED RAM is prestaged from it) and compare
/// it with `prev_fb`, which holds the staged frame the panel now shows.
/// Gives way to a press like staging does.
#[cfg(feature = "bench-serial")]
pub(crate) fn check_staged(
    fb: &mut Framebuffer,
    prev_fb: &Framebuffer,
    request: RenderRequest,
    store: &ReaderStore,
) {
    let presses = Presses(BUTTON_PRESSES.load(Ordering::Relaxed));
    let started = Instant::now();
    // The render the press would have made takes this path too: a staged
    // page never reads the card.
    let card = crate::views::reader_reads_card(request, store);
    if !crate::views::render_sd_reading_while(fb, request, store, || presses.current()) {
        esp_println::println!(
            "bench: stage_check page={} done=false t_ms={}",
            request.page,
            Instant::now().as_millis(),
        );
        return;
    }
    let first_diff = fb
        .bytes()
        .iter()
        .zip(prev_fb.bytes())
        .position(|(fresh, staged)| fresh != staged);
    esp_println::println!(
        "bench: stage_check page={} done=true match={} first_diff={} card={} check_ms={} t_ms={}",
        request.page,
        first_diff.is_none() && !card,
        first_diff.map_or(-1, |at| at as i64),
        card,
        started.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
}
