#[path = "../../fw/src/custom_font/bitmap_cache.rs"]
mod bitmap_cache;

use bitmap_cache::BitmapCache;

#[test]
fn repeated_glyphs_reuse_bytes_but_identity_offset_and_length_must_match() {
    let mut cache = BitmapCache::new();
    let mut reads = 0;
    for (identity, offset, len) in [(1, 20, 5), (1, 20, 5), (2, 20, 5), (2, 21, 5), (2, 21, 6)] {
        let bytes = cache
            .get_or_load(identity, offset, len, |bytes| {
                reads += 1;
                bytes.fill(offset as u8);
                Some(())
            })
            .unwrap();
        assert_eq!(bytes, vec![offset as u8; len]);
    }
    assert_eq!(reads, 4);
    assert!(core::mem::size_of::<BitmapCache>() <= 2400);
}

#[test]
fn arena_wrap_invalidates_overwritten_glyphs_only() {
    let mut cache = BitmapCache::new();
    for offset in 0..5 {
        assert_eq!(
            cache
                .get_or_load(1, offset, 512, |b| {
                    b.fill(offset as u8);
                    Some(())
                })
                .unwrap(),
            vec![offset as u8; 512]
        );
    }
    // The fifth bitmap overwrites the first; the second is still resident.
    assert_eq!(
        cache
            .get_or_load(1, 1, 512, |_| panic!("unaffected bitmap evicted"))
            .unwrap(),
        vec![1; 512]
    );
    let mut reloaded = false;
    cache
        .get_or_load(1, 0, 512, |b| {
            reloaded = true;
            b.fill(0);
            Some(())
        })
        .unwrap();
    assert!(reloaded);
}

#[test]
fn metadata_eviction_and_failed_reads_never_return_stale_bytes() {
    let mut cache = BitmapCache::new();
    for offset in 0..100 {
        assert_eq!(
            cache
                .get_or_load(1, offset, 1, |b| {
                    b[0] = offset as u8;
                    Some(())
                })
                .unwrap(),
            &[offset as u8]
        );
    }
    assert!(cache
        .get_or_load(1, 100, 512, |b| {
            b.fill(99);
            None
        })
        .is_none());
    assert_eq!(
        cache
            .get_or_load(1, 100, 512, |b| {
                b.fill(42);
                Some(())
            })
            .unwrap(),
        vec![42; 512]
    );
    // Force a failed wrapping write over previously valid data.
    cache
        .get_or_load(1, 101, 512, |b| {
            b.fill(1);
            Some(())
        })
        .unwrap();
    cache
        .get_or_load(1, 102, 512, |b| {
            b.fill(2);
            Some(())
        })
        .unwrap();
    assert!(cache
        .get_or_load(1, 103, 512, |b| {
            b.fill(3);
            None
        })
        .is_none());
    assert_eq!(
        cache
            .get_or_load(1, 0, 1, |b| {
                b[0] = 7;
                Some(())
            })
            .unwrap(),
        &[7]
    );
}

#[test]
fn empty_and_oversized_bitmaps_do_not_read() {
    let mut cache = BitmapCache::new();
    assert_eq!(
        cache.get_or_load(1, 0, 0, |_| panic!("empty glyph read")),
        Some(&[][..])
    );
    assert!(cache
        .get_or_load(1, 0, 2049, |_| panic!("oversized glyph read"))
        .is_none());
}

#[test]
fn failed_wrapping_fill_invalidates_an_existing_hit() {
    let mut cache = BitmapCache::new();
    for offset in 0..4 {
        cache
            .get_or_load(1, offset, 512, |b| {
                b.fill(offset as u8);
                Some(())
            })
            .unwrap();
    }
    assert!(cache
        .get_or_load(1, 4, 512, |b| {
            b.fill(99);
            None
        })
        .is_none());
    let mut reads = 0;
    assert_eq!(
        cache
            .get_or_load(1, 0, 512, |b| {
                reads += 1;
                b.fill(0);
                Some(())
            })
            .unwrap(),
        vec![0; 512]
    );
    assert_eq!(reads, 1, "partially overwritten bitmap must be reloaded");
}

#[test]
fn repeated_page_text_reduces_reads_without_changing_bitmap_bytes() {
    let mut cache = BitmapCache::new();
    let text = "the reader turns the page and reads another line. ".repeat(20);
    let mut reads = 0;
    for ch in text.bytes() {
        // A synthetic 16x24 bitmap: the complete working set fits in 2KB.
        let expected = [ch; 48];
        let bytes = cache
            .get_or_load(1, u32::from(ch) * 48, 48, |b| {
                reads += 1;
                b.copy_from_slice(&expected);
                Some(())
            })
            .unwrap();
        assert_eq!(bytes, expected);
    }
    assert!(
        reads < text.len() / 10,
        "{reads} reads for {} glyphs",
        text.len()
    );
}
