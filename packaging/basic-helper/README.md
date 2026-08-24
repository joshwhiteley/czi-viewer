# Bundled BaSiCPy helper

`helper.py` implements the bounded version-1 pixel-only protocol documented in [`docs/basic-preview.md`](../../docs/basic-preview.md). It is project-owned code adapted from the viewer helper developed in the `deciphaer-image-segmentation` project and is distributed under this repository's `MIT OR Apache-2.0` license.

The release build freezes the helper with Python 3.11, PyInstaller, BaSiCPy 2.0.0, PyTorch 2.2.2, NumPy 1.26.4, SciPy 1.12.0, and scikit-image 0.26.0. `requirements.lock` pins the complete build environment with wheel hashes for Apple Silicon macOS. The frozen helper contains native C, C++, and Fortran libraries and raises the release minimum to macOS 12.3.

Build it with:

```sh
scripts/build-basic-helper.sh /tmp/czi-basic-viewer-helper
```

The build runs a protocol smoke test against the frozen executable with an empty environment. Packaging embeds the resulting directory under `CZI Viewer.app/Contents/Resources/BaSiC/`, ad-hoc signs its Mach-O files with the application, and ships separate Python notices and a CycloneDX SBOM.

The helper receives only app-owned 128 × 128 sample arrays. It never receives a CZI path, SSH profile, remote path, source handle, or credential. Darkfield fitting remains disabled.
