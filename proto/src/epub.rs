use crate::book::{BookId, BookMeta, BookSource, ChapterMeta, CoverStatus};
use crate::text::{FontStyle, TextAlign, TextBlock, TextRole};
use heapless::Vec;
use miniz_oxide::inflate::decompress_slice_iter_to_slice;
use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

// One spine item per chapter, so this bounds how many chapters a book can have.
// Items are now `Span`-encoded (see `SpineItem`/`Span`), ~half the size of the
// old `&str` form, so the spine and manifest tables hold far more chapters
// within the same (tight) EPUB-open stack budget that the fat `&str` version
// overflowed. 192 covers very long serials (HPMOR ~122 + matter); overflow sets
// `spine_truncated` rather than silently dropping the tail. Manifest must
// out-size the spine (it also carries cover/nav/ncx) and errors rather than
// truncating, so keep it comfortably ahead. One value for firmware and host so
// a host test exercises the same ceiling the device does.
pub const MAX_SPINE_ITEMS: usize = 192;
pub const MAX_MANIFEST_ITEMS: usize = 224;
pub const MAX_ENTRY_NAME_BYTES: usize = 160;
const ZIP_TAIL_READ_WINDOW: usize = 512;
const ZIP_EOCD_MIN_BYTES: u32 = 22;
const ZIP_EOCD_OVERLAP_BYTES: u32 = ZIP_EOCD_MIN_BYTES - 1;

pub trait ByteStream {
    type Error;

    fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error>;
}

pub trait ReadAt {
    type Error;

    fn len(&mut self) -> Result<u32, Self::Error>;
    fn is_empty(&mut self) -> Result<bool, Self::Error> {
        Ok(self.len()? == 0)
    }
    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    Start(&'a str),
    End(&'a str),
    Text(&'a str),
}

pub struct XmlCursor<'a> {
    input: &'a str,
    cursor: usize,
}

impl<'a> XmlCursor<'a> {
    pub const fn new(input: &'a str) -> Self {
        Self { input, cursor: 0 }
    }

    pub fn next_token(&mut self) -> Option<Token<'a>> {
        while self.cursor < self.input.len() {
            let rest = &self.input[self.cursor..];
            if let Some(after_lt) = rest.strip_prefix('<') {
                let end = after_lt.find('>')?;
                self.cursor += end + 2;
                let tag = after_lt[..end].trim();
                if tag.starts_with('!') || tag.starts_with('?') {
                    continue;
                }
                if let Some(name) = tag.strip_prefix('/') {
                    return Some(Token::End(name.trim()));
                }
                return Some(Token::Start(tag));
            }

            let end = rest.find('<').unwrap_or(rest.len());
            self.cursor += end;
            let text = &rest[..end];
            if !text.is_empty() {
                return Some(Token::Text(text));
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZipError {
    MissingEndOfCentralDirectory,
    BadCentralDirectory,
    BadLocalHeader,
    EntryNotFound,
    NameTooLong,
    UnsupportedCompression,
    OutputTooSmall,
    Inflate,
    Io,
    EntryBufferTooSmall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZipEntry<'a> {
    pub name: &'a str,
    pub compression_method: u16,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub local_header_offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedZipEntry {
    pub compression_method: u16,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub local_header_offset: u32,
}

/// How a [`ZipStream`] resolves entry names against the central directory.
#[derive(Clone, Copy)]
enum CentralLookup<'a> {
    /// The whole central directory fit in the tail scratch.
    Cached(&'a [u8]),
    /// One streaming pass built a bounded `(name hash, entry)` index in
    /// the tail scratch. Hash hits are verified against the local file
    /// header before use, so collisions cannot return the wrong entry.
    Indexed { records: &'a [u8], complete: bool },
    /// No acceleration: walk the central directory on storage per lookup.
    Walk,
}

const CENTRAL_INDEX_RECORD_BYTES: usize = 20;
const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

fn fnv1a_update(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub struct ZipStream<'a, R> {
    reader: R,
    central_offset: u32,
    entry_count: u16,
    lookup: CentralLookup<'a>,
}

pub struct ZipLocalStream<R> {
    reader: R,
    cursor: u32,
    pending_payload_offset: u32,
    pending_payload_remaining: u32,
}

pub struct ZipInflateScratch {
    state: InflateState,
}

impl ZipInflateScratch {
    pub fn new() -> Self {
        Self {
            state: InflateState::new(DataFormat::Raw),
        }
    }
}

impl Default for ZipInflateScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> ZipLocalStream<R>
where
    R: ByteStream,
{
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            cursor: 0,
            pending_payload_offset: 0,
            pending_payload_remaining: 0,
        }
    }

    pub fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        loop {
            read_exact_stream(&mut self.reader, &mut header_scratch[..30])?;
            let signature = read_u32(header_scratch, 0)?;
            if signature == 0x0201_4b50 || signature == 0x0605_4b50 {
                return Err(ZipError::EntryNotFound);
            }
            if signature != 0x0403_4b50 {
                return Err(ZipError::BadLocalHeader);
            }
            let flags = read_u16(header_scratch, 6)?;
            let compression_method = read_u16(header_scratch, 8)?;
            let compressed_size = read_u32(header_scratch, 18)?;
            let uncompressed_size = read_u32(header_scratch, 22)?;
            let name_len = read_u16(header_scratch, 26)? as usize;
            let extra_len = read_u16(header_scratch, 28)? as usize;
            if flags & 0x0008 != 0 {
                return Err(ZipError::UnsupportedCompression);
            }
            if name_len > name_scratch.len() {
                return Err(ZipError::EntryBufferTooSmall);
            }
            read_exact_stream(&mut self.reader, &mut name_scratch[..name_len])?;
            skip_stream(&mut self.reader, extra_len)?;
            let payload_offset = self
                .cursor
                .checked_add(30 + name_len as u32 + extra_len as u32)
                .ok_or(ZipError::BadLocalHeader)?;
            let entry_matches = core::str::from_utf8(&name_scratch[..name_len])
                .map(|entry_name| entry_name == name)
                .unwrap_or(false);
            if entry_matches {
                self.pending_payload_offset = payload_offset;
                self.pending_payload_remaining = compressed_size;
                return Ok(OwnedZipEntry {
                    compression_method,
                    compressed_size,
                    uncompressed_size,
                    local_header_offset: payload_offset,
                });
            }
            skip_stream(&mut self.reader, compressed_size as usize)?;
            self.cursor = payload_offset
                .checked_add(compressed_size)
                .ok_or(ZipError::BadLocalHeader)?;
        }
    }

    pub fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        if entry.uncompressed_size as usize > output.len() {
            return Err(ZipError::OutputTooSmall);
        }
        let (len, complete) =
            self.read_entry_prefix_streamed(entry, compressed_scratch, output, inflate_scratch)?;
        if complete {
            Ok(len)
        } else {
            Err(ZipError::OutputTooSmall)
        }
    }

    pub fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        if entry.local_header_offset != self.pending_payload_offset
            || entry.compressed_size != self.pending_payload_remaining
        {
            return Err(ZipError::BadLocalHeader);
        }
        match entry.compression_method {
            0 => {
                let output_len = output.len().min(entry.uncompressed_size as usize);
                read_exact_stream(&mut self.reader, &mut output[..output_len])?;
                self.finish_pending_payload(entry.compressed_size, output_len as u32)?;
                Ok((output_len, output_len == entry.uncompressed_size as usize))
            }
            8 => self.inflate_entry_prefix(entry.compressed_size, input, output, inflate_scratch),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    /// Stream a zip entry's uncompressed bytes into a caller-supplied sink.
    /// The `output_window` buffer is reused across chunks; it only needs to be
    /// large enough to make forward progress (a few hundred bytes is plenty).
    /// `emit` is invoked with each filled window; the parser/tokenizer on the
    /// other side consumes them incrementally and never has to materialize the
    /// whole entry in RAM.
    pub fn read_entry_to_sink<F>(
        &mut self,
        entry: OwnedZipEntry,
        input: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        mut emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        if entry.local_header_offset != self.pending_payload_offset
            || entry.compressed_size != self.pending_payload_remaining
        {
            return Err(ZipError::BadLocalHeader);
        }
        match entry.compression_method {
            0 => {
                let mut remaining = entry.uncompressed_size as usize;
                while remaining > 0 {
                    let take = remaining.min(output_window.len());
                    read_exact_stream(&mut self.reader, &mut output_window[..take])?;
                    emit(&output_window[..take])?;
                    remaining -= take;
                }
                self.finish_pending_payload(entry.compressed_size, entry.uncompressed_size)
            }
            8 => self.inflate_entry_to_sink(
                entry.compressed_size,
                input,
                output_window,
                inflate_scratch,
                emit,
            ),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    fn inflate_entry_to_sink<F>(
        &mut self,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        let reader = &mut self.reader;
        let fetched = inflate_chunks_to_sink(
            |_, buf| read_exact_stream(reader, buf),
            compressed_size,
            input,
            output,
            inflate_scratch,
            emit,
        )?;
        self.finish_pending_payload(compressed_size, fetched)
    }

    fn inflate_entry_prefix(
        &mut self,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        let reader = &mut self.reader;
        let (len, complete, fetched) = inflate_chunks_prefix(
            |_, buf| read_exact_stream(reader, buf),
            compressed_size,
            input,
            output,
            inflate_scratch,
        )?;
        self.finish_pending_payload(compressed_size, fetched)?;
        Ok((len, complete))
    }

    /// Skip whatever part of the pending entry payload was not read from the
    /// underlying stream, then advance the cursor past the entry so the next
    /// `find_entry` starts at a clean local-header boundary.
    fn finish_pending_payload(
        &mut self,
        compressed_size: u32,
        fetched: u32,
    ) -> Result<(), ZipError> {
        skip_stream(
            &mut self.reader,
            compressed_size.saturating_sub(fetched) as usize,
        )?;
        self.cursor = self
            .pending_payload_offset
            .checked_add(compressed_size)
            .ok_or(ZipError::BadLocalHeader)?;
        self.pending_payload_remaining = 0;
        Ok(())
    }
}

impl<'a, R> ZipStream<'a, R>
where
    R: ReadAt,
{
    pub fn new(mut reader: R, tail_scratch: &'a mut [u8]) -> Result<Self, ZipError> {
        let len = reader.len().map_err(|_| ZipError::Io)?;
        if len < ZIP_EOCD_MIN_BYTES || tail_scratch.len() < ZIP_EOCD_MIN_BYTES as usize {
            return Err(ZipError::MissingEndOfCentralDirectory);
        }
        let search_floor = len.saturating_sub(tail_scratch.len() as u32);
        let read_window = tail_scratch.len().min(ZIP_TAIL_READ_WINDOW);
        let mut chunk_end = len;
        let (entry_count, central_size, central_offset) = loop {
            let available = chunk_end.saturating_sub(search_floor) as usize;
            let chunk_len = available.min(read_window);
            if chunk_len < ZIP_EOCD_MIN_BYTES as usize {
                return Err(ZipError::MissingEndOfCentralDirectory);
            }
            let chunk_start = chunk_end - chunk_len as u32;
            let chunk = &mut tail_scratch[..chunk_len];
            read_exact_at(&mut reader, chunk_start, chunk)?;
            if let Some(eocd) = find_eocd(chunk) {
                break (
                    read_u16(chunk, eocd + 10)?,
                    read_u32(chunk, eocd + 12)?,
                    read_u32(chunk, eocd + 16)?,
                );
            }
            if chunk_start == search_floor {
                return Err(ZipError::MissingEndOfCentralDirectory);
            }
            chunk_end = chunk_start + ZIP_EOCD_OVERLAP_BYTES;
        };
        let lookup = if central_size as usize <= tail_scratch.len() {
            let central = &mut tail_scratch[..central_size as usize];
            read_exact_at(&mut reader, central_offset, central)?;
            CentralLookup::Cached(&central[..])
        } else {
            match build_central_index(&mut reader, central_offset, entry_count, tail_scratch) {
                Ok((records, complete)) => CentralLookup::Indexed { records, complete },
                Err(_) => CentralLookup::Walk,
            }
        };
        Ok(Self {
            reader,
            central_offset,
            entry_count,
            lookup,
        })
    }

    pub fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        match self.lookup {
            CentralLookup::Cached(central) => {
                find_entry_in_central_cache(central, self.entry_count, name)
            }
            CentralLookup::Indexed { records, complete } => {
                let hash = fnv1a_update(FNV_OFFSET, name.as_bytes());
                let mut unverified_hit = false;
                for record in records.chunks_exact(CENTRAL_INDEX_RECORD_BYTES) {
                    if read_u32(record, 0)? != hash {
                        continue;
                    }
                    let entry = OwnedZipEntry {
                        compression_method: read_u16(record, 16)?,
                        compressed_size: read_u32(record, 8)?,
                        uncompressed_size: read_u32(record, 12)?,
                        local_header_offset: read_u32(record, 4)?,
                    };
                    if self.local_name_matches(
                        entry.local_header_offset,
                        name,
                        header_scratch,
                        name_scratch,
                    )? {
                        return Ok(entry);
                    }
                    unverified_hit = true;
                }
                if complete && !unverified_hit {
                    Err(ZipError::EntryNotFound)
                } else {
                    // Either the index holds only a prefix of the central
                    // directory, or a hash hit failed local-header
                    // verification (possible with central/local name
                    // mismatches); the walk stays authoritative.
                    self.find_entry_by_walk(name, header_scratch, name_scratch)
                }
            }
            CentralLookup::Walk => self.find_entry_by_walk(name, header_scratch, name_scratch),
        }
    }

    fn find_entry_by_walk(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        let mut cursor = self.central_offset;
        for _ in 0..self.entry_count {
            read_exact_at(&mut self.reader, cursor, header_scratch)?;
            if read_u32(header_scratch, 0)? != 0x0201_4b50 {
                return Err(ZipError::BadCentralDirectory);
            }
            let compression_method = read_u16(header_scratch, 10)?;
            let compressed_size = read_u32(header_scratch, 20)?;
            let uncompressed_size = read_u32(header_scratch, 24)?;
            let name_len = read_u16(header_scratch, 28)? as usize;
            let extra_len = read_u16(header_scratch, 30)? as u32;
            let comment_len = read_u16(header_scratch, 32)? as u32;
            let local_header_offset = read_u32(header_scratch, 42)?;
            if name_len > name_scratch.len() {
                return Err(ZipError::EntryBufferTooSmall);
            }
            read_exact_at(&mut self.reader, cursor + 46, &mut name_scratch[..name_len])?;
            if core::str::from_utf8(&name_scratch[..name_len])
                .map(|entry_name| entry_name == name)
                .unwrap_or(false)
            {
                return Ok(OwnedZipEntry {
                    compression_method,
                    compressed_size,
                    uncompressed_size,
                    local_header_offset,
                });
            }
            cursor = cursor
                .checked_add(46 + name_len as u32 + extra_len + comment_len)
                .ok_or(ZipError::BadCentralDirectory)?;
        }
        Err(ZipError::EntryNotFound)
    }

    fn local_name_matches(
        &mut self,
        local_header_offset: u32,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<bool, ZipError> {
        let header = &mut header_scratch[..30];
        read_exact_at(&mut self.reader, local_header_offset, header)?;
        if read_u32(header, 0)? != 0x0403_4b50 {
            return Err(ZipError::BadLocalHeader);
        }
        let name_len = read_u16(header, 26)? as usize;
        if name_len != name.len() || name_len > name_scratch.len() {
            return Ok(false);
        }
        read_exact_at(
            &mut self.reader,
            local_header_offset + 30,
            &mut name_scratch[..name_len],
        )?;
        Ok(&name_scratch[..name_len] == name.as_bytes())
    }

    pub fn read_entry(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
    ) -> Result<usize, ZipError> {
        if entry.compressed_size as usize > compressed_scratch.len() {
            return Err(ZipError::OutputTooSmall);
        }
        if entry.uncompressed_size as usize > output.len() {
            return Err(ZipError::OutputTooSmall);
        }
        let payload_offset = self.entry_payload_offset(entry)?;
        let compressed = &mut compressed_scratch[..entry.compressed_size as usize];
        read_exact_at(&mut self.reader, payload_offset, compressed)?;
        match entry.compression_method {
            0 => {
                if output.len() < compressed.len() {
                    return Err(ZipError::OutputTooSmall);
                }
                output[..compressed.len()].copy_from_slice(compressed);
                Ok(compressed.len())
            }
            8 => decompress_slice_iter_to_slice(
                output,
                core::iter::once(&compressed[..]),
                false,
                true,
            )
            .map_err(|_| ZipError::Inflate),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    pub fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        if entry.uncompressed_size as usize > output.len() {
            return Err(ZipError::OutputTooSmall);
        }
        let (len, complete) =
            self.read_entry_prefix_streamed(entry, compressed_scratch, output, inflate_scratch)?;
        if complete {
            Ok(len)
        } else {
            Err(ZipError::OutputTooSmall)
        }
    }

    pub fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        let payload_offset = self.entry_payload_offset(entry)?;
        match entry.compression_method {
            0 => {
                let output_len = output.len().min(entry.uncompressed_size as usize);
                read_exact_at(&mut self.reader, payload_offset, &mut output[..output_len])?;
                Ok((output_len, output_len == entry.uncompressed_size as usize))
            }
            8 => self.inflate_entry_prefix(
                payload_offset,
                entry.compressed_size,
                compressed_scratch,
                output,
                inflate_scratch,
            ),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    /// Random-access counterpart to [`ZipLocalStream::read_entry_to_sink`].
    /// Inflates a zip entry chunk-by-chunk and forwards each chunk to `emit`
    /// without ever holding the full entry in RAM.
    pub fn read_entry_to_sink<F>(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        mut emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        let payload_offset = self.entry_payload_offset(entry)?;
        match entry.compression_method {
            0 => {
                let total = entry.uncompressed_size as usize;
                let mut written = 0usize;
                while written < total {
                    let take = (total - written).min(output_window.len());
                    read_exact_at(
                        &mut self.reader,
                        payload_offset + written as u32,
                        &mut output_window[..take],
                    )?;
                    emit(&output_window[..take])?;
                    written += take;
                }
                Ok(())
            }
            8 => self.inflate_entry_to_sink(
                payload_offset,
                entry.compressed_size,
                compressed_scratch,
                output_window,
                inflate_scratch,
                emit,
            ),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    fn inflate_entry_to_sink<F>(
        &mut self,
        payload_offset: u32,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: F,
    ) -> Result<(), ZipError>
    where
        F: FnMut(&[u8]) -> Result<(), ZipError>,
    {
        let reader = &mut self.reader;
        inflate_chunks_to_sink(
            |fetched, buf| read_exact_at(reader, payload_offset + fetched, buf),
            compressed_size,
            input,
            output,
            inflate_scratch,
            emit,
        )
        .map(|_| ())
    }

    fn inflate_entry_prefix(
        &mut self,
        payload_offset: u32,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        let reader = &mut self.reader;
        inflate_chunks_prefix(
            |fetched, buf| read_exact_at(reader, payload_offset + fetched, buf),
            compressed_size,
            input,
            output,
            inflate_scratch,
        )
        .map(|(len, complete, _)| (len, complete))
    }

    fn entry_payload_offset(&mut self, entry: OwnedZipEntry) -> Result<u32, ZipError> {
        let mut header = [0u8; 30];
        read_exact_at(&mut self.reader, entry.local_header_offset, &mut header)?;
        if read_u32(&header, 0)? != 0x0403_4b50 {
            return Err(ZipError::BadLocalHeader);
        }
        let name_len = read_u16(&header, 26)? as u32;
        let extra_len = read_u16(&header, 28)? as u32;
        entry
            .local_header_offset
            .checked_add(30 + name_len + extra_len)
            .ok_or(ZipError::BadLocalHeader)
    }
}

/// Narrow zip-entry interface shared by the EPUB loaders: find an entry by
/// name, then read it whole, as a bounded prefix, or streamed into a sink.
/// Both zip front-ends implement it so cache-building code does not care
/// whether compressed bytes come from random-access or forward-only storage.
pub trait EpubZipOps {
    /// Forward-only readers cannot revisit an entry once it has been passed.
    fn is_forward_only(&self) -> bool {
        false
    }

    fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError>;

    fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError>;

    fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError>;

    fn read_entry_to_sink(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
    ) -> Result<(), ZipError>;
}

impl<R> EpubZipOps for ZipStream<'_, R>
where
    R: ReadAt,
{
    fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        ZipStream::find_entry(self, name, header_scratch, name_scratch)
    }

    fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        ZipStream::read_entry_streamed(self, entry, compressed_scratch, output, inflate_scratch)
    }

    fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        ZipStream::read_entry_prefix_streamed(
            self,
            entry,
            compressed_scratch,
            output,
            inflate_scratch,
        )
    }

    fn read_entry_to_sink(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
    ) -> Result<(), ZipError> {
        ZipStream::read_entry_to_sink(
            self,
            entry,
            compressed_scratch,
            output_window,
            inflate_scratch,
            emit,
        )
    }
}

impl<R> EpubZipOps for ZipLocalStream<R>
where
    R: ByteStream,
{
    fn is_forward_only(&self) -> bool {
        true
    }

    fn find_entry(
        &mut self,
        name: &str,
        header_scratch: &mut [u8; 46],
        name_scratch: &mut [u8],
    ) -> Result<OwnedZipEntry, ZipError> {
        ZipLocalStream::find_entry(self, name, header_scratch, name_scratch)
    }

    fn read_entry_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<usize, ZipError> {
        ZipLocalStream::read_entry_streamed(
            self,
            entry,
            compressed_scratch,
            output,
            inflate_scratch,
        )
    }

    fn read_entry_prefix_streamed(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        ZipLocalStream::read_entry_prefix_streamed(
            self,
            entry,
            compressed_scratch,
            output,
            inflate_scratch,
        )
    }

    fn read_entry_to_sink(
        &mut self,
        entry: OwnedZipEntry,
        compressed_scratch: &mut [u8],
        output_window: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
    ) -> Result<(), ZipError> {
        ZipLocalStream::read_entry_to_sink(
            self,
            entry,
            compressed_scratch,
            output_window,
            inflate_scratch,
            emit,
        )
    }
}

/// Streaming inflate engine shared by both zip front-ends.
///
/// `fetch` fills `buf` with compressed payload bytes starting at the given
/// payload-relative offset; the engine only moves forward, so sequential
/// readers can ignore the offset. Inflate always runs with `MZFlush::None`:
/// passing `Finish` on the first call makes miniz decompress straight into
/// the caller's buffer and permanently fail the stream when that buffer is
/// smaller than the entry — exactly the small-window case this engine exists
/// for. The deflate stream's final block signals completion on its own, so
/// `Finish` is never needed.
///
/// Returns the number of compressed bytes fetched, which sequential callers
/// use to restore their cursor.
fn inflate_chunks_to_sink<F, E>(
    mut fetch: F,
    compressed_size: u32,
    input: &mut [u8],
    output: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
    mut emit: E,
) -> Result<u32, ZipError>
where
    F: FnMut(u32, &mut [u8]) -> Result<(), ZipError>,
    E: FnMut(&[u8]) -> Result<(), ZipError>,
{
    if input.is_empty() || output.is_empty() {
        return Err(ZipError::OutputTooSmall);
    }
    if compressed_size == 0 {
        return Ok(0);
    }
    inflate_scratch.state.reset(DataFormat::Raw);
    let mut compressed_read = 0u32;
    let mut output_pos = 0usize;
    loop {
        let remaining = (compressed_size - compressed_read) as usize;
        let input_len = input.len().min(remaining);
        if input_len > 0 {
            fetch(compressed_read, &mut input[..input_len])?;
            compressed_read += input_len as u32;
        }
        let mut consumed = 0usize;
        loop {
            let result = inflate(
                &mut inflate_scratch.state,
                &input[consumed..input_len],
                &mut output[output_pos..],
                MZFlush::None,
            );
            consumed += result.bytes_consumed;
            output_pos += result.bytes_written;
            match result.status {
                Ok(MZStatus::StreamEnd) => {
                    if output_pos > 0 {
                        emit(&output[..output_pos])?;
                    }
                    return Ok(compressed_read);
                }
                // `BufError` is miniz for "no room": output full or input
                // empty. Neither is fatal here, so it shares the Ok logic and
                // the progress checks below decide what happens next.
                Ok(MZStatus::Ok) | Err(MZError::Buf) => {
                    if output_pos == output.len() {
                        emit(&output[..output_pos])?;
                        output_pos = 0;
                        continue;
                    }
                    if result.bytes_consumed == 0 && result.bytes_written == 0 {
                        if consumed == input_len {
                            // Chunk and dictionary both drained: fetch more.
                            break;
                        }
                        // Input and window space available but no progress:
                        // the stream is corrupt.
                        return Err(ZipError::Inflate);
                    }
                    // Progress with window space left: inflate again. This
                    // covers entries larger than the 32 KB dictionary, which
                    // drain in partial pushes between input chunks.
                }
                _ => return Err(ZipError::Inflate),
            }
        }
        if compressed_read == compressed_size {
            // Every payload byte was consumed without an end-of-stream block.
            return Err(ZipError::Inflate);
        }
    }
}

/// Prefix variant of [`inflate_chunks_to_sink`]: decodes into `output` until
/// it fills, returning the decoded length, whether the whole entry fit, and
/// the number of compressed bytes fetched.
fn inflate_chunks_prefix<F>(
    mut fetch: F,
    compressed_size: u32,
    input: &mut [u8],
    output: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
) -> Result<(usize, bool, u32), ZipError>
where
    F: FnMut(u32, &mut [u8]) -> Result<(), ZipError>,
{
    if input.is_empty() || output.is_empty() {
        return Err(ZipError::OutputTooSmall);
    }
    if compressed_size == 0 {
        return Ok((0, true, 0));
    }
    inflate_scratch.state.reset(DataFormat::Raw);
    let mut compressed_read = 0u32;
    let mut output_pos = 0usize;
    loop {
        let remaining = (compressed_size - compressed_read) as usize;
        let input_len = input.len().min(remaining);
        if input_len > 0 {
            fetch(compressed_read, &mut input[..input_len])?;
            compressed_read += input_len as u32;
        }
        let mut consumed = 0usize;
        loop {
            let result = inflate(
                &mut inflate_scratch.state,
                &input[consumed..input_len],
                &mut output[output_pos..],
                MZFlush::None,
            );
            consumed += result.bytes_consumed;
            output_pos += result.bytes_written;
            match result.status {
                Ok(MZStatus::StreamEnd) => return Ok((output_pos, true, compressed_read)),
                // See `inflate_chunks_to_sink` for the shared Ok/Buf logic.
                Ok(MZStatus::Ok) | Err(MZError::Buf) => {
                    if output_pos == output.len() {
                        // A full buffer with the stream still open means the
                        // entry holds more bytes than the prefix can take.
                        return Ok((output_pos, false, compressed_read));
                    }
                    if result.bytes_consumed == 0 && result.bytes_written == 0 {
                        if consumed == input_len {
                            break;
                        }
                        return Err(ZipError::Inflate);
                    }
                }
                _ => return Err(ZipError::Inflate),
            }
        }
        if compressed_read == compressed_size {
            // Every payload byte was consumed without an end-of-stream block.
            return Err(ZipError::Inflate);
        }
    }
}

/// One sequential pass over a central directory too large for the tail
/// scratch, emitting bounded `(name hash, entry)` records into `scratch`.
/// Returns the encoded records and whether every entry fit. Sequential
/// reads are the access pattern storage likes; afterwards each lookup is
/// a RAM scan plus one local-header verification read, instead of a
/// per-lookup walk of the whole directory on storage.
fn build_central_index<'a, R>(
    reader: &mut R,
    central_offset: u32,
    entry_count: u16,
    scratch: &'a mut [u8],
) -> Result<(&'a [u8], bool), ZipError>
where
    R: ReadAt,
{
    let capacity = scratch.len() / CENTRAL_INDEX_RECORD_BYTES;
    let indexed = (entry_count as usize).min(capacity);
    let mut cursor = central_offset;
    let mut header = [0u8; 46];
    let mut name_chunk = [0u8; 64];
    for slot in 0..indexed {
        read_exact_at(reader, cursor, &mut header)?;
        if read_u32(&header, 0)? != 0x0201_4b50 {
            return Err(ZipError::BadCentralDirectory);
        }
        let compression_method = read_u16(&header, 10)?;
        let compressed_size = read_u32(&header, 20)?;
        let uncompressed_size = read_u32(&header, 24)?;
        let name_len = read_u16(&header, 28)? as u32;
        let extra_len = read_u16(&header, 30)? as u32;
        let comment_len = read_u16(&header, 32)? as u32;
        let local_header_offset = read_u32(&header, 42)?;

        let mut hash = FNV_OFFSET;
        let mut name_cursor = cursor
            .checked_add(46)
            .ok_or(ZipError::BadCentralDirectory)?;
        let mut remaining = name_len;
        while remaining > 0 {
            let take = (remaining as usize).min(name_chunk.len());
            read_exact_at(reader, name_cursor, &mut name_chunk[..take])?;
            hash = fnv1a_update(hash, &name_chunk[..take]);
            name_cursor = name_cursor
                .checked_add(take as u32)
                .ok_or(ZipError::BadCentralDirectory)?;
            remaining -= take as u32;
        }

        let record = &mut scratch
            [slot * CENTRAL_INDEX_RECORD_BYTES..(slot + 1) * CENTRAL_INDEX_RECORD_BYTES];
        record[0..4].copy_from_slice(&hash.to_le_bytes());
        record[4..8].copy_from_slice(&local_header_offset.to_le_bytes());
        record[8..12].copy_from_slice(&compressed_size.to_le_bytes());
        record[12..16].copy_from_slice(&uncompressed_size.to_le_bytes());
        record[16..18].copy_from_slice(&compression_method.to_le_bytes());
        record[18..20].copy_from_slice(&[0u8; 2]);

        cursor = cursor
            .checked_add(46)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .and_then(|value| value.checked_add(comment_len))
            .ok_or(ZipError::BadCentralDirectory)?;
    }
    Ok((
        &scratch[..indexed * CENTRAL_INDEX_RECORD_BYTES],
        indexed == entry_count as usize,
    ))
}

fn find_entry_in_central_cache(
    central: &[u8],
    entry_count: u16,
    name: &str,
) -> Result<OwnedZipEntry, ZipError> {
    let mut cursor = 0usize;
    for _ in 0..entry_count {
        if read_u32(central, cursor)? != 0x0201_4b50 {
            return Err(ZipError::BadCentralDirectory);
        }
        let compression_method = read_u16(central, cursor + 10)?;
        let compressed_size = read_u32(central, cursor + 20)?;
        let uncompressed_size = read_u32(central, cursor + 24)?;
        let name_len = read_u16(central, cursor + 28)? as usize;
        let extra_len = read_u16(central, cursor + 30)? as usize;
        let comment_len = read_u16(central, cursor + 32)? as usize;
        let local_header_offset = read_u32(central, cursor + 42)?;

        let name_start = cursor
            .checked_add(46)
            .ok_or(ZipError::BadCentralDirectory)?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or(ZipError::BadCentralDirectory)?;
        let entry_name = central
            .get(name_start..name_end)
            .ok_or(ZipError::BadCentralDirectory)?;
        if entry_name == name.as_bytes() {
            return Ok(OwnedZipEntry {
                compression_method,
                compressed_size,
                uncompressed_size,
                local_header_offset,
            });
        }

        cursor = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .ok_or(ZipError::BadCentralDirectory)?;
    }
    Err(ZipError::EntryNotFound)
}

pub struct ZipArchive<'a> {
    bytes: &'a [u8],
    central_offset: usize,
    entry_count: usize,
}

impl<'a> ZipArchive<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, ZipError> {
        let eocd = find_eocd(bytes).ok_or(ZipError::MissingEndOfCentralDirectory)?;
        if eocd + 22 > bytes.len() {
            return Err(ZipError::BadCentralDirectory);
        }
        let entry_count = read_u16(bytes, eocd + 10)? as usize;
        let central_size = read_u32(bytes, eocd + 12)? as usize;
        let central_offset = read_u32(bytes, eocd + 16)? as usize;
        if central_offset
            .checked_add(central_size)
            .filter(|end| *end <= bytes.len())
            .is_none()
        {
            return Err(ZipError::BadCentralDirectory);
        }
        Ok(Self {
            bytes,
            central_offset,
            entry_count,
        })
    }

    pub fn entries(&self) -> ZipEntries<'a> {
        ZipEntries {
            bytes: self.bytes,
            cursor: self.central_offset,
            remaining: self.entry_count,
        }
    }

    pub fn find(&self, name: &str) -> Result<ZipEntry<'a>, ZipError> {
        self.entries()
            .find(|entry| entry.map(|entry| entry.name == name).unwrap_or(false))
            .ok_or(ZipError::EntryNotFound)?
    }

    pub fn read_entry(&self, entry: ZipEntry<'a>, output: &mut [u8]) -> Result<usize, ZipError> {
        let compressed = self.entry_payload(entry)?;
        match entry.compression_method {
            0 => {
                if output.len() < compressed.len() {
                    return Err(ZipError::OutputTooSmall);
                }
                output[..compressed.len()].copy_from_slice(compressed);
                Ok(compressed.len())
            }
            8 => decompress_slice_iter_to_slice(output, core::iter::once(compressed), false, true)
                .map_err(|_| ZipError::Inflate),
            _ => Err(ZipError::UnsupportedCompression),
        }
    }

    fn entry_payload(&self, entry: ZipEntry<'a>) -> Result<&'a [u8], ZipError> {
        let offset = entry.local_header_offset as usize;
        if read_u32(self.bytes, offset)? != 0x0403_4b50 {
            return Err(ZipError::BadLocalHeader);
        }
        let name_len = read_u16(self.bytes, offset + 26)? as usize;
        let extra_len = read_u16(self.bytes, offset + 28)? as usize;
        let start = offset
            .checked_add(30)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .ok_or(ZipError::BadLocalHeader)?;
        let end = start
            .checked_add(entry.compressed_size as usize)
            .ok_or(ZipError::BadLocalHeader)?;
        self.bytes.get(start..end).ok_or(ZipError::BadLocalHeader)
    }
}

pub struct ZipEntries<'a> {
    bytes: &'a [u8],
    cursor: usize,
    remaining: usize,
}

impl<'a> Iterator for ZipEntries<'a> {
    type Item = Result<ZipEntry<'a>, ZipError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let result = parse_central_entry(self.bytes, self.cursor);
        if let Ok((entry, next_cursor)) = result {
            self.cursor = next_cursor;
            Some(Ok(entry))
        } else {
            self.remaining = 0;
            Some(result.map(|(entry, _)| entry))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpubError {
    Zip(ZipError),
    Utf8,
    MissingContainer,
    MissingOpfPath,
    MissingOpf,
    TooManyManifestItems,
    TooManySpineItems,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XhtmlError {
    TooManyRuns,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TocError {
    TooManyItems,
}

pub trait XhtmlBlockSink {
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
        paragraph_end: bool,
    ) -> Result<(), XhtmlError>;
}

pub trait EpubTocSink {
    fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError>;
}

pub const MAX_CSS_RULES: usize = 96;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssRule {
    pub selector: heapless::String<64>,
    pub align: TextAlign,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssRules {
    pub rules: Vec<CssRule, MAX_CSS_RULES>,
}

impl CssRules {
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.rules.clear();
    }

    pub fn push_text_align(&mut self, selector: &str, align: TextAlign) {
        let selector = selector.trim();
        if !selector_is_supported(selector) {
            return;
        }
        let mut stored = heapless::String::<64>::new();
        if stored.push_str(selector).is_err() {
            return;
        }
        if let Some(rule) = self
            .rules
            .iter_mut()
            .find(|rule| rule.selector.as_str() == stored.as_str())
        {
            rule.align = align;
            return;
        }
        let _ = self.rules.push(CssRule {
            selector: stored,
            align,
        });
    }

    pub fn alignment_for(&self, tag: &str) -> Option<TextAlign> {
        let tag_name = tag_local_name(tag)?;
        let mut result = self.align_for_selector(tag_name);
        if let Some(classes) = attr_value(tag, "class") {
            for class in classes.split_ascii_whitespace() {
                let mut selector = heapless::String::<64>::new();
                if selector.push('.').is_ok() && selector.push_str(class).is_ok() {
                    result = self.align_for_selector(selector.as_str()).or(result);
                }
                selector.clear();
                if selector.push_str(tag_name).is_ok()
                    && selector.push('.').is_ok()
                    && selector.push_str(class).is_ok()
                {
                    result = self.align_for_selector(selector.as_str()).or(result);
                }
            }
        }
        result
    }

    fn align_for_selector(&self, selector: &str) -> Option<TextAlign> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.selector.as_str().eq_ignore_ascii_case(selector))
            .map(|rule| rule.align)
    }
}

impl Default for CssRules {
    fn default() -> Self {
        Self::new()
    }
}

impl From<ZipError> for EpubError {
    fn from(value: ZipError) -> Self {
        Self::Zip(value)
    }
}

/// A byte range inside the OPF text, resolved on demand. Spine and manifest
/// items hold spans instead of `&str` so they carry no lifetime and pack into
/// half the space -- letting the (transient, build-time) spine and manifest
/// tables hold a long book's chapters within the tight EPUB-open stack budget,
/// where fat `&str` items (8 bytes each on 32-bit) overflowed it. This mirrors
/// the offset+len encoding every stored record already uses (`TocRecord`,
/// `BlockRecord`, `SpineRecord`). The OPF is <= 16 KB, so `u16` always fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Span {
    pub off: u16,
    pub len: u16,
}

impl Span {
    /// The slice of `opf` this span indexes. Empty if the span is out of range
    /// (it never is for spans built from `opf` itself).
    pub fn of<'a>(&self, opf: &'a str) -> &'a str {
        let start = self.off as usize;
        opf.get(start..start + self.len as usize).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The span of `sub` within `opf`. `sub` must be a subslice of `opf` (every
/// parsed attribute value is); anything else, or a range past `u16`, collapses
/// to an empty span rather than panicking.
fn span_in(opf: &str, sub: &str) -> Span {
    let base = opf.as_ptr() as usize;
    let start = sub.as_ptr() as usize;
    if start < base {
        return Span::default();
    }
    let off = start - base;
    if off + sub.len() > opf.len() || off > u16::MAX as usize || sub.len() > u16::MAX as usize {
        return Span::default();
    }
    Span {
        off: off as u16,
        len: sub.len() as u16,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ManifestItem {
    pub id: Span,
    pub href: Span,
    pub media_type: Span,
    pub properties: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SpineItem {
    pub href: Span,
    pub media_type: Span,
    pub properties: Span,
}

pub struct EpubPackage<'a> {
    pub meta: BookMeta<'a>,
    /// The OPF text the spine/manifest spans index into; resolve with `Span::of`.
    pub opf_text: &'a str,
    pub opf_path: &'a str,
    pub text_reference_href: Option<&'a str>,
    pub nav_href: Option<&'a str>,
    pub ncx_href: Option<&'a str>,
    /// True when the OPF held more spine items than `MAX_SPINE_ITEMS`, so the
    /// spine (and the book) is clipped. Callers flag the book partial instead
    /// of silently dropping the tail chapters.
    pub spine_truncated: bool,
    pub manifest: Vec<ManifestItem, MAX_MANIFEST_ITEMS>,
    pub spine: Vec<SpineItem, MAX_SPINE_ITEMS>,
}

impl<'a> EpubPackage<'a> {
    /// The href of a spine item, resolved against this package's OPF text.
    pub fn spine_href(&self, item: &SpineItem) -> &'a str {
        item.href.of(self.opf_text)
    }

    pub fn chapters(&self, output: &mut Vec<ChapterMeta<'a>, MAX_SPINE_ITEMS>) {
        output.clear();
        for (index, spine) in self.spine.iter().enumerate() {
            let href = spine.href.of(self.opf_text);
            let title = href.rsplit('/').next().unwrap_or(href);
            let _ = output.push(ChapterMeta {
                title,
                spine_index: index as u16,
                source_href: href,
            });
        }
    }
}

pub fn load_epub_package<'a>(
    epub_bytes: &'a [u8],
    container_scratch: &'a mut [u8],
    opf_scratch: &'a mut [u8],
    book_id: BookId,
    source_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError> {
    let zip = ZipArchive::new(epub_bytes)?;
    let container = zip.find("META-INF/container.xml")?;
    let container_len = zip.read_entry(container, container_scratch)?;
    let container_xml =
        core::str::from_utf8(&container_scratch[..container_len]).map_err(|_| EpubError::Utf8)?;
    let opf_path =
        find_attr_value(container_xml, "rootfile", "full-path").ok_or(EpubError::MissingOpfPath)?;

    let opf_entry = zip.find(opf_path).map_err(|_| EpubError::MissingOpf)?;
    let opf_len = zip.read_entry(opf_entry, opf_scratch)?;
    let opf_xml = core::str::from_utf8(&opf_scratch[..opf_len]).map_err(|_| EpubError::Utf8)?;
    parse_opf(
        opf_xml,
        book_id,
        source_path,
        epub_bytes.len() as u32,
        opf_path,
    )
}

/// Helper to extract the OPF path from an EPUB's META-INF/container.xml entry
/// using the streaming Zip operations. Errors keep their nature — zip
/// failures stay `EpubError::Zip`, text and structure failures map to
/// `Utf8`/`MissingOpfPath` — matching `load_epub_package`'s handling of
/// the identical cases so callers surface the right diagnostics.
pub fn load_container_xml_and_find_opf_path<Z: EpubZipOps>(
    zip: &mut Z,
    header_scratch: &mut [u8; 46],
    name_scratch: &mut [u8],
    compressed_scratch: &mut [u8],
    container_scratch: &mut [u8],
    zip_inflate: &mut ZipInflateScratch,
    opf_path_buf: &mut heapless::String<256>,
) -> Result<(), EpubError> {
    let container_entry = zip
        .find_entry("META-INF/container.xml", header_scratch, name_scratch)
        .map_err(EpubError::Zip)?;
    let container_len = zip
        .read_entry_streamed(
            container_entry,
            compressed_scratch,
            container_scratch,
            zip_inflate,
        )
        .map_err(EpubError::Zip)?;
    let container_xml =
        core::str::from_utf8(&container_scratch[..container_len]).map_err(|_| EpubError::Utf8)?;
    let opf_path =
        find_attr_value(container_xml, "rootfile", "full-path").ok_or(EpubError::MissingOpfPath)?;
    opf_path_buf.clear();
    opf_path_buf
        .push_str(opf_path)
        .map_err(|_| EpubError::Zip(ZipError::NameTooLong))?;
    Ok(())
}

pub fn parse_opf<'a>(
    opf_xml: &'a str,
    book_id: BookId,
    source_path: &'a str,
    byte_size: u32,
    opf_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError> {
    let title = element_text(opf_xml, "title").unwrap_or("Untitled");
    let author = element_text(opf_xml, "creator").unwrap_or("Unknown Author");

    let spine_idrefs = collect_spine_idrefs(opf_xml);
    let mut manifest = Vec::new();
    let mut in_manifest = false;
    let mut cursor = XmlCursor::new(opf_xml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "manifest") => in_manifest = true,
            Token::End(tag) if tag_name_is(tag, "manifest") => in_manifest = false,
            Token::Start(tag) if in_manifest && tag_name_is(tag, "item") => {
                let Some(id) = attr_value(tag, "id") else {
                    continue;
                };
                let Some(href) = attr_value(tag, "href") else {
                    continue;
                };
                let media_type = attr_value(tag, "media-type").unwrap_or("");
                let properties = attr_value(tag, "properties").unwrap_or("");
                if manifest_item_needed(id, href, media_type, properties, &spine_idrefs) {
                    manifest
                        .push(ManifestItem {
                            id: span_in(opf_xml, id),
                            href: span_in(opf_xml, href),
                            media_type: span_in(opf_xml, media_type),
                            properties: span_in(opf_xml, properties),
                        })
                        .map_err(|_| EpubError::TooManyManifestItems)?;
                }
            }
            _ => {}
        }
    }

    let mut spine = Vec::new();
    let mut spine_truncated = false;
    let mut in_spine = false;
    let mut cursor = XmlCursor::new(opf_xml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "spine") => in_spine = true,
            Token::End(tag) if tag_name_is(tag, "spine") => in_spine = false,
            Token::Start(tag) if in_spine && tag_name_is(tag, "itemref") => {
                let Some(idref) = attr_value(tag, "idref") else {
                    continue;
                };
                let Some(item) = manifest.iter().find(|item| item.id.of(opf_xml) == idref) else {
                    continue;
                };
                // Manifest spans already index the same OPF, so copy them
                // straight across.
                if spine
                    .push(SpineItem {
                        href: item.href,
                        media_type: item.media_type,
                        properties: item.properties,
                    })
                    .is_err()
                {
                    spine_truncated = true;
                    break;
                }
            }
            _ => {}
        }
    }
    if spine.is_empty() {
        collect_fallback_spine_items(opf_xml, &mut spine)?;
    }

    let text_reference_href = find_guide_reference(opf_xml, "text")
        .or_else(|| find_guide_reference(opf_xml, "start"))
        .map(strip_fragment);
    let nav_href = manifest
        .iter()
        .find(|item| {
            item.properties
                .of(opf_xml)
                .split_ascii_whitespace()
                .any(|prop| prop == "nav")
        })
        .map(|item| item.href.of(opf_xml));
    let ncx_href = manifest
        .iter()
        .find(|item| {
            item.media_type
                .of(opf_xml)
                .eq_ignore_ascii_case("application/x-dtbncx+xml")
                || item.href.of(opf_xml).ends_with(".ncx")
        })
        .map(|item| item.href.of(opf_xml));

    Ok(EpubPackage {
        meta: BookMeta {
            id: book_id,
            title,
            author,
            source_path,
            byte_size,
            source: BookSource::MicroSd,
            cover_status: cover_status(&manifest, opf_xml),
        },
        opf_text: opf_xml,
        opf_path,
        text_reference_href,
        nav_href,
        ncx_href,
        spine_truncated,
        manifest,
        spine,
    })
}

fn collect_fallback_spine_items(
    opf_xml: &str,
    spine: &mut Vec<SpineItem, MAX_SPINE_ITEMS>,
) -> Result<(), EpubError> {
    let mut in_manifest = false;
    let mut cursor = XmlCursor::new(opf_xml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "manifest") => in_manifest = true,
            Token::End(tag) if tag_name_is(tag, "manifest") => in_manifest = false,
            Token::Start(tag) if in_manifest && tag_name_is(tag, "item") => {
                let Some(href) = attr_value(tag, "href") else {
                    continue;
                };
                let media_type = attr_value(tag, "media-type").unwrap_or("");
                let properties = attr_value(tag, "properties").unwrap_or("");
                if manifest_item_is_reading_candidate(href, media_type, properties) {
                    let _ = spine.push(SpineItem {
                        href: span_in(opf_xml, href),
                        media_type: span_in(opf_xml, media_type),
                        properties: span_in(opf_xml, properties),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_spine_idrefs(opf_xml: &str) -> Vec<&str, MAX_SPINE_ITEMS> {
    let mut idrefs = Vec::new();
    let mut in_spine = false;
    let mut cursor = XmlCursor::new(opf_xml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "spine") => in_spine = true,
            Token::End(tag) if tag_name_is(tag, "spine") => in_spine = false,
            Token::Start(tag) if in_spine && tag_name_is(tag, "itemref") => {
                if let Some(idref) = attr_value(tag, "idref") {
                    let _ = idrefs.push(idref);
                }
            }
            _ => {}
        }
    }
    idrefs
}

fn manifest_item_is_reading_candidate(href: &str, media_type: &str, properties: &str) -> bool {
    if href.is_empty()
        || properties
            .split_ascii_whitespace()
            .any(|prop| prop == "nav")
        || media_type.eq_ignore_ascii_case("application/x-dtbncx+xml")
        || href.ends_with(".ncx")
        || href.ends_with(".css")
    {
        return false;
    }
    media_type.eq_ignore_ascii_case("application/xhtml+xml")
        || href.ends_with(".xhtml")
        || href.ends_with(".html")
}

fn manifest_item_needed(
    id: &str,
    href: &str,
    media_type: &str,
    properties: &str,
    spine_idrefs: &Vec<&str, MAX_SPINE_ITEMS>,
) -> bool {
    spine_idrefs.contains(&id)
        || properties
            .split_ascii_whitespace()
            .any(|prop| prop == "nav" || prop == "cover-image")
        || media_type.eq_ignore_ascii_case("application/x-dtbncx+xml")
        || id == "cover"
        || href.contains("cover")
        || href.ends_with(".ncx")
}

pub fn xhtml_text_blocks_with_css<const BLOCK_LEN: usize, const BLOCKS: usize>(
    xhtml: &str,
    css: Option<&CssRules>,
    output: &mut Vec<TextBlock<BLOCK_LEN>, BLOCKS>,
) -> Result<(), XhtmlError> {
    output.clear();
    struct VecSink<'a, const BLOCK_LEN: usize, const BLOCKS: usize> {
        output: &'a mut Vec<TextBlock<BLOCK_LEN>, BLOCKS>,
        continuation_open: bool,
        pending_space: bool,
    }
    impl<const BLOCK_LEN: usize, const BLOCKS: usize> XhtmlBlockSink
        for VecSink<'_, BLOCK_LEN, BLOCKS>
    {
        fn push_block(
            &mut self,
            text: &str,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            paragraph_end: bool,
        ) -> Result<(), XhtmlError> {
            if self.continuation_open {
                if let Some(last) = self.output.last_mut() {
                    if self.pending_space
                        && !text
                            .chars()
                            .next()
                            .map(|ch| ch.is_whitespace() || is_leading_punctuation_char(ch))
                            .unwrap_or(true)
                    {
                        let _ = last.text.push(' ');
                    }
                    append_owned_text(&mut last.text, text);
                    trim_owned(&mut last.text);
                    self.continuation_open = !paragraph_end;
                    self.pending_space = text
                        .chars()
                        .next_back()
                        .map(|ch| ch.is_whitespace())
                        .unwrap_or(false);
                    return Ok(());
                }
            }
            let mut owned = heapless::String::<BLOCK_LEN>::new();
            append_owned_text(&mut owned, text);
            let result = flush_owned_block(&mut owned, role, style, align, self.output);
            self.continuation_open = !paragraph_end;
            self.pending_space = text
                .chars()
                .next_back()
                .map(|ch| ch.is_whitespace())
                .unwrap_or(false);
            result
        }
    }

    let mut sink = VecSink {
        output,
        continuation_open: false,
        pending_space: false,
    };
    xhtml_blocks_to_sink(xhtml, css, &mut sink)
}

pub fn xhtml_blocks_to_sink(
    xhtml: &str,
    css: Option<&CssRules>,
    sink: &mut impl XhtmlBlockSink,
) -> Result<(), XhtmlError> {
    let body_required = xhtml.contains("<body") || xhtml.contains(":body");
    let mut parser = XhtmlBlockStreamParser::new(!body_required);
    let mut cursor = XmlCursor::new(xhtml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) => parser.handle_start(tag, css, sink)?,
            Token::End(tag) => parser.handle_end(tag, sink)?,
            Token::Text(text) => parser.handle_text(text, sink)?,
        }
    }
    parser.finish(sink)
}

const SKIP_TAG_NAME_BYTES: usize = 24;

/// Resumable form of the XHTML block state machine: the one extraction
/// path behind both [`xhtml_blocks_to_sink`] (whole document in RAM) and
/// the streamed front-end on [`StreamingXmlTokenizer`], which lets
/// firmware decode spine members of any size through a bounded window.
pub struct XhtmlBlockStreamParser {
    block: heapless::String<384>,
    role: TextRole,
    align: TextAlign,
    bold_depth: u8,
    italic_depth: u8,
    in_body: bool,
    skip_depth: u8,
    skip_tag: Option<heapless::String<SKIP_TAG_NAME_BYTES>>,
    list_kind_stack: [ListKind; 8],
    list_count_stack: [u16; 8],
    list_depth: usize,
    table_align_stack: [TextAlign; 4],
    table_depth: usize,
}

impl XhtmlBlockStreamParser {
    /// `assume_in_body` mirrors the whole-document behavior for inputs
    /// without a `<body>` element: callers that cannot scan ahead decide
    /// from whatever prefix they have.
    pub fn new(assume_in_body: bool) -> Self {
        Self {
            block: heapless::String::new(),
            role: TextRole::Body,
            align: TextAlign::Justify,
            bold_depth: 0,
            italic_depth: 0,
            in_body: assume_in_body,
            skip_depth: 0,
            skip_tag: None,
            list_kind_stack: [ListKind::Unordered; 8],
            list_count_stack: [0u16; 8],
            list_depth: 0,
            table_align_stack: [TextAlign::Justify; 4],
            table_depth: 0,
        }
    }

    pub fn handle_start(
        &mut self,
        tag: &str,
        css: Option<&CssRules>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if tag_name_is(tag, "body") {
            self.in_body = true;
            return Ok(());
        }
        if self.skip_depth == 0
            && (tag_name_is(tag, "head")
                || tag_name_is(tag, "style")
                || tag_name_is(tag, "script")
                || tag_name_is(tag, "svg")
                || tag_name_is(tag, "nav")
                || tag_is_hidden(tag)
                || tag_is_pagebreak(tag))
        {
            if !tag_is_void(tag) {
                self.skip_tag = tag_local_name(tag).map(bounded_skip_name);
                self.skip_depth = self.skip_depth.saturating_add(1);
            }
            return Ok(());
        }
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        if tag_name_is(tag, "table") {
            self.flush(sink)?;
            if self.table_depth < self.table_align_stack.len() {
                self.table_align_stack[self.table_depth] =
                    table_align_for_tag(tag, css).unwrap_or(TextAlign::Justify);
                self.table_depth += 1;
            }
        } else if tag_name_is(tag, "td") || tag_name_is(tag, "th") {
            append_table_cell_separator(&mut self.block);
        } else if tag_name_is(tag, "br") {
            self.flush(sink)?;
        } else if tag_name_is(tag, "img") {
            self.flush(sink)?;
            let placeholder = attr_value(tag, "alt").unwrap_or("[Image]");
            sink.push_block(
                placeholder,
                TextRole::Body,
                FontStyle::Italic,
                TextAlign::Center,
                true,
            )?;
        } else if tag_name_is(tag, "ul") || tag_name_is(tag, "ol") {
            self.flush(sink)?;
            if self.list_depth < self.list_kind_stack.len() {
                self.list_kind_stack[self.list_depth] = if tag_name_is(tag, "ol") {
                    ListKind::Ordered
                } else {
                    ListKind::Unordered
                };
                self.list_count_stack[self.list_depth] = 0;
                self.list_depth += 1;
            }
        } else if tag_starts_block(tag) {
            self.flush(sink)?;
            self.align = block_align_for_tag(tag, css)
                .or_else(|| current_table_align(&self.table_align_stack, self.table_depth))
                .unwrap_or(TextAlign::Justify);
            if tag_name_is(tag, "li") {
                append_list_marker(
                    &mut self.block,
                    &mut self.list_count_stack,
                    &self.list_kind_stack,
                    self.list_depth,
                );
            }
        } else if tag_name_is(tag, "h1") {
            self.flush(sink)?;
            self.role = TextRole::Heading1;
            self.align = TextAlign::Center;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_name_is(tag, "h2") {
            self.flush(sink)?;
            self.role = TextRole::Heading2;
            self.align = TextAlign::Center;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_name_is(tag, "h3")
            || tag_name_is(tag, "h4")
            || tag_name_is(tag, "h5")
            || tag_name_is(tag, "h6")
        {
            self.flush(sink)?;
            self.role = TextRole::Heading3;
            self.align = TextAlign::Center;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_name_is(tag, "blockquote") {
            self.flush(sink)?;
            self.role = TextRole::BlockQuote;
            self.align = block_align_for_tag(tag, css).unwrap_or(TextAlign::Left);
            self.italic_depth = self.italic_depth.saturating_add(1);
        } else if tag_name_is(tag, "strong") || tag_name_is(tag, "b") {
            self.flush_continue(sink)?;
            self.bold_depth = self.bold_depth.saturating_add(1);
        } else if tag_is_italic(tag) {
            self.flush_continue(sink)?;
            self.italic_depth = self.italic_depth.saturating_add(1);
        }
        Ok(())
    }

    pub fn handle_end(
        &mut self,
        tag: &str,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if tag_name_is(tag, "body") {
            self.flush(sink)?;
            self.in_body = false;
            return Ok(());
        }
        if self.skip_tag_matches(tag) {
            self.skip_depth = self.skip_depth.saturating_sub(1);
            if self.skip_depth == 0 {
                self.skip_tag = None;
            }
            return Ok(());
        }
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        if tag_name_is(tag, "table") {
            self.flush(sink)?;
            self.table_depth = self.table_depth.saturating_sub(1);
        } else if tag_name_is(tag, "ul") || tag_name_is(tag, "ol") {
            self.flush(sink)?;
            self.list_depth = self.list_depth.saturating_sub(1);
        } else if tag_name_is(tag, "h1")
            || tag_name_is(tag, "h2")
            || tag_name_is(tag, "h3")
            || tag_name_is(tag, "h4")
            || tag_name_is(tag, "h5")
            || tag_name_is(tag, "h6")
        {
            self.flush(sink)?;
            self.role = TextRole::Body;
            self.bold_depth = self.bold_depth.saturating_sub(1);
            self.align = TextAlign::Justify;
        } else if tag_name_is(tag, "blockquote") {
            self.flush(sink)?;
            self.role = TextRole::Body;
            self.italic_depth = self.italic_depth.saturating_sub(1);
            self.align = TextAlign::Justify;
        } else if tag_name_is(tag, "strong") || tag_name_is(tag, "b") {
            self.flush_continue(sink)?;
            self.bold_depth = self.bold_depth.saturating_sub(1);
        } else if tag_is_italic(tag) {
            self.flush_continue(sink)?;
            self.italic_depth = self.italic_depth.saturating_sub(1);
        } else if tag_ends_block(tag) {
            self.flush(sink)?;
            self.align = TextAlign::Justify;
        }
        Ok(())
    }

    pub fn handle_text(
        &mut self,
        text: &str,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if !self.in_body || self.skip_depth > 0 {
            return Ok(());
        }
        append_text_to_sink_block(
            &mut self.block,
            text,
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            sink,
        )
    }

    pub fn finish(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        self.flush(sink)
    }

    fn flush(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        flush_sink_block(
            &mut self.block,
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            sink,
        )
    }

    fn flush_continue(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        flush_sink_block_continue(
            &mut self.block,
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            sink,
        )
    }

    fn skip_tag_matches(&self, tag: &str) -> bool {
        let Some(stored) = self.skip_tag.as_ref() else {
            return false;
        };
        tag_local_name(tag)
            .map(|name| bounded_skip_name(name).as_str() == stored.as_str())
            .unwrap_or(false)
    }
}

/// Skip-tag names are stored owned (streaming input has no stable slice
/// to borrow); both the opening and closing side truncate identically so
/// overlong names still pair up.
fn bounded_skip_name(name: &str) -> heapless::String<SKIP_TAG_NAME_BYTES> {
    let mut out = heapless::String::new();
    for ch in name.chars() {
        if out.push(ch).is_err() {
            break;
        }
    }
    out
}

pub fn parse_epub3_nav_to_sink(xhtml: &str, sink: &mut impl EpubTocSink) -> Result<(), TocError> {
    let mut cursor = XmlCursor::new(xhtml);
    let body_required = xhtml.contains("<body") || xhtml.contains(":body");
    let mut in_body = !body_required;
    let mut in_nav = false;
    let mut skip_depth = 0u8;
    let mut skip_tag: Option<&str> = None;
    let mut list_depth = 0u8;
    let mut href: Option<&str> = None;
    let mut title = heapless::String::<160>::new();
    let mut level = 0u8;

    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "body") => in_body = true,
            Token::End(tag) if tag_name_is(tag, "body") => in_body = false,
            Token::Start(tag)
                if skip_depth == 0
                    && (tag_name_is(tag, "head")
                        || tag_name_is(tag, "style")
                        || tag_name_is(tag, "script")
                        || tag_name_is(tag, "svg")
                        || tag_is_hidden(tag))
                    && !tag_is_void(tag) =>
            {
                skip_tag = tag_local_name(tag);
                skip_depth = skip_depth.saturating_add(1);
            }
            Token::End(tag) if skip_tag.map(|name| tag_name_is(tag, name)).unwrap_or(false) => {
                skip_depth = skip_depth.saturating_sub(1);
                if skip_depth == 0 {
                    skip_tag = None;
                }
            }
            _ if !in_body || skip_depth > 0 => {}
            Token::Start(tag) if tag_name_is(tag, "nav") => in_nav = nav_start_is_toc(tag),
            Token::End(tag) if tag_name_is(tag, "nav") => in_nav = false,
            Token::Start(tag) if in_nav && tag_name_is(tag, "ol") => {
                list_depth = list_depth.saturating_add(1);
            }
            Token::End(tag) if in_nav && tag_name_is(tag, "ol") => {
                list_depth = list_depth.saturating_sub(1);
            }
            Token::Start(tag) if in_nav && tag_name_is(tag, "a") => {
                href = attr_value(tag, "href");
                title.clear();
                level = list_depth.max(1);
            }
            Token::Text(text) if href.is_some() => append_owned_text(&mut title, text),
            Token::End(tag) if href.is_some() && tag_name_is(tag, "a") => {
                if let Some(found_href) = href.take() {
                    let text = title.as_str().trim();
                    if !text.is_empty() && !found_href.is_empty() {
                        sink.push_toc(text, found_href, level)?;
                    }
                }
                title.clear();
            }
            _ => {}
        }
    }

    Ok(())
}

pub fn parse_epub2_ncx_to_sink(ncx: &str, sink: &mut impl EpubTocSink) -> Result<(), TocError> {
    let mut cursor = XmlCursor::new(ncx);
    let mut nav_depth = 0u8;
    let mut item_open = false;
    let mut item_pushed = false;
    let mut in_text = false;
    let mut title = heapless::String::<160>::new();
    let mut href: Option<&str> = None;

    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "navPoint") => {
                if item_open && !item_pushed {
                    push_pending_ncx_item(sink, title.as_str(), href, nav_depth)?;
                }
                nav_depth = nav_depth.saturating_add(1);
                item_open = true;
                item_pushed = false;
                in_text = false;
                title.clear();
                href = None;
            }
            Token::End(tag) if tag_name_is(tag, "navPoint") => {
                if item_open && !item_pushed {
                    push_pending_ncx_item(sink, title.as_str(), href, nav_depth)?;
                }
                nav_depth = nav_depth.saturating_sub(1);
                item_open = nav_depth > 0;
                item_pushed = false;
                in_text = false;
                title.clear();
                href = None;
            }
            Token::Start(tag) if item_open && tag_name_is(tag, "text") => in_text = true,
            Token::End(tag) if tag_name_is(tag, "text") => in_text = false,
            Token::Start(tag) if item_open && tag_name_is(tag, "content") => {
                href = attr_value(tag, "src");
                if !item_pushed && !title.as_str().trim().is_empty() {
                    push_pending_ncx_item(sink, title.as_str(), href, nav_depth)?;
                    item_pushed = true;
                }
            }
            Token::Text(text) if in_text => append_owned_text(&mut title, text),
            _ => {}
        }
    }

    Ok(())
}

fn push_pending_ncx_item(
    sink: &mut impl EpubTocSink,
    title: &str,
    href: Option<&str>,
    level: u8,
) -> Result<(), TocError> {
    let title = title.trim();
    let href = href.unwrap_or("").trim();
    if title.is_empty() || href.is_empty() {
        return Ok(());
    }
    sink.push_toc(title, href, level.max(1))
}

// === Streaming TOC parsing ===
//
// The slice-based parsers above require the full NCX/nav file to fit in a
// single RAM buffer. For books with large tables of contents (HPMOR's NCX is
// 53 KB, well over the 24 KB XHTML scratch on the C3) that's a hard ceiling.
// The streaming variants below consume bytes incrementally from a `ByteStream`
// using a small XML tokenizer with bounded internal buffers, so peak memory is
// a function of the longest single tag/title rather than the file size.

const STREAM_TAG_BUF_BYTES: usize = 1024;
const STREAM_TEXT_CHUNK_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TocStreamError {
    Tokenizer,
    Toc(TocError),
}

impl From<TocError> for TocStreamError {
    fn from(value: TocError) -> Self {
        Self::Toc(value)
    }
}

#[derive(Clone, Copy)]
enum TokState {
    Text,
    Tag,
    TagQuoted(u8),
}

enum TokEvent<'a> {
    StartTag(&'a str),
    EndTag(&'a str),
    Text(&'a str),
}

/// Longest text suffix held back at a forced mid-text flush so an HTML
/// entity is never split across two Text events ("&#x10FFFF;" is 10
/// bytes; named entities top out around "&hellip;").
const STREAM_ENTITY_HOLDBACK_BYTES: usize = 12;

/// Byte-level XML tokenizer. Emits StartTag/EndTag/Text events with all
/// content held in bounded internal buffers — never borrows from the caller's
/// input. Comments/PI/doctype tags are treated as opaque skip-until-`>` blocks,
/// matching the imperfections (but bounded behaviour) of [`XmlCursor`].
///
/// Text accumulates as raw bytes: a forced flush when the buffer fills
/// holds back incomplete UTF-8 sequences and incomplete entities, so the
/// emitted `&str` chunks are always valid UTF-8 and never split an
/// entity, regardless of how the input is chunked.
pub struct StreamingXmlTokenizer {
    state: TokState,
    tag_buf: heapless::String<STREAM_TAG_BUF_BYTES>,
    text_buf: heapless::Vec<u8, STREAM_TEXT_CHUNK_BYTES>,
    tag_overflow: bool,
}

impl Default for StreamingXmlTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingXmlTokenizer {
    pub fn new() -> Self {
        Self {
            state: TokState::Text,
            tag_buf: heapless::String::new(),
            text_buf: heapless::Vec::new(),
            tag_overflow: false,
        }
    }

    pub fn feed_ncx(
        &mut self,
        bytes: &[u8],
        parser: &mut NcxStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.feed(bytes, &mut |event| parser.handle(event, sink))
    }

    pub fn finish_ncx(
        &mut self,
        parser: &mut NcxStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.finish(&mut |event| parser.handle(event, sink))
    }

    pub fn feed_nav(
        &mut self,
        bytes: &[u8],
        parser: &mut Epub3NavStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.feed(bytes, &mut |event| parser.handle(event, sink))
    }

    pub fn finish_nav(
        &mut self,
        parser: &mut Epub3NavStreamParser,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        self.finish(&mut |event| parser.handle(event, sink))
    }

    pub fn feed_xhtml_blocks(
        &mut self,
        bytes: &[u8],
        parser: &mut XhtmlBlockStreamParser,
        css: Option<&CssRules>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        self.feed(bytes, &mut |event| match event {
            TokEvent::StartTag(tag) => parser.handle_start(tag, css, sink),
            TokEvent::EndTag(tag) => parser.handle_end(tag, sink),
            TokEvent::Text(text) => parser.handle_text(text, sink),
        })
    }

    pub fn finish_xhtml_blocks(
        &mut self,
        parser: &mut XhtmlBlockStreamParser,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        self.finish(&mut |event| match event {
            TokEvent::StartTag(_) | TokEvent::EndTag(_) => Ok(()),
            TokEvent::Text(text) => parser.handle_text(text, sink),
        })?;
        parser.finish(sink)
    }

    fn feed<F, E>(&mut self, bytes: &[u8], emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        for &byte in bytes {
            self.consume(byte, emit)?;
        }
        Ok(())
    }

    fn finish<F, E>(&mut self, emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        if matches!(self.state, TokState::Text) {
            self.flush_text(emit, true)?;
        }
        Ok(())
    }

    fn consume<F, E>(&mut self, byte: u8, emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        match self.state {
            TokState::Text => {
                if byte == b'<' {
                    self.flush_text(emit, true)?;
                    self.state = TokState::Tag;
                    self.tag_buf.clear();
                    self.tag_overflow = false;
                } else {
                    self.push_text_byte(byte, emit)?;
                }
            }
            TokState::Tag => {
                if byte == b'>' {
                    if !self.tag_overflow {
                        let tag = self.tag_buf.as_str().trim();
                        if !(tag.starts_with('!') || tag.starts_with('?')) {
                            if let Some(name) = tag.strip_prefix('/') {
                                emit(TokEvent::EndTag(name.trim()))?;
                            } else if !tag.is_empty() {
                                emit(TokEvent::StartTag(tag))?;
                            }
                        }
                    }
                    self.tag_buf.clear();
                    self.tag_overflow = false;
                    self.state = TokState::Text;
                } else if byte == b'"' || byte == b'\'' {
                    self.push_tag_byte(byte);
                    self.state = TokState::TagQuoted(byte);
                } else {
                    self.push_tag_byte(byte);
                }
            }
            TokState::TagQuoted(quote) => {
                self.push_tag_byte(byte);
                if byte == quote {
                    self.state = TokState::Tag;
                }
            }
        }
        Ok(())
    }

    fn push_tag_byte(&mut self, byte: u8) {
        if self.tag_overflow {
            return;
        }
        if self.tag_buf.push(byte as char).is_err() {
            self.tag_overflow = true;
        }
    }

    fn push_text_byte<F, E>(&mut self, byte: u8, emit: &mut F) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        if self.text_buf.push(byte).is_err() {
            self.flush_text(emit, false)?;
            let _ = self.text_buf.push(byte);
        }
        Ok(())
    }

    /// Emit buffered text as valid UTF-8. At a tag boundary the whole
    /// buffer drains (a malformed trailing sequence cannot be completed by
    /// later bytes and is dropped). At a forced mid-text flush, incomplete
    /// UTF-8 sequences and incomplete trailing entities are held back so
    /// later bytes can complete them. Invalid bytes are skipped.
    fn flush_text<F, E>(&mut self, emit: &mut F, at_tag_boundary: bool) -> Result<(), E>
    where
        F: FnMut(TokEvent<'_>) -> Result<(), E>,
    {
        loop {
            if self.text_buf.is_empty() {
                return Ok(());
            }
            let (valid_len, invalid_skip) = match core::str::from_utf8(&self.text_buf) {
                Ok(_) => (self.text_buf.len(), 0),
                Err(error) => (error.valid_up_to(), error.error_len().unwrap_or(0)),
            };
            if invalid_skip > 0 {
                if let Ok(text) = core::str::from_utf8(&self.text_buf[..valid_len]) {
                    if !text.is_empty() {
                        emit(TokEvent::Text(text))?;
                    }
                }
                self.drain_text_prefix(valid_len + invalid_skip);
                continue;
            }

            let mut emit_len = valid_len;
            if !at_tag_boundary {
                if let Ok(text) = core::str::from_utf8(&self.text_buf[..valid_len]) {
                    if let Some(amp) = text.rfind('&') {
                        let tail = &text[amp..];
                        if !tail.contains(';') && tail.len() <= STREAM_ENTITY_HOLDBACK_BYTES {
                            emit_len = amp;
                        }
                    }
                }
            }
            if emit_len > 0 {
                if let Ok(text) = core::str::from_utf8(&self.text_buf[..emit_len]) {
                    if !text.is_empty() {
                        emit(TokEvent::Text(text))?;
                    }
                }
            }
            if at_tag_boundary {
                self.text_buf.clear();
            } else {
                self.drain_text_prefix(emit_len);
            }
            return Ok(());
        }
    }

    fn drain_text_prefix(&mut self, prefix_len: usize) {
        let len = self.text_buf.len();
        let prefix_len = prefix_len.min(len);
        self.text_buf.as_mut_slice().copy_within(prefix_len.., 0);
        self.text_buf.truncate(len - prefix_len);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NcxItemState {
    Closed,
    Open,
    Pushed,
}

pub struct NcxStreamParser {
    nav_depth: u8,
    item: NcxItemState,
    in_text: bool,
    title: heapless::String<160>,
    href: heapless::String<256>,
    href_set: bool,
}

impl Default for NcxStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NcxStreamParser {
    pub fn new() -> Self {
        Self {
            nav_depth: 0,
            item: NcxItemState::Closed,
            in_text: false,
            title: heapless::String::new(),
            href: heapless::String::new(),
            href_set: false,
        }
    }

    fn handle(
        &mut self,
        event: TokEvent<'_>,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        match event {
            TokEvent::StartTag(tag) if tag_name_is(tag, "navPoint") => {
                if matches!(self.item, NcxItemState::Open) {
                    self.flush_to_sink(sink)?;
                }
                self.nav_depth = self.nav_depth.saturating_add(1);
                self.item = NcxItemState::Open;
                self.in_text = false;
                self.title.clear();
                self.href.clear();
                self.href_set = false;
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "navPoint") => {
                if matches!(self.item, NcxItemState::Open) {
                    self.flush_to_sink(sink)?;
                }
                self.nav_depth = self.nav_depth.saturating_sub(1);
                self.item = if self.nav_depth > 0 {
                    NcxItemState::Open
                } else {
                    NcxItemState::Closed
                };
                self.in_text = false;
                self.title.clear();
                self.href.clear();
                self.href_set = false;
            }
            TokEvent::StartTag(tag)
                if matches!(self.item, NcxItemState::Open | NcxItemState::Pushed)
                    && tag_name_is(tag, "text") =>
            {
                self.in_text = true;
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "text") => {
                self.in_text = false;
            }
            TokEvent::StartTag(tag)
                if matches!(self.item, NcxItemState::Open) && tag_name_is(tag, "content") =>
            {
                if let Some(src) = attr_value(tag, "src") {
                    self.href.clear();
                    let _ = push_str_bounded(&mut self.href, src);
                    self.href_set = true;
                }
                if !self.title.as_str().trim().is_empty() && self.href_set {
                    self.flush_to_sink(sink)?;
                    self.item = NcxItemState::Pushed;
                }
            }
            TokEvent::Text(text) if self.in_text => {
                append_owned_text(&mut self.title, text);
            }
            _ => {}
        }
        Ok(())
    }

    fn flush_to_sink(&mut self, sink: &mut impl EpubTocSink) -> Result<(), TocStreamError> {
        let title = self.title.as_str().trim();
        let href = self.href.as_str().trim();
        if title.is_empty() || href.is_empty() {
            return Ok(());
        }
        sink.push_toc(title, href, self.nav_depth.max(1))?;
        Ok(())
    }
}

pub struct Epub3NavStreamParser {
    in_body: bool,
    body_required: bool,
    in_nav: bool,
    list_depth: u8,
    href: heapless::String<256>,
    href_set: bool,
    title: heapless::String<160>,
    level: u8,
}

impl Default for Epub3NavStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Epub3NavStreamParser {
    pub fn new() -> Self {
        Self {
            in_body: true,
            body_required: false,
            in_nav: false,
            list_depth: 0,
            href: heapless::String::new(),
            href_set: false,
            title: heapless::String::new(),
            level: 0,
        }
    }

    fn handle(
        &mut self,
        event: TokEvent<'_>,
        sink: &mut impl EpubTocSink,
    ) -> Result<(), TocStreamError> {
        match event {
            TokEvent::StartTag(tag) if tag_name_is(tag, "body") => {
                self.body_required = true;
                self.in_body = true;
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "body") => {
                self.in_body = false;
            }
            _ if self.body_required && !self.in_body => {}
            TokEvent::StartTag(tag) if tag_name_is(tag, "nav") => {
                self.in_nav = nav_start_is_toc(tag);
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "nav") => self.in_nav = false,
            TokEvent::StartTag(tag) if self.in_nav && tag_name_is(tag, "ol") => {
                self.list_depth = self.list_depth.saturating_add(1);
            }
            TokEvent::EndTag(tag) if self.in_nav && tag_name_is(tag, "ol") => {
                self.list_depth = self.list_depth.saturating_sub(1);
            }
            TokEvent::StartTag(tag) if self.in_nav && tag_name_is(tag, "a") => {
                self.href.clear();
                self.href_set = false;
                if let Some(value) = attr_value(tag, "href") {
                    let _ = push_str_bounded(&mut self.href, value);
                    self.href_set = true;
                }
                self.title.clear();
                self.level = self.list_depth.max(1);
            }
            TokEvent::Text(text) if self.href_set => {
                append_owned_text(&mut self.title, text);
            }
            TokEvent::EndTag(tag) if self.href_set && tag_name_is(tag, "a") => {
                let title = self.title.as_str().trim();
                let href = self.href.as_str().trim();
                if !title.is_empty() && !href.is_empty() {
                    sink.push_toc(title, href, self.level)?;
                }
                self.title.clear();
                self.href.clear();
                self.href_set = false;
            }
            _ => {}
        }
        Ok(())
    }
}

fn push_str_bounded<const N: usize>(out: &mut heapless::String<N>, value: &str) -> Result<(), ()> {
    for ch in value.chars() {
        if out.push(ch).is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// Parse an EPUB 2 NCX directly from a byte stream into an [`EpubTocSink`].
/// Memory use is bounded by the tokenizer's tag/text buffers; the whole file
/// is never resident in RAM.
pub fn parse_epub2_ncx_stream<R>(
    reader: &mut R,
    sink: &mut impl EpubTocSink,
) -> Result<(), TocStreamError>
where
    R: ByteStream,
{
    let mut tokenizer = StreamingXmlTokenizer::new();
    let mut parser = NcxStreamParser::new();
    let mut buf = [0u8; 512];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| TocStreamError::Tokenizer)?;
        if n == 0 {
            break;
        }
        let bytes = &buf[..n];
        tokenizer.feed(bytes, &mut |event| parser.handle(event, sink))?;
    }
    tokenizer.finish(&mut |event| parser.handle(event, sink))?;
    Ok(())
}

/// Parse an EPUB 3 nav document directly from a byte stream.
pub fn parse_epub3_nav_stream<R>(
    reader: &mut R,
    sink: &mut impl EpubTocSink,
) -> Result<(), TocStreamError>
where
    R: ByteStream,
{
    let mut tokenizer = StreamingXmlTokenizer::new();
    let mut parser = Epub3NavStreamParser::new();
    let mut buf = [0u8; 512];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| TocStreamError::Tokenizer)?;
        if n == 0 {
            break;
        }
        let bytes = &buf[..n];
        tokenizer.feed(bytes, &mut |event| parser.handle(event, sink))?;
    }
    tokenizer.finish(&mut |event| parser.handle(event, sink))?;
    Ok(())
}

/// Slice-backed [`ByteStream`] used by tests and the slice-based parser
/// wrappers below.
pub struct SliceByteStream<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> SliceByteStream<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
}

impl ByteStream for SliceByteStream<'_> {
    type Error = ();

    fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        let remaining = self.bytes.len().saturating_sub(self.cursor);
        let take = remaining.min(out.len());
        out[..take].copy_from_slice(&self.bytes[self.cursor..self.cursor + take]);
        self.cursor += take;
        Ok(take)
    }
}

pub fn parse_css_text_align(css: &str, rules: &mut CssRules) {
    let mut cursor = 0usize;
    while let Some(open_rel) = css[cursor..].find('{') {
        let selector_text = css[cursor..cursor + open_rel].trim();
        let body_start = cursor + open_rel + 1;
        let Some(close_rel) = css[body_start..].find('}') else {
            break;
        };
        let body = &css[body_start..body_start + close_rel];
        if let Some(align) = parse_text_align_property(body) {
            for selector in selector_text.split(',') {
                rules.push_text_align(selector, align);
            }
        }
        cursor = body_start + close_rel + 1;
    }
}

fn tag_name_is(tag: &str, expected: &str) -> bool {
    tag_local_name(tag)
        .map(|name| name.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn tag_local_name(tag: &str) -> Option<&str> {
    let tag = tag.split_whitespace().next().unwrap_or(tag);
    Some(tag.trim_end_matches('/').rsplit(':').next().unwrap_or(tag))
}

/// Whether this `<nav>` start tag is the reading table of contents.
///
/// An EPUB 3 navigation document usually holds several `<nav>` elements:
/// `toc`, `page-list` (page markers like `i`, `ii`, `1`, `2`), landmarks, and
/// sometimes back-matter indices. Only the reading TOC lists chapters. EPUBs
/// are not consistent about where they put that meaning: some use `epub:type`,
/// others use ARIA/doc roles, labels, ids, or classes, so keep both the
/// positive and negative checks here.
fn nav_start_is_toc(tag: &str) -> bool {
    if let Some(value) = attr_value(tag, "epub:type") {
        return value
            .split_ascii_whitespace()
            .any(|word| word.eq_ignore_ascii_case("toc"));
    }

    if nav_attr_has_any(tag, "role", &["doc-toc", "toc"])
        || nav_attr_has_any(tag, "aria-label", &["toc", "contents", "tableofcontents"])
        || nav_attr_has_any(tag, "id", &["toc", "contents", "tableofcontents"])
        || nav_attr_has_any(tag, "class", &["toc", "contents", "tableofcontents"])
    {
        return true;
    }

    !nav_attr_has_any(
        tag,
        "role",
        &[
            "doc-pagelist",
            "pagelist",
            "page-list",
            "doc-index",
            "index",
            "doc-landmarks",
            "landmarks",
            "doc-loi",
            "loi",
            "doc-lot",
            "lot",
        ],
    ) && !nav_attr_has_any(
        tag,
        "aria-label",
        &[
            "pagelist",
            "page-list",
            "pages",
            "index",
            "landmarks",
            "listofillustrations",
            "listoftables",
        ],
    ) && !nav_attr_has_any(
        tag,
        "id",
        &[
            "pagelist",
            "page-list",
            "page_list",
            "pages",
            "index",
            "landmarks",
            "loi",
            "lot",
        ],
    ) && !nav_attr_has_any(
        tag,
        "class",
        &[
            "pagelist",
            "page-list",
            "page_list",
            "pages",
            "index",
            "landmarks",
            "loi",
            "lot",
        ],
    )
}

fn nav_attr_has_any(tag: &str, attr: &str, markers: &[&str]) -> bool {
    let Some(value) = attr_value(tag, attr) else {
        return false;
    };
    markers
        .iter()
        .any(|marker| ascii_contains_normalized(value, marker))
}

fn ascii_contains_normalized(value: &str, marker: &str) -> bool {
    let marker_len = marker
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .count();
    if marker_len == 0 {
        return false;
    }
    let mut matched = 0usize;
    for byte in value.bytes() {
        if !byte.is_ascii_alphanumeric() {
            continue;
        }
        let Some(expected) = normalized_marker_byte(marker, matched) else {
            return true;
        };
        if byte.eq_ignore_ascii_case(&expected) {
            matched += 1;
            if matched == marker_len {
                return true;
            }
        } else {
            let first = normalized_marker_byte(marker, 0).unwrap_or(expected);
            matched = usize::from(byte.eq_ignore_ascii_case(&first));
        }
    }
    false
}

fn normalized_marker_byte(marker: &str, index: usize) -> Option<u8> {
    marker
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .nth(index)
}

fn tag_is_hidden(tag: &str) -> bool {
    tag.contains("display:none")
        || tag.contains("display: none")
        || tag.contains("visibility:hidden")
        || tag.contains("visibility: hidden")
        || tag.contains("hidden=\"hidden\"")
        || tag.contains("hidden='hidden'")
        || tag.contains("hidden ")
        || tag.ends_with(" hidden")
        || attr_value(tag, "aria-hidden")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

fn tag_is_pagebreak(tag: &str) -> bool {
    tag_contains_class_word(tag, "pagebreak")
        || tag_contains_class_word(tag, "pagenum")
        || attr_value(tag, "role")
            .map(|value| value.eq_ignore_ascii_case("doc-pagebreak"))
            .unwrap_or(false)
        || attr_value(tag, "epub:type")
            .map(|value| {
                value
                    .split_ascii_whitespace()
                    .any(|word| word.eq_ignore_ascii_case("pagebreak"))
            })
            .unwrap_or(false)
}

fn tag_is_void(tag: &str) -> bool {
    tag.trim_end().ends_with('/')
        || tag_name_is(tag, "br")
        || tag_name_is(tag, "hr")
        || tag_name_is(tag, "img")
        || tag_name_is(tag, "meta")
        || tag_name_is(tag, "link")
        || tag_name_is(tag, "input")
}

fn tag_is_italic(tag: &str) -> bool {
    tag_name_is(tag, "em")
        || tag_name_is(tag, "i")
        || tag_name_is(tag, "code")
        || tag_name_is(tag, "tt")
        || tag_name_is(tag, "kbd")
        || tag_name_is(tag, "samp")
}

fn block_align_for_tag(tag: &str, css: Option<&CssRules>) -> Option<TextAlign> {
    if tag_contains_attr_value(tag, "align", "center")
        || tag_contains_class_word(tag, "center")
        || tag_contains_class_word(tag, "centered")
        || tag_contains_class_word(tag, "title")
        || tag_contains_style_value(tag, "text-align", "center")
    {
        Some(TextAlign::Center)
    } else {
        css.and_then(|rules| rules.alignment_for(tag))
    }
}

fn table_align_for_tag(tag: &str, css: Option<&CssRules>) -> Option<TextAlign> {
    if tag_contains_style_value(tag, "margin-left", "auto")
        && tag_contains_style_value(tag, "margin-right", "auto")
    {
        Some(TextAlign::Center)
    } else {
        block_align_for_tag(tag, css)
    }
}

fn current_table_align(stack: &[TextAlign; 4], depth: usize) -> Option<TextAlign> {
    if depth == 0 {
        None
    } else {
        Some(stack[depth.min(stack.len()) - 1])
    }
}

fn selector_is_supported(selector: &str) -> bool {
    !selector.is_empty()
        && !selector
            .as_bytes()
            .iter()
            .any(|byte| matches!(*byte, b'+' | b'>' | b'[' | b':' | b'#' | b'~' | b'*' | b' '))
}

fn parse_text_align_property(body: &str) -> Option<TextAlign> {
    for declaration in body.split(';') {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("text-align") {
            continue;
        }
        let value = value.trim();
        if value.eq_ignore_ascii_case("center") {
            return Some(TextAlign::Center);
        }
        if value.eq_ignore_ascii_case("left")
            || value.eq_ignore_ascii_case("right")
            || value.eq_ignore_ascii_case("start")
            || value.eq_ignore_ascii_case("end")
        {
            return Some(TextAlign::Left);
        }
        if value.eq_ignore_ascii_case("justify") {
            return Some(TextAlign::Justify);
        }
    }
    None
}

fn tag_contains_attr_value(tag: &str, attr: &str, value: &str) -> bool {
    attr_value(tag, attr)
        .map(|found| found.eq_ignore_ascii_case(value))
        .unwrap_or(false)
}

fn tag_contains_class_word(tag: &str, word: &str) -> bool {
    attr_value(tag, "class")
        .map(|classes| {
            classes
                .split_ascii_whitespace()
                .any(|class| class.eq_ignore_ascii_case(word))
        })
        .unwrap_or(false)
}

fn tag_contains_style_value(tag: &str, property: &str, expected: &str) -> bool {
    let Some(style) = attr_value(tag, "style") else {
        return false;
    };
    style.split(';').any(|declaration| {
        let Some((name, value)) = declaration.split_once(':') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case(property)
            && value
                .trim()
                .split_ascii_whitespace()
                .next()
                .map(|value| value.eq_ignore_ascii_case(expected))
                .unwrap_or(false)
    })
}

fn tag_starts_block(tag: &str) -> bool {
    tag_name_is(tag, "p")
        || tag_name_is(tag, "li")
        || tag_name_is(tag, "div")
        || tag_name_is(tag, "section")
        || tag_name_is(tag, "article")
        || tag_name_is(tag, "pre")
        || tag_name_is(tag, "dt")
        || tag_name_is(tag, "dd")
        || tag_name_is(tag, "tr")
}

fn tag_ends_block(tag: &str) -> bool {
    tag_name_is(tag, "p")
        || tag_name_is(tag, "li")
        || tag_name_is(tag, "div")
        || tag_name_is(tag, "section")
        || tag_name_is(tag, "article")
        || tag_name_is(tag, "pre")
        || tag_name_is(tag, "dt")
        || tag_name_is(tag, "dd")
        || tag_name_is(tag, "tr")
}

fn append_owned_text<const N: usize>(out: &mut heapless::String<N>, text: &str) {
    let mut previous_space = out
        .as_str()
        .chars()
        .last()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let (ch, advance) = if let Some(decoded) = decode_html_entity(rest) {
            (decoded, rest.find(';').map(|index| index + 1).unwrap_or(1))
        } else {
            let Some(ch) = rest.chars().next() else {
                break;
            };
            (ch, ch.len_utf8())
        };
        if ch.is_whitespace() {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else {
            if out.push(ch).is_err() {
                break;
            }
            previous_space = false;
        }
        cursor += advance;
    }
}

fn is_leading_punctuation_char(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
    )
}

fn append_text_to_sink_block<const N: usize>(
    out: &mut heapless::String<N>,
    text: &str,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
) -> Result<(), XhtmlError> {
    let mut previous_space = out
        .as_str()
        .chars()
        .last()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let (ch, advance) = if let Some(decoded) = decode_html_entity(rest) {
            (decoded, rest.find(';').map(|index| index + 1).unwrap_or(1))
        } else {
            let Some(ch) = rest.chars().next() else {
                break;
            };
            (ch, ch.len_utf8())
        };
        let should_push_space = ch.is_whitespace() && !previous_space;
        let should_push_char = !ch.is_whitespace();
        if should_push_space {
            if out.push(' ').is_err() {
                flush_sink_block_at_word_boundary(out, role, style, align, sink)?;
                let _ = out.push(' ');
                previous_space = true;
            } else {
                previous_space = true;
            }
        } else if should_push_char {
            if out.push(ch).is_err() {
                flush_sink_block_at_word_boundary(out, role, style, align, sink)?;
                if out.push(ch).is_err() {
                    break;
                }
            }
            previous_space = false;
        }
        cursor += advance;
    }
    Ok(())
}

fn flush_sink_block_at_word_boundary<const N: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
) -> Result<(), XhtmlError> {
    let Some(split) = block.as_str().trim_end().rfind(char::is_whitespace) else {
        return flush_sink_block(block, role, style, align, sink);
    };
    let carry_start = block.as_str()[split..]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| split + index)
        .unwrap_or(block.len());
    let mut carry = heapless::String::<N>::new();
    let _ = carry.push_str(&block.as_str()[carry_start..]);
    let mut emit = heapless::String::<N>::new();
    let _ = emit.push_str(block.as_str()[..split].trim_end());
    let _ = emit.push(' ');
    if !emit.is_empty() {
        sink.push_block(emit.as_str(), role, style, align, false)?;
    }
    *block = carry;
    Ok(())
}

fn flush_sink_block_continue<const N: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
) -> Result<(), XhtmlError> {
    if block.as_str().trim().is_empty() {
        block.clear();
        return Ok(());
    }
    let result = sink.push_block(block.as_str(), role, style, align, false);
    block.clear();
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Ordered,
    Unordered,
}

fn append_list_marker<const N: usize>(
    out: &mut heapless::String<N>,
    counts: &mut [u16; 8],
    kinds: &[ListKind; 8],
    depth: usize,
) {
    if depth == 0 {
        append_owned_text(out, "- ");
        return;
    }
    let index = depth - 1;
    counts[index] = counts[index].saturating_add(1);
    match kinds[index] {
        ListKind::Unordered => append_owned_text(out, "- "),
        ListKind::Ordered => {
            append_u16(out, counts[index]);
            let _ = out.push('.');
            let _ = out.push(' ');
        }
    }
}

fn append_table_cell_separator<const N: usize>(out: &mut heapless::String<N>) {
    if out
        .as_str()
        .chars()
        .last()
        .map(|ch| !ch.is_whitespace())
        .unwrap_or(false)
    {
        let _ = out.push(' ');
    }
}

fn append_u16<const N: usize>(out: &mut heapless::String<N>, value: u16) {
    let mut digits = [0u8; 5];
    let mut len = 0usize;
    let mut remaining = value;
    loop {
        digits[len] = b'0' + (remaining % 10) as u8;
        len += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    while len > 0 {
        len -= 1;
        let _ = out.push(digits[len] as char);
    }
}

fn flush_sink_block<const N: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    sink: &mut impl XhtmlBlockSink,
) -> Result<(), XhtmlError> {
    if block.as_str().trim().is_empty() {
        block.clear();
        return sink.push_block("", role, style, align, true);
    }
    let result = sink.push_block(block.as_str(), role, style, align, true);
    block.clear();
    result
}

/// Decode the HTML entity at the start of `input` (`&amp;`, `&#x2014;`, ...).
/// Shared by firmware cache building and host preview so EPUB text renders
/// identically on both sides.
pub fn decode_html_entity(input: &str) -> Option<char> {
    if let Some(decoded) = decode_numeric_entity(input) {
        Some(decoded)
    } else if input.starts_with("&amp;") {
        Some('&')
    } else if input.starts_with("&lt;") {
        Some('<')
    } else if input.starts_with("&gt;") {
        Some('>')
    } else if input.starts_with("&quot;") {
        Some('"')
    } else if input.starts_with("&apos;") {
        Some('\'')
    } else if input.starts_with("&nbsp;") {
        Some(' ')
    } else if input.starts_with("&mdash;") {
        Some('—')
    } else if input.starts_with("&ndash;") {
        Some('–')
    } else if input.starts_with("&lsquo;") {
        Some('‘')
    } else if input.starts_with("&rsquo;") {
        Some('’')
    } else if input.starts_with("&ldquo;") {
        Some('“')
    } else if input.starts_with("&rdquo;") {
        Some('”')
    } else if input.starts_with("&hellip;") {
        Some('…')
    } else {
        None
    }
}

/// Decode a numeric character reference (`&#8212;` or `&#x2014;`) at the
/// start of `input`.
pub fn decode_numeric_entity(input: &str) -> Option<char> {
    let rest = input.strip_prefix("&#")?;
    let end = rest.find(';')?;
    let entity = &rest[..end];
    let value = if let Some(hex) = entity
        .strip_prefix('x')
        .or_else(|| entity.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        entity.parse::<u32>().ok()?
    };
    char::from_u32(value)
}

fn flush_owned_block<const N: usize, const M: usize>(
    block: &mut heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    output: &mut Vec<TextBlock<N>, M>,
) -> Result<(), XhtmlError> {
    trim_owned(block);
    if block.is_empty() {
        return Ok(());
    }
    let mut text = heapless::String::<N>::new();
    core::mem::swap(block, &mut text);
    push_owned_block(text, role, style, align, output)
}

fn push_owned_block<const N: usize, const M: usize>(
    text: heapless::String<N>,
    role: TextRole,
    style: FontStyle,
    align: TextAlign,
    output: &mut Vec<TextBlock<N>, M>,
) -> Result<(), XhtmlError> {
    output
        .push(TextBlock::new(text, role, style, align))
        .map_err(|_| XhtmlError::TooManyRuns)
}

fn trim_owned<const N: usize>(text: &mut heapless::String<N>) {
    while text.as_str().as_bytes().last().copied() == Some(b' ') {
        text.pop();
    }
    let trim_len = text
        .as_str()
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if trim_len == 0 {
        return;
    }
    let mut trimmed = heapless::String::<N>::new();
    let _ = trimmed.push_str(&text.as_str()[trim_len..]);
    *text = trimmed;
}

fn parse_central_entry(bytes: &[u8], cursor: usize) -> Result<(ZipEntry<'_>, usize), ZipError> {
    if read_u32(bytes, cursor)? != 0x0201_4b50 {
        return Err(ZipError::BadCentralDirectory);
    }
    let compression_method = read_u16(bytes, cursor + 10)?;
    let compressed_size = read_u32(bytes, cursor + 20)?;
    let uncompressed_size = read_u32(bytes, cursor + 24)?;
    let name_len = read_u16(bytes, cursor + 28)? as usize;
    let extra_len = read_u16(bytes, cursor + 30)? as usize;
    let comment_len = read_u16(bytes, cursor + 32)? as usize;
    let local_header_offset = read_u32(bytes, cursor + 42)?;
    let name_start = cursor + 46;
    let name_end = name_start
        .checked_add(name_len)
        .ok_or(ZipError::BadCentralDirectory)?;
    let next = name_end
        .checked_add(extra_len)
        .and_then(|value| value.checked_add(comment_len))
        .ok_or(ZipError::BadCentralDirectory)?;
    let name_bytes = bytes
        .get(name_start..name_end)
        .ok_or(ZipError::BadCentralDirectory)?;
    if name_len > MAX_ENTRY_NAME_BYTES {
        return Err(ZipError::NameTooLong);
    }
    let name = core::str::from_utf8(name_bytes).map_err(|_| ZipError::BadCentralDirectory)?;
    Ok((
        ZipEntry {
            name,
            compression_method,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        },
        next,
    ))
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    let mut cursor = bytes.len() - 22;
    loop {
        if bytes.get(cursor..cursor + 4) == Some(&[0x50, 0x4b, 0x05, 0x06]) {
            return Some(cursor);
        }
        if cursor == 0 {
            return None;
        }
        cursor -= 1;
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ZipError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(ZipError::BadCentralDirectory)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ZipError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(ZipError::BadCentralDirectory)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_exact_at<R>(reader: &mut R, offset: u32, out: &mut [u8]) -> Result<(), ZipError>
where
    R: ReadAt,
{
    let mut filled = 0;
    while filled < out.len() {
        let count = reader
            .read_at(offset + filled as u32, &mut out[filled..])
            .map_err(|_| ZipError::Io)?;
        if count == 0 {
            return Err(ZipError::Io);
        }
        filled += count;
    }
    Ok(())
}

fn read_exact_stream<R>(reader: &mut R, mut out: &mut [u8]) -> Result<(), ZipError>
where
    R: ByteStream,
{
    while !out.is_empty() {
        let count = reader.read(out).map_err(|_| ZipError::Io)?;
        if count == 0 {
            return Err(ZipError::Io);
        }
        let tmp = out;
        out = &mut tmp[count..];
    }
    Ok(())
}

fn skip_stream<R>(reader: &mut R, mut bytes: usize) -> Result<(), ZipError>
where
    R: ByteStream,
{
    let mut scratch = [0u8; 512];
    while bytes > 0 {
        let count = bytes.min(scratch.len());
        read_exact_stream(reader, &mut scratch[..count])?;
        bytes -= count;
    }
    Ok(())
}

fn next_start_tag<'a>(xml: &'a str, name: &str, from: usize) -> Option<(&'a str, usize)> {
    let mut cursor = from;
    while let Some(relative) = xml[cursor..].find('<') {
        let start = cursor + relative + 1;
        let end = start + xml[start..].find('>')?;
        let tag = xml[start..end].trim();
        let tag_name = tag.split_whitespace().next().unwrap_or(tag);
        if tag_name
            .rsplit(':')
            .next()
            .map(|local| local.eq_ignore_ascii_case(name))
            .unwrap_or(false)
        {
            return Some((tag, end + 1));
        }
        cursor = end + 1;
    }
    None
}

/// Strip a `#fragment` suffix from an href so it can be compared against
/// spine and manifest paths.
pub fn strip_fragment(value: &str) -> &str {
    value.split('#').next().unwrap_or(value)
}

fn find_attr_value<'a>(xml: &'a str, tag_name: &str, attr: &str) -> Option<&'a str> {
    let mut cursor = 0;
    while let Some((tag, next)) = next_start_tag(xml, tag_name, cursor) {
        if let Some(value) = attr_value(tag, attr) {
            return Some(value);
        }
        cursor = next;
    }
    None
}

fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut cursor = 0usize;
    while cursor < tag.len() {
        let rest = &tag[cursor..];
        let position = rest.find(name)?;
        let absolute = cursor + position;
        if absolute > 0 {
            let previous = tag.as_bytes()[absolute - 1];
            if is_attr_name_byte(previous) {
                cursor = absolute + name.len();
                continue;
            }
        }
        let after_name = &tag[absolute + name.len()..];
        let trimmed = after_name.trim_start();
        if !trimmed.starts_with('=') {
            cursor = absolute + name.len();
            continue;
        }
        let after_eq = trimmed[1..].trim_start();
        let quote = after_eq.as_bytes().first().copied()?;
        if quote != b'\'' && quote != b'"' {
            cursor = absolute + name.len();
            continue;
        }
        let value = &after_eq[1..];
        let end = value.find(quote as char)?;
        return Some(&value[..end]);
    }
    None
}

fn is_attr_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-')
}

fn attr_word_eq(tag: &str, name: &str, word: &str) -> bool {
    attr_value(tag, name)
        .map(|value| {
            value
                .split_ascii_whitespace()
                .any(|value| value.eq_ignore_ascii_case(word))
        })
        .unwrap_or(false)
}

fn find_guide_reference<'a>(xml: &'a str, reference_type: &str) -> Option<&'a str> {
    let mut in_guide = false;
    let mut cursor = XmlCursor::new(xml);
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "guide") => in_guide = true,
            Token::End(tag) if tag_name_is(tag, "guide") => in_guide = false,
            Token::Start(tag)
                if in_guide
                    && tag_name_is(tag, "reference")
                    && attr_word_eq(tag, "type", reference_type) =>
            {
                return attr_value(tag, "href");
            }
            _ => {}
        }
    }
    None
}

fn element_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut cursor = XmlCursor::new(xml);
    let mut in_target = false;
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(start) if tag_name_is(start, tag) => {
                in_target = true;
            }
            Token::End(end) if tag_name_is(end, tag) => {
                in_target = false;
            }
            Token::Text(text) if in_target => return Some(text),
            _ => {}
        }
    }
    None
}

fn cover_status(manifest: &[ManifestItem], opf: &str) -> CoverStatus {
    if manifest.iter().any(|item| {
        item.id.of(opf) == "cover"
            || item.href.of(opf).contains("cover")
            || item.media_type.of(opf).starts_with("image/")
    }) {
        CoverStatus::Present
    } else {
        CoverStatus::Missing
    }
}

fn style_for(bold_depth: u8, italic_depth: u8) -> FontStyle {
    match (bold_depth > 0, italic_depth > 0) {
        (true, true) => FontStyle::BoldItalic,
        (true, false) => FontStyle::Bold,
        (false, true) => FontStyle::Italic,
        (false, false) => FontStyle::Regular,
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec as StdVec;

    struct SliceReader<'a> {
        bytes: &'a [u8],
    }

    impl ReadAt for SliceReader<'_> {
        type Error = ();

        fn len(&mut self) -> Result<u32, Self::Error> {
            Ok(self.bytes.len() as u32)
        }

        fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
            let offset = offset as usize;
            let Some(rest) = self.bytes.get(offset..) else {
                return Ok(0);
            };
            let count = out.len().min(rest.len());
            out[..count].copy_from_slice(&rest[..count]);
            Ok(count)
        }
    }

    struct SliceStream<'a> {
        bytes: &'a [u8],
        cursor: usize,
    }

    impl ByteStream for SliceStream<'_> {
        type Error = ();

        fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
            let take = out.len().min(self.bytes.len() - self.cursor);
            out[..take].copy_from_slice(&self.bytes[self.cursor..self.cursor + take]);
            self.cursor += take;
            Ok(take)
        }
    }

    #[test]
    fn parses_nested_opf_path_and_spine() {
        let opf = r#"
            <package>
              <metadata>
                <dc:title>Flowers for Algernon</dc:title>
                <dc:creator>Daniel Keyes</dc:creator>
              </metadata>
              <manifest>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
                <item id="cover" href="images/cover.jpg" media-type="image/jpeg"/>
              </manifest>
              <spine><itemref idref="chap1"/></spine>
            </package>
        "#;

        let package = parse_opf(
            opf,
            BookId(7),
            "/books/flowers.epub",
            1234,
            "OPS/package.opf",
        )
        .expect("opf parses");

        assert_eq!(package.meta.title, "Flowers for Algernon");
        assert_eq!(package.meta.author, "Daniel Keyes");
        assert_eq!(package.meta.cover_status, CoverStatus::Present);
        assert_eq!(package.spine.len(), 1);
        assert_eq!(package.spine[0].href.of(package.opf_text), "text/ch1.xhtml");
    }

    struct RecordingSink {
        fragments: StdVec<(std::string::String, TextRole, FontStyle, TextAlign, bool)>,
    }

    impl XhtmlBlockSink for RecordingSink {
        fn push_block(
            &mut self,
            text: &str,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            paragraph_end: bool,
        ) -> Result<(), XhtmlError> {
            if !text.is_empty() {
                self.fragments
                    .push((text.into(), role, style, align, paragraph_end));
            }
            Ok(())
        }
    }

    #[test]
    fn xhtml_sink_emits_styled_fragments() {
        let xhtml = "<body><h1>Chapter</h1><p>Hello <em>soft</em> <strong>bold</strong></p></body>";
        let mut sink = RecordingSink {
            fragments: StdVec::new(),
        };

        xhtml_blocks_to_sink(xhtml, None, &mut sink).expect("xhtml parses");

        assert_eq!(
            sink.fragments[0],
            (
                "Chapter".into(),
                TextRole::Heading1,
                FontStyle::Bold,
                TextAlign::Center,
                true
            )
        );
        assert_eq!(
            sink.fragments[1],
            (
                "Hello ".into(),
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Justify,
                false
            )
        );
        assert_eq!(
            sink.fragments[2],
            (
                "soft".into(),
                TextRole::Body,
                FontStyle::Italic,
                TextAlign::Justify,
                false
            )
        );
        assert_eq!(
            sink.fragments[3],
            (
                "bold".into(),
                TextRole::Body,
                FontStyle::Bold,
                TextAlign::Justify,
                false
            )
        );
        assert_eq!(sink.fragments.len(), 4);
    }

    #[test]
    fn xhtml_blocks_skip_head_style_and_script_text() {
        let xhtml = r#"
            <html>
              <head>
                <title>Not reader text</title>
                <style>body { font-family: serif; } .calibre { margin: 0; }</style>
                <script>console.log("skip");</script>
              </head>
              <body><p>Actual chapter text.</p></body>
            </html>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Actual chapter text.");
        assert_eq!(blocks[0].role, TextRole::Body);
        assert_eq!(blocks[0].align, TextAlign::Justify);
    }

    #[test]
    fn xhtml_blocks_skip_nav_and_hidden_content() {
        let xhtml = r#"
            <html>
              <body>
                <nav>Table of contents</nav>
                <p style="display:none">Hidden paragraph</p>
                <p>Visible text</p>
              </body>
            </html>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Visible text");
    }

    #[test]
    fn xhtml_blocks_number_ordered_list_items() {
        let xhtml = "<body><ol><li>One</li><li>Two</li></ol></body>";
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "1. One");
        assert_eq!(blocks[1].text, "2. Two");
    }

    #[test]
    fn xhtml_blocks_mark_center_aligned_blocks() {
        let xhtml =
            r#"<body><p class="center">Title</p><p style="text-align: center">Author</p></body>"#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[0].align, TextAlign::Center);
        assert_eq!(blocks[1].text, "Author");
        assert_eq!(blocks[1].align, TextAlign::Center);
    }

    #[test]
    fn xhtml_blocks_emit_simple_table_rows() {
        let xhtml = r#"
            <body>
              <h1>CONTENTS</h1>
              <table style="margin-left: auto; margin-right: auto;">
                <tr><td>I</td><td>Introduction</td></tr>
                <tr><td>II</td><td>The Machine</td></tr>
              </table>
            </body>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<64>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks[0].text, "CONTENTS");
        assert_eq!(blocks[0].align, TextAlign::Center);
        assert_eq!(blocks[1].text, "I Introduction");
        assert_eq!(blocks[1].align, TextAlign::Center);
        assert_eq!(blocks[2].text, "II The Machine");
        assert_eq!(blocks[2].align, TextAlign::Center);
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn epub3_nav_emits_flat_toc_records() {
        struct Sink {
            items: Vec<(heapless::String<32>, heapless::String<48>, u8), 8>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut stored_title = heapless::String::<32>::new();
                let mut stored_href = heapless::String::<48>::new();
                stored_title
                    .push_str(title)
                    .map_err(|_| TocError::TooManyItems)?;
                stored_href
                    .push_str(href)
                    .map_err(|_| TocError::TooManyItems)?;
                self.items
                    .push((stored_title, stored_href, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="chapter1.xhtml#start">Introduction</a></li>
                <li><a href="chapter2.xhtml">The Machine</a>
                  <ol><li><a href="chapter2.xhtml#part">A room</a></li></ol>
                </li>
              </ol></nav>
            </body></html>
        "#;
        let mut sink = Sink { items: Vec::new() };
        parse_epub3_nav_to_sink(nav, &mut sink).expect("nav parses");

        assert_eq!(sink.items.len(), 3);
        assert_eq!(sink.items[0].0.as_str(), "Introduction");
        assert_eq!(sink.items[0].1.as_str(), "chapter1.xhtml#start");
        assert_eq!(sink.items[0].2, 1);
        assert_eq!(sink.items[2].0.as_str(), "A room");
        assert_eq!(sink.items[2].2, 2);
    }

    #[test]
    fn epub2_ncx_emits_flat_toc_records() {
        struct Sink {
            items: Vec<(heapless::String<32>, heapless::String<48>, u8), 8>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut stored_title = heapless::String::<32>::new();
                let mut stored_href = heapless::String::<48>::new();
                stored_title
                    .push_str(title)
                    .map_err(|_| TocError::TooManyItems)?;
                stored_href
                    .push_str(href)
                    .map_err(|_| TocError::TooManyItems)?;
                self.items
                    .push((stored_title, stored_href, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = r#"
            <ncx><navMap>
              <navPoint><navLabel><text>Introduction</text></navLabel><content src="chapter1.xhtml"/>
                <navPoint><navLabel><text>Part A</text></navLabel><content src="chapter1.xhtml#a"/></navPoint>
              </navPoint>
              <navPoint><navLabel><text>The Machine</text></navLabel><content src="chapter2.xhtml"/></navPoint>
            </navMap></ncx>
        "#;
        let mut sink = Sink { items: Vec::new() };
        parse_epub2_ncx_to_sink(ncx, &mut sink).expect("ncx parses");

        assert_eq!(sink.items.len(), 3);
        assert_eq!(sink.items[0].0.as_str(), "Introduction");
        assert_eq!(sink.items[0].1.as_str(), "chapter1.xhtml");
        assert_eq!(sink.items[0].2, 1);
        assert_eq!(sink.items[1].0.as_str(), "Part A");
        assert_eq!(sink.items[1].2, 2);
        assert_eq!(sink.items[2].0.as_str(), "The Machine");
    }

    #[test]
    fn ncx_stream_keeps_multibyte_titles_across_any_chunking() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = "<ncx><navMap>\
            <navPoint><navLabel><text>Pr\u{e9}face \u{2014} \u{4e16}\u{754c}</text></navLabel>\
            <content src=\"ch1.xhtml\"/></navPoint>\
            <navPoint><navLabel><text>Caf\u{e9} &amp; r\u{ea}ve</text></navLabel>\
            <content src=\"ch2.xhtml\"/></navPoint>\
            </navMap></ncx>";

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub2_ncx_to_sink(ncx, &mut slice_sink).expect("slice parses");
        assert_eq!(slice_sink.0.len(), 2);

        // One byte at a time: every UTF-8 sequence and the entity get
        // split across feed calls, which the old char-per-byte text
        // accumulation turned into mojibake.
        let mut tokenizer = StreamingXmlTokenizer::new();
        let mut parser = NcxStreamParser::new();
        let mut stream_sink = CountingSink(Vec::new());
        for byte in ncx.as_bytes() {
            tokenizer
                .feed_ncx(core::slice::from_ref(byte), &mut parser, &mut stream_sink)
                .expect("byte feeds");
        }
        tokenizer
            .finish_ncx(&mut parser, &mut stream_sink)
            .expect("finish");

        assert_eq!(slice_sink.0.len(), stream_sink.0.len());
        for (a, b) in slice_sink.0.iter().zip(stream_sink.0.iter()) {
            assert_eq!(a.0.as_str(), b.0.as_str());
            assert_eq!(a.1.as_str(), b.1.as_str());
        }
    }

    #[test]
    fn epub2_ncx_stream_matches_slice_parser() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 256>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let ncx = r#"<?xml version='1.0' encoding='utf-8'?>
            <ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
              <head><meta name="dtb:uid" content="x"/></head>
              <navMap>
                <navPoint><navLabel><text>Introduction</text></navLabel><content src="chapter1.xhtml"/>
                  <navPoint><navLabel><text>Part A</text></navLabel><content src="chapter1.xhtml#a"/></navPoint>
                </navPoint>
                <navPoint><navLabel><text>The Machine</text></navLabel><content src="chapter2.xhtml"/></navPoint>
              </navMap>
            </ncx>"#;

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub2_ncx_to_sink(ncx, &mut slice_sink).expect("slice parses");

        let mut stream = SliceByteStream::new(ncx.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub2_ncx_stream(&mut stream, &mut stream_sink).expect("stream parses");

        assert_eq!(slice_sink.0.len(), stream_sink.0.len());
        for (a, b) in slice_sink.0.iter().zip(stream_sink.0.iter()) {
            assert_eq!(a.0.as_str(), b.0.as_str());
            assert_eq!(a.1.as_str(), b.1.as_str());
            assert_eq!(a.2, b.2);
        }
    }

    #[test]
    fn epub3_nav_stream_matches_slice_parser() {
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="chapter1.xhtml#start">Introduction</a></li>
                <li><a href="chapter2.xhtml">The Machine</a>
                  <ol><li><a href="chapter2.xhtml#part">A room</a></li></ol>
                </li>
              </ol></nav>
            </body></html>
        "#;

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut slice_sink).expect("slice parses");

        let mut stream = SliceByteStream::new(nav.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub3_nav_stream(&mut stream, &mut stream_sink).expect("stream parses");

        assert_eq!(slice_sink.0.len(), stream_sink.0.len());
        for (a, b) in slice_sink.0.iter().zip(stream_sink.0.iter()) {
            assert_eq!(a.0.as_str(), b.0.as_str());
            assert_eq!(a.1.as_str(), b.1.as_str());
            assert_eq!(a.2, b.2);
        }
    }

    #[test]
    fn epub3_nav_skips_page_list_and_landmarks() {
        // A real EPUB 3 nav document carries the `toc` alongside a `page-list`
        // (roman-numeral / numeric page markers) and `landmarks` navs. Only the
        // toc lists chapters; the others must not leak in as phantom chapters.
        struct CountingSink(Vec<(heapless::String<64>, heapless::String<64>, u8), 16>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let mut h = heapless::String::<64>::new();
                let _ = t.push_str(title);
                let _ = h.push_str(href);
                self.0
                    .push((t, h, level))
                    .map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a href="chapter1.xhtml">Introduction</a></li>
                <li><a href="chapter2.xhtml">The Machine</a></li>
              </ol></nav>
              <nav epub:type="page-list" role="doc-pagelist"><ol>
                <li><a href="front.xhtml#pi">i</a></li>
                <li><a href="front.xhtml#pii">ii</a></li>
                <li><a href="front.xhtml#piii">iii</a></li>
                <li><a href="chapter1.xhtml#p1">1</a></li>
                <li><a href="chapter1.xhtml#p2">2</a></li>
              </ol></nav>
              <nav epub:type="landmarks"><ol>
                <li><a epub:type="cover" href="cover.xhtml">Cover</a></li>
                <li><a epub:type="bodymatter" href="chapter1.xhtml">Start Reading</a></li>
              </ol></nav>
              <nav role="doc-pagelist"><ol>
                <li><a href="back.xhtml#pix">ix</a></li>
                <li><a href="back.xhtml#px">x</a></li>
              </ol></nav>
              <nav aria-label="Index"><ol>
                <li><a href="back.xhtml#index-a">A</a></li>
                <li><a href="back.xhtml#index-b">B</a></li>
              </ol></nav>
              <nav id="loi"><ol>
                <li><a href="figures.xhtml#f1">Figure 1</a></li>
              </ol></nav>
              <nav class="page_list"><ol>
                <li><a href="back.xhtml#pxi">xi</a></li>
              </ol></nav>
              <nav epub:type="lot"><ol>
                <li><a href="tables.xhtml#t1">Table 1</a></li>
              </ol></nav>
            </body></html>
        "#;

        let expected = [
            ("Introduction", "chapter1.xhtml"),
            ("The Machine", "chapter2.xhtml"),
        ];

        let mut slice_sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut slice_sink).expect("slice parses");
        assert_eq!(slice_sink.0.len(), expected.len());
        for (item, (title, href)) in slice_sink.0.iter().zip(expected.iter()) {
            assert_eq!(item.0.as_str(), *title);
            assert_eq!(item.1.as_str(), *href);
        }

        // The streaming parser (the one that runs on-device) must agree.
        let mut stream = SliceByteStream::new(nav.as_bytes());
        let mut stream_sink = CountingSink(Vec::new());
        parse_epub3_nav_stream(&mut stream, &mut stream_sink).expect("stream parses");
        assert_eq!(stream_sink.0.len(), expected.len());
        for (item, (title, href)) in stream_sink.0.iter().zip(expected.iter()) {
            assert_eq!(item.0.as_str(), *title);
            assert_eq!(item.1.as_str(), *href);
        }
    }

    #[test]
    fn epub3_nav_without_epub_type_is_still_read() {
        // Older/minimal single-nav documents omit `epub:type`; those must still
        // parse as the table of contents.
        struct CountingSink(Vec<heapless::String<64>, 8>);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                let mut t = heapless::String::<64>::new();
                let _ = t.push_str(title);
                self.0.push(t).map_err(|_| TocError::TooManyItems)
            }
        }

        let nav = r#"
            <html><body>
              <nav><ol>
                <li><a href="chapter1.xhtml">Introduction</a></li>
              </ol></nav>
            </body></html>
        "#;

        let mut sink = CountingSink(Vec::new());
        parse_epub3_nav_to_sink(nav, &mut sink).expect("nav parses");
        assert_eq!(sink.0.len(), 1);
        assert_eq!(sink.0[0].as_str(), "Introduction");
    }

    #[test]
    fn epub2_ncx_stream_handles_ncx_larger_than_any_scratch_buffer() {
        // Build a synthetic NCX with 200 chapters of descriptive titles —
        // roughly 60 KB of XML, comfortably larger than any realistic in-RAM
        // scratch on the device. The streaming parser should still surface
        // every entry because peak memory is bounded by a single tag/title,
        // not the file size.
        use core::fmt::Write as _;
        let mut xml =
            std::string::String::from("<?xml version='1.0' encoding='utf-8'?><ncx><navMap>");
        for index in 0..200 {
            write!(
                xml,
                "<navPoint><navLabel><text>Chapter {:03}: The Long Title That Inflates File Size</text></navLabel><content src=\"chapter_{:03}.xhtml\"/></navPoint>",
                index, index
            )
            .unwrap();
        }
        xml.push_str("</navMap></ncx>");
        assert!(xml.len() > 24 * 1024, "fixture must exceed XHTML scratch");

        struct Sink {
            count: usize,
            last_title: heapless::String<128>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, title: &str, _href: &str, _level: u8) -> Result<(), TocError> {
                self.last_title.clear();
                let _ = self.last_title.push_str(title);
                self.count += 1;
                Ok(())
            }
        }

        let mut stream = SliceByteStream::new(xml.as_bytes());
        let mut sink = Sink {
            count: 0,
            last_title: heapless::String::new(),
        };
        parse_epub2_ncx_stream(&mut stream, &mut sink).expect("large ncx parses");

        assert_eq!(sink.count, 200);
        assert!(sink.last_title.as_str().starts_with("Chapter 199"));
    }

    #[test]
    fn epub2_ncx_stream_survives_tiny_read_chunks() {
        struct CountingSink(usize);
        impl EpubTocSink for CountingSink {
            fn push_toc(&mut self, _t: &str, _h: &str, _l: u8) -> Result<(), TocError> {
                self.0 += 1;
                Ok(())
            }
        }

        // Read one byte at a time to exercise tag-spanning-chunk-boundary.
        struct OneByteStream<'a> {
            bytes: &'a [u8],
            cursor: usize,
        }
        impl ByteStream for OneByteStream<'_> {
            type Error = ();
            fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
                if self.cursor >= self.bytes.len() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.bytes[self.cursor];
                self.cursor += 1;
                Ok(1)
            }
        }

        let ncx = r#"<ncx><navMap>
            <navPoint><navLabel><text>One</text></navLabel><content src="a.xhtml"/></navPoint>
            <navPoint><navLabel><text>Two</text></navLabel><content src="b.xhtml"/></navPoint>
            <navPoint><navLabel><text>Three</text></navLabel><content src="c.xhtml"/></navPoint>
        </navMap></ncx>"#;

        let mut stream = OneByteStream {
            bytes: ncx.as_bytes(),
            cursor: 0,
        };
        let mut sink = CountingSink(0);
        parse_epub2_ncx_stream(&mut stream, &mut sink).expect("byte-by-byte parses");
        assert_eq!(sink.0, 3);
    }

    #[test]
    fn opf_parses_only_manifest_and_spine_sections() {
        let opf = r#"
            <package>
              <metadata><dc:title>The Time Machine</dc:title><dc:creator>H. G. Wells</dc:creator></metadata>
              <guide><reference type="text" title="Start" href="text/start.xhtml#p1"/></guide>
              <manifest>
                <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
                <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
              </manifest>
              <spine toc="ncx"><itemref idref="chap1"/></spine>
              <item id="outside" href="wrong.xhtml" media-type="application/xhtml+xml"/>
            </package>
        "#;

        let package = parse_opf(opf, BookId(9), "/books/time.epub", 42, "OPS/content.opf")
            .expect("opf parses");

        assert_eq!(package.manifest.len(), 3);
        assert_eq!(package.spine.len(), 1);
        assert_eq!(package.spine[0].href.of(package.opf_text), "text/ch1.xhtml");
        assert_eq!(package.text_reference_href, Some("text/start.xhtml"));
        assert_eq!(package.nav_href, Some("nav.xhtml"));
        assert_eq!(package.ncx_href, Some("toc.ncx"));
    }

    #[test]
    fn opf_falls_back_to_xhtml_manifest_when_spine_refs_do_not_resolve() {
        let opf = r#"
            <package>
              <metadata><dc:title>Loose Book</dc:title></metadata>
              <manifest>
                <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
                <item id="style" href="book.css" media-type="text/css"/>
              </manifest>
              <spine><itemref idref="missing"/></spine>
            </package>
        "#;

        let package = parse_opf(opf, BookId(11), "/books/loose.epub", 42, "OPS/content.opf")
            .expect("opf parses");

        assert_eq!(package.spine.len(), 1);
        assert_eq!(package.spine[0].href.of(package.opf_text), "text/ch1.xhtml");
    }

    #[test]
    fn xhtml_skips_pagebreaks_comments_and_aria_hidden() {
        let xhtml = r#"
            <body>
              <!-- comment should not be text -->
              <?xml-stylesheet href="x.css"?>
              <span role="doc-pagebreak" aria-label="12">12</span>
              <span aria-hidden="true">hidden</span>
              <p>Visible&nbsp;text &amp; symbols</p>
            </body>
        "#;
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Visible text & symbols");
    }

    #[test]
    fn xhtml_emits_ordered_list_markers() {
        let xhtml = "<body><ol><li>One</li><li>Two</li></ol></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks[0].text, "1. One");
        assert_eq!(blocks[1].text, "2. Two");
    }

    #[test]
    fn xhtml_sink_splits_long_paragraph_without_dropping_words() {
        struct CountingSink {
            blocks: usize,
            words: usize,
        }
        impl XhtmlBlockSink for CountingSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                paragraph_end: bool,
            ) -> Result<(), XhtmlError> {
                if text.is_empty() {
                    return Ok(());
                }
                self.blocks += 1;
                self.words += text.split_ascii_whitespace().count();
                assert!(self.blocks > 1 || !paragraph_end);
                Ok(())
            }
        }

        let mut xhtml = std::string::String::from("<body><p>");
        for _ in 0..180 {
            xhtml.push_str("word ");
        }
        xhtml.push_str("</p></body>");
        let mut sink = CountingSink {
            blocks: 0,
            words: 0,
        };

        xhtml_blocks_to_sink(&xhtml, None, &mut sink).expect("sink accepts chunks");

        assert!(sink.blocks > 1);
        assert_eq!(sink.words, 180);
    }

    #[test]
    fn xhtml_sink_does_not_split_word_when_buffer_flushes() {
        struct CollectingSink {
            text: std::string::String,
        }
        impl XhtmlBlockSink for CollectingSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                _paragraph_end: bool,
            ) -> Result<(), XhtmlError> {
                self.text.push_str(text);
                Ok(())
            }
        }

        let mut xhtml = std::string::String::from("<body><p>");
        for _ in 0..80 {
            xhtml.push_str("filler ");
        }
        xhtml.push_str("Space, and a fourth, Time.</p></body>");
        let mut sink = CollectingSink {
            text: std::string::String::new(),
        };

        xhtml_blocks_to_sink(&xhtml, None, &mut sink).expect("sink accepts chunks");

        assert!(sink.text.contains("Space, and a fourth, Time."));
        assert!(!sink.text.contains(" a nd "));
    }

    #[test]
    fn xhtml_sink_does_not_split_inline_word_when_buffer_flushes() {
        struct CollectingSink {
            text: std::string::String,
        }
        impl XhtmlBlockSink for CollectingSink {
            fn push_block(
                &mut self,
                text: &str,
                _role: TextRole,
                _style: FontStyle,
                _align: TextAlign,
                _paragraph_end: bool,
            ) -> Result<(), XhtmlError> {
                self.text.push_str(text);
                Ok(())
            }
        }

        let mut xhtml = std::string::String::from("<body><p>");
        for _ in 0..80 {
            xhtml.push_str("filler ");
        }
        xhtml.push_str("Space, <em>a</em>nd a fourth, Time.</p></body>");
        let mut sink = CollectingSink {
            text: std::string::String::new(),
        };

        xhtml_blocks_to_sink(&xhtml, None, &mut sink).expect("sink accepts chunks");

        assert!(sink.text.contains("Space, and a fourth, Time."));
        assert!(!sink.text.contains(" a nd "));
    }

    #[test]
    fn xhtml_keeps_inline_italic_inside_paragraph_block() {
        let xhtml = "<body><p>You must <em>follow</em> me carefully.</p></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "You must follow me carefully.");
        assert_eq!(blocks[0].role, TextRole::Body);
    }

    #[test]
    fn xhtml_attaches_punctuation_after_inline_tag() {
        let xhtml = "<body><p>chairs, being his <em>patents</em>, embraced</p></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "chairs, being his patents, embraced");
    }

    #[test]
    fn xhtml_does_not_split_word_across_inline_tag() {
        let xhtml = "<body><p>Space, <em>a</em>nd a fourth, Time.</p></body>";
        let mut blocks = heapless::Vec::<TextBlock<96>, 8>::new();

        xhtml_text_blocks_with_css(xhtml, None, &mut blocks).expect("blocks fit");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Space, and a fourth, Time.");
    }

    #[test]
    fn zip_rejects_missing_entry() {
        let zip_bytes = stored_zip(&[("hello.txt", b"hi".as_slice())]);
        let archive = ZipArchive::new(&zip_bytes).expect("zip parses");

        assert_eq!(archive.find("missing.txt"), Err(ZipError::EntryNotFound));
    }

    #[test]
    fn zip_rejects_malformed_central_directory() {
        assert_eq!(
            ZipArchive::new(b"not a zip file").err(),
            Some(ZipError::MissingEndOfCentralDirectory)
        );
    }

    #[test]
    fn zip_reads_stored_entry() {
        let zip_bytes = stored_zip(&[("META-INF/container.xml", b"<container/>".as_slice())]);
        let archive = ZipArchive::new(&zip_bytes).expect("zip parses");
        let entry = archive
            .find("META-INF/container.xml")
            .expect("entry exists");
        let mut output = [0u8; 32];

        let len = archive.read_entry(entry, &mut output).expect("stored read");

        assert_eq!(&output[..len], b"<container/>");
    }

    #[test]
    fn zip_stream_reads_stored_entry_by_offset() {
        let zip_bytes = stored_zip(&[("OPS/package.opf", b"<package/>".as_slice())]);
        let mut tail = [0u8; 512];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/package.opf", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 64];
        let mut output = [0u8; 64];

        let len = stream
            .read_entry(entry, &mut compressed, &mut output)
            .expect("entry read");

        assert_eq!(&output[..len], b"<package/>");
    }

    #[test]
    fn zip_stream_read_entry_to_sink_streams_full_inflate_output() {
        // 1.8 KB of plain text compressed to 54 bytes — small input, large
        // output. The window holds only 32 bytes, so the inflate has to keep
        // flushing buffered output many times after the compressed input is
        // fully consumed. Catches the "stop after first chunk" bug where the
        // inner loop exits on consumed==input_len even when flush=Finish has
        // more buffered output to drain.
        let plain: std::vec::Vec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .copied()
            .cycle()
            .take(1800)
            .collect();
        let deflated: &[u8] = &[
            75, 76, 74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46,
            41, 45, 43, 175, 168, 172, 50, 48, 52, 50, 54, 49, 53, 51, 183, 176, 76, 28, 85, 51,
            170, 102, 84, 205, 168, 154, 81, 53, 163, 106, 134, 169, 26, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/long.xhtml", 8, deflated, &plain)]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/long.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 512];
        let mut window = [0u8; 32];
        let mut inflate = ZipInflateScratch::new();
        let mut collected: std::vec::Vec<u8> = std::vec::Vec::new();

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected, plain);
    }

    #[test]
    fn zip_local_stream_read_entry_to_sink_streams_full_inflate_output() {
        // Forward-only twin of the ZipStream regression test above, with a
        // 16-byte input scratch on top so both dimensions chunk: compressed
        // bytes arrive across several fetches and the 32-byte window drains
        // many times per fetch.
        let plain: StdVec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .copied()
            .cycle()
            .take(1800)
            .collect();
        let deflated: &[u8] = &[
            75, 76, 74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46,
            41, 45, 43, 175, 168, 172, 50, 48, 52, 50, 54, 49, 53, 51, 183, 176, 76, 28, 85, 51,
            170, 102, 84, 205, 168, 154, 81, 53, 163, 106, 134, 169, 26, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/long.xhtml", 8, deflated, &plain)]);
        let mut stream = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let entry = stream
            .find_entry("OPS/long.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 16];
        let mut window = [0u8; 32];
        let mut inflate = ZipInflateScratch::new();
        let mut collected: StdVec<u8> = StdVec::new();

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected, plain);
    }

    #[test]
    fn zip_stream_read_entry_to_sink_drains_entries_larger_than_inflate_dictionary() {
        // 100 KB decompressed forces miniz to cycle its 32 KB internal
        // dictionary several times, and the 33-byte window guarantees the
        // dictionary drains in partial pushes that do not line up with input
        // chunk boundaries. Catches the engine treating "no input consumed
        // but output written" or BufError-at-chunk-edges as failures.
        let plain: StdVec<u8> = (0u32..100_000)
            .map(|value| (value.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let deflated = miniz_oxide::deflate::compress_to_vec(&plain, 6);
        let zip_bytes = zip_with_methods(&[("OPS/huge.xhtml", 8, &deflated, &plain)]);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/huge.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 1000];
        let mut window = [0u8; 33];
        let mut inflate = ZipInflateScratch::new();
        let mut collected: StdVec<u8> = StdVec::new();

        stream
            .read_entry_to_sink(entry, &mut compressed, &mut window, &mut inflate, |chunk| {
                collected.extend_from_slice(chunk);
                Ok(())
            })
            .expect("entry streams");

        assert_eq!(collected, plain);
    }

    #[test]
    fn zip_local_stream_prefix_stop_leaves_cursor_on_next_entry() {
        // A bounded prefix read stops mid-entry; the unread payload must be
        // skipped so the forward-only cursor lands on the next local header.
        let plain: StdVec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .copied()
            .cycle()
            .take(1800)
            .collect();
        let deflated: &[u8] = &[
            75, 76, 74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46,
            41, 45, 43, 175, 168, 172, 50, 48, 52, 50, 54, 49, 53, 51, 183, 176, 76, 28, 85, 51,
            170, 102, 84, 205, 168, 154, 81, 53, 163, 106, 134, 169, 26, 0,
        ];
        let zip_bytes = zip_with_methods(&[
            ("OPS/big.xhtml", 8, deflated, &plain),
            ("OPS/next.xhtml", 0, b"tail entry", b"tail entry"),
        ]);
        let mut stream = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let entry = stream
            .find_entry("OPS/big.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("big entry exists");
        let mut compressed = [0u8; 16];
        let mut prefix = [0u8; 64];
        let mut inflate = ZipInflateScratch::new();

        let (len, complete) = stream
            .read_entry_prefix_streamed(entry, &mut compressed, &mut prefix, &mut inflate)
            .expect("prefix reads");
        assert_eq!(len, prefix.len());
        assert!(!complete);
        assert_eq!(&prefix[..len], &plain[..len]);

        let next = stream
            .find_entry("OPS/next.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("next entry found after prefix stop");
        let mut out = [0u8; 32];
        let read = stream
            .read_entry_streamed(next, &mut compressed, &mut out, &mut inflate)
            .expect("stored entry reads");
        assert_eq!(&out[..read], b"tail entry");
    }

    #[test]
    fn zip_stream_inflates_deflated_entry_in_small_chunks() {
        let plain = b"Large compressed member ".repeat(20);
        let deflated: &[u8] = &[
            243, 73, 44, 74, 79, 85, 72, 206, 207, 45, 40, 74, 45, 46, 78, 77, 81, 200, 77, 205,
            77, 74, 45, 82, 240, 25, 21, 31, 22, 226, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/chapter.xhtml", 8, deflated, plain.as_slice())]);
        let mut tail = [0u8; 512];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/chapter.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 8];
        let mut output = [0u8; 512];
        let mut inflate = ZipInflateScratch::new();

        let len = stream
            .read_entry_streamed(entry, &mut compressed, &mut output, &mut inflate)
            .expect("entry read");

        assert_eq!(&output[..len], plain.as_slice());
    }

    #[test]
    fn zip_stream_returns_partial_prefix_when_output_fills() {
        let zip_bytes = stored_zip(&[("OPS/chapter.xhtml", b"0123456789abcdef".as_slice())]);
        let mut tail = [0u8; 512];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");
        let entry = stream
            .find_entry("OPS/chapter.xhtml", &mut [0u8; 46], &mut [0u8; 64])
            .expect("entry exists");
        let mut compressed = [0u8; 4];
        let mut output = [0u8; 8];
        let mut inflate = ZipInflateScratch::new();

        let (len, complete) = stream
            .read_entry_prefix_streamed(entry, &mut compressed, &mut output, &mut inflate)
            .expect("entry read");

        assert_eq!(len, 8);
        assert!(!complete);
        assert_eq!(&output, b"01234567");
    }

    #[test]
    fn streamed_xhtml_blocks_match_whole_document_parse_for_any_chunking() {
        // Rich enough to exercise every state-machine path: headings,
        // nested styles, lists, a table, br, img alt, skipped head/style/
        // script/nav/hidden content, entities, multi-byte UTF-8, and a
        // paragraph long enough to force a mid-block word-boundary flush.
        let mut long_para = std::string::String::new();
        for index in 0..80 {
            long_para.push_str(&std::format!("w{index:03}rd léng—thy "));
        }
        let xhtml = std::format!(
            r#"<?xml version="1.0"?><!DOCTYPE html><!-- comment -->
            <html><head><title>skip me</title><style>p {{ color: red; }}</style>
            <script>var x = "<p>not text</p>";</script></head>
            <body>
              <nav><ol><li>toc entry</li></ol></nav>
              <h1>Chapter &amp; Verse</h1>
              <p>Hello <em>soft &#233;</em> and <strong>bold <i>both</i></strong> end.</p>
              <p style="display:none">invisible</p>
              <blockquote>Quoted &hellip; text</blockquote>
              <ul><li>alpha</li><li>beta<ol><li>nested</li></ol></li></ul>
              <table style="margin-left: auto; margin-right: auto;">
                <tr><td>I</td><td>Intro 世界</td></tr>
              </table>
              <p>{long_para}</p>
              <p class="center">Centered<br/>after break</p>
              <img src="x.png" alt="A picture"/>
              <span epub:type="pagebreak" title="12"></span>
            </body></html>"#
        );

        struct Recorder {
            fragments: StdVec<(std::string::String, TextRole, FontStyle, TextAlign, bool)>,
        }
        impl XhtmlBlockSink for Recorder {
            fn push_block(
                &mut self,
                text: &str,
                role: TextRole,
                style: FontStyle,
                align: TextAlign,
                paragraph_end: bool,
            ) -> Result<(), XhtmlError> {
                self.fragments
                    .push((text.into(), role, style, align, paragraph_end));
                Ok(())
            }
        }

        let mut whole = Recorder {
            fragments: StdVec::new(),
        };
        xhtml_blocks_to_sink(&xhtml, None, &mut whole).expect("whole parse");
        assert!(whole.fragments.len() > 10, "doc should produce many blocks");

        for chunk_len in [1usize, 2, 3, 5, 7, 64, 4096] {
            let mut streamed = Recorder {
                fragments: StdVec::new(),
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = XhtmlBlockStreamParser::new(false);
            for chunk in xhtml.as_bytes().chunks(chunk_len) {
                tokenizer
                    .feed_xhtml_blocks(chunk, &mut parser, None, &mut streamed)
                    .expect("chunk feeds");
            }
            tokenizer
                .finish_xhtml_blocks(&mut parser, &mut streamed)
                .expect("finish");

            assert_eq!(
                whole.fragments, streamed.fragments,
                "chunk size {chunk_len} must not change emitted blocks"
            );
        }
    }

    #[test]
    fn zip_stream_indexes_oversized_central_directory() {
        // 16 entries x (46-byte header + 20-byte name) > 1 KB of central
        // directory against a 1 KB tail scratch: the whole directory no
        // longer fits, but all 16 index records (20 bytes each) do.
        let names: StdVec<std::string::String> = (0..16)
            .map(|index| std::format!("OPS/chapter-{index:02}.xhtml"))
            .collect();
        let bodies: StdVec<std::string::String> = (0..16)
            .map(|index| std::format!("body {index:02}"))
            .collect();
        let files: StdVec<(&str, &[u8])> = names
            .iter()
            .zip(bodies.iter())
            .map(|(name, body)| (name.as_str(), body.as_bytes()))
            .collect();
        let zip_bytes = stored_zip(&files);
        let mut tail = [0u8; 1024];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");

        for (name, body) in files.iter() {
            let entry = stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .expect("indexed entry found");
            let mut compressed = [0u8; 64];
            let mut output = [0u8; 64];
            let len = stream
                .read_entry(entry, &mut compressed, &mut output)
                .expect("entry read");
            assert_eq!(&output[..len], *body);
        }
        assert_eq!(
            stream
                .find_entry("OPS/missing.xhtml", &mut [0u8; 46], &mut [0u8; 64])
                .err(),
            Some(ZipError::EntryNotFound)
        );
    }

    #[test]
    fn zip_stream_partial_index_falls_back_to_the_walk() {
        // 128-byte scratch indexes only 6 of 10 entries; lookups past the
        // indexed prefix must still resolve through the directory walk.
        let names: StdVec<std::string::String> = (0..10)
            .map(|index| std::format!("OPS/chapter-{index:02}.xhtml"))
            .collect();
        let bodies: StdVec<std::string::String> = (0..10)
            .map(|index| std::format!("body {index:02}"))
            .collect();
        let files: StdVec<(&str, &[u8])> = names
            .iter()
            .zip(bodies.iter())
            .map(|(name, body)| (name.as_str(), body.as_bytes()))
            .collect();
        let zip_bytes = stored_zip(&files);
        let mut tail = [0u8; 128];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut tail)
            .expect("stream zip parses");

        for (name, body) in files.iter() {
            let entry = stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .expect("entry found through index or walk");
            let mut compressed = [0u8; 64];
            let mut output = [0u8; 64];
            let len = stream
                .read_entry(entry, &mut compressed, &mut output)
                .expect("entry read");
            assert_eq!(&output[..len], *body);
        }
        assert_eq!(
            stream
                .find_entry("OPS/missing.xhtml", &mut [0u8; 46], &mut [0u8; 64])
                .err(),
            Some(ZipError::EntryNotFound)
        );
    }

    fn stored_zip(files: &[(&str, &[u8])]) -> StdVec<u8> {
        let entries: StdVec<_> = files
            .iter()
            .map(|(name, data)| (*name, 0, *data, *data))
            .collect();
        zip_with_methods(&entries)
    }

    fn zip_with_methods(files: &[(&str, u16, &[u8], &[u8])]) -> StdVec<u8> {
        let mut bytes = StdVec::new();
        let mut central = StdVec::new();
        let mut offsets = StdVec::new();

        for (name, method, compressed, plain) in files {
            offsets.push(bytes.len() as u32);
            push_u32(&mut bytes, 0x0403_4b50);
            push_u16(&mut bytes, 20);
            push_u16(&mut bytes, 0);
            push_u16(&mut bytes, *method);
            push_u16(&mut bytes, 0);
            push_u16(&mut bytes, 0);
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, compressed.len() as u32);
            push_u32(&mut bytes, plain.len() as u32);
            push_u16(&mut bytes, name.len() as u16);
            push_u16(&mut bytes, 0);
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(compressed);
        }

        for ((name, method, compressed, plain), offset) in files.iter().zip(offsets.iter()) {
            push_u32(&mut central, 0x0201_4b50);
            push_u16(&mut central, 20);
            push_u16(&mut central, 20);
            push_u16(&mut central, 0);
            push_u16(&mut central, *method);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, compressed.len() as u32);
            push_u32(&mut central, plain.len() as u32);
            push_u16(&mut central, name.len() as u16);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, *offset);
            central.extend_from_slice(name.as_bytes());
        }

        let central_offset = bytes.len() as u32;
        let central_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        push_u32(&mut bytes, 0x0605_4b50);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, files.len() as u16);
        push_u16(&mut bytes, files.len() as u16);
        push_u32(&mut bytes, central_size);
        push_u32(&mut bytes, central_offset);
        push_u16(&mut bytes, 0);
        bytes
    }

    fn push_u16(bytes: &mut StdVec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut StdVec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn test_load_container_xml_and_find_opf_path_requires_4096_capacity() {
        let mut container_content = StdVec::new();
        container_content.extend_from_slice(b"<?xml version=\"1.0\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n  <rootfiles>\n    <rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/>\n  </rootfiles>\n  <!-- ");
        while container_content.len() < 4092 {
            container_content.push(b'a');
        }
        container_content.extend_from_slice(b" -->");
        assert_eq!(container_content.len(), 4096);

        let zip_bytes = stored_zip(&[("META-INF/container.xml", container_content.as_slice())]);

        let mut header_scratch = [0u8; 46];
        let mut name_scratch = [0u8; 256];
        let mut compressed_scratch = [0u8; 1024];
        let mut zip_inflate = ZipInflateScratch::new();
        let mut opf_path_buf = heapless::String::<256>::new();

        // 1. With 3840 bytes (the regression limit), it must fail with OutputTooSmall
        let mut zip_small = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let mut small_container_scratch = [0u8; 3840];
        let res_small = load_container_xml_and_find_opf_path(
            &mut zip_small,
            &mut header_scratch,
            &mut name_scratch,
            &mut compressed_scratch,
            &mut small_container_scratch,
            &mut zip_inflate,
            &mut opf_path_buf,
        );
        assert_eq!(res_small, Err(EpubError::Zip(ZipError::OutputTooSmall)));

        // 2. With 4096 bytes (the fixed limit), it must succeed
        let mut zip_full = ZipLocalStream::new(SliceStream {
            bytes: &zip_bytes,
            cursor: 0,
        });
        let mut full_container_scratch = [0u8; 4096];
        let mut zip_inflate = ZipInflateScratch::new(); // reset inflation
        let res_full = load_container_xml_and_find_opf_path(
            &mut zip_full,
            &mut header_scratch,
            &mut name_scratch,
            &mut compressed_scratch,
            &mut full_container_scratch,
            &mut zip_inflate,
            &mut opf_path_buf,
        );
        assert!(res_full.is_ok());
        assert_eq!(opf_path_buf.as_str(), "OEBPS/content.opf");
    }
}
