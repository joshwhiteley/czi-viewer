# CZI Viewer

A macOS-first, pure-Rust desktop viewer for local and remote ZEISS CZI microscopy images.

The viewer displays tiled and pyramidal CZI mosaics with sparse C/S/Z/T selection, organized metadata, channel labels, display levels, pan, zoom, scale bars, annotated PNG snapshots, and an optional reversible BaSiC flat-field preview. It supports local files and read-only remote files over the system OpenSSH SFTP subsystem. Uncompressed Gray8 and Gray16 pixels are supported; compressed codecs are not yet supported.

## Principles

- Never load a complete large mosaic into memory.
- Never modify a source CZI in place.
- Keep remote sources read-only.
- Do not ship C or C++ code.
- Preserve unknown CZI data when creating a modified copy.

## Launch

The workspace requires stable Rust 1.85 or newer.

Run the viewer without a dataset:

```sh
cargo run --release -p czi-viewer
```

For a local file, you can also pass a path on the command line:

```sh
cargo run --release -p czi-viewer -- /path/to/image.czi
```

The command-line argument is always a local path. In the window, choose **Local**, click **Choose CZI…**, paste a local CZI path and click **Open**, or drag a local CZI from Finder.

## Open a remote CZI through SSH

The remote viewer uses your normal macOS OpenSSH configuration and profiles from `~/.ssh/config`. It does not mount a remote filesystem or download the whole CZI. See [the embedded SSH guide](docs/embedded-ssh.md) for the full flow.

1. Start the viewer with `cargo run --release -p czi-viewer`.
2. Choose **SSH**. The **Remote files** panel opens on the right.
3. Keep the default **Direct SSH** mode. Enter an existing OpenSSH profile or host alias, such as `lab-czi`, then click **Connect**.
4. When the authentication terminal opens, it takes keyboard focus. Click it again to refocus, then type the normal password, 2FA, or host-key response. Typing goes directly to system `ssh`. The viewer does not store, echo, parse, or interpret credentials. There is no password field.
5. After SFTP VERSION succeeds, the authentication terminal hides. Use **Home**, **Up**, **Refresh**, or the editable path and **Go** to browse.
6. Click once to select an entry. Double-click a directory to enter it. Double-click a `.czi` file, or select it and click **Open selected CZI**.

### Optional Tufts VPN mode

On macOS, **Tufts VPN** mode can connect to the fixed `login-prod.pax.tufts.edu` SSH target without changing system routes. It requires separately installed Homebrew OpenConnect and ocproxy tools and a pre-existing, independently verified `known_hosts` entry for that SSH host. First-use trust is disabled. Enter your VPN username and a separate SSH profile such as `jwhite22@login-prod.pax.tufts.edu`; the two usernames are not assumed to match. Complete the VPN and SSH authentication phases in their terminals. Passwords and Duo responses are never fields and are not stored. See [Tufts VPN setup and security](docs/tufts-vpn.md).

Remote browsing, opening, and range reads use one authenticated, read-only SFTP session. Directory actions and opening a selected CZI do not prompt again. Change the profile or click **Reconnect** to create a new session. The browser remains available while a dataset is open; use **Hide remote browser** in the open bar to give the canvas more room.

The viewer resolves home with `REALPATH('.')`, reads only the requested directory, scans at most 4,096 entries, filters unsafe names, and shows at most 200 entries. It lists directories first, then `.czi` files, with type, size, and modification time when the server supplies them. The filename filter is local and does not send another network request. An opened CZI uses a 1 MiB block cache with a 256 MiB budget, and indexes and decodes only the ranges it needs.

Pyramid viewing follows the Deep Zoom/OpenSeadragon model: it requests the finest available level that is not undersampled, retains the prior coarser level until the requested level is complete, and warms a clamped 12% border around the viewport for the next pan.

## Inspect, annotate, and export

The left **Dataset** panel has **Display** and **Metadata** tabs. Display names each C channel from CZI metadata when available. Metadata starts with a concise overview and channel list, followed by searchable image-oriented sections; vendor details and raw XML stay collapsed by default. High-value fields are extracted with separate strict bounds, so channels, calibration, acquisition date, and objective can remain available when the generic ordered XML tree is partial. Unknown elements remain searchable, and malformed or bounded metadata is reported without blocking image opening. Raw XML is retained only when it fits the 2 MiB metadata raw-XML limit.

The compact strip above the canvas identifies the source file, Scene, named Channel, Z/T, and requested and displayed pyramid scales. The lower-left scale bar uses CZI X/Y physical calibration in micrometers when available; otherwise it reports logical pixels.

### Optional BaSiC preview

BaSiC fitting uses a separately installed helper executable. Click **Choose helper…** once, or set `CZI_BASIC_HELPER` to an absolute override path. The chosen canonical executable path is stored privately under macOS Application Support and can be cleared in the app; profiles remain session-only. Open a local or SSH CZI, then click **Prepare BaSiC Preview** in **Display**. Preparation samples every sparse C channel on the background dataset worker, yields to visible viewport requests between bounded batches, and can be cancelled. **BaSiC Preview** remains **Off** until every channel has a valid profile.

Turning the preview **On** clears the rendered texture cache and reloads the bounded visible viewport without changing the display range or field of view. Correction is flat-field only; unsupported profile pixels remain raw. Corrected PNG snapshots include `BaSiC preview · darkfield off · not quantitatively validated`. The viewer does not export corrected CZI/TIFF files or persist profiles. See [BaSiC preview and helper protocol](docs/basic-preview.md).

Click **Save PNG** in the canvas toolbar to export only the annotated title strip and canvas. Side panels and controls are excluded. The viewer crops the native egui screenshot at the current display scale, then writes and encodes PNG on a background Rust thread. It saves to `~/Desktop` when it exists, otherwise the current working directory. Filenames are sanitized and include a Unix timestamp; collisions receive a numeric suffix.

Direct SSH does not launch a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or retain credentials, passwords, or one-time codes. OpenSSH stdin/stdout carry only binary SFTP packets; authentication output stays on the PTY. Optional Tufts VPN mode gives OpenConnect a fixed, private helper command for ocproxy; no username, password, path, prompt text, or other user input enters that command. Closing the viewer stops its SSH and VPN process groups. The viewer never writes to the remote host.

The optional BaSiC helper is also launched without a shell. It receives only the fixed `--request-dir` flag and one app-owned temporary request-directory value, with an empty environment. It never receives a CZI path, SSH profile, credential, SFTP session, or remote handle. The temporary directory contains only downsampled pixels and is removed after fitting or cancellation.

## Development and tests

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p czi-viewer
```

Local CZI fixtures belong outside Git. Copy `test-data/local-fixtures.toml.example` to `test-data/local-fixtures.toml` and update the paths for your machine. The ignored HADA and plate tests require the corresponding local fixtures and `CZI_RUN_FIXTURES=1`.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
