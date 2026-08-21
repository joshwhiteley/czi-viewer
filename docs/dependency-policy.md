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

## Review requirements

Before adding a dependency:

1. Check its direct license and repository.
2. Inspect default and enabled features.
3. Inspect normal, build, and transitive dependencies for native code.
4. Record any vendored source or generated bindings.
5. Prefer a smaller pure-Rust dependency when it meets the requirement.
6. Pin and review experimental codecs separately.

Development-only interoperability tools may use libCZI or Bio-Formats outside the shipped dependency graph. Their output can act as an independent test oracle, but their code and test fixtures must not be copied without compatible terms.
