//! Per-recipient personalisation of cached PDF documents.
//!
//! markdoc-pdf renders the canonical PDF once and caches it. When a
//! specific person requests a copy, this crate walks the cached
//! bytes and stamps two things on top:
//!
//! - A **visible watermark** — a footer line, or text across the page
//!   diagonal — carrying who downloaded it and when, so a leaked copy
//!   is obviously personalised.
//! - **Invisible metadata** (custom `/Info` fields) carrying the
//!   recipient identifier and a render timestamp so leaks can be
//!   traced back to the recipient even if the visible mark is
//!   cropped away.
//!
//! The public entry point is [`personalise`]: it takes the bytes of
//! a PDF plus a [`Recipient`], applies the configured stamp, and
//! returns the modified bytes.
//!
//! ## The output is the input, plus a few kilobytes
//!
//! Nothing is re-rendered and nothing is re-serialised. PDF allows a
//! file to be extended with an *incremental update*: the original
//! bytes, then the objects that changed, then a cross-reference
//! section pointing back at the old one. So the canonical bytes are
//! still there, byte for byte, at the front of every personalised
//! copy, and a 62-page manual grows by about one percent.
//!
//! That matters for more than speed. Anything this crate does not
//! understand — the outline, link annotations, embedded fonts, images,
//! a structure tree — is carried through intact because it is never
//! parsed, let alone rewritten. A full round-trip through any PDF
//! library is a chance to lose one of them.
//!
//! ## What a watermark is and is not
//!
//! It is attribution, not protection. Anyone who wants the mark gone
//! can remove it, and nothing here pretends otherwise. What it buys is
//! that a copy found where it should not be names the account that
//! downloaded it, and that passing one on stops feeling anonymous.
//!
//! Note also that stamping an address onto a document means that
//! address travels with every copy the recipient forwards. That is the
//! point of the feature, and it is personal data moving outside your
//! control: it belongs in whatever policy governs the download.

mod font;
mod metrics;
mod stamp;

use std::sync::Arc;

use lopdf::{Dictionary, Document, IncrementalDocument, Object, dictionary};
use thiserror::Error;
use time::OffsetDateTime;

/// Identity stamped onto a personalised copy. Drives both the visible
/// watermark text and the invisible metadata fields used to trace
/// leaks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Recipient {
    /// Human-readable name. May be empty when all you have is an
    /// address — a template that does not mention `{name}` does not
    /// care.
    pub name: String,
    /// Stable, unique identifier (UUID, account ID, etc.). Stored in
    /// the invisible `/Info` fields and is what investigators match
    /// leaked copies against.
    pub identifier: String,
    /// Optional email — rendered by the `{email}` placeholder.
    pub email: Option<String>,
}

/// Knobs that shape one personalisation pass. Construct with
/// [`Options::default`] and tweak fields, or build a fresh struct
/// when every value matters.
#[derive(Debug, Clone)]
pub struct Options {
    pub visible: Option<VisibleWatermark>,
    pub invisible: InvisibleMetadata,
    /// Optional "phone-home on open" check that pings Adeptus for
    /// the latest published version of this document. See
    /// [`FreshnessCheck`] for caveats — only fires in viewers that
    /// permit PDF JavaScript (Adobe Acrobat / Reader DC).
    pub freshness: Option<FreshnessCheck>,
    /// When set, replaces the canonical PDF's `/CreationDate` with
    /// the request time. Defaults to `false` so leak traces preserve
    /// the original render timestamp.
    pub stamp_creation_date: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            visible: Some(VisibleWatermark::default()),
            invisible: InvisibleMetadata::default(),
            freshness: None,
            stamp_creation_date: false,
        }
    }
}

/// A "phone-home on open" version check. Stamped as a document-level
/// JavaScript open action: when the PDF is opened in a viewer that
/// supports PDF JS (essentially Adobe Acrobat / Reader DC), the
/// script fires an HTTP `GET` against `url_template` with the
/// document id and version substituted in, then warns the user via
/// `app.alert(...)` if Adeptus reports that a newer version exists.
///
/// **Best-effort by design.** If Adeptus is unreachable, the network
/// call throws inside the JS try/catch and the script silently exits
/// — the user sees nothing. If the viewer doesn't run PDF JS at all
/// (Chrome's pdfjs, most mobile readers, browser previews), the check
/// is silently skipped. So an outdated badge can never be a *blocker*,
/// only an advisory.
///
/// **Privacy note.** This sends an outbound request the first time a
/// recipient opens the PDF (and on every subsequent open). Disclose
/// it in the document's distribution policy.
#[derive(Debug, Clone)]
pub struct FreshnessCheck {
    /// URL template — `{doc_id}` and `{doc_version}` are substituted
    /// at stamp time. Example:
    /// `"https://adeptus.example.com/api/freshness?id={doc_id}&v={doc_version}"`.
    pub url_template: String,
    /// Adeptus document identifier baked into the URL.
    pub doc_id: String,
    /// Document version at the time the canonical PDF was rendered.
    pub doc_version: String,
    /// Alert message shown when the check reports the PDF is stale.
    /// Ends up inside `app.alert(...)` — single line, no newlines.
    pub outdated_message: String,
}

impl FreshnessCheck {
    pub fn new(
        url_template: impl Into<String>,
        doc_id: impl Into<String>,
        doc_version: impl Into<String>,
    ) -> Self {
        Self {
            url_template: url_template.into(),
            doc_id: doc_id.into(),
            doc_version: doc_version.into(),
            outdated_message:
                "This document is out of date. Please download the latest version from your portal."
                    .into(),
        }
    }
}

/// Where on the page the visible watermark is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// One line along the bottom, `margin_x` in from the left edge and
    /// `margin_y` up from the bottom, measured on the page as the
    /// reader sees it.
    ///
    /// The quiet option: it carries the same information as a banner
    /// without covering the document. For something people have to
    /// read and work from — a manual, a checklist — that is usually
    /// what you want.
    Footer { margin_x: f32, margin_y: f32 },
    /// Text across the page diagonal, centred and usually translucent.
    /// Unmissable, and it sits on top of the content.
    Diagonal,
}

/// Where the stamp gets glyphs for characters the standard font
/// cannot encode.
///
/// Whenever the text fits WinAnsi — which an address and a timestamp
/// always do — the stamp is set in Helvetica, one of the fourteen
/// fonts every reader is required to have, and no font program is
/// embedded at all. This decides what happens to the text that does
/// not fit: a name written in Polish, Greek, Russian or Japanese.
#[derive(Clone, Default)]
pub enum FontSource {
    /// DejaVu Sans, bundled with this crate. Covers Latin — including
    /// the Extended-A block most of Europe needs — Greek, Cyrillic,
    /// Armenian, Georgian and Hebrew.
    ///
    /// It has no CJK, because a face that does is tens of megabytes
    /// and does not belong inside a library. For those, supply one.
    #[default]
    Bundled,
    /// A TrueType or OpenType program supplied by the caller, for
    /// scripts the bundled font does not cover. Only the glyphs the
    /// stamp actually uses are embedded, so a 20 MB CJK face still
    /// costs a few kilobytes per document.
    Supplied(Arc<Vec<u8>>),
    /// Embed nothing, and let characters outside WinAnsi fall back to
    /// `?`. Smaller output, and a name nobody can read.
    None,
}

impl FontSource {
    /// The program to embed, when there is one.
    pub(crate) fn program(&self) -> Option<&[u8]> {
        match self {
            Self::Bundled => Some(font::BUNDLED),
            Self::Supplied(bytes) => Some(bytes.as_slice()),
            Self::None => None,
        }
    }
}

/// Spelled out by hand so that `{:?}` on a watermark does not print
/// three quarters of a megabyte of font.
impl std::fmt::Debug for FontSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bundled => f.write_str("Bundled"),
            Self::Supplied(bytes) => write!(f, "Supplied({} bytes)", bytes.len()),
            Self::None => f.write_str("None"),
        }
    }
}

/// The overlay drawn on every page.
///
/// Templates use `{name}`, `{email}`, `{identifier}`, `{date}`,
/// `{time}`, `{timezone}` and `{datetime}`. Times are always UTC.
#[derive(Debug, Clone)]
pub struct VisibleWatermark {
    pub template: String,
    pub font_size: f32,
    /// `0.0`–`1.0`. At `1.0` no transparency state is added at all.
    pub opacity: f32,
    /// Only used by [`Placement::Diagonal`].
    pub rotation_deg: f32,
    /// `(r, g, b)` 0..=255.
    pub color: (u8, u8, u8),
    pub placement: Placement,
    /// Where glyphs come from when the text needs characters WinAnsi
    /// cannot hold. Only consulted when it does.
    pub font: FontSource,
}

impl VisibleWatermark {
    /// A discreet line along the bottom of every page naming who
    /// downloaded it and when.
    pub fn footer() -> Self {
        Self {
            template: "Downloaded by {email} · {datetime}".into(),
            font_size: 7.0,
            opacity: 1.0,
            rotation_deg: 0.0,
            color: (115, 115, 115),
            placement: Placement::Footer {
                margin_x: 60.0,
                margin_y: 26.0,
            },
            font: FontSource::default(),
        }
    }

    /// Draw the stamp with a caller-supplied font instead of the
    /// bundled one, for a recipient whose name the bundled font cannot
    /// set — anything CJK, most obviously.
    ///
    /// The font is only reached for when the text needs it, so passing
    /// a large face costs nothing on the ordinary ASCII download.
    pub fn with_font(mut self, program: impl Into<Vec<u8>>) -> Self {
        self.font = FontSource::Supplied(Arc::new(program.into()));
        self
    }
}

impl Default for VisibleWatermark {
    fn default() -> Self {
        Self {
            template: "Confidential — {name}".into(),
            font_size: 36.0,
            opacity: 0.18,
            rotation_deg: -30.0,
            color: (180, 180, 180),
            placement: Placement::Diagonal,
            font: FontSource::default(),
        }
    }
}

/// Invisible metadata written to the PDF's `/Info` dictionary. Each
/// field that is set ends up as a separate `/Info` entry under a
/// custom key prefix so the fields are easy to grep for in a leaked
/// copy.
#[derive(Debug, Clone, Default)]
pub struct InvisibleMetadata {
    pub recipient_id: bool,
    pub recipient_email: bool,
    pub stamped_at: bool,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid PDF input: {0}")]
    InvalidInput(#[from] lopdf::Error),
    #[error("output write failed: {0}")]
    Output(#[from] std::io::Error),
    #[error("structural mismatch: {0}")]
    Structure(&'static str),
    /// The template rendered to nothing. Returned rather than stamping
    /// a blank line: a watermark that silently is not there is worse
    /// than a download that fails loudly.
    #[error("the watermark template produced no text")]
    EmptyStamp,
    /// The font that was to be embedded could not be read or subset.
    /// Only ever raised on the path that needs one — a stamp that
    /// fits WinAnsi never touches a font program.
    #[error("the fallback font could not be used: {0}")]
    Font(&'static str),
    /// Appending plaintext objects to an encrypted document would
    /// produce a file whose new content the reader tries to decrypt.
    #[error("the input is encrypted; decrypt it before stamping")]
    Encrypted,
}

/// Render a watermark template against a recipient, at the current
/// instant, without touching a PDF.
///
/// The portal's HTML pages carry the same line as the PDFs do, printed
/// from a stylesheet rather than drawn into a content stream. Letting
/// that line be formatted somewhere else is how the two come to
/// disagree about what a timestamp looks like — or about whether it is
/// UTC. Same template syntax, same renderer, same answer.
pub fn render_template(template: &str, recipient: &Recipient) -> String {
    render_template_at(template, recipient, OffsetDateTime::now_utc())
}

/// [`render_template`], with the instant supplied rather than read from
/// the clock.
pub fn render_template_at(template: &str, recipient: &Recipient, at: OffsetDateTime) -> String {
    stamp::render(template, recipient, at)
}

/// Apply one personalisation pass to `input` and return the modified
/// PDF bytes. Pure function — does not touch the filesystem.
///
/// The timestamp is read from the system clock in UTC. Use
/// [`personalise_at`] to supply it.
pub fn personalise(
    input: &[u8],
    recipient: &Recipient,
    options: &Options,
) -> Result<Vec<u8>, Error> {
    personalise_at(input, recipient, options, OffsetDateTime::now_utc())
}

/// [`personalise`], with the instant supplied rather than read from the
/// clock.
///
/// One instant is threaded through every stamp in the pass, so the
/// visible line and `Stamp.StampedAt` cannot disagree — which they
/// would, now and then, if each read the clock for itself.
pub fn personalise_at(
    input: &[u8],
    recipient: &Recipient,
    options: &Options,
    at: OffsetDateTime,
) -> Result<Vec<u8>, Error> {
    let prev = Document::load_mem(input)?;
    if prev.trailer.has(b"Encrypt") {
        return Err(Error::Encrypted);
    }
    let version = prev.version.clone();

    let mut inc = IncrementalDocument::create_from(input.to_vec(), prev);
    // Otherwise the update announces itself as PDF 1.4 regardless of
    // what the document actually is.
    inc.new_document.version = version;

    if let Some(wm) = &options.visible {
        stamp::apply(&mut inc, recipient, wm, at)?;
    }
    apply_invisible_metadata(
        &mut inc,
        recipient,
        &options.invisible,
        options.stamp_creation_date,
        at,
    )?;
    if let Some(check) = &options.freshness {
        apply_freshness_check(&mut inc, check)?;
    }

    let mut out = Vec::new();
    inc.save_to(&mut out)?;
    Ok(out)
}

const META_PREFIX: &str = "Stamp.";

fn apply_invisible_metadata(
    inc: &mut IncrementalDocument,
    recipient: &Recipient,
    flags: &InvisibleMetadata,
    stamp_creation_date: bool,
    at: OffsetDateTime,
) -> Result<(), Error> {
    if !flags.recipient_id && !flags.recipient_email && !flags.stamped_at && !stamp_creation_date {
        return Ok(());
    }

    // `/Info` is normally an indirect reference. A direct dictionary is
    // out of spec but readable, so it is copied into an object of its
    // own rather than discarded — replacing it outright would throw
    // away the title and producer the document already carries.
    enum Info {
        Existing(lopdf::ObjectId),
        Fresh(Dictionary),
    }
    let found = match inc.get_prev_documents().trailer.get(b"Info") {
        Ok(Object::Reference(id)) => Info::Existing(*id),
        Ok(Object::Dictionary(d)) => Info::Fresh(d.clone()),
        _ => Info::Fresh(Dictionary::new()),
    };
    let info_id = match found {
        Info::Existing(id) => {
            inc.opt_clone_object_to_new_document(id)?;
            id
        }
        Info::Fresh(d) => {
            let id = inc.new_document.add_object(d);
            inc.new_document.trailer.set("Info", Object::Reference(id));
            id
        }
    };

    let info = inc.new_document.get_object_mut(info_id)?.as_dict_mut()?;
    if flags.recipient_id {
        info.set(
            format!("{META_PREFIX}RecipientID").as_bytes().to_vec(),
            Object::string_literal(recipient.identifier.as_str()),
        );
    }
    if flags.recipient_email
        && let Some(email) = &recipient.email
    {
        info.set(
            format!("{META_PREFIX}RecipientEmail").as_bytes().to_vec(),
            Object::string_literal(email.as_str()),
        );
    }
    if flags.stamped_at {
        info.set(
            format!("{META_PREFIX}StampedAt").as_bytes().to_vec(),
            Object::string_literal(iso_date(at).as_str()),
        );
    }
    if stamp_creation_date {
        info.set(
            "CreationDate",
            Object::string_literal(pdf_date(at).as_str()),
        );
    }
    Ok(())
}

/// JavaScript template for the freshness check. Two placeholders are
/// substituted by Rust before injection: `__URL__` and `__MSG__`.
/// Wrapped in a `try/catch` so any failure (network, bad response,
/// no JS support, viewer sandbox restrictions) silently no-ops —
/// the document stays usable.
const FRESHNESS_JS: &str = r#"try {
  var url = "__URL__";
  var msg = "__MSG__";
  if (typeof Net !== 'undefined' && Net.HTTP && Net.HTTP.request) {
    Net.HTTP.request({
      cVerb: "GET",
      cURL: url,
      oRequest: {},
      oHandler: {
        response: function(msgObj, uri, oRequest) {
          try {
            var body = msgObj.response;
            if (body && body.indexOf("\"outdated\":true") !== -1) {
              app.alert({ cMsg: msg, cTitle: "Outdated document", nIcon: 1 });
            }
          } catch (e) {}
        }
      }
    });
  }
} catch (e) {}"#;

fn apply_freshness_check(
    inc: &mut IncrementalDocument,
    check: &FreshnessCheck,
) -> Result<(), Error> {
    let url = check
        .url_template
        .replace("{doc_id}", &js_escape(&check.doc_id))
        .replace("{doc_version}", &js_escape(&check.doc_version));
    let js = FRESHNESS_JS
        .replace("__URL__", &js_escape(&url))
        .replace("__MSG__", &js_escape(&check.outdated_message));

    let catalog_id = match inc.get_prev_documents().trailer.get(b"Root") {
        Ok(Object::Reference(id)) => *id,
        _ => return Err(Error::Structure("trailer /Root missing")),
    };
    inc.opt_clone_object_to_new_document(catalog_id)?;

    // Build an `/Action` of subtype `/JavaScript` and add as the
    // catalog's `/OpenAction`. Some viewers expect /JS to be a stream
    // for long scripts — short ones can be a literal string. We use
    // a string here since the freshness check stays well under 64 KB.
    let action_id = inc.new_document.add_object(dictionary! {
        "Type" => "Action",
        "S" => "JavaScript",
        "JS" => Object::string_literal(js.as_str()),
    });
    inc.new_document
        .get_object_mut(catalog_id)?
        .as_dict_mut()?
        .set("OpenAction", Object::Reference(action_id));
    Ok(())
}

/// Escape characters that would break a PDF JavaScript literal. The
/// JS sits inside a PDF string, then inside JS double-quoted strings,
/// so we minimally escape `\` and `"`. Newlines in the substituted
/// values are also forbidden.
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => {} // strip — would break the JS literal
            other => out.push(other),
        }
    }
    out
}

/// `D:YYYYMMDDHHmmSSZ`, which is what the PDF spec calls a date. An
/// ISO 8601 string in `/CreationDate` parses as nothing.
fn pdf_date(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ` for the custom `Stamp.*` keys, which are
/// ours and are meant to be read by a person grepping a leaked file.
fn iso_date(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Stream;
    use time::macros::datetime;

    const AT: OffsetDateTime = datetime!(2026-11-05 21:48:03 UTC);

    fn recipient() -> Recipient {
        Recipient {
            name: "Alice Cooper".into(),
            identifier: "uuid-abc-123".into(),
            email: Some("alice@example.com".into()),
        }
    }

    /// A document with more surface than an empty page: several pages,
    /// real content streams, a resource object shared between them
    /// holding a font of the document's own, and `/MediaBox` inherited
    /// from `/Pages` rather than stated per page.
    fn multi_page_pdf(pages: usize, rotate: Option<i64>) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Times-Roman",
        });
        let shared_res = doc.add_object(dictionary! {
            "Font" => dictionary! { "f0" => font_id },
        });
        let kids: Vec<Object> = (0..pages)
            .map(|i| {
                let content = doc.add_object(
                    Stream::new(
                        dictionary! {},
                        format!("BT /f0 12 Tf 72 700 Td (page {i}) Tj ET\n").into_bytes(),
                    )
                    .with_compression(false),
                );
                let mut page = dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "Resources" => shared_res,
                    "Contents" => content,
                };
                if let Some(r) = rotate {
                    page.set("Rotate", r);
                }
                Object::Reference(doc.add_object(page))
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => pages as i64,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn footer_options() -> Options {
        Options {
            visible: Some(VisibleWatermark::footer()),
            invisible: InvisibleMetadata::default(),
            freshness: None,
            stamp_creation_date: false,
        }
    }

    /// The last stream referenced by a page's `/Contents`.
    fn stamp_text(doc: &Document, page_id: lopdf::ObjectId) -> String {
        let c = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Contents")
            .unwrap();
        let id = c
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_reference()
            .unwrap();
        let s = doc.get_object(id).unwrap().as_stream().unwrap();
        String::from_utf8_lossy(&s.content).to_string()
    }

    #[test]
    fn the_original_bytes_survive_untouched_at_the_front() {
        let input = multi_page_pdf(3, None);
        let out = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        assert!(
            out.starts_with(&input),
            "an incremental update must leave the original file intact"
        );
        assert!(out.len() > input.len());
    }

    #[test]
    fn every_page_gets_the_stamp() {
        let input = multi_page_pdf(5, None);
        let out = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        assert_eq!(pages.len(), 5);
        for (_, page_id) in pages {
            let c = doc
                .get_dictionary(page_id)
                .unwrap()
                .get(b"Contents")
                .unwrap();
            // guard + the page's own stream + the stamp
            assert_eq!(c.as_array().unwrap().len(), 3);
            let text = stamp_text(&doc, page_id);
            assert!(text.contains("alice@example.com"), "{text}");
            assert!(text.contains("2026-11-05 21:48:03 UTC"), "{text}");
        }
    }

    /// A page's resources, whether they sit inline or in an object of
    /// their own. `get_page_resources` reports those two separately.
    fn resources(doc: &Document, page_id: lopdf::ObjectId) -> &Dictionary {
        let (inline, ids) = doc.get_page_resources(page_id).unwrap();
        inline.unwrap_or_else(|| doc.get_dictionary(ids[0]).unwrap())
    }

    #[test]
    fn the_stamp_font_reaches_the_resources_without_evicting_the_page_font() {
        let input = multi_page_pdf(2, None);
        let out = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        for (_, page_id) in doc.get_pages() {
            let fonts = resources(&doc, page_id)
                .get(b"Font")
                .unwrap()
                .as_dict()
                .unwrap();
            assert!(fonts.has(b"PdfStampFont"), "stamp font missing");
            assert!(fonts.has(b"f0"), "the page's own font was evicted");
        }
    }

    /// A PDF name, as a string, so assertions read as names do.
    fn name_of(d: &Dictionary, key: &[u8]) -> String {
        String::from_utf8(d.get(key).unwrap().as_name().unwrap().to_vec()).unwrap()
    }

    /// The font dictionary the stamp draws in, as a page resolves it.
    fn stamp_font(doc: &Document, page_id: lopdf::ObjectId) -> Dictionary {
        let fonts = resources(doc, page_id)
            .get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap();
        let id = fonts.get(b"PdfStampFont").unwrap().as_reference().unwrap();
        doc.get_dictionary(id).unwrap().clone()
    }

    /// Stamp a two-page document with `name` as the whole template.
    fn stamped_name(name: &str, font: FontSource) -> Vec<u8> {
        let input = multi_page_pdf(2, None);
        let recipient = Recipient {
            name: name.into(),
            ..recipient()
        };
        let options = Options {
            visible: Some(VisibleWatermark {
                template: "{name}".into(),
                font,
                ..VisibleWatermark::footer()
            }),
            ..footer_options()
        };
        personalise_at(&input, &recipient, &options, AT).unwrap()
    }

    #[test]
    fn an_ascii_stamp_embeds_no_font_program_at_all() {
        let out = stamped_name("Alice Cooper", FontSource::Bundled);
        let doc = Document::load_mem(&out).unwrap();
        let font = stamp_font(&doc, doc.get_pages()[&1]);
        assert_eq!(name_of(&font, b"Subtype"), "Type1");
        assert_eq!(name_of(&font, b"BaseFont"), "Helvetica");
        // The bundled font never got anywhere near the output.
        assert!(
            !out.windows(9).any(|w| w == b"FontFile2".as_slice()),
            "a font program was embedded for text that did not need one"
        );
    }

    #[test]
    fn a_name_outside_win_ansi_is_set_in_an_embedded_font() {
        let out = stamped_name("Łukasz Żółć", FontSource::Bundled);
        let doc = Document::load_mem(&out).unwrap();
        let page = doc.get_pages()[&1];
        let font = stamp_font(&doc, page);

        // A composite font, so the glyphs are addressed directly and
        // no single-byte encoding stands in the way.
        assert_eq!(name_of(&font, b"Subtype"), "Type0");
        assert_eq!(name_of(&font, b"Encoding"), "Identity-H");
        assert!(font.has(b"ToUnicode"), "the stamp would not be greppable");

        let kid = font.get(b"DescendantFonts").unwrap().as_array().unwrap()[0]
            .as_reference()
            .unwrap();
        let kid = doc.get_dictionary(kid).unwrap();
        assert_eq!(name_of(kid, b"Subtype"), "CIDFontType2");
        assert_eq!(name_of(kid, b"CIDToGIDMap"), "Identity");

        let desc = kid.get(b"FontDescriptor").unwrap().as_reference().unwrap();
        let desc = doc.get_dictionary(desc).unwrap();
        assert!(desc.has(b"FontFile2"), "the program itself is missing");
        assert!(
            name_of(desc, b"FontName").ends_with("+DejaVuSans"),
            "{}",
            name_of(desc, b"FontName")
        );

        // The line went out as glyph ids, not as bytes a reader would
        // have to guess an encoding for.
        let drawn = stamp_text(&doc, page);
        assert!(drawn.contains('<') && drawn.contains("> Tj"), "{drawn}");
    }

    #[test]
    fn declining_to_embed_falls_back_to_question_marks() {
        let out = stamped_name("Łukasz", FontSource::None);
        let doc = Document::load_mem(&out).unwrap();
        let page = doc.get_pages()[&1];
        assert_eq!(name_of(&stamp_font(&doc, page), b"Subtype"), "Type1");
        assert!(stamp_text(&doc, page).contains("?ukasz"));
    }

    #[test]
    fn a_font_the_text_does_not_need_is_never_read() {
        // Nonsense where a font should be. An ASCII stamp must not
        // care, because it never reaches for one — which is also what
        // keeps the common download off this code path entirely.
        let out = stamped_name(
            "Alice",
            FontSource::Supplied(Arc::new(b"certainly not a font".to_vec())),
        );
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn a_broken_font_is_an_error_once_the_text_actually_needs_it() {
        let input = multi_page_pdf(1, None);
        let recipient = Recipient {
            name: "Łukasz".into(),
            ..recipient()
        };
        let options = Options {
            visible: Some(VisibleWatermark {
                template: "{name}".into(),
                font: FontSource::Supplied(Arc::new(b"certainly not a font".to_vec())),
                ..VisibleWatermark::footer()
            }),
            ..footer_options()
        };
        assert!(matches!(
            personalise_at(&input, &recipient, &options, AT),
            Err(Error::Font(_))
        ));
    }

    #[test]
    fn a_translucent_stamp_registers_its_graphics_state_on_every_page() {
        let input = multi_page_pdf(2, None);
        let options = Options {
            visible: Some(VisibleWatermark::default()), // diagonal, opacity 0.18
            ..footer_options()
        };
        let out = personalise_at(&input, &recipient(), &options, AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        for (_, page_id) in doc.get_pages() {
            let states = resources(&doc, page_id)
                .get(b"ExtGState")
                .unwrap()
                .as_dict()
                .unwrap();
            assert!(states.has(b"PdfStampGS"), "transparency state missing");
        }
    }

    #[test]
    fn an_opaque_stamp_adds_no_graphics_state_at_all() {
        let input = multi_page_pdf(1, None);
        let out = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages().into_values().next().unwrap();
        assert!(resources(&doc, page_id).get(b"ExtGState").is_err());
    }

    #[test]
    fn pages_sharing_a_geometry_share_one_stamp_stream() {
        let input = multi_page_pdf(6, None);
        let out = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let ids: std::collections::HashSet<_> = doc
            .get_pages()
            .into_values()
            .map(|p| {
                let c = doc.get_dictionary(p).unwrap().get(b"Contents").unwrap();
                c.as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .as_reference()
                    .unwrap()
            })
            .collect();
        assert_eq!(ids.len(), 1, "six identical pages should share one stream");
    }

    #[test]
    fn an_inherited_media_box_is_found_rather_than_defaulted() {
        // Every page in the fixture inherits /MediaBox from /Pages. A
        // lookup that stopped at the page dictionary would place the
        // stamp against a guessed size, or refuse outright.
        let input = multi_page_pdf(1, None);
        assert!(personalise_at(&input, &recipient(), &footer_options(), AT).is_ok());
    }

    #[test]
    fn a_rotated_page_is_stamped_in_its_rotated_frame() {
        let input = multi_page_pdf(1, Some(90));
        let out = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages().into_values().next().unwrap();
        let text = stamp_text(&doc, page_id);
        assert!(text.contains("0 1 -1 0 595 0 cm"), "{text}");
    }

    #[test]
    fn a_template_that_renders_to_nothing_is_an_error_not_a_blank_stamp() {
        let input = multi_page_pdf(1, None);
        let options = Options {
            visible: Some(VisibleWatermark {
                template: "   ".into(),
                ..VisibleWatermark::footer()
            }),
            ..footer_options()
        };
        assert!(matches!(
            personalise_at(&input, &recipient(), &options, AT),
            Err(Error::EmptyStamp)
        ));
    }

    #[test]
    fn asking_for_no_watermark_leaves_the_pages_alone() {
        let input = multi_page_pdf(2, None);
        let options = Options {
            visible: None,
            ..footer_options()
        };
        let out = personalise_at(&input, &recipient(), &options, AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        for (_, page_id) in doc.get_pages() {
            let c = doc
                .get_dictionary(page_id)
                .unwrap()
                .get(b"Contents")
                .unwrap();
            assert!(c.as_reference().is_ok(), "contents should be untouched");
        }
    }

    #[test]
    fn freshness_check_lands_in_open_action() {
        let input = multi_page_pdf(1, None);
        let options = Options {
            visible: None,
            invisible: InvisibleMetadata::default(),
            freshness: Some(FreshnessCheck::new(
                "https://adeptus.example.com/api/freshness?id={doc_id}&v={doc_version}",
                "doc-uuid",
                "v42",
            )),
            stamp_creation_date: false,
        };
        let out = personalise_at(&input, &recipient(), &options, AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let action = doc
            .get_dictionary(root)
            .unwrap()
            .get(b"OpenAction")
            .unwrap()
            .as_reference()
            .unwrap();
        let js = doc.get_dictionary(action).unwrap().get(b"JS").unwrap();
        let js = String::from_utf8_lossy(js.as_str().unwrap()).to_string();
        assert!(js.contains("id=doc-uuid"), "{js}");
        assert!(js.contains("v=v42"), "{js}");
    }

    #[test]
    fn personalise_round_trips_metadata() {
        let input = multi_page_pdf(1, None);
        let options = Options {
            visible: None,
            invisible: InvisibleMetadata {
                recipient_id: true,
                recipient_email: true,
                stamped_at: true,
            },
            freshness: None,
            stamp_creation_date: false,
        };
        let out = personalise_at(&input, &recipient(), &options, AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let info_id = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        let info = doc.get_dictionary(info_id).unwrap();
        assert!(
            info.get(b"Stamp.RecipientID")
                .unwrap()
                .as_str()
                .unwrap()
                .ends_with(b"uuid-abc-123")
        );
        assert!(
            info.get(b"Stamp.RecipientEmail")
                .unwrap()
                .as_str()
                .unwrap()
                .ends_with(b"alice@example.com")
        );
        assert_eq!(
            info.get(b"Stamp.StampedAt").unwrap().as_str().unwrap(),
            b"2026-11-05T21:48:03Z"
        );
    }

    #[test]
    fn stamping_the_creation_date_writes_a_pdf_date_not_an_iso_one() {
        let input = multi_page_pdf(1, None);
        let options = Options {
            visible: None,
            stamp_creation_date: true,
            ..footer_options()
        };
        let out = personalise_at(&input, &recipient(), &options, AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let info_id = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        assert_eq!(
            doc.get_dictionary(info_id)
                .unwrap()
                .get(b"CreationDate")
                .unwrap()
                .as_str()
                .unwrap(),
            b"D:20261105214803Z"
        );
    }

    #[test]
    fn the_visible_line_and_the_metadata_agree_on_the_instant() {
        let input = multi_page_pdf(1, None);
        let options = Options {
            visible: Some(VisibleWatermark::footer()),
            invisible: InvisibleMetadata {
                stamped_at: true,
                ..InvisibleMetadata::default()
            },
            ..footer_options()
        };
        let out = personalise_at(&input, &recipient(), &options, AT).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let info_id = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        assert_eq!(
            doc.get_dictionary(info_id)
                .unwrap()
                .get(b"Stamp.StampedAt")
                .unwrap()
                .as_str()
                .unwrap(),
            b"2026-11-05T21:48:03Z"
        );
        let page_id = doc.get_pages().into_values().next().unwrap();
        assert!(stamp_text(&doc, page_id).contains("2026-11-05 21:48:03 UTC"));
    }

    #[test]
    fn stamping_twice_leaves_both_marks_and_a_readable_file() {
        // The delivery layer should not do this, but an incremental
        // update on top of an incremental update is exactly the case
        // that breaks a naive `/Prev` chain.
        let input = multi_page_pdf(2, None);
        let once = personalise_at(&input, &recipient(), &footer_options(), AT).unwrap();
        let twice = personalise_at(&once, &recipient(), &footer_options(), AT).unwrap();
        assert!(twice.starts_with(&once));
        let doc = Document::load_mem(&twice).unwrap();
        assert_eq!(doc.get_pages().len(), 2);
        for (_, page_id) in doc.get_pages() {
            let c = doc
                .get_dictionary(page_id)
                .unwrap()
                .get(b"Contents")
                .unwrap();
            assert_eq!(c.as_array().unwrap().len(), 5);
        }
    }

    #[test]
    fn an_encrypted_document_is_refused_rather_than_corrupted() {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let enc_id = doc.add_object(dictionary! { "Filter" => "Standard", "V" => 1, "R" => 2 });
        doc.trailer.set("Encrypt", Object::Reference(enc_id));
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        assert!(matches!(
            personalise_at(&bytes, &recipient(), &footer_options(), AT),
            Err(Error::Encrypted)
        ));
    }
}

#[cfg(test)]
mod render_template_tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_html_footer_and_the_pdf_stamp_render_identically() {
        // One renderer, so a printed page and a downloaded PDF cannot
        // come to disagree about what a timestamp looks like.
        let who = Recipient {
            name: String::new(),
            identifier: "reader@example.com".into(),
            email: Some("reader@example.com".into()),
        };
        let at = datetime!(2026-11-05 21:48:03 UTC);
        assert_eq!(
            render_template_at("Printed by {email} · {datetime}", &who, at),
            "Printed by reader@example.com · 2026-11-05 21:48:03 UTC"
        );
        // and it is UTC whatever the caller's clock says.
        assert_eq!(
            render_template_at("{time}", &who, datetime!(2026-11-05 23:48:03 +02:00)),
            "21:48:03"
        );
    }
}
