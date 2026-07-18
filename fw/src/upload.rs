//! Browser-to-shelf book upload plumbing.
//!
//! The wifi task receives raw EPUB bytes over HTTP and streams them to
//! the display task (the single SD owner) through a two-buffer
//! ping-pong: chunks carry loaned 4 KB buffers one way, the buffers
//! come back on the return channel once written. The display task holds
//! one SD session for the whole upload phase and writes a visible `.epub`
//! long filename backed by a collision-safe 8.3 FAT alias.

// riscv32imc has no CAS; portable-atomic provides it on single-core.
use portable_atomic::AtomicBool;

pub use proto::upload::{
    upload_short_alias, wireless_epub_filename, UploadFilename, UploadShortName as UploadName,
};

/// True while a book body is streaming; the session-ending reset waits
/// for it so a done press cannot truncate a file mid-write.
pub static UPLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// True from the moment Wi-Fi requests the upload session until board I/O has
/// closed it. Setting it before queuing the storage command closes the Exit
/// race where reset could otherwise beat the SD owner into the session.
pub static UPLOAD_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct UploadBegin {
    pub name: UploadName,
    /// True removes the named book instead of writing one.
    pub delete: bool,
    /// Whether the name lives in /BOOKS (uploads always do; deletions
    /// follow the catalog's location flag).
    pub in_books: bool,
    /// Portable VFAT long filename for new uploads. Empty for deletions.
    pub long_name: UploadFilename,
}

pub struct UploadChunk {
    /// `None` only on aborts that have no buffer left to hand over.
    pub buffer: Option<&'static mut [u8]>,
    pub len: usize,
    pub last: bool,
    pub abort: bool,
}
