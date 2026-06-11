//! One-way memory loan for the Wi-Fi sync session.
//!
//! The radio blob wants roughly 100 KB of heap that this firmware does
//! not have while the reader pipeline owns its scratch. Sync is
//! therefore a terminal mode: the display task dismantles the EPUB
//! scratch into raw byte regions, the wifi task donates them (plus the
//! otherwise unused dram2 boot-loader shadow segment) to esp-alloc, and
//! the only way back to reading is the software reset that ends the
//! session. Nothing here may be touched by reader code after the loan;
//! the display task enforces that by refusing scratch-using storage
//! commands once it has handed the memory over.
#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use esp_alloc::{HeapRegion, MemoryCapability};
// riscv32imc has no CAS; portable-atomic provides swap on single-core.
use portable_atomic::{AtomicBool, Ordering};

/// Bytes claimed from `dram2_seg` for the radio heap. The segment is
/// ~64.8 KB and also hosts the previous-frame framebuffer (below), which
/// was moved here so esp-wifi's static demand fits in main DRAM without
/// eating the stack region.
pub const DRAM2_HEAP_BYTES: usize = 16 * 1024;

/// A loanable byte region described by raw parts. Heap donations use raw
/// pointers rather than slices because the scratch-struct region is
/// repurposed padding-and-all, and no safe reference to it survives.
pub struct RawRegion {
    pub ptr: *mut u8,
    pub len: usize,
}

// Safety: a RawRegion is a one-way transfer of exclusive ownership; the
// loaning side never touches the memory again.
unsafe impl Send for RawRegion {}

/// The active book's kosync identity and position, gathered by the
/// display task while it still owns SD access. `None` when no SD book
/// has a saved position to exchange.
#[derive(Clone)]
pub struct SyncBookInfo {
    /// KOReader partial-MD5 of the EPUB file, the cross-device document id.
    pub document_md5: [u8; 16],
    /// Whole-book position, 0..=1000.
    pub percent_permille: u16,
    /// 1-based spine index for the DocFragment xpath KOReader jumps to.
    pub doc_fragment_1based: u16,
    pub page_count: u32,
    /// Saved state to base a pulled-position StoreProgress on.
    pub persisted: app_core::PersistedAppState,
    /// 0-based chapter start pages, for mapping a pulled page to a chapter.
    pub chapter_pages: [u16; app_core::MAX_SD_CHAPTERS],
    pub chapter_count: u8,
}

/// Everything the display task hands the wifi task: two heap regions
/// (the dismantled scratch struct and the XHTML window) plus initialized
/// buffers reused directly as socket and HTTP scratch, and the active
/// book's sync identity.
pub struct SyncLoan {
    pub heap_a: RawRegion,
    pub heap_b: RawRegion,
    pub tcp_rx: &'static mut [u8],
    pub tcp_tx: &'static mut [u8],
    pub http_a: &'static mut [u8],
    pub http_b: &'static mut [u8],
    pub book: Option<SyncBookInfo>,
    /// Credentials from /XTEINK/WIFI.BIN; `None` sends the wifi task into
    /// the onboarding portal unless the build carries compile-time ones.
    pub wifi: Option<app_core::WifiCredentials>,
}

struct Dram2(UnsafeCell<MaybeUninit<[u8; DRAM2_HEAP_BYTES]>>);

// Safety: access is gated by DRAM2_TAKEN below.
unsafe impl Sync for Dram2 {}

/// NOLOAD section: never zero-initialized at boot, which a heap region
/// does not need.
#[link_section = ".dram2_uninit"]
static DRAM2_HEAP: Dram2 = Dram2(UnsafeCell::new(MaybeUninit::uninit()));
static DRAM2_TAKEN: AtomicBool = AtomicBool::new(false);

struct PrevFbSlot(UnsafeCell<MaybeUninit<display::fb::Framebuffer>>);

// Safety: access is gated by PREV_FB_TAKEN below.
unsafe impl Sync for PrevFbSlot {}

/// The previous-frame framebuffer lives in dram2 instead of .bss so the
/// radio's static demand fits in main DRAM. NOLOAD, so the display task
/// claims it through here, which writes a cleared framebuffer first.
#[link_section = ".dram2_uninit"]
static PREV_FB_SLOT: PrevFbSlot = PrevFbSlot(UnsafeCell::new(MaybeUninit::uninit()));
static PREV_FB_TAKEN: AtomicBool = AtomicBool::new(false);

/// Hands out the dram2-resident previous-frame buffer, exactly once.
pub fn take_prev_fb() -> Option<&'static mut display::fb::Framebuffer> {
    if PREV_FB_TAKEN.swap(true, Ordering::SeqCst) {
        return None;
    }
    // Safety: the flag above makes this the only reference, and writing
    // a fresh framebuffer initializes the NOLOAD memory before any read.
    unsafe {
        let slot = &mut *PREV_FB_SLOT.0.get();
        Some(slot.write(display::fb::Framebuffer::new()))
    }
}

/// Donates dram2 plus the two loaned regions to the esp-alloc heap the
/// radio blob allocates from. Callable once; esp-alloc supports exactly
/// three regions, which is precisely what the sync session uses.
pub fn donate_heap(heap_a: RawRegion, heap_b: RawRegion) {
    if DRAM2_TAKEN.swap(true, Ordering::SeqCst) {
        return;
    }
    let dram2_ptr = DRAM2_HEAP.0.get().cast::<u8>();
    // Safety: each region is exclusively owned for the rest of the
    // session (dram2 by the flag above, the loans by the one-way
    // handoff), 'static by construction, and non-empty.
    unsafe {
        esp_alloc::HEAP.add_region(HeapRegion::new(
            dram2_ptr,
            DRAM2_HEAP_BYTES,
            MemoryCapability::Internal.into(),
        ));
        esp_alloc::HEAP.add_region(HeapRegion::new(
            heap_a.ptr,
            heap_a.len,
            MemoryCapability::Internal.into(),
        ));
        esp_alloc::HEAP.add_region(HeapRegion::new(
            heap_b.ptr,
            heap_b.len,
            MemoryCapability::Internal.into(),
        ));
    }
}
