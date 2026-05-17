//! The visible watermark: template rendering, placement, and the
//! content stream that draws it.
//!
//! ## Why the stamp is drawn rather than annotated
//!
//! An annotation would be less work — it carries its own resources, so
//! no page dictionary has to be touched. It is also the first thing a
//! reader can switch off, and some viewers do not print annotations at
//! all. A leak trace that a menu item removes is not a leak trace, so
//! the stamp goes into the page's own content stream.
//!
//! ## Why the original content is wrapped
//!
//! The stamp is appended after the page's existing content, and a
//! content stream can end in any graphics state it likes: a clip, a
//! scale, a translation. Inheriting that would put the text somewhere
//! unpredictable, or hide it entirely. So `/Contents` becomes
//!
//! ```text
//!     [ <q> <the page's own streams...> <Q + the stamp> ]
//! ```
//!
//! which a reader concatenates into one stream. The leading `q` saves
//! the default state and the `Q` restores it, so the stamp always draws
//! in default user space no matter what the page did.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId, Stream, dictionary};
use time::{OffsetDateTime, UtcOffset};

use crate::{Error, FontSource, Placement, Recipient, VisibleWatermark, font, metrics};

/// Resource names for the things the stamp adds to a page. Long enough
/// that colliding with a name the document already uses would take
/// deliberate effort — overwriting the page's own `/F1` would change
/// how the page itself renders.
const FONT_NAME: &str = "PdfStampFont";
const GS_NAME: &str = "PdfStampGS";

/// The stamp's text, encoded for whichever font is going to draw it.
///
/// Everything downstream — measuring the line, centring it, writing it
/// into the content stream — works through this rather than through
/// bytes, because the two encodings disagree about what a byte is.
enum Text {
    /// WinAnsi codes, one per character, for the standard Helvetica.
    Simple(Vec<u8>),
    /// Subset glyph ids for an embedded `Identity-H` font.
    Cid { codes: Vec<u16>, per_pt: f32 },
}

impl Text {
    /// How wide the line is when set at `size`.
    fn width(&self, size: f32) -> f32 {
        match self {
            Self::Simple(bytes) => metrics::width(bytes, size),
            Self::Cid { per_pt, .. } => per_pt * size,
        }
    }

    /// The string as the content stream wants it.
    fn operand(&self) -> Vec<u8> {
        match self {
            Self::Simple(bytes) => literal(bytes),
            Self::Cid { codes, .. } => hex(codes),
        }
    }
}

/// Decide how the stamp will be set, and register the font it needs.
///
/// WinAnsi is tried first and kept whenever it is lossless, which it
/// is for every address, every timestamp, and most European names. It
/// costs one small dictionary and no font program, so a document only
/// pays for an embedded font when a recipient's name actually calls
/// for one.
fn choose(
    inc: &mut IncrementalDocument,
    text: &str,
    source: &FontSource,
) -> Result<(Text, ObjectId), Error> {
    let standard = |inc: &mut IncrementalDocument| {
        inc.new_document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        })
    };

    if let Some(bytes) = win_ansi_lossless(text) {
        let id = standard(inc);
        return Ok((Text::Simple(bytes), id));
    }
    let Some(program) = source.program() else {
        // Told not to embed anything. Set what WinAnsi can hold and
        // accept the question marks for the rest.
        let id = standard(inc);
        return Ok((Text::Simple(win_ansi(text)), id));
    };

    let shaped = font::shape(program, text, |c| win_ansi_byte(c).is_some())?;
    // Embed only when it helps more than it hurts. A Latin face asked
    // for a Japanese name gains nothing; a CJK fallback face has no
    // Latin at all, so using it would turn the address — the part that
    // actually identifies the recipient — into a row of question
    // marks to rescue four characters of the name.
    if shaped.gained == 0 || shaped.lost >= shaped.gained {
        let id = standard(inc);
        return Ok((Text::Simple(win_ansi(text)), id));
    }
    let id = font::embed(&mut inc.new_document, &shaped.subset)?;
    Ok((
        Text::Cid {
            codes: shaped.codes,
            per_pt: shaped.per_pt,
        },
        id,
    ))
}

/// Everything about a page that changes where the stamp goes.
#[derive(Clone, Copy, PartialEq)]
struct Geometry {
    llx: f32,
    lly: f32,
    width: f32,
    height: f32,
    rotate: i64,
}

impl Geometry {
    /// A key that can go in a map, so pages that share a geometry —
    /// which in a normal document means all of them — share one stamp
    /// stream instead of getting a byte-identical copy each.
    fn key(&self) -> (u32, u32, u32, u32, i64) {
        (
            self.llx.to_bits(),
            self.lly.to_bits(),
            self.width.to_bits(),
            self.height.to_bits(),
            self.rotate,
        )
    }

    /// The page as the reader sees it, after `/Rotate` is applied.
    fn display_size(&self) -> (f32, f32) {
        match self.rotate {
            90 | 270 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }

    /// Maps the displayed page's coordinates back into the user space
    /// the content stream draws in, so placement can be worked out
    /// against what the reader sees rather than against `/MediaBox`.
    ///
    /// Without this a footer on a `/Rotate 90` page would come out
    /// along one of the sides.
    fn base_matrix(&self) -> [f32; 6] {
        let (w, h) = (self.width, self.height);
        let m = match self.rotate {
            90 => [0.0, 1.0, -1.0, 0.0, w, 0.0],
            180 => [-1.0, 0.0, 0.0, -1.0, w, h],
            270 => [0.0, -1.0, 1.0, 0.0, 0.0, h],
            _ => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        };
        [m[0], m[1], m[2], m[3], m[4] + self.llx, m[5] + self.lly]
    }
}

/// Where a page's `/Resources` live, and therefore what has to be
/// rewritten to add a font to them.
enum ResTarget {
    /// A separate object, possibly shared with other pages.
    Object(ObjectId),
    /// A dictionary to install on the page itself — either the one
    /// already there, or a copy of one inherited from `/Pages`.
    Inline(Dictionary),
}

/// Draw `wm` on every page of the document being built.
pub fn apply(
    inc: &mut IncrementalDocument,
    recipient: &Recipient,
    wm: &VisibleWatermark,
    at: OffsetDateTime,
) -> Result<(), Error> {
    let rendered = render(&wm.template, recipient, at);
    if rendered
        .chars()
        .all(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::EmptyStamp);
    }

    let pages: Vec<ObjectId> = inc.get_prev_documents().get_pages().into_values().collect();
    if pages.is_empty() {
        return Err(Error::Structure("document has no pages to stamp"));
    }

    let (text, font_id) = choose(inc, &rendered, &wm.font)?;
    // An ExtGState is only needed when the stamp is translucent. Adding
    // one that sets `ca 1` would be a resource every page carries for
    // no effect.
    let gs_id = (wm.opacity < 1.0).then(|| {
        inc.new_document.add_object(dictionary! {
            "Type" => "ExtGState",
            "ca" => wm.opacity.clamp(0.0, 1.0),
            "CA" => wm.opacity.clamp(0.0, 1.0),
        })
    });
    let guard_id = inc
        .new_document
        .add_object(Stream::new(dictionary! {}, b"q\n".to_vec()).with_compression(false));

    let mut streams: HashMap<(u32, u32, u32, u32, i64), ObjectId> = HashMap::new();

    for page_id in pages {
        // Everything read from the previous document is collected first:
        // the borrow has to end before `new_document` can be written to.
        let (geom, contents, res) = {
            let prev = inc.get_prev_documents();
            (
                geometry(prev, page_id)?,
                contents_of(prev, page_id)?,
                resources_of(prev, page_id)?,
            )
        };

        let stamp_id = match streams.get(&geom.key()) {
            Some(id) => *id,
            None => {
                let body = content_stream(&text, wm, &geom, gs_id.is_some());
                let id = inc
                    .new_document
                    .add_object(Stream::new(dictionary! {}, body).with_compression(false));
                streams.insert(geom.key(), id);
                id
            }
        };

        let mut chain = Vec::with_capacity(contents.len() + 2);
        chain.push(Object::Reference(guard_id));
        chain.extend(contents);
        chain.push(Object::Reference(stamp_id));

        inc.opt_clone_object_to_new_document(page_id)?;

        match res {
            ResTarget::Object(id) => {
                inc.opt_clone_object_to_new_document(id)?;
                let dict = inc.new_document.get_object_mut(id)?.as_dict_mut()?;
                add_resources(dict, font_id, gs_id);
            }
            ResTarget::Inline(mut dict) => {
                add_resources(&mut dict, font_id, gs_id);
                inc.new_document
                    .get_object_mut(page_id)?
                    .as_dict_mut()?
                    .set("Resources", dict);
            }
        }

        inc.new_document
            .get_object_mut(page_id)?
            .as_dict_mut()?
            .set("Contents", Object::Array(chain));
    }
    Ok(())
}

/// Register the font — and the transparency state, when there is one —
/// under names of our own, leaving whatever the page already had.
fn add_resources(res: &mut Dictionary, font_id: ObjectId, gs_id: Option<ObjectId>) {
    if !res.has(b"Font") {
        res.set("Font", Dictionary::new());
    }
    if let Ok(fonts) = res.get_mut(b"Font").and_then(Object::as_dict_mut) {
        fonts.set(FONT_NAME, Object::Reference(font_id));
    }
    if let Some(gs_id) = gs_id {
        if !res.has(b"ExtGState") {
            res.set("ExtGState", Dictionary::new());
        }
        if let Ok(states) = res.get_mut(b"ExtGState").and_then(Object::as_dict_mut) {
            states.set(GS_NAME, Object::Reference(gs_id));
        }
    }
}

/// The page's box and rotation, walking `/Parent` for the ones it does
/// not state itself — both are inheritable, and a document that sets
/// `/MediaBox` once on `/Pages` is perfectly ordinary.
fn geometry(doc: &Document, page_id: ObjectId) -> Result<Geometry, Error> {
    let media =
        inherited(doc, page_id, b"MediaBox").ok_or(Error::Structure("page has no /MediaBox"))?;
    let nums = media
        .as_array()
        .map_err(|_| Error::Structure("/MediaBox is not an array"))?;
    if nums.len() != 4 {
        return Err(Error::Structure("/MediaBox does not have four numbers"));
    }
    let mut v = [0f32; 4];
    for (i, n) in nums.iter().enumerate() {
        v[i] = as_f32(doc, n).ok_or(Error::Structure("/MediaBox holds a non-number"))?;
    }
    // The spec does not require the corners in any particular order.
    let (llx, urx) = (v[0].min(v[2]), v[0].max(v[2]));
    let (lly, ury) = (v[1].min(v[3]), v[1].max(v[3]));
    if urx - llx <= 0.0 || ury - lly <= 0.0 {
        return Err(Error::Structure("/MediaBox has no area"));
    }

    let rotate = inherited(doc, page_id, b"Rotate")
        .and_then(|o| doc.dereference(&o).ok().and_then(|(_, o)| o.as_i64().ok()))
        .unwrap_or(0);
    // Only right angles are legal, and a reader rounds anything else.
    let rotate = ((rotate % 360) + 360) % 360;
    let rotate = if rotate % 90 == 0 { rotate } else { 0 };

    Ok(Geometry {
        llx,
        lly,
        width: urx - llx,
        height: ury - lly,
        rotate,
    })
}

/// Look `key` up on the page, then on each `/Parent` in turn.
fn inherited(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut id = page_id;
    // A malformed document can loop its `/Parent` chain back on itself;
    // the cap means that is a missing attribute rather than a hang.
    for _ in 0..64 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(value) = dict.get(key) {
            return Some(value.clone());
        }
        id = dict.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn as_f32(doc: &Document, o: &Object) -> Option<f32> {
    let (_, o) = doc.dereference(o).ok()?;
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

/// The page's existing content streams, as references to splice around.
fn contents_of(doc: &Document, page_id: ObjectId) -> Result<Vec<Object>, Error> {
    let dict = doc.get_dictionary(page_id)?;
    match dict.get(b"Contents") {
        // A page with no content is legal — a deliberately blank one.
        // It still gets a stamp.
        Err(_) => Ok(Vec::new()),
        Ok(Object::Reference(id)) => Ok(vec![Object::Reference(*id)]),
        Ok(Object::Array(items)) => Ok(items.clone()),
        Ok(_) => Err(Error::Structure(
            "/Contents is neither a reference nor an array",
        )),
    }
}

/// Where to put the font for this page.
fn resources_of(doc: &Document, page_id: ObjectId) -> Result<ResTarget, Error> {
    let dict = doc.get_dictionary(page_id)?;
    match dict.get(b"Resources") {
        Ok(Object::Reference(id)) => Ok(ResTarget::Object(*id)),
        Ok(Object::Dictionary(d)) => Ok(ResTarget::Inline(d.clone())),
        _ => {
            // Inherited, or absent. An inherited dictionary is copied
            // down onto this page rather than edited in place: editing
            // the ancestor would be editing every page that shares it,
            // and creating an empty one here would *shadow* the
            // inherited resources and blank the page.
            match inherited(doc, page_id, b"Resources") {
                Some(Object::Reference(id)) => Ok(ResTarget::Object(id)),
                Some(Object::Dictionary(d)) => Ok(ResTarget::Inline(d)),
                _ => Ok(ResTarget::Inline(Dictionary::new())),
            }
        }
    }
}

/// The operators that draw the stamp.
fn content_stream(text: &Text, wm: &VisibleWatermark, geom: &Geometry, use_gs: bool) -> Vec<u8> {
    let requested = if wm.font_size.is_finite() && wm.font_size > 0.0 {
        wm.font_size
    } else {
        9.0
    };
    let size = fit(text, requested, wm, geom);
    let (dw, dh) = geom.display_size();
    let mut s = String::with_capacity(256);

    // Close the `q` that was prepended ahead of the page's own content,
    // then open a state of our own.
    s.push_str("Q\nq\n");
    if use_gs {
        s.push_str(&format!("/{GS_NAME} gs\n"));
    }
    let (r, g, b) = wm.color;
    s.push_str(&format!(
        "{} {} {} rg\n",
        num(f32::from(r) / 255.0),
        num(f32::from(g) / 255.0),
        num(f32::from(b) / 255.0)
    ));

    let m = geom.base_matrix();
    if m != [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] {
        s.push_str(&format!(
            "{} {} {} {} {} {} cm\n",
            num(m[0]),
            num(m[1]),
            num(m[2]),
            num(m[3]),
            num(m[4]),
            num(m[5])
        ));
    }

    match wm.placement {
        Placement::Footer { margin_x, margin_y } => {
            s.push_str(&format!(
                "BT\n/{FONT_NAME} {} Tf\n{} {} Td\n",
                num(size),
                num(margin_x),
                num(margin_y)
            ));
        }
        Placement::Diagonal => {
            let theta = wm.rotation_deg.to_radians();
            let (sin, cos) = theta.sin_cos();
            s.push_str(&format!(
                "{} {} {} {} {} {} cm\n",
                num(cos),
                num(sin),
                num(-sin),
                num(cos),
                num(dw / 2.0),
                num(dh / 2.0)
            ));
            // Centred on the page centre: half the line's width to the
            // left, and a little under half the cap height down, which
            // puts the middle of the glyphs on the centre rather than
            // their baseline.
            let w = text.width(size);
            s.push_str(&format!(
                "BT\n/{FONT_NAME} {} Tf\n{} {} Td\n",
                num(size),
                num(-w / 2.0),
                num(-size * 0.35)
            ));
        }
    }

    let mut out = s.into_bytes();
    out.extend_from_slice(&text.operand());
    out.extend_from_slice(b" Tj\nET\nQ\n");
    out
}

/// Shrink the type until the line fits the space it is drawn in.
///
/// A watermark is there to be read. The default diagonal is 36pt,
/// which suits `Confidential — Alice` and runs off both edges of an A4
/// page the moment the template carries an address and a timestamp —
/// so the end of the line, which is the part saying *when*, is the
/// part that falls off. Shrinking keeps all of it on the page.
///
/// Text is measured, never truncated: half an address identifies
/// nobody.
fn fit(text: &Text, size: f32, wm: &VisibleWatermark, geom: &Geometry) -> f32 {
    let (dw, dh) = geom.display_size();
    let available = match wm.placement {
        // From the left margin to the mirror of it on the right.
        Placement::Footer { margin_x, .. } => dw - 2.0 * margin_x.max(0.0),
        Placement::Diagonal => {
            // A line through the centre at angle θ is bounded by the
            // page's width along one axis and its height along the
            // other; whichever runs out first sets the length.
            let (sin, cos) = wm.rotation_deg.to_radians().sin_cos();
            let by_width = if cos.abs() > 1e-6 {
                dw / cos.abs()
            } else {
                f32::INFINITY
            };
            let by_height = if sin.abs() > 1e-6 {
                dh / sin.abs()
            } else {
                f32::INFINITY
            };
            // Back off the edges rather than touching them.
            by_width.min(by_height) * 0.92
        }
    };
    if !available.is_finite() || available <= 0.0 {
        return size;
    }
    let width = text.width(size);
    if width <= available {
        size
    } else {
        // Width scales linearly with the size, so this lands exactly
        // on `available`.
        size * available / width
    }
}

/// Substitute the placeholders in a watermark template.
///
/// Everything time-related is UTC, whatever offset the caller's clock
/// carries — a stamp that says `14:02` without saying where is not
/// evidence of anything.
///
/// Recognised: `{name}`, `{email}`, `{identifier}`, `{date}`, `{time}`,
/// `{timezone}`, `{datetime}`. Anything else is left exactly as written,
/// so a typo shows up in the output instead of vanishing.
pub fn render(template: &str, recipient: &Recipient, at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    let date = format!(
        "{:04}-{:02}-{:02}",
        at.year(),
        u8::from(at.month()),
        at.day()
    );
    let time = format!("{:02}:{:02}:{:02}", at.hour(), at.minute(), at.second());
    let datetime = format!("{date} {time} UTC");

    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // No closing brace: the rest is literal text.
            out.push_str(&rest[open..]);
            return out;
        };
        let key = &after[..close];
        let value = match key {
            "name" => Some(recipient.name.clone()),
            "email" => Some(recipient.email.clone().unwrap_or_default()),
            "identifier" => Some(recipient.identifier.clone()),
            "date" => Some(date.clone()),
            "time" => Some(time.clone()),
            "timezone" => Some("UTC".to_string()),
            "datetime" => Some(datetime.clone()),
            _ => None,
        };
        match value {
            // The substituted value is pushed, never rescanned, so a
            // name containing a brace cannot turn into a placeholder.
            Some(v) => out.push_str(&v),
            None => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The WinAnsi code for `c`, if WinAnsi has one.
///
/// WinAnsi and Latin-1 agree everywhere except 0x80..=0x9F, where
/// Latin-1 keeps the C1 controls and WinAnsi puts printable characters
/// — the curly quotes, the dashes, the euro. That range is the whole
/// reason this is a table rather than a cast.
fn win_ansi_byte(c: char) -> Option<u8> {
    Some(match c {
        '\n' | '\r' | '\t' => b' ',
        c if (c as u32) < 0x20 => b' ',
        c if (c as u32) < 0x7F => c as u8,
        '\u{20AC}' => 0x80,
        '\u{201A}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201E}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02C6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8A,
        '\u{2039}' => 0x8B,
        '\u{0152}' => 0x8C,
        '\u{017D}' => 0x8E,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201C}' => 0x93,
        '\u{201D}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02DC}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9A,
        '\u{203A}' => 0x9B,
        '\u{0153}' => 0x9C,
        '\u{017E}' => 0x9E,
        '\u{0178}' => 0x9F,
        c if (0xA0..=0xFF).contains(&(c as u32)) => c as u8,
        _ => return None,
    })
}

/// Encode to WinAnsi, or decline if any character would be lost.
///
/// `None` is the signal to go and embed a font: it means this text
/// cannot be set in the standard one without throwing a character
/// away, and the character it would throw away is part of somebody's
/// name.
fn win_ansi_lossless(s: &str) -> Option<Vec<u8>> {
    s.chars().map(win_ansi_byte).collect()
}

/// Encode to WinAnsi, accepting `?` for whatever will not fit.
///
/// Only reached when the caller has asked for no embedded font, by
/// way of [`crate::FontSource::None`].
fn win_ansi(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| win_ansi_byte(c).unwrap_or(b'?'))
        .collect()
}

/// A PDF literal string: the bytes, with the three characters that
/// would end or confuse it escaped.
fn literal(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 2);
    out.push(b'(');
    for &b in text {
        match b {
            b'(' | b')' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            _ => out.push(b),
        }
    }
    out.push(b')');
    out
}

/// A PDF hex string, which is how the glyph ids of an `Identity-H`
/// font go into a content stream: two bytes each, and no escaping
/// rules to worry about whatever those bytes turn out to be.
fn hex(codes: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(codes.len() * 4 + 2);
    out.push(b'<');
    for code in codes {
        out.extend_from_slice(format!("{code:04X}").as_bytes());
    }
    out.push(b'>');
    out
}

/// A number a PDF reader will accept: no exponent, no `NaN`, no
/// trailing noise.
fn num(v: f32) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let mut s = format!("{v:.4}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".to_string();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// A stamp set in the standard font, which is what most of these
    /// tests are about.
    fn t(s: &str) -> Text {
        Text::Simple(win_ansi(s))
    }

    fn recipient() -> Recipient {
        Recipient {
            name: "Alice Cooper".into(),
            identifier: "uuid-abc-123".into(),
            email: Some("alice@example.com".into()),
        }
    }

    const AT: OffsetDateTime = datetime!(2026-11-05 21:48:03 UTC);

    #[test]
    fn every_placeholder_is_substituted() {
        let out = render(
            "{name}|{email}|{identifier}|{date}|{time}|{timezone}|{datetime}",
            &recipient(),
            AT,
        );
        assert_eq!(
            out,
            "Alice Cooper|alice@example.com|uuid-abc-123|\
             2026-11-05|21:48:03|UTC|2026-11-05 21:48:03 UTC"
        );
    }

    #[test]
    fn the_stamp_is_utc_whatever_offset_it_is_given() {
        // Same instant, written with a +02:00 offset. The stamp must
        // still read 19:48:03 UTC, not 21:48:03 local.
        let local = datetime!(2026-11-05 23:48:03 +02:00);
        assert_eq!(
            render("{datetime}", &recipient(), local),
            "2026-11-05 21:48:03 UTC"
        );
    }

    #[test]
    fn an_unknown_placeholder_survives_instead_of_vanishing() {
        assert_eq!(
            render("a {emial} b {name}", &recipient(), AT),
            "a {emial} b Alice Cooper"
        );
    }

    #[test]
    fn an_unclosed_brace_is_literal_text() {
        assert_eq!(render("tail {name", &recipient(), AT), "tail {name");
    }

    #[test]
    fn a_missing_email_renders_empty_rather_than_the_placeholder() {
        let r = Recipient {
            email: None,
            ..recipient()
        };
        assert_eq!(render("[{email}]", &r, AT), "[]");
    }

    #[test]
    fn a_brace_inside_a_value_is_not_rescanned() {
        let r = Recipient {
            name: "{date}".into(),
            ..recipient()
        };
        assert_eq!(render("{name}", &r, AT), "{date}");
    }

    #[test]
    fn win_ansi_maps_latin1_and_the_windows_range() {
        assert_eq!(win_ansi("Søren"), vec![b'S', 0xF8, b'r', b'e', b'n']);
        assert_eq!(win_ansi("\u{2013}"), vec![0x96]); // en dash
        assert_eq!(win_ansi("\u{20AC}"), vec![0x80]); // euro
    }

    #[test]
    fn a_character_win_ansi_cannot_hold_becomes_a_question_mark() {
        assert_eq!(win_ansi("中"), vec![b'?']);
    }

    #[test]
    fn win_ansi_declines_text_it_would_have_to_damage() {
        // Everything Latin-1 covers is set in the standard font, which
        // is the whole point of trying it first.
        assert_eq!(
            win_ansi_lossless("Søren · 2026"),
            Some(win_ansi("Søren · 2026"))
        );
        // A question mark the caller wrote is not a lost character.
        assert_eq!(win_ansi_lossless("why?"), Some(b"why?".to_vec()));
        // These are: Latin Extended-A and CJK both need a font.
        assert_eq!(win_ansi_lossless("Łukasz"), None);
        assert_eq!(win_ansi_lossless("田中"), None);
    }

    #[test]
    fn glyph_ids_go_out_as_two_byte_hex() {
        assert_eq!(hex(&[1, 0x4E2D, 0xFFFF]), b"<00014E2DFFFF>".to_vec());
        assert_eq!(hex(&[]), b"<>".to_vec());
    }

    #[test]
    fn an_embedded_line_is_measured_from_its_own_advances() {
        // Helvetica's metrics say nothing about a font we embedded, so
        // the width has to come from the glyphs that were actually used.
        let text = Text::Cid {
            codes: vec![1, 2],
            per_pt: 1.25,
        };
        assert_eq!(text.width(8.0), 10.0);
    }

    #[test]
    fn newlines_cannot_break_out_of_the_text_object() {
        assert_eq!(win_ansi("a\nb"), vec![b'a', b' ', b'b']);
    }

    #[test]
    fn a_string_cannot_be_closed_early() {
        assert_eq!(literal(b"a)b(c\\d"), b"(a\\)b\\(c\\\\d)".to_vec());
    }

    #[test]
    fn numbers_never_come_out_in_exponent_form() {
        assert_eq!(num(0.0000001), "0");
        assert_eq!(num(1e9), "1000000000");
        assert_eq!(num(f32::NAN), "0");
        assert_eq!(num(0.45), "0.45");
        assert_eq!(num(595.0), "595");
    }

    /// The matrix has to put the displayed page's bottom-left corner
    /// where the reader sees it, for each rotation.
    fn map(g: &Geometry, x: f32, y: f32) -> (f32, f32) {
        let m = g.base_matrix();
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    }

    fn geom(rotate: i64) -> Geometry {
        Geometry {
            llx: 0.0,
            lly: 0.0,
            width: 400.0,
            height: 800.0,
            rotate,
        }
    }

    #[test]
    fn an_unrotated_page_needs_no_transform() {
        assert_eq!(geom(0).base_matrix(), [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(map(&geom(0), 10.0, 20.0), (10.0, 20.0));
    }

    #[test]
    fn rotation_moves_the_origin_to_the_right_corner() {
        // Clockwise 90: what the reader sees bottom-left is the
        // unrotated page's bottom-right.
        assert_eq!(map(&geom(90), 0.0, 0.0), (400.0, 0.0));
        // 180: the top-right.
        assert_eq!(map(&geom(180), 0.0, 0.0), (400.0, 800.0));
        // 270: the top-left.
        assert_eq!(map(&geom(270), 0.0, 0.0), (0.0, 800.0));
    }

    #[test]
    fn a_rotated_page_swaps_its_displayed_sides() {
        assert_eq!(geom(0).display_size(), (400.0, 800.0));
        assert_eq!(geom(90).display_size(), (800.0, 400.0));
        assert_eq!(geom(270).display_size(), (800.0, 400.0));
        assert_eq!(geom(180).display_size(), (400.0, 800.0));
    }

    #[test]
    fn a_media_box_away_from_the_origin_shifts_the_stamp_with_it() {
        let g = Geometry {
            llx: 20.0,
            lly: 30.0,
            ..geom(0)
        };
        assert_eq!(map(&g, 0.0, 0.0), (20.0, 30.0));
    }

    #[test]
    fn the_stream_closes_the_guard_and_balances_its_own_state() {
        let wm = VisibleWatermark::footer();
        let s = content_stream(&t("hi"), &wm, &geom(0), false);
        let s = String::from_utf8(s).unwrap();
        assert!(s.starts_with("Q\nq\n"), "{s}");
        assert!(s.ends_with("ET\nQ\n"), "{s}");
        assert_eq!(s.matches(" Tj").count(), 1, "{s}");
        // No ExtGState was asked for, so none is referenced.
        assert!(!s.contains("gs"), "{s}");
    }

    #[test]
    fn a_translucent_stamp_references_the_graphics_state() {
        let wm = VisibleWatermark::default(); // diagonal, opacity 0.18
        let s = String::from_utf8(content_stream(&t("hi"), &wm, &geom(0), true)).unwrap();
        assert!(s.contains(&format!("/{GS_NAME} gs")), "{s}");
    }

    #[test]
    fn a_diagonal_stamp_is_centred_on_the_page() {
        let wm = VisibleWatermark::default();
        let s = String::from_utf8(content_stream(&t("hi"), &wm, &geom(0), true)).unwrap();
        // Rotation is about the centre of the displayed page.
        assert!(s.contains("200 400 cm"), "{s}");
        // and the text is pulled back by half its own width.
        let half = metrics::width(b"hi", wm.font_size) / 2.0;
        assert!(s.contains(&format!("{} ", num(-half))), "{s}");
    }

    #[test]
    fn a_line_that_already_fits_is_left_at_its_requested_size() {
        let wm = VisibleWatermark::footer();
        assert_eq!(fit(&t("short"), wm.font_size, &wm, &geom(0)), wm.font_size);
    }

    #[test]
    fn a_footer_too_long_for_the_page_is_shrunk_to_the_margins() {
        let wm = VisibleWatermark::footer();
        let long = "x".repeat(400);
        let got = fit(&t(&long), wm.font_size, &wm, &geom(0));
        assert!(got < wm.font_size, "{got}");
        // 400 - 2*60 = 280pt of room between the margins.
        assert!((metrics::width(long.as_bytes(), got) - 280.0).abs() < 0.5);
    }

    #[test]
    fn a_diagonal_is_measured_against_the_diagonal_not_the_width() {
        let wm = VisibleWatermark::default(); // -30 degrees
        let text = "alice@example.com - 2026-11-05 21:48:03 UTC";
        let got = fit(&t(text), wm.font_size, &wm, &geom(0));
        let w = metrics::width(text.as_bytes(), got);
        // It must fit the page both across and up, with the 8% inset.
        let (sin, cos) = wm.rotation_deg.to_radians().sin_cos();
        assert!(w * cos.abs() <= 400.0, "runs off the sides");
        assert!(w * sin.abs() <= 800.0, "runs off the top");
        assert!(got < wm.font_size, "should have been shrunk: {got}");
    }

    #[test]
    fn a_stamp_laid_along_the_page_is_bounded_by_its_height_not_its_width() {
        // Rotated to vertical: the limit is now the long side.
        let wm = VisibleWatermark {
            rotation_deg: 90.0,
            ..VisibleWatermark::default()
        };
        let long = "y".repeat(120);
        let got = fit(&t(&long), wm.font_size, &wm, &geom(0));
        assert!((metrics::width(long.as_bytes(), got) - 800.0 * 0.92).abs() < 1.0);
    }

    #[test]
    fn a_zero_font_size_falls_back_rather_than_vanishing() {
        let wm = VisibleWatermark {
            font_size: 0.0,
            ..VisibleWatermark::footer()
        };
        let s = String::from_utf8(content_stream(&t("hi"), &wm, &geom(0), false)).unwrap();
        assert!(s.contains("9 Tf"), "{s}");
    }
}
