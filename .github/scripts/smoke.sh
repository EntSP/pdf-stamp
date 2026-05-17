#!/usr/bin/env bash
#
# Stamp a PDF with the binary given, and check the result is what the
# README promises: the input's bytes, unchanged, at the front of the
# output, and the recipient in the /Info dictionary appended after them --
# the trace that survives a cropped watermark.
#
# The PDF is one blank page written here, so no fixture lives in the
# repository. check.sh runs this on the build it made; release.yml runs it
# on each asset before publishing it.
#
#   bash .github/scripts/smoke.sh <pdf-stamp binary>
set -euo pipefail

bin="${1:?usage: smoke.sh <pdf-stamp binary>}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
in="$work/in.pdf"
out="$work/out.pdf"

fail() { echo "::error::$*" >&2; exit 1; }
size() { echo $(( $(wc -c < "$1") )); }

# One blank A4 page, with an empty content stream for the stamp to follow.
# The cross-reference offsets are measured as the file is written: a PDF
# whose xref is wrong is one a strict reader rejects.
printf '%%PDF-1.4\n' > "$in"
o1=$(size "$in"); printf '1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n' >> "$in"
o2=$(size "$in"); printf '2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n' >> "$in"
o3=$(size "$in"); printf '3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << >> /Contents 4 0 R >>\nendobj\n' >> "$in"
o4=$(size "$in"); printf '4 0 obj\n<< /Length 0 >>\nstream\n\nendstream\nendobj\n' >> "$in"
xref=$(size "$in")
printf 'xref\n0 5\n0000000000 65535 f \n%010d 00000 n \n%010d 00000 n \n%010d 00000 n \n%010d 00000 n \n' \
  "$o1" "$o2" "$o3" "$o4" >> "$in"
printf 'trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n' "$xref" >> "$in"

"$bin" --input "$in" --output "$out" --identifier smoke-test-recipient --email alice@example.com

before=$(size "$in")
after=$(size "$out")
[ "$after" -gt "$before" ] || fail "the output ($after bytes) is no larger than the input ($before)"
# An incremental update appends; it rewrites nothing. `head -c` and `cmp -`
# rather than `cmp -n`, which macOS's cmp does not have.
head -c "$before" "$out" | cmp -s - "$in" || fail "the input's bytes are not intact at the front of the output"
grep -a -q 'Stamp.RecipientID' "$out" || fail "no Stamp.RecipientID in the output's /Info"
grep -a -q 'smoke-test-recipient' "$out" || fail "the recipient's identifier is not in the output"
echo "stamped: $before -> $after bytes, the input intact at the front, the recipient recorded"
