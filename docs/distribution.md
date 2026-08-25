# macOS preview distribution

The repository builds one distributable product: **CZI Viewer** for Apple Silicon (`aarch64-apple-darwin`). The application bundle identifier is `io.github.joshwhiteley.czi-viewer`. The minimum supported macOS version is 12.3 because of the bundled scientific Python wheels.

## Build a preview

Run this on an Apple Silicon Mac with Xcode command-line tools, Rust 1.88.0, uv 0.11.13, `cargo-about` 0.8.2, `cargo-cyclonedx` 0.5.9, and `jq` installed:

```sh
cargo install cargo-about --version 0.8.2 --locked
cargo install cargo-cyclonedx --version 0.5.9 --locked
scripts/package-macos.sh
scripts/verify-macos-release.sh
```

The package script builds `czi-viewer` with `cargo build --locked --release --target aarch64-apple-darwin`, sets `MACOSX_DEPLOYMENT_TARGET=12.3`, freezes the hash-locked BaSiCPy helper, and creates an ad-hoc signed `CZI Viewer.app`. It includes the Rust executable, Python 3.11/BaSiCPy helper, project licenses, Rust and Python notices/SBOMs, and a temporary repository-owned neutral icon. It does not include OpenConnect, ocproxy, or SSH.

Outputs are ignored under `dist/`:

- `CZI Viewer.app`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview.dmg`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview.zip`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview-sbom.cdx.json`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview-THIRD-PARTY-NOTICES.html`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview-basic-helper-sbom.cdx.json`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview-BASIC-THIRD-PARTY-NOTICES.html`
- `SHA256SUMS`

The DMG presents `CZI Viewer.app` next to an Applications shortcut for normal drag-to-Applications installation. The standalone `.app` is available for local testing. The frozen Python environment makes release archives substantially larger than the earlier Rust-only preview.

`verify-macos-release.sh` validates both the DMG and ZIP, the plist, exact `arm64` architecture, the 12.3 deployment ceiling for bundled Mach-O files, strict ad-hoc code signatures, helper symlink confinement, bundle contents, archive extraction, and every listed SHA-256 checksum. It also confirms that both embedded SBOMs and notice sets match their checksummed standalone files.

To build and verify locally without tagging or publishing, run:

```sh
scripts/release-preview.sh
```

## Offline update signing key

Publishing requires OpenSSL 3, an unencrypted Ed25519 private key, and its pinned public key. Their default paths are:

```text
~/.config/czi-viewer-release/update-signing-key.pem
~/.config/czi-viewer-release/update-signing-public-key.pem
```

Restore the established key pair from the maintainer's protected backup. Do not generate a replacement key for a normal release: existing CZI Viewer installations trust one embedded public key. The following commands document the one-time bootstrap procedure only and must not be rerun after the first updater release:

```sh
install -d -m 700 ~/.config/czi-viewer-release
openssl genpkey -algorithm ED25519 \
  -out ~/.config/czi-viewer-release/update-signing-key.pem
chmod 600 ~/.config/czi-viewer-release/update-signing-key.pem
openssl pkey \
  -in ~/.config/czi-viewer-release/update-signing-key.pem \
  -pubout \
  -out ~/.config/czi-viewer-release/update-signing-public-key.pem
chmod 644 ~/.config/czi-viewer-release/update-signing-public-key.pem
scripts/sign-update-manifest.sh --check-key
```

Set `CZI_UPDATE_SIGNING_KEY` and `CZI_UPDATE_SIGNING_PUBLIC_KEY` to select other protected paths. The signer refuses a private/public pair that does not match the public key embedded in CZI Viewer. Never commit, upload, print, or add the private key to GitHub Actions secrets. Keep the private key offline from CI and hosted services, and back it up through the release operator's secret-management process. Preserve the public-key fingerprint separately.

The local publish command itself requires GitHub network access to download attested CI output and create the release. “Offline” describes custody of the private key outside GitHub and CI; it does not mean that the release workstation is air-gapped while the command runs. The public key is not accepted dynamically from the release being verified; the updater contains the trusted public key.

## Publish a preview

Update the version in `crates/czi-app/Cargo.toml` and `Cargo.lock`, commit the change to a clean `main`, and then run:

```sh
scripts/release-preview.sh --publish
```

The publish flow fails closed and performs these steps:

1. Build and verify the same version locally.
2. Check GitHub authentication, the offline Ed25519 key, tag uniqueness, and release absence.
3. Push `main` and the annotated `preview-v<version>` tag.
4. Wait up to 30 minutes for the unique tag-triggered `preview.yml` run.
5. Require that run to succeed, then download the named CI artifact.
6. Require the exact DMG, ZIP, four sidecars, and `SHA256SUMS` inventory.
7. Verify every GitHub build-provenance attestation against the repository, `preview.yml`, tag ref, and tag commit.
8. Reconstruct the standalone app from the CI ZIP and run `verify-macos-release.sh` against both CI archives.
9. Generate `CZI-Viewer-<version>-aarch64-apple-darwin-preview-update.json`, regenerate it byte-for-byte, sign its exact bytes offline, and verify the new signature.
10. Recheck release absence and create the GitHub prerelease with only the exact approved files.

The manifest is one UTF-8 JSON line followed by one LF. It has fixed lexicographic key order, no insignificant whitespace, and this schema:

```json
{"bundle_identifier":"io.github.joshwhiteley.czi-viewer","channel":"preview","dmg_name":"CZI-Viewer-<version>-aarch64-apple-darwin-preview.dmg","dmg_sha256":"<lowercase SHA-256>","dmg_size":123,"minimum_macos":"12.3","schema":1,"tag":"preview-v<version>","target":"aarch64-apple-darwin","version":"<version>"}
```

The matching `…-update.json.sig` is the raw 64-byte Ed25519 signature over the exact manifest bytes, including the final LF. `SHA256SUMS` covers the CI-built archives and sidecars. The signed manifest separately authenticates the release channel and tag, DMG name, byte size, SHA-256, product identity, platform, minimum macOS version, and release version.

A failed CI run, missing or extra artifact, failed attestation, verification mismatch, unavailable key, duplicate tag, duplicate release, or signature failure leaves the tag for investigation but does not create a release. Increment the version for a replacement release; do not reuse a published tag.

## Preview security posture

These are ad-hoc signed preview builds. They are not Developer ID signed and are not notarized. The offline update signature authenticates release metadata and DMG bytes to software that already trusts the public key. It does not give the app an Apple-trusted identity or notarization ticket.

Gatekeeper will warn or block the first launch. Users who trust the source can Control-click the app in Finder and select **Open**, or select **Open Anyway** in **System Settings → Privacy & Security**. Do not remove quarantine attributes or describe this preview as notarized or generally trusted by Gatekeeper.

The first version that contains updater support and the trusted update public key is a **manual bootstrap release**. Existing versions have neither an updater nor an embedded trust anchor and cannot securely install that version automatically. Users must download its DMG from the GitHub release, verify it, and install it manually. Later updater behavior must still require explicit user confirmation because ad-hoc signing does not remove Gatekeeper limitations.

Before opening a downloaded preview, validate its release files:

```sh
shasum -a 256 -c SHA256SUMS
gh attestation verify CZI-Viewer-<version>-aarch64-apple-darwin-preview.dmg \
  --repo joshwhiteley/czi-viewer
```

The updater verifies the matching `…-update.json.sig`, requires all canonical manifest fields, compares the DMG size and SHA-256, and only then mounts the DMG. `SHA256SUMS` alone is not an independent authenticity mechanism because it is delivered through the same release channel as the artifact.

## GitHub Actions boundary

`.github/workflows/preview.yml` runs manually and for `preview-*` tags. It packages, verifies, attests, and uploads a short-lived workflow artifact. It has read-only repository contents permission and has no release job or contents-write permission. Neither a tag run nor a manual run publishes a GitHub release.

Only the local `scripts/release-preview.sh --publish` flow can create the prerelease after the tagged CI output passes the offline checks and receives an offline signature. The private signing key never enters the repository, GitHub Actions, workflow artifacts, release assets, or command output.
