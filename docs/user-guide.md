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
3. **Export.** Select **Save PNG** or **File → Export PNG…** (Cmd+S). The app captures the canvas, then opens a native Save As dialog. Choose a `.png` filename and destination. Encoding and atomic file writing run off the UI thread. Cancellation leaves existing files unchanged. **File → Copy Canvas** (Cmd+Shift+C) copies the image instead. **Reveal Last Export in Finder** locates a saved PNG.

The export excludes inspectors, menus, and the overview map. **Settings → Export** controls the title, channel legend, and scale bar. Corrected BaSiC exports always retain their validation annotations.

The scale bar uses CZI X/Y physical calibration in micrometers when available. Otherwise it reports logical pixels.

## Channel display

Black, white, and gamma are independent for each sparse channel and remain set while switching planes in the open dataset. In Composite mode, **Display channel** selects which channel to adjust without changing the field of view. **Reset channel display** restores that channel's defaults.

The raw preview histogram uses at most 64 available decoded visible tiles per plane and at most 65,536 sampled pixels per tile. It can include pixels outside the exact viewport and omit raw tiles evicted from the cache. It never reads an extra tile solely for histogram statistics. **Auto Contrast (visible preview)** applies an approximate 1st–99th percentile range to the selected display channel. These are raw, bounded preview statistics, not whole-image or quantitative results; BaSiC correction is not included in the histogram.

Z/T controls provide both native-value selection and a slider through observed planes. Missing values are never synthesized.

## Desktop controls

- **File → Open Recent** lists up to 12 successfully opened local files. Settings can disable history or clear it. Remote paths and credentials are never saved in this history.
- **File → Settings…** (Cmd+,) selects system/light/dark appearance, interface scale, background preparation, export annotations, and history policy. Window size and inspector/overview visibility are remembered. If saved preferences cannot be read, recent-file recording and automatic writes stay disabled until you explicitly select **Reset Saved Preferences** in Settings.
- **View → Inspector** hides the side inspector for more canvas space. **Diagnostics…** shows cache information and the last error, with a copy-details action.
- **F** fits the image. **1** selects one logical image pixel per UI point (not per physical Retina pixel). With the canvas hovered or focused, arrow keys pan and **+ / −** zoom. Wheel and pinch zoom at the cursor. Text fields and authentication input retain their keys.
- The geometry-only **Overview** shows dataset bounds and the current viewport. Click or drag in it to navigate without loading an extra image thumbnail.
- **Bookmarks** stores up to 12 views for the current dataset. Opening or closing a dataset clears them. Bookmarks are not persisted.

Menus are currently in the app window, not the macOS menu bar. Finder double-click/open-document integration is not implemented; use the file chooser, drag and drop, or a command-line path. Platform accessibility is enabled through AccessKit; VoiceOver behavior still needs manual validation.

## Metadata

The **Metadata** tab starts with a concise overview and channel list. Image-oriented sections are searchable. Vendor details and raw XML are collapsed by default. The viewer independently extracts channels, calibration, acquisition date, and objective information, so these fields can remain available when the ordered XML tree is partial. Unknown elements remain searchable. Malformed or bounded metadata is reported without stopping image opening. Raw XML is retained only within its 2 MiB limit.

## Remote CZI files

Choose **SSH**, keep **Direct SSH**, enter an existing OpenSSH profile or host alias, and select **Connect**. The authentication transcript is not a password field: click it and enter normal OpenSSH password, 2FA, or host-key responses directly to system `ssh`.

One authenticated, read-only SFTP session handles browsing, opening, and range reads. The viewer does not mount a filesystem or download the full CZI. Change the profile or select **Reconnect** to create a session. The browser stays available while a file is open; use **Hide remote browser** for more canvas room.

For authentication behavior, browser safety limits, and host-key handling, see [Embedded SSH](embedded-ssh.md). [AnyConnect VPN mode](anyconnect-vpn.md) is an optional macOS-only route configured with a validated HTTPS gateway and SSH `user@host` destination; it requires separately installed tools and an independently verified host key.

## BaSiC preview

BaSiC is a reversible display preview. It does not modify a CZI, persist a fitted profile, or export a corrected CZI or TIFF. Select **Prepare BaSiC** to fit every sparse channel from every supported native acquisition position with the bundled BaSiCPy helper. Preparation is on demand by default. Enable automatic preparation for future opens in **File → Settings…**. The toggle remains off until the complete profile set is ready.

The collapsed **Advanced** section can select a custom protocol-compatible helper for testing or specialized deployments. Normal viewing requires no Python installation or helper selection.

The preview reloads only the bounded visible viewport and preserves the display range and field of view. Corrected PNGs state that the result is not quantitatively validated. See [BaSiC preview and helper protocol](basic-preview.md) for limits, packaging, and the protocol.

## Updates

CZI Viewer checks the signed preview releases at most once every 24 hours. An automatic check is silent when the Mac is offline or the installed version is current. Select **Check for Updates…** to check immediately.

When a newer compatible Apple Silicon release is available, the app shows a banner. Select **Download Update…** to review it. Automatic installation is enabled only while CZI Viewer is running from `/Applications/CZI Viewer.app`. Select **Download, Install, and Restart** to confirm. The app then downloads the exact DMG, verifies its maintainer-signed manifest and SHA-256, mounts it read-only, validates the bundle, stages it beside the installed app, and restarts through a rollback-capable helper. The previous bundle is removed only after the updated app completes its first UI frame; if launch never reaches that point, the backup is preserved for recovery. No CZI path, metadata, SSH setting, credential, or user identifier is sent. GitHub still receives the connection IP as it does for any HTTPS request.

Preview builds remain ad-hoc signed and are not notarized. The updater does not remove quarantine or bypass Gatekeeper. macOS can require **Open Anyway** after an update. If the app is not in Applications or the destination is not writable, use the verified GitHub release link and install the DMG manually.

## Make safe demo data

The repository does not contain real microscopy data. Generate a deterministic, uncompressed Gray16 CZI locally:

```sh
cargo run -p czi-core --example generate_demo_czi -- test-data/demo.czi
cargo run --release -p czi-viewer -- test-data/demo.czi
```

The output is ignored by Git. It contains a 2 × 2 tiled mosaic for the synthetic **Phase**, **Blue**, and **Green** channels, with native and 2:1 coarse levels. The generator refuses to replace an existing output file. It is for demos and parser/query tests, not a general CZI writer.

## Known limits

Only uncompressed Gray8 and Gray16 pixels are currently decoded. Compressed codecs are not supported. CZI Viewer is a previewer, not a quantitatively validated analysis or conversion workflow.
