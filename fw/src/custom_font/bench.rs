//! Isolated SD-backed glyph/cache bench; never installs CUSTOM.FNT or settings.
//! Call `run` only inside the board I/O owner's SD session. It creates an
//! exclusive temporary root file, restores touched framebuffer pixels and
//! invalidates temporary metric entries, then removes its verified fixture.

use super::*;
use crate::sd_session::sd_stats as stats;
use proto::font_pack::{FontPackHeader, FONT_PACK_FACE_RECORD_BYTES, FONT_PACK_HEADER_BYTES};

const NAME: &str = "FNTBENCH.BIN";
const FIXTURE: &[u8] = include_bytes!("bench_font.fnt");
const PIXELS: [u8; 8] = [0x18, 0x24, 0x42, 0x7e, 0x42, 0x42, 0x42, 0];

/// Run against the existing framebuffer/store/cache without changing settings.
/// A pre-existing FNTBENCH.BIN is always refused, even if it is our fixture.
/// Cleanup is exposed separately for an interrupted earlier bench.
#[inline(never)]
pub(crate) fn run<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    fb: &mut Framebuffer,
    store: &ReaderStore,
    metrics: &mut MetricCache,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match root.open_file_in_dir(NAME, Mode::ReadWriteCreate) {
        Ok(file) => file,
        Err(_) => {
            esp_println::println!("bench: font_fixture status=create_refused file={}", NAME);
            return false;
        }
    };
    let wrote = file.write(FIXTURE).is_ok();
    let closed = file.close().is_ok();
    if !wrote || !closed {
        // This invocation exclusively created the file, so even a partial
        // write is ours. No pre-existing file can reach this cleanup path.
        let removed = crate::sd_session::remove_file_reclaiming_clusters(root, NAME);
        esp_println::println!(
            "bench: font_fixture status=write_failed cleanup={}",
            removed
        );
        return false;
    }
    memcpy_bench();
    let bulk_ok = sequential_read_bench(root);
    let result = exercise(root, fb, store, metrics) && bulk_ok;
    let removed = cleanup(root);
    esp_println::println!(
        "bench: font_fixture status=complete ok={} cleanup={} bytes={}",
        result,
        removed,
        FIXTURE.len()
    );
    result && removed
}

/// A fixed physical read workload independent of cache-hit reader paths.
/// Keep this frame separate from the glyph bitmap working set below.
#[inline(never)]
fn sequential_read_bench<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    const ITERATIONS: usize = 64;
    let Ok(file) = root.open_file_in_dir(NAME, Mode::ReadOnly) else {
        esp_println::println!("bench: sd_read_bulk ok=false reason=open");
        return false;
    };
    let mut bytes = [0u8; 2048];
    let spi = spi_counters();
    for counter in spi {
        stats::take(counter);
    }
    stats::take(&stats::READ_MICROS);
    let before = stats::snapshot();
    let started = embassy_time::Instant::now();
    let mut completed = 0;
    for _ in 0..ITERATIONS {
        if file.seek_from_start(0).is_err()
            || read_full(&file, &mut bytes).is_none()
            || bytes != FIXTURE[..bytes.len()]
        {
            break;
        }
        completed += 1;
    }
    let elapsed_us = started.elapsed().as_micros();
    let after = stats::snapshot();
    let read_us = stats::take(&stats::READ_MICROS);
    let [short, short_us, data, data_bytes, data_us] = spi.map(stats::take);
    let ok = completed == ITERATIONS;
    esp_println::println!(
        "bench: sd_read_bulk ok={} bytes=2048 iterations={} expected_iterations={} elapsed_us={} rd_calls={} rd_blocks={}",
        ok,
        completed,
        ITERATIONS,
        elapsed_us,
        after.read_calls.wrapping_sub(before.read_calls),
        after.read_blocks.wrapping_sub(before.read_blocks),
    );
    // Where the read time went: short transactions (commands and polls),
    // data transactions, and what the driver spent outside both.
    esp_println::println!(
        "bench: sd_spi read_us={} short_txns={} short_us={} data_txns={} data_bytes={} data_us={} rd_errors={}",
        read_us,
        short,
        short_us,
        data,
        data_bytes,
        data_us,
        stats::READ_ERRORS.load(core::sync::atomic::Ordering::Relaxed),
    );
    ok
}

/// Times the linked `memcpy` (the chip ROM's) on one card block, word-
/// aligned and not, since every SD block is copied several times between
/// the card and its reader.
#[inline(never)]
fn memcpy_bench() {
    #[repr(align(4))]
    struct Aligned([u8; 516]);
    const ROUNDS: u32 = 64;
    let mut src = Aligned([0; 516]);
    let mut dst = Aligned([0; 516]);
    for (i, byte) in src.0.iter_mut().enumerate() {
        *byte = i as u8;
    }
    let mut time = |offset: usize| {
        let started = embassy_time::Instant::now();
        for _ in 0..ROUNDS {
            let from = core::hint::black_box(&src.0[offset..offset + 512]);
            dst.0[..512].copy_from_slice(from);
            core::hint::black_box(&mut dst.0);
        }
        started.elapsed().as_micros()
    };
    let aligned = time(0);
    let unaligned = time(1);
    esp_println::println!(
        "bench: memcpy bytes=512 rounds={} aligned_us={} unaligned_us={}",
        ROUNDS,
        aligned,
        unaligned
    );
}

fn spi_counters() -> [&'static core::sync::atomic::AtomicU32; 5] {
    [
        &stats::SPI_SHORT_TXNS,
        &stats::SPI_SHORT_MICROS,
        &stats::SPI_DATA_TXNS,
        &stats::SPI_DATA_BYTES,
        &stats::SPI_DATA_MICROS,
    ]
}

fn reset_metrics(cache: &mut MetricCache) {
    for slot in &mut cache.slots {
        slot.key = None;
    }
    for slot in &mut cache.non_ascii {
        slot.key = None;
    }
}

#[inline(never)]
fn exercise<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    fb: &mut Framebuffer,
    source: &ReaderStore,
    cache: &mut MetricCache,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(file) = root.open_file_in_dir(NAME, Mode::ReadOnly) else {
        return false;
    };
    let mut header = [0; FONT_PACK_HEADER_BYTES];
    if read_full(&file, &mut header).is_none() {
        return false;
    }
    let Ok(header) = FontPackHeader::decode(&header) else {
        return false;
    };
    if header.total_len != FIXTURE.len() as u32 {
        return false;
    }
    let mut face = [0; FONT_PACK_FACE_RECORD_BYTES];
    if file.seek_from_start(header.face_table_offset).is_err()
        || read_full(&file, &mut face).is_none()
    {
        return false;
    }
    let Ok(face) = FontPackFaceRecord::decode(&face) else {
        return false;
    };

    reset_metrics(cache);
    let mut font = CustomFontFile {
        file: &file,
        source,
        cache,
        bitmaps: BitmapCache::new(),
    };
    // Check a page-sized glyph workload while preserving the caller's frame.
    // Each cell is restored immediately, avoiding a second full framebuffer.
    let mut draw_checked = |fb: &mut Framebuffer, left: usize, top: usize| {
        let mut previous = [false; 64];
        for y in 0..8 {
            for x in 0..8 {
                previous[y * 8 + x] = fb.pixel(left + x, top + y);
                fb.set_pixel(left + x, top + y, true);
            }
        }
        let drawn = font
            .metric_for_char(face, 'A')
            .and_then(|metric| {
                font.draw_glyph_bitmap(fb, face, metric, left as i16, top as i16 + 8)
            })
            .is_some();
        let mut matched = drawn;
        for (y, row) in PIXELS.iter().enumerate() {
            for x in 0..8 {
                matched &= fb.pixel(left + x, top + y) == (row & (0x80 >> x) == 0);
                fb.set_pixel(left + x, top + y, previous[y * 8 + x]);
            }
        }
        matched
    };
    let before = crate::sd_session::sd_stats::snapshot();
    let cold = draw_checked(fb, 32, 32);
    let after_cold = crate::sd_session::sd_stats::snapshot();
    let mut pixels_match = cold;
    let mut glyphs = 0;
    for top in (32..fb.frame_height().saturating_sub(32)).step_by(16) {
        for left in (32..fb.frame_width().saturating_sub(32)).step_by(16) {
            pixels_match &= draw_checked(fb, left, top);
            glyphs += 1;
        }
    }
    let after_warm = crate::sd_session::sd_stats::snapshot();
    reset_metrics(font.cache);
    let cold_reads = after_cold.read_calls.wrapping_sub(before.read_calls);
    let warm_reads = after_warm.read_calls.wrapping_sub(after_cold.read_calls);
    let ok = pixels_match && glyphs > 0 && cold_reads > 0 && warm_reads == 0;
    esp_println::println!(
        "bench: font_bitmap ok={} cold_reads={} warm_reads={} pixels_match={} warm_glyphs={} metric_and_bitmap_cache=true",
        ok, cold_reads, warm_reads, pixels_match, glyphs
    );
    ok
}

/// Remove only our byte-for-byte fixture (or an identifiable truncated prefix).
/// Existing files with other contents, including empty files, are preserved.
pub(crate) fn cleanup<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match root.open_file_in_dir(NAME, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    let len = file.length() as usize;
    if !(FONT_PACK_HEADER_BYTES..=FIXTURE.len()).contains(&len) {
        return false;
    }
    let mut bytes = [0; 64];
    for expected in FIXTURE[..len].chunks(bytes.len()) {
        if read_full(&file, &mut bytes[..expected.len()]).is_none()
            || bytes[..expected.len()] != *expected
        {
            return false;
        }
    }
    if file.close().is_err() {
        return false;
    }
    crate::sd_session::remove_file_reclaiming_clusters(root, NAME)
}
