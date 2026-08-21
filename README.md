# CZI Viewer

A macOS-first, pure-Rust desktop viewer for local and remote ZEISS CZI microscopy images.

The project is under active development. The initial milestones cover safe random-access parsing, tiled viewing, metadata inspection, and read-only SSH/SFTP access.

## Principles

- Never load a complete large mosaic into memory.
- Never modify a source CZI in place.
- Keep remote sources read-only.
- Do not ship C or C++ code.
- Preserve unknown CZI data when creating a modified copy.

## Development

The workspace currently requires stable Rust 1.85 or newer.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Local CZI fixtures belong outside Git. Copy `test-data/local-fixtures.toml.example` to `test-data/local-fixtures.toml` and update the paths for your machine.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
