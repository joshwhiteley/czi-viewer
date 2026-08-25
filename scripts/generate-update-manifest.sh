#!/usr/bin/env bash
# Generate the canonical update manifest for a verified preview DMG.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target='aarch64-apple-darwin'
minimum_macos='12.3'
bundle_identifier='io.github.joshwhiteley.czi-viewer'
version=$(awk -F '"' '/^version = / { print $2; exit }' "$repo_root/crates/czi-app/Cargo.toml")

usage() {
  printf 'Usage: scripts/generate-update-manifest.sh <preview.dmg> <update-manifest.json>\n'
}

(( $# == 2 )) || {
  usage >&2
  exit 2
}

dmg=$1
output=$2
expected_name="CZI-Viewer-${version}-${target}-preview.dmg"

[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  printf 'Release version is not a strict three-part version: %s\n' "$version" >&2
  exit 1
}
[[ -f $dmg && ! -L $dmg ]] || {
  printf 'Preview DMG must be a regular, non-symlink file: %s\n' "$dmg" >&2
  exit 1
}
[[ $(basename -- "$dmg") == "$expected_name" ]] || {
  printf 'Preview DMG has the wrong name; expected %s\n' "$expected_name" >&2
  exit 1
}
[[ $output != "$dmg" ]] || {
  printf '%s\n' 'Manifest output must differ from the DMG.' >&2
  exit 1
}
[[ ! -L $output ]] || {
  printf 'Refusing to replace a symlink manifest: %s\n' "$output" >&2
  exit 1
}
command -v shasum >/dev/null || {
  printf '%s\n' 'Required tool not found: shasum' >&2
  exit 1
}

output_dir=$(dirname -- "$output")
[[ -d $output_dir ]] || {
  printf 'Manifest output directory does not exist: %s\n' "$output_dir" >&2
  exit 1
}
size=$(wc -c < "$dmg" | tr -d '[:space:]')
[[ $size =~ ^[1-9][0-9]*$ ]] || {
  printf 'Preview DMG has an invalid size: %s\n' "$size" >&2
  exit 1
}
sha256=$(shasum -a 256 "$dmg" | awk '{ print $1 }')
[[ $sha256 =~ ^[0-9a-f]{64}$ ]] || {
  printf '%s\n' 'Could not calculate a lowercase SHA-256 digest.' >&2
  exit 1
}

temporary=$(mktemp "$output_dir/.update-manifest.XXXXXX")
cleanup() {
  rm -f -- "$temporary"
}
trap cleanup EXIT HUP INT TERM
printf '{"bundle_identifier":"%s","channel":"preview","dmg_name":"%s","dmg_sha256":"%s","dmg_size":%s,"minimum_macos":"%s","schema":1,"tag":"preview-v%s","target":"%s","version":"%s"}\n' \
  "$bundle_identifier" "$expected_name" "$sha256" "$size" "$minimum_macos" "$version" "$target" "$version" > "$temporary"
[[ $(wc -l < "$temporary" | tr -d '[:space:]') == 1 ]] || {
  printf '%s\n' 'Generated manifest is not one canonical line.' >&2
  exit 1
}
mv -f -- "$temporary" "$output"
trap - EXIT HUP INT TERM
printf 'Wrote canonical update manifest %s\n' "$output"
