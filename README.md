# CZI Viewer

A macOS-first, pure-Rust desktop viewer for local and remote ZEISS CZI microscopy images.

The viewer displays tiled and pyramidal CZI mosaics with sparse C/S/Z/T selection, metadata preview, display levels, pan, and zoom. It supports local files and read-only remote files over the system OpenSSH SFTP subsystem. Uncompressed Gray8 and Gray16 pixels are supported; compressed codecs are not yet supported.

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

The command-line argument is always a local path. In the window, choose **Local**, paste a local CZI path, and click **Open**. You can also drag a local CZI from Finder.

## Open a remote CZI through SSH

The remote viewer uses your normal macOS OpenSSH configuration and profiles from `~/.ssh/config`. It does not mount a remote filesystem or download the whole CZI. See [the embedded SSH guide](docs/embedded-ssh.md) for the full flow.

1. Start the viewer with `cargo run --release -p czi-viewer`.
2. Choose **SSH**. The **Remote files** panel opens on the right.
3. Enter an existing OpenSSH profile or host alias, such as `lab-czi`, then click **Connect**.
4. When the authentication terminal opens, it takes keyboard focus. Click it again to refocus, then type the normal password, 2FA, or host-key response. Typing goes directly to system `ssh`. The viewer does not store, echo, parse, or interpret credentials. There is no password field.
5. After SFTP VERSION succeeds, the authentication terminal hides. Use **Home**, **Up**, **Refresh**, or the editable path and **Go** to browse.
6. Click once to select an entry. Double-click a directory to enter it. Double-click a `.czi` file, or select it and click **Open selected CZI**.

Remote browsing, opening, and range reads use one authenticated, read-only SFTP session. Directory actions and opening a selected CZI do not prompt again. Change the profile or click **Reconnect** to create a new session. The browser remains available while a dataset is open; use **Hide remote browser** in the open bar to give the canvas more room.

The viewer resolves home with `REALPATH('.')`, reads only the requested directory, scans at most 4,096 entries, filters unsafe names, and shows at most 200 entries. It lists directories first, then `.czi` files, with type, size, and modification time when the server supplies them. The filename filter is local and does not send another network request. An opened CZI uses a 1 MiB block cache with a 256 MiB budget, and indexes and decodes only the ranges it needs.

Pyramid viewing follows the Deep Zoom/OpenSeadragon model: it requests the finest available level that is not undersampled, retains the prior coarser level until the requested level is complete, and warms a clamped 12% border around the viewport for the next pan.

The viewer does not launch a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or retain credentials, passwords, or one-time codes. OpenSSH stdin/stdout carry only binary SFTP packets; authentication output stays on the PTY. Closing the viewer stops its worker sessions. The viewer never writes to the remote host.

A real Tufts connection is out of scope until a user provides a remote path and is present to authenticate.

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
