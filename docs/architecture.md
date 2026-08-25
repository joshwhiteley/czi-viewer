# Architecture

CZI Viewer keeps image access sparse and bounded. It does not expose a dense full-frame or full-mosaic operation.

## Components

- **`czi-core`** opens a random-access source, reads the CZI summary directory into a tile-first index, and resolves a subblock only when a caller requests that tile. It decodes only uncompressed Gray8 and Gray16 tiles.
- **Geometry query index** builds from tile metadata only. It selects exact sparse C/S/Z/T planes and pyramid scales, then returns viewport tile hits without reading payloads.
- **`czi-app`** requests visible tiles, composes them for display, and keeps the prior coarser pyramid level until a requested level is complete. It warms a clamped 12% border around the viewport for the next pan.
- **Sources** provide local random access and read-only SFTP random access. An opened remote CZI uses a 1 MiB block cache with a 256 MiB budget.
- **`czi-ssh`** owns the embedded OpenSSH/SFTP transport. The optional macOS AnyConnect VPN bridge is isolated in `czi-ssh-darwin`.

## Opening a dataset

The parser uses bounded `ParseOptions` and validates headers, directory counts, dimensions, offsets, and sizes before indexing. Opening reads the directory and optional metadata, but it does not resolve each tile payload. A requested tile then validates its inline subblock descriptor, reads its bounded payload, and decodes it only when the format is supported.

The metadata parser is schema-tolerant and bounded. Metadata failure is diagnostic rather than an image-opening failure. It retains a bounded ordered tree and independently extracts high-value image fields.

## Safety and data handling

- Never load a complete large mosaic into memory.
- Never modify a source CZI in place.
- Keep remote sources read-only.
- Preserve unknown CZI data when creating a modified copy.
- Keep the viewer, parser, renderer, network transport, and updater free of bundled C or C++ code; isolate the documented BaSiCPy scientific-runtime exception.

Direct SSH does not launch a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or retain credentials, passwords, or one-time codes. OpenSSH stdin and stdout carry binary SFTP packets; authentication output stays on the PTY. Remote paths are SFTP packets, not OpenSSH command-line text. Read [Embedded SSH](embedded-ssh.md) for the full connection and host-key boundary.

The bundled BaSiCPy helper starts automatically after a CZI opens. It runs without a shell, with an empty environment, a fixed `--request-dir` flag, and one app-owned temporary directory. It receives downsampled pixels only; it never receives a CZI path, SSH profile, credentials, SFTP session, or remote handle. See [BaSiC preview](basic-preview.md).

A separate bounded update worker checks only fixed GitHub release endpoints and persists only the last automatic-check time. It verifies an offline-maintainer-signed manifest before downloading a DMG. Confirmed installation validates the DMG and bundle off the UI thread, stages beside `/Applications/CZI Viewer.app`, and uses a narrowly scoped replacement helper with rollback. An exact private receipt retains the previous bundle until the updated app explicitly acknowledges its first completed UI frame. It never receives dataset or connection state and never disables Gatekeeper.

[AnyConnect VPN mode](anyconnect-vpn.md) has a narrow external-tool boundary. The viewer does not bundle, link, or distribute OpenConnect or ocproxy. [Dependency policy](dependency-policy.md) defines the wider shipped-code and licensing rules.

## Test strategy

Unit and integration tests cover malformed segment data, bounds, lazy payload validation, metadata diagnostics, sparse geometry queries, and random-access caching. Real fixture tests are ignored because their data is local or downloaded separately. The small synthetic demo generator reuses bounded test-builder primitives and has a focused parser/query test; it supplies a safe reproducible demo input without pretending to validate every CZI variant.
