# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- Apple Silicon macOS preview packaging for CZI Viewer, including a drag-to-Applications DMG.
- Generic AnyConnect VPN connections with validated gateway and SSH route fields.
- Full-acquisition BaSiC profile fitting with a bundled BaSiCPy helper; on demand by default, with an automatic-preparation setting.
- Daily and manual authenticated preview update checks with confirmed, rollback-capable DMG installation.
- Private recent-local-file history, appearance/interface-scale preferences, view bookmarks, a geometry overview, keyboard shortcuts, and diagnostics.
- Native PNG Save As, clipboard image export, and export annotation controls.
- Per-channel contrast/gamma/reset, bounded raw preview histograms and auto-contrast, and observed-plane Z/T sliders.
- Composite navigation fits the union of active channel bounds, including sparse partial composites.
- Configurable decoded-tile allocation limits, bounded renderer resources, and parser/helper mutation smoke tests.

### Changed

- Use a per-plane/pyramid spatial index for viewport queries.
- Retain bounded decoded tiles for display changes and perform pixel conversion off the UI thread.
- Limit per-frame texture uploads and wake the UI for dataset worker results.
- Stop BaSiC helpers after a five-minute execution deadline.
- Pin CI action revisions and enable platform accessibility through AccessKit.
