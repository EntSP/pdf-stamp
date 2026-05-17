//! Embedding a font, for text WinAnsi cannot hold.
//!
//! ## Why there are two paths at all
//!
//! Helvetica is one of the fourteen fonts every PDF reader must
//! provide, so the common stamp — an address and a timestamp, both
//! ASCII — costs one small dictionary and no font program. Throwing
//! that away for every download because a minority of recipients are
//! called `Łukasz` would be a poor trade, so the standard font stays
//! and this module handles the rest.
//!
//! ## What "the rest" needs
//!
//! A simple PDF font addresses glyphs with one byte, which caps it at
//! 256 of them and ties it to an encoding the reader has opinions
//! about. The way out is a composite font: `/Type0` with the
//! `Identity-H` encoding, whose strings are two-byte glyph ids
//! addressing the embedded program directly. No encoding table stands
//! between the bytes and the glyphs, so any script the font covers
//! works, and the only limit left is which glyphs the font actually
//! has.
//!
//! Embedding the whole program would add three quarters of a megabyte
//! to a document that needs about forty glyphs, so it is subset first.
//! A `/ToUnicode` map goes in beside it, which is what keeps the stamp
//! selectable, searchable and greppable — a watermark you cannot
//! extract text from is a poor leak trace.

use std::collections::HashSet;

use lopdf::{Document, Object, ObjectId, Stream, dictionary};
use subsetter::GlyphRemapper;
use ttf_parser::{Face, GlyphId, name_id};

use crate::Error;

/// DejaVu Sans, bundled so that the fix works without the caller
/// having to find a font first. Covers Latin (including the Extended-A
/// block that most of Europe needs), Greek, Cyrillic, Armenian,
/// Georgian and Hebrew. It has no CJK — that needs a font no crate can
/// reasonably carry, which is what [`crate::FontSource::Supplied`] is
/// for.
pub const BUNDLED: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

/// One line of text mapped through a font and ready to draw.
pub struct Shaped {
    /// Subset glyph ids in drawing order. Under `Identity-H` these are
    /// the string: two bytes each, big-endian.
    pub codes: Vec<u16>,
    /// The line's advance at one point, so width at size `s` is
    /// `per_pt * s`.
    pub per_pt: f32,
    /// The subset program and what the PDF needs to describe it.
    pub subset: Subset,
    /// Characters the font supplied that WinAnsi could not have. This
    /// is the entire reason to embed anything.
    pub gained: usize,
    /// Characters WinAnsi could have set that the font could not. Each
    /// one is a character embedding would turn *into* a `?` that the
    /// standard font would have drawn correctly.
    pub lost: usize,
}

/// A subset font program plus the descriptor values that go with it.
pub struct Subset {
    pub program: Vec<u8>,
    /// `ABCDEF+DejaVuSans`. The six-letter tag is required of a subset
    /// so that two documents carrying different subsets of one font
    /// cannot be mistaken for each other when merged.
    pub base_name: String,
    /// Advance per subset glyph id, in 1000ths of an em.
    pub widths: Vec<u16>,
    /// Subset glyph id to the character it stands for.
    pub to_unicode: Vec<(u16, char)>,
    pub bbox: [i32; 4],
    pub ascent: i32,
    pub descent: i32,
    pub cap_height: i32,
    pub italic_angle: f32,
}

/// Map `text` through `program`, subsetting it to just the glyphs used.
///
/// `simple` reports whether the standard font could have set a
/// character on its own, which is what makes the result comparable:
/// an embedded font is only worth having if it draws something the
/// standard one could not, and does not lose anything it could.
pub fn shape(program: &[u8], text: &str, simple: fn(char) -> bool) -> Result<Shaped, Error> {
    let face =
        Face::parse(program, 0).map_err(|_| Error::Font("the font program could not be read"))?;
    let upem = f32::from(face.units_per_em());
    if upem <= 0.0 {
        return Err(Error::Font("the font declares no units per em"));
    }

    // `.notdef` is glyph 0 in the subset whether or not it is used, so
    // the remapper starts with it and every id below is one we chose.
    let mut remapper = GlyphRemapper::new();
    let mut codes = Vec::with_capacity(text.len());
    let mut to_unicode: Vec<(u16, char)> = Vec::new();
    let mut seen: HashSet<u16> = HashSet::new();
    let mut gained = 0usize;
    let mut lost = 0usize;

    for c in text.chars() {
        // A newline inside a text object would not break out of it —
        // `Tj` takes one string — but it would still be a glyph lookup
        // for something with no business being drawn.
        let c = if c.is_control() { ' ' } else { c };
        let (gid, shown) = match face.glyph_index(c) {
            Some(g) => {
                if !simple(c) {
                    gained += 1;
                }
                (g, c)
            }
            None => {
                if simple(c) {
                    lost += 1;
                }
                // A visible mark the reader can see is wrong beats a
                // blank the reader reads straight past.
                match face.glyph_index('?') {
                    Some(g) => (g, '?'),
                    None => (GlyphId(0), '\u{FFFD}'),
                }
            }
        };
        let code = remapper.remap(gid.0);
        if seen.insert(code) {
            to_unicode.push((code, shown));
        }
        codes.push(code);
    }

    // Widths are indexed by subset id, and `remapped_gids` yields the
    // originals in exactly that order.
    let widths: Vec<u16> = remapper
        .remapped_gids()
        .map(|old| {
            scale(
                face.glyph_hor_advance(GlyphId(old)).unwrap_or(0).into(),
                upem,
            ) as u16
        })
        .collect();

    // Measured from the widths that were just written rather than from
    // the font, so what this crate centres on and what the reader
    // advances by are the same number.
    let per_pt = codes
        .iter()
        .map(|&c| f32::from(widths.get(usize::from(c)).copied().unwrap_or(0)))
        .sum::<f32>()
        / 1000.0;

    let program = subsetter::subset(program, 0, &remapper)
        .map_err(|_| Error::Font("the font could not be subset"))?;

    // A font usually carries this name twice, once for Macintosh and
    // once for Windows, and only the Windows record is Unicode. Taking
    // the first match rather than the first that decodes is how every
    // DejaVu subset ends up named `Embedded`.
    let ps_name = face
        .names()
        .into_iter()
        .filter(|n| n.name_id == name_id::POST_SCRIPT_NAME)
        .find_map(|n| n.to_string())
        .unwrap_or_else(|| "Embedded".to_string());
    // A PDF name is easier to trust without spaces or delimiters in it.
    let ps_name: String = ps_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let ps_name = if ps_name.is_empty() {
        "Embedded".to_string()
    } else {
        ps_name
    };

    let bb = face.global_bounding_box();
    to_unicode.sort_unstable_by_key(|(gid, _)| *gid);

    Ok(Shaped {
        codes,
        per_pt,
        gained,
        lost,
        subset: Subset {
            base_name: format!("{}+{}", tag(&ps_name, &remapper), ps_name),
            program,
            widths,
            to_unicode,
            bbox: [
                scale(bb.x_min.into(), upem),
                scale(bb.y_min.into(), upem),
                scale(bb.x_max.into(), upem),
                scale(bb.y_max.into(), upem),
            ],
            ascent: scale(face.ascender().into(), upem),
            descent: scale(face.descender().into(), upem),
            cap_height: scale(
                face.capital_height().unwrap_or(face.ascender()).into(),
                upem,
            ),
            italic_angle: face.italic_angle(),
        },
    })
}

/// Font units to the 1000ths of an em a PDF measures glyphs in.
fn scale(v: f32, upem: f32) -> i32 {
    (v * 1000.0 / upem).round() as i32
}

/// The six-letter subset tag, derived from the font and the exact set
/// of glyphs kept so that stamping the same text twice produces the
/// same name.
fn tag(name: &str, remapper: &GlyphRemapper) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in name.as_bytes() {
        eat(*b);
    }
    for g in remapper.remapped_gids() {
        for b in g.to_be_bytes() {
            eat(b);
        }
    }
    (0..6)
        .map(|i| char::from(b'A' + ((h >> (i * 8)) % 26) as u8))
        .collect()
}

/// Write the font into `doc` and return the `/Type0` font to reference.
pub fn embed(doc: &mut Document, s: &Subset) -> Result<ObjectId, Error> {
    let name = || Object::Name(s.base_name.as_bytes().to_vec());

    let mut file = Stream::new(
        // `/Length1` is the program's length before compression, which
        // a reader needs to hand the bytes to its font engine.
        dictionary! { "Length1" => Object::Integer(s.program.len() as i64) },
        s.program.clone(),
    );
    // A font program is the one thing here big enough to be worth
    // compressing, and it compresses very well.
    let _ = file.compress();
    let file_id = doc.add_object(file);

    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => name(),
        // Symbolic. The subset holds whichever glyphs the recipient's
        // name needed, which is by definition not a standard Latin set,
        // and claiming otherwise invites a reader to second-guess the
        // encoding we just went to the trouble of making unambiguous.
        "Flags" => Object::Integer(4),
        "FontBBox" => vec![
            Object::Integer(s.bbox[0].into()),
            Object::Integer(s.bbox[1].into()),
            Object::Integer(s.bbox[2].into()),
            Object::Integer(s.bbox[3].into()),
        ],
        "ItalicAngle" => Object::Real(s.italic_angle),
        "Ascent" => Object::Integer(s.ascent.into()),
        "Descent" => Object::Integer(s.descent.into()),
        "CapHeight" => Object::Integer(s.cap_height.into()),
        // Required, and only used for substitution when the program is
        // missing. Ours never is, so a mid-range value is honest enough.
        "StemV" => Object::Integer(80),
        "FontFile2" => Object::Reference(file_id),
    });

    let widths: Vec<Object> = s
        .widths
        .iter()
        .map(|w| Object::Integer(i64::from(*w)))
        .collect();
    let descendant = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType2",
        "BaseFont" => name(),
        "CIDSystemInfo" => dictionary! {
            "Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"),
            "Supplement" => Object::Integer(0),
        },
        "FontDescriptor" => Object::Reference(descriptor),
        "DW" => Object::Integer(1000),
        // Subset ids run 0..n, so one contiguous run states them all.
        "W" => vec![Object::Integer(0), Object::Array(widths)],
        // Character code, CID and glyph id are all the same number.
        "CIDToGIDMap" => "Identity",
    });

    let to_unicode = doc.add_object(
        Stream::new(dictionary! {}, to_unicode_cmap(&s.to_unicode)).with_compression(false),
    );

    Ok(doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => name(),
        "Encoding" => "Identity-H",
        "DescendantFonts" => vec![Object::Reference(descendant)],
        "ToUnicode" => Object::Reference(to_unicode),
    }))
}

/// The CMap that turns the glyph ids back into text, so the stamp can
/// be selected, searched and copied out of the finished document.
fn to_unicode_cmap(map: &[(u16, char)]) -> Vec<u8> {
    let mut s = String::with_capacity(512 + map.len() * 20);
    s.push_str(
        "/CIDInit /ProcSet findresource begin\n\
         12 dict begin\n\
         begincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n\
         /CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // A `bfchar` block holds at most 100 entries.
    for chunk in map.chunks(100) {
        s.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (gid, c) in chunk {
            s.push_str(&format!("<{gid:04X}> <"));
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                s.push_str(&format!("{unit:04X}"));
            }
            s.push_str(">\n");
        }
        s.push_str("endbfchar\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the real WinAnsi test. The exact repertoire is
    /// `stamp`'s business; what matters here is that `shape` is told
    /// which characters the standard font could have set on its own.
    fn latin1(c: char) -> bool {
        (c as u32) < 0x100
    }

    #[test]
    fn the_bundled_font_sets_the_scripts_it_claims_to() {
        for (script, text) in [
            ("Polish", "Łukasz Żółć"),
            ("Czech", "Dvořák"),
            ("Turkish", "Çağlar"),
            ("Greek", "Ωμέγα"),
            ("Cyrillic", "Привет"),
        ] {
            let shaped = shape(BUNDLED, text, latin1).unwrap();
            assert_eq!(shaped.lost, 0, "{script}: lost a character Latin-1 had");
            assert!(shaped.gained > 0, "{script}: gained nothing over Latin-1");
        }
    }

    #[test]
    fn the_bundled_font_has_no_cjk_and_reports_as_much() {
        // Not a defect — a face with CJK in it is tens of megabytes.
        // What matters is that it is detectable, so the caller is not
        // charged for a font program that draws question marks.
        let shaped = shape(BUNDLED, "田中太郎", latin1).unwrap();
        assert_eq!(shaped.gained, 0);
    }

    #[test]
    fn only_the_glyphs_actually_used_are_embedded() {
        let shaped = shape(BUNDLED, "Łukasz", latin1).unwrap();
        // Six distinct letters, plus `.notdef`, out of 6253.
        assert!(
            shaped.subset.widths.len() <= 8,
            "{:?}",
            shaped.subset.widths
        );
        assert!(
            shaped.subset.program.len() < BUNDLED.len() / 20,
            "subset is {} bytes of {}",
            shaped.subset.program.len(),
            BUNDLED.len()
        );
    }

    #[test]
    fn the_subset_is_named_after_the_font_it_came_from() {
        // A font states its PostScript name more than once — once for
        // Macintosh, once for Windows — and only the Windows record is
        // Unicode. Taking the first match rather than the first that
        // decodes named every DejaVu subset `Embedded`.
        let shaped = shape(BUNDLED, "Łukasz", latin1).unwrap();
        let (tag, name) = shaped
            .subset
            .base_name
            .split_once('+')
            .expect("a subset font is tagged");
        assert_eq!(name, "DejaVuSans");
        assert_eq!(tag.len(), 6);
        assert!(tag.chars().all(|c| c.is_ascii_uppercase()), "{tag}");
    }

    #[test]
    fn the_same_text_subsets_to_the_same_bytes_every_time() {
        let a = shape(BUNDLED, "Łukasz", latin1).unwrap();
        let b = shape(BUNDLED, "Łukasz", latin1).unwrap();
        assert_eq!(a.subset.base_name, b.subset.base_name);
        assert_eq!(a.subset.program, b.subset.program);
    }

    #[test]
    fn every_glyph_drawn_can_be_read_back_as_the_text_it_came_from() {
        // This is what keeps a stamp greppable. A watermark nobody can
        // extract text from is a poor leak trace.
        let text = "Łukasz Ωμέγα Привет";
        let shaped = shape(BUNDLED, text, latin1).unwrap();
        let map: std::collections::HashMap<u16, char> =
            shaped.subset.to_unicode.iter().copied().collect();
        let read: String = shaped.codes.iter().map(|c| map[c]).collect();
        assert_eq!(read, text);
    }

    #[test]
    fn the_cmap_names_every_code_and_closes_itself() {
        let shaped = shape(BUNDLED, "Łu", latin1).unwrap();
        let cmap = String::from_utf8(to_unicode_cmap(&shaped.subset.to_unicode)).unwrap();
        assert!(
            cmap.contains("begincmap") && cmap.contains("endcmap"),
            "{cmap}"
        );
        assert!(cmap.contains("beginbfchar"), "{cmap}");
        // Ł is U+0141, and the map has to say so.
        assert!(cmap.contains("0141"), "{cmap}");
        for (gid, _) in &shaped.subset.to_unicode {
            assert!(
                cmap.contains(&format!("<{gid:04X}>")),
                "{gid} missing:\n{cmap}"
            );
        }
    }

    #[test]
    fn a_character_the_font_lacks_counts_against_embedding_it() {
        // Told the standard font could set this, and the embedded one
        // cannot: embedding would be a step backwards.
        let shaped = shape(BUNDLED, "中", |_| true).unwrap();
        assert_eq!(shaped.lost, 1);
        assert_eq!(shaped.gained, 0);
    }

    #[test]
    fn widths_are_converted_out_of_the_fonts_own_units() {
        // DejaVu measures in 2048ths of an em and PDF wants 1000ths,
        // so a raw advance leaking through would be obvious.
        let shaped = shape(BUNDLED, "M", latin1).unwrap();
        let m = shaped.subset.widths[1];
        assert!((600..1200).contains(&m), "M came out {m}");
    }

    #[test]
    fn something_that_is_not_a_font_is_an_error_rather_than_a_panic() {
        assert!(matches!(
            shape(b"certainly not a font", "x", latin1),
            Err(Error::Font(_))
        ));
    }
}
