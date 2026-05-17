//! `pdf-stamp` — command-line driver for [`pdf_stamp::personalise`].
//!
//! Adeptus's delivery layer invokes this binary per recipient. Reads
//! the cached canonical PDF from `--input`, applies the configured
//! stamp, writes the personalised copy to `--output` (or stdout when
//! `--output -`).

use std::io::Write;

use clap::Parser;
use pdf_stamp::{
    FontSource, FreshnessCheck, InvisibleMetadata, Options, Placement, Recipient, VisibleWatermark,
    personalise,
};

#[derive(Copy, Clone, Debug, PartialEq, clap::ValueEnum)]
enum PlacementArg {
    /// One discreet line along the bottom of every page.
    Footer,
    /// Translucent text across the page diagonal.
    Diagonal,
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Cached canonical PDF to personalise.
    #[arg(short, long)]
    input: std::path::PathBuf,

    /// Output path, or `-` to stream to stdout.
    #[arg(short, long)]
    output: String,

    /// Recipient display name, for the `{name}` placeholder. Optional:
    /// the default template names the address, not the person.
    #[arg(long, default_value = "")]
    name: String,

    /// Stable recipient identifier (UUID, account ID, etc.) — written
    /// as invisible metadata for leak tracing.
    #[arg(long)]
    identifier: String,

    /// Recipient email. Required by the default template.
    #[arg(long)]
    email: Option<String>,

    /// Where the watermark is drawn. A footer carries the same trace
    /// as a banner without covering the document, which is what you
    /// want for anything people have to read and work from.
    #[arg(long, value_enum, default_value_t = PlacementArg::Footer)]
    placement: PlacementArg,

    /// Override the watermark text. Placeholders: `{name}`, `{email}`,
    /// `{identifier}`, `{date}`, `{time}`, `{timezone}`, `{datetime}`.
    /// Times are always UTC.
    #[arg(long)]
    template: Option<String>,

    /// Override the watermark's size in points.
    #[arg(long)]
    font_size: Option<f32>,

    /// A TrueType or OpenType font to set the stamp in when its text
    /// needs characters the standard font cannot encode. Only needed
    /// for scripts the bundled font does not cover — in practice, CJK.
    /// Only the glyphs the stamp uses are embedded, so a 20 MB face
    /// still costs a few kilobytes per document.
    #[arg(long, conflicts_with = "no_embed_font")]
    font: Option<std::path::PathBuf>,

    /// Never embed a font. Characters the standard font cannot encode
    /// become `?`, and the output stays as small as possible.
    #[arg(long)]
    no_embed_font: bool,

    /// Skip the visible watermark, leaving only the invisible metadata.
    #[arg(long)]
    no_visible: bool,

    /// URL template for the freshness check. Supports `{doc_id}` and
    /// `{doc_version}` substitutions. When omitted, no check is
    /// injected. Requires `--doc-id` and `--doc-version` together.
    #[arg(long)]
    freshness_url: Option<String>,
    #[arg(long, requires = "freshness_url")]
    doc_id: Option<String>,
    #[arg(long, requires = "freshness_url")]
    doc_version: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let recipient = Recipient {
        name: args.name,
        identifier: args.identifier,
        email: args.email,
    };
    let freshness = match (args.freshness_url, args.doc_id, args.doc_version) {
        (Some(url), Some(id), Some(version)) => Some(FreshnessCheck::new(url, id, version)),
        // clap's `requires` already rejects partial combos, so anything
        // else here means the caller didn't ask for a check.
        _ => None,
    };

    // Read before the watermark is built so an unreadable font is a
    // clear error here rather than a silent fallback to question marks.
    let font = match &args.font {
        Some(path) => Some(std::fs::read(path)?),
        None => None,
    };

    let visible = (!args.no_visible).then(|| {
        let mut wm = match args.placement {
            PlacementArg::Footer => VisibleWatermark::footer(),
            PlacementArg::Diagonal => VisibleWatermark {
                placement: Placement::Diagonal,
                ..VisibleWatermark::default()
            },
        };
        if let Some(t) = args.template {
            wm.template = t;
        }
        if let Some(s) = args.font_size {
            wm.font_size = s;
        }
        wm.font = match font {
            Some(bytes) => FontSource::Supplied(std::sync::Arc::new(bytes)),
            None if args.no_embed_font => FontSource::None,
            None => FontSource::Bundled,
        };
        wm
    });

    let options = Options {
        visible,
        invisible: InvisibleMetadata {
            recipient_id: true,
            recipient_email: recipient.email.is_some(),
            stamped_at: true,
        },
        freshness,
        stamp_creation_date: false,
    };

    let bytes = std::fs::read(&args.input)?;
    let out = personalise(&bytes, &recipient, &options)?;

    if args.output == "-" {
        std::io::stdout().write_all(&out)?;
    } else {
        std::fs::write(&args.output, &out)?;
        eprintln!(
            "Stamped {} bytes for {} into {}",
            out.len(),
            recipient.identifier,
            args.output
        );
    }
    Ok(())
}
