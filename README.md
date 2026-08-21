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

The remote viewer uses your normal macOS OpenSSH configuration and profiles from `~/.ssh/config`. It does not mount a remote filesystem or download the whole CZI.

1. Start the viewer with `cargo run --release -p czi-viewer`.
2. Choose **SSH** in the open bar.
3. Enter an existing OpenSSH profile or host alias, such as `lab-czi`. This is only an example; do not enter a password or one-time code in the viewer.
4. Click **Home** to discover your remote home directory, or type an absolute directory or partial path and click **Browse**.
5. Choose a directory to enter it, or choose a `.czi` file to fill the remote path. Directory entries end in `/`.
6. Click **Connect**.

Remote browsing runs in the dataset worker over the same read-only SFTP configuration as opening a CZI. It resolves home with `REALPATH('.')`, reads only the requested directory, scans at most 4,096 entries, filters unsafe names, and shows at most 200 sorted directories and CZI files. The viewer validates the selected profile and path in its dataset worker, wraps an opened CZI in a 1 MiB block cache with a 256 MiB budget, and indexes and decodes only the ranges it needs.

### Authenticate in Terminal when needed

The viewer first tries direct batch SFTP for existing key-based OpenSSH setups. It uses `BatchMode=yes`, `StrictHostKeyChecking=yes`, `NumberOfPasswordPrompts=0`, a 15-second connection timeout, and SSH keepalives. It never uses `ControlMaster`, so hosts that reject multiplexed session channels remain supported.

If password, 2FA, or host-key interaction is needed, the SSH form shows the sanitized underlying error and a selectable, copyable interactive SFTP bridge command for the same profile and private socket.

1. Copy the command with **Copy command**.
2. Open Terminal yourself and paste/run it.
3. The bridge prints that it is waiting. Return to the viewer and click **Retry**, **Home**, or **Browse**.
4. Complete any normal password, 2FA, or host-key confirmation prompt in Terminal.
5. Keep that Terminal occupied and open while browsing or viewing the remote CZI.

The bridge starts only `/usr/bin/ssh` with normal interactive authentication and `StrictHostKeyChecking=ask`. SSH stdin/stdout carry only binary SFTP packets; prompts and bridge instructions stay on the controlling Terminal/stderr. When the viewer closes its SFTP stream, the bridge terminates its SSH child and removes its socket. Closing Terminal also ends the remote session.

The viewer does not launch a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or store credentials, passwords, or one-time codes. One private Unix-socket directory is created lazily under `/tmp` for each Unix viewer session. Its compact socket path is limited to 80 bytes. Closing the viewer stops its worker sessions and removes that local directory. The viewer never writes to the remote host.

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
