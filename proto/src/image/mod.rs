//! Images in books: finding them ([`path`], [`index`]), naming them in a
//! section's text ([`token`]), decoding them to gray rows at the size layout
//! fitted, and dithering those rows to the panel's 1 bpp.
//!
//! Nothing here allocates or keeps static buffers. Each piece borrows its
//! working memory from the caller, which on device lends buffers that sit
//! idle while an image decodes: the XHTML window for scratch and the ZIP
//! inflater for the zlib stream.

pub mod dither;
mod fit;
pub mod index;
pub mod jpeg;
pub mod pack;
pub mod path;
pub mod png;
pub mod token;

pub use fit::fit_inside;

/// Receives a decoder's output, one gray row at a time, top to bottom.
///
/// A decoder built for `out_w × out_h` calls [`LumaSink::row`] exactly
/// `out_h` times on success, with `y` counting up from 0 and `luma` holding
/// exactly `out_w` bytes (0 = black, 255 = white).
///
/// Rows are provisional until the decoder's `finish` returns `Ok`: a decode
/// can still fail after some rows went out, for example when the file is
/// cut short. Firmware appends rows to the pack file as they come and
/// records the image in the index only once `finish` succeeds, so a failed
/// decode leaves an orphan tail that nothing points at.
pub trait LumaSink {
    /// Takes output row `y`. An error stops the decode, and the decoder
    /// returns it to its caller; firmware uses this to abandon an image when
    /// a button is pressed.
    fn row(&mut self, y: u32, luma: &[u8]) -> Result<(), SinkError>;
}

impl<F: FnMut(u32, &[u8]) -> Result<(), SinkError>> LumaSink for F {
    fn row(&mut self, y: u32, luma: &[u8]) -> Result<(), SinkError> {
        self(y, luma)
    }
}

/// Why a [`LumaSink`] stopped a decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkError {
    /// The caller gave up on this image, for example on a button press.
    Cancelled,
    /// The sink could not keep the row, for example a card write failed.
    Io,
}
