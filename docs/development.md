# Development

CZI Viewer is a Rust workspace that requires stable Rust 1.88 or later.

## Check a change

Run these commands from the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p czi-viewer
```

Run the macOS app with `cargo run --release -p czi-viewer`. Pass a local CZI path after `--` to open it on launch.

## Test data

Do not commit private, non-redistributable, or large CZI files. Local fixture paths belong in the ignored `test-data/local-fixtures.toml`; copy [the example](../test-data/local-fixtures.toml.example) and update it for your machine.

The ignored HADA and plate tests require their matching local fixtures and `CZI_RUN_FIXTURES=1`. Public fixtures are downloaded separately and checked by `scripts/fetch-test-data.sh`; they are network-dependent and kept in the ignored `test-data/cache/` directory.

For a small reproducible input, run:

```sh
cargo run -p czi-core --example generate_demo_czi -- test-data/demo.czi
```

The generated CZI is deterministic, synthetic, and ignored. `crates/czi-core/tests/synthetic_demo.rs` verifies its parser and viewport-query shape: three named channels, a 2 × 2 mosaic, and native plus 2:1 coarse levels.

## Project boundaries

The distributed app must remain pure Rust: it does not ship C or C++ code, and source CZI files are never modified in place. Read the [dependency policy](dependency-policy.md) before adding a dependency or external integration.

CZI behavior is implemented from public references, observed fixtures, and interoperability checks. Follow the [provenance rules](provenance.md): do not copy GPL/LGPL implementation code, specifications, tests, or non-redistributable fixtures.

For component boundaries and bounded I/O behavior, see [architecture](architecture.md). Optional feature details belong in their focused documents: [embedded SSH](embedded-ssh.md), [BaSiC preview](basic-preview.md), and [AnyConnect VPN](anyconnect-vpn.md).
