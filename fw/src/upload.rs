//! Browser-to-shelf book upload plumbing.
//!
//! The wifi task receives raw EPUB bytes over HTTP and streams them to
//! the display task (the single SD owner) through a two-buffer
//! ping-pong: chunks carry loaned 4 KB buffers one way, the buffers
//! come back on the return channel once written. The display task holds
//! one SD session for the whole upload phase and writes /BOOKS/<8.3>.
//!
//! The pure pieces — name derivation, label shaping, identity-sidecar
//! parsing — live in `proto::upload` so host `cargo test` covers them
//! (this crate only compiles for the firmware target).

// riscv32imc has no CAS; portable-atomic provides it on single-core.
use portable_atomic::AtomicBool;

pub use proto::upload::{
    hash_identity, readable_filename, sanitized_name, UploadLabel, UploadName,
};

/// True while a book body is streaming; the session-ending reset waits
/// for it so a done press cannot truncate a file mid-write.
pub static UPLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub struct UploadBegin {
    pub name: UploadName,
    /// True removes the named book instead of writing one.
    pub delete: bool,
    /// Whether the name lives in /BOOKS (uploads always do; deletions
    /// follow the catalog's location flag).
    pub in_books: bool,
    /// The client's original filename, decoded but otherwise untouched.
    /// The 8.3 `name` can't carry a real title, so this is stashed in a
    /// label sidecar and shown in the Library until the book is first opened
    /// (which learns the EPUB title). Empty for deletions.
    pub label: UploadLabel,
    /// A 64-bit FNV-1a hash of the full decoded filename, used to uniquely
    /// identify this upload during collision resolution so long filenames with
    /// identical 64-byte truncated display labels don't overwrite each other.
    pub identity_hash: u64,
}

pub struct UploadChunk {
    /// `None` only on aborts that have no buffer left to hand over.
    pub buffer: Option<&'static mut [u8]>,
    pub len: usize,
    pub last: bool,
    pub abort: bool,
}
