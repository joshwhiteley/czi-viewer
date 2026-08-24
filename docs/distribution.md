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

Before extracting a downloaded preview, validate its release files:

```sh
shasum -a 256 -c SHA256SUMS
gh attestation verify CZI-Viewer-<version>-aarch64-apple-darwin-preview.dmg --repo joshwhiteley/czi-viewer
```

The attestation command applies to artifacts produced on GitHub.com. Run it from the directory containing the downloaded release files.

## Publish a preview

Update the version in `crates/czi-app/Cargo.toml` and `Cargo.lock`, commit the change to `main`, then run:

```sh
scripts/release-preview.sh --publish
```

The script requires a clean `main` branch. It builds and verifies the app locally, pushes `main`, and pushes an annotated `preview-v<version>` tag. The tag triggers `.github/workflows/preview.yml`, which rebuilds the artifacts on GitHub's Apple Silicon runner and creates the prerelease. A version can be published only once; increment it before the next release.

To build the same release locally without pushing or tagging, omit `--publish`.

## Preview security posture

These are ad-hoc signed preview builds. They are not Developer ID signed and are not notarized. Gatekeeper will warn or block the first launch. Users who trust the source can Control-click the app in Finder and select **Open**, or select **Open Anyway** in **System Settings → Privacy & Security**.

Do not describe this preview as notarized or generally trusted by Gatekeeper.

## CI releases

`.github/workflows/preview.yml` runs manually and for `preview-*` tags. Its build artifact includes the DMG, ZIP, standalone app, checksums, SBOM, and notices. GitHub prereleases publish the distributable files: DMG, ZIP, checksums, SBOM, and notices. The workflow requests GitHub artifact attestations for file artifacts on GitHub.com. Only a `push` of a preview tag creates a prerelease; manual runs never write repository contents.
