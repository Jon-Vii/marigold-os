//! Transactional book-upload writes for the SD card.
//!
//! This is the state machine behind the firmware's browser-to-shelf upload:
//! probing the 8.3 slot chain for identity-matched prior copies and a free
//! slot, staging the identity/label sidecars, and — only after the caller has
//! streamed and closed the whole file — retiring the replaced copies. It is
//! generic over [`embedded_sdmmc::BlockDevice`], with no firmware
//! dependencies, so the host test suite can drive it against an in-memory
//! FAT card with injected faults (see `tests/transaction.rs`).
//!
//! The invariant the [`PendingUpload`] shape enforces: incoming data always
//! streams into an *empty* slot, never over the existing book.
//! embedded-sdmmc has no rename, so replacing in place would truncate the old
//! copy before the new one is known good; instead the old copy is deleted
//! only in [`PendingUpload::commit`] — after the target's close proves it
//! durable — and any failure path ([`PendingUpload::abort`], a failed close,
//! or an error inside [`PendingUpload::begin`]) removes only the new slot's
//! artifacts.

#![no_std]
#![forbid(unsafe_code)]

use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::cache::CACHE_ROOT_DIR;
use proto::upload::{base36_tail, UploadName};

/// Subdir under XTEINK holding one `<8.3-stem>.TXT` per uploaded book, each
/// carrying the prettified original filename. Uploads land as 8.3 names with no
/// long filename, so this is the only place a readable label survives until the
/// book is first opened (which then learns the EPUB title).
const LABELS_DIR: &str = "LABELS";

/// Collision-probe window: how many consecutive hash-tail slots an upload
/// examines. Uploads always land inside this window (at its first empty
/// slot), so scanning the whole window finds every surviving same-identity
/// copy — the invariant that makes replacement sound. It holds for any size,
/// but only if the window never shrinks once a release has placed books with
/// it; widening is always safe. 16 tolerates a 15-book tail-collision
/// cluster, far beyond what 4 base-36 hash digits make plausible, while
/// keeping the per-upload probe cost and the obsolete-tail buffer small.
const UPLOAD_PROBE_WINDOW: usize = 16;

fn label_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".TXT");
}

fn identity_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".ID");
}

fn suffixed_name(prefix4: &str, tail: u32) -> UploadName {
    let digits = base36_tail(tail);
    let mut name = UploadName::new();
    let _ = name.push_str(prefix4);
    for digit in digits {
        let _ = name.push(digit as char);
    }
    let _ = name.push_str(".EPU");
    name
}

/// Validates the full `PPPPTTTT.EPU` shape (uppercase-alphanumeric prefix,
/// base-36 tail, exact extension — see `proto::upload::sanitized_name`) and
/// splits it into the probe chain's prefix and starting tail. `UploadName`
/// is only a capacity bound, so every byte is checked before any slicing:
/// a multi-byte character straddling an offset would otherwise panic, and a
/// non-base-36 tail byte would silently probe an unrelated slot chain.
fn parse_upload_name(name: &UploadName) -> Option<(String<4>, u32)> {
    let bytes = name.as_bytes();
    if bytes.len() != 12 || &bytes[8..12] != b".EPU" {
        return None;
    }
    if !bytes[0..8]
        .iter()
        .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
    {
        return None;
    }
    let mut prefix = String::<4>::new();
    let _ = prefix.push_str(&name.as_str()[0..4]);
    let mut tail = 0u32;
    for &b in &bytes[4..8] {
        tail = tail * 36
            + match b {
                b'0'..=b'9' => (b - b'0') as u32,
                _ => (b - b'A' + 10) as u32,
            };
    }
    Some((prefix, tail))
}

fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &'a Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Result<Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match parent.open_dir(name) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let _ = parent.make_dir_in_dir(name);
            parent.open_dir(name).map_err(|_| ())
        }
    }
}

/// Stash an uploaded book's label: write the provided raw filename verbatim
/// to the sidecar keyed by the 8.3 name. Overwrites any prior label for that name.
pub fn write_upload_label<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
    raw_filename: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if raw_filename.is_empty() {
        delete_upload_sidecars(root, open_name);
        return true;
    }
    let Ok(xteink) = open_or_make_dir(root, CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(labels) = open_or_make_dir(&xteink, LABELS_DIR) else {
        return false;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadWriteCreateOrTruncate)
    else {
        return false;
    };
    let write_ok = file.write(raw_filename.as_bytes()).is_ok();
    let close_ok = file.close().is_ok();
    write_ok && close_ok
}

/// Read an uploaded book's stashed label into `out`. Returns false (leaving
/// `out` untouched) when the book has no sidecar -- i.e. it wasn't uploaded, so
/// the caller falls back to the file-stem label.
pub fn read_upload_label<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(labels) = xteink.open_dir(LABELS_DIR) else {
        return false;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let Ok(read) = file.read(&mut buf) else {
        return false;
    };
    let Ok(text) = core::str::from_utf8(&buf[..read]) else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    out.clear();
    let _ = out.push_str(text);
    true
}

/// Write a 64-bit identity hash for an uploaded book to distinguish exact
/// re-uploads from colliding prefix+hash names.
pub fn write_upload_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
    identity_hash: u64,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = open_or_make_dir(root, CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(labels) = open_or_make_dir(&xteink, LABELS_DIR) else {
        return false;
    };
    let mut file_name = String::<12>::new();
    identity_file_name(open_name, &mut file_name);
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadWriteCreateOrTruncate)
    else {
        return false;
    };
    let bytes = identity_hash.to_le_bytes();
    let write_ok = file.write(&bytes).is_ok();
    let close_ok = file.close().is_ok();
    write_ok && close_ok
}

/// How an identity sidecar read resolved, distinguishing "verifiably absent"
/// from "present but malformed" so the probe can count skipped sidecars for
/// the caller's diagnostics (a malformed sidecar reads as no identity, but
/// silently losing that fact would make the resulting duplicate book
/// inexplicable).
enum IdentityRead {
    Identity(u64),
    Absent,
    Malformed,
}

fn read_upload_identity_classified<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) -> Result<IdentityRead, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(d) => d,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(IdentityRead::Absent),
        Err(_) => return Err(()),
    };
    let labels = match xteink.open_dir(LABELS_DIR) {
        Ok(d) => d,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(IdentityRead::Absent),
        Err(_) => return Err(()),
    };
    let mut file_name = String::<12>::new();
    identity_file_name(open_name, &mut file_name);
    let file = match labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) {
        Ok(f) => f,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(IdentityRead::Absent),
        Err(_) => return Err(()),
    };
    let mut buf = [0u8; 8];
    match proto::upload::parse_identity_read(file.length(), file.read(&mut buf), &buf) {
        Ok(Some(identity)) => Ok(IdentityRead::Identity(identity)),
        // The sidecar exists but has the wrong length; parse treats that as
        // "no identity" because it is deterministic and retrying can't fix it.
        Ok(None) => Ok(IdentityRead::Malformed),
        Err(()) => Err(()),
    }
}

/// Read a 64-bit identity hash for an uploaded book. `Ok(None)` means the
/// book has no usable identity sidecar (absent, or malformed for good);
/// `Err` means the card can't be trusted right now (I/O error or a short
/// read of a correctly-sized sidecar), which callers must treat as
/// "unknown", not "different book".
#[allow(clippy::result_unit_err)]
pub fn read_upload_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) -> Result<Option<u64>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match read_upload_identity_classified(root, open_name)? {
        IdentityRead::Identity(identity) => Ok(Some(identity)),
        IdentityRead::Absent | IdentityRead::Malformed => Ok(None),
    }
}

/// Remove an uploaded book's identity and label sidecars, so a deleted book's name can't
/// mislabel a later upload that reuses the same 8.3 name.
pub fn delete_upload_sidecars<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(labels) = xteink.open_dir(LABELS_DIR) else {
        return;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let _ = labels.delete_file_in_dir(file_name.as_str());

    file_name.clear();
    identity_file_name(open_name, &mut file_name);
    let _ = labels.delete_file_in_dir(file_name.as_str());
}

/// One in-flight book write. Created by [`PendingUpload::begin`]; the caller
/// streams the body with [`PendingUpload::write`] and then calls exactly one
/// of [`PendingUpload::commit`] (data landed whole) or
/// [`PendingUpload::abort`] (write error or client abort). The staged target
/// [`File`] is owned privately, so it can neither be committed while still
/// open, substituted for another handle, nor left open to make abort's
/// deletion fail; `commit` closes it first and retires prior copies only
/// after the close succeeds.
///
/// Until a successful `commit` close, every prior copy of the book — and its
/// sidecars — is left untouched, so no failure between `begin` and `commit`
/// can cost the previously valid copy. Dropping a `PendingUpload` without
/// committing closes the target via the [`File`] drop but leaves it staged;
/// its valid identity sidecar means the next re-upload of the same book
/// retires it like any other surviving copy.
pub struct PendingUpload<
    'b,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    prefix: String<4>,
    target: UploadName,
    obsolete_tails: heapless::Vec<u32, UPLOAD_PROBE_WINDOW>,
    skipped_malformed_sidecars: usize,
    file: File<'b, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
}

impl<'b, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    PendingUpload<'b, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Probe the slot chain for `name`, stage the sidecars for a free slot,
    /// and open that slot's file for writing.
    ///
    /// `Err` means nothing usable was staged (any partially staged sidecars
    /// are removed) and no existing book was touched: a malformed `name`,
    /// I/O error while probing, no free slot in the window, or a
    /// sidecar/open failure.
    #[allow(clippy::result_unit_err)]
    pub fn begin(
        root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        books: &'b Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        name: &UploadName,
        identity_hash: u64,
        label: &str,
    ) -> Result<Self, ()> {
        let Some((prefix, mut tail)) = parse_upload_name(name) else {
            return Err(());
        };

        let mut obsolete_tails = heapless::Vec::<u32, UPLOAD_PROBE_WINDOW>::new();
        let mut skipped_malformed_sidecars = 0usize;
        let mut first_empty_name = None;

        // Scan the whole window with no early exit: a deletion can open an
        // empty slot in front of a surviving copy, so the first empty slot
        // proves nothing about later ones. Cost per upload is
        // UPLOAD_PROBE_WINDOW existence probes plus one identity read per
        // occupied slot — noise next to streaming the book body.
        for _ in 0..UPLOAD_PROBE_WINDOW {
            let candidate_name = suffixed_name(prefix.as_str(), tail);
            match books.open_file_in_dir(candidate_name.as_str(), Mode::ReadOnly) {
                Ok(existing_file) => {
                    drop(existing_file);

                    // Read the existing stashed identity to compare exact logical filename
                    match read_upload_identity_classified(root, candidate_name.as_str()) {
                        Ok(IdentityRead::Identity(existing_identity)) => {
                            if existing_identity == identity_hash {
                                // Same logical filename; the old copy is replaced
                                // only after the new data lands whole in an empty
                                // slot, so a failed transfer never costs it.
                                let _ = obsolete_tails.push(tail);
                            }
                        }
                        Ok(IdentityRead::Absent) => {
                            // Verifiably no identity. It's a different file.
                        }
                        Ok(IdentityRead::Malformed) => {
                            // A malformed sidecar never heals, so matching it
                            // can't be trusted and aborting would block this
                            // probe window forever; treat it as a different
                            // file. The worst case is a visible duplicate the
                            // user can delete. Counted so the caller can log it.
                            skipped_malformed_sidecars += 1;
                        }
                        Err(_) => {
                            // I/O error reading the identity sidecar. Abort.
                            return Err(());
                        }
                    }
                }
                Err(embedded_sdmmc::Error::NotFound) => {
                    // No existing file. Remember the first empty slot we find.
                    if first_empty_name.is_none() {
                        first_empty_name = Some(candidate_name.clone());
                    }
                }
                Err(_) => {
                    // I/O error checking if the file exists. Abort.
                    return Err(());
                }
            }

            // Genuinely different filename (or manually-copied file).
            // Increment the suffix and try the next candidate.
            tail = (tail + 1) % 36u32.pow(4);
        }

        // The data always streams into an empty slot, never over the existing
        // book: embedded-sdmmc has no rename, so replacing in place would
        // truncate the old copy before the new one is known good. The old book
        // (if any) is deleted only in commit().
        let Some(target) = first_empty_name else {
            return Err(());
        };

        // Write sidecars before creating the book data
        let sidecars_ok = write_upload_label(root, target.as_str(), label)
            && write_upload_identity(root, target.as_str(), identity_hash);
        let file = if sidecars_ok {
            books
                .open_file_in_dir(target.as_str(), Mode::ReadWriteCreateOrTruncate)
                .ok()
        } else {
            None
        };
        let Some(file) = file else {
            // Don't leave sidecars for a slot that never got its book.
            delete_upload_sidecars(root, target.as_str());
            return Err(());
        };

        Ok(PendingUpload {
            prefix,
            target,
            obsolete_tails,
            skipped_malformed_sidecars,
            file,
        })
    }

    /// Append body bytes to the staged target file.
    pub fn write(&self, data: &[u8]) -> Result<(), embedded_sdmmc::Error<D::Error>> {
        self.file.write(data)
    }

    /// The slot this upload streams into.
    pub fn target(&self) -> &UploadName {
        &self.target
    }

    /// How many malformed identity sidecars the probe skipped (treated as
    /// "different file"), for the caller's diagnostics: each one can turn a
    /// replacement into a visible duplicate.
    pub fn skipped_malformed_sidecars(&self) -> usize {
        self.skipped_malformed_sidecars
    }

    /// The transfer failed or was aborted: close the target file
    /// (best-effort — the slot is being discarded either way) and remove it
    /// with its sidecars. Prior copies were never touched. Sidecars go only
    /// if the file itself went, so a failed delete stays identity-matched
    /// and gets retried by the next re-upload.
    pub fn abort(
        self,
        root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    ) {
        let _ = self.file.close();
        discard_target(root, books, self.target.as_str());
    }

    /// The body streamed without error: close the target file, and only if
    /// the close succeeds — the new copy's metadata is durably on the card —
    /// retire the replaced copies and return the target name. A failed close
    /// discards the target like [`PendingUpload::abort`] and returns `None`,
    /// leaving every prior copy intact. Sidecars go only if the file itself
    /// went, so a failed delete stays identity-matched and gets retried by
    /// the next re-upload.
    #[must_use = "a None commit means the upload failed and the target was discarded"]
    pub fn commit(
        self,
        root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    ) -> Option<UploadName> {
        if self.file.close().is_err() {
            discard_target(root, books, self.target.as_str());
            return None;
        }
        for obsolete_tail in &self.obsolete_tails {
            let old = suffixed_name(self.prefix.as_str(), *obsolete_tail);
            if books.delete_file_in_dir(old.as_str()).is_ok() {
                delete_upload_sidecars(root, old.as_str());
            }
        }
        Some(self.target)
    }
}

/// Remove a discarded target slot's book file and, only if that deletion
/// succeeded, its sidecars (see the abort/commit notes on retry).
fn discard_target<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    target: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if books.delete_file_in_dir(target).is_ok() {
        delete_upload_sidecars(root, target);
    }
}
