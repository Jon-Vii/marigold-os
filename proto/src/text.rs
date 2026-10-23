#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontStyle {
    Regular,
    Italic,
    Bold,
    BoldItalic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextRole {
    Body,
    Heading1,
    Heading2,
    Heading3,
    BlockQuote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextAlign {
    Left,
    Center,
    Justify,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBlock<const N: usize> {
    pub text: heapless::String<N>,
    pub role: TextRole,
    pub style: FontStyle,
    pub align: TextAlign,
}

impl<const N: usize> TextBlock<N> {
    pub const fn new(
        text: heapless::String<N>,
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
    ) -> Self {
        Self {
            text,
            role,
            style,
            align,
        }
    }
}
