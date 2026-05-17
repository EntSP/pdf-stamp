//! Glyph widths for Helvetica, in 1/1000 of the font size.
//!
//! The stamp is drawn in Helvetica, one of the fourteen fonts every PDF
//! reader is required to have, so nothing is embedded and the overlay
//! costs one small dictionary rather than a font programme.
//!
//! The numbers are Adobe's `Helvetica.afm` widths, indexed by WinAnsi
//! code. They are used for one thing: working out how wide a line will
//! be so it can be centred. Nothing structural depends on them — an
//! error here nudges the text sideways, it does not produce an invalid
//! file.

/// Widths for WinAnsi codes 32..=255. Index 0 is code 32 (space).
#[rustfmt::skip]
const WIDTHS: [u16; 224] = [
    // 0x20..0x2F   space ! " # $ % & ' ( ) * + , - . /
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    // 0x30..0x3F   0..9 : ; < = > ?
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    // 0x40..0x4F   @ A..O
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    // 0x50..0x5F   P..Z [ \ ] ^ _
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    // 0x60..0x6F   ` a..o
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    // 0x70..0x7F   p..z { | } ~ (0x7F unused)
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, 350,
    // 0x80..0x8F   EUR . , f ,, ... dagger ddagger ^ permille S < OE . Z .
    556, 350, 222, 556, 333, 1000, 556, 556, 333, 1000, 667, 333, 1000, 350, 611, 350,
    // 0x90..0x9F   . ' ' " " bullet en em ~ tm s > oe . z Y
    350, 222, 222, 333, 333, 350, 556, 1000, 333, 1000, 500, 333, 944, 350, 500, 667,
    // 0xA0..0xAF   nbsp exclamdown cent sterling currency yen brokenbar section
    //              dieresis copyright ordfeminine guillemotleft logicalnot
    //              softhyphen registered macron
    278, 333, 556, 556, 556, 556, 260, 556, 333, 737, 370, 556, 584, 333, 737, 333,
    // 0xB0..0xBF   degree plusminus two three acute mu paragraph periodcentered
    //              cedilla one ordmasculine guillemotright quarter half
    //              threequarters questiondown
    400, 584, 333, 333, 333, 556, 537, 278, 333, 333, 365, 556, 834, 834, 834, 611,
    // 0xC0..0xCF   Agrave..Idieresis
    667, 667, 667, 667, 667, 667, 1000, 722, 667, 667, 667, 667, 278, 278, 278, 278,
    // 0xD0..0xDF   Eth..germandbls
    722, 722, 778, 778, 778, 778, 778, 584, 778, 722, 722, 722, 722, 667, 667, 611,
    // 0xE0..0xEF   agrave..idieresis
    556, 556, 556, 556, 556, 556, 889, 500, 556, 556, 556, 556, 278, 278, 278, 278,
    // 0xF0..0xFF   eth..ydieresis
    556, 556, 556, 556, 556, 556, 556, 584, 611, 556, 556, 556, 556, 500, 556, 500,
];

/// How wide `text` is when set in Helvetica at `size`, in points.
///
/// `text` is WinAnsi-encoded bytes, which is what actually reaches the
/// content stream — measuring the `str` instead would count a two-byte
/// `ø` twice.
pub fn width(text: &[u8], size: f32) -> f32 {
    let thousandths: u32 = text
        .iter()
        .map(|&b| {
            if b < 32 {
                0
            } else {
                u32::from(WIDTHS[usize::from(b) - 32])
            }
        })
        .sum();
    thousandths as f32 * size / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_covers_every_code_from_space_up() {
        assert_eq!(WIDTHS.len(), 256 - 32);
    }

    #[test]
    fn a_known_string_measures_to_its_afm_width() {
        // H=722 e=556 l=222 l=222 o=556 -> 2278/1000 of the font size.
        assert!((width(b"Hello", 10.0) - 22.78).abs() < 0.001);
    }

    #[test]
    fn control_characters_contribute_nothing() {
        assert_eq!(width(b"\n\r\t", 12.0), 0.0);
    }

    #[test]
    fn width_scales_with_the_font_size() {
        let a = width(b"alice@example.com", 7.0);
        let b = width(b"alice@example.com", 14.0);
        assert!((b - a * 2.0).abs() < 0.001);
    }

    #[test]
    fn high_codes_are_measured_rather_than_skipped() {
        // 0xF8 is `oslash` in WinAnsi: a real glyph with a real width.
        assert!(width(&[0xF8], 10.0) > 0.0);
    }
}
