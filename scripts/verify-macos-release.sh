#!/usr/bin/env bash
# Verify the Apple Silicon preview archive without running its executable.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
product='CZI Viewer'
target='aarch64-apple-darwin'
minimum_macos='12.3'
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
basic_notices_name="${artifact_stem}-BASIC-THIRD-PARTY-NOTICES.html"
basic_sbom_name="${artifact_stem}-basic-helper-sbom.cdx.json"
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
[[ -f "$dist_dir/$basic_notices_name" ]] || fail "BaSiC notices do not exist in $dist_dir"
[[ -f "$dist_dir/$basic_sbom_name" ]] || fail "BaSiC SBOM does not exist in $dist_dir"
[[ -f "$dist_dir/SHA256SUMS" ]] || fail "SHA256SUMS does not exist in $dist_dir"
for tool in codesign diff ditto file hdiutil lipo otool plutil readlink realpath shasum cmp grep jq; do
  command -v "$tool" >/dev/null || fail "required tool not found: $tool"
done

zip_name="${artifact_stem}.zip"
dmg_name="${artifact_stem}.dmg"
expected_manifest=$(printf '%s\n%s\n%s\n%s\n%s\n%s' \
  "$zip_name" "$dmg_name" "$sbom_name" "$notices_name" \
  "$basic_sbom_name" "$basic_notices_name")
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
normalized_basic_sbom="$extract_dir/normalized-basic-sbom.cdx.json"
jq -S . "$dist_dir/$basic_sbom_name" > "$normalized_basic_sbom"
cmp -s "$normalized_basic_sbom" "$dist_dir/$basic_sbom_name" || fail 'BaSiC SBOM keys are not in stable order'
if jq -e '.. | objects | select(has("serialNumber") or has("timestamp"))' "$dist_dir/$basic_sbom_name" >/dev/null; then
  fail 'BaSiC SBOM contains a nondeterministic serialNumber or timestamp'
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
[[ -f "$app/Contents/Resources/THIRD-PARTY-NOTICES-BASIC.html" ]] || fail 'missing BaSiC third-party notices'
[[ -f "$app/Contents/Resources/CZIViewer.icns" ]] || fail 'missing application icon'
[[ -f "$app/Contents/Resources/$sbom_name" ]] || fail 'missing SBOM'
[[ -f "$app/Contents/Resources/$basic_sbom_name" ]] || fail 'missing BaSiC SBOM'
cmp -s "$dist_dir/$sbom_name" "$app/Contents/Resources/$sbom_name" || fail 'embedded SBOM differs from the checksummed SBOM'
cmp -s "$dist_dir/$notices_name" "$app/Contents/Resources/THIRD-PARTY-NOTICES.html" || fail 'embedded notices differ from the checksummed notices'
cmp -s "$dist_dir/$basic_sbom_name" "$app/Contents/Resources/$basic_sbom_name" || fail 'embedded BaSiC SBOM differs from its checksummed file'
cmp -s "$dist_dir/$basic_notices_name" "$app/Contents/Resources/THIRD-PARTY-NOTICES-BASIC.html" || fail 'embedded BaSiC notices differ from their checksummed file'

helper="$app/Contents/Resources/BaSiC/czi-basic-viewer-helper"
helper_real=$(realpath "$helper")
helper_binary="$helper/czi-basic-viewer-helper"
[[ -x "$helper_binary" ]] || fail 'missing bundled BaSiCPy helper executable'

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

while IFS= read -r native; do
  file "$native" | grep -q 'Mach-O' || continue
  [[ $(lipo -archs "$native") == arm64 ]] || fail "bundled helper file is not exactly arm64: $native"
  codesign --verify --strict "$native" || fail "bundled helper file has an invalid signature: $native"
  native_minimum=$(otool -l "$native" | awk '
    $1 == "cmd" && $2 == "LC_BUILD_VERSION" { build = 1; next }
    build && $1 == "minos" { print $2; exit }
    $1 == "cmd" && $2 == "LC_VERSION_MIN_MACOSX" { legacy = 1; next }
    legacy && $1 == "version" { print $2; exit }
  ')
  if [[ -n $native_minimum ]] && ! awk -v found="$native_minimum" -v maximum="$minimum_macos" 'BEGIN {
    split(found, f, "."); split(maximum, m, ".");
    exit !((f[1] + 0 < m[1] + 0) || (f[1] + 0 == m[1] + 0 && f[2] + 0 <= m[2] + 0));
  }'; then
    fail "bundled helper requires macOS $native_minimum: $native"
  fi
done < <(find "$helper" -type f \( -name '*.dylib' -o -name '*.so' -o -perm -111 \) -print)

codesign --verify --strict --verbose=4 "$app"
signature=$(codesign -dv --verbose=4 "$app" 2>&1)
[[ $signature == *'Signature=adhoc'* ]] || fail 'application is not ad-hoc signed'
[[ $signature != *'Authority='* ]] || fail 'application unexpectedly has an identity certificate'

while IFS= read -r link; do
  target=$(readlink "$link")
  [[ $target != /* && $target != *'..'* ]] || fail "unsafe helper symlink: $link"
  resolved=$(realpath "$link")
  [[ $resolved == "$helper_real/"* ]] || fail "helper symlink leaves its bundle: $link"
done < <(find "$app/Contents" -type l -print)
if grep -R -a -F -q "$repo_root" "$app/Contents"; then
  fail 'application contains the project builder path'
fi
expected_files=$(printf '%s\n' \
  '_CodeSignature/CodeResources' \
  'Info.plist' \
  'MacOS/czi-viewer' \
  "Resources/$sbom_name" \
  'Resources/CZIViewer.icns' \
  'Resources/LICENSE-APACHE' \
  'Resources/LICENSE-MIT' \
  'Resources/THIRD-PARTY-NOTICES.html' \
  'Resources/THIRD-PARTY-NOTICES-BASIC.html' \
  "Resources/$basic_sbom_name" | LC_ALL=C sort)
actual_files=$(find "$app/Contents" \
  -path "$helper" -prune -o -type f -print | sed "s#^$app/Contents/##" | LC_ALL=C sort)
if [[ $actual_files != "$expected_files" ]]; then
  printf 'Expected bundle files:\n%s\n' "$expected_files" >&2
  printf 'Actual bundle files:\n%s\n' "$actual_files" >&2
  fail 'application contains an unexpected or missing file'
fi

standalone_app="$dist_dir/${product}.app"
[[ -d "$standalone_app" ]] || fail "standalone application bundle does not exist in $dist_dir"
diff -qr "$app" "$standalone_app" >/dev/null || fail 'standalone application bundle differs from the archived app'

printf 'Verified %s\n' "$archive"
