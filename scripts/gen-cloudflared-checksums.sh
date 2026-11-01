#!/usr/bin/env bash
# Regenerate — or verify — the pinned cloudflared SHA-256 table.
#
#   scripts/gen-cloudflared-checksums.sh            # print the table
#   scripts/gen-cloudflared-checksums.sh --check    # verify the table
#
# The digests are taken from the OFFICIAL release URL — a mirror may serve
# different bytes, and a digest computed from a mirror is not the digest we
# want to pin.
#
# To refresh the table when bumping CLOUDFLARED_VERSION:
#   1. edit CLOUDFLARED_VERSION in crates/cloudpipe-sdk/src/binary.rs
#   2. scripts/gen-cloudflared-checksums.sh
#   3. paste the output into ASSET_SHA256
#
# The asset list and the version are parsed out of the SDK source, so the
# two can never drift apart.
#
# If github.com is unreachable, set CFP_GITHUB_PROXY to a mirror prefix.
# The digest then comes from the mirror rather than from GitHub, and the
# script says so — cross-check it against the `digest` field of the
# GitHub release API before pinning it:
#
#   curl -fsSL https://api.github.com/repos/cloudflare/cloudflared/releases/tags/<version>
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/crates/cloudpipe-sdk/src/binary.rs"
BASE="https://github.com/cloudflare/cloudflared/releases/download"

if [ ! -f "$SRC" ]; then
  echo "cannot find $SRC" >&2
  exit 1
fi

VERSION="$(sed -n 's/^pub const CLOUDFLARED_VERSION: &str = "\(.*\)";$/\1/p' "$SRC")"
if [ -z "$VERSION" ]; then
  echo "could not parse CLOUDFLARED_VERSION from $SRC" >&2
  exit 1
fi

# Pull the asset names straight out of the `ALL_ASSETS` constant so the
# two can never drift.
ASSETS="$(sed -n '/^pub const ALL_ASSETS/,/^];$/p' "$SRC" \
  | sed -n 's/^[[:space:]]*"\(cloudflared[^"]*\)",$/\1/p')"

if [ -z "$ASSETS" ]; then
  echo "could not parse ALL_ASSETS from $SRC" >&2
  exit 1
fi

PROXY="${CFP_GITHUB_PROXY:-}"
PROXY="${PROXY%/}"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Prints the pinned table in the same `<asset>\t<digest>` shape this
# script produces, so the two can be diffed line by line.
pinned_digests() {
  awk '
    /^const ASSET_SHA256/ { in_table = 1 }
    in_table && /^];/ { in_table = 0 }
    in_table && /^[[:space:]]*"cloudflared[^"]*",$/ {
      name = $0
      sub(/^[[:space:]]*"/, "", name)
      sub(/",$/, "", name)
      pending = name
      next
    }
    in_table && pending != "" && /^[[:space:]]*"[0-9a-f]{64}",$/ {
      digest = $0
      sub(/^[[:space:]]*"/, "", digest)
      sub(/",$/, "", digest)
      print pending "\t" digest
      pending = ""
    }
  ' "$SRC"
}

# Downloads one asset and prints its SHA-256.
#
# The official URL is tried first and the mirror is only a fallback: a
# mirror's bytes are not the bytes we are pinning, so the fallback says
# out loud that the digest did not come from GitHub.
#
# The asset is deleted as soon as it has been hashed — the assets run to
# ~70 MB each, and holding all seven at once would need half a gigabyte
# of scratch space for no reason.
digest_of() {
  local asset="$1" url dest digest

  for url in "$BASE/$VERSION/$asset" ${PROXY:+"$PROXY/$BASE/$VERSION/$asset"}; do
    dest="$WORKDIR/asset"
    # `--connect-timeout` matters more than it looks: without it a
    # half-open connection blocks for the full `--max-time`, so one
    # unreachable asset costs minutes before the next retry.
    if ! curl -fsSL --retry 5 --retry-all-errors \
      --connect-timeout 30 --max-time 900 -o "$dest" "$url"; then
      rm -f "$dest"
      continue
    fi
    if [ "$url" != "$BASE/$VERSION/$asset" ]; then
      echo "warning: $asset was fetched from the mirror $PROXY, not from GitHub" >&2
      echo "warning: cross-check it against the GitHub release API before pinning" >&2
    fi
    digest="$(sha256sum "$dest" | cut -d' ' -f1)"
    rm -f "$dest"
    printf '%s' "$digest"
    return 0
  done

  return 1
}

GENERATED="$WORKDIR/generated.tsv"
: > "$GENERATED"
for asset in $ASSETS; do
  if digest="$(digest_of "$asset")"; then
    printf '%s\t%s\n' "$asset" "$digest" >> "$GENERATED"
  else
    echo "failed to download $asset for version $VERSION" >&2
    exit 1
  fi
done

case "${1:-}" in
  --check)
    PINNED="$WORKDIR/pinned.tsv"
    pinned_digests > "$PINNED"
    if ! diff -u "$PINNED" "$GENERATED"; then
      echo "::error::the pinned cloudflared digests differ from the upstream release" >&2
      echo "::error::run scripts/gen-cloudflared-checksums.sh and paste the result into ASSET_SHA256" >&2
      exit 1
    fi
    echo "the pinned digests match the upstream release"
    ;;
  "")
    cat "$GENERATED"
    ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac
