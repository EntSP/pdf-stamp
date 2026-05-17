# pdf-stamp

Per-recipient personalisation of cached PDF documents. Sister crate
to **markdoc-pdf** in the Adeptus delivery pipeline:

```
markdoc-pdf  →  cached canonical PDF
                       │
                       ▼
                  pdf-stamp ←── recipient
                       │
                       ▼
              personalised copy → recipient
```

## What it does

1. **Visible watermark** — drawn into every page's content stream,
   either as a discreet footer line or as translucent text across the
   page diagonal. Templates support `{name}`, `{email}`,
   `{identifier}`, `{date}`, `{time}`, `{timezone}` and `{datetime}`.
   Times are always UTC.
2. **Invisible metadata** — recipient identifier, optional email and
   a stamp timestamp written into the PDF's `/Info` dictionary under
   `Stamp.*` keys, so leaks are traceable even if the visible mark is
   cropped away.
3. **Freshness check** — optional document-level JavaScript that fires
   when the PDF is opened. Sends a `GET` to a configurable URL with
   the document id and version; if Adeptus answers that a newer
   version exists, the user sees an `app.alert(...)` banner. Wrapped
   in `try/catch` so unreachable Adeptus, no-network and viewers that
   sandbox or disable PDF JS all silently no-op — never blocks
   reading the document.

## The output is the input, plus a few kilobytes

Nothing is re-rendered and nothing is re-serialised. PDF allows a file
to be extended with an *incremental update*: the original bytes, then
the objects that changed, then a cross-reference section pointing back
at the old one. The canonical bytes are still there, byte for byte, at
the front of every personalised copy.

That matters for more than speed. Anything this crate does not
understand — the outline, link annotations, embedded fonts, images, a
structure tree — is carried through intact because it is never parsed,
let alone rewritten. A full round-trip through any PDF library is a
chance to lose one of them.

Measured on a 62-page, 1.8 MB markdoc-pdf manual: **7 ms, +1.1 %**,
with the outline and all link annotations intact.

## What a watermark is and is not

It is attribution, not protection. Anyone who wants the mark gone can
remove it, and nothing here pretends otherwise. What it buys is that a
copy found where it should not be names the account that downloaded
it, and that passing one on stops feeling anonymous.

Note also that stamping an address onto a document means that address
travels with every copy the recipient forwards. That is the point of
the feature, and it is personal data moving outside your control: it
belongs in whatever policy governs the download.

### Two fonts, and when each is used

Whenever the stamp's text fits **WinAnsi** — which an address and a
timestamp always do — it is set in Helvetica, one of the fourteen
fonts every reader is required to have. Nothing is embedded, and the
font costs one small dictionary.

When it does not fit, a font is embedded instead. The text goes out as
a `/Type0` font with `Identity-H` encoding: two-byte glyph ids
addressing the font program directly, so no single-byte encoding
stands between the bytes and the glyphs, and any script the font
covers works. Only the glyphs the stamp actually uses are embedded,
and a `/ToUnicode` map goes in beside them — which is what keeps the
stamp selectable, searchable and greppable in the finished document.

| Stamp text | Font | Cost on a 1.8 MB manual |
|------------|------|------|
| `alice@example.com · 2026-11-05 21:48:03 UTC` | Helvetica, no program embedded | +1.10 % |
| `Łukasz Żółć · Ωμέγα · Привет · …` | bundled DejaVu subset | +1.77 % |
| `田中太郎 · …`, with `--font NotoSansCJK.ttc` | subset of a 19.5 MB face | +1.43 % |

**The bundled font** is DejaVu Sans 2.37 — `assets/DejaVuSans.ttf`,
the upstream release verbatim, SHA-256 `7da195a74c55bef9…ed848954`,
licence beside it. It covers Latin, including the Extended-A block
most of Europe needs, plus Greek, Cyrillic, Armenian, Georgian and
Hebrew.

**It has no CJK**, because a face that does is tens of megabytes and
does not belong inside a library. For those, supply one — `--font`, or
`FontSource::Supplied`. Only the glyphs used are embedded, so pointing
at a 19.5 MB Noto Sans CJK still costs about five kilobytes.

A supplied font has to cover the *whole* line, not just the part the
standard font could not set. A CJK **fallback** face has no Latin in
it at all, so using one would turn the address — the part that
actually identifies the recipient — into a row of question marks in
order to rescue four characters of the name. The crate checks, and
declines to embed a font that would lose more than it gains; the
stamp stays in Helvetica and only the name degrades.

## Why a separate crate

markdoc-pdf renders once; the cached output is the canonical artefact.
Adding "modify an existing PDF" to its responsibilities would pull in
a different PDF library (`lopdf` here) with different opinions and
duplicate the byte-shuffling code.  Splitting the concern keeps both
crates small and lets Adeptus's delivery layer stitch them together.

## Install

Each [release](https://github.com/EntSP/pdf-stamp/releases) carries a
binary per platform, `pdf-stamp-<platform>`: Linux (x86-64 and ARM64),
Windows (x86-64) and macOS (Apple Silicon and Intel). The Linux ones are
static, so they run on any distribution and in a `FROM scratch` image.
Each has a `.sha256` beside it and a GitHub build-provenance attestation,
which ties it to the commit and the workflow run that built it:

```
curl -LO https://github.com/EntSP/pdf-stamp/releases/latest/download/pdf-stamp-linux-x86_64
gh attestation verify pdf-stamp-linux-x86_64 --repo EntSP/pdf-stamp
```

Or build it from crates.io: `cargo install pdf-stamp`.

## CLI

```
pdf-stamp \
    --input cached.pdf \
    --output personalised.pdf \
    --identifier 4a18-…-ba93 \
    --email alice@example.com \
    --freshness-url "https://adeptus.example.com/api/freshness?id={doc_id}&v={doc_version}" \
    --doc-id   doc-uuid \
    --doc-version v42
```

Default stamp: a footer line reading
`Downloaded by alice@example.com · 2026-11-05 21:48:03 UTC`.

| Flag | Effect |
|------|--------|
| `--placement footer\|diagonal` | Footer by default. A footer carries the same trace as a banner without covering the document, which is what you want for anything people have to read and work from. |
| `--template "…"` | Override the text. Placeholders as above. |
| `--font-size N` | Override the size in points. |
| `--name "Alice Cooper"` | Only needed by templates that use `{name}`. |
| `--font PATH` | A TrueType/OpenType face for scripts the bundled font does not cover — in practice, CJK. Only the glyphs the stamp uses are embedded. |
| `--no-embed-font` | Never embed anything. Characters outside WinAnsi become `?` and the output stays as small as it can be. |
| `--no-visible` | Invisible metadata only. |
| `--output -` | Stream to stdout. |

Long lines are shrunk to fit rather than truncated — half an address
identifies nobody — so a diagonal stamp carrying an address and a
timestamp stays on the page instead of running off both edges.

### Freshness check viewer support

The check rides on PDF document-level JavaScript via `Net.HTTP.request`.
That API is **only available in Adobe Acrobat / Reader DC** (and a
few enterprise viewers that aim for parity). Behaviour elsewhere:

| Viewer                               | Outcome             |
|--------------------------------------|---------------------|
| Adobe Acrobat / Reader DC (desktop)  | Check fires; alert if outdated |
| Chrome / Firefox built-in pdfjs      | Silently skipped (PDF JS disabled) |
| macOS Preview                        | Silently skipped |
| Mobile readers (most)                | Silently skipped |
| Foxit, PDF-XChange, etc.             | Varies — typically prompts user before allowing JS |

So the badge is advisory — users on viewers without PDF JS will
simply not see the warning. The script is wrapped in `try/catch` so
network failures, sandbox restrictions, or unreachable Adeptus all
result in a silent no-op rather than a viewer error.

Adeptus should respond with JSON containing `"outdated": true` when
the user's copy is stale; anything else (including HTTP errors) is
treated as "up to date".

## Library

```rust
use pdf_stamp::{InvisibleMetadata, Options, Recipient, VisibleWatermark, personalise};

let bytes = std::fs::read("cached.pdf")?;
let out = personalise(
    &bytes,
    &Recipient {
        name: String::new(),
        identifier: "uuid".into(),
        email: Some("alice@example.com".into()),
    },
    &Options {
        visible: Some(VisibleWatermark::footer()),
        invisible: InvisibleMetadata {
            recipient_id: true,
            recipient_email: true,
            stamped_at: true,
        },
        ..Options::default()
    },
)?;
std::fs::write("alice.pdf", out)?;
```

`Options::default()` uses `VisibleWatermark::default()`, which is the
diagonal; `VisibleWatermark::footer()` is the quiet one. Use
`personalise_at` to supply the instant instead of reading the clock —
one instant is threaded through the visible line and the metadata, so
the two cannot disagree.

`VisibleWatermark::font` is a [`FontSource`]: `Bundled` by default,
`Supplied(Arc<Vec<u8>>)` for a face of your own — see
`VisibleWatermark::with_font` — or `None` to embed nothing. It is only
consulted when the text needs it, so a large supplied face costs
nothing on an ordinary ASCII download.

### Errors worth knowing about

- `Error::EmptyStamp` — the template rendered to nothing. Returned
  rather than stamping a blank line: a watermark that silently is not
  there is worse than a download that fails loudly.
- `Error::Encrypted` — the input is encrypted. Appending plaintext
  objects to it would produce a file whose new content the reader
  tries to decrypt.
- `Error::Font` — the font to embed could not be read or subset. Only
  ever raised on the path that needs one, so a bad `--font` fails
  loudly on the first recipient who needs it rather than silently
  stamping question marks.

## Tests

`cargo test` runs against fixtures this crate builds itself. Those
only exercise the shapes `lopdf` emits, so there is a second suite
that runs against a real document — compressed content streams, an
outline, link annotations, embedded subset fonts, images:

```
PDF_STAMP_REAL_PDF="/path/to/manual.pdf" cargo test --test real_manual
```

With the variable unset those tests report that they were skipped and
pass, so `cargo test` stays green without a manual to hand.

## Building and releasing

```
cargo build --release
bash .github/scripts/check.sh   # the gate CI runs: fmt, clippy, tests, a release build, a page stamped
```

`check.sh` ends by stamping a one-page PDF it writes itself with the
binary it built (`.github/scripts/smoke.sh`), and checks that the input's
bytes come through untouched at the front of the output and the recipient
lands in `/Info`. Every release asset is put through the same before it is
published.

Releasing is a version bump: push a new version in `Cargo.toml` to main,
and `.github/workflows/release.yml` publishes the crate to crates.io and
the binaries to a GitHub release, and tags the commit.

## Status

- [x] API surface and CLI scaffolding
- [x] Invisible `/Info` metadata stamping
- [x] Freshness check via `/OpenAction` JavaScript
- [x] Visible content-stream watermark emission, footer and diagonal
- [x] Incremental update — the canonical bytes are preserved verbatim
- [x] `/Rotate` and inherited `/MediaBox` / `/Resources` handled
- [ ] XMP metadata packet (in addition to `/Info`)
- [x] Embedded font subset for names outside WinAnsi — `/Type0`,
      `Identity-H`, `/ToUnicode`, bundled DejaVu or a supplied face
- [ ] Per-script font runs — one stamp line set in several fonts, so a
      Latin address and a CJK name can share it without either being
      declined. `fontique` does the selection; the subsetting and CID
      emission here already handle the rest
- [ ] Hidden in-document banner annotation toggled by the freshness
      script (instead of `app.alert`) for a less intrusive UX
- [ ] Page-level signature / hash chain so post-stamp tampering is
      detectable
