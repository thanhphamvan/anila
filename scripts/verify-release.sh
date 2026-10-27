#!/bin/sh
# Downloads an anila-service release archive with the GitHub CLI, checks it
# against the release's SHA256SUMS, and unpacks it only if the hash matches.
#
# Usage: scripts/verify-release.sh [tag] [target]
#   tag     e.g. v0.1.0; defaults to the latest release
#   target  e.g. aarch64-apple-darwin; defaults to this machine
# Env:    OUT_DIR (default .), ANILA_REPO (required: owner/repo)
set -eu
repo=${ANILA_REPO:-}
out=${OUT_DIR:-.}

fail() { printf 'verify-release: %s\n' "$1" >&2; exit 1; }

[ -n "$repo" ] || fail "set ANILA_REPO=owner/repo"

command -v gh >/dev/null || fail "the GitHub CLI (gh) is not installed"
if command -v sha256sum >/dev/null; then
  check() { sha256sum -c "$1"; }
elif command -v shasum >/dev/null; then
  check() { shasum -a 256 -c "$1"; }
else
  fail "neither sha256sum nor shasum is installed"
fi

tag=${1:-$(gh release view --repo "$repo" --json tagName -q .tagName)}
target=${2:-}
if [ -z "$target" ]; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    *) fail "no release build for $(uname -s) $(uname -m)" ;;
  esac
fi

name="anila-service-${tag#v}-$target"
archive="$name.tar.gz"
mkdir -p "$out"
cd "$out"
gh release download "$tag" --repo "$repo" --pattern "$archive" --pattern SHA256SUMS --clobber

# Check against the release-wide list, not just the archive's own .sha256.
grep "  $archive\$" SHA256SUMS > "$archive.sha256" || fail "$archive is not listed in SHA256SUMS"
check "$archive.sha256" || {
  rm -f "$archive"
  fail "checksum mismatch for $archive; deleted it"
}
tar -xzf "$archive"
printf 'Verified %s and unpacked it to %s/%s/\n' "$archive" "$(pwd)" "$name"
