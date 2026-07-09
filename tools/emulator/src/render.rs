use display::fb::Framebuffer;
#[cfg(feature = "gui")]
use egui::{Color32, ColorImage};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use ui::{
    app_render::{
        render_request as render_shared_request, render_sleep as render_shared_sleep, UiRenderModel,
    },
    UiBook, UiLibraryStatus, UiTocItem,
};

const DEMO_TITLE: &str = "Flowers for Algernon";
const DEMO_AUTHOR: &str = "Daniel Keyes";
const DEMO_CHAPTERS: [UiTocItem<'static>; 4] = [
    UiTocItem {
        title: "Bring Up",
        level: 1,
        page: 3,
    },
    UiTocItem {
        title: "Architecture",
        level: 1,
        page: 9,
    },
    UiTocItem {
        title: "Power",
        level: 1,
        page: 17,
    },
    UiTocItem {
        title: "Next Phase",
        level: 1,
        page: 24,
    },
];

pub fn render_request(
    fb: &mut Framebuffer,
    request: app_core::RenderRequest,
    library_entries: &[String],
) {
    let borrowed_entries: Vec<&str> = library_entries.iter().map(String::as_str).collect();
    let model = demo_model(request, &borrowed_entries);
    render_shared_request(fb, request, &model);
    apply_orientation(fb, request.orientation);
}

pub fn render_sleep(
    fb: &mut Framebuffer,
    request: app_core::RenderRequest,
    library_entries: &[String],
) {
    let borrowed_entries: Vec<&str> = library_entries.iter().map(String::as_str).collect();
    let model = demo_model(request, &borrowed_entries);
    render_shared_sleep(fb, request, &model);
    apply_orientation(fb, request.orientation);
}

fn apply_orientation(fb: &mut Framebuffer, orientation: app_core::DisplayOrientation) {
    if orientation == app_core::DisplayOrientation::LandscapeButtonsTop {
        fb.rotate_180();
    }
}

#[cfg(feature = "gui")]
pub fn framebuffer_to_color_image(fb: &Framebuffer) -> ColorImage {
    let (width, height) = (fb.width(), fb.height());
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            pixels.push(if fb.pixel(x, y) {
                Color32::from_rgb(238, 236, 226)
            } else {
                Color32::from_rgb(22, 22, 20)
            });
        }
    }
    ColorImage {
        size: [width, height],
        pixels,
    }
}

pub fn write_png(path: &Path, fb: &Framebuffer) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    encode_png(&mut writer, fb)?;
    Ok(())
}

pub fn write_presented_png(
    path: &Path,
    fb: &Framebuffer,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    encode_presented_png(&mut writer, fb)?;
    Ok(())
}

pub fn encode_png<W: Write>(writer: W, fb: &Framebuffer) -> Result<(), png::EncodingError> {
    // Logical frame dimensions: a portrait frame dumps as an upright
    // HEIGHT x WIDTH image (portrait frames skip the panel-mount mirror).
    let (width, height) = (fb.width(), fb.height());
    let mut encoder = png::Encoder::new(writer, width as u32, height as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut data = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            data.push(if fb.pixel(x, y) { 0xEE } else { 0x18 });
        }
    }
    writer.write_image_data(&data)
}

pub fn encode_presented_png<W: Write>(
    writer: W,
    fb: &Framebuffer,
) -> Result<(), png::EncodingError> {
    let (width, height) = (fb.width(), fb.height());
    let mut encoder = png::Encoder::new(writer, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut data = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            if fb.pixel(x, y) {
                data.extend_from_slice(&[238, 236, 226, 255]);
            } else {
                data.extend_from_slice(&[22, 22, 20, 255]);
            }
        }
    }
    writer.write_image_data(&data)
}

fn demo_model<'a>(
    request: app_core::RenderRequest,
    library_entries: &'a [&'a str],
) -> UiRenderModel<'a> {
    // SD books take their title from the scanned entry, mirroring the
    // firmware's catalog labels; the built-in demo book keeps its own.
    let sd_entry = app_core::ReaderSource::from_book_id(request.book_id)
        .sd_index()
        .and_then(|index| library_entries.get(index as usize).copied());
    let (title, author) = match sd_entry {
        Some(entry) => (entry, ""),
        None => (DEMO_TITLE, DEMO_AUTHOR),
    };
    UiRenderModel {
        active_book: UiBook {
            title,
            author,
            progress_permille: progress_permille(request),
            cover: None,
        },
        library_status: if library_entries.is_empty() {
            UiLibraryStatus::Empty
        } else {
            UiLibraryStatus::Ready
        },
        library_entries,
        // The emulator keeps the whole catalog resident, so the window is the
        // full list starting at index 0.
        library_window_start: 0,
        chapters: &DEMO_CHAPTERS,
        chapters_window_start: 0,
        chapters_total: DEMO_CHAPTERS.len() as u16,
        chapter_title: "",
        custom_font_name: "",
    }
}

fn progress_permille(request: app_core::RenderRequest) -> u16 {
    let total = if app_core::ReaderSource::from_book_id(request.book_id).is_sd() {
        24
    } else {
        4
    };
    (((request.page + request.chapter as u32) * 1000) / total).min(1000) as u16
}
