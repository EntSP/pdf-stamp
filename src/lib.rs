//! Per-recipient personalisation of cached PDF documents.
//!
//! markdoc-pdf renders the canonical PDF once and caches it. When a
//! specific person requests a copy, this crate walks the cached
//! bytes and stamps two things on top:
//!
//! - A **visible watermark** (e.g. the recipient's name across the
//!   page diagonal) so a leaked copy is obviously personalised.
//! - **Invisible metadata** (custom `/Info` and XMP fields) carrying
//!   the recipient identifier and a render timestamp so leaks can be
//!   traced back to the recipient even if the visible mark is
//!   cropped away.
//!
//! The public entry point is [`personalise`]: it takes the bytes of
//! a PDF plus a [`Recipient`], applies the configured stamp, and
//! returns the modified bytes. No re-rendering — we mutate the
//! existing PDF via `lopdf` so the operation is fast even for very
//! large documents.

use lopdf::{Document, Object, dictionary};
use thiserror::Error;

/// Identity stamped onto a personalised copy. Drives both the visible
/// watermark text (typically `name` or `name + email`) and the
/// invisible metadata fields used to trace leaks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Recipient {
    /// Human-readable name shown in the visible watermark.
    pub name: String,
    /// Stable, unique identifier (UUID, account ID, etc.). Stored in
    /// the invisible `/Info` and XMP fields and is what investigators
    /// match leaked copies against.
    pub identifier: String,
    /// Optional email — appended to the visible watermark when set.
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

/// Diagonal-text overlay drawn on every page. Mirrors the shape of
/// markdoc-pdf's render-time watermark but applied post-render.
#[derive(Debug, Clone)]
pub struct VisibleWatermark {
    /// Text printed across the page. Templates use `{name}`,
    /// `{email}`, `{identifier}`, `{date}`.
    pub template: String,
    pub font_size: f32,
    pub opacity: f32,
    pub rotation_deg: f32,
    /// `(r, g, b)` 0..=255.
    pub color: (u8, u8, u8),
}

impl Default for VisibleWatermark {
    fn default() -> Self {
        Self {
            template: "Confidential — {name}".into(),
            font_size: 36.0,
            opacity: 0.18,
            rotation_deg: -30.0,
            color: (180, 180, 180),
        }
    }
}

/// Invisible metadata written to the PDF's `/Info` dictionary (and
/// in a future iteration, XMP). Each field that is `Some(_)` ends up
/// as a separate `/Info` entry under a custom key prefix so the
/// fields are easy to grep for in a leaked copy.
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
}

/// Apply one personalisation pass to `input` and return the modified
/// PDF bytes. Pure function — does not touch the filesystem.
///
/// The pass:
/// 1. Parses `input` with `lopdf` (no re-rendering).
/// 2. Optionally appends a diagonal-text overlay to every page's
///    content stream (visible watermark).
/// 3. Optionally writes recipient/timestamp fields into the PDF
///    `/Info` dictionary (invisible metadata).
/// 4. Serialises the document back to a `Vec<u8>`.
pub fn personalise(
    input: &[u8],
    recipient: &Recipient,
    options: &Options,
) -> Result<Vec<u8>, Error> {
    let mut doc = Document::load_mem(input)?;
    if let Some(wm) = &options.visible {
        apply_visible_watermark(&mut doc, recipient, wm)?;
    }
    apply_invisible_metadata(
        &mut doc,
        recipient,
        &options.invisible,
        options.stamp_creation_date,
    )?;
    if let Some(check) = &options.freshness {
        apply_freshness_check(&mut doc, check)?;
    }
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────
// Watermark and metadata stampers — minimal v0 implementations.
// Both leave room for proper XMP packets, font subsetting and stamped
// QR codes once the Adeptus delivery layer is firmed up.
// ─────────────────────────────────────────────────────────────────────

fn apply_visible_watermark(
    _doc: &mut Document,
    _recipient: &Recipient,
    _wm: &VisibleWatermark,
) -> Result<(), Error> {
    // TODO: append a content-stream overlay to every page that draws
    // `wm.template` rotated by `wm.rotation_deg`, in `wm.color` at
    // `wm.opacity`. Approach:
    //   - Iterate doc.page_iter() collecting page IDs.
    //   - For each page, construct a content stream that:
    //       q                              % save graphics state
    //       /GS_stamp gs                   % opacity ext-gstate (added once at /Resources)
    //       <r> <g> <b> rg                 % set fill colour
    //       <cos a> <sin a> <-sin a> <cos a> <cx> <cy> cm  % rotate around centre
    //       BT /F_helv <size> Tf <-w/2> 0 Td (text) Tj ET
    //       Q                              % restore
    //   - Append as an additional content stream object (PDF 1.x
    //     allows page Contents to be an array of streams).
    //   - Add Helvetica (Type1 standard) font + ext-gstate to
    //     /Resources if not already present.
    Ok(())
}

const META_PREFIX: &str = "Stamp.";

fn apply_invisible_metadata(
    doc: &mut Document,
    recipient: &Recipient,
    flags: &InvisibleMetadata,
    stamp_creation_date: bool,
) -> Result<(), Error> {
    // Ensure an Info dict exists.
    let info_id = match doc.trailer.get(b"Info") {
        Ok(Object::Reference(id)) => *id,
        _ => {
            let id = doc.add_object(dictionary! {});
            doc.trailer.set("Info", Object::Reference(id));
            id
        }
    };

    let info = doc
        .objects
        .get_mut(&info_id)
        .and_then(|o| match o {
            Object::Dictionary(d) => Some(d),
            _ => None,
        })
        .ok_or(Error::Structure(
            "/Info reference does not point at a dictionary",
        ))?;

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
        let now = current_pdf_date();
        info.set(
            format!("{META_PREFIX}StampedAt").as_bytes().to_vec(),
            Object::string_literal(now.as_str()),
        );
    }
    if stamp_creation_date {
        let now = current_pdf_date();
        info.set("CreationDate", Object::string_literal(now.as_str()));
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

fn apply_freshness_check(doc: &mut Document, check: &FreshnessCheck) -> Result<(), Error> {
    let url = check
        .url_template
        .replace("{doc_id}", &js_escape(&check.doc_id))
        .replace("{doc_version}", &js_escape(&check.doc_version));
    let js = FRESHNESS_JS
        .replace("__URL__", &js_escape(&url))
        .replace("__MSG__", &js_escape(&check.outdated_message));

    // Build a `/Action` of subtype `/JavaScript` and add as the
    // catalog's `/OpenAction`. Some viewers expect /JS to be a stream
    // for long scripts — short ones can be a literal string. We use
    // a string here since the freshness check stays well under 64 KB.
    let action_id = doc.add_object(dictionary! {
        "Type" => "Action",
        "S" => "JavaScript",
        "JS" => Object::string_literal(js.as_str()),
    });

    let catalog_id = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|o| match o {
            Object::Reference(id) => Some(*id),
            _ => None,
        })
        .ok_or(Error::Structure("trailer /Root missing"))?;
    let catalog = doc
        .objects
        .get_mut(&catalog_id)
        .and_then(|o| match o {
            Object::Dictionary(d) => Some(d),
            _ => None,
        })
        .ok_or(Error::Structure("/Root is not a dictionary"))?;
    catalog.set("OpenAction", Object::Reference(action_id));
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

fn current_pdf_date() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Iso8601;
    OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single empty page produced by lopdf. Enough surface to exercise
    /// the metadata path without bringing in markdoc-pdf as a build dep.
    fn minimal_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn freshness_check_lands_in_open_action() {
        let input = minimal_pdf();
        let recipient = Recipient {
            name: "Bob".into(),
            identifier: "bob-123".into(),
            email: None,
        };
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
        let out = personalise(&input, &recipient, &options).unwrap();

        // The JS body lives inline; bytewise look for the URL we
        // just templated in.
        assert!(
            out.windows(b"adeptus.example.com".len())
                .any(|w| w == b"adeptus.example.com")
        );
        assert!(
            out.windows(b"id=doc-uuid".len())
                .any(|w| w == b"id=doc-uuid")
        );
        assert!(out.windows(b"OpenAction".len()).any(|w| w == b"OpenAction"));
        assert!(
            out.windows(b"/JavaScript".len())
                .any(|w| w == b"/JavaScript")
        );
    }

    #[test]
    fn personalise_round_trips_metadata() {
        let input = minimal_pdf();
        let recipient = Recipient {
            name: "Alice".into(),
            identifier: "uuid-abc-123".into(),
            email: Some("alice@example.com".into()),
        };
        let options = Options {
            visible: None, // visible-stamp pass is still a TODO
            invisible: InvisibleMetadata {
                recipient_id: true,
                recipient_email: true,
                stamped_at: true,
            },
            freshness: None,
            stamp_creation_date: false,
        };
        let out = personalise(&input, &recipient, &options).unwrap();
        // Round-trip: load the output and check our keys are there.
        let doc = Document::load_mem(&out).unwrap();
        let info_id = match doc.trailer.get(b"Info").unwrap() {
            Object::Reference(id) => *id,
            _ => panic!("Info should be an indirect reference"),
        };
        let info = doc.objects.get(&info_id).unwrap().as_dict().unwrap();
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
        assert!(info.get(b"Stamp.StampedAt").is_ok());
    }
}
