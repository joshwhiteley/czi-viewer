#!/usr/bin/env bash
# Build the current preview and optionally publish verified CI artifacts locally.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
repository='joshwhiteley/czi-viewer'
workflow='preview.yml'
artifact_name='czi-viewer-macos-preview'
target='aarch64-apple-darwin'
version=$(awk -F '"' '/^version = / { print $2; exit }' "$repo_root/crates/czi-app/Cargo.toml")
tag="preview-v${version}"
artifact_stem="CZI-Viewer-${version}-${target}-preview"
publish=0
publish_work_dir=

usage() {
  cat <<EOF
Usage: scripts/release-preview.sh [--publish]

Build and verify ${tag} from a clean main branch.

  --publish  Push main and ${tag}, wait for its successful CI package run,
             verify and sign the CI artifacts locally, and create a prerelease.

Before releasing another version, update crates/czi-app/Cargo.toml and Cargo.lock.
The operator-held Ed25519 key pair defaults to:
  ~/.config/czi-viewer-release/update-signing-key.pem
  ~/.config/czi-viewer-release/update-signing-public-key.pem
EOF
}

cleanup() {
  if [[ -n $publish_work_dir ]]; then
    rm -rf -- "$publish_work_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

fail() {
  printf 'Release failed: %s\n' "$*" >&2
  exit 1
}

require_tool() {
  command -v "$1" >/dev/null || fail "required tool not found: $1"
}

assert_release_absent() {
  local response first_line
  if response=$(gh api --include "repos/$repository/releases/tags/$tag" 2>&1); then
    fail "GitHub release already exists: $tag"
  fi
  first_line=$(printf '%s\n' "$response" | head -n 1)
  [[ $first_line == HTTP/*' 404 '* ]] || {
    fail "could not confirm that GitHub release $tag is absent"
  }
}

wait_for_tagged_run() {
  local commit=$1 not_before=$2 deadline runs count
  deadline=$((SECONDS + 1800))
  while (( SECONDS < deadline )); do
    runs=$(gh api --method GET "repos/$repository/actions/workflows/$workflow/runs" \
      -f event=push -f head_sha="$commit" -f per_page=100)
    count=$(jq --arg tag "$tag" --arg not_before "$not_before" \
      '[.workflow_runs[] | select(.event == "push" and .head_branch == $tag and .created_at >= $not_before)] | length' \
      <<<"$runs")
    case $count in
      0) sleep 10 ;;
      1)
        jq -r --arg tag "$tag" --arg not_before "$not_before" \
          '.workflow_runs[] | select(.event == "push" and .head_branch == $tag and .created_at >= $not_before) | .id' \
          <<<"$runs"
        return 0
        ;;
      *) fail "more than one new tagged CI run matched $tag and commit $commit" ;;
    esac
  done
  fail "timed out waiting for the new tagged CI run for $tag"
}

case ${1:-} in
  '') ;;
  --publish) publish=1 ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac
(( $# <= 1 )) || { usage >&2; exit 2; }

cd -- "$repo_root"
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "invalid release version: $version"
branch=$(git branch --show-current)
[[ $branch == main ]] || fail "release previews must be built from main, not ${branch:-a detached HEAD}"
[[ -z $(git status --porcelain) ]] || fail 'commit or remove working-tree changes before releasing'
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && fail "tag already exists locally: $tag"
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
  fail "tag already exists on origin: $tag"
fi

"$repo_root/scripts/package-macos.sh"
"$repo_root/scripts/verify-macos-release.sh"

if (( ! publish )); then
  printf '\nBuilt and verified %s.\n' "$tag"
  printf 'Publish it with: scripts/release-preview.sh --publish\n'
  exit 0
fi

for tool in cmp ditto gh jq openssl shasum; do
  require_tool "$tool"
done
gh auth status --hostname github.com >/dev/null
"$repo_root/scripts/sign-update-manifest.sh" --check-key
assert_release_absent

commit=$(git rev-parse HEAD)
git push origin main
git tag -a "$tag" -m "CZI Viewer ${tag}"
not_before=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
if ! git push origin "$tag"; then
  git tag -d "$tag" >/dev/null
  fail "could not push $tag; removed the local tag"
fi

printf 'Waiting for the tagged GitHub Actions package run for %s...\n' "$tag"
run_id=$(wait_for_tagged_run "$commit" "$not_before")
gh run watch "$run_id" --repo "$repository" --exit-status

publish_work_dir=$(mktemp -d "${TMPDIR:-/tmp}/czi-viewer-release.XXXXXX")
artifact_dir="$publish_work_dir/artifacts"
mkdir -m 700 -- "$artifact_dir"
gh run download "$run_id" --repo "$repository" --name "$artifact_name" --dir "$artifact_dir"

zip_name="${artifact_stem}.zip"
dmg_name="${artifact_stem}.dmg"
sbom_name="${artifact_stem}-sbom.cdx.json"
notices_name="${artifact_stem}-THIRD-PARTY-NOTICES.html"
basic_sbom_name="${artifact_stem}-basic-helper-sbom.cdx.json"
basic_notices_name="${artifact_stem}-BASIC-THIRD-PARTY-NOTICES.html"
expected_inventory=$(printf '%s\n' \
  "$zip_name" "$dmg_name" "$sbom_name" "$notices_name" \
  "$basic_sbom_name" "$basic_notices_name" SHA256SUMS | LC_ALL=C sort)
actual_inventory=$(find "$artifact_dir" -mindepth 1 -maxdepth 1 -print \
  | while IFS= read -r path; do basename -- "$path"; done | LC_ALL=C sort)
[[ $actual_inventory == "$expected_inventory" ]] || fail 'downloaded CI artifact inventory is not exact'
while IFS= read -r name; do
  [[ -f $artifact_dir/$name && ! -L $artifact_dir/$name ]] \
    || fail "downloaded CI artifact is not a regular file: $name"
done <<<"$expected_inventory"

printf '%s\n' 'Verifying GitHub build provenance attestations...'
while IFS= read -r name; do
  gh attestation verify "$artifact_dir/$name" \
    --repo "$repository" \
    --signer-workflow "$repository/.github/workflows/preview.yml" \
    --source-ref "refs/tags/$tag" \
    --source-digest "$commit"
done <<<"$expected_inventory"

# The workflow uploads only release files. Reconstruct the standalone app from
# the CI ZIP so the repository verifier can compare the ZIP and DMG bundles.
ditto -x -k "$artifact_dir/$zip_name" "$artifact_dir"
"$repo_root/scripts/verify-macos-release.sh" "$artifact_dir/$zip_name"
"$repo_root/scripts/verify-macos-release.sh" "$artifact_dir/$dmg_name"

manifest="$artifact_dir/${artifact_stem}-update.json"
signature="$artifact_dir/${artifact_stem}-update.json.sig"
canonical_check="$publish_work_dir/update-manifest.canonical-check.json"
"$repo_root/scripts/generate-update-manifest.sh" "$artifact_dir/$dmg_name" "$manifest"
"$repo_root/scripts/generate-update-manifest.sh" "$artifact_dir/$dmg_name" "$canonical_check"
cmp -s "$manifest" "$canonical_check" || fail 'update manifest regeneration was not byte-for-byte stable'
"$repo_root/scripts/sign-update-manifest.sh" "$manifest" "$signature"

assert_release_absent
release_assets=(
  "$artifact_dir/$dmg_name"
  "$artifact_dir/$zip_name"
  "$artifact_dir/$sbom_name"
  "$artifact_dir/$notices_name"
  "$artifact_dir/$basic_sbom_name"
  "$artifact_dir/$basic_notices_name"
  "$artifact_dir/SHA256SUMS"
  "$manifest"
  "$signature"
)
gh release create "$tag" "${release_assets[@]}" \
  --repo "$repository" \
  --verify-tag \
  --prerelease \
  --title "CZI Viewer $tag" \
  --notes-file "$repo_root/.github/preview-release-notes.md"

release_url=$(gh release view "$tag" --repo "$repository" --json url --jq .url)
printf '\nPublished verified, signed prerelease %s\n%s\n' "$tag" "$release_url"
