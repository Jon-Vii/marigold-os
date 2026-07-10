//! Hand-rolled 1bpp key-strip icons for the portrait shell.
//!
//! Each glyph is authored as 24 rows of ASCII art (`#` = ink, space =
//! paper) and packed at compile time into a `[u32; 24]` bitmap — one bit
//! per column, MSB = leftmost. The panel is 1bpp and the strip draws in
//! pure ink, so a monochrome mask is exactly what reaches the glass;
//! ~1.3 KB of bitmap rodata and a two-loop blit cover the whole strip
//! without pulling in an icon crate (`embedded-icon` plus the
//! `embedded-graphics` drawing stack would cost ~18 KB of flash).

use display::fb::Framebuffer;

pub const ICON_SIZE: i16 = 24;

/// One 24x24 monochrome glyph, row-major, column `x` in bit `23 - x`.
type Icon = [u32; 24];

/// Pack 24 rows of ASCII art into a bitmap. Any non-space character inks
/// the pixel; rows may be short (the tail stays paper).
const fn icon(rows: [&str; 24]) -> Icon {
    let mut out = [0u32; 24];
    let mut y = 0;
    while y < 24 {
        let bytes = rows[y].as_bytes();
        let mut x = 0;
        while x < bytes.len() && x < 24 {
            if bytes[x] != b' ' {
                out[y] |= 1 << (23 - x);
            }
            x += 1;
        }
        y += 1;
    }
    out
}

/// Blit an icon in ink with its top-left corner at `(x, y)`, clipped
/// per-pixel to the framebuffer's logical bounds.
pub fn draw_icon(fb: &mut Framebuffer, glyph: &Icon, x: i16, y: i16) {
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..24u32 {
            if (bits >> (23 - col)) & 1 != 0 {
                let px = x + col as i16;
                let py = y + row as i16;
                if px >= 0 && py >= 0 {
                    fb.set_pixel(px as usize, py as usize, false);
                }
            }
        }
    }
}

/// The strip icon for a key label; the fallback question mark covers any
/// label without a dedicated glyph.
pub fn icon_for_label(label: &str) -> &'static Icon {
    match label {
        "home" => &HOME,
        "library" | "contents" => &LIST,
        "continue" | "open" => &BOOK,
        "wireless" => &WIFI,
        "settings" => &TUNE,
        "previous" => &CHEVRON_LEFT,
        "next" => &CHEVRON_RIGHT,
        "close" | "cancel" => &CROSS,
        "change" => &PENCIL,
        "connect" | "done" => &CHECK,
        "forget" => &TRASH,
        "set up" => &PLUS,
        "again" => &REFRESH,
        _ => &HELP,
    }
}

#[rustfmt::skip]
static HOME: Icon = icon([
    "                        ",
    "                        ",
    "           ##           ",
    "          ####          ",
    "         ######         ",
    "        ########        ",
    "       ##########       ",
    "      ############      ",
    "     ##############     ",
    "    ################    ",
    "   ##################   ",
    "     ##          ##     ",
    "     ##          ##     ",
    "     ##   ####   ##     ",
    "     ##   #  #   ##     ",
    "     ##   #  #   ##     ",
    "     ##   #  #   ##     ",
    "     ##   #  #   ##     ",
    "     ##   #  #   ##     ",
    "     ##############     ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static LIST: Icon = icon([
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "   ##    ############   ",
    "   ##    ############   ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "   ##    ############   ",
    "   ##    ############   ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "   ##    ############   ",
    "   ##    ############   ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static BOOK: Icon = icon([
    "                        ",
    "                        ",
    "     ######  ######     ",
    "  ##########  ##########",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##      ##  ##      ##",
    "  ##########  ##########",
    "     ######  ######     ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static WIFI: Icon = icon([
    "                        ",
    "                        ",
    "      ############      ",
    "    ####        ####    ",
    "   ##              ##   ",
    "  ##                ##  ",
    "       ##########       ",
    "     ####      ####     ",
    "    ##            ##    ",
    "                        ",
    "         ######         ",
    "       ##      ##       ",
    "                        ",
    "                        ",
    "          ####          ",
    "          ####          ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

// The "tune" sliders glyph stands in for settings: a gear is muddy at
// 24px 1bpp, three labelled tracks read cleanly. Each track is a thin rail
// with a square knob straddling it at a different position.
#[rustfmt::skip]
static TUNE: Icon = icon([
    "                        ",
    "                        ",
    "                        ",
    "      ####              ",
    "      ####              ",
    "   ##################   ",
    "   ##################   ",
    "      ####              ",
    "      ####              ",
    "              ####      ",
    "              ####      ",
    "   ##################   ",
    "   ##################   ",
    "              ####      ",
    "              ####      ",
    "         ####           ",
    "         ####           ",
    "   ##################   ",
    "   ##################   ",
    "         ####           ",
    "         ####           ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static CHEVRON_LEFT: Icon = icon([
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "              ###       ",
    "             ###        ",
    "            ###         ",
    "           ###          ",
    "          ###           ",
    "         ###            ",
    "        ###             ",
    "       ###              ",
    "      ###               ",
    "       ###              ",
    "        ###             ",
    "         ###            ",
    "          ###           ",
    "           ###          ",
    "            ###         ",
    "             ###        ",
    "              ###       ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static CHEVRON_RIGHT: Icon = icon([
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "       ###              ",
    "        ###             ",
    "         ###            ",
    "          ###           ",
    "           ###          ",
    "            ###         ",
    "             ###        ",
    "              ###       ",
    "               ###      ",
    "              ###       ",
    "             ###        ",
    "            ###         ",
    "           ###          ",
    "          ###           ",
    "         ###            ",
    "        ###             ",
    "       ###              ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static CROSS: Icon = icon([
    "                        ",
    "                        ",
    "    ###            ###   ",
    "    ####          ####   ",
    "     ####        ####    ",
    "      ####      ####     ",
    "       ####    ####      ",
    "        ####  ####       ",
    "         ########        ",
    "          ######         ",
    "          ######         ",
    "         ########        ",
    "        ####  ####       ",
    "       ####    ####      ",
    "      ####      ####     ",
    "     ####        ####    ",
    "    ####          ####   ",
    "    ###            ###   ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static PENCIL: Icon = icon([
    "                        ",
    "                 ####   ",
    "                ######  ",
    "               ####### ",
    "              #### #### ",
    "             ####  #### ",
    "            ####  ####  ",
    "           ####  ####   ",
    "          ####  ####    ",
    "         ####  ####     ",
    "        ####  ####      ",
    "       ####  ####       ",
    "      ####  ####        ",
    "     ####  ####         ",
    "    ####  ####          ",
    "   ####  ####           ",
    "   ###  ####            ",
    "   ##  ####             ",
    "   ######              ",
    "   #####               ",
    "   ###                 ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static CHECK: Icon = icon([
    "                        ",
    "                        ",
    "                        ",
    "                   ###  ",
    "                  ####  ",
    "                 ####   ",
    "                ####    ",
    "               ####     ",
    "              ####      ",
    "  ###        ####       ",
    "  ####      ####        ",
    "   ####    ####         ",
    "    ####  ####          ",
    "     ########           ",
    "      ######            ",
    "       ####             ",
    "        ##              ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static TRASH: Icon = icon([
    "                        ",
    "                        ",
    "        ########        ",
    "        ########        ",
    "     ##############     ",
    "     ##############     ",
    "                        ",
    "    ################    ",
    "    ##            ##    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##  ##  ##  ##  #    ",
    "    ##            ##    ",
    "    ################    ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static PLUS: Icon = icon([
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "   ##################   ",
    "   ##################   ",
    "   ##################   ",
    "   ##################   ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "          ####          ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static REFRESH: Icon = icon([
    "                        ",
    "                        ",
    "        ######          ",
    "      ##########        ",
    "     ####    ####    ##  ",
    "    ###        ##    ##  ",
    "   ###          #   ###  ",
    "   ##              ####  ",
    "  ###             #####  ",
    "  ###            ######  ",
    "  ###                   ",
    "  ###                   ",
    "  ###                   ",
    "   ##              ###   ",
    "   ###            ###    ",
    "    ###          ###     ",
    "     ####      ####      ",
    "      ##########        ",
    "        ######          ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);

#[rustfmt::skip]
static HELP: Icon = icon([
    "                        ",
    "        ######          ",
    "      ##########        ",
    "     ####    ####       ",
    "    ###        ###      ",
    "   ###          ###     ",
    "   ##            ##     ",
    "   ##     ####   ##     ",
    "          ####   ##     ",
    "         ####   ###     ",
    "        ####   ###      ",
    "        ####  ###       ",
    "        ####            ",
    "        ####            ",
    "                        ",
    "        ####            ",
    "        ####            ",
    "         ##             ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
    "                        ",
]);
