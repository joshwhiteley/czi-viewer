# CZI implementation provenance

This project implements CZI behavior from public documentation, independently observed fixture structure, and interoperability tests.

## Rules

- Do not translate ZEISS/libCZI line by line.
- Do not copy LGPL or GPL implementation code or tests.
- Do not commit non-redistributable CZI specifications or fixtures.
- Record public references for format decisions that are not demonstrated by project-owned tests.
- Preserve unknown fields and segments rather than inventing semantics.
- Use libCZI, CZICheck, ZEN, and Bio-Formats only as external development-time compatibility oracles.

## Initial public references

- ZEISS CZI overview: <https://www.zeiss.com/microscopy/en/products/software/zeiss-zen/czi-image-file-format.html>
- ZEISS libCZI repository: <https://github.com/ZEISS/libczi>
- `czi-rs` public API and permissively licensed source: <https://github.com/keejkrej/czi-rs>
- OpenSSH protocol documentation: <https://github.com/openssh/openssh-portable/tree/master>
- BaSiC publication: Peng et al., “A BaSiC tool for background and shading correction of optical microscopy images,” *Nature Communications* 8, 14836 (2017), <https://doi.org/10.1038/ncomms14836>

A source review does not imply that source was copied. Implementation commits must cite project tests or the relevant public reference when format behavior is not obvious.

The bundled pixel-only helper in `packaging/basic-helper/helper.py` is project-owned code adapted from the CZI viewer helper developed in the maintainer's `deciphaer-image-segmentation` project. It does not contain libCZI, aicspylibczi, CZI parsing, or source-file access. Its BaSiC fitting behavior uses the separately licensed BaSiCPy package and the bounded protocol documented in [BaSiC preview](basic-preview.md).

## Preview release provenance

GitHub Actions builds each tagged macOS preview from the tag commit, runs the repository package verifier, and issues GitHub build-provenance attestations for the DMG, ZIP, SBOMs, notices, and `SHA256SUMS`. The workflow can upload its short-lived artifact but cannot create a release.

The release operator downloads the unique successful tag-triggered workflow artifact. The local publish script requires its exact file inventory, verifies every GitHub attestation against `joshwhiteley/czi-viewer`, and runs the repository verifier against both archives. It then creates a deterministic manifest containing the version, target, minimum macOS version, bundle identifier, and exact DMG name, size, and SHA-256.

An operator-held Ed25519 private key signs the exact canonical manifest bytes with OpenSSL 3. The signer first requires that private key to match a separately pinned public key, then verifies the detached signature with the pinned key before uploading it. The private key is never available to GitHub Actions and is never published. This separates GitHub's build-provenance statement from the maintainer's local release authorization. The release workstation still uses the network to retrieve attestations and publish assets; key custody is offline from CI and hosted services, not air-gapped during the command.

The signature authenticates the manifest only to clients that already possess the trusted public key. The first updater-capable version must therefore be installed manually with its public key embedded during the normal source build. A public key delivered beside a release cannot authenticate that same release. Ad-hoc Apple code signing remains an integrity seal, not publisher identity, Developer ID signing, or notarization.
