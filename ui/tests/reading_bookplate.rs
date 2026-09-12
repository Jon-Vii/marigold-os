use display::fb::{FbFrame, Framebuffer};
use ui::reading::draw_reading_bookplate;

#[test]
fn long_utf8_bookplate_labels_stay_inside_each_frame() {
    let title = "Été à Göteborg — a very long title with enough words to overflow both portrait and landscape reading frames";
    let author =
        "François Ångström and many collaborators with names that also exceed the available width";
    let mut fb = Framebuffer::new();
    for frame in [
        FbFrame::Landscape,
        FbFrame::LandscapeFlipped,
        FbFrame::Portrait,
    ] {
        fb.set_frame(frame);
        fb.clear(true);
        draw_reading_bookplate(&mut fb, title, author);
        let (width, height) = (fb.frame_width(), fb.frame_height());
        let mut ink = 0;
        for y in 0..height {
            for x in 0..width {
                if !fb.pixel(x, y) {
                    ink += 1;
                    // Leave a few pixels for italic glyph overhang beyond
                    // the measured advance; text must never hit the edge.
                    assert!(x >= 4 && x < width - 4, "clipped label in {frame:?}");
                    assert!(y >= height / 2 - 48 && y <= height / 2 + 40);
                }
            }
        }
        assert!(ink > 0);
    }
}
