#!/bin/sh
set -eu

cache_dir=${CZI_TEST_CACHE:-"$(CDPATH= cd -- "$(dirname -- "$0")/../test-data" && pwd)/cache"}
mkdir -p "$cache_dir"

verify_sha256() {
    expected=$1
    path=$2
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$path" | cut -d ' ' -f 1)
    else
        actual=$(shasum -a 256 "$path" | cut -d ' ' -f 1)
    fi
    if [ "$actual" != "$expected" ]; then
        printf 'checksum mismatch for %s\nexpected: %s\nactual:   %s\n' "$path" "$expected" "$actual" >&2
        return 1
    fi
}

download() {
    filename=$1
    url=$2
    sha256=$3
    destination="$cache_dir/$filename"

    if [ ! -f "$destination" ]; then
        printf 'Downloading %s\n' "$filename"
        curl --fail --location --retry 3 --output "$destination.partial" "$url"
        mv "$destination.partial" "$destination"
    fi
    verify_sha256 "$sha256" "$destination"
    printf 'Verified %s\n' "$destination"
}

download \
    'T=3_Z=5_CH=2.czi' \
    'https://zenodo.org/api/records/7015307/files/T=3_Z=5_CH=2.czi/content' \
    'fa940d1a7816be6628f0b508c39258a69b83f29f98b950692f819fbe7cee5427'

download \
    'Zeiss-5-SlidePreview-Zstd1-HiLo.czi' \
    'https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-SlidePreview-Zstd1-HiLo.czi' \
    'fd5cea806a557954e2ff63c2eb0dd23c83fe05d4900aac9d81a4af2a412288b0'

download \
    'Zeiss-5-JXR.czi' \
    'https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi' \
    'c202ddf7b0bd473cdbe29977aee07c10c207077779485c0b1f876e8c00da77f7'
