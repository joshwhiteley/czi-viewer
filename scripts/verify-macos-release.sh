#!/usr/bin/env bash
# Verify the Apple Silicon preview archive without running its executable.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
product='CZI Viewer'
target='aarch64-apple-darwin'
minimum_macos='11.0'
version=$(awk -F '"' '/^version = / { print $2; exit }' "$repo_root/crates/czi-app/Cargo.toml")
if (( $# == 0 )); then
  "$0" "$repo_root/dist/CZI-Viewer-${version}-${target}-preview.zip"
  "$0" "$repo_root/dist/CZI-Viewer-${version}-${target}-preview.dmg"
  exit 0
fi
archive=$1
dist_dir=$(CDPATH= cd -- "$(dirname -- "$archive")" && pwd)
archive="$dist_dir/$(basename -- "$archive")"
archive_name=$(basename -- "$archive")
artifact_stem="CZI-Viewer-${version}-${target}-preview"
sbom_name="${artifact_stem}-sbom.cdx.json"
notices_name="${artifact_stem}-THIRD-PARTY-NOTICES.html"
extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/czi-viewer-verify.XXXXXX")
mount_dir=$(mktemp -d "${TMPDIR:-/tmp}/czi-viewer-mount.XXXXXX")
mounted=0

cleanup() {
  if (( mounted )); then
    hdiutil detach "$mount_dir" -quiet || true
  fi
  rm -rf -- "$extract_dir" "$mount_dir"
}
trap cleanup EXIT HUP INT TERM

fail() {
  printf 'Verification failed: %s\n' "$*" >&2
  exit 1
}

[[ $(uname -s) == Darwin ]] || fail 'macOS tools are required'
[[ -f "$archive" ]] || fail "archive does not exist: $archive"
[[ -f "$dist_dir/$sbom_name" ]] || fail "SBOM does not exist in $dist_dir"
[[ -f "$dist_dir/$notices_name" ]] || fail "notices do not exist in $dist_dir"
[[ -f "$dist_dir/SHA256SUMS" ]] || fail "SHA256SUMS does not exist in $dist_dir"
for tool in codesign diff ditto hdiutil lipo otool plutil readlink shasum cmp grep jq; do
  command -v "$tool" >/dev/null || fail "required tool not found: $tool"
done

zip_name="${artifact_stem}.zip"
dmg_name="${artifact_stem}.dmg"
expected_manifest=$(printf '%s\n%s\n%s\n%s' "$zip_name" "$dmg_name" "$sbom_name" "$notices_name")
actual_manifest=$(awk 'NF != 2 { exit 1 } { print $2 }' "$dist_dir/SHA256SUMS") || fail 'SHA256SUMS has an invalid line'
[[ $actual_manifest == "$expected_manifest" ]] || fail 'SHA256SUMS does not exactly cover this preview archive, SBOM, and notices'
(
  cd -- "$dist_dir"
  shasum -a 256 -c SHA256SUMS
)
normalized_sbom="$extract_dir/normalized-sbom.cdx.json"
jq -S . "$dist_dir/$sbom_name" > "$normalized_sbom"
cmp -s "$normalized_sbom" "$dist_dir/$sbom_name" || fail 'SBOM keys are not in stable order'
if jq -e '.. | objects | select(has("serialNumber") or has("timestamp"))' "$dist_dir/$sbom_name" >/dev/null; then
  fail 'SBOM contains a nondeterministic serialNumber or timestamp'
fi
if jq -e '.. | strings | select(test("/Users/|file:///|download_url=file:"))' "$dist_dir/$sbom_name" >/dev/null; then
  fail 'SBOM contains an absolute builder path or local download URL'
fi
case "$archive" in
  *.zip)
    ditto -x -k "$archive" "$extract_dir"
    ;;
  *.dmg)
    hdiutil attach -quiet -readonly -nobrowse -noautoopen -mountpoint "$mount_dir" "$archive"
    mounted=1
    [[ -L "$mount_dir/Applications" ]] || fail 'DMG is missing its Applications shortcut'
    [[ $(readlink "$mount_dir/Applications") == /Applications ]] || fail 'DMG Applications shortcut has the wrong target'
    [[ -d "$mount_dir/${product}.app" ]] || fail 'DMG is missing the application bundle'
    ditto "$mount_dir/${product}.app" "$extract_dir/${product}.app"
    hdiutil detach "$mount_dir" -quiet
    mounted=0
    ;;
  *)
    fail "unsupported archive type: $archive_name"
    ;;
esac
app="$extract_dir/${product}.app"
binary="$app/Contents/MacOS/czi-viewer"
plist="$app/Contents/Info.plist"

[[ -x "$binary" ]] || fail 'missing application executable'
[[ -f "$plist" ]] || fail 'missing Info.plist'
[[ -f "$app/Contents/Resources/LICENSE-APACHE" ]] || fail 'missing Apache license'
[[ -f "$app/Contents/Resources/LICENSE-MIT" ]] || fail 'missing MIT license'
[[ -f "$app/Contents/Resources/THIRD-PARTY-NOTICES.html" ]] || fail 'missing third-party notices'
[[ -f "$app/Contents/Resources/CZIViewer.icns" ]] || fail 'missing application icon'
[[ -f "$app/Contents/Resources/$sbom_name" ]] || fail 'missing SBOM'
cmp -s "$dist_dir/$sbom_name" "$app/Contents/Resources/$sbom_name" || fail 'embedded SBOM differs from the checksummed SBOM'
cmp -s "$dist_dir/$notices_name" "$app/Contents/Resources/THIRD-PARTY-NOTICES.html" || fail 'embedded notices differ from the checksummed notices'

plutil -lint "$plist" >/dev/null
[[ $(plutil -extract CFBundleIdentifier raw -o - "$plist") == 'io.github.joshwhiteley.czi-viewer' ]] || fail 'wrong bundle identifier'
[[ $(plutil -extract CFBundleDisplayName raw -o - "$plist") == "$product" ]] || fail 'wrong product name'
[[ $(plutil -extract CFBundleName raw -o - "$plist") == "$product" ]] || fail 'wrong bundle name'
[[ $(plutil -extract CFBundleExecutable raw -o - "$plist") == 'czi-viewer' ]] || fail 'wrong executable name'
[[ $(plutil -extract CFBundleIconFile raw -o - "$plist") == 'CZIViewer' ]] || fail 'wrong icon name'
[[ $(plutil -extract CFBundlePackageType raw -o - "$plist") == 'APPL' ]] || fail 'wrong package type'
[[ $(plutil -extract CFBundleShortVersionString raw -o - "$plist") == "$version" ]] || fail 'wrong short version'
[[ $(plutil -extract CFBundleVersion raw -o - "$plist") == "$version" ]] || fail 'wrong bundle version'
[[ $(plutil -extract LSMinimumSystemVersion raw -o - "$plist") == "$minimum_macos" ]] || fail 'wrong Info.plist deployment target'

[[ $(lipo -archs "$binary") == arm64 ]] || fail 'executable is not exactly arm64'
minimum_from_binary=$(otool -l "$binary" | awk '
  $1 == "cmd" && $2 == "LC_BUILD_VERSION" { build = 1; next }
  build && $1 == "platform" && $2 != "1" { build = 0; next }
  build && $1 == "minos" { print $2; exit }
  $1 == "cmd" && $2 == "LC_VERSION_MIN_MACOSX" { legacy = 1; next }
  legacy && $1 == "version" { print $2; exit }
')
[[ $minimum_from_binary == "$minimum_macos" ]] || fail "binary deployment target is $minimum_from_binary, expected $minimum_macos"

while IFS= read -r dylib; do
  case "$dylib" in
    /System/Library/*|/usr/lib/*) ;;
    *) fail "non-system dynamic library: $dylib" ;;
  esac
done < <(otool -L "$binary" | tail -n +2 | awk '{ print $1 }')

codesign --verify --deep --strict --verbose=4 "$app"
signature=$(codesign -dv --verbose=4 "$app" 2>&1)
[[ $signature == *'Signature=adhoc'* ]] || fail 'application is not ad-hoc signed'
[[ $signature != *'Authority='* ]] || fail 'application unexpectedly has an identity certificate'

[[ -z $(find "$app/Contents" -type l -print -quit) ]] || fail 'application contains a symlink'
if grep -R -a -q '/Users/' "$app/Contents"; then
  fail 'application contains an absolute builder path'
fi
expected_files=$(printf '%s\n' \
  '_CodeSignature/CodeResources' \
  'Info.plist' \
  'MacOS/czi-viewer' \
  "Resources/$sbom_name" \
  'Resources/CZIViewer.icns' \
  'Resources/LICENSE-APACHE' \
  'Resources/LICENSE-MIT' \
  'Resources/THIRD-PARTY-NOTICES.html')
actual_files=$(find "$app/Contents" -type f -print | sed "s#^$app/Contents/##" | sort)
[[ $actual_files == "$expected_files" ]] || fail 'application contains an unexpected or missing file'

standalone_app="$dist_dir/${product}.app"
[[ -d "$standalone_app" ]] || fail "standalone application bundle does not exist in $dist_dir"
diff -qr "$app" "$standalone_app" >/dev/null || fail 'standalone application bundle differs from the archived app'

printf 'Verified %s\n' "$archive"
