/// The size an image is shown at: the largest that fits inside
/// `max_w × max_h` with the source's aspect ratio, never larger than the
/// source, and never zero in either direction.
///
/// The constrained side takes the box's full length and the other side is
/// rounded down, so the result always fits. Layout calls this once and hands
/// the result to the decoder, which emits exactly that size; fitting a
/// fitted size again could shrink it by a pixel.
pub fn fit_inside(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (src_w, src_h) = (src_w.max(1), src_h.max(1));
    let (max_w, max_h) = (max_w.max(1), max_h.max(1));
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let (sw, sh) = (u64::from(src_w), u64::from(src_h));
    let (mw, mh) = (u64::from(max_w), u64::from(max_h));
    // Width constrains when src_w / max_w >= src_h / max_h. Each quotient
    // below is at most the box side it is compared against, so it fits u32.
    if sw * mh >= sh * mw {
        (max_w, ((sh * mw / sw) as u32).max(1))
    } else {
        (((sw * mh / sh) as u32).max(1), max_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_images_keep_their_size() {
        assert_eq!(fit_inside(138, 40, 784, 451), (138, 40));
        assert_eq!(fit_inside(784, 451, 784, 451), (784, 451));
        assert_eq!(fit_inside(1, 1, 784, 451), (1, 1));
    }

    #[test]
    fn large_images_shrink_to_the_constraining_side() {
        // The design's worked example: a 1600×1200 figure in the X4
        // landscape page box.
        assert_eq!(fit_inside(1600, 1200, 784, 451), (601, 451));
        assert_eq!(fit_inside(1600, 400, 784, 451), (784, 196));
        assert_eq!(fit_inside(1000, 1000, 464, 763), (464, 464));
        assert_eq!(fit_inside(900, 451, 784, 451), (784, 392));
    }

    #[test]
    fn extreme_shapes_never_reach_zero() {
        assert_eq!(fit_inside(16_384, 1, 800, 600), (800, 1));
        assert_eq!(fit_inside(1, 16_384, 800, 600), (1, 600));
        assert_eq!(fit_inside(0, 0, 0, 0), (1, 1));
        assert_eq!(fit_inside(u32::MAX, u32::MAX, 3, 2), (2, 2));
    }

    #[test]
    fn result_fits_keeps_aspect_and_never_upscales() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let src_w = 1 + (seed % 5000) as u32;
            let src_h = 1 + ((seed >> 16) % 5000) as u32;
            let max_w = 1 + ((seed >> 32) % 900) as u32;
            let max_h = 1 + ((seed >> 44) % 900) as u32;
            let (w, h) = fit_inside(src_w, src_h, max_w, max_h);
            assert!(w >= 1 && h >= 1);
            assert!(w <= max_w && h <= max_h);
            assert!(w <= src_w && h <= src_h);
            // One side touches the box unless the source already fit.
            if src_w > max_w || src_h > max_h {
                assert!(w == max_w || h == max_h);
            }
            // Rounding down loses less than a pixel of the free side.
            if w == max_w && h > 1 && src_w > max_w {
                assert!(u64::from(h) * u64::from(src_w) <= u64::from(src_h) * u64::from(w));
                assert!(u64::from(h + 1) * u64::from(src_w) > u64::from(src_h) * u64::from(w));
            }
        }
    }
}
