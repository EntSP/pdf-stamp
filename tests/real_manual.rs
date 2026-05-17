//! Stamping a real document, rather than one this crate built itself.
//!
//! The unit tests use fixtures assembled with `lopdf`, which means they
//! only ever exercise the shapes `lopdf` happens to emit. A manual out
//! of markdoc-pdf has compressed content streams, an outline, link
//! annotations, embedded subset fonts and images — and it is the thing
//! that will actually be stamped in production.
//!
//! The file is not vendored into the repository: a 1.8 MB binary in git
//! to run one test is a poor trade. Point the test at one instead:
//!
//! ```text
//!     PDF_STAMP_REAL_PDF="/path/to/manual.pdf" cargo test --test real_manual
//! ```
//!
//! With the variable unset the test reports that it was skipped and
//! passes, so `cargo test` stays green on a machine with no manual to
//! hand.

use lopdf::{Document, Object, ObjectId};
use pdf_stamp::{
    InvisibleMetadata, Options, Placement, Recipient, VisibleWatermark, personalise_at,
};
use time::macros::datetime;

const EMAIL: &str = "alice@example.com";
const WHEN: &str = "2026-11-05 21:48:03 UTC";

fn recipient() -> Recipient {
    Recipient {
        name: String::new(),
        identifier: "acct-9f3c".into(),
        email: Some(EMAIL.into()),
    }
}

fn options() -> Options {
    Options {
        visible: Some(VisibleWatermark::footer()),
        invisible: InvisibleMetadata {
            recipient_id: true,
            recipient_email: true,
            stamped_at: true,
        },
        freshness: None,
        stamp_creation_date: false,
    }
}

/// The stream a page's `/Contents` ends with, decoded.
fn stamp_text(doc: &Document, page_id: ObjectId) -> String {
    let contents = doc
        .get_dictionary(page_id)
        .expect("page is a dictionary")
        .get(b"Contents")
        .expect("page has contents");
    let last = contents
        .as_array()
        .expect("contents became an array")
        .last()
        .expect("the array is not empty")
        .as_reference()
        .expect("the last entry is a reference");
    let stream = doc.get_object(last).unwrap().as_stream().unwrap();
    // The stamp is written uncompressed, so this needs no filter pass.
    String::from_utf8_lossy(&stream.content).to_string()
}

fn annots(doc: &Document) -> usize {
    doc.get_pages()
        .into_values()
        .filter_map(|p| doc.get_dictionary(p).ok())
        .filter_map(|d| d.get(b"Annots").ok())
        .filter_map(|a| a.as_array().ok())
        .map(|a| a.len())
        .sum()
}

fn outline_present(doc: &Document) -> bool {
    let Ok(Object::Reference(root)) = doc.trailer.get(b"Root") else {
        return false;
    };
    doc.get_dictionary(*root)
        .map(|d| d.has(b"Outlines"))
        .unwrap_or(false)
}

fn real_manual() -> Option<Vec<u8>> {
    let path = std::env::var_os("PDF_STAMP_REAL_PDF")?;
    Some(std::fs::read(&path).expect("the manual named by PDF_STAMP_REAL_PDF is readable"))
}

#[test]
fn a_real_manual_survives_being_stamped() {
    let Some(input) = real_manual() else {
        eprintln!("PDF_STAMP_REAL_PDF is not set — skipping the real-manual test");
        return;
    };

    let before = Document::load_mem(&input).expect("the input parses");
    let page_count = before.get_pages().len();
    let annots_before = annots(&before);
    let outline_before = outline_present(&before);
    assert!(page_count > 1, "expected a multi-page manual");

    let out = personalise_at(
        &input,
        &recipient(),
        &options(),
        datetime!(2026-11-05 21:48:03 UTC),
    )
    .expect("stamping succeeds");

    // The canonical bytes are still there, unaltered, at the front.
    assert!(
        out.starts_with(&input),
        "the original document was rewritten instead of appended to"
    );
    let growth = (out.len() - input.len()) as f64 / input.len() as f64;
    assert!(
        growth < 0.05,
        "an incremental update should add a few percent, not {:.1}%",
        growth * 100.0
    );

    let after = Document::load_mem(&out).expect("the stamped file parses");
    let pages = after.get_pages();
    assert_eq!(pages.len(), page_count, "page count changed");
    assert_eq!(annots(&after), annots_before, "link annotations were lost");
    assert_eq!(
        outline_present(&after),
        outline_before,
        "the outline was lost"
    );

    for (number, page_id) in pages {
        let text = stamp_text(&after, page_id);
        assert!(
            text.contains(EMAIL),
            "page {number} does not carry the address: {text}"
        );
        assert!(
            text.contains(WHEN),
            "page {number} does not carry the timestamp: {text}"
        );

        let (inline, ids) = after.get_page_resources(page_id).unwrap();
        let res = inline.unwrap_or_else(|| after.get_dictionary(ids[0]).unwrap());
        let fonts = res
            .get(b"Font")
            .unwrap_or_else(|_| panic!("page {number} has no /Font"))
            .as_dict()
            .unwrap();
        assert!(
            fonts.has(b"PdfStampFont"),
            "page {number} cannot resolve the stamp font"
        );
    }

    // The metadata went in alongside the visible mark.
    let info_id = after.trailer.get(b"Info").unwrap().as_reference().unwrap();
    let info = after.get_dictionary(info_id).unwrap();
    assert!(
        info.get(b"Stamp.RecipientEmail")
            .unwrap()
            .as_str()
            .unwrap()
            .ends_with(EMAIL.as_bytes())
    );
    // and the document's own metadata was not trampled on the way.
    assert!(
        info.has(b"Producer"),
        "the original /Info entries were replaced rather than added to"
    );
}

#[test]
fn a_diagonal_stamp_also_works_on_a_real_manual() {
    let Some(input) = real_manual() else {
        eprintln!("PDF_STAMP_REAL_PDF is not set — skipping the real-manual test");
        return;
    };
    let options = Options {
        visible: Some(VisibleWatermark {
            template: "{email} · {date}".into(),
            placement: Placement::Diagonal,
            ..VisibleWatermark::default()
        }),
        ..options()
    };
    let out = personalise_at(
        &input,
        &recipient(),
        &options,
        datetime!(2026-11-05 21:48:03 UTC),
    )
    .expect("stamping succeeds");

    let after = Document::load_mem(&out).expect("the stamped file parses");
    for (number, page_id) in after.get_pages() {
        let text = stamp_text(&after, page_id);
        assert!(text.contains(EMAIL), "page {number}: {text}");
        // The translucency state is referenced, and registered.
        assert!(text.contains("/PdfStampGS gs"), "page {number}: {text}");
        let (inline, ids) = after.get_page_resources(page_id).unwrap();
        let res = inline.unwrap_or_else(|| after.get_dictionary(ids[0]).unwrap());
        assert!(
            res.get(b"ExtGState")
                .unwrap()
                .as_dict()
                .unwrap()
                .has(b"PdfStampGS"),
            "page {number} cannot resolve the stamp's graphics state"
        );
    }
}

/// A recipient whose name the standard font cannot set, on a document
/// that already carries six embedded subset fonts of its own.
#[test]
fn a_name_outside_win_ansi_is_embedded_into_a_real_manual() {
    let Some(input) = real_manual() else {
        eprintln!("PDF_STAMP_REAL_PDF is not set — skipping the real-manual test");
        return;
    };
    let before = Document::load_mem(&input).expect("the input parses");
    let page_count = before.get_pages().len();

    // The shared `recipient()` has no name, and a template that names
    // nobody is pure ASCII — which would quietly test the standard
    // font instead of the embedded one.
    let recipient = Recipient {
        name: "Łukasz Żółć · Ωμέγα · Привет".into(),
        ..recipient()
    };
    let options = Options {
        visible: Some(VisibleWatermark {
            template: "{name} · {email}".into(),
            ..VisibleWatermark::footer()
        }),
        ..options()
    };
    let out = personalise_at(
        &input,
        &recipient,
        &options,
        datetime!(2026-11-05 21:48:03 UTC),
    )
    .expect("stamping succeeds");

    // Embedding a font must not cost the incremental update.
    assert!(
        out.starts_with(&input),
        "the original document was rewritten instead of appended to"
    );
    let growth = (out.len() - input.len()) as f64 / input.len() as f64;
    assert!(growth < 0.05, "grew {:.1}%", growth * 100.0);

    let after = Document::load_mem(&out).expect("the stamped file parses");
    let pages = after.get_pages();
    assert_eq!(pages.len(), page_count, "page count changed");

    for (number, page_id) in pages {
        let (inline, ids) = after.get_page_resources(page_id).unwrap();
        let res = inline.unwrap_or_else(|| after.get_dictionary(ids[0]).unwrap());
        let fonts = res.get(b"Font").unwrap().as_dict().unwrap();
        let id = fonts
            .get(b"PdfStampFont")
            .unwrap_or_else(|_| panic!("page {number} cannot resolve the stamp font"))
            .as_reference()
            .unwrap();
        let font = after.get_dictionary(id).unwrap();
        assert_eq!(
            font.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Type0".as_slice(),
            "page {number} did not get the embedded font"
        );
        // The document's own fonts are still the document's own.
        assert!(fonts.len() > 1, "page {number} lost the fonts it came with");
    }
}
