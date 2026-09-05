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

## Performance and resource checks

Run the deterministic geometry/open/decode timing example without real microscopy data:

```sh
cargo run --release -p czi-core --example query_benchmark -- 100000 2000
```

The arguments select the synthetic tile count and viewport-query count. The output separates geometry-index construction, repeated queries, and a small parser/open/decode probe. It is not an end-to-end remote-loading or GPU benchmark. Compare the same build profile, machine, and arguments before and after a change. For representative local/SSH datasets, separately measure time to first image, pan/zoom frame stalls, peak memory, and remote bytes read.

Tests include a brute-force differential check of spatial queries, giant virtual tile payloads rejected before pixel reads, and bounded deterministic parser/helper-response mutations. These are regression and mutation smoke tests, not exhaustive fuzzing or a substitute for a long-running fuzz campaign.

CI actions use full commit pins. Run `scripts/check-ci-action-pins.sh` after editing workflows. `cargo deny check` also reports existing upstream dependency-policy issues; see [dependency policy](dependency-policy.md). Do not suppress those findings to make unrelated UI work pass.

## Project boundaries

The viewer, CZI parser, renderer, and SSH transport remain Rust. The release also contains the isolated, reviewed BaSiCPy scientific helper described in the [dependency policy](dependency-policy.md). Source CZI files are never modified in place. Read the policy before adding a dependency or external integration.

CZI behavior is implemented from public references, observed fixtures, and interoperability checks. Follow the [provenance rules](provenance.md): do not copy GPL/LGPL implementation code, specifications, tests, or non-redistributable fixtures.

For component boundaries and bounded I/O behavior, see [architecture](architecture.md). Optional feature details belong in their focused documents: [embedded SSH](embedded-ssh.md), [BaSiC preview](basic-preview.md), and [AnyConnect VPN](anyconnect-vpn.md).
