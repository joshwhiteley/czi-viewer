#!/usr/bin/env bash
# Build the current preview and optionally publish its tag for GitHub Actions.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(awk -F '"' '/^version = / { print $2; exit }' "$repo_root/crates/czi-app/Cargo.toml")
tag="preview-v${version}"
publish=0

usage() {
  cat <<EOF
Usage: scripts/release-preview.sh [--publish]

Build and verify ${tag} from a clean main branch.

  --publish  Push main, create and push ${tag}, and trigger the GitHub prerelease workflow.

Before releasing another version, update crates/czi-app/Cargo.toml and Cargo.lock.
EOF
}

case ${1:-} in
  '') ;;
  --publish) publish=1 ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac
(( $# <= 1 )) || { usage >&2; exit 2; }

cd -- "$repo_root"
branch=$(git branch --show-current)
[[ $branch == main ]] || {
  printf 'Release previews must be built from main, not %s.\n' "${branch:-a detached HEAD}" >&2
  exit 1
}
[[ -z $(git status --porcelain) ]] || {
  printf '%s\n' 'Commit or remove working-tree changes before releasing.' >&2
  exit 1
}
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  printf 'Tag already exists locally: %s\n' "$tag" >&2
  exit 1
fi
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
  printf 'Tag already exists on origin: %s\n' "$tag" >&2
  exit 1
fi

"$repo_root/scripts/package-macos.sh"
"$repo_root/scripts/verify-macos-release.sh"

if (( ! publish )); then
  printf '\nBuilt and verified %s.\n' "$tag"
  printf 'Publish it with: scripts/release-preview.sh --publish\n'
  exit 0
fi

git push origin main
git tag -a "$tag" -m "CZI Viewer ${tag}"
if ! git push origin "$tag"; then
  git tag -d "$tag" >/dev/null
  printf 'Could not push %s; removed the local tag.\n' "$tag" >&2
  exit 1
fi

printf '\nPublished %s. Follow the release build at:\n' "$tag"
printf 'https://github.com/joshwhiteley/czi-viewer/actions/workflows/preview.yml\n'
