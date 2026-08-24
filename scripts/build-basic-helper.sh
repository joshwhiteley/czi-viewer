#!/usr/bin/env bash
# Build the bundled Apple Silicon BaSiCPy protocol helper with pinned Python wheels.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-}
[[ -n $output ]] || {
  printf '%s\n' 'Usage: scripts/build-basic-helper.sh OUTPUT_DIRECTORY' >&2
  exit 2
}
case "$output" in
  /*) ;;
  *) output="$PWD/$output" ;;
esac

[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || {
  printf '%s\n' 'The bundled BaSiCPy helper must be built on Apple Silicon macOS.' >&2
  exit 1
}
command -v jq >/dev/null || {
  printf '%s\n' 'jq is required to build the bundled BaSiCPy helper.' >&2
  exit 1
}
command -v uv >/dev/null || {
  printf '%s\n' 'uv is required to build the bundled BaSiCPy helper.' >&2
  exit 1
}

source_file="$repo_root/packaging/basic-helper/helper.py"
smoke_test="$repo_root/packaging/basic-helper/smoke_test.py"
lock_file="$repo_root/packaging/basic-helper/requirements.lock"
build_root="$repo_root/target/basic-helper-build"
venv="$build_root/venv"
dist="$build_root/dist/czi-basic-viewer-helper"
marker="$build_root/fingerprint"
fingerprint=$(
  { printf '%s\n' 'czi-basic-pyinstaller-v1'; shasum -a 256 "$source_file" "$smoke_test" "$lock_file"; } \
    | shasum -a 256 | awk '{print $1}'
)

if [[ ! -x "$dist/czi-basic-viewer-helper" || ! -f "$marker" || $(<"$marker") != "$fingerprint" ]]; then
  rm -rf -- "$build_root"
  mkdir -p -- "$build_root"
  UV_NO_PROJECT=1 uv venv --no-project --managed-python --python 3.11 "$venv"
  UV_NO_PROJECT=1 uv pip sync --python "$venv/bin/python" --require-hashes "$lock_file"
  "$venv/bin/pyinstaller" \
    --noconfirm \
    --clean \
    --onedir \
    --name czi-basic-viewer-helper \
    --distpath "$build_root/dist" \
    --workpath "$build_root/work" \
    --specpath "$build_root" \
    --collect-all basicpy \
    --collect-all torch_dct \
    --recursive-copy-metadata basicpy \
    "$source_file"
  "$dist/czi-basic-viewer-helper" --help >/dev/null
  "$venv/bin/python" "$smoke_test" "$dist/czi-basic-viewer-helper"
  printf '%s\n' "$fingerprint" > "$marker"
fi

rm -rf -- "$output"
mkdir -p -- "$(dirname -- "$output")"
ditto "$dist" "$output"

basic_notices="$(dirname -- "$output")/BASIC-THIRD-PARTY-NOTICES.html"
"$venv/bin/pip-licenses" \
  --from=mixed \
  --format=html \
  --with-license-file \
  --output-file "$basic_notices"
"$venv/bin/python" - "$basic_notices" "$venv" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
prefix = sys.argv[2]
path.write_text(path.read_text(encoding="utf-8").replace(prefix, "."), encoding="utf-8")
PY
basic_sbom="$(dirname -- "$output")/basic-helper-sbom.cdx.json"
"$venv/bin/cyclonedx-py" environment "$venv/bin/python" \
  --output-format JSON \
  --output-reproducible \
  --output-file "$basic_sbom.tmp"
jq -S . "$basic_sbom.tmp" > "$basic_sbom"
rm -f -- "$basic_sbom.tmp"

printf 'Wrote %s\n' "$output"
