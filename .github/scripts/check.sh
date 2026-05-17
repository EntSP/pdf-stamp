#!/usr/bin/env bash
#
# Everything that has to be true before pdf-stamp is published.
#
# ONE COPY: ci.yml runs it on every push and pull request, on Linux, macOS
# and Windows, and release.yml runs it before anything is published. A
# second list of steps drifts, and the half that drifts is always the one
# nobody watches.
#
# Inputs:
#   $1          the pdf-stamp checkout                 default: $PWD
#   STRICT      1 to fail on clippy warnings           default: 1
set -euo pipefail

ROOT="${1:-$PWD}"
cd "$ROOT"
ROOT="$PWD"
STRICT="${STRICT:-1}"

group() { echo "::group::$*"; }
endgroup() { echo "::endgroup::"; }
die() { echo "::error::$*" >&2; exit 1; }

[ -f Cargo.toml ] || die "no Cargo.toml in $ROOT — point this at the pdf-stamp checkout"

failed=""
step() {
  local name="$1"; shift
  group "$name"
  if "$@"; then
    endgroup
  else
    endgroup
    echo "::error::$name failed"
    failed="$failed $name"
  fi
}

# Formatting first: it is the cheapest, and a reformat in the middle of a
# clippy fix is a confusing diff to review.
step "cargo fmt" cargo fmt --all --check

# Cargo.lock is committed, and has to agree with Cargo.toml: the release
# publishes with `cargo publish --locked`, which refuses one that does not,
# and finding that out here costs a push rather than a release.
lock_is_current() { cargo metadata --locked --format-version 1 >/dev/null; }
step "Cargo.lock is current" lock_is_current

if [ "$STRICT" = "1" ]; then
  step "cargo clippy" cargo clippy --locked --all-targets -- -D warnings
else
  step "cargo clippy" cargo clippy --locked --all-targets
fi

step "cargo test" cargo test --locked --all-targets

# The release profile is what ships.
step "cargo build --release" cargo build --locked --release --bin pdf-stamp

# A binary that will not start is worse than one that was never built, and
# one that starts and stamps wrongly is worse again: smoke.sh stamps a page
# and checks the canonical bytes survive it.
step "it runs" ./target/release/pdf-stamp --version
step "it stamps" bash .github/scripts/smoke.sh ./target/release/pdf-stamp

for s in .github/scripts/*.sh; do
  step "bash -n $(basename "$s")" bash -n "$s"
done

if [ -n "$failed" ]; then
  die "failed:$failed"
fi
echo "==> all checks passed"
