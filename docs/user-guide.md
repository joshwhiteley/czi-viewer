# User guide

CZI Viewer is a macOS-first desktop previewer for local and read-only remote CZI files. It is designed for inspecting large tiled mosaics without loading a full dense image into memory.

## Start the viewer

The source build needs stable Rust 1.88 or later:

```sh
cargo run --release -p czi-viewer
```

You can also pass one local path:

```sh
cargo run --release -p czi-viewer -- /path/to/image.czi
```

The command-line argument is always a local path. In the window, choose **Local**, paste a path, and select **Open**. You can also drag a local CZI from Finder.

## Open, explore, export

1. **Open.** Choose a local CZI, or choose **SSH** to browse a remote source through an existing OpenSSH profile.
2. **Explore.** Pan and zoom the canvas. Use the Dataset panel to select observed Scene, Channel, Z, and T values. Display names use CZI metadata when available. The compact canvas strip shows the source, selected plane, and requested and displayed pyramid scales.
3. **Export.** Select **Save PNG**. The app exports the annotated title strip and canvas, not the side panels or controls. It writes on a background Rust thread to `~/Desktop` when it exists, otherwise the current working directory. Filenames are sanitized, timestamped, and receive a numeric suffix on collision.

The scale bar uses CZI X/Y physical calibration in micrometers when available. Otherwise it reports logical pixels.

## Metadata

The **Metadata** tab starts with a concise overview and channel list. Image-oriented sections are searchable. Vendor details and raw XML are collapsed by default. The viewer independently extracts channels, calibration, acquisition date, and objective information, so these fields can remain available when the ordered XML tree is partial. Unknown elements remain searchable. Malformed or bounded metadata is reported without stopping image opening. Raw XML is retained only within its 2 MiB limit.

## Remote CZI files

Choose **SSH**, keep **Direct SSH**, enter an existing OpenSSH profile or host alias, and select **Connect**. The authentication transcript is not a password field: click it and enter normal OpenSSH password, 2FA, or host-key responses directly to system `ssh`.

One authenticated, read-only SFTP session handles browsing, opening, and range reads. The viewer does not mount a filesystem or download the full CZI. Change the profile or select **Reconnect** to create a session. The browser stays available while a file is open; use **Hide remote browser** for more canvas room.

For authentication behavior, browser safety limits, and host-key handling, see [Embedded SSH](embedded-ssh.md). [AnyConnect VPN mode](anyconnect-vpn.md) is an optional macOS-only route configured with a validated HTTPS gateway and SSH `user@host` destination; it requires separately installed tools and an independently verified host key.

## BaSiC preview

BaSiC is a reversible display preview. It does not modify a CZI, persist a fitted profile, or export a corrected CZI or TIFF. Opening a CZI automatically fits every sparse channel from every supported native acquisition position with the bundled BaSiCPy helper. The toggle remains off until the complete profile set is ready.

The collapsed **Advanced** section can select a custom protocol-compatible helper for testing or specialized deployments. Normal viewing requires no Python installation or helper selection.

The preview reloads only the bounded visible viewport and preserves the display range and field of view. Corrected PNGs state that the result is not quantitatively validated. See [BaSiC preview and helper protocol](basic-preview.md) for limits, packaging, and the protocol.

## Make safe demo data

The repository does not contain real microscopy data. Generate a deterministic, uncompressed Gray16 CZI locally:

```sh
cargo run -p czi-core --example generate_demo_czi -- test-data/demo.czi
cargo run --release -p czi-viewer -- test-data/demo.czi
```

The output is ignored by Git. It contains a 2 × 2 tiled mosaic for the synthetic **Phase**, **Blue**, and **Green** channels, with native and 2:1 coarse levels. The generator refuses to replace an existing output file. It is for demos and parser/query tests, not a general CZI writer.

## Known limits

Only uncompressed Gray8 and Gray16 pixels are currently decoded. Compressed codecs are not supported. CZI Viewer is a previewer, not a quantitatively validated analysis or conversion workflow.
