use display::fb::Framebuffer;
use display::font::{
    draw_text, literata, measure_text, style_from_marker_code, style_marker_code, BitmapFont,
    FontStyle, STYLE_MARKER,
};
use display::render::{draw_ascii, fill_rect};
use display::{Rect, HEIGHT, WIDTH};
use proto::book::BookId;
use proto::cache::{
    cache_key_for, encode_cover_header, CoverCacheHeader, CACHE_COVER_FILE, CACHE_DIR,
    CACHE_ROOT_DIR, COVER_BYTES, COVER_HEIGHT, COVER_STRIDE, COVER_WIDTH,
};
use proto::epub::{
    decode_html_entity, load_epub_package, parse_css_text_align, strip_fragment,
    xhtml_blocks_to_sink, CssRules, EpubPackage, SpineItem, XhtmlBlockSink, XhtmlError, ZipArchive,
    ZipEntry,
};
use proto::text::{TextAlign, TextRole};
use std::env;
use std::fs::{create_dir_all, read, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use ui::{
    render::render_shell, UiBook, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell,
    UiTocItem, UiView,
};

use display::font::TypeSettings;
use ui::reading::{
    draw_styled_line, first_styled_line_style, line_advance, paragraph_gap, reader_x_for,
    styled_text_ink_width, READER_LEFT_X, READER_PAGE_BOTTOM as PAGE_BOTTOM,
    READER_PAGE_TOP as PAGE_TOP, READER_RIGHT_X, READER_WRAP_SAFETY,
};

mod design_mockups;
mod mockup_fonts_generated;

const MAX_PREVIEW_LINES: usize = 4096;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;
    create_dir_all(&args.out_dir)?;
    if args.design_mockups {
        design_mockups::write_design_mockups(&args.out_dir)?;
        return Ok(());
    }
    if let Some(epub) = args.epub {
        preview_epub(
            &epub,
            &args.out_dir,
            args.pages,
            args.cover_bin.as_deref(),
            args.sd_root.as_deref(),
            args.source_path.as_deref(),
        )?;
    } else {
        write_static_previews(&args.out_dir)?;
    }
    Ok(())
}

struct Args {
    epub: Option<PathBuf>,
    out_dir: PathBuf,
    pages: usize,
    cover_bin: Option<PathBuf>,
    sd_root: Option<PathBuf>,
    source_path: Option<String>,
    design_mockups: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut epub = None;
        let mut out_dir = PathBuf::from("target/previews");
        let mut pages = 5usize;
        let mut cover_bin = None;
        let mut sd_root = None;
        let mut source_path = None;
        let mut design_mockups = false;
        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--design-mockups" => design_mockups = true,
                "--out" => out_dir = PathBuf::from(iter.next().ok_or("--out needs a path")?),
                "--pages" => {
                    let value = iter.next().ok_or("--pages needs a count")?;
                    pages = value.parse().map_err(|_| "--pages needs a number")?;
                }
                "--cover-bin" => {
                    cover_bin = Some(PathBuf::from(
                        iter.next().ok_or("--cover-bin needs a path")?,
                    ))
                }
                "--sd-root" => {
                    sd_root = Some(PathBuf::from(iter.next().ok_or("--sd-root needs a path")?))
                }
                "--source-path" => {
                    source_path = Some(iter.next().ok_or("--source-path needs a path")?)
                }
                "--help" | "-h" => {
                    return Err(
                        "usage: cargo run -- <book.epub> [--out target/previews] [--pages 5] [--cover-bin COVER.BIN] [--sd-root /Volumes/CARD] [--source-path /books/Book.epub]"
                            .into(),
                    );
                }
                value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
                value => epub = Some(PathBuf::from(value)),
            }
        }
        Ok(Self {
            epub,
            out_dir,
            pages,
            cover_bin,
            sd_root,
            source_path,
            design_mockups,
        })
    }
}

fn preview_epub(
    epub_path: &Path,
    out_dir: &Path,
    page_limit: usize,
    cover_bin: Option<&Path>,
    sd_root: Option<&Path>,
    source_path: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = read(epub_path)?;
    let zip = ZipArchive::new(&bytes).map_err(|err| format!("zip: {err:?}"))?;
    let mut container = vec![0u8; 8192];
    let mut opf = vec![0u8; 128 * 1024];
    let package = load_epub_package(
        &bytes,
        &mut container,
        &mut opf,
        BookId(1),
        epub_path.to_str().unwrap_or("book.epub"),
    )
    .map_err(|err| format!("epub package: {err:?}"))?;

    let mut css_rules = CssRules::new();
    load_css_rules(&zip, &package, &mut css_rules);
    if let Some(path) = cover_bin {
        write_cover_cache_file(&zip, &package, path)?;
    }
    if let Some(sd_root) = sd_root {
        let default_source_path;
        let source_path = if let Some(source_path) = source_path {
            source_path
        } else {
            default_source_path = default_device_source_path(epub_path);
            default_source_path.as_str()
        };
        let key = cache_key_for(source_path, bytes.len() as u32);
        let path = sd_root
            .join(CACHE_ROOT_DIR)
            .join(CACHE_DIR)
            .join(key.as_str())
            .join(CACHE_COVER_FILE);
        write_cover_cache_file(&zip, &package, &path)?;
    }

    let start_spine = package
        .text_reference_href
        .and_then(|href| {
            package
                .spine
                .iter()
                .position(|item| strip_fragment(item.href.of(package.opf_text)) == href)
        })
        .unwrap_or(0);

    let mut lines = PreviewLines::new();
    for spine in package
        .spine
        .iter()
        .skip(start_spine)
        .filter(|item| !item.href.is_empty() && !spine_item_is_navigation(item, &package))
    {
        if !lines.is_empty() {
            lines.force_next_line_to_new_page();
        }
        let path = resolve_epub_href(package.opf_path, spine.href.of(package.opf_text));
        let Some(xhtml_entry) = zip.entries().flatten().find(|entry| entry.name == path) else {
            continue;
        };
        let xhtml_bytes = read_zip_entry(&zip, xhtml_entry)?;
        let Ok(xhtml) = std::str::from_utf8(&xhtml_bytes) else {
            continue;
        };
        let mut sink = PreviewSink::new(&mut lines);
        xhtml_blocks_to_sink(xhtml, Some(&css_rules), &mut sink)
            .map_err(|err| format!("xhtml: {err:?}"))?;
        if paginate(&lines).len() >= page_limit {
            break;
        }
    }

    let pages = paginate(&lines);
    write_report(out_dir, epub_path, &package, &lines, &pages)?;
    for (page_index, page) in pages.iter().take(page_limit).enumerate() {
        let mut fb = Framebuffer::new();
        draw_preview_page(&mut fb, &lines, *page);
        write_pbm(&out_dir.join(format!("epub-page-{page_index:02}.pbm")), &fb)?;
        write_png(&out_dir.join(format!("epub-page-{page_index:02}.png")), &fb)?;
        write_page_text(
            &out_dir.join(format!("epub-page-{page_index:02}.txt")),
            &lines,
            *page,
        )?;
    }

    println!(
        "Wrote {} page previews for '{}' to {}",
        pages.len().min(page_limit),
        package.meta.title,
        out_dir.display()
    );
    Ok(())
}

fn default_device_source_path(epub_path: &Path) -> String {
    epub_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let mut path = String::from("/books/");
            path.push_str(name);
            path
        })
        .unwrap_or_else(|| String::from("/books/book.epub"))
}

fn write_cover_cache_file(
    zip: &ZipArchive<'_>,
    package: &EpubPackage<'_>,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let cover = find_cover_href(package).ok_or("no cover image found in manifest")?;
    let cover_path = resolve_epub_href(package.opf_path, cover);
    let entry = zip
        .entries()
        .flatten()
        .find(|entry| entry.name == cover_path)
        .ok_or_else(|| format!("cover entry not found: {cover_path}"))?;
    let bytes = read_zip_entry(zip, entry)?;
    let image = image::load_from_memory(&bytes)?;
    let gray = image
        .resize_to_fill(
            COVER_WIDTH as u32,
            COVER_HEIGHT as u32,
            image::imageops::FilterType::Triangle,
        )
        .to_luma8();
    let mut bits = [0u8; COVER_BYTES];
    for y in 0..COVER_HEIGHT {
        for x in 0..COVER_WIDTH {
            let luma = gray.get_pixel(x as u32, y as u32)[0];
            if luma < 180 {
                let index = y * COVER_STRIDE + x / 8;
                bits[index] |= 0x80 >> (x & 7);
            }
        }
    }

    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    let mut header = [0u8; proto::cache::COVER_HEADER_BYTES];
    encode_cover_header(CoverCacheHeader::x4_dock_clean(), &mut header).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cover header: {err:?}"),
        )
    })?;
    file.write_all(&header)?;
    file.write_all(&bits)?;
    println!("Wrote cover cache to {}", path.display());
    Ok(())
}

fn find_cover_href<'a>(package: &'a EpubPackage<'a>) -> Option<&'a str> {
    let opf = package.opf_text;
    package
        .manifest
        .iter()
        .find(|item| {
            item.properties
                .of(opf)
                .split_ascii_whitespace()
                .any(|prop| prop == "cover-image")
        })
        .or_else(|| {
            package.manifest.iter().find(|item| {
                item.media_type.of(opf).starts_with("image/")
                    && (item.id.of(opf).eq_ignore_ascii_case("cover")
                        || item.href.of(opf).to_ascii_lowercase().contains("cover"))
            })
        })
        .map(|item| item.href.of(opf))
}

fn read_zip_entry(zip: &ZipArchive<'_>, entry: ZipEntry<'_>) -> Result<Vec<u8>, String> {
    let mut output = vec![0u8; entry.uncompressed_size as usize];
    let len = zip
        .read_entry(entry, &mut output)
        .map_err(|err| format!("zip entry {}: {err:?}", entry.name))?;
    output.truncate(len);
    Ok(output)
}

fn load_css_rules(zip: &ZipArchive<'_>, package: &EpubPackage<'_>, rules: &mut CssRules) {
    rules.clear();
    for item in package.manifest.iter().filter(|item| {
        item.media_type.of(package.opf_text).contains("css")
            || item.href.of(package.opf_text).ends_with(".css")
    }) {
        let path = resolve_epub_href(package.opf_path, item.href.of(package.opf_text));
        let Some(entry) = zip.entries().flatten().find(|entry| entry.name == path) else {
            continue;
        };
        let Ok(bytes) = read_zip_entry(zip, entry) else {
            continue;
        };
        let Ok(css) = std::str::from_utf8(&bytes) else {
            continue;
        };
        parse_css_text_align(css, rules);
    }
}

#[derive(Clone, Copy)]
struct PageSlice {
    first_line: usize,
    line_count: usize,
}

#[derive(Clone)]
struct PreviewLine {
    text: String,
    role: TextRole,
    align: TextAlign,
    style: FontStyle,
    paragraph_end: bool,
    page_break_before: bool,
}

struct PreviewLines {
    lines: Vec<PreviewLine>,
}

impl PreviewLines {
    fn new() -> Self {
        Self { lines: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn force_next_line_to_new_page(&mut self) {
        self.lines.push(PreviewLine {
            text: String::new(),
            role: TextRole::Body,
            align: TextAlign::Justify,
            style: FontStyle::Regular,
            paragraph_end: false,
            page_break_before: true,
        });
    }
}

struct PreviewSink<'a> {
    lines: &'a mut PreviewLines,
    line: String,
    line_role: TextRole,
    line_align: TextAlign,
    line_style: FontStyle,
    pending_space: bool,
    dropping_paragraph: bool,
}

impl<'a> PreviewSink<'a> {
    fn new(lines: &'a mut PreviewLines) -> Self {
        Self {
            lines,
            line: String::new(),
            line_role: TextRole::Body,
            line_align: TextAlign::Justify,
            line_style: FontStyle::Regular,
            pending_space: false,
            dropping_paragraph: false,
        }
    }
}

impl XhtmlBlockSink for PreviewSink<'_> {
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
    ) -> Result<(), XhtmlError> {
        push_styled_fragment(
            self,
            text,
            preview_style_for_proto_style(style, role),
            role,
            align,
            paragraph_end,
        );
        Ok(())
    }
}

fn push_styled_fragment(
    sink: &mut PreviewSink<'_>,
    text: &str,
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    paragraph_end: bool,
) {
    if sink.dropping_paragraph {
        if paragraph_end {
            sink.dropping_paragraph = false;
            sink.pending_space = false;
        }
        return;
    }
    let starts_with_space = text
        .chars()
        .next()
        .map(char::is_whitespace)
        .unwrap_or(false);
    let ends_with_space = text
        .chars()
        .next_back()
        .map(char::is_whitespace)
        .unwrap_or(false);
    let mut normalized = normalize_text(text);
    if !sanitize_preview_block(&mut normalized) {
        sink.dropping_paragraph = !paragraph_end;
        sink.pending_space = false;
        return;
    }
    if normalized.is_empty() {
        sink.pending_space |= starts_with_space || ends_with_space;
        if paragraph_end {
            flush_preview_line(sink, true);
        }
        return;
    }

    normalize_decorative_separator(&mut normalized);
    let align = block_align_for(align, &normalized, role);
    let x = reader_x_for(role);
    let max_x = READER_RIGHT_X;

    if !sink.line.is_empty() && (sink.line_role != role || sink.line_align != align) {
        flush_preview_line(sink, false);
    }
    if sink.line.is_empty() {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = FontStyle::Regular;
    }

    let mut first_word = true;
    for word in normalized.split_whitespace() {
        let attach = is_leading_punctuation_word(word) && !sink.line.is_empty();
        let leading_space = !sink.line.is_empty()
            && !attach
            && (sink.pending_space || !first_word || starts_with_space);
        let mut candidate = sink.line.clone();
        append_styled_word(&mut candidate, word, style, leading_space);
        if !sink.line.is_empty()
            && styled_text_ink_width(&candidate, TypeSettings::DEFAULT, style) + x
                + READER_WRAP_SAFETY
                > max_x
        {
            flush_preview_line(sink, false);
            append_styled_word(&mut sink.line, word, style, false);
        } else {
            sink.line = candidate;
        }
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = style;
        sink.pending_space = false;
        first_word = false;
    }

    sink.pending_space |= ends_with_space;
    if paragraph_end {
        flush_preview_line(sink, true);
    }
}

fn append_styled_word(line: &mut String, word: &str, style: FontStyle, leading_space: bool) {
    if leading_space {
        line.push(' ');
    }
    line.push(STYLE_MARKER);
    line.push(style_marker_code(style));
    line.push_str(word);
}

fn flush_preview_line(sink: &mut PreviewSink<'_>, paragraph_end: bool) {
    if sink.line.is_empty() {
        if paragraph_end {
            if let Some(previous) = sink.lines.lines.last_mut() {
                previous.paragraph_end = true;
            }
        }
        return;
    }
    if sink.lines.lines.len() < MAX_PREVIEW_LINES {
        sink.lines.lines.push(PreviewLine {
            text: sink.line.clone(),
            role: sink.line_role,
            align: sink.line_align,
            style: first_styled_line_style(&sink.line).unwrap_or(FontStyle::Regular),
            paragraph_end,
            page_break_before: false,
        });
    }
    sink.line.clear();
    sink.line_style = FontStyle::Regular;
    sink.pending_space = false;
}

fn paginate(lines: &PreviewLines) -> Vec<PageSlice> {
    let mut pages = Vec::new();
    let mut first_line = 0usize;
    let mut line_count = 0usize;
    let mut y = PAGE_TOP;

    for (index, line) in lines.lines.iter().enumerate() {
        if line.text.is_empty() && line.page_break_before {
            if line_count > 0 {
                pages.push(PageSlice {
                    first_line,
                    line_count,
                });
            }
            first_line = index + 1;
            line_count = 0;
            y = PAGE_TOP;
            continue;
        }

        let height = line_advance(TypeSettings::DEFAULT, line.role) + paragraph_gap_after(line);
        if (line.page_break_before || y + height > PAGE_BOTTOM) && line_count > 0 {
            pages.push(PageSlice {
                first_line,
                line_count,
            });
            first_line = index;
            line_count = 0;
            y = PAGE_TOP;
        }
        line_count += 1;
        y += height;
    }

    if line_count > 0 {
        pages.push(PageSlice {
            first_line,
            line_count,
        });
    }
    pages
}

fn draw_preview_page(fb: &mut Framebuffer, lines: &PreviewLines, page: PageSlice) {
    let mut y = PAGE_TOP + 16;
    for index in page.first_line..page.first_line + page.line_count {
        let Some(line) = lines.lines.get(index) else {
            break;
        };
        if line.text.is_empty() {
            continue;
        }
        let x = match line.align {
            TextAlign::Center => {
                let width = styled_text_ink_width(&line.text, TypeSettings::DEFAULT, line.style)
                    .min(READER_RIGHT_X - READER_LEFT_X);
                ((WIDTH as i16 - width) / 2).max(READER_LEFT_X)
            }
            TextAlign::Left | TextAlign::Justify => reader_x_for(line.role),
        };
        draw_styled_line(fb, TypeSettings::DEFAULT, &line.text, x, y, line.style);
        y += line_advance(TypeSettings::DEFAULT, line.role) + paragraph_gap_after(line);
    }
}

fn write_report(
    out_dir: &Path,
    epub_path: &Path,
    package: &EpubPackage<'_>,
    lines: &PreviewLines,
    pages: &[PageSlice],
) -> std::io::Result<()> {
    let mut file = BufWriter::new(File::create(out_dir.join("epub-report.txt"))?);
    writeln!(file, "source: {}", epub_path.display())?;
    writeln!(file, "title: {}", package.meta.title)?;
    writeln!(file, "author: {}", package.meta.author)?;
    writeln!(file, "spine items: {}", package.spine.len())?;
    writeln!(file, "rendered lines: {}", lines.lines.len())?;
    writeln!(file, "pages: {}", pages.len())?;
    writeln!(file)?;
    for (index, page) in pages.iter().take(12).enumerate() {
        writeln!(
            file,
            "page {index}: lines {}..{}",
            page.first_line,
            page.first_line + page.line_count
        )?;
    }
    Ok(())
}

fn write_page_text(path: &Path, lines: &PreviewLines, page: PageSlice) -> std::io::Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    for index in page.first_line..page.first_line + page.line_count {
        let Some(line) = lines.lines.get(index) else {
            break;
        };
        if line.text.is_empty() {
            continue;
        }
        writeln!(
            file,
            "[{:?} {:?} {:?}] {}",
            line.role,
            line.align,
            line.style,
            styled_text_snapshot(&line.text)
        )?;
    }
    Ok(())
}

fn write_static_previews(out: &Path) -> std::io::Result<()> {
    write_shell_preview(out, "home", UiView::Home, 0)?;
    write_landscape_home_mockups(out)?;
    write_shell_preview(out, "files", UiView::Library, 1)?;
    write_reading(&out.join("reading.pbm"))?;
    write_chapters(&out.join("chapters.pbm"))?;
    write_shell_preview(out, "chapters-ui", UiView::Chapters, 3)?;
    write_shell_preview(out, "settings", UiView::Settings, 1)?;
    Ok(())
}

fn write_landscape_home_mockups(out: &Path) -> std::io::Result<()> {
    let variants = [
        ("home-landscape-rail", LandscapeHomeVariant::Rail),
        ("home-landscape-rail-quiet", LandscapeHomeVariant::RailQuiet),
        ("home-landscape-rail-cover", LandscapeHomeVariant::RailCover),
        (
            "home-landscape-rail-hardware",
            LandscapeHomeVariant::RailHardware,
        ),
        (
            "home-landscape-skeuo-buttons",
            LandscapeHomeVariant::SkeuoButtons,
        ),
        (
            "home-landscape-skeuo-bookplate",
            LandscapeHomeVariant::SkeuoBookplate,
        ),
        (
            "home-landscape-skeuo-library",
            LandscapeHomeVariant::SkeuoLibrary,
        ),
        ("home-landscape-ive-pure", LandscapeHomeVariant::IvePure),
        ("home-landscape-ive-glass", LandscapeHomeVariant::IveGlass),
        ("home-landscape-ive-object", LandscapeHomeVariant::IveObject),
        (
            "home-landscape-hybrid-soft",
            LandscapeHomeVariant::HybridSoft,
        ),
        (
            "home-landscape-hybrid-wells",
            LandscapeHomeVariant::HybridWells,
        ),
        (
            "home-landscape-hybrid-slab",
            LandscapeHomeVariant::HybridSlab,
        ),
        (
            "home-landscape-affordance-edge",
            LandscapeHomeVariant::AffordanceEdge,
        ),
        (
            "home-landscape-affordance-connectors",
            LandscapeHomeVariant::AffordanceConnectors,
        ),
        (
            "home-landscape-affordance-dock",
            LandscapeHomeVariant::AffordanceDock,
        ),
        (
            "home-landscape-dock-refined",
            LandscapeHomeVariant::DockRefined,
        ),
        ("home-landscape-dock-paper", LandscapeHomeVariant::DockPaper),
        (
            "home-landscape-dock-widebook",
            LandscapeHomeVariant::DockWideBook,
        ),
        ("home-landscape-dock-clean", LandscapeHomeVariant::DockClean),
        ("home-landscape-dock-open", LandscapeHomeVariant::DockOpen),
        ("home-landscape-dock-panel", LandscapeHomeVariant::DockPanel),
        ("home-landscape-tabs", LandscapeHomeVariant::Tabs),
        ("home-landscape-book", LandscapeHomeVariant::BookFirst),
    ];
    for (name, variant) in variants {
        let mut fb = Framebuffer::new();
        draw_landscape_home(&mut fb, variant);
        write_pbm(&out.join(format!("{name}.pbm")), &fb)?;
        write_png(&out.join(format!("{name}.png")), &fb)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum LandscapeHomeVariant {
    Rail,
    RailQuiet,
    RailCover,
    RailHardware,
    SkeuoButtons,
    SkeuoBookplate,
    SkeuoLibrary,
    IvePure,
    IveGlass,
    IveObject,
    HybridSoft,
    HybridWells,
    HybridSlab,
    AffordanceEdge,
    AffordanceConnectors,
    AffordanceDock,
    DockRefined,
    DockPaper,
    DockWideBook,
    DockClean,
    DockOpen,
    DockPanel,
    Tabs,
    BookFirst,
}

/// Centers an 800x480-authored design (the X4 panel) on the actual
/// framebuffer, clipping the outermost margins; a no-op on the X4. Used by the
/// `--design-mockups` studies, which are absolute-positioned. The landscape
/// home mockups instead reflow to fill the panel (see `ls_x`/`ls_y`).
pub(crate) fn fit_design_to_board(fb: &mut Framebuffer) {
    const DESIGN_W: i16 = 800;
    const DESIGN_H: i16 = 480;
    // The design is drawn straight into `fb` at 800x480 coordinates, so on a
    // board narrower/shorter than the design, Framebuffer::set_pixel already
    // silently dropped the excess before this function ever runs — there's
    // nothing left on that axis to center. Only shift on axes where the
    // board has *extra* room to distribute; shifting on a clipped axis would
    // just crop a second time.
    let dx = (WIDTH as i16 - DESIGN_W).max(0) / 2;
    let dy = (HEIGHT as i16 - DESIGN_H).max(0) / 2;
    if dx == 0 && dy == 0 {
        return;
    }
    let mut src = vec![true; WIDTH * HEIGHT];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            src[y * WIDTH + x] = fb.pixel(x, y);
        }
    }
    fb.clear(true);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let tx = x as i16 + dx;
            let ty = y as i16 + dy;
            if tx >= 0 && ty >= 0 && (tx as usize) < WIDTH && (ty as usize) < HEIGHT {
                fb.set_pixel(tx as usize, ty as usize, src[y * WIDTH + x]);
            }
        }
    }
}

fn draw_landscape_home(fb: &mut Framebuffer, variant: LandscapeHomeVariant) {
    fb.clear(true);
    match variant {
        LandscapeHomeVariant::Rail => draw_landscape_home_rail(fb),
        LandscapeHomeVariant::RailQuiet => draw_landscape_home_rail_quiet(fb),
        LandscapeHomeVariant::RailCover => draw_landscape_home_rail_cover(fb),
        LandscapeHomeVariant::RailHardware => draw_landscape_home_rail_hardware(fb),
        LandscapeHomeVariant::SkeuoButtons => draw_landscape_home_skeuo_buttons(fb),
        LandscapeHomeVariant::SkeuoBookplate => draw_landscape_home_skeuo_bookplate(fb),
        LandscapeHomeVariant::SkeuoLibrary => draw_landscape_home_skeuo_library(fb),
        LandscapeHomeVariant::IvePure => draw_landscape_home_ive_pure(fb),
        LandscapeHomeVariant::IveGlass => draw_landscape_home_ive_glass(fb),
        LandscapeHomeVariant::IveObject => draw_landscape_home_ive_object(fb),
        LandscapeHomeVariant::HybridSoft => draw_landscape_home_hybrid_soft(fb),
        LandscapeHomeVariant::HybridWells => draw_landscape_home_hybrid_wells(fb),
        LandscapeHomeVariant::HybridSlab => draw_landscape_home_hybrid_slab(fb),
        LandscapeHomeVariant::AffordanceEdge => draw_landscape_home_affordance_edge(fb),
        LandscapeHomeVariant::AffordanceConnectors => {
            draw_landscape_home_affordance_connectors(fb);
        }
        LandscapeHomeVariant::AffordanceDock => draw_landscape_home_affordance_dock(fb),
        LandscapeHomeVariant::DockRefined => draw_landscape_home_dock_refined(fb),
        LandscapeHomeVariant::DockPaper => draw_landscape_home_dock_paper(fb),
        LandscapeHomeVariant::DockWideBook => draw_landscape_home_dock_widebook(fb),
        LandscapeHomeVariant::DockClean => draw_landscape_home_dock_clean(fb),
        LandscapeHomeVariant::DockOpen => draw_landscape_home_dock_open(fb),
        LandscapeHomeVariant::DockPanel => draw_landscape_home_dock_panel(fb),
        LandscapeHomeVariant::Tabs => draw_landscape_home_tabs(fb),
        LandscapeHomeVariant::BookFirst => draw_landscape_home_book_first(fb),
    }
}

// The landscape home mockups below are authored in an 800x480 design space
// (the X4 panel). Every draw in them routes through these axis scalers, so the
// whole composition reflows to fill the actual panel: on the X3 the extra
// height becomes taller covers, rails, and spacing rather than a letterbox.
// The scale is identity on the X4, so its mockups render byte-for-byte as
// before. (The centering fallback in `fit_design_to_board` is still used by the
// unscaled `--design-mockups` studies.)
fn ls_x(v: i32) -> i32 {
    (v * WIDTH as i32 + 400) / 800
}
fn ls_y(v: i32) -> i32 {
    (v * HEIGHT as i32 + 240) / 480
}
fn lx(v: u16) -> u16 {
    ls_x(v as i32) as u16
}
fn ly(v: u16) -> u16 {
    ls_y(v as i32) as u16
}
fn lxi(v: i16) -> i16 {
    ls_x(v as i32) as i16
}
fn lyi(v: i16) -> i16 {
    ls_y(v as i32) as i16
}
fn l_rect(fb: &mut Framebuffer, r: Rect, white: bool) {
    fill_rect(fb, Rect::new(lx(r.x), ly(r.y), lx(r.w), ly(r.h)), white);
}
fn l_text(fb: &mut Framebuffer, font: &BitmapFont, text: &str, x: i16, y: i16, white: bool) -> i16 {
    draw_text(fb, font, text, lxi(x), lyi(y), white)
}

fn draw_landscape_home_rail(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let small_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 24, 82);
    l_rect(fb, Rect::new(316, 28, 1, 424), false);
    draw_landscape_action_stack(fb, 28, 56, 248, 344, 0);
    draw_cover_art(fb, 430, 48, 205, 306);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 532, 388);
    draw_text_centered(fb, body_font, "Daniel Keyes", 532, 418);
    draw_thin_progress(fb, 462, 442, 140, 420);
    l_text(fb, small_font, "42%", 612, 450, false);
}

fn draw_landscape_home_rail_quiet(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 22, 82);
    draw_landscape_action_stack_quiet(fb, 34, 62, 224, 332, 0);
    l_rect(fb, Rect::new(292, 54, 1, 360), false);
    draw_cover_art(fb, 438, 44, 194, 290);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 535, 376);
    draw_text_centered(fb, body_font, "Daniel Keyes", 535, 406);
    draw_thin_progress(fb, 480, 438, 112, 420);
    l_text(fb, body_font, "42%", 604, 446, false);
}

fn draw_landscape_home_rail_cover(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 22, 82);
    draw_landscape_action_stack_quiet(fb, 26, 74, 202, 308, 0);
    l_rect(fb, Rect::new(260, 42, 1, 390), false);
    draw_cover_art(fb, 376, 28, 236, 354);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 494, 420);
    draw_text_centered(fb, body_font, "Daniel Keyes", 494, 450);
    draw_thin_progress(fb, 642, 206, 90, 420);
    l_text(fb, body_font, "42%", 666, 238, false);
}

fn draw_landscape_home_rail_hardware(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 22, 82);
    draw_landscape_action_stack_hardware(fb, 20, 36, 266, 396, 0);
    l_rect(fb, Rect::new(320, 28, 1, 424), false);
    draw_cover_art(fb, 444, 42, 206, 308);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 547, 388);
    draw_text_centered(fb, body_font, "Daniel Keyes", 547, 418);
    draw_thin_progress(fb, 492, 448, 110, 420);
    l_text(fb, body_font, "42%", 614, 456, false);
}

fn draw_landscape_home_skeuo_buttons(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 22, 82);
    draw_skeuo_button_stack(fb, 26, 52, 246, 352);
    draw_paper_gutter(fb, 308, 34, 392);
    draw_cover_art_skeuo(fb, 426, 46, 208, 312);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 530, 394);
    draw_text_centered(fb, body_font, "Daniel Keyes", 530, 424);
    draw_thin_progress(fb, 474, 452, 112, 420);
    l_text(fb, body_font, "42%", 598, 460, false);
}

fn draw_landscape_home_skeuo_bookplate(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 22, 82);
    draw_paper_panel(fb, 20, 44, 270, 364);
    draw_bookplate_actions(fb, 42, 82, 226);
    draw_cover_art_skeuo(fb, 424, 34, 224, 336);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 536, 412);
    draw_text_centered(fb, body_font, "Daniel Keyes", 536, 442);
    draw_thin_progress(fb, 662, 214, 70, 420);
    l_text(fb, body_font, "42%", 672, 248, false);
}

fn draw_landscape_home_skeuo_library(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 22, 82);
    draw_spine_actions(fb, 18, 42, 274, 386);
    draw_paper_gutter(fb, 326, 36, 400);
    draw_cover_art_skeuo(fb, 430, 56, 194, 291);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 527, 388);
    draw_text_centered(fb, body_font, "Daniel Keyes", 527, 418);
    draw_thin_progress(fb, 474, 446, 106, 420);
    l_text(fb, body_font, "42%", 592, 454, false);
}

fn draw_landscape_home_ive_pure(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_ive_action_rail(fb, 56, 86, 204, 300);
    l_rect(fb, Rect::new(312, 84, 1, 300), false);
    draw_cover_art_minimal(fb, 454, 58, 178, 267);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 543, 374);
    draw_text_centered(fb, body_font, "Daniel Keyes", 543, 404);
    draw_ive_progress(fb, 496, 438, 94, 420);
}

fn draw_landscape_home_ive_glass(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_ive_glass_rail(fb, 34, 58, 250, 340);
    l_rect(fb, Rect::new(328, 52, 1, 360), false);
    l_rect(fb, Rect::new(334, 88, 1, 288), false);
    draw_cover_art_minimal(fb, 448, 52, 190, 285);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 543, 386);
    draw_text_centered(fb, body_font, "Daniel Keyes", 543, 416);
    draw_ive_progress(fb, 488, 450, 110, 420);
}

fn draw_landscape_home_ive_object(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_ive_action_rail(fb, 50, 104, 176, 264);
    l_rect(fb, Rect::new(270, 68, 1, 344), false);
    draw_cover_art_minimal(fb, 388, 34, 232, 348);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 504, 424);
    draw_text_centered(fb, body_font, "Daniel Keyes", 504, 454);
    draw_ive_progress(fb, 654, 206, 78, 420);
}

fn draw_landscape_home_hybrid_soft(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_hybrid_soft_rail(fb, 34, 58, 250, 340);
    l_rect(fb, Rect::new(326, 52, 1, 360), false);
    draw_cover_art_minimal(fb, 448, 50, 196, 294);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 546, 390);
    draw_text_centered(fb, body_font, "Daniel Keyes", 546, 420);
    draw_ive_progress(fb, 490, 452, 112, 420);
}

fn draw_landscape_home_hybrid_wells(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_hybrid_well_rail(fb, 38, 66, 238, 324);
    l_rect(fb, Rect::new(318, 66, 1, 324), false);
    l_rect(fb, Rect::new(324, 96, 1, 264), false);
    draw_cover_art_minimal(fb, 452, 54, 184, 276);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 544, 384);
    draw_text_centered(fb, body_font, "Daniel Keyes", 544, 414);
    draw_ive_progress(fb, 492, 448, 104, 420);
}

fn draw_landscape_home_hybrid_slab(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_hybrid_slab_rail(fb, 24, 54, 260, 348);
    l_rect(fb, Rect::new(326, 54, 1, 348), false);
    draw_cover_art_minimal(fb, 444, 42, 208, 312);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 548, 398);
    draw_text_centered(fb, body_font, "Daniel Keyes", 548, 428);
    draw_ive_progress(fb, 496, 458, 104, 420);
}

fn draw_landscape_home_affordance_edge(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_affordance_edge_rail(fb, 32, 58, 252, 340);
    l_rect(fb, Rect::new(326, 52, 1, 360), false);
    draw_cover_art_minimal(fb, 448, 50, 196, 294);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 546, 390);
    draw_text_centered(fb, body_font, "Daniel Keyes", 546, 420);
    draw_ive_progress(fb, 490, 452, 112, 420);
}

fn draw_landscape_home_affordance_connectors(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_affordance_connector_rail(fb, 26, 64, 258, 328);
    l_rect(fb, Rect::new(326, 58, 1, 348), false);
    draw_cover_art_minimal(fb, 448, 50, 196, 294);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 546, 390);
    draw_text_centered(fb, body_font, "Daniel Keyes", 546, 420);
    draw_ive_progress(fb, 490, 452, 112, 420);
}

fn draw_landscape_home_affordance_dock(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_affordance_dock_rail(fb, 28, 56, 258, 344);
    l_rect(fb, Rect::new(328, 52, 1, 360), false);
    draw_cover_art_minimal(fb, 448, 50, 196, 294);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 546, 390);
    draw_text_centered(fb, body_font, "Daniel Keyes", 546, 420);
    draw_ive_progress(fb, 490, 452, 112, 420);
}

fn draw_landscape_home_dock_refined(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_refined_dock_rail(fb, 30, 58, 258, 340, DockRailStyle::Plain);
    draw_section_divider(fb, 330, 58, 340);
    draw_cover_art_varied(fb, 448, 48, 202, 303);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 549, 394);
    draw_text_centered(fb, body_font, "Daniel Keyes", 549, 424);
    draw_ive_progress(fb, 494, 454, 110, 420);
}

fn draw_landscape_home_dock_paper(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_refined_dock_rail(fb, 28, 54, 268, 350, DockRailStyle::Paper);
    draw_section_divider(fb, 334, 54, 350);
    draw_cover_art_varied(fb, 446, 50, 198, 297);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 545, 392);
    draw_text_centered(fb, body_font, "Daniel Keyes", 545, 422);
    draw_ive_progress(fb, 492, 452, 106, 420);
}

fn draw_landscape_home_dock_widebook(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_refined_dock_rail(fb, 28, 68, 240, 318, DockRailStyle::Plain);
    draw_section_divider(fb, 306, 68, 318);
    draw_cover_art_varied(fb, 424, 34, 226, 339);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 537, 414);
    draw_text_centered(fb, body_font, "Daniel Keyes", 537, 444);
    draw_ive_progress(fb, 642, 214, 76, 420);
}

fn draw_landscape_home_dock_clean(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_refined_dock_rail(fb, 30, 58, 258, 340, DockRailStyle::Clean);
    draw_section_divider(fb, 330, 58, 340);
    draw_cover_art_varied(fb, 448, 48, 202, 303);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 549, 394);
    draw_text_centered(fb, body_font, "Daniel Keyes", 549, 424);
    draw_ive_progress(fb, 494, 454, 110, 420);
}

fn draw_landscape_home_dock_open(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_refined_dock_rail(fb, 30, 62, 250, 332, DockRailStyle::Open);
    draw_section_divider(fb, 326, 62, 332);
    draw_cover_art_varied(fb, 448, 48, 202, 303);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 549, 394);
    draw_text_centered(fb, body_font, "Daniel Keyes", 549, 424);
    draw_ive_progress(fb, 494, 454, 110, 420);
}

fn draw_landscape_home_dock_panel(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, 82);
    draw_refined_dock_rail(fb, 28, 54, 268, 350, DockRailStyle::Panel);
    draw_section_divider(fb, 334, 54, 350);
    draw_cover_art_varied(fb, 446, 50, 198, 297);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 545, 392);
    draw_text_centered(fb, body_font, "Daniel Keyes", 545, 422);
    draw_ive_progress(fb, 492, 452, 106, 420);
}

fn draw_landscape_home_tabs(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 24, 82);
    draw_cover_art(fb, 468, 50, 190, 284);
    l_text(fb, title_font, "Flowers for Algernon", 58, 82, false);
    l_text(fb, body_font, "Daniel Keyes", 58, 116, false);
    draw_thin_progress(fb, 60, 146, 232, 420);
    l_text(fb, body_font, "42%", 306, 153, false);
    l_rect(fb, Rect::new(58, 198, 318, 1), false);
    l_text(fb, body_font, "Current book", 58, 238, false);
    l_text(fb, body_font, "Chapter 7", 58, 272, false);
    draw_bottom_tabs(fb, 16, 416, 768, 52, 0);
}

fn draw_landscape_action_stack_quiet(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    selected: usize,
) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        if index == selected {
            l_rect(fb, Rect::new(x, row_y + row_h - 8, w, 3), false);
            l_rect(fb, Rect::new(x, row_y + 12, 4, row_h - 26), false);
        }
        let text_x = x as i16 + 22;
        let text_y = row_y as i16 + row_h as i16 / 2 + 8;
        l_text(fb, font, label, text_x, text_y, false);
        l_rect(
            fb,
            Rect::new(x + w - 34, row_y + row_h / 2 - 1, 24, 2),
            false,
        );
    }
}

fn draw_landscape_action_stack_hardware(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    selected: usize,
) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        let selected_row = index == selected;
        if selected_row {
            l_rect(fb, Rect::new(x, row_y, w, row_h - 6), false);
        } else {
            stroke_rect_direct(fb, x, row_y, w, row_h - 6);
        }
        let text_x = x as i16 + 28;
        let text_y = row_y as i16 + row_h as i16 / 2 + 7;
        l_text(fb, font, label, text_x, text_y, selected_row);
        l_rect(
            fb,
            Rect::new(x + w - 40, row_y + row_h / 2 - 1, 24, 2),
            selected_row,
        );
    }
}

fn draw_skeuo_button_stack(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        draw_recessed_slot(fb, x, row_y + 6, w, row_h - 16);
        l_text(
            fb,
            font,
            label,
            x as i16 + 26,
            row_y as i16 + row_h as i16 / 2 + 7,
            false,
        );
        draw_button_well(fb, x + w - 44, row_y + row_h / 2 - 11);
    }
}

fn draw_bookplate_actions(fb: &mut Framebuffer, x: u16, y: u16, w: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * 72;
        l_rect(fb, Rect::new(x + 8, row_y, w - 16, 1), false);
        l_text(fb, font, label, x as i16 + 26, row_y as i16 + 42, false);
        draw_button_well(fb, x + w - 48, row_y + 22);
    }
    l_rect(fb, Rect::new(x + 8, y + 288, w - 16, 1), false);
}

fn draw_spine_actions(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        l_rect(fb, Rect::new(x + 8, row_y + 4, w - 24, row_h - 12), false);
        stroke_rect_direct(fb, x + 12, row_y + 8, w - 32, row_h - 20);
        l_rect(fb, Rect::new(x + 24, row_y + 12, 1, row_h - 28), true);
        l_text(
            fb,
            font,
            label,
            x as i16 + 44,
            row_y as i16 + row_h as i16 / 2 + 7,
            true,
        );
        l_rect(
            fb,
            Rect::new(x + w - 58, row_y + row_h / 2 - 1, 26, 2),
            true,
        );
    }
}

fn draw_ive_action_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        l_text(
            fb,
            font,
            label,
            x as i16,
            row_y as i16 + row_h as i16 / 2 + 8,
            false,
        );
        l_rect(fb, Rect::new(x + w - 20, row_y + row_h / 2, 20, 1), false);
    }
}

fn draw_ive_glass_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 8, y + 8, w - 16, 1), false);
    l_rect(fb, Rect::new(x + 8, y + h - 10, w - 16, 1), false);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        if index > 0 {
            l_rect(fb, Rect::new(x + 22, row_y, w - 44, 1), false);
        }
        l_text(
            fb,
            font,
            label,
            x as i16 + 32,
            row_y as i16 + row_h as i16 / 2 + 8,
            false,
        );
        l_rect(fb, Rect::new(x + w - 48, row_y + row_h / 2, 24, 1), false);
    }
}

fn draw_hybrid_soft_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 10, y + 10, w - 20, 1), false);
    l_rect(fb, Rect::new(x + 10, y + h - 12, w - 20, 1), false);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        if index > 0 {
            l_rect(fb, Rect::new(x + 24, row_y, w - 48, 1), false);
        }
        l_text(
            fb,
            font,
            label,
            x as i16 + 30,
            row_y as i16 + row_h as i16 / 2 + 8,
            false,
        );
        draw_mini_recess(fb, x + w - 52, row_y + row_h / 2 - 9, 30, 18);
    }
}

fn draw_hybrid_well_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        draw_soft_slot(fb, x, row_y + 8, w, row_h - 16);
        l_text(
            fb,
            font,
            label,
            x as i16 + 30,
            row_y as i16 + row_h as i16 / 2 + 8,
            false,
        );
        draw_mini_recess(fb, x + w - 50, row_y + row_h / 2 - 9, 30, 18);
    }
}

fn draw_hybrid_slab_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    stroke_rect_direct(fb, x, y, w, h);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        if index > 0 {
            l_rect(fb, Rect::new(x, row_y, w, 1), false);
        }
        l_rect(fb, Rect::new(x + 10, row_y + 10, 1, row_h - 20), false);
        l_text(
            fb,
            font,
            label,
            x as i16 + 32,
            row_y as i16 + row_h as i16 / 2 + 8,
            false,
        );
        draw_mini_recess(fb, x + w - 52, row_y + row_h / 2 - 9, 30, 18);
    }
}

fn draw_affordance_edge_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    stroke_rect_direct(fb, x, y, w, h);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        if index > 0 {
            l_rect(fb, Rect::new(x + 28, row_y, w - 56, 1), false);
        }
        draw_edge_button_mark(fb, x + 8, row_y + row_h / 2 - 18);
        l_text(
            fb,
            font,
            label,
            x as i16 + 54,
            row_y as i16 + row_h as i16 / 2 + 8,
            false,
        );
        l_rect(fb, Rect::new(x + w - 42, row_y + row_h / 2, 24, 1), false);
    }
}

fn draw_affordance_connector_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        let center_y = row_y + row_h / 2;
        draw_edge_button_mark(fb, x, center_y - 18);
        l_rect(fb, Rect::new(x + 34, center_y, 34, 1), false);
        l_rect(fb, Rect::new(x + 68, center_y - 10, 1, 20), false);
        l_text(fb, font, label, x as i16 + 86, center_y as i16 + 8, false);
        draw_mini_recess(fb, x + w - 38, center_y - 9, 30, 18);
    }
}

fn draw_affordance_dock_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    draw_soft_slot(fb, x, y, w, h);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        let center_y = row_y + row_h / 2;
        if index > 0 {
            l_rect(fb, Rect::new(x + 22, row_y, w - 44, 1), false);
        }
        draw_left_dock_notch(fb, x + 8, center_y - 14);
        l_text(fb, font, label, x as i16 + 44, center_y as i16 + 8, false);
        draw_mini_recess(fb, x + w - 48, center_y - 9, 30, 18);
    }
}

#[derive(Clone, Copy)]
enum DockRailStyle {
    Plain,
    Paper,
    Clean,
    Open,
    Panel,
}

fn draw_refined_dock_rail(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    style: DockRailStyle,
) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    match style {
        DockRailStyle::Plain => {
            l_rect(fb, Rect::new(x + 8, y, w - 28, 1), false);
            l_rect(fb, Rect::new(x + 18, y + h - 1, w - 46, 1), false);
            l_rect(fb, Rect::new(x, y + 14, 1, h - 28), false);
            l_rect(fb, Rect::new(x + w - 1, y + 38, 1, h - 76), false);
        }
        DockRailStyle::Paper => {
            stroke_rect_direct(fb, x, y, w, h);
            l_rect(fb, Rect::new(x + 10, y + 8, w - 36, 1), false);
            l_rect(fb, Rect::new(x + 16, y + h - 10, w - 52, 1), false);
            l_rect(fb, Rect::new(x + 6, y + 34, 1, h - 68), false);
        }
        DockRailStyle::Clean => {
            stroke_rect_direct(fb, x, y, w, h);
        }
        DockRailStyle::Open => {
            l_rect(fb, Rect::new(x, y + 12, 1, h - 24), false);
            l_rect(fb, Rect::new(x + w - 1, y + 22, 1, h - 44), false);
        }
        DockRailStyle::Panel => {
            stroke_rect_direct(fb, x, y, w, h);
            l_rect(fb, Rect::new(x + 8, y + 8, w - 16, 1), false);
            l_rect(fb, Rect::new(x + 8, y + h - 10, w - 16, 1), false);
        }
    }
    let row_h = h / labels.len() as u16;
    let separator_lengths = [180u16, 206, 168];
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        let center_y = row_y + row_h / 2;
        if index > 0 {
            let sep_w = separator_lengths[index - 1].min(w.saturating_sub(58));
            let sep_x = x + 22 + (index as u16 % 2) * 10;
            l_rect(fb, Rect::new(sep_x, row_y, sep_w, 1), false);
        }
        draw_refined_left_notch(fb, x + 10, center_y - 15, index);
        l_text(fb, font, label, x as i16 + 46, center_y as i16 + 8, false);
        draw_refined_button_well(fb, x + w - 48, center_y - 9, index);
    }
}

fn draw_refined_left_notch(fb: &mut Framebuffer, x: u16, y: u16, index: usize) {
    let stem_h = [30u16, 24, 28, 22][index.min(3)];
    let arm_w = [18u16, 14, 20, 16][index.min(3)];
    l_rect(fb, Rect::new(x, y + (30 - stem_h) / 2, 3, stem_h), false);
    l_rect(fb, Rect::new(x + 6, y + 15, arm_w, 1), false);
    if index.is_multiple_of(2) {
        l_rect(fb, Rect::new(x + 6, y + 7, 1, 16), false);
    }
}

fn draw_refined_button_well(fb: &mut Framebuffer, x: u16, y: u16, index: usize) {
    let widths = [28u16, 24, 30, 26];
    let w = widths[index.min(3)];
    stroke_rect_direct(fb, x + (30 - w), y, w, 18);
    l_rect(fb, Rect::new(x + (30 - w) + 5, y + 5, w - 10, 1), false);
    if index != 1 {
        l_rect(fb, Rect::new(x + (30 - w) + 5, y + 12, w - 10, 1), false);
    }
}

fn draw_section_divider(fb: &mut Framebuffer, x: u16, y: u16, h: u16) {
    l_rect(fb, Rect::new(x, y, 1, h), false);
    l_rect(fb, Rect::new(x + 5, y + 34, 1, h - 68), false);
}

fn draw_edge_button_mark(fb: &mut Framebuffer, x: u16, y: u16) {
    stroke_rect_direct(fb, x, y, 28, 36);
    l_rect(fb, Rect::new(x + 5, y + 8, 18, 2), false);
    l_rect(fb, Rect::new(x + 5, y + 26, 18, 1), false);
}

fn draw_left_dock_notch(fb: &mut Framebuffer, x: u16, y: u16) {
    l_rect(fb, Rect::new(x, y, 3, 28), false);
    l_rect(fb, Rect::new(x + 6, y + 5, 1, 18), false);
    l_rect(fb, Rect::new(x + 6, y + 14, 18, 1), false);
}

fn draw_soft_slot(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    l_rect(fb, Rect::new(x + 8, y, w - 16, 1), false);
    l_rect(fb, Rect::new(x + 8, y + h - 1, w - 16, 1), false);
    l_rect(fb, Rect::new(x, y + 8, 1, h - 16), false);
    l_rect(fb, Rect::new(x + w - 1, y + 8, 1, h - 16), false);
}

fn draw_mini_recess(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 5, y + 5, w - 10, 1), false);
    l_rect(fb, Rect::new(x + 5, y + h - 6, w - 10, 1), false);
}

fn draw_cover_art_minimal(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 12, y + 14, w - 24, 1), false);
    l_rect(fb, Rect::new(x + 22, y + 44, w - 44, 2), false);
    l_rect(fb, Rect::new(x + 32, y + 72, w - 64, 1), false);
    for row in 0..6 {
        let yy = y + 108 + row * 22;
        let inset = 26 + (row % 2) * 12;
        l_rect(fb, Rect::new(x + inset, yy, w - inset * 2, 2), false);
    }
    l_rect(fb, Rect::new(x + 26, y + h - 40, w - 52, 1), false);
}

fn draw_cover_art_varied(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 12, y + 14, w - 24, 1), false);
    l_rect(fb, Rect::new(x + 24, y + 42, w - 56, 2), false);
    l_rect(fb, Rect::new(x + 34, y + 70, w - 72, 1), false);
    let line_specs = [
        (104u16, 30u16, 122u16, 3u16),
        (126, 44, 86, 2),
        (148, 26, 138, 3),
        (172, 58, 74, 2),
        (194, 38, 112, 2),
        (220, 50, 96, 3),
        (246, 28, 130, 1),
    ];
    for (dy, inset, line_w, line_h) in line_specs {
        if dy + 8 < h {
            l_rect(
                fb,
                Rect::new(
                    x + inset,
                    y + dy,
                    line_w.min(w.saturating_sub(inset + 18)),
                    line_h,
                ),
                false,
            );
        }
    }
    l_rect(fb, Rect::new(x + 30, y + h - 48, w - 72, 1), false);
    l_rect(fb, Rect::new(x + 42, y + h - 34, w - 104, 2), false);
}

fn draw_battery_landscape_minimal(fb: &mut Framebuffer, x: u16, y: u16, percent: u8) {
    stroke_rect_direct(fb, x, y, 38, 16);
    l_rect(fb, Rect::new(x + 38, y + 5, 3, 6), false);
    let fill_w = ((percent.min(100) as u16 * 30) / 100).max(1);
    l_rect(fb, Rect::new(x + 4, y + 4, fill_w, 8), false);
}

fn draw_ive_progress(fb: &mut Framebuffer, x: u16, y: u16, w: u16, permille: u16) {
    l_rect(fb, Rect::new(x, y, w, 1), false);
    let fill_w = ((w as u32 * permille.min(1000) as u32) / 1000) as u16;
    l_rect(
        fb,
        Rect::new(x, y.saturating_sub(1), fill_w.max(1), 3),
        false,
    );
}

fn draw_paper_panel(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 6, y + 6, w - 12, 1), false);
    l_rect(fb, Rect::new(x + 6, y + h - 8, w - 12, 1), false);
    for inset in [18, 22, 26] {
        l_rect(fb, Rect::new(x + inset, y + 18, 1, h - 36), false);
    }
}

fn draw_paper_gutter(fb: &mut Framebuffer, x: u16, y: u16, h: u16) {
    l_rect(fb, Rect::new(x, y, 1, h), false);
    l_rect(fb, Rect::new(x + 5, y + 16, 1, h - 32), false);
}

fn draw_recessed_slot(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect_direct(fb, x, y, w, h);
    l_rect(fb, Rect::new(x + 4, y + 4, w - 8, 1), false);
    l_rect(fb, Rect::new(x + 4, y + h - 6, w - 8, 2), false);
    l_rect(fb, Rect::new(x + 8, y + 8, 1, h - 16), false);
}

fn draw_button_well(fb: &mut Framebuffer, x: u16, y: u16) {
    stroke_rect_direct(fb, x, y, 28, 22);
    l_rect(fb, Rect::new(x + 5, y + 6, 18, 2), false);
    l_rect(fb, Rect::new(x + 5, y + 14, 18, 1), false);
}

fn draw_landscape_home_book_first(fb: &mut Framebuffer) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape(fb, 724, 24, 82);
    draw_landscape_action_stack(fb, 36, 92, 206, 296, 0);
    draw_cover_art(fb, 336, 34, 245, 366);
    draw_text_centered(fb, title_font, "Flowers for Algernon", 458, 432);
    draw_text_centered(fb, body_font, "Daniel Keyes", 458, 460);
    draw_thin_progress(fb, 622, 156, 120, 420);
    l_text(fb, body_font, "42%", 660, 188, false);
}

fn draw_landscape_action_stack(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    selected: usize,
) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let row_h = h / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        if index == selected {
            l_rect(fb, Rect::new(x, row_y, w, row_h.saturating_sub(2)), false);
        } else {
            l_rect(fb, Rect::new(x, row_y, w, 1), false);
            l_rect(
                fb,
                Rect::new(x, row_y + row_h.saturating_sub(2), w, 1),
                false,
            );
        }
        let text_w = measure_text(font, label) as i16;
        let text_x = x as i16 + 24;
        let text_y = row_y as i16 + (row_h as i16 / 2) + 8;
        l_text(fb, font, label, text_x, text_y, index == selected);
        l_rect(
            fb,
            Rect::new(x + w - 30, row_y + row_h / 2 - 1, 20, 2),
            index == selected,
        );
        if index == selected {
            l_rect(fb, Rect::new(x + 10, row_y + 10, 4, row_h - 22), true);
        } else {
            l_rect(
                fb,
                Rect::new((text_x + text_w + 18) as u16, row_y + row_h / 2, 32, 1),
                false,
            );
        }
    }
}

fn draw_bottom_tabs(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16, selected: usize) {
    let labels = ["Read", "Files", "Sync", "Settings"];
    let font = literata(FontStyle::Regular);
    let tab_w = w / labels.len() as u16;
    for (index, label) in labels.iter().enumerate() {
        let tab_x = x + index as u16 * tab_w;
        if index == selected {
            l_rect(fb, Rect::new(tab_x, y, tab_w, h), false);
        } else {
            stroke_rect_direct(fb, tab_x, y, tab_w, h);
        }
        let text_x = tab_x as i16 + (tab_w as i16 - measure_text(font, label) as i16) / 2;
        l_text(fb, font, label, text_x, y as i16 + 34, index == selected);
    }
}

fn draw_cover_art(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect_direct(fb, x, y, w, h);
    stroke_rect_direct(fb, x + 8, y + 8, w - 16, h - 16);
    l_rect(fb, Rect::new(x + 18, y + 28, w - 36, 2), false);
    l_rect(fb, Rect::new(x + 18, y + 58, w - 36, 1), false);
    l_rect(fb, Rect::new(x + 28, y + h - 58, w - 56, 2), false);
    for row in 0..8 {
        let yy = y + 86 + row * 20;
        let inset = 24 + (row % 3) * 10;
        l_rect(fb, Rect::new(x + inset, yy, w - inset * 2, 3), false);
    }
    draw_ascii(fb, "FLOWERS", x as usize + 48, y as usize + 36, false);
    draw_ascii(
        fb,
        "ALGERNON",
        x as usize + 42,
        y as usize + h as usize - 40,
        false,
    );
}

fn draw_cover_art_skeuo(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    l_rect(fb, Rect::new(x + 8, y + 8, w, h), false);
    l_rect(fb, Rect::new(x + 9, y + 9, w - 2, h - 2), true);
    stroke_rect_direct(fb, x, y, w, h);
    stroke_rect_direct(fb, x + 8, y + 8, w - 16, h - 16);
    l_rect(fb, Rect::new(x + 18, y + 22, 2, h - 44), false);
    l_rect(fb, Rect::new(x + 30, y + 36, w - 60, 3), false);
    l_rect(fb, Rect::new(x + 30, y + 66, w - 60, 1), false);
    for row in 0..7 {
        let yy = y + 104 + row * 22;
        let inset = 32 + (row % 2) * 16;
        l_rect(fb, Rect::new(x + inset, yy, w - inset * 2, 3), false);
    }
    l_rect(fb, Rect::new(x + 34, y + h - 56, w - 68, 2), false);
    draw_ascii(fb, "FLOWERS", x as usize + 50, y as usize + 46, false);
    draw_ascii(
        fb,
        "ALGERNON",
        x as usize + 44,
        y as usize + h as usize - 38,
        false,
    );
}

fn draw_battery_landscape(fb: &mut Framebuffer, x: u16, y: u16, percent: u8) {
    let font = literata(FontStyle::Regular);
    stroke_rect_direct(fb, x, y, 42, 18);
    l_rect(fb, Rect::new(x + 42, y + 6, 4, 6), false);
    let fill_w = ((percent.min(100) as u16 * 34) / 100).max(1);
    l_rect(fb, Rect::new(x + 4, y + 4, fill_w, 10), false);
    l_text(fb, font, "82%", x as i16 - 48, y as i16 + 16, false);
}

fn draw_thin_progress(fb: &mut Framebuffer, x: u16, y: u16, w: u16, permille: u16) {
    l_rect(fb, Rect::new(x, y, w, 1), false);
    let fill_w = ((w as u32 * permille.min(1000) as u32) / 1000) as u16;
    l_rect(
        fb,
        Rect::new(x, y.saturating_sub(2), fill_w.max(1), 5),
        false,
    );
}

fn draw_text_centered(fb: &mut Framebuffer, font: &BitmapFont, text: &str, center_x: i16, y: i16) {
    // Center native-width text on the scaled center point (landscape design space).
    let x = lxi(center_x) - measure_text(font, text) as i16 / 2;
    draw_text(fb, font, text, x, lyi(y), false);
}

fn stroke_rect_direct(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    // Landscape design space: scale the box, keep hairlines 1px.
    let (x, y, w, h) = (lx(x), ly(y), lx(w), ly(h));
    fill_rect(fb, Rect::new(x, y, w, 1), false);
    fill_rect(fb, Rect::new(x, y + h - 1, w, 1), false);
    fill_rect(fb, Rect::new(x, y, 1, h), false);
    fill_rect(fb, Rect::new(x + w - 1, y, 1, h), false);
}

fn write_shell_preview(out: &Path, name: &str, view: UiView, selection: u16) -> std::io::Result<()> {
    let mut fb = Framebuffer::new();
    let entries = [
        "/books/Flowers for Algernon.epub",
        "/books/The Time Machine.epub",
        "/books/Unsong.epub",
    ];
    let chapters = [
        UiTocItem {
            title: "The Time Machine",
            level: 1,
            page: 1,
        },
        UiTocItem {
            title: "I. Introduction",
            level: 1,
            page: 1,
        },
        UiTocItem {
            title: "II. The Machine",
            level: 1,
            page: 9,
        },
        UiTocItem {
            title: "III. The Time Traveller Returns",
            level: 1,
            page: 21,
        },
        UiTocItem {
            title: "IV. Time Travelling",
            level: 1,
            page: 30,
        },
        UiTocItem {
            title: "V. In the Golden Age",
            level: 1,
            page: 44,
        },
    ];
    let shell = UiShell {
        view,
        orientation: UiOrientation::LandscapeButtonsBottom,
        refresh_policy: UiRefreshPolicy::FullOnWake,
        font_size: display::font::FontSize::Medium,
        line_spacing: display::font::LineSpacing::Normal,
        font_weight: display::font::FontWeight::Normal,
        font_family: display::font::FontFamily::Literata,
        custom_font_name: "",
        selection,
        chapter: 2,
        chapter_title: "",
        page: 141,
        page_count: 380,
        battery_percent: 82,
        active_book: UiBook {
            title: "Flowers for Algernon",
            author: "Daniel Keyes",
            progress_permille: 420,
            cover: None,
        },
        library_status: UiLibraryStatus::Ready,
        library_entries: &entries,
        library_window_start: 0,
        library_total: entries.len() as u16,
        chapters: &chapters,
        chapters_window_start: 0,
        chapters_total: chapters.len() as u16,
        sync_status: ui::UiSyncStatus::Idle,
        wifi_ssid: "HOME-WIFI",
    };
    render_shell(&mut fb, &shell);
    write_pbm(&out.join(format!("{name}.pbm")), &fb)?;
    write_png(&out.join(format!("{name}.png")), &fb)?;
    if matches!(
        view,
        UiView::Home | UiView::Library | UiView::Settings | UiView::Wireless
    ) {
        write_portrait_left_png(&out.join(format!("{name}-upright.png")), &fb)?;
    } else {
        write_panel_png(&out.join(format!("{name}-panel.png")), &fb)?;
    }
    Ok(())
}

fn write_reading(path: &Path) -> std::io::Result<()> {
    let mut fb = Framebuffer::new();
    let heading = literata(FontStyle::Bold);
    let body = literata(FontStyle::Regular);
    draw_text(&mut fb, heading, "Chapter 1", 320, 54, false);
    fill_rect(&mut fb, Rect::new(32, 76, 736, 2), false);
    let mut y = 120;
    for line in [
        "This is the first text-only EPUB reader surface.",
        "It uses generated Literata bitmap glyphs and",
        "keeps pagination as bounded data instead of a DOM.",
    ] {
        draw_text(&mut fb, body, line, 72, y, false);
        y += body.line_height as i16;
    }
    fill_rect(&mut fb, Rect::new(32, 424, 736, 2), false);
    draw_ascii(&mut fb, "Flowers for Algernon", 32, 444, false);
    write_pbm(path, &fb)
}

fn write_chapters(path: &Path) -> std::io::Result<()> {
    let mut fb = Framebuffer::new();
    draw_ascii(&mut fb, "CHAPTERS", 96, 112, false);
    for (index, item) in ["Chapter 1", "Chapter 2", "Chapter 3"].iter().enumerate() {
        draw_ascii(&mut fb, item, 136, 168 + index * 44, false);
    }
    write_pbm(path, &fb)
}

fn write_pbm(path: &Path, fb: &Framebuffer) -> std::io::Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    writeln!(file, "P1")?;
    writeln!(file, "{} {}", WIDTH, HEIGHT)?;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let black = !fb.pixel(x, y);
            write!(file, "{} ", black as u8)?;
        }
        writeln!(file)?;
    }
    Ok(())
}

pub(crate) fn write_png(path: &Path, fb: &Framebuffer) -> std::io::Result<()> {
    let file = BufWriter::new(File::create(path)?);
    let mut encoder = png::Encoder::new(file, WIDTH as u32, HEIGHT as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut pixels = vec![0u8; WIDTH * HEIGHT];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            pixels[y * WIDTH + x] = if fb.pixel(x, y) { 255 } else { 0 };
        }
    }
    writer.write_image_data(&pixels)?;
    Ok(())
}

fn write_panel_png(path: &Path, fb: &Framebuffer) -> std::io::Result<()> {
    let file = BufWriter::new(File::create(path)?);
    let mut encoder = png::Encoder::new(file, WIDTH as u32, HEIGHT as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut pixels = vec![0u8; WIDTH * HEIGHT];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let fb_x = WIDTH - 1 - x;
            pixels[y * WIDTH + x] = if fb.pixel(fb_x, y) { 255 } else { 0 };
        }
    }
    writer.write_image_data(&pixels)?;
    Ok(())
}

fn write_portrait_left_png(path: &Path, fb: &Framebuffer) -> std::io::Result<()> {
    let file = BufWriter::new(File::create(path)?);
    let mut encoder = png::Encoder::new(file, HEIGHT as u32, WIDTH as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut pixels = vec![0u8; WIDTH * HEIGHT];
    for y in 0..WIDTH {
        for x in 0..HEIGHT {
            let fb_x = WIDTH - 1 - y;
            let fb_y = HEIGHT - 1 - x;
            pixels[y * HEIGHT + x] = if fb.pixel(fb_x, fb_y) { 255 } else { 0 };
        }
    }
    writer.write_image_data(&pixels)?;
    Ok(())
}

fn preview_style_for_proto_style(style: proto::text::FontStyle, role: TextRole) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => {
            if matches!(
                role,
                TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
            ) {
                FontStyle::Bold
            } else {
                FontStyle::Regular
            }
        }
    }
}

fn paragraph_gap_after(line: &PreviewLine) -> i16 {
    if line.paragraph_end {
        paragraph_gap(line.role)
    } else {
        0
    }
}

fn block_align_for(run_align: TextAlign, block: &str, role: TextRole) -> TextAlign {
    if run_align == TextAlign::Center
        || matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
        || is_decorative_separator(block)
    {
        TextAlign::Center
    } else {
        run_align
    }
}

fn normalize_text(text: &str) -> String {
    let mut output = String::new();
    let mut previous_space = true;
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
            if !previous_space {
                output.push(' ');
            }
            previous_space = true;
        } else {
            output.push(normalize_char(ch));
            previous_space = false;
        }
        cursor += advance;
    }
    output.trim_end().to_string()
}

fn normalize_char(ch: char) -> char {
    match ch {
        '\u{00A0}' => ' ',
        ch if ch as u32 <= u16::MAX as u32 => ch,
        _ => '?',
    }
}

fn sanitize_preview_block(block: &mut String) -> bool {
    *block = block.trim().to_string();
    if block.is_empty() {
        return false;
    }
    if is_epub_titlepage_label(block) || contains_gutenberg_metadata(block) {
        return false;
    }
    if is_decorative_separator(block) {
        normalize_decorative_separator(block);
        return true;
    }
    if let Some(rest) = decorative_prefix_rest(block) {
        if rest.is_empty() {
            normalize_decorative_separator(block);
            return true;
        }
        if is_epub_titlepage_label(rest) || contains_gutenberg_metadata(rest) {
            return false;
        }
    }
    true
}

fn is_epub_titlepage_label(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with(": ")
        || lower == "title"
        || lower == "author"
        || lower == "creator"
        || lower == "language"
        || lower == "english"
        || lower == "english:"
        || lower == "release date"
        || lower == "original publication"
        || lower.starts_with("most recently updated")
        || lower.starts_with("other information")
        || lower.starts_with("other formats")
        || lower.starts_with("credits")
        || lower.starts_with("produced by")
        || lower.starts_with("transcribed from")
        || lower.starts_with("project gutenberg")
        || lower.starts_with("the project gutenberg")
}

fn contains_gutenberg_metadata(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("most recently updated")
        || lower.contains("project gutenberg ebook")
        || lower.contains("start of the project gutenberg")
        || lower.contains("end of the project gutenberg")
        || lower.contains("other information and formats")
        || lower.contains("this ebook is for the use of anyone")
        || lower.contains("project gutenberg license")
        || lower.contains("www.gutenberg.org")
        || lower.contains("laws of the country where you are located")
}

fn decorative_prefix_rest(text: &str) -> Option<&str> {
    let mut mark_count = 0u8;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '*' {
            mark_count = mark_count.saturating_add(1);
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if mark_count >= 3 {
        Some(text[end..].trim())
    } else {
        None
    }
}

fn is_decorative_separator(text: &str) -> bool {
    let mut saw_mark = false;
    let mut mark_count = 0u8;
    for ch in text.chars() {
        if ch == '*' {
            saw_mark = true;
            mark_count = mark_count.saturating_add(1);
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        return false;
    }
    saw_mark && mark_count >= 3
}

fn normalize_decorative_separator(block: &mut String) {
    if is_decorative_separator(block) {
        block.clear();
        block.push_str("* * *");
    }
}

fn is_leading_punctuation_word(word: &str) -> bool {
    word.chars()
        .next()
        .map(|ch| {
            matches!(
                ch,
                ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
            )
        })
        .unwrap_or(false)
}

fn styled_text_snapshot(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    let mut current = FontStyle::Regular;
    while let Some(ch) = chars.next() {
        if ch == STYLE_MARKER {
            if let Some(code) = chars.next().and_then(style_from_marker_code) {
                if code != current {
                    current = code;
                    match current {
                        FontStyle::Regular => out.push_str("[regular]"),
                        FontStyle::Italic => out.push_str("[italic]"),
                        FontStyle::Bold => out.push_str("[bold]"),
                        FontStyle::BoldItalic => out.push_str("[bold-italic]"),
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn resolve_epub_href(opf_path: &str, href: &str) -> String {
    let href_no_fragment = href.split('#').next().unwrap_or(href);
    if href_no_fragment.starts_with('/') {
        return href_no_fragment.trim_start_matches('/').to_string();
    }
    let mut out = String::new();
    if let Some((dir, _)) = opf_path.rsplit_once('/') {
        out.push_str(dir);
        out.push('/');
    }
    out.push_str(href_no_fragment);
    out
}

fn spine_item_is_navigation(item: &SpineItem, package: &EpubPackage<'_>) -> bool {
    let opf = package.opf_text;
    let href = item.href.of(opf);
    let lower_href = href.to_ascii_lowercase();
    let lower_props = item.properties.of(opf).to_ascii_lowercase();
    item.media_type.of(opf) == "application/x-dtbncx+xml"
        || package.nav_href.map(|nav| nav == href).unwrap_or(false)
        || package.ncx_href.map(|ncx| ncx == href).unwrap_or(false)
        || lower_props
            .split_ascii_whitespace()
            .any(|prop| prop == "nav")
        || lower_href.ends_with("toc.xhtml")
        || lower_href.ends_with("toc.html")
        || lower_href.ends_with("nav.xhtml")
        || lower_href.ends_with("nav.html")
}
