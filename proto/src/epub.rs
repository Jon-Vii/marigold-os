use crate::book::{BookId, BookMeta, BookSource, ChapterMeta, CoverStatus};
use crate::text::{FontStyle, TextAlign, TextBlock, TextRole};
use heapless::Vec;
use miniz_oxide::inflate::decompress_slice_iter_to_slice;
use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

// Upper bound on spine items (one per chapter file). The spine lives in the OPF
// arena, 8 bytes an item plus its href, not on the stack (see `Spine`), so in
// practice the arena's size decides how many fit; this caps the slot table so
// strings always keep room. A spine past either limit sets `spine_truncated`
// rather than silently dropping the tail. One value for firmware and host so a
// host test exercises the same ceiling the device does.
pub const MAX_SPINE_ITEMS: usize = 1024;
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
            // Same rules as `StreamingXmlTokenizer`: a comment ends only at
            // `-->`, and CDATA content is text.
            if let Some(body) = rest.strip_prefix("<!--") {
                let end = body.find("-->")?;
                self.cursor += 4 + end + 3;
                continue;
            }
            if let Some(body) = rest.strip_prefix("<![CDATA[") {
                let end = body.find("]]>")?;
                self.cursor += 9 + end + 3;
                if end > 0 {
                    return Some(Token::Text(&body[..end]));
                }
                continue;
            }
            if let Some(after_lt) = rest.strip_prefix('<') {
                let end = tag_close_index(after_lt)?;
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

/// Index of the `>` closing a tag body. Quoted attribute values may hold
/// `>`; declarations (`<!DOCTYPE ...>`) treat quotes literally, matching
/// the streaming tokenizer.
fn tag_close_index(after_lt: &str) -> Option<usize> {
    let bytes = after_lt.as_bytes();
    if bytes.first() == Some(&b'!') {
        return after_lt.find('>');
    }
    let mut quote = None;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return Some(index),
            None => {}
        }
    }
    None
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
    /// Whether any entry's name looks like an image, once known: the index
    /// pass learns it as the names stream by, and a walk otherwise.
    image_entries: Option<bool>,
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

    /// The inflater itself, lent to a decoder with a zlib stream of its own
    /// (a PNG's image data) while no entry is being read. Every entry read
    /// resets it first, so whatever the borrower leaves in it is harmless.
    pub fn inflater(&mut self) -> &mut InflateState {
        &mut self.state
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
        let mut image_entries = None;
        let lookup = if central_size as usize <= tail_scratch.len() {
            let central = &mut tail_scratch[..central_size as usize];
            read_exact_at(&mut reader, central_offset, central)?;
            CentralLookup::Cached(&central[..])
        } else {
            match build_central_index(&mut reader, central_offset, entry_count, tail_scratch) {
                Ok(index) => {
                    // An image among the indexed names settles it; without
                    // one, only an index of every entry rules images out.
                    image_entries = (index.images || index.complete).then_some(index.images);
                    CentralLookup::Indexed {
                        records: index.records,
                        complete: index.complete,
                    }
                }
                Err(_) => CentralLookup::Walk,
            }
        };
        Ok(Self {
            reader,
            central_offset,
            entry_count,
            lookup,
            image_entries,
        })
    }

    /// Calls `visit` with each central-directory entry, in order, until it
    /// returns false. A directory the tail scratch caches is walked in RAM;
    /// otherwise it streams from storage through `buf`, a run of entries
    /// per read. An entry whose header and name do not fit `buf` together,
    /// or whose name is not UTF-8, is passed over: nothing could look it up
    /// by name either.
    pub fn for_each_entry(
        &mut self,
        buf: &mut [u8],
        visit: &mut dyn FnMut(&ZipEntry<'_>) -> bool,
    ) -> Result<(), ZipError> {
        if let CentralLookup::Cached(central) = self.lookup {
            let mut cursor = 0usize;
            for _ in 0..self.entry_count {
                let header = CentralHeader::parse(central.get(cursor..).unwrap_or(&[]))?;
                let name_at = cursor + CENTRAL_HEADER_BYTES;
                let name = central
                    .get(name_at..name_at + header.name_len)
                    .ok_or(ZipError::BadCentralDirectory)?;
                if let Ok(name) = core::str::from_utf8(name) {
                    if !visit(&header.entry(name)) {
                        return Ok(());
                    }
                }
                cursor = usize::try_from(header.next(cursor as u32)?)
                    .map_err(|_| ZipError::BadCentralDirectory)?;
            }
            return Ok(());
        }
        let end = self.reader.len().map_err(|_| ZipError::Io)?;
        let mut window = CentralWindow { at: 0, held: 0 };
        let mut cursor = self.central_offset;
        for _ in 0..self.entry_count {
            if !window.cover(&mut self.reader, buf, end, cursor, CENTRAL_HEADER_BYTES)? {
                return Err(ZipError::BadCentralDirectory);
            }
            let header = CentralHeader::parse(window.bytes(buf, cursor))?;
            let named = CENTRAL_HEADER_BYTES + header.name_len;
            if window.cover(&mut self.reader, buf, end, cursor, named)? {
                let name = &window.bytes(buf, cursor)[CENTRAL_HEADER_BYTES..named];
                if let Ok(name) = core::str::from_utf8(name) {
                    if !visit(&header.entry(name)) {
                        return Ok(());
                    }
                }
            }
            cursor = header.next(cursor)?;
        }
        Ok(())
    }

    /// Whether any entry's name looks like an image
    /// ([`crate::image::index::is_image_path`]). Known without a read when
    /// the directory is cached, or when the index pass saw every name (or
    /// an image) on the way; otherwise the directory is walked through
    /// `buf` once, and the answer kept.
    pub fn has_image_entries(&mut self, buf: &mut [u8]) -> Result<bool, ZipError> {
        if let Some(known) = self.image_entries {
            return Ok(known);
        }
        let mut found = false;
        self.for_each_entry(buf, &mut |entry| {
            found = crate::image::index::is_image_path(entry.name);
            !found
        })?;
        self.image_entries = Some(found);
        Ok(found)
    }

    /// Reads the start of an entry found by its stored local-header offset
    /// rather than by name, as an image probe does, handing `visitor` its
    /// bytes from the first until it has what it needs or the entry ends.
    /// A stored entry is read a piece of `input` at a time, and what the
    /// visitor would pass over unread ([`EntryVisitor::skippable`]) is not
    /// read at all. A deflated one is inflated through `output`, its
    /// compressed bytes a piece of `input` at a time, and what the visitor
    /// passes over is inflated and dropped. Either way no more than `cap`
    /// bytes of the entry are read, nor, from a deflated one, inflated: a
    /// stream of a few KB can inflate to hundreds of MB.
    ///
    /// Only a failed read comes back as [`ZipError::Io`], which is worth
    /// retrying. Anything wrong with the archive itself is another error,
    /// the same every time: a local header that is missing, lies past the
    /// archive's end (checked against its length, so it never reads as a
    /// failed read), or names another entry (`is_name` says, given the
    /// header's name, which must fit `input`), data past the end, an
    /// unknown method, or a stream that does not inflate, or that reaches
    /// `cap` (read or inflated) before the visitor has what it needs. A
    /// stored entry that reaches `cap` first just ends, and the visitor
    /// judges what it has.
    #[allow(clippy::too_many_arguments)]
    pub fn visit_entry_start(
        &mut self,
        entry: OwnedZipEntry,
        is_name: &mut dyn FnMut(&[u8]) -> bool,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        cap: u32,
        visitor: &mut dyn EntryVisitor,
    ) -> Result<(), ZipError> {
        let payload = entry_payload_at(&mut self.reader, entry, input, is_name)?;
        match entry.compression_method {
            0 => {
                let size = entry.uncompressed_size.min(entry.compressed_size);
                let (mut at, mut read) = (0u32, 0u32);
                while at < size && read < cap && !input.is_empty() {
                    let len = (size - at).min(cap - read).min(input.len() as u32);
                    let piece = &mut input[..len as usize];
                    read_exact_at(&mut self.reader, payload + at, piece)?;
                    at += len;
                    read += len;
                    if !visitor.take(piece) {
                        return Ok(());
                    }
                    let skip = visitor.skippable().min(size - at);
                    if skip > 0 {
                        at += skip;
                        visitor.skip(skip);
                    }
                }
                Ok(())
            }
            8 => {
                let reader = &mut self.reader;
                let mut stopped = false;
                let mut inflated_len = 0u32;
                let inflated = inflate_chunks_to_sink(
                    |fetched, buf| read_exact_at(reader, payload + fetched, buf),
                    entry.compressed_size.min(cap),
                    input,
                    output,
                    inflate_scratch,
                    |chunk| {
                        if !visitor.take(chunk) {
                            stopped = true;
                            return Err(ZipError::OutputTooSmall);
                        }
                        inflated_len = inflated_len.saturating_add(chunk.len() as u32);
                        if inflated_len >= cap {
                            // Inflated as far as a probe may go, still
                            // wanting more: the visitor has failed.
                            return Err(ZipError::OutputTooSmall);
                        }
                        Ok(())
                    },
                );
                if stopped {
                    return Ok(());
                }
                inflated.map(|_| ())
            }
            _ => Err(ZipError::UnsupportedCompression),
        }
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

    /// Visits the central directory's entries; see
    /// [`ZipStream::for_each_entry`]. A forward-only reader cannot go back
    /// to the directory, and visits none.
    fn for_each_entry(
        &mut self,
        buf: &mut [u8],
        visit: &mut dyn FnMut(&ZipEntry<'_>) -> bool,
    ) -> Result<(), ZipError> {
        let _ = (buf, visit);
        Ok(())
    }

    /// Whether the archive has image entries; see
    /// [`ZipStream::has_image_entries`]. A forward-only reader says no: an
    /// image is read back by its offset, which it cannot do.
    fn has_image_entries(&mut self, buf: &mut [u8]) -> Result<bool, ZipError> {
        let _ = buf;
        Ok(false)
    }

    /// Reads the start of an entry by its stored offset; see
    /// [`ZipStream::visit_entry_start`]. A forward-only reader cannot go
    /// back, and finds nothing there.
    #[allow(clippy::too_many_arguments)]
    fn visit_entry_start(
        &mut self,
        entry: OwnedZipEntry,
        is_name: &mut dyn FnMut(&[u8]) -> bool,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        cap: u32,
        visitor: &mut dyn EntryVisitor,
    ) -> Result<(), ZipError> {
        let _ = (entry, is_name, input, output, inflate_scratch, cap, visitor);
        Err(ZipError::EntryNotFound)
    }
}

/// Takes an entry's bytes from its start, as
/// [`ZipStream::visit_entry_start`] reads them.
pub trait EntryVisitor {
    /// Takes the entry's next bytes; false once it needs no more.
    fn take(&mut self, bytes: &[u8]) -> bool;

    /// How many of the bytes right after those taken it would pass over
    /// unread. A reader that can seek does not read them, and says so with
    /// [`EntryVisitor::skip`].
    fn skippable(&self) -> u32 {
        0
    }

    /// The reader passed over `n` bytes, at most what
    /// [`EntryVisitor::skippable`] said.
    fn skip(&mut self, n: u32) {
        let _ = n;
    }
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

    fn for_each_entry(
        &mut self,
        buf: &mut [u8],
        visit: &mut dyn FnMut(&ZipEntry<'_>) -> bool,
    ) -> Result<(), ZipError> {
        ZipStream::for_each_entry(self, buf, visit)
    }

    fn has_image_entries(&mut self, buf: &mut [u8]) -> Result<bool, ZipError> {
        ZipStream::has_image_entries(self, buf)
    }

    fn visit_entry_start(
        &mut self,
        entry: OwnedZipEntry,
        is_name: &mut dyn FnMut(&[u8]) -> bool,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
        cap: u32,
        visitor: &mut dyn EntryVisitor,
    ) -> Result<(), ZipError> {
        ZipStream::visit_entry_start(
            self,
            entry,
            is_name,
            input,
            output,
            inflate_scratch,
            cap,
            visitor,
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

/// Where an entry found by its stored local-header offset starts its data,
/// once the local header checks out: present, inside the archive with all
/// of its data, and naming the entry (`is_name` says, given the header's
/// name, read into `name`, which must hold it).
///
/// Only a failed read comes back as [`ZipError::Io`], which is worth
/// retrying. A header that is missing, lies past the archive's end
/// (checked against its length, so it never reads as a failed read), or
/// names another entry is [`ZipError::BadLocalHeader`], the same every
/// time. An image is read this way, by the offset its index entry keeps.
pub fn entry_payload_at<R: ReadAt>(
    reader: &mut R,
    entry: OwnedZipEntry,
    name: &mut [u8],
    is_name: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<u32, ZipError> {
    let end = reader.len().map_err(|_| ZipError::Io)?;
    let within = |offset: u32, len: u32| offset.checked_add(len).filter(|&last| last <= end);
    let name_at = within(entry.local_header_offset, 30).ok_or(ZipError::BadLocalHeader)?;
    let mut header = [0u8; 30];
    read_exact_at(reader, entry.local_header_offset, &mut header)?;
    if read_u32(&header, 0)? != 0x0403_4b50 {
        return Err(ZipError::BadLocalHeader);
    }
    let name_len = read_u16(&header, 26)? as usize;
    let extra_len = u32::from(read_u16(&header, 28)?);
    let name = name.get_mut(..name_len).ok_or(ZipError::BadLocalHeader)?;
    within(name_at, name_len as u32).ok_or(ZipError::BadLocalHeader)?;
    read_exact_at(reader, name_at, name)?;
    if !is_name(name) {
        return Err(ZipError::BadLocalHeader);
    }
    let payload = (name_at + name_len as u32)
        .checked_add(extra_len)
        .ok_or(ZipError::BadLocalHeader)?;
    within(payload, entry.compressed_size).ok_or(ZipError::BadLocalHeader)?;
    Ok(payload)
}

/// Fills `out` from `offset`; a short read is [`ZipError::Io`].
pub fn read_payload_at<R: ReadAt>(
    reader: &mut R,
    offset: u32,
    out: &mut [u8],
) -> Result<(), ZipError> {
    read_exact_at(reader, offset, out)
}

/// Fills a buffer with compressed bytes from an offset into an entry's data.
pub type FetchAt<'a> = dyn FnMut(u32, &mut [u8]) -> Result<(), ZipError> + 'a;

/// Inflates a deflated entry's `compressed_size` bytes of data and hands
/// `emit` the output a full `output` at a time, as
/// [`ZipStream::read_entry_to_sink`] does. `fetch` fills a buffer with the
/// compressed bytes from an offset into the data (for an entry found by
/// [`entry_payload_at`], a read of the archive at the payload plus that
/// offset), a piece of `input` at a time. Either closure stops the inflate
/// by returning an error, which comes back unchanged: `fetch` gives a
/// caller a say between pieces even where the output never fills.
pub fn inflate_to_sink(
    fetch: &mut FetchAt<'_>,
    compressed_size: u32,
    input: &mut [u8],
    output: &mut [u8],
    inflate_scratch: &mut ZipInflateScratch,
    emit: &mut dyn FnMut(&[u8]) -> Result<(), ZipError>,
) -> Result<(), ZipError> {
    inflate_chunks_to_sink(fetch, compressed_size, input, output, inflate_scratch, emit).map(|_| ())
}

/// What [`build_central_index`] made of the directory.
struct CentralIndex<'a> {
    records: &'a [u8],
    /// Every entry fit.
    complete: bool,
    /// An indexed entry's name looks like an image.
    images: bool,
}

/// One sequential pass over a central directory too large for the tail
/// scratch, emitting bounded `(name hash, entry)` records into `scratch`.
/// Returns the encoded records, whether every entry fit, and whether any
/// indexed name looks like an image. Sequential reads are the access
/// pattern storage likes; afterwards each lookup is a RAM scan plus one
/// local-header verification read, instead of a per-lookup walk of the
/// whole directory on storage.
fn build_central_index<'a, R>(
    reader: &mut R,
    central_offset: u32,
    entry_count: u16,
    scratch: &'a mut [u8],
) -> Result<CentralIndex<'a>, ZipError>
where
    R: ReadAt,
{
    let capacity = scratch.len() / CENTRAL_INDEX_RECORD_BYTES;
    let indexed = (entry_count as usize).min(capacity);
    let mut cursor = central_offset;
    let mut header = [0u8; 46];
    let mut name_chunk = [0u8; 64];
    let mut images = false;
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
        // The first read takes what is left over past whole chunks, so the
        // last holds the name's end, where an image's extension is.
        let mut take = match name_len as usize % name_chunk.len() {
            0 => name_chunk.len(),
            rest => rest,
        };
        let mut last = 0;
        let mut utf8 = Utf8Pieces::new();
        while remaining > 0 {
            take = take.min(remaining as usize);
            read_exact_at(reader, name_cursor, &mut name_chunk[..take])?;
            hash = fnv1a_update(hash, &name_chunk[..take]);
            utf8.push(&name_chunk[..take]);
            name_cursor = name_cursor
                .checked_add(take as u32)
                .ok_or(ZipError::BadCentralDirectory)?;
            remaining -= take as u32;
            last = take;
            take = name_chunk.len();
        }
        // A tail of a longer name still holds the extension and the byte
        // before its dot, which is all the check looks at. A name that is
        // not UTF-8 is passed over, as a walk passes it over: nothing could
        // look it up.
        images |= utf8.whole() && crate::image::index::is_image_name(&name_chunk[..last]);

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
    Ok(CentralIndex {
        records: &scratch[..indexed * CENTRAL_INDEX_RECORD_BYTES],
        complete: indexed == entry_count as usize,
        images,
    })
}

/// Checks that a name read in pieces is UTF-8, though a character may
/// straddle two pieces: a piece's unfinished last character is held over
/// and finished with the next piece's first bytes.
struct Utf8Pieces {
    held: [u8; 4],
    held_len: usize,
    valid: bool,
}

impl Utf8Pieces {
    fn new() -> Self {
        Self {
            held: [0; 4],
            held_len: 0,
            valid: true,
        }
    }

    fn push(&mut self, mut piece: &[u8]) {
        if !self.valid {
            return;
        }
        if self.held_len > 0 {
            // The held bytes start a character that from_utf8 found
            // unfinished, so its first byte gives its length.
            let width = match self.held[0] {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            let take = (width - self.held_len).min(piece.len());
            self.held[self.held_len..self.held_len + take].copy_from_slice(&piece[..take]);
            self.held_len += take;
            piece = &piece[take..];
            if self.held_len < width {
                return;
            }
            self.valid = core::str::from_utf8(&self.held[..width]).is_ok();
            self.held_len = 0;
            if !self.valid {
                return;
            }
        }
        match core::str::from_utf8(piece) {
            Ok(_) => {}
            Err(error) if error.error_len().is_none() => {
                let rest = &piece[error.valid_up_to()..];
                self.held[..rest.len()].copy_from_slice(rest);
                self.held_len = rest.len();
            }
            Err(_) => self.valid = false,
        }
    }

    /// The pieces so far make a whole UTF-8 string.
    fn whole(&self) -> bool {
        self.valid && self.held_len == 0
    }
}

const CENTRAL_HEADER_BYTES: usize = 46;

/// A central-directory entry's fixed 46-byte header.
struct CentralHeader {
    method: u16,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u32,
    name_len: usize,
    /// Extra field and comment, which follow the name.
    trailer_len: u32,
}

impl CentralHeader {
    fn parse(bytes: &[u8]) -> Result<Self, ZipError> {
        if read_u32(bytes, 0)? != 0x0201_4b50 {
            return Err(ZipError::BadCentralDirectory);
        }
        Ok(Self {
            method: read_u16(bytes, 10)?,
            compressed_size: read_u32(bytes, 20)?,
            uncompressed_size: read_u32(bytes, 24)?,
            name_len: usize::from(read_u16(bytes, 28)?),
            trailer_len: u32::from(read_u16(bytes, 30)?) + u32::from(read_u16(bytes, 32)?),
            local_header_offset: read_u32(bytes, 42)?,
        })
    }

    fn entry<'n>(&self, name: &'n str) -> ZipEntry<'n> {
        ZipEntry {
            name,
            compression_method: self.method,
            compressed_size: self.compressed_size,
            uncompressed_size: self.uncompressed_size,
            local_header_offset: self.local_header_offset,
        }
    }

    /// Where the next entry starts, for this one at `at`.
    fn next(&self, at: u32) -> Result<u32, ZipError> {
        at.checked_add(CENTRAL_HEADER_BYTES as u32 + self.name_len as u32)
            .and_then(|end| end.checked_add(self.trailer_len))
            .ok_or(ZipError::BadCentralDirectory)
    }
}

/// The stretch of the archive a walk's buffer holds: `held` bytes from
/// offset `at`.
struct CentralWindow {
    at: u32,
    held: usize,
}

impl CentralWindow {
    /// Makes the buffer hold `len` bytes from `offset`, reading a buffer's
    /// worth from there when it does not already. False when `len` is more
    /// than the buffer holds.
    fn cover<R: ReadAt>(
        &mut self,
        reader: &mut R,
        buf: &mut [u8],
        end: u32,
        offset: u32,
        len: usize,
    ) -> Result<bool, ZipError> {
        if len > buf.len() {
            return Ok(false);
        }
        let start = offset.wrapping_sub(self.at) as usize;
        if offset >= self.at && start + len <= self.held {
            return Ok(true);
        }
        let available = end
            .checked_sub(offset)
            .ok_or(ZipError::BadCentralDirectory)? as usize;
        if available < len {
            return Err(ZipError::BadCentralDirectory);
        }
        let take = buf.len().min(available);
        self.held = 0;
        read_exact_at(reader, offset, &mut buf[..take])?;
        (self.at, self.held) = (offset, take);
        Ok(true)
    }

    /// The held bytes from `offset`, which [`CentralWindow::cover`] made
    /// sure of.
    fn bytes<'b>(&self, buf: &'b [u8], offset: u32) -> &'b [u8] {
        let start = offset.wrapping_sub(self.at) as usize;
        buf.get(start..self.held).unwrap_or(&[])
    }
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

    /// An `<img>` in the chapter. The parser has already ended the open
    /// paragraph, unless the sink [`XhtmlBlockSink::wants_inline_images`]:
    /// then it has ended only the open run (pushed as a block that does not
    /// end its paragraph, a lone space included), and the image belongs to
    /// the paragraph, between that run and the next.
    ///
    /// Both values are raw attribute slices borrowed from the parser's
    /// input, entities and percent-escapes intact: `src` is empty when the
    /// tag has none, and `alt` is `None` when the tag has no `alt`. Resolve
    /// `src` with [`crate::image::path::resolve_image_path`]; nothing is
    /// copied here, because this runs at the deepest point of a book build.
    ///
    /// The default pushes the alt-text placeholder the reader has always
    /// shown, through [`push_image_placeholder`].
    fn push_image(&mut self, src: &str, alt: Option<&str>) -> Result<(), XhtmlError> {
        let _ = src;
        push_image_placeholder(self, alt)
    }

    /// An `<img>` in a block whose start tag floats it left (class
    /// `figleft`, or a `float: left` style), for a sink that
    /// [`XhtmlBlockSink::wants_inline_images`]: Project Gutenberg's markup
    /// for a drop cap set beside the paragraph it starts. Otherwise as
    /// [`XhtmlBlockSink::push_image`], which the default calls.
    fn push_floated_image(&mut self, src: &str, alt: Option<&str>) -> Result<(), XhtmlError> {
        self.push_image(src, alt)
    }

    /// Whether an `<img>` stays in its paragraph. The default, false, ends
    /// the paragraph before every image, as the parser always did, so a
    /// sink that does not lay images out inside lines sees no change.
    fn wants_inline_images(&self) -> bool {
        false
    }

    /// The `<image>` inside an `<svg>` wrapper, for a sink whose
    /// [`XhtmlBlockSink::wants_svg_images`] says so. The parser has already
    /// ended the open paragraph. `href` is the raw `href` or `xlink:href`,
    /// as `src` is for [`XhtmlBlockSink::push_image`]; an svg image has no
    /// alt. The default pushes nothing: an svg image never had a
    /// placeholder, so one that cannot be shown leaves no trace.
    fn push_svg_image(&mut self, href: &str) -> Result<(), XhtmlError> {
        let _ = href;
        Ok(())
    }

    /// Whether the first `<image>` with an `href` or `xlink:href` inside
    /// each `<svg>` (the markup most covers use) reaches
    /// [`XhtmlBlockSink::push_svg_image`]. The rest of the svg is skipped
    /// either way. The default, false, keeps svg subtrees skipped whole with
    /// no calls at all, as before images had hooks, so the paragraph around
    /// an svg is not ended early either.
    fn wants_svg_images(&self) -> bool {
        false
    }
}

/// The placeholder an image has always become: its `alt` with entities
/// decoded and whitespace collapsed, or `[Image]` when it has none, as one
/// italic centred paragraph of at most 160 bytes. An empty `alt` pushes an
/// empty paragraph, which sinks drop.
///
/// The default [`XhtmlBlockSink::push_image`]; a sink that shows images
/// calls it for the ones it cannot show.
// Kept out of line so its 160-byte buffer stays off the parser's own frame,
// which every block of every chapter passes through: on the X3 that frame
// is 64 bytes with this out of line and 208 with it inlined.
#[inline(never)]
pub fn push_image_placeholder<S: XhtmlBlockSink + ?Sized>(
    sink: &mut S,
    alt: Option<&str>,
) -> Result<(), XhtmlError> {
    let mut placeholder = heapless::String::<160>::new();
    append_owned_text(&mut placeholder, alt.unwrap_or("[Image]"));
    sink.push_block(
        placeholder.as_str(),
        TextRole::Body,
        FontStyle::Italic,
        TextAlign::Center,
        true,
    )
}

/// The text the placeholder of an image that cannot be shown carries, as
/// [`push_image_placeholder`] builds it (its `alt` with entities decoded
/// and HTML whitespace collapsed, or `[Image]` when it has none), written
/// into `out` and cut on a character boundary where `out` ends. Returns its
/// length. A sink that holds an image back before deciding where it goes
/// keeps this instead of the borrowed attribute.
pub fn image_alt_text(alt: Option<&str>, out: &mut [u8]) -> usize {
    let text = alt.unwrap_or("[Image]");
    let mut len = 0usize;
    let mut previous_space = false;
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
        let ch = if is_html_space(ch) {
            if previous_space {
                cursor += advance;
                continue;
            }
            ' '
        } else {
            ch
        };
        let Some(room) = out.get_mut(len..len + ch.len_utf8()) else {
            break;
        };
        len += ch.encode_utf8(room).len();
        previous_space = ch == ' ';
        cursor += advance;
    }
    len
}

/// Whether an image's `alt` (the raw attribute) names a single letter,
/// optionally after an opening quote: how Project Gutenberg labels a drop
/// cap set as an image (`alt="T"`, `alt="&quot;C"`).
pub fn alt_is_initial(alt: &str) -> bool {
    let mut out = [0u8; 16];
    let len = image_alt_text(Some(alt), &mut out);
    let text = core::str::from_utf8(&out[..len]).unwrap_or("").trim();
    let mut chars = text.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), None, None) => letter.is_alphabetic(),
        (Some(quote), Some(letter), None) => {
            !quote.is_alphanumeric() && !quote.is_whitespace() && letter.is_alphabetic()
        }
        _ => false,
    }
}

pub trait EpubTocSink {
    fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError>;

    /// The EPUB 3 nav's `bodymatter` landmark: where the book's main text
    /// starts, after its front matter. Reported at most once.
    fn push_bodymatter(&mut self, _href: &str) {}
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
/// `BlockRecord`, `SpineRecord`). Spans index the OPF string arena
/// (`EpubPackage::opf_text`), which is at most `u16::MAX` bytes.
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
    /// The package's first `dc:language` tag (`en`, `en-GB`, `sv`, ...).
    pub language: Option<&'a str>,
    /// The guide's `cover` page reference: an XHTML page that only frames
    /// the cover image (not the image itself, which is `cover_href`).
    pub cover_page_href: Option<&'a str>,
    pub nav_href: Option<&'a str>,
    pub ncx_href: Option<&'a str>,
    /// The cover image's href: the `cover-image` item, else an image whose id
    /// or href says cover.
    pub cover_href: Option<&'a str>,
    /// True when the OPF held more spine items than `MAX_SPINE_ITEMS`, so the
    /// spine (and the book) is clipped. Callers flag the book partial instead
    /// of silently dropping the tail chapters.
    pub spine_truncated: bool,
    pub spine: Spine<'a>,
}

/// The package's spine, in reading order, as stored in the OPF arena: one
/// `SPINE_SLOT_BYTES` slot per item at the arena's top (item 0 highest), each
/// holding the item's href span and its interned media type and properties.
#[derive(Clone, Copy)]
pub struct Spine<'a> {
    /// The arena up to its top; slot `i` ends `i * SPINE_SLOT_BYTES` below.
    arena: &'a [u8],
    len: usize,
    interned: [Span; OPF_INTERNED_VALUES],
}

impl<'a> Spine<'a> {
    const fn empty() -> Self {
        Self {
            arena: &[],
            len: 0,
            interned: [Span { off: 0, len: 0 }; OPF_INTERNED_VALUES],
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<SpineItem> {
        if index >= self.len {
            return None;
        }
        let slot = SpineSlot::read(self.arena, index);
        let interned = |value: u8| {
            self.interned
                .get(value as usize)
                .copied()
                .unwrap_or_default()
        };
        Some(SpineItem {
            href: slot.href,
            media_type: interned(slot.media_type),
            properties: interned(slot.properties),
        })
    }

    pub fn first(&self) -> Option<SpineItem> {
        self.get(0)
    }

    pub fn iter(&self) -> impl Iterator<Item = SpineItem> + Clone + 'a {
        let spine = *self;
        (0..spine.len).filter_map(move |index| spine.get(index))
    }
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
    opf_arena: &'a mut [u8],
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
    parse_opf(
        &opf_scratch[..opf_len],
        opf_arena,
        book_id,
        source_path,
        epub_bytes.len() as u32,
        opf_path,
    )
}

/// Parse an OPF held whole in memory. Thin wrapper over
/// [`parse_opf_streamed`], so slice and streamed callers share one parser.
pub fn parse_opf<'a>(
    opf_xml: &[u8],
    arena: &'a mut [u8],
    book_id: BookId,
    source_path: &'a str,
    byte_size: u32,
    opf_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError> {
    parse_opf_streamed(
        |feed| feed(opf_xml),
        arena,
        book_id,
        source_path,
        byte_size,
        opf_path,
    )
}

/// Parse an OPF package document of any size in bounded memory.
///
/// `replay` streams the whole OPF (as decoded bytes, in any chunking) into
/// the callback it is given; it is called two or three times. The first
/// pass records which manifest ids the spine uses; the second keeps only
/// those manifest items (plus nav/NCX/cover) and resolves the spine; a
/// third runs only for OPFs whose spine resolves to nothing, falling back
/// to manifest order. Every string the package refers to is copied into
/// `arena` (`EpubPackage::opf_text` then views it), so the OPF itself never
/// has to fit in RAM. A spine longer than `MAX_SPINE_ITEMS` or an arena
/// that runs out marks the package `spine_truncated` instead of silently
/// dropping chapters.
///
/// Never inlined: its tokenizer, spine filter and manifest table are
/// several KB, and must be popped before the caller streams chapters on the
/// deepest stack path of the EPUB build.
#[inline(never)]
pub fn parse_opf_streamed<'a, R>(
    mut replay: R,
    arena: &'a mut [u8],
    book_id: BookId,
    source_path: &'a str,
    byte_size: u32,
    opf_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError>
where
    R: FnMut(&mut dyn FnMut(&[u8]) -> Result<(), EpubError>) -> Result<(), EpubError>,
{
    // Pass 1: one slot per itemref, in spine order, holding its idref's hash
    // until the manifest item it names is found.
    let top = arena.len().min(u16::MAX as usize);
    let capacity = MAX_SPINE_ITEMS.min(top / (SPINE_SLOT_BYTES + SPINE_MIN_HREF_ROOM));
    let mut slots = 0usize;
    let mut spine_overflow = false;
    let mut tokenizer = StreamingXmlTokenizer::new();
    {
        let mut in_spine = false;
        let arena = &mut *arena;
        replay(&mut |chunk| {
            tokenizer.feed(chunk, &mut |event| {
                match event {
                    TokEvent::StartTag(tag) if tag_name_is(tag, "spine") => in_spine = true,
                    TokEvent::EndTag(tag) if tag_name_is(tag, "spine") => in_spine = false,
                    TokEvent::StartTag(tag) if in_spine && tag_name_is(tag, "itemref") => {
                        if let Some(idref) = attr_value(tag, "idref") {
                            if slots == capacity {
                                spine_overflow = true;
                            } else {
                                SpineSlot::unresolved(id_hash48(idref)).write(arena, slots);
                                slots += 1;
                            }
                        }
                    }
                    _ => {}
                }
                Ok::<(), EpubError>(())
            })
        })?;
    }

    let mut package = EpubPackage {
        meta: BookMeta {
            id: book_id,
            title: "Untitled",
            author: "Unknown Author",
            source_path,
            byte_size,
            source: BookSource::MicroSd,
            cover_status: CoverStatus::Missing,
        },
        opf_text: "",
        opf_path,
        text_reference_href: None,
        language: None,
        cover_page_href: None,
        nav_href: None,
        ncx_href: None,
        cover_href: None,
        spine_truncated: false,
        spine: Spine::empty(),
    };

    let summary = {
        let mut collector = OpfCollector::new(arena, slots);
        for fallback in [false, true] {
            if fallback && collector.resolved_slots() > 0 {
                break;
            }
            collector.begin_pass(fallback);
            tokenizer = StreamingXmlTokenizer::new();
            replay(&mut |chunk| {
                tokenizer.feed(chunk, &mut |event| {
                    collector.handle(event);
                    Ok::<(), EpubError>(())
                })
            })?;
            tokenizer.finish(&mut |event| {
                collector.handle(event);
                Ok::<(), EpubError>(())
            })?;
        }
        collector.compact_slots();
        collector.summary()
    };

    let arena: &'a [u8] = arena;
    package.spine = Spine {
        arena: &arena[..top],
        len: summary.slots,
        interned: summary.interned,
    };
    let text = core::str::from_utf8(&arena[..summary.len]).map_err(|_| EpubError::Utf8)?;
    package.opf_text = text;
    if let Some(title) = summary.title {
        package.meta.title = title.of(text);
    }
    if let Some(author) = summary.author {
        package.meta.author = author.of(text);
    }
    package.language = summary.language.map(|span| span.of(text));
    package.text_reference_href = summary
        .text_reference
        .or(summary.start_reference)
        .map(|span| strip_fragment(span.of(text)));
    package.cover_page_href = summary
        .cover_reference
        .map(|span| strip_fragment(span.of(text)));
    package.nav_href = summary.nav.map(|span| span.of(text));
    package.ncx_href = summary.ncx.map(|span| span.of(text));
    package.cover_href = summary.cover.map(|span| span.of(text));
    package.meta.cover_status = if summary.any_cover {
        CoverStatus::Present
    } else {
        CoverStatus::Missing
    };
    package.spine_truncated = spine_overflow || summary.lost;
    Ok(package)
}

/// Bytes a spine slot takes at the top of the OPF arena.
const SPINE_SLOT_BYTES: usize = 8;
/// Arena room kept per slot for its href, so a huge spine cannot crowd the
/// strings out entirely: items past it are flagged lost instead.
const SPINE_MIN_HREF_ROOM: usize = 16;
/// Marks a slot whose manifest item has not been seen yet.
const SLOT_UNRESOLVED: u8 = 0xFE;

/// 48 bits of FNV-1a 64 over a manifest id. Pass 1 keeps the itemrefs' ids
/// only as these; a manifest item claims every slot with its hash. Two of a
/// book's ids sharing one is a ~1e-9 chance.
fn id_hash48(id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash & 0xFFFF_FFFF_FFFF
}

/// One spine item's slot. Unresolved, its first six bytes are the idref's
/// hash; resolved, the first four are the href span.
#[derive(Clone, Copy)]
struct SpineSlot {
    hash: u64,
    href: Span,
    media_type: u8,
    properties: u8,
}

impl SpineSlot {
    fn unresolved(hash: u64) -> Self {
        Self {
            hash,
            href: Span::default(),
            media_type: SLOT_UNRESOLVED,
            properties: SLOT_UNRESOLVED,
        }
    }

    fn is_resolved(&self) -> bool {
        self.media_type != SLOT_UNRESOLVED
    }

    fn at(arena_top: usize, index: usize) -> usize {
        arena_top - (index + 1) * SPINE_SLOT_BYTES
    }

    fn read(arena: &[u8], index: usize) -> Self {
        let top = arena.len().min(u16::MAX as usize);
        let at = Self::at(top, index);
        let bytes = &arena[at..at + SPINE_SLOT_BYTES];
        let media_type = bytes[6];
        let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        if media_type == SLOT_UNRESOLVED {
            let mut hash = [0u8; 8];
            hash[..6].copy_from_slice(&bytes[..6]);
            Self::unresolved(u64::from_le_bytes(hash))
        } else {
            Self {
                hash: 0,
                href: Span {
                    off: word(0),
                    len: word(2),
                },
                media_type,
                properties: bytes[7],
            }
        }
    }

    fn write(&self, arena: &mut [u8], index: usize) {
        let top = arena.len().min(u16::MAX as usize);
        let at = Self::at(top, index);
        let bytes = &mut arena[at..at + SPINE_SLOT_BYTES];
        if self.is_resolved() {
            bytes[0..2].copy_from_slice(&self.href.off.to_le_bytes());
            bytes[2..4].copy_from_slice(&self.href.len.to_le_bytes());
            bytes[4..6].fill(0);
        } else {
            bytes[..6].copy_from_slice(&self.hash.to_le_bytes()[..6]);
        }
        bytes[6] = self.media_type;
        bytes[7] = self.properties;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpfCapture {
    None,
    Title,
    Author,
    Language,
}

struct OpfSummary {
    len: usize,
    slots: usize,
    interned: [Span; OPF_INTERNED_VALUES],
    title: Option<Span>,
    author: Option<Span>,
    language: Option<Span>,
    text_reference: Option<Span>,
    start_reference: Option<Span>,
    cover_reference: Option<Span>,
    nav: Option<Span>,
    ncx: Option<Span>,
    cover: Option<Span>,
    any_cover: bool,
    lost: bool,
}

/// Strings grow up from the arena's bottom; the spine slots sit at its top.
/// Media types and properties repeat across items, so each distinct value is
/// stored once (up to this many) and slots refer to it by index.
const OPF_INTERNED_VALUES: usize = 8;
const OPF_NO_INTERNED: u8 = u8::MAX;

/// Second/third-pass state: copies the strings the package needs into the
/// arena and fills the spine slots pass 1 laid out, as the manifest items
/// they name stream by.
struct OpfCollector<'b> {
    arena: &'b mut [u8],
    len: usize,
    /// Spine slots in use; they occupy the arena's top.
    slots: usize,
    fallback: bool,
    in_manifest: bool,
    in_spine: bool,
    in_guide: bool,
    capture: OpfCapture,
    capture_text: heapless::String<256>,
    title: Option<Span>,
    author: Option<Span>,
    language: Option<Span>,
    text_reference: Option<Span>,
    start_reference: Option<Span>,
    cover_reference: Option<Span>,
    nav: Option<Span>,
    ncx: Option<Span>,
    cover: Option<Span>,
    image_cover: Option<Span>,
    any_cover: bool,
    interned: Vec<Span, OPF_INTERNED_VALUES>,
    lost: bool,
}

impl<'b> OpfCollector<'b> {
    fn new(arena: &'b mut [u8], slots: usize) -> Self {
        Self {
            arena,
            len: 0,
            slots,
            fallback: false,
            in_manifest: false,
            in_spine: false,
            in_guide: false,
            capture: OpfCapture::None,
            capture_text: heapless::String::new(),
            title: None,
            author: None,
            language: None,
            text_reference: None,
            start_reference: None,
            cover_reference: None,
            nav: None,
            ncx: None,
            cover: None,
            image_cover: None,
            any_cover: false,
            interned: Vec::new(),
            lost: false,
        }
    }

    fn begin_pass(&mut self, fallback: bool) {
        self.fallback = fallback;
        if fallback {
            // Nothing resolved: the spine is rebuilt in manifest order.
            self.slots = 0;
        }
        self.in_manifest = false;
        self.in_spine = false;
        self.in_guide = false;
        self.capture = OpfCapture::None;
    }

    fn summary(&self) -> OpfSummary {
        let mut interned = [Span::default(); OPF_INTERNED_VALUES];
        interned[..self.interned.len()].copy_from_slice(&self.interned);
        OpfSummary {
            len: self.len,
            slots: self.slots,
            interned,
            title: self.title,
            author: self.author,
            language: self.language,
            text_reference: self.text_reference,
            start_reference: self.start_reference,
            cover_reference: self.cover_reference,
            nav: self.nav,
            ncx: self.ncx,
            cover: self.cover.or(self.image_cover),
            any_cover: self.any_cover,
            lost: self.lost,
        }
    }

    fn text(&self, span: Span) -> &str {
        let start = span.off as usize;
        core::str::from_utf8(&self.arena[start..start + span.len as usize]).unwrap_or("")
    }

    fn top(&self) -> usize {
        self.arena.len().min(u16::MAX as usize)
    }

    /// Where the spine slots begin; strings must stay below it.
    fn slots_start(&self) -> usize {
        self.top() - self.slots * SPINE_SLOT_BYTES
    }

    fn push(&mut self, text: &str) -> Option<Span> {
        let end = self.len + text.len();
        if end > self.slots_start() {
            self.lost = true;
            return None;
        }
        self.arena[self.len..end].copy_from_slice(text.as_bytes());
        let span = Span {
            off: self.len as u16,
            len: text.len() as u16,
        };
        self.len = end;
        Some(span)
    }

    /// Media types and properties repeat across every item; store each
    /// distinct value once and refer to it by index.
    fn intern(&mut self, text: &str) -> Option<u8> {
        if text.is_empty() {
            return Some(OPF_NO_INTERNED);
        }
        if let Some(index) = self
            .interned
            .iter()
            .position(|span| self.text(*span) == text)
        {
            return Some(index as u8);
        }
        if self.interned.is_full() {
            return Some(OPF_NO_INTERNED);
        }
        let span = self.push(text)?;
        let _ = self.interned.push(span);
        Some((self.interned.len() - 1) as u8)
    }

    fn resolved_slots(&self) -> usize {
        (0..self.slots)
            .filter(|index| SpineSlot::read(self.arena, *index).is_resolved())
            .count()
    }

    /// Drops slots no manifest item filled (an itemref naming nothing),
    /// keeping spine order.
    fn compact_slots(&mut self) {
        let mut kept = 0;
        for index in 0..self.slots {
            let slot = SpineSlot::read(self.arena, index);
            if slot.is_resolved() {
                slot.write(self.arena, kept);
                kept += 1;
            }
        }
        self.slots = kept;
    }

    /// Adds a resolved slot below the existing ones (the manifest-order
    /// fallback), if the strings leave room.
    fn push_slot(&mut self, slot: SpineSlot) -> bool {
        if self.slots >= MAX_SPINE_ITEMS || self.slots_start() < self.len + SPINE_SLOT_BYTES {
            self.lost = true;
            return false;
        }
        self.slots += 1;
        slot.write(self.arena, self.slots - 1);
        true
    }

    fn handle(&mut self, event: TokEvent<'_>) {
        match event {
            TokEvent::StartTag(tag) => {
                let self_closing = tag.ends_with('/');
                if tag_name_is(tag, "manifest") {
                    self.in_manifest = !self_closing;
                } else if tag_name_is(tag, "spine") {
                    self.in_spine = !self_closing;
                } else if tag_name_is(tag, "guide") {
                    self.in_guide = !self_closing;
                } else if self.in_manifest && tag_name_is(tag, "item") {
                    self.handle_item(tag);
                } else if self.in_guide && tag_name_is(tag, "reference") {
                    if !self.fallback {
                        self.handle_reference(tag);
                    }
                } else if !self_closing && !self.fallback {
                    if self.title.is_none() && tag_name_is(tag, "title") {
                        self.capture = OpfCapture::Title;
                        self.capture_text.clear();
                    } else if self.author.is_none() && tag_name_is(tag, "creator") {
                        self.capture = OpfCapture::Author;
                        self.capture_text.clear();
                    } else if self.language.is_none() && tag_name_is(tag, "language") {
                        self.capture = OpfCapture::Language;
                        self.capture_text.clear();
                    }
                }
            }
            TokEvent::EndTag(tag) => {
                if tag_name_is(tag, "manifest") {
                    self.in_manifest = false;
                } else if tag_name_is(tag, "spine") {
                    self.in_spine = false;
                } else if tag_name_is(tag, "guide") {
                    self.in_guide = false;
                } else if self.capture != OpfCapture::None
                    && (tag_name_is(tag, "title")
                        || tag_name_is(tag, "creator")
                        || tag_name_is(tag, "language"))
                {
                    let mut value = heapless::String::<256>::new();
                    let _ = value.push_str(self.capture_text.as_str().trim());
                    let span = if value.is_empty() {
                        None
                    } else {
                        self.push(value.as_str())
                    };
                    match self.capture {
                        OpfCapture::Title => self.title = span,
                        OpfCapture::Author => self.author = span,
                        OpfCapture::Language => self.language = span,
                        OpfCapture::None => {}
                    }
                    self.capture = OpfCapture::None;
                }
            }
            TokEvent::Text(text) => {
                if self.capture != OpfCapture::None {
                    append_owned_text(&mut self.capture_text, text);
                }
            }
        }
    }

    fn handle_item(&mut self, tag: &str) {
        let Some(id) = attr_value(tag, "id") else {
            return;
        };
        let Some(href) = attr_value(tag, "href") else {
            return;
        };
        let media_type = attr_value(tag, "media-type").unwrap_or("");
        let properties = attr_value(tag, "properties").unwrap_or("");
        if self.fallback {
            if !manifest_item_is_reading_candidate(href, media_type, properties) {
                return;
            }
            let (Some(href), Some(media_type), Some(properties)) = (
                self.push(href),
                self.intern(media_type),
                self.intern(properties),
            ) else {
                return;
            };
            self.push_slot(SpineSlot {
                hash: 0,
                href,
                media_type,
                properties,
            });
            return;
        }
        let is_nav = properties
            .split_ascii_whitespace()
            .any(|prop| prop == "nav");
        let is_ncx =
            media_type.eq_ignore_ascii_case("application/x-dtbncx+xml") || href.ends_with(".ncx");
        let is_cover_image = properties
            .split_ascii_whitespace()
            .any(|prop| prop == "cover-image");
        let names_cover = id == "cover" || href.contains("cover");
        let is_image = media_type.starts_with("image/");
        if names_cover || is_cover_image {
            self.any_cover = true;
        }
        if is_nav && self.nav.is_none() {
            self.nav = self.push(href);
        }
        if is_ncx && self.ncx.is_none() {
            self.ncx = self.push(href);
        }
        if is_cover_image && self.cover.is_none() {
            self.cover = self.push(href);
        } else if is_image
            && self.image_cover.is_none()
            && (id.eq_ignore_ascii_case("cover") || href.to_ascii_lowercase_contains("cover"))
        {
            self.image_cover = self.push(href);
        }
        // Fill every spine slot that names this item (a spine may list an
        // item twice); its strings are stored once.
        let hash = id_hash48(id);
        let mut resolved: Option<SpineSlot> = None;
        for index in 0..self.slots {
            let slot = SpineSlot::read(self.arena, index);
            if slot.is_resolved() || slot.hash != hash {
                continue;
            }
            let fill = match resolved {
                Some(fill) => fill,
                None => {
                    let (Some(href), Some(media_type), Some(properties)) = (
                        self.push(href),
                        self.intern(media_type),
                        self.intern(properties),
                    ) else {
                        return;
                    };
                    let fill = SpineSlot {
                        hash: 0,
                        href,
                        media_type,
                        properties,
                    };
                    resolved = Some(fill);
                    fill
                }
            };
            fill.write(self.arena, index);
        }
    }

    fn handle_reference(&mut self, tag: &str) {
        let Some(href) = attr_value(tag, "href") else {
            return;
        };
        if self.text_reference.is_none() && attr_word_eq(tag, "type", "text") {
            self.text_reference = self.push(href);
        } else if self.start_reference.is_none() && attr_word_eq(tag, "type", "start") {
            self.start_reference = self.push(href);
        } else if self.cover_reference.is_none() && attr_word_eq(tag, "type", "cover") {
            self.cover_reference = self.push(href);
        }
    }
}

/// ASCII-case-insensitive substring test without allocating.
trait AsciiContains {
    fn to_ascii_lowercase_contains(&self, needle: &str) -> bool;
}

impl AsciiContains for str {
    fn to_ascii_lowercase_contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        self.as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    }
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
                            .map(|ch| is_html_space(ch) || is_leading_punctuation_char(ch))
                            .unwrap_or(true)
                    {
                        let _ = last.text.push(' ');
                    }
                    append_owned_text(&mut last.text, text);
                    trim_owned(&mut last.text);
                    self.continuation_open = !paragraph_end;
                    self.pending_space =
                        text.chars().next_back().map(is_html_space).unwrap_or(false);
                    return Ok(());
                }
            }
            let mut owned = heapless::String::<BLOCK_LEN>::new();
            append_owned_text(&mut owned, text);
            let result = flush_owned_block(&mut owned, role, style, align, self.output);
            self.continuation_open = !paragraph_end;
            self.pending_space = text.chars().next_back().map(is_html_space).unwrap_or(false);
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
    /// The open block holds only a list marker (`1. `): a `<p>` inside the
    /// `<li>` continues it instead of stranding the number on its own line.
    list_marker_pending: bool,
    table_align_stack: [TextAlign; 4],
    table_depth: usize,
    /// Inside an `<svg>` whose image the sink wants and has not had yet.
    svg_image_pending: bool,
    /// The open block's start tag floats it left: an image in it goes to
    /// [`XhtmlBlockSink::push_floated_image`].
    float_left_block: bool,
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
            list_marker_pending: false,
            table_align_stack: [TextAlign::Justify; 4],
            table_depth: 0,
            svg_image_pending: false,
            float_left_block: false,
        }
    }

    pub fn handle_start(
        &mut self,
        tag: &str,
        css: Option<&CssRules>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        let self_closing = tag.trim_end().ends_with('/');
        if self.skip_depth > 0 {
            if self.svg_image_pending && tag_name_is(tag, "image") {
                if let Some(href) = svg_image_href(tag) {
                    self.svg_image_pending = false;
                    self.flush(sink)?;
                    sink.push_svg_image(href)?;
                }
            }
            // Count nested elements of the skipped name so the skip ends at
            // the matching close tag, not the first inner one.
            if !self_closing && self.skip_tag_matches(tag) {
                self.skip_depth = self.skip_depth.saturating_add(1);
            }
            return Ok(());
        }
        self.handle_start_tag(tag, css, sink)?;
        // `<i/>`, `<h2/>`, `<ol/>`: an XHTML self-closed element opens and
        // closes at once. Without the matching end its style or role would
        // leak over the rest of the chapter.
        if self_closing
            && !tag_is_void(tag.trim_end().trim_end_matches('/'))
            && !tag_starts_skip(tag)
        {
            if let Some(name) = tag_local_name(tag) {
                self.handle_end(name, sink)?;
            }
        }
        Ok(())
    }

    fn handle_start_tag(
        &mut self,
        tag: &str,
        css: Option<&CssRules>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        if tag_name_is(tag, "body") {
            self.in_body = true;
            return Ok(());
        }
        if self.skip_depth == 0 && tag_starts_skip(tag) {
            if !tag_is_void(tag) {
                self.skip_tag = tag_local_name(tag).map(bounded_skip_name);
                self.skip_depth = self.skip_depth.saturating_add(1);
                self.svg_image_pending =
                    self.in_body && tag_name_is(tag, "svg") && sink.wants_svg_images();
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
        } else if tag_name_is(tag, "hr") {
            // A thematic break is a scene break; render it the way the
            // reader already normalizes typed `* * *` separators.
            self.flush(sink)?;
            sink.push_block(
                "* * *",
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Center,
                true,
            )?;
        } else if tag_name_is(tag, "img") {
            let (src, alt) = (attr_value(tag, "src").unwrap_or(""), attr_value(tag, "alt"));
            if sink.wants_inline_images() {
                self.push_inline_image(src, alt, sink)?;
            } else {
                self.flush(sink)?;
                sink.push_image(src, alt)?;
            }
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
        } else if tag_starts_block(tag) || (tag_is_figure(tag) && sink.wants_inline_images()) {
            if !self.list_marker_pending || tag_name_is(tag, "li") {
                self.flush(sink)?;
            }
            // Only a sink that keeps images in their lines asks.
            self.float_left_block = sink.wants_inline_images() && tag_floats_left(tag);
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
                self.list_marker_pending = true;
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
                self.svg_image_pending = false;
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
        } else if tag_ends_block(tag) || (tag_is_figure(tag) && sink.wants_inline_images()) {
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
        if self.list_marker_pending && !is_html_blank(text) {
            self.list_marker_pending = false;
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
        self.list_marker_pending = false;
        self.float_left_block = false;
        flush_sink_block(
            &mut self.block,
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            sink,
        )
    }

    /// An image that joins its paragraph: the open run ends, and the image
    /// goes to the sink. Out of line, so a book whose sink keeps images out
    /// of its lines parses in the frames it always did.
    #[inline(never)]
    fn push_inline_image(
        &mut self,
        src: &str,
        alt: Option<&str>,
        sink: &mut impl XhtmlBlockSink,
    ) -> Result<(), XhtmlError> {
        self.end_run_before_image(sink)?;
        if self.float_left_block {
            sink.push_floated_image(src, alt)
        } else {
            sink.push_image(src, alt)
        }
    }

    /// Ends the open run, not its paragraph, ahead of an image that joins
    /// the paragraph. A run of nothing but spaces still goes to the sink as
    /// one space, so the space stays before the image rather than moving
    /// after it, as it would between two styled runs.
    fn end_run_before_image(&mut self, sink: &mut impl XhtmlBlockSink) -> Result<(), XhtmlError> {
        if !is_html_blank(self.block.as_str()) {
            return self.flush_continue(sink);
        }
        if self.block.is_empty() {
            return Ok(());
        }
        self.block.clear();
        sink.push_block(
            " ",
            self.role,
            style_for(self.bold_depth, self.italic_depth),
            self.align,
            false,
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
    let mut in_landmarks = false;
    let mut bodymatter_seen = false;
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
            Token::Start(tag) if tag_name_is(tag, "nav") => {
                in_nav = nav_start_is_toc(tag);
                in_landmarks = attr_word_eq(tag, "epub:type", "landmarks");
            }
            Token::End(tag) if tag_name_is(tag, "nav") => {
                in_nav = false;
                in_landmarks = false;
            }
            Token::Start(tag)
                if in_landmarks
                    && !bodymatter_seen
                    && tag_name_is(tag, "a")
                    && attr_word_eq(tag, "epub:type", "bodymatter") =>
            {
                if let Some(target) = attr_value(tag, "href").filter(|href| !href.is_empty()) {
                    sink.push_bodymatter(target);
                    bodymatter_seen = true;
                }
            }
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
    /// Inside `<!-- ... -->`; counts the trailing run of `-` so `-->`
    /// closes it. Quotes and `>` inside a comment mean nothing.
    Comment(u8),
    /// Inside `<![CDATA[ ... ]]>`; content is character data. Counts the
    /// trailing run of `]` that may be the start of the terminator.
    CData(u8),
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
/// input. Comments run to `-->` regardless of quotes or `>` inside them,
/// CDATA sections emit their content as text, and processing instructions
/// and doctypes are skipped.
///
/// Text accumulates as raw bytes: a forced flush when the buffer fills
/// holds back incomplete UTF-8 sequences and incomplete entities, so the
/// emitted `&str` chunks are always valid UTF-8 and never split an
/// entity, regardless of how the input is chunked.
pub struct StreamingXmlTokenizer {
    state: TokState,
    /// Raw tag bytes. Kept as bytes (not chars) so multi-byte UTF-8 in
    /// attribute values survives; decoded once when the tag closes.
    tag_buf: heapless::Vec<u8, STREAM_TAG_BUF_BYTES>,
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
            tag_buf: heapless::Vec::new(),
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
        if let TokState::CData(brackets) = self.state {
            for _ in 0..brackets {
                self.push_text_byte(b']', emit)?;
            }
            self.state = TokState::Text;
        }
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
                    // An overlong tag still emits its (truncated) prefix: the
                    // name and leading attributes are what block structure,
                    // skip and style decisions key on. Dropping it whole would
                    // let a hidden element's content leak into the page.
                    let valid = match core::str::from_utf8(&self.tag_buf) {
                        Ok(tag) => tag,
                        Err(error) => {
                            core::str::from_utf8(&self.tag_buf[..error.valid_up_to()]).unwrap_or("")
                        }
                    };
                    let tag = valid.trim();
                    if !(tag.starts_with('!') || tag.starts_with('?')) {
                        if let Some(name) = tag.strip_prefix('/') {
                            emit(TokEvent::EndTag(name.trim()))?;
                        } else if !tag.is_empty() {
                            emit(TokEvent::StartTag(tag))?;
                        }
                    }
                    self.tag_buf.clear();
                    self.tag_overflow = false;
                    self.state = TokState::Text;
                } else if byte == b'"' || byte == b'\'' {
                    // Quotes only delimit attribute values inside an element
                    // tag; in a declaration (`<!DOCTYPE`, `<!-- `) they are
                    // literal and must not hide the closing `>`.
                    self.push_tag_byte(byte);
                    if self.tag_buf.first() != Some(&b'!') {
                        self.state = TokState::TagQuoted(byte);
                    }
                } else {
                    self.push_tag_byte(byte);
                    if self.tag_buf.as_slice() == b"!--" {
                        self.tag_buf.clear();
                        self.state = TokState::Comment(0);
                    } else if self.tag_buf.as_slice() == b"![CDATA[" {
                        self.tag_buf.clear();
                        self.state = TokState::CData(0);
                    }
                }
            }
            TokState::TagQuoted(quote) => {
                self.push_tag_byte(byte);
                if byte == quote {
                    self.state = TokState::Tag;
                }
            }
            TokState::Comment(dashes) => {
                self.state = match byte {
                    b'-' => TokState::Comment(dashes.saturating_add(1)),
                    b'>' if dashes >= 2 => TokState::Text,
                    _ => TokState::Comment(0),
                };
            }
            TokState::CData(brackets) => match byte {
                b']' => self.state = TokState::CData(brackets.saturating_add(1)),
                b'>' if brackets >= 2 => {
                    for _ in 2..brackets {
                        self.push_text_byte(b']', emit)?;
                    }
                    self.state = TokState::Text;
                }
                _ => {
                    for _ in 0..brackets {
                        self.push_text_byte(b']', emit)?;
                    }
                    self.state = TokState::CData(0);
                    // CDATA content is literal: an `&` here is not an
                    // entity, so escape it before the shared text path
                    // decodes entities.
                    if byte == b'&' {
                        for &escaped in b"&amp;" {
                            self.push_text_byte(escaped, emit)?;
                        }
                    } else {
                        self.push_text_byte(byte, emit)?;
                    }
                }
            },
        }
        Ok(())
    }

    fn push_tag_byte(&mut self, byte: u8) {
        if self.tag_overflow {
            return;
        }
        if self.tag_buf.push(byte).is_err() {
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
    in_landmarks: bool,
    bodymatter_seen: bool,
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
            in_landmarks: false,
            bodymatter_seen: false,
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
                self.in_landmarks = attr_word_eq(tag, "epub:type", "landmarks");
            }
            TokEvent::EndTag(tag) if tag_name_is(tag, "nav") => {
                self.in_nav = false;
                self.in_landmarks = false;
            }
            TokEvent::StartTag(tag)
                if self.in_landmarks
                    && !self.bodymatter_seen
                    && tag_name_is(tag, "a")
                    && attr_word_eq(tag, "epub:type", "bodymatter") =>
            {
                if let Some(href) = attr_value(tag, "href").filter(|href| !href.is_empty()) {
                    sink.push_bodymatter(href);
                    self.bodymatter_seen = true;
                }
            }
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

/// Elements whose whole subtree contributes no reading text.
fn tag_starts_skip(tag: &str) -> bool {
    tag_name_is(tag, "head")
        || tag_name_is(tag, "style")
        || tag_name_is(tag, "script")
        || tag_name_is(tag, "svg")
        || tag_name_is(tag, "nav")
        || tag_is_hidden(tag)
        || tag_is_pagebreak(tag)
}

/// Where an svg `<image>` points: `href` (SVG 2) or `xlink:href` (SVG 1.1,
/// what EPUB 2 covers use). An empty value counts as none.
fn svg_image_href(tag: &str) -> Option<&str> {
    attr_value(tag, "href")
        .or_else(|| attr_value(tag, "xlink:href"))
        .filter(|href| !href.trim().is_empty())
}

fn tag_is_hidden(tag: &str) -> bool {
    tag_contains_style_value(tag, "display", "none")
        || tag_contains_style_value(tag, "visibility", "hidden")
        || tag_has_attr(tag, "hidden")
        || attr_value(tag, "aria-hidden")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Whether the start tag carries an attribute named `name` (boolean or
/// valued). Walks attribute names properly, so a word inside another
/// attribute's quoted value (`title="the hidden door"`) never matches.
fn tag_has_attr(tag: &str, name: &str) -> bool {
    let bytes = tag.as_bytes();
    // Skip the element name.
    let mut at = bytes
        .iter()
        .position(|byte| byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        let start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && bytes[at] != b'='
            && bytes[at] != b'/'
        {
            at += 1;
        }
        if at > start && tag[start..at].eq_ignore_ascii_case(name) {
            return true;
        }
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at < bytes.len() && bytes[at] == b'=' {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&quote) if quote == b'"' || quote == b'\'' => {
                    at += 1;
                    while at < bytes.len() && bytes[at] != quote {
                        at += 1;
                    }
                    at += 1;
                }
                _ => {
                    while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                        at += 1;
                    }
                }
            }
        } else if at == start {
            at += 1;
        }
    }
    false
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

/// `<figure>` and `<figcaption>`: blocks for a sink that keeps images in
/// their lines, so a figure's caption is never its image's paragraph text.
/// A sink that does not has always seen them as inline, and still does.
fn tag_is_figure(tag: &str) -> bool {
    tag_name_is(tag, "figure") || tag_name_is(tag, "figcaption")
}

/// A block start tag that floats its block left: Project Gutenberg's
/// `figleft`, or an inline `float: left`.
fn tag_floats_left(tag: &str) -> bool {
    tag_contains_class_word(tag, "figleft") || tag_contains_style_value(tag, "float", "left")
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

/// HTML collapses only ASCII whitespace. Other spaces (notably U+00A0
/// from `&nbsp;`) are ordinary characters: they never collapse, never
/// trim, and never break a line.
fn is_html_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

fn is_html_blank(text: &str) -> bool {
    text.chars().all(is_html_space)
}

fn append_owned_text<const N: usize>(out: &mut heapless::String<N>, text: &str) {
    let mut previous_space = out
        .as_str()
        .chars()
        .last()
        .map(is_html_space)
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
        if is_html_space(ch) {
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
        .map(is_html_space)
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
        let should_push_space = is_html_space(ch) && !previous_space;
        let should_push_char = !is_html_space(ch);
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
    let Some(split) = block.as_str().trim_end().rfind(is_html_space) else {
        return flush_sink_block(block, role, style, align, sink);
    };
    let carry_start = block.as_str()[split..]
        .char_indices()
        .find(|(_, ch)| !is_html_space(*ch))
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
    if is_html_blank(block.as_str()) {
        // Inter-word space between two styled runs (`<i>a</i> <i>b</i>`):
        // keep a single space as the next run's leading text instead of
        // dropping it, so the words don't glue together.
        let had_space = !block.is_empty();
        block.clear();
        if had_space {
            let _ = block.push(' ');
        }
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
        .map(|ch| !is_html_space(ch))
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
    if is_html_blank(block.as_str()) {
        block.clear();
        return sink.push_block("", role, style, align, true);
    }
    let result = sink.push_block(block.as_str(), role, style, align, true);
    block.clear();
    result
}

/// Decode the HTML entity at the start of `input` (`&amp;`, `&#x2014;`, ...).
/// Shared by firmware cache building and host preview so EPUB text renders
/// identically on both sides. Named entities cover the HTML 4 set (what the
/// XHTML 1.1 DTDs that EPUB 2 books reference define) plus XML's `&apos;`.
pub fn decode_html_entity(input: &str) -> Option<char> {
    if let Some(decoded) = decode_numeric_entity(input) {
        return Some(decoded);
    }
    let rest = input.strip_prefix('&')?;
    let end = rest
        .bytes()
        .take(NAMED_ENTITY_MAX_LEN + 1)
        .position(|byte| byte == b';')?;
    let mut key = [0u8; NAMED_ENTITY_MAX_LEN];
    key[..end].copy_from_slice(&rest.as_bytes()[..end]);
    NAMED_ENTITIES
        .binary_search_by(|(candidate, _)| candidate.cmp(&key))
        .ok()
        .map(|index| NAMED_ENTITIES[index].1)
}

const NAMED_ENTITY_MAX_LEN: usize = 8;

/// HTML 4 named character references, NUL-padded and sorted for binary
/// search. Names are inline byte arrays, not separate string literals: tiny
/// literals land in the merged constant pool, which this chip's linker
/// script keeps in RAM.
#[rustfmt::skip]
const NAMED_ENTITIES: &[([u8; NAMED_ENTITY_MAX_LEN], char)] = &[
    ([b'A', b'E', b'l', b'i', b'g', 0, 0, 0], '\u{00C6}'),
    ([b'A', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00C1}'),
    ([b'A', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00C2}'),
    ([b'A', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00C0}'),
    ([b'A', b'l', b'p', b'h', b'a', 0, 0, 0], '\u{0391}'),
    ([b'A', b'r', b'i', b'n', b'g', 0, 0, 0], '\u{00C5}'),
    ([b'A', b't', b'i', b'l', b'd', b'e', 0, 0], '\u{00C3}'),
    ([b'A', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00C4}'),
    ([b'B', b'e', b't', b'a', 0, 0, 0, 0], '\u{0392}'),
    ([b'C', b'c', b'e', b'd', b'i', b'l', 0, 0], '\u{00C7}'),
    ([b'C', b'h', b'i', 0, 0, 0, 0, 0], '\u{03A7}'),
    ([b'D', b'a', b'g', b'g', b'e', b'r', 0, 0], '\u{2021}'),
    ([b'D', b'e', b'l', b't', b'a', 0, 0, 0], '\u{0394}'),
    ([b'E', b'T', b'H', 0, 0, 0, 0, 0], '\u{00D0}'),
    ([b'E', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00C9}'),
    ([b'E', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00CA}'),
    ([b'E', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00C8}'),
    ([b'E', b'p', b's', b'i', b'l', b'o', b'n', 0], '\u{0395}'),
    ([b'E', b't', b'a', 0, 0, 0, 0, 0], '\u{0397}'),
    ([b'E', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00CB}'),
    ([b'G', b'a', b'm', b'm', b'a', 0, 0, 0], '\u{0393}'),
    ([b'I', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00CD}'),
    ([b'I', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00CE}'),
    ([b'I', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00CC}'),
    ([b'I', b'o', b't', b'a', 0, 0, 0, 0], '\u{0399}'),
    ([b'I', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00CF}'),
    ([b'K', b'a', b'p', b'p', b'a', 0, 0, 0], '\u{039A}'),
    ([b'L', b'a', b'm', b'b', b'd', b'a', 0, 0], '\u{039B}'),
    ([b'M', b'u', 0, 0, 0, 0, 0, 0], '\u{039C}'),
    ([b'N', b't', b'i', b'l', b'd', b'e', 0, 0], '\u{00D1}'),
    ([b'N', b'u', 0, 0, 0, 0, 0, 0], '\u{039D}'),
    ([b'O', b'E', b'l', b'i', b'g', 0, 0, 0], '\u{0152}'),
    ([b'O', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00D3}'),
    ([b'O', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00D4}'),
    ([b'O', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00D2}'),
    ([b'O', b'm', b'e', b'g', b'a', 0, 0, 0], '\u{03A9}'),
    ([b'O', b'm', b'i', b'c', b'r', b'o', b'n', 0], '\u{039F}'),
    ([b'O', b's', b'l', b'a', b's', b'h', 0, 0], '\u{00D8}'),
    ([b'O', b't', b'i', b'l', b'd', b'e', 0, 0], '\u{00D5}'),
    ([b'O', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00D6}'),
    ([b'P', b'h', b'i', 0, 0, 0, 0, 0], '\u{03A6}'),
    ([b'P', b'i', 0, 0, 0, 0, 0, 0], '\u{03A0}'),
    ([b'P', b'r', b'i', b'm', b'e', 0, 0, 0], '\u{2033}'),
    ([b'P', b's', b'i', 0, 0, 0, 0, 0], '\u{03A8}'),
    ([b'R', b'h', b'o', 0, 0, 0, 0, 0], '\u{03A1}'),
    ([b'S', b'c', b'a', b'r', b'o', b'n', 0, 0], '\u{0160}'),
    ([b'S', b'i', b'g', b'm', b'a', 0, 0, 0], '\u{03A3}'),
    ([b'T', b'H', b'O', b'R', b'N', 0, 0, 0], '\u{00DE}'),
    ([b'T', b'a', b'u', 0, 0, 0, 0, 0], '\u{03A4}'),
    ([b'T', b'h', b'e', b't', b'a', 0, 0, 0], '\u{0398}'),
    ([b'U', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00DA}'),
    ([b'U', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00DB}'),
    ([b'U', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00D9}'),
    ([b'U', b'p', b's', b'i', b'l', b'o', b'n', 0], '\u{03A5}'),
    ([b'U', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00DC}'),
    ([b'X', b'i', 0, 0, 0, 0, 0, 0], '\u{039E}'),
    ([b'Y', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00DD}'),
    ([b'Y', b'u', b'm', b'l', 0, 0, 0, 0], '\u{0178}'),
    ([b'Z', b'e', b't', b'a', 0, 0, 0, 0], '\u{0396}'),
    ([b'a', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00E1}'),
    ([b'a', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00E2}'),
    ([b'a', b'c', b'u', b't', b'e', 0, 0, 0], '\u{00B4}'),
    ([b'a', b'e', b'l', b'i', b'g', 0, 0, 0], '\u{00E6}'),
    ([b'a', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00E0}'),
    ([b'a', b'l', b'e', b'f', b's', b'y', b'm', 0], '\u{2135}'),
    ([b'a', b'l', b'p', b'h', b'a', 0, 0, 0], '\u{03B1}'),
    ([b'a', b'm', b'p', 0, 0, 0, 0, 0], '\u{0026}'),
    ([b'a', b'n', b'd', 0, 0, 0, 0, 0], '\u{2227}'),
    ([b'a', b'n', b'g', 0, 0, 0, 0, 0], '\u{2220}'),
    ([b'a', b'p', b'o', b's', 0, 0, 0, 0], '\u{0027}'),
    ([b'a', b'r', b'i', b'n', b'g', 0, 0, 0], '\u{00E5}'),
    ([b'a', b's', b'y', b'm', b'p', 0, 0, 0], '\u{2248}'),
    ([b'a', b't', b'i', b'l', b'd', b'e', 0, 0], '\u{00E3}'),
    ([b'a', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00E4}'),
    ([b'b', b'd', b'q', b'u', b'o', 0, 0, 0], '\u{201E}'),
    ([b'b', b'e', b't', b'a', 0, 0, 0, 0], '\u{03B2}'),
    ([b'b', b'r', b'v', b'b', b'a', b'r', 0, 0], '\u{00A6}'),
    ([b'b', b'u', b'l', b'l', 0, 0, 0, 0], '\u{2022}'),
    ([b'c', b'a', b'p', 0, 0, 0, 0, 0], '\u{2229}'),
    ([b'c', b'c', b'e', b'd', b'i', b'l', 0, 0], '\u{00E7}'),
    ([b'c', b'e', b'd', b'i', b'l', 0, 0, 0], '\u{00B8}'),
    ([b'c', b'e', b'n', b't', 0, 0, 0, 0], '\u{00A2}'),
    ([b'c', b'h', b'i', 0, 0, 0, 0, 0], '\u{03C7}'),
    ([b'c', b'i', b'r', b'c', 0, 0, 0, 0], '\u{02C6}'),
    ([b'c', b'l', b'u', b'b', b's', 0, 0, 0], '\u{2663}'),
    ([b'c', b'o', b'n', b'g', 0, 0, 0, 0], '\u{2245}'),
    ([b'c', b'o', b'p', b'y', 0, 0, 0, 0], '\u{00A9}'),
    ([b'c', b'r', b'a', b'r', b'r', 0, 0, 0], '\u{21B5}'),
    ([b'c', b'u', b'p', 0, 0, 0, 0, 0], '\u{222A}'),
    ([b'c', b'u', b'r', b'r', b'e', b'n', 0, 0], '\u{00A4}'),
    ([b'd', b'A', b'r', b'r', 0, 0, 0, 0], '\u{21D3}'),
    ([b'd', b'a', b'g', b'g', b'e', b'r', 0, 0], '\u{2020}'),
    ([b'd', b'a', b'r', b'r', 0, 0, 0, 0], '\u{2193}'),
    ([b'd', b'e', b'g', 0, 0, 0, 0, 0], '\u{00B0}'),
    ([b'd', b'e', b'l', b't', b'a', 0, 0, 0], '\u{03B4}'),
    ([b'd', b'i', b'a', b'm', b's', 0, 0, 0], '\u{2666}'),
    ([b'd', b'i', b'v', b'i', b'd', b'e', 0, 0], '\u{00F7}'),
    ([b'e', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00E9}'),
    ([b'e', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00EA}'),
    ([b'e', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00E8}'),
    ([b'e', b'm', b'p', b't', b'y', 0, 0, 0], '\u{2205}'),
    ([b'e', b'm', b's', b'p', 0, 0, 0, 0], '\u{2003}'),
    ([b'e', b'n', b's', b'p', 0, 0, 0, 0], '\u{2002}'),
    ([b'e', b'p', b's', b'i', b'l', b'o', b'n', 0], '\u{03B5}'),
    ([b'e', b'q', b'u', b'i', b'v', 0, 0, 0], '\u{2261}'),
    ([b'e', b't', b'a', 0, 0, 0, 0, 0], '\u{03B7}'),
    ([b'e', b't', b'h', 0, 0, 0, 0, 0], '\u{00F0}'),
    ([b'e', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00EB}'),
    ([b'e', b'u', b'r', b'o', 0, 0, 0, 0], '\u{20AC}'),
    ([b'e', b'x', b'i', b's', b't', 0, 0, 0], '\u{2203}'),
    ([b'f', b'n', b'o', b'f', 0, 0, 0, 0], '\u{0192}'),
    ([b'f', b'o', b'r', b'a', b'l', b'l', 0, 0], '\u{2200}'),
    ([b'f', b'r', b'a', b'c', b'1', b'2', 0, 0], '\u{00BD}'),
    ([b'f', b'r', b'a', b'c', b'1', b'4', 0, 0], '\u{00BC}'),
    ([b'f', b'r', b'a', b'c', b'3', b'4', 0, 0], '\u{00BE}'),
    ([b'f', b'r', b'a', b's', b'l', 0, 0, 0], '\u{2044}'),
    ([b'g', b'a', b'm', b'm', b'a', 0, 0, 0], '\u{03B3}'),
    ([b'g', b'e', 0, 0, 0, 0, 0, 0], '\u{2265}'),
    ([b'g', b't', 0, 0, 0, 0, 0, 0], '\u{003E}'),
    ([b'h', b'A', b'r', b'r', 0, 0, 0, 0], '\u{21D4}'),
    ([b'h', b'a', b'r', b'r', 0, 0, 0, 0], '\u{2194}'),
    ([b'h', b'e', b'a', b'r', b't', b's', 0, 0], '\u{2665}'),
    ([b'h', b'e', b'l', b'l', b'i', b'p', 0, 0], '\u{2026}'),
    ([b'i', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00ED}'),
    ([b'i', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00EE}'),
    ([b'i', b'e', b'x', b'c', b'l', 0, 0, 0], '\u{00A1}'),
    ([b'i', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00EC}'),
    ([b'i', b'm', b'a', b'g', b'e', 0, 0, 0], '\u{2111}'),
    ([b'i', b'n', b'f', b'i', b'n', 0, 0, 0], '\u{221E}'),
    ([b'i', b'n', b't', 0, 0, 0, 0, 0], '\u{222B}'),
    ([b'i', b'o', b't', b'a', 0, 0, 0, 0], '\u{03B9}'),
    ([b'i', b'q', b'u', b'e', b's', b't', 0, 0], '\u{00BF}'),
    ([b'i', b's', b'i', b'n', 0, 0, 0, 0], '\u{2208}'),
    ([b'i', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00EF}'),
    ([b'k', b'a', b'p', b'p', b'a', 0, 0, 0], '\u{03BA}'),
    ([b'l', b'A', b'r', b'r', 0, 0, 0, 0], '\u{21D0}'),
    ([b'l', b'a', b'm', b'b', b'd', b'a', 0, 0], '\u{03BB}'),
    ([b'l', b'a', b'n', b'g', 0, 0, 0, 0], '\u{2329}'),
    ([b'l', b'a', b'q', b'u', b'o', 0, 0, 0], '\u{00AB}'),
    ([b'l', b'a', b'r', b'r', 0, 0, 0, 0], '\u{2190}'),
    ([b'l', b'c', b'e', b'i', b'l', 0, 0, 0], '\u{2308}'),
    ([b'l', b'd', b'q', b'u', b'o', 0, 0, 0], '\u{201C}'),
    ([b'l', b'e', 0, 0, 0, 0, 0, 0], '\u{2264}'),
    ([b'l', b'f', b'l', b'o', b'o', b'r', 0, 0], '\u{230A}'),
    ([b'l', b'o', b'w', b'a', b's', b't', 0, 0], '\u{2217}'),
    ([b'l', b'o', b'z', 0, 0, 0, 0, 0], '\u{25CA}'),
    ([b'l', b'r', b'm', 0, 0, 0, 0, 0], '\u{200E}'),
    ([b'l', b's', b'a', b'q', b'u', b'o', 0, 0], '\u{2039}'),
    ([b'l', b's', b'q', b'u', b'o', 0, 0, 0], '\u{2018}'),
    ([b'l', b't', 0, 0, 0, 0, 0, 0], '\u{003C}'),
    ([b'm', b'a', b'c', b'r', 0, 0, 0, 0], '\u{00AF}'),
    ([b'm', b'd', b'a', b's', b'h', 0, 0, 0], '\u{2014}'),
    ([b'm', b'i', b'c', b'r', b'o', 0, 0, 0], '\u{00B5}'),
    ([b'm', b'i', b'd', b'd', b'o', b't', 0, 0], '\u{00B7}'),
    ([b'm', b'i', b'n', b'u', b's', 0, 0, 0], '\u{2212}'),
    ([b'm', b'u', 0, 0, 0, 0, 0, 0], '\u{03BC}'),
    ([b'n', b'a', b'b', b'l', b'a', 0, 0, 0], '\u{2207}'),
    ([b'n', b'b', b's', b'p', 0, 0, 0, 0], '\u{00A0}'),
    ([b'n', b'd', b'a', b's', b'h', 0, 0, 0], '\u{2013}'),
    ([b'n', b'e', 0, 0, 0, 0, 0, 0], '\u{2260}'),
    ([b'n', b'i', 0, 0, 0, 0, 0, 0], '\u{220B}'),
    ([b'n', b'o', b't', 0, 0, 0, 0, 0], '\u{00AC}'),
    ([b'n', b'o', b't', b'i', b'n', 0, 0, 0], '\u{2209}'),
    ([b'n', b's', b'u', b'b', 0, 0, 0, 0], '\u{2284}'),
    ([b'n', b't', b'i', b'l', b'd', b'e', 0, 0], '\u{00F1}'),
    ([b'n', b'u', 0, 0, 0, 0, 0, 0], '\u{03BD}'),
    ([b'o', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00F3}'),
    ([b'o', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00F4}'),
    ([b'o', b'e', b'l', b'i', b'g', 0, 0, 0], '\u{0153}'),
    ([b'o', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00F2}'),
    ([b'o', b'l', b'i', b'n', b'e', 0, 0, 0], '\u{203E}'),
    ([b'o', b'm', b'e', b'g', b'a', 0, 0, 0], '\u{03C9}'),
    ([b'o', b'm', b'i', b'c', b'r', b'o', b'n', 0], '\u{03BF}'),
    ([b'o', b'p', b'l', b'u', b's', 0, 0, 0], '\u{2295}'),
    ([b'o', b'r', 0, 0, 0, 0, 0, 0], '\u{2228}'),
    ([b'o', b'r', b'd', b'f', 0, 0, 0, 0], '\u{00AA}'),
    ([b'o', b'r', b'd', b'm', 0, 0, 0, 0], '\u{00BA}'),
    ([b'o', b's', b'l', b'a', b's', b'h', 0, 0], '\u{00F8}'),
    ([b'o', b't', b'i', b'l', b'd', b'e', 0, 0], '\u{00F5}'),
    ([b'o', b't', b'i', b'm', b'e', b's', 0, 0], '\u{2297}'),
    ([b'o', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00F6}'),
    ([b'p', b'a', b'r', b'a', 0, 0, 0, 0], '\u{00B6}'),
    ([b'p', b'a', b'r', b't', 0, 0, 0, 0], '\u{2202}'),
    ([b'p', b'e', b'r', b'm', b'i', b'l', 0, 0], '\u{2030}'),
    ([b'p', b'e', b'r', b'p', 0, 0, 0, 0], '\u{22A5}'),
    ([b'p', b'h', b'i', 0, 0, 0, 0, 0], '\u{03C6}'),
    ([b'p', b'i', 0, 0, 0, 0, 0, 0], '\u{03C0}'),
    ([b'p', b'i', b'v', 0, 0, 0, 0, 0], '\u{03D6}'),
    ([b'p', b'l', b'u', b's', b'm', b'n', 0, 0], '\u{00B1}'),
    ([b'p', b'o', b'u', b'n', b'd', 0, 0, 0], '\u{00A3}'),
    ([b'p', b'r', b'i', b'm', b'e', 0, 0, 0], '\u{2032}'),
    ([b'p', b'r', b'o', b'd', 0, 0, 0, 0], '\u{220F}'),
    ([b'p', b'r', b'o', b'p', 0, 0, 0, 0], '\u{221D}'),
    ([b'p', b's', b'i', 0, 0, 0, 0, 0], '\u{03C8}'),
    ([b'q', b'u', b'o', b't', 0, 0, 0, 0], '\u{0022}'),
    ([b'r', b'A', b'r', b'r', 0, 0, 0, 0], '\u{21D2}'),
    ([b'r', b'a', b'd', b'i', b'c', 0, 0, 0], '\u{221A}'),
    ([b'r', b'a', b'n', b'g', 0, 0, 0, 0], '\u{232A}'),
    ([b'r', b'a', b'q', b'u', b'o', 0, 0, 0], '\u{00BB}'),
    ([b'r', b'a', b'r', b'r', 0, 0, 0, 0], '\u{2192}'),
    ([b'r', b'c', b'e', b'i', b'l', 0, 0, 0], '\u{2309}'),
    ([b'r', b'd', b'q', b'u', b'o', 0, 0, 0], '\u{201D}'),
    ([b'r', b'e', b'a', b'l', 0, 0, 0, 0], '\u{211C}'),
    ([b'r', b'e', b'g', 0, 0, 0, 0, 0], '\u{00AE}'),
    ([b'r', b'f', b'l', b'o', b'o', b'r', 0, 0], '\u{230B}'),
    ([b'r', b'h', b'o', 0, 0, 0, 0, 0], '\u{03C1}'),
    ([b'r', b'l', b'm', 0, 0, 0, 0, 0], '\u{200F}'),
    ([b'r', b's', b'a', b'q', b'u', b'o', 0, 0], '\u{203A}'),
    ([b'r', b's', b'q', b'u', b'o', 0, 0, 0], '\u{2019}'),
    ([b's', b'b', b'q', b'u', b'o', 0, 0, 0], '\u{201A}'),
    ([b's', b'c', b'a', b'r', b'o', b'n', 0, 0], '\u{0161}'),
    ([b's', b'd', b'o', b't', 0, 0, 0, 0], '\u{22C5}'),
    ([b's', b'e', b'c', b't', 0, 0, 0, 0], '\u{00A7}'),
    ([b's', b'h', b'y', 0, 0, 0, 0, 0], '\u{00AD}'),
    ([b's', b'i', b'g', b'm', b'a', 0, 0, 0], '\u{03C3}'),
    ([b's', b'i', b'g', b'm', b'a', b'f', 0, 0], '\u{03C2}'),
    ([b's', b'i', b'm', 0, 0, 0, 0, 0], '\u{223C}'),
    ([b's', b'p', b'a', b'd', b'e', b's', 0, 0], '\u{2660}'),
    ([b's', b'u', b'b', 0, 0, 0, 0, 0], '\u{2282}'),
    ([b's', b'u', b'b', b'e', 0, 0, 0, 0], '\u{2286}'),
    ([b's', b'u', b'm', 0, 0, 0, 0, 0], '\u{2211}'),
    ([b's', b'u', b'p', 0, 0, 0, 0, 0], '\u{2283}'),
    ([b's', b'u', b'p', b'1', 0, 0, 0, 0], '\u{00B9}'),
    ([b's', b'u', b'p', b'2', 0, 0, 0, 0], '\u{00B2}'),
    ([b's', b'u', b'p', b'3', 0, 0, 0, 0], '\u{00B3}'),
    ([b's', b'u', b'p', b'e', 0, 0, 0, 0], '\u{2287}'),
    ([b's', b'z', b'l', b'i', b'g', 0, 0, 0], '\u{00DF}'),
    ([b't', b'a', b'u', 0, 0, 0, 0, 0], '\u{03C4}'),
    ([b't', b'h', b'e', b'r', b'e', b'4', 0, 0], '\u{2234}'),
    ([b't', b'h', b'e', b't', b'a', 0, 0, 0], '\u{03B8}'),
    ([b't', b'h', b'e', b't', b'a', b's', b'y', b'm'], '\u{03D1}'),
    ([b't', b'h', b'i', b'n', b's', b'p', 0, 0], '\u{2009}'),
    ([b't', b'h', b'o', b'r', b'n', 0, 0, 0], '\u{00FE}'),
    ([b't', b'i', b'l', b'd', b'e', 0, 0, 0], '\u{02DC}'),
    ([b't', b'i', b'm', b'e', b's', 0, 0, 0], '\u{00D7}'),
    ([b't', b'r', b'a', b'd', b'e', 0, 0, 0], '\u{2122}'),
    ([b'u', b'A', b'r', b'r', 0, 0, 0, 0], '\u{21D1}'),
    ([b'u', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00FA}'),
    ([b'u', b'a', b'r', b'r', 0, 0, 0, 0], '\u{2191}'),
    ([b'u', b'c', b'i', b'r', b'c', 0, 0, 0], '\u{00FB}'),
    ([b'u', b'g', b'r', b'a', b'v', b'e', 0, 0], '\u{00F9}'),
    ([b'u', b'm', b'l', 0, 0, 0, 0, 0], '\u{00A8}'),
    ([b'u', b'p', b's', b'i', b'h', 0, 0, 0], '\u{03D2}'),
    ([b'u', b'p', b's', b'i', b'l', b'o', b'n', 0], '\u{03C5}'),
    ([b'u', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00FC}'),
    ([b'w', b'e', b'i', b'e', b'r', b'p', 0, 0], '\u{2118}'),
    ([b'x', b'i', 0, 0, 0, 0, 0, 0], '\u{03BE}'),
    ([b'y', b'a', b'c', b'u', b't', b'e', 0, 0], '\u{00FD}'),
    ([b'y', b'e', b'n', 0, 0, 0, 0, 0], '\u{00A5}'),
    ([b'y', b'u', b'm', b'l', 0, 0, 0, 0], '\u{00FF}'),
    ([b'z', b'e', b't', b'a', 0, 0, 0, 0], '\u{03B6}'),
    ([b'z', b'w', b'j', 0, 0, 0, 0, 0], '\u{200D}'),
    ([b'z', b'w', b'n', b'j', 0, 0, 0, 0], '\u{200C}'),
];

/// Decode a numeric character reference (`&#8212;` or `&#x2014;`) at the
/// start of `input`.
pub fn decode_numeric_entity(input: &str) -> Option<char> {
    let rest = input.strip_prefix("&#")?;
    let end = rest.bytes().take(9).position(|byte| byte == b';')?;
    let entity = &rest[..end];
    let value = if let Some(hex) = entity
        .strip_prefix('x')
        .or_else(|| entity.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        entity.parse::<u32>().ok()?
    };
    if value == 0 {
        return None;
    }
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
        .find(|(_, ch)| !is_html_space(*ch))
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

        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(7),
            "/books/flowers.epub",
            1234,
            "OPS/package.opf",
        )
        .expect("opf parses");

        assert_eq!(package.meta.title, "Flowers for Algernon");
        assert_eq!(package.meta.author, "Daniel Keyes");
        assert_eq!(package.meta.cover_status, CoverStatus::Present);
        assert_eq!(package.cover_href, Some("images/cover.jpg"));
        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "text/ch1.xhtml"
        );
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
        // The space between the two runs travels with the second run, so
        // the words never glue together.
        assert_eq!(
            sink.fragments[3],
            (
                " bold".into(),
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
    fn epub3_nav_reports_the_bodymatter_landmark_once() {
        #[derive(Default)]
        struct Sink {
            toc: usize,
            bodymatter: Vec<heapless::String<48>, 2>,
        }
        impl EpubTocSink for Sink {
            fn push_toc(&mut self, _: &str, _: &str, _: u8) -> Result<(), TocError> {
                self.toc += 1;
                Ok(())
            }
            fn push_bodymatter(&mut self, href: &str) {
                let mut stored = heapless::String::new();
                stored.push_str(href).unwrap();
                self.bodymatter.push(stored).unwrap();
            }
        }

        let nav = r#"
            <html><body>
              <nav epub:type="toc"><ol>
                <li><a epub:type="bodymatter" href="toc-link.xhtml">Not a landmark</a></li>
              </ol></nav>
              <nav epub:type="landmarks" id="landmarks"><ol>
                <li><a epub:type="cover" href="cover.xhtml">Cover</a></li>
                <li><a epub:type="bodymatter" href="text/chapter1.xhtml#c1">Start</a></li>
                <li><a epub:type="bodymatter" href="text/later.xhtml">Second</a></li>
              </ol></nav>
            </body></html>
        "#;
        let mut sink = Sink::default();
        parse_epub3_nav_to_sink(nav, &mut sink).expect("nav parses");
        assert_eq!(sink.toc, 1, "landmarks never join the chapter list");
        assert_eq!(sink.bodymatter.len(), 1);
        assert_eq!(sink.bodymatter[0].as_str(), "text/chapter1.xhtml#c1");

        // The streaming parser the firmware uses agrees, at any chunking.
        for chunk in [1usize, 7, 64, nav.len()] {
            let mut sink = Sink::default();
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser = Epub3NavStreamParser::new();
            for part in nav.as_bytes().chunks(chunk) {
                tokenizer
                    .feed(part, &mut |event| parser.handle(event, &mut sink))
                    .unwrap();
            }
            tokenizer
                .finish(&mut |event| parser.handle(event, &mut sink))
                .unwrap();
            assert_eq!(sink.toc, 1);
            assert_eq!(sink.bodymatter.len(), 1, "chunk {chunk}");
            assert_eq!(sink.bodymatter[0].as_str(), "text/chapter1.xhtml#c1");
        }
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
              <metadata><dc:title>The Time Machine</dc:title><dc:creator>H. G. Wells</dc:creator><dc:language>en-GB</dc:language><dc:language>fr</dc:language></metadata>
              <guide><reference type="cover" title="Cover" href="wrap0000.html"/><reference type="text" title="Start" href="text/start.xhtml#p1"/></guide>
              <manifest>
                <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
                <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
                <item id="chap1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
              </manifest>
              <spine toc="ncx"><itemref idref="chap1"/></spine>
              <item id="outside" href="wrong.xhtml" media-type="application/xhtml+xml"/>
            </package>
        "#;

        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(9),
            "/books/time.epub",
            42,
            "OPS/content.opf",
        )
        .expect("opf parses");

        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "text/ch1.xhtml"
        );
        assert_eq!(package.text_reference_href, Some("text/start.xhtml"));
        assert_eq!(package.cover_page_href, Some("wrap0000.html"));
        assert_eq!(package.language, Some("en-GB"));
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

        let mut arena = [0u8; 4096];
        let package = parse_opf(
            opf.as_bytes(),
            &mut arena,
            BookId(11),
            "/books/loose.epub",
            42,
            "OPS/content.opf",
        )
        .expect("opf parses");

        assert_eq!(package.spine.len(), 1);
        assert_eq!(
            package.spine.get(0).unwrap().href.of(package.opf_text),
            "text/ch1.xhtml"
        );
    }

    /// An OPF with `chapters` spine items plus `extra` manifest-only items
    /// (images, CSS) that the reader never needs.
    fn large_opf(chapters: usize, extra: usize) -> std::string::String {
        let mut opf = std::string::String::from(
            "<?xml version=\"1.0\"?><package><metadata>\
             <dc:title>Pride &amp; Prejudice \u{2014} \u{dc}ber</dc:title>\
             <dc:creator>Jane Austen</dc:creator></metadata><manifest>",
        );
        for index in 0..extra {
            opf.push_str(&std::format!(
                "<item id=\"img{index}\" href=\"images/illustration-{index:05}.jpg\" \
                 media-type=\"image/jpeg\"/>\n"
            ));
        }
        for index in 0..chapters {
            opf.push_str(&std::format!(
                "<item id=\"ch{index}\" href=\"text/chapter-{index:04}.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>\n"
            ));
        }
        opf.push_str("</manifest><spine toc=\"ncx\">");
        for index in 0..chapters {
            opf.push_str(&std::format!("<itemref idref=\"ch{index}\"/>"));
        }
        opf.push_str("</spine><guide><reference type=\"text\" href=\"text/chapter-0002.xhtml#x\"/></guide></package>");
        opf
    }

    fn parse_streamed_in_chunks<'a>(
        opf: &str,
        chunk: usize,
        arena: &'a mut [u8],
    ) -> Result<EpubPackage<'a>, EpubError> {
        parse_opf_streamed(
            |feed| {
                for piece in opf.as_bytes().chunks(chunk) {
                    feed(piece)?;
                }
                Ok(())
            },
            arena,
            BookId(1),
            "/books/big.epub",
            0,
            "OPS/content.opf",
        )
    }

    #[test]
    fn opf_far_larger_than_the_arena_keeps_every_spine_item() {
        let opf = large_opf(150, 1200);
        assert!(opf.len() > 100_000, "fixture should dwarf the arena");
        for chunk in [1usize, 37, 4096] {
            let mut arena = [0u8; 16_384];
            let package = parse_streamed_in_chunks(&opf, chunk, &mut arena).expect("parses");
            assert_eq!(package.spine.len(), 150, "chunk {chunk}");
            assert!(!package.spine_truncated);
            assert_eq!(
                package.spine.get(0).unwrap().href.of(package.opf_text),
                "text/chapter-0000.xhtml"
            );
            assert_eq!(
                package.spine.get(149).unwrap().href.of(package.opf_text),
                "text/chapter-0149.xhtml"
            );
            assert_eq!(package.meta.title, "Pride & Prejudice \u{2014} \u{dc}ber");
            assert_eq!(package.meta.author, "Jane Austen");
            assert_eq!(package.text_reference_href, Some("text/chapter-0002.xhtml"));
        }
    }

    #[test]
    fn opf_spine_past_the_arena_is_flagged_truncated() {
        let opf = large_opf(1500, 0);
        let mut arena = [0u8; 16_384];
        let package = parse_streamed_in_chunks(&opf, 512, &mut arena).expect("parses");
        assert!(package.spine.len() > 300 && package.spine.len() < 1500);
        assert!(package.spine_truncated);
        // What was kept is the head of the spine, in order.
        let last = package.spine.len() - 1;
        assert_eq!(
            package.spine.get(last).unwrap().href.of(package.opf_text),
            std::format!("text/chapter-{last:04}.xhtml")
        );
    }

    #[test]
    fn a_spine_of_hundreds_of_items_fits_the_device_arena() {
        // An essay collection: 600 items, each its own XHTML file, with
        // bulky manifest ids and hundreds of images the spine never names.
        let mut opf = std::string::String::from(
            r#"<package><metadata><dc:title>Essays</dc:title></metadata><manifest>"#,
        );
        for index in 0..600 {
            opf.push_str(&std::format!(
                r#"<item id="essay-{index:04}-xhtml-content-document" href="text/essay-{index:04}.xhtml" media-type="application/xhtml+xml"/>"#
            ));
            opf.push_str(&std::format!(
                r#"<item id="figure-{index:04}" href="images/figure-{index:04}.png" media-type="image/png"/>"#
            ));
        }
        opf.push_str("</manifest><spine>");
        for index in (0..600).rev() {
            opf.push_str(&std::format!(
                r#"<itemref idref="essay-{index:04}-xhtml-content-document"/>"#
            ));
        }
        // A dangling itemref is skipped, as before.
        opf.push_str(r#"<itemref idref="missing"/></spine></package>"#);
        let mut arena = [0u8; 20 * 1024];
        let package = parse_streamed_in_chunks(&opf, 777, &mut arena).expect("parses");
        assert!(!package.spine_truncated);
        assert_eq!(package.spine.len(), 600);
        // Spine order, not manifest order.
        let href = |index: usize| package.spine.get(index).unwrap().href.of(package.opf_text);
        assert_eq!(href(0), "text/essay-0599.xhtml");
        assert_eq!(href(599), "text/essay-0000.xhtml");
        assert_eq!(
            package
                .spine
                .get(3)
                .unwrap()
                .media_type
                .of(package.opf_text),
            "application/xhtml+xml"
        );
    }

    #[test]
    fn opf_arena_exhaustion_is_flagged_not_silent() {
        let opf = large_opf(40, 0);
        let mut arena = [0u8; 600];
        let package = parse_streamed_in_chunks(&opf, 64, &mut arena).expect("parses");
        assert!(package.spine.len() < 40);
        assert!(package.spine_truncated);
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
        // `&nbsp;` stays a no-break space rather than collapsing.
        assert_eq!(blocks[0].text, "Visible\u{a0}text & symbols");
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

    type Fragment = (std::string::String, TextRole, FontStyle, TextAlign, bool);

    struct AllFragments(StdVec<Fragment>);

    impl XhtmlBlockSink for AllFragments {
        fn push_block(
            &mut self,
            text: &str,
            role: TextRole,
            style: FontStyle,
            align: TextAlign,
            paragraph_end: bool,
        ) -> Result<(), XhtmlError> {
            self.0
                .push((text.into(), role, style, align, paragraph_end));
            Ok(())
        }
    }

    /// Parse `xhtml` through the whole-document path and the streamed path
    /// at several chunk sizes; every path must agree. Returns the fragments.
    fn parse_both_ways(xhtml: &str) -> StdVec<Fragment> {
        let mut whole = AllFragments(StdVec::new());
        xhtml_blocks_to_sink(xhtml, None, &mut whole).expect("whole parse");
        for chunk_len in [1usize, 3, 7, 4096] {
            let mut streamed = AllFragments(StdVec::new());
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
            assert_eq!(whole.0, streamed.0, "chunk size {chunk_len}");
        }
        whole.0
    }

    /// Join fragments the way the reader does: runs concatenate, paragraph
    /// ends start a new line, runs of spaces collapse.
    fn reading_text(fragments: &[Fragment]) -> std::string::String {
        let mut out = std::string::String::new();
        for (text, _, _, _, paragraph_end) in fragments {
            out.push_str(text);
            if *paragraph_end {
                out.push('\n');
            }
        }
        let mut collapsed = std::string::String::new();
        for line in out.split('\n') {
            let words: StdVec<&str> = line.split(' ').filter(|word| !word.is_empty()).collect();
            if !words.is_empty() {
                collapsed.push_str(&words.join(" "));
                collapsed.push('\n');
            }
        }
        collapsed
    }

    #[test]
    fn comment_with_quote_or_markup_does_not_swallow_or_leak_text() {
        let text = reading_text(&parse_both_ways(
            r#"<body><p>One</p><!-- "an unbalanced quote ” --><p>Two</p>
            <!-- <p>DRAFT</p> --><p>Three</p></body>"#,
        ));
        assert_eq!(text, "One\nTwo\nThree\n");
    }

    #[test]
    fn cdata_section_is_text() {
        let text = reading_text(&parse_both_ways(
            "<body><p><![CDATA[a < b > c]]> done</p></body>",
        ));
        assert_eq!(text, "a < b > c done\n");
    }

    #[test]
    fn quoted_greater_than_inside_attribute_does_not_end_the_tag() {
        let text = reading_text(&parse_both_ways(
            r#"<body><p title="a>b">Text one</p></body>"#,
        ));
        assert_eq!(text, "Text one\n");
    }

    /// Images as books mark them up, for the hook tests below: alt text
    /// with entities and runs of whitespace, no alt, an empty or blank alt,
    /// an image inside a paragraph, and the two `<svg>` wrappers covers use.
    const IMAGE_MARKUP: &str = concat!(
        r#"<body><p>Before</p>"#,
        r#"<img src="images/a%20b.png#frag" alt="Caf&eacute;  &amp;&#10;  tea"/>"#,
        r#"<img src="b.png"/>"#,
        r#"<img src="c.png" alt=""/>"#,
        r#"<img src="d.png" alt="   "/>"#,
        r#"<p>mid <img alt="x" src="e.png"/> tail</p>"#,
        r#"<div><svg xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 9 9">"#,
        r#"<title>Cover</title><image width="9" height="9" xlink:href="cover.png"/>"#,
        r#"<image href="second.png"/><text>skipped</text></svg></div>"#,
        r#"<p>text <svg><g><image/><image href="i.png"></image></g></svg> more</p>"#,
        r#"<p>After</p></body>"#,
    );

    /// What the parser pushed for [`IMAGE_MARKUP`] before images had hooks,
    /// recorded from that parser: a sink that overrides nothing must keep
    /// receiving exactly this. `true` marks an image placeholder (italic,
    /// centred); the rest are body text, justified. Every push ends its
    /// paragraph.
    fn image_markup_placeholders() -> StdVec<Fragment> {
        [
            ("", false),
            ("Before", false),
            ("", false),
            ("Caf\u{e9} & tea", true),
            ("", false),
            ("[Image]", true),
            ("", false),
            ("", true),
            ("", false),
            (" ", true),
            ("", false),
            ("mid ", false),
            ("x", true),
            (" tail", false),
            ("", false),
            ("", false),
            ("", false),
            ("text more", false),
            ("", false),
            ("After", false),
            ("", false),
            ("", false),
        ]
        .into_iter()
        .map(|(text, image)| {
            let (style, align) = if image {
                (FontStyle::Italic, TextAlign::Center)
            } else {
                (FontStyle::Regular, TextAlign::Justify)
            };
            (text.into(), TextRole::Body, style, align, true)
        })
        .collect()
    }

    #[test]
    fn sinks_without_image_hooks_get_the_placeholders_they_always_did() {
        assert_eq!(parse_both_ways(IMAGE_MARKUP), image_markup_placeholders());
    }

    #[test]
    fn image_hooks_get_raw_sources_in_document_order() {
        #[derive(Default)]
        struct Images(StdVec<std::string::String>);
        impl XhtmlBlockSink for Images {
            fn push_block(
                &mut self,
                text: &str,
                _: TextRole,
                _: FontStyle,
                _: TextAlign,
                paragraph_end: bool,
            ) -> Result<(), XhtmlError> {
                if !text.trim().is_empty() {
                    self.0
                        .push(std::format!("text {:?} {paragraph_end}", text.trim()));
                }
                Ok(())
            }
            fn push_image(&mut self, src: &str, alt: Option<&str>) -> Result<(), XhtmlError> {
                self.0.push(std::format!("image {src:?} {alt:?}"));
                Ok(())
            }
            fn push_svg_image(&mut self, href: &str) -> Result<(), XhtmlError> {
                self.0.push(std::format!("svg image {href:?}"));
                Ok(())
            }
            fn wants_svg_images(&self) -> bool {
                true
            }
        }
        let run = |xhtml: &str| {
            let mut whole = Images::default();
            xhtml_blocks_to_sink(xhtml, None, &mut whole).expect("whole parse");
            for chunk_len in [1usize, 3, 7, 4096] {
                let mut streamed = Images::default();
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
                assert_eq!(whole.0, streamed.0, "chunk size {chunk_len}");
            }
            whole.0
        };
        assert_eq!(
            run(IMAGE_MARKUP),
            [
                r#"text "Before" true"#,
                r#"image "images/a%20b.png#frag" Some("Caf&eacute;  &amp;&#10;  tea")"#,
                r#"image "b.png" None"#,
                r#"image "c.png" Some("")"#,
                r#"image "d.png" Some("   ")"#,
                // The open paragraph ends before the image, as it always has.
                r#"text "mid" true"#,
                r#"image "e.png" Some("x")"#,
                r#"text "tail" true"#,
                // Only the first svg image with a reference; its title, the
                // second image and the svg's text stay skipped.
                r#"svg image "cover.png""#,
                r#"text "text" true"#,
                r#"svg image "i.png""#,
                r#"text "more" true"#,
                r#"text "After" true"#,
            ]
        );
        // An svg outside the body, or nested in a skipped element, reports
        // nothing; a sink that does not ask for svg images hears nothing of
        // them (see the placeholder test above).
        assert_eq!(
            run(concat!(
                r#"<html><head><svg><image href="head.png"/></svg></head>"#,
                r#"<body><div style="display:none"><svg><image href="hidden.png"/></svg></div>"#,
                r#"<p>Only text</p></body></html>"#,
            )),
            [r#"text "Only text" true"#]
        );
    }

    /// Everything a sink that keeps images in their paragraphs hears, in
    /// order, for a whole parse and for streamed ones at several chunk sizes
    /// (which must agree).
    fn inline_image_events(xhtml: &str) -> StdVec<std::string::String> {
        #[derive(Default)]
        struct Inline(StdVec<std::string::String>);
        impl XhtmlBlockSink for Inline {
            fn push_block(
                &mut self,
                text: &str,
                _: TextRole,
                style: FontStyle,
                _: TextAlign,
                paragraph_end: bool,
            ) -> Result<(), XhtmlError> {
                self.0
                    .push(std::format!("text {text:?} {style:?} {paragraph_end}"));
                Ok(())
            }
            fn push_image(&mut self, src: &str, alt: Option<&str>) -> Result<(), XhtmlError> {
                self.0.push(std::format!("image {src:?} {alt:?}"));
                Ok(())
            }
            fn push_floated_image(
                &mut self,
                src: &str,
                alt: Option<&str>,
            ) -> Result<(), XhtmlError> {
                self.0.push(std::format!("floated image {src:?} {alt:?}"));
                Ok(())
            }
            fn push_svg_image(&mut self, href: &str) -> Result<(), XhtmlError> {
                self.0.push(std::format!("svg image {href:?}"));
                Ok(())
            }
            fn wants_svg_images(&self) -> bool {
                true
            }
            fn wants_inline_images(&self) -> bool {
                true
            }
        }
        let mut whole = Inline::default();
        xhtml_blocks_to_sink(xhtml, None, &mut whole).expect("whole parse");
        for chunk_len in [1usize, 3, 7, 4096] {
            let mut streamed = Inline::default();
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
            assert_eq!(whole.0, streamed.0, "chunk size {chunk_len}");
        }
        // The empty pushes that end a paragraph, unless one ends nothing
        // but another: drop runs of them to keep the expectations legible.
        let mut events: StdVec<std::string::String> = StdVec::new();
        for event in whole.0 {
            if event == r#"text "" Regular true"#
                && events.last().is_some_and(|last| last.ends_with("true"))
            {
                continue;
            }
            events.push(event);
        }
        events
    }

    #[test]
    fn inline_image_sinks_keep_images_in_their_paragraph() {
        // Mid-sentence: the run before ends without ending the paragraph,
        // with its space, and the text after keeps its own.
        assert_eq!(
            inline_image_events(
                r#"<body><p>Complete the <img alt="PICT" src="t12x.png"/> AEBG.</p></body>"#
            ),
            [
                r#"text "" Regular true"#,
                r#"text "Complete the " Regular false"#,
                r#"image "t12x.png" Some("PICT")"#,
                r#"text " AEBG." Regular true"#,
            ]
        );
        // At the paragraph's start, and at its end.
        assert_eq!(
            inline_image_events(
                r#"<body><p><img src="a.png"/>Start</p><p>End <img src="e.png"/></p></body>"#
            ),
            [
                r#"text "" Regular true"#,
                r#"image "a.png" None"#,
                r#"text "Start" Regular true"#,
                r#"text "End " Regular false"#,
                r#"image "e.png" None"#,
                r#"text "" Regular true"#,
            ]
        );
        // A paragraph of images alone, and a lone space between styled
        // runs and an image, which stays before the image.
        assert_eq!(
            inline_image_events(
                r#"<body><p><img src="x.png"/> <img src="y.png"/></p><p><i>a</i> <img src="z.png"/>b</p></body>"#
            ),
            [
                r#"text "" Regular true"#,
                r#"image "x.png" None"#,
                r#"text " " Regular false"#,
                r#"image "y.png" None"#,
                r#"text "" Regular true"#,
                r#"text "a" Italic false"#,
                r#"text " " Regular false"#,
                r#"image "z.png" None"#,
                r#"text "b" Regular true"#,
            ]
        );
        // svg images still end their paragraph.
        assert_eq!(
            inline_image_events(
                r#"<body><p>text <svg><image href="i.png"/></svg> more</p></body>"#
            ),
            [
                r#"text "" Regular true"#,
                r#"text "text " Regular true"#,
                r#"svg image "i.png""#,
                r#"text " more" Regular true"#,
            ]
        );
    }

    #[test]
    fn a_figure_s_caption_is_a_paragraph_of_its_own() {
        let figure = concat!(
            r#"<body><figure><img alt="A plate" src="plate.png"/>"#,
            r#"<figcaption>The plate, a caption.</figcaption></figure></body>"#,
        );
        assert_eq!(
            inline_image_events(figure),
            [
                r#"text "" Regular true"#,
                r#"image "plate.png" Some("A plate")"#,
                r#"text "" Regular true"#,
                r#"text "The plate, a caption." Regular true"#,
            ]
        );
        // A sink that keeps images out of its lines hears what it always
        // did: the figure is no block to it.
        let fragments = parse_both_ways(figure);
        assert_eq!(reading_text(&fragments), "A plate\nThe plate, a caption.\n");
    }

    #[test]
    fn images_in_left_floated_blocks_say_so() {
        // Rackham's Alice: a drop cap floated in a block of its own, then
        // the paragraph it starts.
        assert_eq!(
            inline_image_events(concat!(
                r#"<body><div class="chapter">CHAPTER III</div>"#,
                r#"<div class="figleft" style="width: 75px;">"#,
                "
",
                r#"<img alt="T" src="t.png" title="" id="img_images_t.png"/>"#,
                "
",
                r#"</div><div class="unindent">HEY were indeed</div></body>"#,
            )),
            [
                r#"text "" Regular true"#,
                r#"text "CHAPTER III" Regular true"#,
                // The line break before the image is a space, and a space
                // at a block's start is the sink's to drop.
                r#"text " " Regular false"#,
                r#"floated image "t.png" Some("T")"#,
                r#"text "" Regular true"#,
                r#"text "HEY were indeed" Regular true"#,
            ]
        );
        // A float in the style says so too; the float ends with its block.
        assert_eq!(
            inline_image_events(concat!(
                r#"<body><p style="float: left"><img alt="A" src="a.png"/></p>"#,
                r#"<p class="figcenter"><img alt="A" src="a.png"/></p></body>"#,
            )),
            [
                r#"text "" Regular true"#,
                r#"floated image "a.png" Some("A")"#,
                r#"text "" Regular true"#,
                r#"image "a.png" Some("A")"#,
                r#"text "" Regular true"#,
            ]
        );
    }

    #[test]
    fn held_alt_text_is_the_placeholder_text() {
        #[derive(Default)]
        struct Placeholder(std::string::String);
        impl XhtmlBlockSink for Placeholder {
            fn push_block(
                &mut self,
                text: &str,
                _: TextRole,
                _: FontStyle,
                _: TextAlign,
                _: bool,
            ) -> Result<(), XhtmlError> {
                self.0 = text.into();
                Ok(())
            }
        }
        let long = "é and &amp; ".repeat(30);
        for alt in [
            None,
            Some(""),
            Some("   "),
            Some("Caf&eacute;  &amp;&#10;  tea"),
            Some(" lead and trail "),
            Some("&bogus; &#x2014;"),
            Some(long.as_str()),
        ] {
            let mut sink = Placeholder::default();
            push_image_placeholder(&mut sink, alt).unwrap();
            let mut out = [0u8; 160];
            let len = image_alt_text(alt, &mut out);
            assert_eq!(
                core::str::from_utf8(&out[..len]).unwrap(),
                sink.0,
                "{alt:?}"
            );
        }
        // A short buffer cuts on a character boundary.
        let mut out = [0u8; 5];
        let len = image_alt_text(Some("abcé"), &mut out);
        assert_eq!(&out[..len], "abcé".as_bytes());
        let mut out = [0u8; 4];
        let len = image_alt_text(Some("abcé"), &mut out);
        assert_eq!(&out[..len], b"abc");
    }

    #[test]
    fn drop_cap_alts_are_a_letter() {
        for alt in ["T", "&quot;C", "\"Y", "'A", " T ", "\u{201C}H", "\u{C9}"] {
            assert!(alt_is_initial(alt), "{alt:?}");
        }
        for alt in [
            "", " ", "TH", "1", "\"1", "Alice", "A cat", "[Image]", "--", "T.",
        ] {
            assert!(!alt_is_initial(alt), "{alt:?}");
        }
    }

    #[test]
    fn non_ascii_attribute_values_survive() {
        let fragments = parse_both_ways(r#"<body><img src="x.png" alt="Café Ægir"/></body>"#);
        assert_eq!(reading_text(&fragments), "Café Ægir\n");
    }

    #[test]
    fn adjacent_styled_runs_keep_the_space_between_them() {
        let text = reading_text(&parse_both_ways(
            "<body><p>see <i>Dune</i> <i>Messiah</i> and <b>A</b> <b>B</b>.</p></body>",
        ));
        assert_eq!(text, "see Dune Messiah and A B.\n");
    }

    #[test]
    fn paragraph_inside_list_item_keeps_its_marker() {
        let text = reading_text(&parse_both_ways(
            "<body><ol><li><p>First</p></li><li>\n  <p>Second</p></li></ol></body>",
        ));
        assert_eq!(text, "1. First\n2. Second\n");
    }

    #[test]
    fn horizontal_rule_becomes_a_scene_break() {
        let fragments = parse_both_ways("<body><p>before</p><hr/><p>after</p></body>");
        assert!(fragments
            .iter()
            .any(|(text, _, _, align, end)| text == "* * *"
                && *align == TextAlign::Center
                && *end));
        assert_eq!(reading_text(&fragments), "before\n* * *\nafter\n");
    }

    #[test]
    fn self_closed_inline_and_heading_tags_do_not_leak_style() {
        let fragments = parse_both_ways("<body><p>a<i/>b</p><h2/><ol/><p>body text</p></body>");
        let body = fragments
            .iter()
            .find(|(text, ..)| text.contains("body text"))
            .expect("body paragraph");
        assert_eq!(body.1, TextRole::Body);
        assert_eq!(body.2, FontStyle::Regular);
        assert_eq!(body.3, TextAlign::Justify);
    }

    #[test]
    fn nested_hidden_content_stays_hidden() {
        let text = reading_text(&parse_both_ways(
            r#"<body><div style="display:none"><div>inner</div>LEAKED</div><p>shown</p></body>"#,
        ));
        assert_eq!(text, "shown\n");
    }

    #[test]
    fn hidden_as_a_word_in_another_attribute_is_not_hidden() {
        let text = reading_text(&parse_both_ways(
            r#"<body><p title="The hidden door">Visible</p><p hidden="">Gone</p><p hidden>Gone</p></body>"#,
        ));
        assert_eq!(text, "Visible\n");
    }

    #[test]
    fn overlong_tag_still_applies_its_leading_attributes() {
        let filler = "x".repeat(1500);
        let text = reading_text(&parse_both_ways(&std::format!(
            r#"<body><div hidden="hidden" data-filler="{filler}">secret</div><p>ok</p></body>"#
        )));
        assert_eq!(text, "ok\n");
    }

    #[test]
    fn named_html_entities_decode() {
        let text = reading_text(&parse_both_ways(
            "<body><p>caf&eacute; &copy; &Aring; &mdash; &bogus; &amp;lt;</p></body>",
        ));
        assert_eq!(text, "café © Å — &bogus; &lt;\n");
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
              <!-- a "quoted" comment with <p>markup</p> -->
              <p>see <i>Dune</i> <i>Messiah</i> &eacute;t&eacute;</p>
              <ol><li><p>item para</p></li></ol>
              <hr/><h2/><p title="the hidden door">kept</p>
              <div style="display:none"><div>x</div>leak</div>
              <img alt="Café Ægir"/>
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

    /// A reader that counts its reads, for the walk tests below.
    struct CountingReader<'a> {
        bytes: &'a [u8],
        reads: usize,
    }

    impl ReadAt for CountingReader<'_> {
        type Error = ();

        fn len(&mut self) -> Result<u32, Self::Error> {
            Ok(self.bytes.len() as u32)
        }

        fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
            self.reads += 1;
            SliceReader { bytes: self.bytes }.read_at(offset, out)
        }
    }

    /// A book's worth of entries: chapters, a stylesheet, a JPEG cover
    /// (which the index does not hold), and `images` PNGs (one with a name
    /// longer than the index pass's 64-byte chunk) after them, as Gutenberg
    /// orders its ZIPs.
    fn book_zip(chapters: usize, images: usize) -> StdVec<u8> {
        let mut names: StdVec<std::string::String> = (0..chapters)
            .map(|index| std::format!("OEBPS/text/chapter-{index:03}.xhtml"))
            .collect();
        names.push("OEBPS/style.css".into());
        // Not an image the index holds.
        names.push("OEBPS/images/cover.svg".into());
        names.extend((0..images).map(|index| {
            if index == 0 {
                std::format!("OEBPS/images/{}-plate.PNG", "long".repeat(20))
            } else if index == 1 {
                "OEBPS/images/photo.jpeg".into()
            } else {
                std::format!("OEBPS/images/fig{index:03}.png")
            }
        }));
        let files: StdVec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), name.as_bytes()))
            .collect();
        stored_zip(&files)
    }

    #[test]
    fn a_walk_visits_every_entry_whether_cached_indexed_or_streamed() {
        let zip = book_zip(40, 3);
        for tail_len in [8192usize, 1024, 200] {
            let mut tail = std::vec![0u8; tail_len];
            let mut stream =
                ZipStream::new(SliceReader { bytes: &zip }, &mut tail).expect("zip opens");
            let cached = tail_len == 8192;
            for buf_len in [46 + 60, 300, 4096] {
                let mut buf = std::vec![0xA5u8; buf_len];
                let mut names = StdVec::new();
                stream
                    .for_each_entry(&mut buf, &mut |entry| {
                        names.push(std::string::String::from(entry.name));
                        let found = stream_entry_matches(&zip, entry);
                        assert!(found, "{} is where the walk says", entry.name);
                        true
                    })
                    .expect("walk");
                // Every entry, except the long image name when its header
                // and name (149 bytes) do not fit a streaming walk's buffer.
                let expected = if cached || buf_len >= 149 { 45 } else { 44 };
                assert_eq!(names.len(), expected, "tail {tail_len} buf {buf_len}");
                assert_eq!(names[0], "OEBPS/text/chapter-000.xhtml");
            }
            let mut stops = 0;
            stream
                .for_each_entry(&mut [0u8; 512], &mut |_| {
                    stops += 1;
                    stops < 5
                })
                .unwrap();
            assert_eq!(stops, 5, "the walk stops when told");
        }
    }

    /// Whether `entry` matches the local header its offset names.
    fn stream_entry_matches(zip: &[u8], entry: &ZipEntry<'_>) -> bool {
        let at = entry.local_header_offset as usize;
        let name_len = u16::from_le_bytes([zip[at + 26], zip[at + 27]]) as usize;
        &zip[at + 30..at + 30 + name_len] == entry.name.as_bytes()
            && u32::from_le_bytes(zip[at + 18..at + 22].try_into().unwrap())
                == entry.compressed_size
    }

    #[test]
    fn image_entries_are_known_from_the_open_unless_the_index_was_cut_short() {
        // Cached: answered from RAM, no read beyond the open's.
        for (images, expected) in [(0, false), (2, true)] {
            let zip = book_zip(10, images);
            let mut tail = [0u8; 4096];
            let reader = CountingReader {
                bytes: &zip,
                reads: 0,
            };
            let mut stream = ZipStream::new(reader, &mut tail).unwrap();
            assert_eq!(stream.has_image_entries(&mut [0u8; 64]), Ok(expected));
            assert_eq!(stream.reader.reads, 2, "the EOCD and the directory");
        }
        // Indexed: the index pass saw every name, including the long one
        // read in pieces.
        for (images, expected) in [(0, false), (1, true), (3, true)] {
            let zip = book_zip(60, images);
            let mut tail = [0u8; 2048];
            let reader = CountingReader {
                bytes: &zip,
                reads: 0,
            };
            let mut stream = ZipStream::new(reader, &mut tail).unwrap();
            let opened = stream.reader.reads;
            assert_eq!(stream.has_image_entries(&mut [0u8; 64]), Ok(expected));
            assert_eq!(stream.reader.reads, opened, "no read past the open");
        }
        // An index cut short knows only an image it saw; otherwise one walk
        // settles it, through the caller's buffer, and is not repeated.
        for (images, expected) in [(0, false), (2, true)] {
            let zip = book_zip(60, images);
            let mut tail = [0u8; 400];
            let reader = CountingReader {
                bytes: &zip,
                reads: 0,
            };
            let mut stream = ZipStream::new(reader, &mut tail).unwrap();
            let opened = stream.reader.reads;
            let mut buf = [0u8; 2048];
            assert_eq!(stream.has_image_entries(&mut buf), Ok(expected));
            let walked = stream.reader.reads - opened;
            assert!((1..=4).contains(&walked), "{walked} reads");
            assert_eq!(stream.has_image_entries(&mut buf), Ok(expected));
            assert_eq!(stream.reader.reads - opened, walked);
        }
        // A forward-only reader cannot read an image back: it has none.
        let zip = book_zip(3, 2);
        let mut local = ZipLocalStream::new(SliceStream {
            bytes: &zip,
            cursor: 0,
        });
        assert_eq!(
            EpubZipOps::has_image_entries(&mut local, &mut [0u8; 64]),
            Ok(false)
        );
    }

    #[test]
    fn a_damaged_directory_fails_the_walk_without_panicking() {
        let zip = book_zip(30, 2);
        let central = zip.len()
            - 22
            - u32::from_le_bytes(zip[zip.len() - 10..zip.len() - 6].try_into().unwrap()) as usize;
        for (at, value) in [(central, 0u8), (central + 28, 0xff), (central + 29, 0xff)] {
            let mut damaged = zip.clone();
            damaged[at] = value;
            for tail_len in [8192usize, 300] {
                let mut tail = std::vec![0u8; tail_len];
                let Ok(mut stream) = ZipStream::new(SliceReader { bytes: &damaged }, &mut tail)
                else {
                    continue;
                };
                let result = stream.for_each_entry(&mut [0u8; 256], &mut |_| true);
                assert!(result.is_err(), "byte {at} tail {tail_len}");
            }
        }
    }

    /// Whether the index pass, which sees each name in 64-byte pieces,
    /// finds an image among `names` without any read past the open.
    fn indexed_images(names: &[&str]) -> bool {
        let files: StdVec<(&str, &[u8])> = names.iter().map(|name| (*name, &b"x"[..])).collect();
        let zip = stored_zip(&files);
        // Too small to cache the directory, big enough to index it whole.
        let mut tail = [0u8; 200];
        let reader = CountingReader {
            bytes: &zip,
            reads: 0,
        };
        let mut stream = ZipStream::new(reader, &mut tail).unwrap();
        assert!(
            matches!(stream.lookup, CentralLookup::Indexed { complete: true, .. }),
            "the index pass saw every name"
        );
        let opened = stream.reader.reads;
        let found = stream.has_image_entries(&mut [0u8; 64]).unwrap();
        assert_eq!(stream.reader.reads, opened);
        // A walk, which reads each name whole, agrees.
        let mut walked = false;
        stream
            .for_each_entry(&mut [0u8; 512], &mut |entry| {
                walked |= crate::image::index::is_image_path(entry.name);
                true
            })
            .unwrap();
        assert_eq!(found, walked, "{names:?}");
        found
    }

    #[test]
    fn the_index_pass_finds_an_extension_across_its_name_pieces() {
        let filler = [
            "OEBPS/a.xhtml",
            "OEBPS/b.xhtml",
            "OEBPS/c.xhtml",
            "OEBPS/d.xhtml",
        ];
        // 65 to 67 bytes: a 64-byte first piece would leave only "g",
        // "ng" or "png" for the last.
        for extra in 0..3 {
            let name = std::format!("OEBPS/images/{}.png", "p".repeat(48 + extra));
            assert_eq!(name.len(), 65 + extra);
            let mut names = filler.to_vec();
            names.push(&name);
            assert!(indexed_images(&names), "{} bytes", name.len());
        }
        // A character split across two pieces is still UTF-8.
        // 130 bytes: pieces of 2, 64 and 64, the é at 65 and 66.
        let accented = std::format!("OEBPS/images/{}é{}.png", "a".repeat(52), "b".repeat(59));
        assert_eq!(accented.len(), 130);
        assert!(core::str::from_utf8(&accented.as_bytes()[2..66]).is_err());
        let mut names = filler.to_vec();
        names.push(&accented);
        assert!(indexed_images(&names));
        assert!(!indexed_images(&filler));
    }

    #[test]
    fn a_name_that_is_not_utf8_is_no_image_to_the_open_or_the_walk() {
        for prefix_len in [3usize, 60, 70] {
            let mut name = b"OEBPS/".to_vec();
            name.extend(std::iter::repeat_n(b'x', prefix_len));
            name.push(0xff);
            name.extend_from_slice(b".png");
            let zip = {
                let names = ["OEBPS/a.xhtml", "OEBPS/b.xhtml", "OEBPS/c.xhtml"];
                let mut files: StdVec<(&str, &[u8])> =
                    names.iter().map(|name| (*name, &b"x"[..])).collect();
                // stored_zip takes names as text; write the bad name after.
                files.push(("OEBPS/z", &b"x"[..]));
                let mut zip = stored_zip(&files);
                let at = zip.windows(7).rposition(|w| w == b"OEBPS/z").unwrap();
                let mut patched = zip[..at].to_vec();
                // Only the central copy needs the bad name: the open and
                // the walk read nothing else. Keep its length in step.
                let length_at = at - 46 + 28;
                patched[length_at..length_at + 2]
                    .copy_from_slice(&(name.len() as u16).to_le_bytes());
                patched.extend_from_slice(&name);
                let rest = zip.split_off(at + 7);
                let central_size_at = rest.len() - 22 + 12;
                let mut rest = rest;
                let size = u32::from_le_bytes(
                    rest[central_size_at..central_size_at + 4]
                        .try_into()
                        .unwrap(),
                ) + name.len() as u32
                    - 7;
                rest[central_size_at..central_size_at + 4].copy_from_slice(&size.to_le_bytes());
                patched.extend_from_slice(&rest);
                patched
            };
            for tail_len in [4096usize, 300] {
                let mut tail = std::vec![0u8; tail_len];
                let mut stream = ZipStream::new(SliceReader { bytes: &zip }, &mut tail).unwrap();
                assert_eq!(
                    stream.has_image_entries(&mut [0u8; 512]),
                    Ok(false),
                    "prefix {prefix_len} tail {tail_len}"
                );
            }
        }
    }

    /// A reader whose reads starting in `fails` fail, as a card's can.
    struct FailingReader<'a> {
        bytes: &'a [u8],
        fails: core::ops::Range<u32>,
    }

    impl ReadAt for FailingReader<'_> {
        type Error = ();

        fn len(&mut self) -> Result<u32, Self::Error> {
            Ok(self.bytes.len() as u32)
        }

        fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
            if self.fails.contains(&offset) {
                return Err(());
            }
            SliceReader { bytes: self.bytes }.read_at(offset, out)
        }
    }

    #[test]
    fn reading_an_entry_by_offset_tells_damage_from_a_failed_read() {
        let png = b"\x89PNG\r\n\x1a\n0123456789abcdefghijklmnopqrstuvwxyz";
        let mut deflated = std::vec![0x01, png.len() as u8, 0, !(png.len() as u8), 0xff];
        deflated.extend_from_slice(png);
        // Empty stored blocks that never finish: a stream that reads on
        // and on without producing a byte.
        let endless: StdVec<u8> = std::iter::repeat_n([0u8, 0, 0, 0xff, 0xff], 2000)
            .flatten()
            .collect();
        let zip = zip_with_methods(&[
            ("a.png", 0, png, png),
            ("b.png", 8, &deflated, png),
            ("c.png", 8, &endless, png),
            ("d.png", 9, png, png),
        ]);
        let mut tail = [0u8; 4096];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip }, &mut tail).unwrap();
        let entry = |stream: &mut ZipStream<'_, SliceReader<'_>>, name: &str| {
            stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .unwrap()
        };
        let read =
            |stream: &mut ZipStream<'_, SliceReader<'_>>, entry: OwnedZipEntry, name: &str| {
                let mut input = [0xa5u8; 512];
                let mut output = [0xa5u8; 33];
                let mut inflate = ZipInflateScratch::new();
                let mut prefix = Prefix {
                    bytes: StdVec::new(),
                    want: 33,
                };
                stream
                    .visit_entry_start(
                        entry,
                        &mut |found| found == name.as_bytes(),
                        &mut input,
                        &mut output,
                        &mut inflate,
                        4096,
                        &mut prefix,
                    )
                    .map(|()| prefix.bytes)
            };
        for name in ["a.png", "b.png"] {
            let found = entry(&mut stream, name);
            assert_eq!(
                read(&mut stream, found, name).unwrap(),
                &png[..33],
                "{name}"
            );
        }
        let c = entry(&mut stream, "c.png");
        assert_eq!(read(&mut stream, c, "c.png"), Err(ZipError::Inflate));
        let d = entry(&mut stream, "d.png");
        assert_eq!(
            read(&mut stream, d, "d.png"),
            Err(ZipError::UnsupportedCompression)
        );
        // The header names another entry, or the offset or the data runs
        // past the archive's end: damage, not a failed read.
        let a = entry(&mut stream, "a.png");
        assert_eq!(read(&mut stream, a, "b.png"), Err(ZipError::BadLocalHeader));
        for damaged in [
            OwnedZipEntry {
                local_header_offset: zip.len() as u32 - 10,
                ..a
            },
            OwnedZipEntry {
                local_header_offset: u32::MAX - 4,
                ..a
            },
            OwnedZipEntry {
                compressed_size: zip.len() as u32,
                ..a
            },
            OwnedZipEntry {
                local_header_offset: a.local_header_offset + 1,
                ..a
            },
        ] {
            assert_eq!(
                read(&mut stream, damaged, "a.png"),
                Err(ZipError::BadLocalHeader),
                "{damaged:?}"
            );
        }
        // A read that fails is only that, worth trying again.
        let b = entry(&mut stream, "b.png");
        let mut tail = [0u8; 4096];
        let mut failing = ZipStream::new(
            FailingReader {
                bytes: &zip,
                fails: b.local_header_offset..b.local_header_offset + 64,
            },
            &mut tail,
        )
        .unwrap();
        let mut input = [0u8; 512];
        let mut output = [0u8; 33];
        let mut inflate = ZipInflateScratch::new();
        let mut prefix = Prefix {
            bytes: StdVec::new(),
            want: 33,
        };
        assert_eq!(
            failing.visit_entry_start(
                b,
                &mut |_| true,
                &mut input,
                &mut output,
                &mut inflate,
                4096,
                &mut prefix,
            ),
            Err(ZipError::Io)
        );
    }

    /// Collects an entry's first `want` bytes.
    struct Prefix {
        bytes: StdVec<u8>,
        want: usize,
    }

    impl EntryVisitor for Prefix {
        fn take(&mut self, bytes: &[u8]) -> bool {
            let n = (self.want - self.bytes.len()).min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..n]);
            self.bytes.len() < self.want
        }
    }

    /// Takes 4 bytes at every 1,000th offset and passes over the rest.
    struct Sampler {
        seen: StdVec<(u32, u8)>,
        at: u32,
        skip: u32,
    }

    impl EntryVisitor for Sampler {
        fn take(&mut self, bytes: &[u8]) -> bool {
            for &byte in bytes {
                if self.skip > 0 {
                    self.skip -= 1;
                } else {
                    self.seen.push((self.at, byte));
                    if self.at % 1000 == 3 {
                        self.skip = 996;
                    }
                }
                self.at += 1;
            }
            self.seen.len() < 40
        }

        fn skippable(&self) -> u32 {
            self.skip
        }

        fn skip(&mut self, n: u32) {
            self.skip -= n;
            self.at += n;
        }
    }

    /// A reader that records where each read starts.
    struct LoggingReader<'a> {
        bytes: &'a [u8],
        reads: &'a core::cell::RefCell<StdVec<u32>>,
    }

    impl ReadAt for LoggingReader<'_> {
        type Error = ();

        fn len(&mut self) -> Result<u32, Self::Error> {
            Ok(self.bytes.len() as u32)
        }

        fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
            self.reads.borrow_mut().push(offset);
            SliceReader { bytes: self.bytes }.read_at(offset, out)
        }
    }

    /// A JPEG of nothing but maximal APP1 segments deflates about 1,000
    /// to 1: under a 256 KB cap on compressed bytes it would inflate to a
    /// quarter of a GB. The cap holds for inflated bytes too, and the
    /// probe fails there.
    #[test]
    fn a_deflated_probe_stops_at_the_cap_on_inflated_bytes() {
        let mut bomb = std::vec![0xFF, 0xD8];
        for _ in 0..64 {
            bomb.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xFF]);
            bomb.extend(std::iter::repeat_n(0u8, 65_533));
        }
        let deflated = miniz_oxide::deflate::compress_to_vec(&bomb, 9);
        assert!(deflated.len() * 500 < bomb.len(), "{}", deflated.len());
        let zip = zip_with_methods(&[("bomb.jpg", 8, &deflated, &bomb)]);
        let mut tail = [0u8; 4096];
        let mut stream = ZipStream::new(SliceReader { bytes: &zip }, &mut tail).unwrap();
        let entry = stream
            .find_entry("bomb.jpg", &mut [0u8; 46], &mut [0u8; 64])
            .unwrap();
        struct Counting(crate::image::index::ImageProber, usize);
        impl EntryVisitor for Counting {
            fn take(&mut self, bytes: &[u8]) -> bool {
                self.1 += bytes.len();
                self.0.take(bytes)
            }
        }
        let mut prober = Counting(crate::image::index::ImageProber::new(), 0);
        let cap = 256 * 1024;
        let result = stream.visit_entry_start(
            entry,
            &mut |_| true,
            &mut [0u8; 512],
            &mut [0u8; 512],
            &mut ZipInflateScratch::new(),
            cap,
            &mut prober,
        );
        assert_eq!(result, Err(ZipError::OutputTooSmall));
        assert!(
            (cap as usize..cap as usize + 512).contains(&prober.1),
            "{}",
            prober.1
        );
        assert_eq!(
            prober.0.finish().status,
            crate::image::index::ProbeStatus::Failed
        );
    }

    #[test]
    fn a_stored_entry_is_not_read_where_the_visitor_skips() {
        let data: StdVec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut deflated = StdVec::new();
        for piece in data.chunks(65_535) {
            deflated.push(0);
            deflated.extend_from_slice(&(piece.len() as u16).to_le_bytes());
            deflated.extend_from_slice(&(!(piece.len() as u16)).to_le_bytes());
            deflated.extend_from_slice(piece);
        }
        deflated.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
        let zip = zip_with_methods(&[("s.bin", 0, &data, &data), ("d.bin", 8, &deflated, &data)]);
        let expected: StdVec<(u32, u8)> = (0..10u32)
            .flat_map(|k| (0..4).map(move |i| k * 1000 + i))
            .map(|at| (at, data[at as usize]))
            .collect();
        for name in ["s.bin", "d.bin"] {
            let reads = core::cell::RefCell::new(StdVec::new());
            let mut tail = [0u8; 4096];
            let mut stream = ZipStream::new(
                LoggingReader {
                    bytes: &zip,
                    reads: &reads,
                },
                &mut tail,
            )
            .unwrap();
            let entry = stream
                .find_entry(name, &mut [0u8; 46], &mut [0u8; 64])
                .unwrap();
            reads.borrow_mut().clear();
            let mut sampler = Sampler {
                seen: StdVec::new(),
                at: 0,
                skip: 0,
            };
            stream
                .visit_entry_start(
                    entry,
                    &mut |found| found == name.as_bytes(),
                    &mut [0u8; 64],
                    &mut [0u8; 100],
                    &mut ZipInflateScratch::new(),
                    1 << 20,
                    &mut sampler,
                )
                .unwrap();
            assert_eq!(sampler.seen, expected, "{name}");
            let data_reads = reads.borrow().len();
            if name == "s.bin" {
                // The local header, its name, then one read per sample.
                assert_eq!(data_reads, 2 + 10, "{:?}", reads.borrow());
            } else {
                // Deflated: everything up to the last sample is read.
                assert!(data_reads > 100, "{data_reads}");
            }
        }
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
}
