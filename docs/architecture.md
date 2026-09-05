# Architecture

CZI Viewer keeps image access sparse and bounded. It does not expose a dense full-frame or full-mosaic operation.

## Components

- **`czi-core`** opens a random-access source, reads the CZI summary directory into a tile-first index, and resolves a subblock only when a caller requests that tile. It decodes only uncompressed Gray8 and Gray16 tiles.
- **Geometry query index** builds a static bounding-volume hierarchy for each exact sparse plane and pyramid scale from metadata only. Queries traverse intersecting bounds and preserve the previous deterministic tile paint order without reading payloads.
- **`czi-app`** requests visible tiles and performs pixel conversion/BaSiC display correction on the dataset worker. A 256 MiB decoded-tile cache (at most 8,192 entries) allows display changes to reuse resident raw pixels. Rendered images in transit have a 64 MiB byte budget. The UI uploads at most four textures per frame, normally within 32 MiB, and maintains a bounded 256 MiB texture cache with at most 4,096 entries. Visible working-set preflight shares both byte and tile-count quotas across active planes. These are buffer-accounting limits, not a promise about total process RSS or driver memory.
- **Progressive display** keeps the prior coarser pyramid level until a requested level is complete. It warms a clamped 12% viewport border. Unrenderable or oversized visible working sets use a suitable coarser representation or report an explicit error. BaSiC never falls back to an unverified detector mapping. Failed or incomplete uploads block export.
- **Desktop state** keeps bounded local preferences in a private, atomically replaced file. Recent history contains local paths only. Menus, keyboard navigation, and the geometry overview are separate from transport and rendering. PNG capture is source/view-generation checked and delayed until menus close; native Save As and atomic writes run on the export thread.
- **Sources** provide local random access and read-only SFTP random access. An opened remote CZI uses a 1 MiB block cache with a 256 MiB budget.
- **`czi-ssh`** owns the embedded OpenSSH/SFTP transport. The optional macOS AnyConnect VPN bridge is isolated in `czi-ssh-darwin`.

## Opening a dataset

The parser uses bounded `ParseOptions` and validates headers, directory counts, dimensions, offsets, and sizes before indexing. Opening reads the directory and optional metadata, but it does not resolve each tile payload. A requested tile then validates its inline subblock descriptor and checks decoded dimensions and byte size before pixel allocation. Core defaults allow at most 256 MiB of decoded pixels and 65,536 pixels per dimension, with configurable `ParseOptions` builders. The app applies stricter renderer and visible-working-set limits before display. Only supported pixel formats are decoded.

The metadata parser is schema-tolerant and bounded. Metadata failure is diagnostic rather than an image-opening failure. It retains a bounded ordered tree and independently extracts high-value image fields.

## Safety and data handling

- Never load a complete large mosaic into memory.
- Never modify a source CZI in place.
- Keep remote sources read-only.
- Preserve unknown CZI data when creating a modified copy.
- Keep the viewer, parser, renderer, network transport, and updater free of bundled C or C++ code; isolate the documented BaSiCPy scientific-runtime exception.

Direct SSH does not launch a shell, parse prompts, automate Terminal, prefill commands, use `SSH_ASKPASS`, or retain credentials, passwords, or one-time codes. OpenSSH stdin and stdout carry binary SFTP packets; authentication output stays on the PTY. Remote paths are SFTP packets, not OpenSSH command-line text. Read [Embedded SSH](embedded-ssh.md) for the full connection and host-key boundary.

The bundled BaSiCPy helper starts on demand by default; Settings can enable preparation after opening. It runs without a shell, with an empty environment, a fixed `--request-dir` flag, and one app-owned temporary directory. It receives downsampled pixels only; it never receives a CZI path, SSH profile, credentials, SFTP session, or remote handle. See [BaSiC preview](basic-preview.md).

Dataset results request immediate UI repaint. A slow housekeeping timer remains for workers without repaint callbacks; active operations and pending uploads use shorter polling as needed.

A separate bounded update worker checks only fixed GitHub release endpoints and persists only the last automatic-check time. It verifies an offline-maintainer-signed manifest before downloading a DMG. Confirmed installation validates the DMG and bundle off the UI thread, stages beside `/Applications/CZI Viewer.app`, and uses a narrowly scoped replacement helper with rollback. An exact private receipt retains the previous bundle until the updated app explicitly acknowledges its first completed UI frame. It never receives dataset or connection state and never disables Gatekeeper.

[AnyConnect VPN mode](anyconnect-vpn.md) has a narrow external-tool boundary. The viewer does not bundle, link, or distribute OpenConnect or ocproxy. [Dependency policy](dependency-policy.md) defines the wider shipped-code and licensing rules.

## Test strategy

Unit and integration tests cover malformed segment data, bounds, lazy payload validation, metadata diagnostics, sparse geometry queries, and random-access caching. Real fixture tests are ignored because their data is local or downloaded separately. The small synthetic demo generator reuses bounded test-builder primitives and has a focused parser/query test; it supplies a safe reproducible demo input without pretending to validate every CZI variant.
