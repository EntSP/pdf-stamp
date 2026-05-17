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

1. **Visible watermark** — diagonal text overlay on every page (e.g.
   "Confidential — Alice Cooper"). Templates support `{name}`,
   `{email}`, `{identifier}`, `{date}`. (TODO: content-stream emission
   not yet implemented; v0 ships the API surface only.)
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

## Why a separate crate

markdoc-pdf renders once; the cached output is the canonical artefact.
Adding "modify an existing PDF" to its responsibilities would pull in
a different PDF library (`lopdf` here) with different opinions and
duplicate the byte-shuffling code.  Splitting the concern keeps both
crates small and lets Adeptus's delivery layer stitch them together.

## CLI

```
pdf-stamp \
    --input cached.pdf \
    --output personalised.pdf \
    --name "Alice Cooper" \
    --identifier 4a18-…-ba93 \
    --email alice@example.com \
    --freshness-url "https://adeptus.example.com/api/freshness?id={doc_id}&v={doc_version}" \
    --doc-id   doc-uuid \
    --doc-version v42
```

Use `--output -` to stream to stdout. Skip the visible watermark with
`--no-visible`. Skip the freshness check by omitting `--freshness-url`.

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
let bytes = std::fs::read("cached.pdf")?;
let out = pdf_stamp::personalise(
    &bytes,
    &pdf_stamp::Recipient {
        name: "Alice".into(),
        identifier: "uuid".into(),
        email: Some("alice@example.com".into()),
    },
    &pdf_stamp::Options::default(),
)?;
std::fs::write("alice.pdf", out)?;
```

## Status

- [x] API surface and CLI scaffolding
- [x] Invisible `/Info` metadata stamping (v0)
- [x] Freshness check via `/OpenAction` JavaScript (v0)
- [ ] Visible content-stream watermark emission
- [ ] XMP metadata packet (in addition to `/Info`)
- [ ] Hidden in-document banner annotation toggled by the freshness
      script (instead of `app.alert`) for a less intrusive UX
- [ ] Page-level signature / hash chain so post-stamp tampering is
      detectable
