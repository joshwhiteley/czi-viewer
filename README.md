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
3. Enter an existing OpenSSH profile or host alias, such as `lab-czi`.
4. Click **Home**, **Browse**, or **Connect**. On macOS, the viewer opens its authentication-only SSH console when interaction is needed.
5. Click that console to focus it, then type the normal password, 2FA, or host-key response. Input is sent directly to the PTY and is not stored, echoed, or interpreted by the viewer. **Cancel** stops a pending authentication. Input disables only after strict SFTP VERSION negotiation succeeds.
6. Choose a directory to enter it, or choose a `.czi` file to fill the remote path. Directory entries end in `/`.

Remote browsing runs in the dataset worker over the same authenticated, read-only SFTP session as opening and reading a CZI. Browsing and range reads serialize on that session, so opening a dataset does not prompt again. The viewer resolves home with `REALPATH('.')`, reads only the requested directory, scans at most 4,096 entries, filters unsafe names, and shows at most 200 sorted directories and CZI files. It wraps an opened CZI in a 1 MiB block cache with a 256 MiB budget, and indexes and decodes only the ranges it needs.

### Explicit Terminal fallback

The primary macOS path is the embedded authentication console. If it cannot start or authenticate, select **Use Terminal fallback** to reveal a copyable interactive SFTP bridge command. Run it in Terminal, then select **Reconnect**, **Home**, or **Browse** in the viewer. Keep that Terminal open while the remote file is in use.

The viewer does not launch a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or retain credentials, passwords, or one-time codes. OpenSSH stdin/stdout carry only binary SFTP packets; authentication output stays on the PTY. Closing the viewer stops its worker sessions and removes its local bridge directory. The viewer never writes to the remote host.

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
