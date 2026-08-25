#!/usr/bin/env bash
# Sign a canonical update manifest with the operator-held Ed25519 release key.
set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/sign-update-manifest.sh --check-key
       scripts/sign-update-manifest.sh <update-manifest.json> <update-manifest.sig>

The private key defaults to ~/.config/czi-viewer-release/update-signing-key.pem.
The pinned public key defaults to ~/.config/czi-viewer-release/update-signing-public-key.pem.
Set CZI_UPDATE_SIGNING_KEY and CZI_UPDATE_SIGNING_PUBLIC_KEY to override them.
EOF
}

config_dir=${HOME:+$HOME/.config/czi-viewer-release}
key=${CZI_UPDATE_SIGNING_KEY:-${config_dir:+$config_dir/update-signing-key.pem}}
public_key=${CZI_UPDATE_SIGNING_PUBLIC_KEY:-${config_dir:+$config_dir/update-signing-public-key.pem}}
embedded_public_key_hex='b3f32b2a26c334f6956ec73a572b14223f54f1c8811a9a659ce2d9cf87d0be3c'
[[ -n $key && -n $public_key ]] || {
  printf '%s\n' 'HOME is unset and signing key paths were not both provided.' >&2
  exit 1
}

openssl_major() {
  openssl version | awk '$1 == "OpenSSL" { split($2, version, "."); print version[1]; exit }'
}

file_mode() {
  stat -f '%Lp' "$1" 2>/dev/null || stat -c '%a' "$1"
}

file_owner() {
  stat -f '%u' "$1" 2>/dev/null || stat -c '%u' "$1"
}

check_key() {
  command -v openssl >/dev/null || {
    printf '%s\n' 'Required tool not found: openssl' >&2
    return 1
  }
  command -v shasum >/dev/null || {
    printf '%s\n' 'Required tool not found: shasum' >&2
    return 1
  }
  command -v xxd >/dev/null || {
    printf '%s\n' 'Required tool not found: xxd' >&2
    return 1
  }
  [[ $(openssl_major) == 3 ]] || {
    printf '%s\n' 'OpenSSL 3 is required to sign update manifests.' >&2
    return 1
  }
  [[ -f $key && ! -L $key ]] || {
    printf '%s\n' 'Update signing key must be a regular, non-symlink file.' >&2
    return 1
  }
  [[ -f $public_key && ! -L $public_key ]] || {
    printf '%s\n' 'Pinned update public key must be a regular, non-symlink file.' >&2
    return 1
  }
  local mode owner public_mode public_owner private_fingerprint public_fingerprint public_hex
  mode=$(file_mode "$key")
  owner=$(file_owner "$key")
  public_mode=$(file_mode "$public_key")
  public_owner=$(file_owner "$public_key")
  [[ $mode =~ ^[0-7]{3,4}$ && $public_mode =~ ^[0-7]{3,4}$ ]] || {
    printf '%s\n' 'Could not determine signing-key permissions.' >&2
    return 1
  }
  (( (8#$mode & 077) == 0 )) || {
    printf '%s\n' 'Update signing key must not grant group or other permissions.' >&2
    return 1
  }
  (( (8#$public_mode & 022) == 0 )) || {
    printf '%s\n' 'Pinned update public key must not be group- or other-writable.' >&2
    return 1
  }
  [[ $owner == "$(id -u)" && $public_owner == "$(id -u)" ]] || {
    printf '%s\n' 'Update signing keys must be owned by the current user.' >&2
    return 1
  }
  openssl pkey -in "$key" -text -noout </dev/null 2>/dev/null \
    | grep -q '^ED25519 Private-Key:$' || {
      printf '%s\n' 'Update signing key is not an unencrypted Ed25519 private key.' >&2
      return 1
    }
  openssl pkey -pubin -in "$public_key" -text -noout </dev/null 2>/dev/null \
    | grep -q '^ED25519 Public-Key:$' || {
      printf '%s\n' 'Pinned update public key is not an Ed25519 public key.' >&2
      return 1
    }
  private_fingerprint=$(openssl pkey -in "$key" -pubout -outform DER </dev/null 2>/dev/null \
    | shasum -a 256 | awk '{ print $1 }')
  public_fingerprint=$(openssl pkey -pubin -in "$public_key" -pubout -outform DER </dev/null 2>/dev/null \
    | shasum -a 256 | awk '{ print $1 }')
  [[ $private_fingerprint == "$public_fingerprint" ]] || {
    printf '%s\n' 'Update signing key does not match the pinned public key.' >&2
    return 1
  }
  public_hex=$(openssl pkey -pubin -in "$public_key" -outform DER </dev/null 2>/dev/null \
    | tail -c 32 | xxd -p -c 64)
  [[ $public_hex == "$embedded_public_key_hex" ]] || {
    printf '%s\n' 'Configured update key does not match the public key embedded in CZI Viewer.' >&2
    return 1
  }
}

case ${1:-} in
  --check-key)
    (( $# == 1 )) || {
      usage >&2
      exit 2
    }
    check_key
    printf '%s\n' 'Operator-held Ed25519 update signing key matches the pinned public key.'
    exit 0
    ;;
  -h|--help)
    usage
    exit 0
    ;;
esac

(( $# == 2 )) || {
  usage >&2
  exit 2
}
manifest=$1
signature=$2
check_key
command -v jq >/dev/null || {
  printf '%s\n' 'Required tool not found: jq' >&2
  exit 1
}
[[ -f $manifest && ! -L $manifest ]] || {
  printf '%s\n' 'Update manifest must be a regular, non-symlink file.' >&2
  exit 1
}
manifest_size=$(wc -c < "$manifest" | tr -d '[:space:]')
[[ $manifest_size =~ ^[1-9][0-9]*$ && $manifest_size -le 4096 ]] || {
  printf '%s\n' 'Update manifest must contain 1 to 4096 bytes.' >&2
  exit 1
}
[[ $signature != "$manifest" ]] || {
  printf '%s\n' 'Signature output must differ from the manifest.' >&2
  exit 1
}
[[ ! -L $signature ]] || {
  printf '%s\n' 'Refusing to replace a symlink signature.' >&2
  exit 1
}
signature_dir=$(dirname -- "$signature")
[[ -d $signature_dir ]] || {
  printf '%s\n' 'Signature output directory does not exist.' >&2
  exit 1
}

temporary_signature=$(mktemp "$signature_dir/.update-signature.XXXXXX")
temporary_canonical=$(mktemp "${TMPDIR:-/tmp}/czi-update-manifest.XXXXXX")
cleanup() {
  rm -f -- "$temporary_signature" "$temporary_canonical"
}
trap cleanup EXIT HUP INT TERM
jq -ceS '
  if (
    type == "object" and
    keys == ["bundle_identifier", "channel", "dmg_name", "dmg_sha256", "dmg_size", "minimum_macos", "schema", "tag", "target", "version"] and
    .bundle_identifier == "io.github.joshwhiteley.czi-viewer" and
    .channel == "preview" and
    .minimum_macos == "12.3" and
    .schema == 1 and
    .target == "aarch64-apple-darwin" and
    (.version | type == "string" and test("^[0-9]+\\.[0-9]+\\.[0-9]+$")) and
    .tag == ("preview-v" + .version) and
    .dmg_name == ("CZI-Viewer-" + .version + "-aarch64-apple-darwin-preview.dmg") and
    (.dmg_sha256 | type == "string" and test("^[0-9a-f]{64}$")) and
    (.dmg_size | type == "number" and . == floor and . > 0 and . <= 1073741824)
  ) then . else error("manifest does not match the canonical update schema") end
' "$manifest" > "$temporary_canonical"
cmp -s "$manifest" "$temporary_canonical" || {
  printf '%s\n' 'Update manifest bytes are not strict canonical JSON.' >&2
  exit 1
}

openssl pkeyutl -sign -rawin -inkey "$key" -in "$manifest" -out "$temporary_signature" </dev/null
[[ $(wc -c < "$temporary_signature" | tr -d '[:space:]') == 64 ]] || {
  printf '%s\n' 'Ed25519 signature is not exactly 64 bytes.' >&2
  exit 1
}
openssl pkeyutl -verify -rawin -pubin -inkey "$public_key" \
  -in "$manifest" -sigfile "$temporary_signature" </dev/null >/dev/null
mv -f -- "$temporary_signature" "$signature"
trap - EXIT HUP INT TERM
rm -f -- "$temporary_canonical"
printf 'Signed and verified exact canonical manifest bytes in %s\n' "$manifest"
