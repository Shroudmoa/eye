//! Character cell attributes: colours, styles, and the cell itself.

/// A terminal colour. Kept as a semantic enum (rather than a packed int) so
/// exporters cannot confuse a palette index with a truecolor triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's own foreground/background.
    #[default]
    Default,
    /// A palette entry, 0-255.
    Indexed(u8),
    /// A 24-bit colour.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Resolve to a CSS colour string for HTML export.
    ///
    /// The first sixteen entries use the widely-agreed xterm defaults so that
    /// exported transcripts look right without shipping a palette.
    pub fn to_css(&self, default: &str) -> String {
        const BASE: [(u8, u8, u8); 16] = [
            (0x1c, 0x1c, 0x1c),
            (0xcc, 0x24, 0x24),
            (0x27, 0x9e, 0x4c),
            (0xc4, 0xa0, 0x00),
            (0x34, 0x65, 0xa4),
            (0x8b, 0x57, 0xad),
            (0x00, 0x89, 0x8b),
            (0xc8, 0xc8, 0xc8),
            (0x76, 0x76, 0x76),
            (0xff, 0x4b, 0x4b),
            (0x5c, 0xe0, 0x7a),
            (0xf5, 0xd0, 0x4c),
            (0x6a, 0x9f, 0xe0),
            (0xb4, 0x82, 0xd8),
            (0x4b, 0xc4, 0xc4),
            (0xff, 0xff, 0xff),
        ];
        match self {
            Color::Default => default.to_owned(),
            Color::Indexed(i) if *i < 16 => {
                let (r, g, b) = BASE[*i as usize];
                format!("#{r:02x}{g:02x}{b:02x}")
            }
            Color::Indexed(i) => xterm256(*i),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    /// Is this the default colour? Used to decide whether a span needs a class.
    pub fn is_default(&self) -> bool {
        matches!(self, Color::Default)
    }
}

/// The xterm 256-colour cube.
fn xterm256(i: u8) -> String {
    const CUBE: [u8; 24] = [
        0, 95, 135, 175, 215, 255, 88, 118, 148, 178, 208, 238, 51, 81, 111, 141, 171, 201,
        231, 26, 56, 86, 116, 146,
    ];
    const LEVELS: [u8; 6] = [0, 51, 102, 153, 204, 255];
    match i {
        0..=15 => xterm256(0), // unreachable in practice; keeps the match total
        16..=231 => {
            let n = i - 16;
            let (r, g, b) = (CUBE[(n / 36) as usize], CUBE[((n % 36) / 6) as usize], LEVELS[(n % 6) as usize]);
            format!("#{r:02x}{g:02x}{b:02x}")
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

/// Style bits, mirroring the SGR attributes a terminal actually honours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Attrs(u16);

impl Attrs {
    pub const BOLD: u16 = 1 << 0;
    pub const FAINT: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const BLINK: u16 = 1 << 4;
    pub const INVERSE: u16 = 1 << 5;
    pub const HIDDEN: u16 = 1 << 6;
    pub const STRIKE: u16 = 1 << 7;

    pub const fn empty() -> Self {
        Attrs(0)
    }
    pub const fn bits(self) -> u16 {
        self.0
    }
    pub const fn contains(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
    pub fn set(&mut self, flag: u16, on: bool) {
        if on {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }
    pub fn is_plain(&self) -> bool {
        self.0 == 0
    }
}

/// One character cell.
///
/// `ch` is a space for the second half of a double-width glyph, which keeps
/// the grid rectangular and makes column maths correct everywhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    /// Display width: 0 for the trailing half of a wide glyph, 1 normally, 2
    /// for a wide glyph's leading cell.
    pub width: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
            width: 1,
        }
    }
}

impl Cell {
    /// Does this cell carry no visible information?
    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.bg.is_default() && self.attrs.is_plain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_colours_resolve_to_the_xterm_palette() {
        assert_eq!(Color::Indexed(1).to_css("#fff"), "#cc2424");
        assert_eq!(Color::Indexed(16).to_css("#fff"), "#000000");
        assert_eq!(Color::Indexed(231).to_css("#fff"), "#ffffff");
        assert_eq!(Color::Indexed(232).to_css("#fff"), "#080808");
        assert_eq!(Color::Indexed(255).to_css("#fff"), "#eeeeee");
        assert_eq!(Color::Rgb(1, 2, 3).to_css("#fff"), "#010203");
        assert_eq!(Color::Default.to_css("#abc"), "#abc");
    }

    #[test]
    fn attrs_toggle_independently() {
        let mut a = Attrs::empty();
        assert!(a.is_plain());
        a.set(Attrs::BOLD, true);
        assert!(a.contains(Attrs::BOLD));
        assert!(!a.contains(Attrs::ITALIC));
        a.set(Attrs::BOLD, false);
        assert!(a.is_plain());
    }

    #[test]
    fn blank_cells_account_for_background() {
        assert!(Cell::default().is_blank());
        let mut styled = Cell::default();
        styled.bg = Color::Indexed(4);
        assert!(!styled.is_blank());
    }
}
