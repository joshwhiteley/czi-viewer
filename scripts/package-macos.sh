#!/usr/bin/env bash
# Build the unsigned-preview, ad-hoc signed Apple Silicon application bundle.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
product='CZI Viewer'
target='aarch64-apple-darwin'
minimum_macos='12.3'
version=$(awk -F '"' '/^version = / { print $2; exit }' "$repo_root/crates/czi-app/Cargo.toml")
artifact_stem="CZI-Viewer-${version}-${target}-preview"
dist_dir="$repo_root/dist"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/czi-viewer-package.XXXXXX")
app="$work_dir/${product}.app"
resources="$app/Contents/Resources"
zip_path="$dist_dir/${artifact_stem}.zip"
dmg_path="$dist_dir/${artifact_stem}.dmg"
standalone_app="$dist_dir/${product}.app"
sbom_path="$dist_dir/${artifact_stem}-sbom.cdx.json"
notices_path="$dist_dir/${artifact_stem}-THIRD-PARTY-NOTICES.html"
basic_notices_path="$dist_dir/${artifact_stem}-BASIC-THIRD-PARTY-NOTICES.html"
basic_sbom_path="$dist_dir/${artifact_stem}-basic-helper-sbom.cdx.json"
source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "$repo_root" log -1 --format=%ct)}

cleanup() {
  rm -rf -- "$work_dir"
}
trap cleanup EXIT HUP INT TERM

if [[ $(uname -s) != Darwin || $(uname -m) != arm64 ]]; then
  printf '%s\n' 'This package script must run on an Apple Silicon Mac.' >&2
  exit 1
fi

[[ $source_date_epoch =~ ^[0-9]+$ ]] || {
  printf 'SOURCE_DATE_EPOCH must be seconds since the Unix epoch: %s\n' "$source_date_epoch" >&2
  exit 1
}
export SOURCE_DATE_EPOCH="$source_date_epoch"
bundle_timestamp=$(TZ=UTC date -r "$source_date_epoch" '+%Y%m%d%H%M.%S')

for tool in cargo date iconutil codesign ditto file hdiutil plutil shasum uv zip jq; do
  command -v "$tool" >/dev/null || {
    printf 'Required tool not found: %s\n' "$tool" >&2
    exit 1
  }
done
cargo about --version | grep -q '0\.8\.2$' || {
  printf '%s\n' 'cargo-about 0.8.2 is required; install it with: cargo install cargo-about --version 0.8.2 --locked' >&2
  exit 1
}

cd -- "$repo_root"
mkdir -p -- "$dist_dir" "$app/Contents/MacOS" "$resources"
rustflags="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$repo_root=."
MACOSX_DEPLOYMENT_TARGET="$minimum_macos" RUSTFLAGS="$rustflags" \
  cargo build --locked --release --target "$target" --package czi-viewer

binary="$repo_root/target/$target/release/czi-viewer"
[[ -x "$binary" ]] || {
  printf 'Build did not produce %s\n' "$binary" >&2
  exit 1
}

cp -- "$binary" "$app/Contents/MacOS/czi-viewer"
cp -- "$repo_root/LICENSE-APACHE" "$resources/LICENSE-APACHE"
cp -- "$repo_root/LICENSE-MIT" "$resources/LICENSE-MIT"
sed "s/@VERSION@/${version}/g" "$repo_root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
iconutil -c icns "$repo_root/packaging/macos/CZIViewer.iconset" -o "$resources/CZIViewer.icns"

"$repo_root/scripts/build-basic-helper.sh" \
  "$resources/BaSiC/czi-basic-viewer-helper"
mv -- "$resources/BaSiC/BASIC-THIRD-PARTY-NOTICES.html" "$resources/THIRD-PARTY-NOTICES-BASIC.html"
mv -- "$resources/BaSiC/basic-helper-sbom.cdx.json" "$resources/$(basename -- "$basic_sbom_path")"

"$repo_root/scripts/generate-sbom.sh" "$sbom_path"
cargo about generate "$repo_root/packaging/macos/THIRD-PARTY-NOTICES.hbs" \
  --config "$repo_root/packaging/macos/about.toml" \
  --target "$target" \
  --output-file "$notices_path"
cp -- "$sbom_path" "$resources/$(basename -- "$sbom_path")"
cp -- "$notices_path" "$resources/THIRD-PARTY-NOTICES.html"
cp -- "$resources/THIRD-PARTY-NOTICES-BASIC.html" "$basic_notices_path"
cp -- "$resources/$(basename -- "$basic_sbom_path")" "$basic_sbom_path"

# This is intentionally not Developer ID signing or notarization. Sign frozen helper
# Mach-O files explicitly because Python metadata directories are not code bundles.
sign_adhoc() {
  local output
  if ! output=$(codesign --force --sign - --timestamp=none "$1" 2>&1); then
    printf '%s\n' "$output" >&2
    return 1
  fi
}
while IFS= read -r native; do
  file "$native" | grep -q 'Mach-O' || continue
  sign_adhoc "$native"
done < <(find "$resources/BaSiC" -type f \( -name '*.dylib' -o -name '*.so' -o -perm -111 \) -print)
sign_adhoc "$app"
# Normalize every bundle entry after signing so the archive has stable timestamps.
find "$app" -exec touch -t "$bundle_timestamp" {} +

rm -rf -- "$standalone_app"
ditto "$app" "$standalone_app"

rm -f -- "$zip_path" "$dmg_path" "$dist_dir/SHA256SUMS"
(
  cd -- "$work_dir"
  find "${product}.app" -print | LC_ALL=C sort | zip -X -y -q "$zip_path" -@
)

dmg_staging="$work_dir/dmg"
mkdir -p -- "$dmg_staging"
ditto "$app" "$dmg_staging/${product}.app"
ln -s /Applications "$dmg_staging/Applications"
hdiutil create -quiet -ov -volname "$product" -srcfolder "$dmg_staging" -format UDZO "$dmg_path"

(
  cd -- "$dist_dir"
  shasum -a 256 \
    "$(basename -- "$zip_path")" \
    "$(basename -- "$dmg_path")" \
    "$(basename -- "$sbom_path")" \
    "$(basename -- "$notices_path")" \
    "$(basename -- "$basic_sbom_path")" \
    "$(basename -- "$basic_notices_path")" > SHA256SUMS
)

"$repo_root/scripts/verify-macos-release.sh" "$zip_path"
"$repo_root/scripts/verify-macos-release.sh" "$dmg_path"
printf 'Wrote %s\n' "$zip_path"
printf 'Wrote %s\n' "$dmg_path"
printf 'Wrote %s\n' "$standalone_app"
printf 'Wrote %s\n' "$dist_dir/SHA256SUMS"
