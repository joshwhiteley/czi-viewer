# Dependency policy

The primary viewer and CZI/SSH implementation remain Rust. The only bundled native-code exception is the isolated BaSiCPy helper described below. System macOS frameworks and the Rust bindings needed to call them are also allowed.

## Allowed licenses

Dependencies normally require one of these licenses:

- MIT
- Apache-2.0
- BSD-2-Clause or BSD-3-Clause
- ISC
- Zlib
- Unicode-3.0
- CC0-1.0
- OFL-1.1
- Ubuntu-font-1.0

Other permissive licenses require explicit review. GPL, LGPL, AGPL, SSPL, and proprietary runtime dependencies are not allowed unless a focused exception below documents why redistribution is permitted and how notices/source obligations are met.

The optional AnyConnect VPN integration has one narrow external-tool exception. A user may separately install OpenConnect (LGPL-2.1-only) and ocproxy (BSD-3-Clause). The viewer invokes them only as absolute-path executables from a fixed matching Homebrew pair. They are not linked, bundled, copied, or distributed with the viewer and are not part of its Rust dependency graph. This exception does not permit other LGPL dependencies or distributed native code. The VPN path remains external to the shipped application.

The BaSiC preview has a reviewed bundled-helper exception. Release builds freeze Python 3.11, BaSiCPy, PyTorch, NumPy, SciPy, scikit-image, and their pinned transitive dependencies into an isolated Apple Silicon helper. This directory contains native C, C++, and Fortran libraries and is not linked into the Rust viewer. The viewer invokes one frozen executable without a shell and passes only an app-owned temporary directory of downsampled samples. It never passes a source path, remote handle, or credential.

The Python environment is hash-locked, receives a separate CycloneDX SBOM, and includes complete generated license notices. MPL components remain file-scoped. The PyInstaller bootloader is distributed under GPL-2.0 with the upstream bootloader exception that permits distributing the generated executable without applying the GPL to the bundled application; PyInstaller and its build hooks are also reported in the build-environment notices and SBOM. This exception does not permit GPL code without an equivalent reviewed distribution exception elsewhere in the app.

A custom protocol-compatible helper remains an Advanced override for testing or specialized deployments. Its licensing and numerical validation remain the operator's responsibility.

The eframe `default_fonts` feature embeds font assets under OFL-1.1 and Ubuntu-font-1.0. These two font licenses are allowed only for those bundled assets.

The metadata parser uses `quick-xml` 0.41 with default features disabled. It is pure Rust and MIT licensed. It uses the plain reader rather than `NsReader`; namespace declarations are ordinary attributes and are bounded by the parser's aggregate attribute-byte limit. PNG snapshot export uses `png` 0.18 with default features disabled. It is pure Rust and dual MIT/Apache-2.0 licensed. The BaSiC protocol manifest uses `serde` and `serde_json`; both are pure Rust and dual MIT/Apache-2.0 licensed. Its added `itoa` transitive dependency is pure Rust and dual MIT/Apache-2.0 licensed; `zmij` is pure Rust and MIT licensed.

`cargo deny check advisories` currently reports two upstream, transitive, unmaintained crates with no safe upgrade: `paste` (`RUSTSEC-2024-0436`) through `metal`/`wgpu`, and `ttf-parser` (`RUSTSEC-2026-0192`) through `epaint`. Review them when updating the pinned eframe/wgpu stack. The metadata-parser advisories `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` are removed by the quick-xml 0.41 upgrade.

## Review requirements

Before adding a dependency:

1. Check its direct license and repository.
2. Inspect default and enabled features.
3. Inspect normal, build, and transitive dependencies for native code.
4. Record any vendored source or generated bindings.
5. Prefer a smaller pure-Rust dependency when it meets the requirement.
6. Pin and review experimental codecs separately.

Development-only interoperability tools may use libCZI or Bio-Formats outside the shipped dependency graph. Their output can act as an independent test oracle, but their code and test fixtures must not be copied without compatible terms.
