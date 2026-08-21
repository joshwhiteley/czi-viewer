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
4. Enter the absolute remote CZI path, such as `/absolute/path/image.czi`.
5. Click **Connect**.

The viewer validates the profile and path in its dataset worker, opens the file as read-only SFTP, wraps it in a 1 MiB block cache with a 256 MiB budget, and indexes and decodes only the ranges it needs.

### Authenticate in Terminal when needed

GUI SFTP children never prompt. They run with `BatchMode=yes`, `StrictHostKeyChecking=yes`, `NumberOfPasswordPrompts=0`, a 15-second connection timeout, and SSH keepalives. This prevents an invisible GUI process from accepting a host key or asking for credentials.

If a remote open fails, the SSH form shows the sanitized underlying error and a selectable, copyable bootstrap command for the same profile and private control socket.

1. Copy the command with **Copy command**.
2. Open Terminal yourself.
3. Paste and run the command in Terminal.
4. Complete any normal password, 2FA, or host-key confirmation prompt there.
5. Leave the Terminal master command running.
6. Return to the viewer and click **Retry**.

The Terminal master uses normal interactive OpenSSH behavior with `StrictHostKeyChecking=ask`. The viewer does not run a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or store credentials, passwords, or one-time codes.

One private OpenSSH control-path directory is created lazily for each viewer session. Close the Terminal master when you are done. Closing the viewer stops its worker sessions and removes that local control directory. The viewer never writes to the remote host.

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
