# Dependency policy

The distributed application must not contain C or C++ code. System macOS frameworks and the Rust bindings needed to call them are allowed, but vendored or dynamically linked C/C++ libraries are not.

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

Other permissive licenses require explicit review. GPL, LGPL, AGPL, SSPL, and proprietary runtime dependencies are not allowed.

The eframe `default_fonts` feature embeds font assets under OFL-1.1 and Ubuntu-font-1.0. These two font licenses are allowed only for those bundled assets.

The metadata parser uses `quick-xml` 0.41 with default features disabled. It is pure Rust and MIT licensed. It uses the plain reader rather than `NsReader`; namespace declarations are ordinary attributes and are bounded by the parser's aggregate attribute-byte limit. PNG snapshot export uses `png` 0.18 with default features disabled. It is pure Rust and dual MIT/Apache-2.0 licensed.

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
