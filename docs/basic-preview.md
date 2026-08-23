# BaSiC preview and helper protocol

The BaSiC feature is a reversible display preview. It never modifies the CZI, writes to an SSH source, exports a corrected CZI/TIFF, or stores a fitted profile after the viewer session.

## Configure the helper

Install a compatible helper separately, then click **Choose helper…** in the BaSiC preview panel. The viewer validates and stores only its canonical absolute executable path in a private, bounded settings file under macOS Application Support. **Clear** removes that choice. You can instead provide an environment override:

```sh
CZI_BASIC_HELPER=/absolute/path/to/basic-helper cargo run --release -p czi-viewer
```

The viewer uses `std::process::Command` directly. It does not use a shell. The child receives the fixed `--request-dir` flag and one value: an app-owned absolute temporary request directory. Its environment is cleared. The child receives no CZI path, SSH profile, credential, remote path, SFTP session, or source handle. The directory is removed after success, failure, or cooperative cancellation.

## Protocol v1

The helper reads `request.json`:

```json
{
  "version": 1,
  "width": 128,
  "height": 128,
  "channels": [
    {
      "id": "channel-0",
      "c_index": 0,
      "name": "Phase",
      "sample_count": 120,
      "pixel_max": 65535,
      "is_phase": true,
      "file": "channel-channel-0.u16le"
    }
  ]
}
```

Each channel file contains exactly `sample_count * 128 * 128` row-major little-endian `u16` values. Gray8 input is widened without changing its 0–255 range.

The helper writes `response.json` and one gain/support pair per channel:

```json
{
  "version": 1,
  "status": "preview-not-held-out-validated",
  "darkfield_enabled": false,
  "channels": [
    {
      "id": "channel-0",
      "method": "BaSiC approximate",
      "version": 1,
      "sample_count": 120,
      "support_fraction": 0.95,
      "gain_range": {"min": 0.7, "max": 1.3},
      "gain_file": "gain-channel-0.f32le",
      "support_file": "support-channel-0.u8"
    }
  ]
}
```

Protocol v1 permits at most 64 channels, 512 samples per channel, 32 MiB (33,554,432 bytes) of aggregate sample data, and 64 KiB for each JSON manifest. All supplied channels use the same sample count, and at most one is marked as Phase. IDs are 1–64 ASCII letters, digits, `_`, or `-`; names are 1–128 characters; C indices are unique non-negative `i32` values; and `pixel_max` is exactly 255 or 65535.

Gain contains exactly `128 * 128` little-endian `f32` values. Support contains exactly `128 * 128` bytes, each 0 or 1. Output paths must be single relative filenames and regular non-symlink files inside the request directory. The viewer rejects a response with a wrong version, validation status, darkfield mode, method, sample count, channel identity/count, size, path, non-finite gain, non-positive or extreme supported gain, invalid support byte, or reported range/fraction that does not match the files. One invalid or missing channel rejects the complete profile set.

The helper must normalize gain so raw division has the intended scale. Darkfield is not accepted or applied in protocol v1.

## Sampling and scheduling

The viewer fits every observed sparse C index within the protocol-v1 64-channel/32-MiB bounds. For each channel it considers only native detector tiles when selecting acquisition positions, so CZI pyramid duplicates do not increase the count. Selection is deterministic, spatial/key ordered, and stratified. It uses the same count for every channel: the smallest available channel count, capped at 512 and reduced as needed by the 32 MiB aggregate bound. When a Phase channel supplies shared masks, positions are aligned across channels by scene, Z/T, detector rectangle, and mosaic index. At least 32 positions are required. The UI warns below 100. Consequently, the 32 MiB aggregate bound prevents preparation of more than 32 channels at the required minimum, even though the manifest can describe up to 64.

For sample decoding, a pyramid tile is used only when its logical rectangle and mosaic index map one-to-one to one native detector tile and its stored dimensions remain at least 128 × 128. Otherwise the native tile is decoded. Every sample is resized to 128 × 128 in detector-tile orientation and written as little-endian `u16`.

Local and Shared SFTP sources use the same read-only dataset worker. Sampling performs one tile step only when no viewer command is waiting. Visible viewport work therefore preempts sampling between tile reads. A single source read or decode cannot be interrupted with the current random-access abstraction; cancellation takes effect after that bounded tile operation. Helper fitting runs on a separate worker and cancellation terminates the helper process.

Detector/pyramid mapping verification is deferred until **Prepare BaSiC Preview**, so opening a dataset has no unconditional BaSiC planning cost. Planning polls the same cancellation token between planes and scales. A single in-memory geometry query or sort is not preemptible and can briefly delay queued viewport work before tile sampling begins. Source and fit generations reject late progress and results after cancel, refit, or source replacement.

## Preview correction

The master toggle defaults to **Off** and is enabled only when every sparse C channel has a structurally valid profile. A composite is never corrected with a partial profile set.

For a stored tile pixel, the viewer maps pixel centers into the 128 × 128 detector profile. Gain uses bilinear interpolation over supported gain cells. Support uses nearest-neighbor lookup. Unsupported pixels remain raw. Supported pixels use:

```text
corrected = round(raw / gain)
```

The result must be finite and is clipped to 0–255 for Gray8 or 0–65535 for Gray16. Correction occurs before display levels, channel color, and compositing.

Native tiles are always valid detector representations. A pyramid level is used during preview only when every tile at that exact sparse plane/level maps one-to-one to the native detector tiles by logical rectangle and mosaic index. Otherwise the preview reload falls back to a verified finer level, ultimately native. This keeps correction in detector-tile coordinates without inventing a sensor mapping.

Toggling clears the rendered texture cache and reloads only the bounded visible/prefetch request. It preserves display levels and field of view. Corrected PNG snapshots always include:

```text
BaSiC preview · darkfield off · not quantitatively validated
```
