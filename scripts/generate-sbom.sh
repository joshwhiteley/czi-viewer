#!/usr/bin/env bash
# Generate the release SBOM for the only shipped target. Requires cargo-cyclonedx 0.5.9.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target='aarch64-apple-darwin'
version=$(awk -F '"' '/^version = / { print $2; exit }' "$repo_root/crates/czi-app/Cargo.toml")
output=${1:-"$repo_root/dist/CZI-Viewer-${version}-${target}-preview-sbom.cdx.json"}
output_dir=$(dirname -- "$output")
output_name=$(basename -- "$output" .cdx.json)
source_output="$repo_root/crates/czi-app/${output_name}.json"

command -v cargo >/dev/null
cargo cyclonedx --version | grep -q '0\.5\.9$' || {
  printf '%s\n' 'cargo-cyclonedx 0.5.9 is required; install it with: cargo install cargo-cyclonedx --version 0.5.9 --locked' >&2
  exit 1
}

mkdir -p -- "$output_dir"
find "$repo_root/crates" -maxdepth 2 -type f -name "${output_name}.json" -delete
rm -f -- "$output"
cargo cyclonedx \
  --manifest-path "$repo_root/crates/czi-app/Cargo.toml" \
  --format json \
  --target "$target" \
  --spec-version 1.5 \
  --no-build-deps \
  --override-filename "$output_name" \
  --quiet
[[ -f "$source_output" ]] || {
  printf 'cargo-cyclonedx did not produce %s\n' "$source_output" >&2
  exit 1
}
mv -- "$source_output" "$output"
find "$repo_root/crates" -maxdepth 2 -type f -name "${output_name}.json" -delete
printf 'Wrote %s\n' "$output"
