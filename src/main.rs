//! `pdf-stamp` — command-line driver for [`pdf_stamp::personalise`].
//!
//! Adeptus's delivery layer invokes this binary per recipient. Reads
//! the cached canonical PDF from `--input`, applies the configured
//! stamp, writes the personalised copy to `--output` (or stdout when
//! `--output -`).

use std::io::Write;

use clap::Parser;
use pdf_stamp::{
    FreshnessCheck, InvisibleMetadata, Options, Recipient, VisibleWatermark, personalise,
};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Cached canonical PDF to personalise.
    #[arg(short, long)]
    input: std::path::PathBuf,

    /// Output path, or `-` to stream to stdout.
    #[arg(short, long)]
    output: String,

    /// Recipient display name (used in the visible watermark).
    #[arg(long)]
    name: String,

    /// Stable recipient identifier (UUID, account ID, etc.) — written
    /// as invisible metadata for leak tracing.
    #[arg(long)]
    identifier: String,

    /// Optional recipient email.
    #[arg(long)]
    email: Option<String>,

    /// Skip the visible diagonal-text watermark.
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
    let options = Options {
        visible: if args.no_visible {
            None
        } else {
            Some(VisibleWatermark::default())
        },
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
