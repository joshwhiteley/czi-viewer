# macOS preview distribution

The repository builds one distributable product: **CZI Viewer** for Apple Silicon (`aarch64-apple-darwin`). The application bundle identifier is `io.github.joshwhiteley.czi-viewer`. The minimum supported macOS version is 11.0.

## Build a preview

Run this on an Apple Silicon Mac with Xcode command-line tools, Rust 1.88.0, `cargo-about` 0.8.2, `cargo-cyclonedx` 0.5.9, and `jq` installed:

```sh
cargo install cargo-about --version 0.8.2 --locked
cargo install cargo-cyclonedx --version 0.5.9 --locked
scripts/package-macos.sh
scripts/verify-macos-release.sh
```

The package script builds `czi-viewer` with `cargo build --locked --release --target aarch64-apple-darwin`, sets `MACOSX_DEPLOYMENT_TARGET=11.0`, and creates an ad-hoc signed `CZI Viewer.app`. It includes the executable, project licenses, third-party notices, SBOM, and a temporary repository-owned neutral icon. It does not include Python, BaSiCPy, OpenConnect, ocproxy, or SSH.

Outputs are ignored under `dist/`:

- `CZI Viewer.app`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview.dmg`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview.zip`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview-sbom.cdx.json`
- `CZI-Viewer-<version>-aarch64-apple-darwin-preview-THIRD-PARTY-NOTICES.html`
- `SHA256SUMS`

The DMG presents `CZI Viewer.app` next to an Applications shortcut for normal drag-to-Applications installation. The ZIP remains the reproducible archive and the standalone `.app` is available for local testing.

`verify-macos-release.sh` validates both the DMG and ZIP, the plist, exact `arm64` architecture, 11.0 binary deployment target, system-only dynamic libraries, strict ad-hoc code signature, exact bundle contents, archive extraction, and every listed SHA-256 checksum. It also confirms that the embedded SBOM and notices match their checksummed standalone files.

Before extracting a downloaded preview, validate its release files:

```sh
shasum -a 256 -c SHA256SUMS
gh attestation verify CZI-Viewer-<version>-aarch64-apple-darwin-preview.dmg --repo joshwhiteley/czi-viewer
```

The attestation command applies to artifacts produced on GitHub.com. Run it from the directory containing the downloaded release files.

## Preview security posture

These are ad-hoc signed preview builds. They are not Developer ID signed and are not notarized. Gatekeeper will warn or block the first launch. Users who trust the source can Control-click the app in Finder and select **Open**, or select **Open Anyway** in **System Settings → Privacy & Security**.

Do not describe this preview as notarized or generally trusted by Gatekeeper.

## CI releases

`.github/workflows/preview.yml` runs manually and for `preview-*` tags. It uploads the DMG, ZIP, standalone app, checksums, SBOM, and notices as an artifact, and requests GitHub artifact attestations for file artifacts on GitHub.com. Only a `push` of a preview tag creates a prerelease; manual runs never write repository contents.
