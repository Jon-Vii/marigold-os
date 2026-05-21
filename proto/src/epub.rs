use crate::book::{BookId, BookMeta, BookSource, ChapterMeta, CoverStatus};
use crate::text::{FontStyle, TextAlign, TextBlock, TextRole, TextRun};
use heapless::Vec;
use miniz_oxide::inflate::decompress_slice_iter_to_slice;
use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

pub const MAX_SPINE_ITEMS: usize = 128;
pub const MAX_MANIFEST_ITEMS: usize = 160;
pub const MAX_ENTRY_NAME_BYTES: usize = 160;

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

pub struct ZipStream<R> {
    reader: R,
    central_offset: u32,
    entry_count: u16,
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

impl<R> ZipStream<R>
where
    R: ReadAt,
{
    pub fn new(mut reader: R, tail_scratch: &mut [u8]) -> Result<Self, ZipError> {
        let len = reader.len().map_err(|_| ZipError::Io)?;
        if len < 22 {
            return Err(ZipError::MissingEndOfCentralDirectory);
        }
        let tail_len = tail_scratch.len().min(len as usize);
        let tail_offset = len - tail_len as u32;
        read_exact_at(&mut reader, tail_offset, &mut tail_scratch[..tail_len])?;
        let eocd_in_tail =
            find_eocd(&tail_scratch[..tail_len]).ok_or(ZipError::MissingEndOfCentralDirectory)?;
        let eocd = eocd_in_tail;
        let entry_count = read_u16(tail_scratch, eocd + 10)?;
        let central_offset = read_u32(tail_scratch, eocd + 16)?;
        Ok(Self {
            reader,
            central_offset,
            entry_count,
        })
    }

    pub fn find_entry(
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

    fn inflate_entry_prefix(
        &mut self,
        payload_offset: u32,
        compressed_size: u32,
        input: &mut [u8],
        output: &mut [u8],
        inflate_scratch: &mut ZipInflateScratch,
    ) -> Result<(usize, bool), ZipError> {
        if input.is_empty() || output.is_empty() {
            return Err(ZipError::OutputTooSmall);
        }
        inflate_scratch.state.reset(DataFormat::Raw);
        let mut compressed_read = 0u32;
        let mut output_pos = 0usize;
        while compressed_read < compressed_size {
            let remaining = (compressed_size - compressed_read) as usize;
            let input_len = input.len().min(remaining);
            read_exact_at(
                &mut self.reader,
                payload_offset + compressed_read,
                &mut input[..input_len],
            )?;
            compressed_read += input_len as u32;
            let flush = if compressed_read == compressed_size {
                MZFlush::Finish
            } else {
                MZFlush::None
            };
            let mut consumed = 0usize;
            while consumed < input_len || flush == MZFlush::Finish {
                if output_pos == output.len() {
                    return Ok((output_pos, false));
                }
                let result = inflate(
                    &mut inflate_scratch.state,
                    &input[consumed..input_len],
                    &mut output[output_pos..],
                    flush,
                );
                consumed += result.bytes_consumed;
                output_pos += result.bytes_written;
                match result.status {
                    Ok(MZStatus::StreamEnd) => return Ok((output_pos, true)),
                    Ok(MZStatus::Ok) => {
                        if result.bytes_consumed == 0 && result.bytes_written == 0 {
                            break;
                        }
                        if consumed == input_len {
                            break;
                        }
                    }
                    Ok(_) => return Err(ZipError::Inflate),
                    Err(_) => {
                        if output_pos == output.len() {
                            return Ok((output_pos, false));
                        }
                        return Err(ZipError::Inflate);
                    }
                }
            }
        }
        Ok((
            output_pos,
            inflate_scratch.state.last_status() == miniz_oxide::inflate::TINFLStatus::Done,
        ))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestItem<'a> {
    pub id: &'a str,
    pub href: &'a str,
    pub media_type: &'a str,
    pub properties: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpineItem<'a> {
    pub idref: &'a str,
    pub href: &'a str,
    pub media_type: &'a str,
    pub properties: &'a str,
}

pub struct EpubPackage<'a> {
    pub meta: BookMeta<'a>,
    pub opf_path: &'a str,
    pub text_reference_href: Option<&'a str>,
    pub nav_href: Option<&'a str>,
    pub ncx_href: Option<&'a str>,
    pub manifest: Vec<ManifestItem<'a>, MAX_MANIFEST_ITEMS>,
    pub spine: Vec<SpineItem<'a>, MAX_SPINE_ITEMS>,
}

impl<'a> EpubPackage<'a> {
    pub fn chapters(&self, output: &mut Vec<ChapterMeta<'a>, MAX_SPINE_ITEMS>) {
        output.clear();
        for (index, spine) in self.spine.iter().enumerate() {
            let title = spine.href.rsplit('/').next().unwrap_or(spine.href);
            let _ = output.push(ChapterMeta {
                title,
                spine_index: index as u16,
                source_href: spine.href,
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

pub fn parse_opf<'a>(
    opf_xml: &'a str,
    book_id: BookId,
    source_path: &'a str,
    byte_size: u32,
    opf_path: &'a str,
) -> Result<EpubPackage<'a>, EpubError> {
    let title = element_text(opf_xml, "title").unwrap_or("Untitled");
    let author = element_text(opf_xml, "creator").unwrap_or("Unknown Author");

    let mut spine_idrefs: Vec<&'a str, MAX_SPINE_ITEMS> = Vec::new();
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
                let _ = spine_idrefs.push(idref);
            }
            _ => {}
        }
    }

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
                            id,
                            href,
                            media_type,
                            properties,
                        })
                        .map_err(|_| EpubError::TooManyManifestItems)?;
                }
            }
            _ => {}
        }
    }

    let mut spine = Vec::new();
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
                let Some(item) = manifest.iter().find(|item| item.id == idref) else {
                    continue;
                };
                spine
                    .push(SpineItem {
                        idref,
                        href: item.href,
                        media_type: item.media_type,
                        properties: item.properties,
                    })
                    .ok();
            }
            _ => {}
        }
    }

    let text_reference_href = find_guide_reference(opf_xml, "text")
        .or_else(|| find_guide_reference(opf_xml, "start"))
        .map(strip_fragment);
    let nav_href = manifest
        .iter()
        .find(|item| {
            item.properties
                .split_ascii_whitespace()
                .any(|prop| prop == "nav")
        })
        .map(|item| item.href);
    let ncx_href = manifest
        .iter()
        .find(|item| {
            item.media_type
                .eq_ignore_ascii_case("application/x-dtbncx+xml")
                || item.href.ends_with(".ncx")
        })
        .map(|item| item.href);

    Ok(EpubPackage {
        meta: BookMeta {
            id: book_id,
            title,
            author,
            source_path,
            byte_size,
            source: BookSource::MicroSd,
            cover_status: cover_status(&manifest),
        },
        opf_path,
        text_reference_href,
        nav_href,
        ncx_href,
        manifest,
        spine,
    })
}

pub fn xhtml_text_runs<'a>(
    xhtml: &'a str,
    output: &mut Vec<TextRun<'a>, 256>,
) -> Result<(), XhtmlError> {
    xhtml_text_runs_with_css(xhtml, None, output)
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
        || media_type.contains("css")
        || id == "cover"
        || href.contains("cover")
        || href.ends_with(".ncx")
        || href.ends_with(".css")
}

pub fn xhtml_text_runs_with_css<'a>(
    xhtml: &'a str,
    css: Option<&CssRules>,
    output: &mut Vec<TextRun<'a>, 256>,
) -> Result<(), XhtmlError> {
    output.clear();
    let mut cursor = XmlCursor::new(xhtml);
    let mut role = TextRole::Body;
    let mut align = TextAlign::Justify;
    let mut bold_depth = 0u8;
    let mut italic_depth = 0u8;
    let body_required = xhtml.contains("<body") || xhtml.contains(":body");
    let mut in_body = !body_required;
    let mut skip_depth = 0u8;
    let mut skip_tag: Option<&str> = None;
    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "body") => {
                in_body = true;
            }
            Token::End(tag) if tag_name_is(tag, "body") => {
                in_body = false;
            }
            Token::Start(tag)
                if skip_depth == 0
                    && (tag_name_is(tag, "head")
                        || tag_name_is(tag, "style")
                        || tag_name_is(tag, "script")
                        || tag_name_is(tag, "svg")
                        || tag_name_is(tag, "nav")
                        || tag_is_hidden(tag)) =>
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
            Token::Start(tag) if tag_name_is(tag, "br") => {
                push_break_run(output);
            }
            Token::Start(tag) if tag_starts_block(tag) => {
                push_break_run(output);
                align = block_align_for_tag(tag, css).unwrap_or(TextAlign::Justify);
                if tag_name_is(tag, "li") {
                    push_text_run("- ", role, bold_depth, italic_depth, align, output);
                }
            }
            Token::Start(tag) if tag_name_is(tag, "h1") => {
                push_break_run(output);
                role = TextRole::Heading1;
                align = TextAlign::Center;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "h2") => {
                push_break_run(output);
                role = TextRole::Heading2;
                align = TextAlign::Center;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "h3") => {
                push_break_run(output);
                role = TextRole::Heading3;
                align = TextAlign::Center;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "blockquote") => {
                push_break_run(output);
                role = TextRole::BlockQuote;
                align = block_align_for_tag(tag, css).unwrap_or(TextAlign::Left);
                italic_depth = italic_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "strong") || tag_name_is(tag, "b") => {
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "em") || tag_name_is(tag, "i") => {
                italic_depth = italic_depth.saturating_add(1);
            }
            Token::End(tag)
                if tag_name_is(tag, "h1") || tag_name_is(tag, "h2") || tag_name_is(tag, "h3") =>
            {
                role = TextRole::Body;
                bold_depth = bold_depth.saturating_sub(1);
                push_break_run(output);
                align = TextAlign::Justify;
            }
            Token::End(tag) if tag_name_is(tag, "blockquote") => {
                role = TextRole::Body;
                italic_depth = italic_depth.saturating_sub(1);
                push_break_run(output);
                align = TextAlign::Justify;
            }
            Token::End(tag) if tag_name_is(tag, "strong") || tag_name_is(tag, "b") => {
                bold_depth = bold_depth.saturating_sub(1);
            }
            Token::End(tag) if tag_name_is(tag, "em") || tag_name_is(tag, "i") => {
                italic_depth = italic_depth.saturating_sub(1);
            }
            Token::End(tag) if tag_ends_block(tag) => {
                push_break_run(output);
                align = TextAlign::Justify;
            }
            Token::Text(text) => {
                push_text_run(text, role, bold_depth, italic_depth, align, output);
            }
            _ => {}
        }
    }

    Ok(())
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
    let mut cursor = XmlCursor::new(xhtml);
    let mut block = heapless::String::<384>::new();
    let mut role = TextRole::Body;
    let mut align = TextAlign::Justify;
    let mut bold_depth = 0u8;
    let mut italic_depth = 0u8;
    let body_required = xhtml.contains("<body") || xhtml.contains(":body");
    let mut in_body = !body_required;
    let mut skip_depth = 0u8;
    let mut skip_tag: Option<&str> = None;
    let mut list_kind_stack = [ListKind::Unordered; 8];
    let mut list_count_stack = [0u16; 8];
    let mut list_depth = 0usize;
    let mut table_align_stack = [TextAlign::Justify; 4];
    let mut table_depth = 0usize;

    while let Some(token) = cursor.next_token() {
        match token {
            Token::Start(tag) if tag_name_is(tag, "body") => {
                in_body = true;
            }
            Token::End(tag) if tag_name_is(tag, "body") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                in_body = false;
            }
            Token::Start(tag)
                if skip_depth == 0
                    && (tag_name_is(tag, "head")
                        || tag_name_is(tag, "style")
                        || tag_name_is(tag, "script")
                        || tag_name_is(tag, "svg")
                        || tag_name_is(tag, "nav")
                        || tag_is_hidden(tag)
                        || tag_is_pagebreak(tag)) =>
            {
                if !tag_is_void(tag) {
                    skip_tag = tag_local_name(tag);
                    skip_depth = skip_depth.saturating_add(1);
                }
            }
            Token::End(tag) if skip_tag.map(|name| tag_name_is(tag, name)).unwrap_or(false) => {
                skip_depth = skip_depth.saturating_sub(1);
                if skip_depth == 0 {
                    skip_tag = None;
                }
            }
            _ if !in_body || skip_depth > 0 => {}
            Token::Start(tag) if tag_name_is(tag, "table") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                if table_depth < table_align_stack.len() {
                    table_align_stack[table_depth] =
                        table_align_for_tag(tag, css).unwrap_or(TextAlign::Justify);
                    table_depth += 1;
                }
            }
            Token::End(tag) if tag_name_is(tag, "table") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                table_depth = table_depth.saturating_sub(1);
            }
            Token::Start(tag) if tag_name_is(tag, "td") || tag_name_is(tag, "th") => {
                append_table_cell_separator(&mut block);
            }
            Token::Start(tag) if tag_name_is(tag, "br") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
            }
            Token::Start(tag) if tag_name_is(tag, "img") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                let placeholder = attr_value(tag, "alt").unwrap_or("[Image]");
                sink.push_block(
                    placeholder,
                    TextRole::Body,
                    FontStyle::Italic,
                    TextAlign::Center,
                    true,
                )?;
            }
            Token::Start(tag) if tag_name_is(tag, "ul") || tag_name_is(tag, "ol") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                if list_depth < list_kind_stack.len() {
                    list_kind_stack[list_depth] = if tag_name_is(tag, "ol") {
                        ListKind::Ordered
                    } else {
                        ListKind::Unordered
                    };
                    list_count_stack[list_depth] = 0;
                    list_depth += 1;
                }
            }
            Token::End(tag) if tag_name_is(tag, "ul") || tag_name_is(tag, "ol") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                list_depth = list_depth.saturating_sub(1);
            }
            Token::Start(tag) if tag_starts_block(tag) => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                align = block_align_for_tag(tag, css)
                    .or_else(|| current_table_align(&table_align_stack, table_depth))
                    .unwrap_or(TextAlign::Justify);
                if tag_name_is(tag, "li") {
                    append_list_marker(
                        &mut block,
                        &mut list_count_stack,
                        &list_kind_stack,
                        list_depth,
                    );
                }
            }
            Token::Start(tag) if tag_name_is(tag, "h1") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                role = TextRole::Heading1;
                align = TextAlign::Center;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "h2") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                role = TextRole::Heading2;
                align = TextAlign::Center;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag)
                if tag_name_is(tag, "h3")
                    || tag_name_is(tag, "h4")
                    || tag_name_is(tag, "h5")
                    || tag_name_is(tag, "h6") =>
            {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                role = TextRole::Heading3;
                align = TextAlign::Center;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "blockquote") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                role = TextRole::BlockQuote;
                align = block_align_for_tag(tag, css).unwrap_or(TextAlign::Left);
                italic_depth = italic_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_name_is(tag, "strong") || tag_name_is(tag, "b") => {
                flush_sink_block_continue(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                bold_depth = bold_depth.saturating_add(1);
            }
            Token::Start(tag) if tag_is_italic(tag) => {
                flush_sink_block_continue(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                italic_depth = italic_depth.saturating_add(1);
            }
            Token::End(tag)
                if tag_name_is(tag, "h1")
                    || tag_name_is(tag, "h2")
                    || tag_name_is(tag, "h3")
                    || tag_name_is(tag, "h4")
                    || tag_name_is(tag, "h5")
                    || tag_name_is(tag, "h6") =>
            {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                role = TextRole::Body;
                bold_depth = bold_depth.saturating_sub(1);
                align = TextAlign::Justify;
            }
            Token::End(tag) if tag_name_is(tag, "blockquote") => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                role = TextRole::Body;
                italic_depth = italic_depth.saturating_sub(1);
                align = TextAlign::Justify;
            }
            Token::End(tag) if tag_name_is(tag, "strong") || tag_name_is(tag, "b") => {
                flush_sink_block_continue(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                bold_depth = bold_depth.saturating_sub(1);
            }
            Token::End(tag) if tag_is_italic(tag) => {
                flush_sink_block_continue(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                italic_depth = italic_depth.saturating_sub(1);
            }
            Token::End(tag) if tag_ends_block(tag) => {
                flush_sink_block(
                    &mut block,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
                align = TextAlign::Justify;
            }
            Token::Text(text) => {
                append_text_to_sink_block(
                    &mut block,
                    text,
                    role,
                    style_for(bold_depth, italic_depth),
                    align,
                    sink,
                )?;
            }
            _ => {}
        }
    }
    flush_sink_block(
        &mut block,
        role,
        style_for(bold_depth, italic_depth),
        align,
        sink,
    )
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
                        || tag_is_hidden(tag)) =>
            {
                if !tag_is_void(tag) {
                    skip_tag = tag_local_name(tag);
                    skip_depth = skip_depth.saturating_add(1);
                }
            }
            Token::End(tag) if skip_tag.map(|name| tag_name_is(tag, name)).unwrap_or(false) => {
                skip_depth = skip_depth.saturating_sub(1);
                if skip_depth == 0 {
                    skip_tag = None;
                }
            }
            _ if !in_body || skip_depth > 0 => {}
            Token::Start(tag) if tag_name_is(tag, "nav") => in_nav = true,
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

fn push_text_run<'a>(
    text: &'a str,
    role: TextRole,
    bold_depth: u8,
    italic_depth: u8,
    align: TextAlign,
    output: &mut Vec<TextRun<'a>, 256>,
) {
    if text.trim().is_empty() {
        return;
    }
    let _ = output.push(TextRun::aligned(
        text,
        role,
        style_for(bold_depth, italic_depth),
        align,
    ));
}

fn push_break_run<'a>(output: &mut Vec<TextRun<'a>, 256>) {
    if output.last().map(|run| run.text == "\n").unwrap_or(true) {
        return;
    }
    let _ = output.push(TextRun::new("\n", TextRole::Body, FontStyle::Regular));
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

fn decode_html_entity(input: &str) -> Option<char> {
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

fn decode_numeric_entity(input: &str) -> Option<char> {
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

fn strip_fragment(value: &str) -> &str {
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

fn cover_status(manifest: &[ManifestItem<'_>]) -> CoverStatus {
    if manifest.iter().any(|item| {
        item.id == "cover" || item.href.contains("cover") || item.media_type.starts_with("image/")
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
        assert_eq!(package.spine[0].href, "text/ch1.xhtml");
    }

    #[test]
    fn xhtml_emits_styled_runs() {
        let xhtml = "<body><h1>Chapter</h1><p>Hello <em>soft</em> <strong>bold</strong></p></body>";
        let mut runs = heapless::Vec::<TextRun<'_>, 256>::new();

        xhtml_text_runs(xhtml, &mut runs).expect("runs fit");
        let visible = visible_runs(&runs);

        assert_eq!(
            visible[0],
            TextRun::aligned(
                "Chapter",
                TextRole::Heading1,
                FontStyle::Bold,
                TextAlign::Center
            )
        );
        assert_eq!(
            visible[1],
            TextRun::aligned(
                "Hello ",
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Justify
            )
        );
        assert_eq!(
            visible[2],
            TextRun::aligned(
                "soft",
                TextRole::Body,
                FontStyle::Italic,
                TextAlign::Justify
            )
        );
        assert_eq!(
            visible[3],
            TextRun::aligned("bold", TextRole::Body, FontStyle::Bold, TextAlign::Justify)
        );
    }

    #[test]
    fn xhtml_skips_head_style_and_script_text() {
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
        let mut runs = heapless::Vec::<TextRun<'_>, 256>::new();

        xhtml_text_runs(xhtml, &mut runs).expect("runs fit");
        let visible = visible_runs(&runs);

        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible[0],
            TextRun::aligned(
                "Actual chapter text.",
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Justify
            )
        );
    }

    #[test]
    fn xhtml_skips_nav_and_hidden_content() {
        let xhtml = r#"
            <html>
              <body>
                <nav>Table of contents</nav>
                <p style="display:none">Hidden paragraph</p>
                <p>Visible text</p>
              </body>
            </html>
        "#;
        let mut runs = heapless::Vec::<TextRun<'_>, 256>::new();

        xhtml_text_runs(xhtml, &mut runs).expect("runs fit");
        let visible = visible_runs(&runs);

        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible[0],
            TextRun::aligned(
                "Visible text",
                TextRole::Body,
                FontStyle::Regular,
                TextAlign::Justify
            )
        );
    }

    #[test]
    fn xhtml_emits_breaks_between_list_items() {
        let xhtml = "<body><ol><li>One</li><li>Two</li></ol></body>";
        let mut runs = heapless::Vec::<TextRun<'_>, 256>::new();

        xhtml_text_runs(xhtml, &mut runs).expect("runs fit");

        assert!(runs.iter().any(|run| run.text == "\n"));
        let visible = visible_runs(&runs);
        assert_eq!(visible[0].text, "- ");
        assert_eq!(visible[1].text, "One");
        assert_eq!(visible[2].text, "- ");
        assert_eq!(visible[3].text, "Two");
    }

    #[test]
    fn xhtml_marks_center_aligned_blocks() {
        let xhtml =
            r#"<body><p class="center">Title</p><p style="text-align: center">Author</p></body>"#;
        let mut runs = heapless::Vec::<TextRun<'_>, 256>::new();

        xhtml_text_runs(xhtml, &mut runs).expect("runs fit");
        let visible = visible_runs(&runs);

        assert_eq!(visible[0].align, TextAlign::Center);
        assert_eq!(visible[1].align, TextAlign::Center);
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
        assert_eq!(package.spine[0].href, "text/ch1.xhtml");
        assert_eq!(package.text_reference_href, Some("text/start.xhtml"));
        assert_eq!(package.nav_href, Some("nav.xhtml"));
        assert_eq!(package.ncx_href, Some("toc.ncx"));
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

    fn visible_runs<'a>(runs: &'a [TextRun<'a>]) -> heapless::Vec<TextRun<'a>, 256> {
        runs.iter()
            .copied()
            .filter(|run| run.text != "\n")
            .collect()
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
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut [0u8; 512])
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
    fn zip_stream_inflates_deflated_entry_in_small_chunks() {
        let plain = b"Large compressed member ".repeat(20);
        let deflated: &[u8] = &[
            243, 73, 44, 74, 79, 85, 72, 206, 207, 45, 40, 74, 45, 46, 78, 77, 81, 200, 77, 205,
            77, 74, 45, 82, 240, 25, 21, 31, 22, 226, 0,
        ];
        let zip_bytes = zip_with_methods(&[("OPS/chapter.xhtml", 8, deflated, plain.as_slice())]);
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut [0u8; 512])
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
        let mut stream = ZipStream::new(SliceReader { bytes: &zip_bytes }, &mut [0u8; 512])
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
