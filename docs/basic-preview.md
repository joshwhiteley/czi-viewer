# BaSiC preview and helper protocol

The BaSiC feature is a reversible display preview. It never modifies the CZI, writes to an SSH source, exports a corrected CZI/TIFF, or stores a fitted profile after the viewer session.

## On-demand fitting and helper selection

The packaged app includes a frozen BaSiCPy helper. Select **Prepare BaSiC** in the Display inspector to start preparation. Automatic preparation is off by default to avoid background CPU, disk, and remote-network work. Enable **Prepare BaSiC automatically on future opens** in **File → Settings…** if desired.

Preparation fits every sparse channel from every supported acquisition position in that CZI. The profile remains session-only and the display toggle stays off until every channel is ready.

The **Advanced** section can select a compatible custom helper. The viewer validates and stores only its canonical absolute executable path in a private, bounded settings file under macOS Application Support. **Use bundled helper** removes that override. Developers can instead provide an environment override:

```sh
CZI_BASIC_HELPER=/absolute/path/to/basic-helper cargo run --release -p czi-viewer
```

The viewer uses `std::process::Command` directly. It does not use a shell. The bundled or custom child receives the fixed `--request-dir` flag and one value: an app-owned absolute temporary request directory. Its environment is cleared. The child receives no CZI path, SSH profile, credential, remote path, SFTP session, or source handle. The directory is removed after success, failure, or cancellation. The helper has a five-minute execution deadline, separate from tile sampling time. Timeout or cancellation kills and reaps the helper. A timeout reports an error rather than silently accepting an incomplete profile.

Release builds freeze Python 3.11, BaSiCPy 2.0.0, PyTorch 2.2.2, NumPy 1.26.4, SciPy 1.12.0, and scikit-image 0.26.0 from a hash-locked Apple Silicon wheel set. The helper is ad-hoc signed with the app. Separate Python notices and a CycloneDX SBOM are included in each release.

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

Each channel file contains exactly `sample_count * 128 * 128` row-major little-endian `u16` values. Gray8 input is widened without changing its 0–255 range. Whole-CZI requests require the same raw native acquisition count for every channel.

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

Protocol v1 permits at most 64 channels, 512 samples per channel, 32 MiB (33,554,432 bytes) of aggregate sample data, and 64 KiB for each JSON manifest. At most one channel is marked as Phase. IDs are 1–64 ASCII letters, digits, `_`, or `-`; names are 1–128 characters; C indices are unique non-negative `i32` values; and `pixel_max` is exactly 255 or 65535.

Gain contains exactly `128 * 128` little-endian `f32` values. Support contains exactly `128 * 128` bytes, each 0 or 1. Output paths must be single relative filenames and regular non-symlink files inside the request directory. The viewer rejects a response with a wrong version, validation status, darkfield mode, method, sample count, channel identity/count, size, path, non-finite gain, non-positive or extreme supported gain, invalid support byte, or reported range/fraction that does not match the files. One invalid or missing channel rejects the complete profile set.

The helper must normalize gain so raw division has the intended scale. Darkfield is not accepted or applied in protocol v1.

## Sampling and scheduling

The viewer fits every observed sparse C index. For each channel it uses every native acquisition position, so CZI pyramid duplicates do not increase the count and no position is intersected, stratified, capped, subsampled, or silently omitted. All channels must have equal raw native acquisition counts. When a Phase channel supplies shared masks, every channel must have the exact same scene/Z/T/detector rectangle/mosaic acquisition identity set. A count or identity mismatch rejects the plan instead of taking an intersection. The UI warns below 100 positions but does not impose a sampling minimum.

Before Phase alignment checks or any tile payload read, the viewer rejects a whole-CZI plan when any raw channel exceeds 512 positions or the aggregate raw 128 × 128 `u16` samples exceed 32 MiB. The error includes exact per-channel counts, total tile reads, exact sample bytes, and guidance to use offline/cluster profile generation. For example, the HADA 300 × 3 plan retains all 900 tile reads and uses 29,491,200 sample bytes.

For sample decoding, a pyramid tile is used only when its logical rectangle and mosaic index map one-to-one to one native detector tile and its stored dimensions remain at least 128 × 128. Otherwise the native tile is decoded. Every sample is resized to 128 × 128 in detector-tile orientation and written as little-endian `u16`.

Local and Shared SFTP sources use the same read-only dataset worker. Local sampling runs at most four concurrent decode/downsample operations in a batch and caps aggregate staging estimates at 64 MiB. Shared SFTP remains one single-session tile step at a time. Sampling runs only when no viewer command is waiting, so visible viewport work preempts it between bounded local batches or remote tile steps. A batch, source read, or decode cannot be interrupted with the current random-access abstraction; cancellation discards a completed local batch before its samples are written. Helper fitting runs on a separate worker and cancellation terminates the helper process.

The UI reports positions per channel, total tile reads, native/pyramid representation counts, estimated decoded bytes, and whether sampling is waiting for viewport work. Sample files are app-owned temporary data, source access remains read-only, and fitted profiles remain session-only.

Detector/pyramid mapping verification begins when preparation is requested, or after opening when automatic preparation is enabled. Planning polls the same cancellation token between planes and scales. A single in-memory geometry query or sort is not preemptible and can briefly delay queued viewport work before tile sampling begins. Viewport requests preempt sampling between bounded steps. Source and fit generations reject late progress and results after cancel, refit, or source replacement.

## Preview correction

The master toggle defaults to **Off** and is enabled only when every sparse C channel has a structurally valid profile. A composite is never corrected with a partial profile set.

For a stored tile pixel, the viewer maps pixel centers into the 128 × 128 detector profile. Gain uses bilinear interpolation over supported gain cells. Support uses nearest-neighbor lookup. Unsupported pixels remain raw. Supported pixels use:

```text
corrected = round(raw / gain)
```

The result must be finite and is clipped to 0–255 for Gray8 or 0–65535 for Gray16. Correction occurs before display levels, channel color, and compositing.

Native tiles are always valid detector representations. A pyramid level is used during preview only when every tile at that exact sparse plane/level maps one-to-one to the native detector tiles by logical rectangle and mosaic index. Otherwise the preview reload falls back to a verified finer level, ultimately native. This keeps correction in detector-tile coordinates without inventing a sensor mapping.

Toggling rebuilds display textures for the bounded visible/prefetch request. Resident decoded tiles can be reused without rereading the source. It preserves display levels and field of view. Corrected PNG snapshots always include:

```text
BaSiC preview · darkfield off · not quantitatively validated
```
