"""Bounded BaSiCPy fitting helper for the Rust CZI viewer.

The viewer owns a request directory and passes already-decoded 128x128 samples.
This module deliberately has no CZI input or libCZI dependency path.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import stat
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np
from basicpy import BaSiC
from skimage.morphology import dilation, disk

PROTOCOL_VERSION = 1
WIDTH = HEIGHT = 128
PIXELS_PER_SAMPLE = WIDTH * HEIGHT
MAX_REQUEST_BYTES = 64 * 1024
MAX_RESPONSE_BYTES = 64 * 1024
MAX_CHANNELS = 64
MAX_SAMPLES_PER_CHANNEL = 512
MAX_INPUT_BYTES = 32 * 1024 * 1024
UINT16_MAX = float(np.iinfo(np.uint16).max)
SATURATION_FRACTION = 0.98
SUPPORT_THRESHOLD_FRACTION = 0.25
_NAME = re.compile(r"[A-Za-z0-9_-]{1,64}\Z")


def _robust_location_scale(values: np.ndarray) -> tuple[float, float]:
    location = float(np.median(values))
    scale = 1.4826 * float(np.median(np.abs(values - location)))
    return location, max(scale, float(np.finfo(np.float32).eps))


def _build_references(
    stacks: dict[str, np.ndarray],
    phase_channel: str,
    support: np.ndarray,
    indices: np.ndarray,
) -> dict[str, np.ndarray]:
    phase = stacks[phase_channel]
    normalized_phase = phase / np.median(phase[:, support], axis=1)[:, None, None]
    references = {phase_channel: np.median(normalized_phase[indices], axis=0)}
    for name, stack in stacks.items():
        if name == phase_channel:
            continue
        logged = np.log1p(np.maximum(stack, 0))
        centered = logged - np.median(logged[:, support], axis=1)[:, None, None]
        references[name] = np.median(centered[indices], axis=0)
    return references


def _foreground_masks_for(
    stacks: dict[str, np.ndarray],
    phase_channel: str,
    support: np.ndarray,
    indices: np.ndarray,
    references: dict[str, np.ndarray],
    *,
    mad_multiplier: float,
    dilation_radius: int,
    saturation_fraction: float,
) -> np.ndarray:
    phase = stacks[phase_channel]
    normalized_phase = phase / np.median(phase[:, support], axis=1)[:, None, None]
    centered_fluorescence: dict[str, np.ndarray] = {}
    for name, stack in stacks.items():
        if name == phase_channel:
            continue
        logged = np.log1p(np.maximum(stack, 0))
        centered_fluorescence[name] = (
            logged - np.median(logged[:, support], axis=1)[:, None, None]
        )
    footprint = disk(dilation_radius)
    masks: list[np.ndarray] = []
    threshold = saturation_fraction * UINT16_MAX
    for index in indices:
        residual = normalized_phase[index] - references[phase_channel]
        location, scale = _robust_location_scale(residual[support])
        foreground = np.abs(residual - location) > mad_multiplier * scale
        foreground |= phase[index] >= threshold
        foreground = dilation(foreground, footprint)
        for name, centered in centered_fluorescence.items():
            residual = centered[index] - references[name]
            location, scale = _robust_location_scale(residual[support])
            channel_mask = residual > location + mad_multiplier * scale
            channel_mask |= stacks[name][index] >= threshold
            foreground |= dilation(channel_mask, footprint)
        masks.append(foreground & support)
    return np.stack(masks)


def valid_pixels(
    images: np.ndarray,
    support: np.ndarray,
    *,
    saturation_fraction: float = SATURATION_FRACTION,
) -> np.ndarray:
    images = np.asarray(images)
    _require(images.ndim == 3, f"Expected (tiles, y, x), got {images.shape}")
    _require(support.shape == images.shape[1:], "Support shape does not match images")
    valid = np.broadcast_to(support, images.shape).copy()
    valid &= np.isfinite(images)
    valid &= images < saturation_fraction * UINT16_MAX
    return valid


def sanitize_images(images: np.ndarray, valid: np.ndarray) -> np.ndarray:
    result = np.asarray(images, dtype=np.float32).copy()
    for index in range(result.shape[0]):
        selected = valid[index] & np.isfinite(result[index])
        if np.any(selected):
            fill = float(np.median(result[index][selected]))
        else:
            finite = result[index][np.isfinite(result[index])]
            fill = float(np.median(finite)) if finite.size else 0.0
        result[index][~valid[index] | ~np.isfinite(result[index])] = fill
    return result


def _fit_basic(
    images: np.ndarray,
    weights: np.ndarray,
    working_size: int,
    *,
    fitting_mode: str,
    device: str,
) -> tuple[np.ndarray, dict[str, float | None]]:
    model = BaSiC(
        fitting_mode=fitting_mode,
        get_darkfield=False,
        working_size=working_size,
        device=device,
    )
    model.fit(images, fitting_weight=weights)
    return np.asarray(model.flatfield), {}


def _normalize_flatfield(
    flatfield: np.ndarray, support: np.ndarray, target_mean: float = 1.0
) -> tuple[np.ndarray | None, str | None]:
    values = np.asarray(flatfield, dtype=np.float32)
    support_array = np.asarray(support, dtype=bool)
    if values.ndim != 2 or values.shape != support_array.shape:
        return (
            None,
            f"flatfield shape {values.shape} does not match support {support_array.shape}",
        )
    supported_values = values[support_array]
    if int(supported_values.size) < max(1, values.size // 100):
        return None, "insufficient finite positive detector support"
    if not bool(np.all(np.isfinite(supported_values))):
        return None, "nonfinite flatfield values on detector support"
    if not bool(np.all(supported_values > 0)):
        return None, "nonpositive flatfield values on detector support"
    scale = float(np.mean(supported_values)) / target_mean
    if not np.isfinite(scale) or scale <= 0:
        return None, "flatfield mean is not finite and positive"
    normalized = values / scale
    supported_values = normalized[support_array]
    if not bool(np.all(np.isfinite(supported_values))) or bool(
        np.any(supported_values <= 0)
    ):
        return None, "normalized flatfield is not finite and positive"
    p01, p99 = np.percentile(supported_values, [1, 99])
    if p01 < 0.05 or p99 > 20 or p99 / max(p01, np.finfo(np.float32).eps) > 100:
        return None, "flatfield has an unstable gain range"
    normalized = np.asarray(normalized, dtype=np.float32)
    normalized[~support_array] = 1.0
    return normalized, None


class ProtocolError(ValueError):
    """The viewer request does not satisfy the version-1 protocol."""


@dataclass(frozen=True)
class ChannelRequest:
    id: str
    c_index: int
    name: str
    sample_count: int
    pixel_max: int
    is_phase: bool
    file: str
    samples: np.ndarray


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ProtocolError(message)


def _is_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _safe_request_dir(value: str) -> Path:
    path = Path(value)
    _require(path.is_absolute(), "--request-dir must be an absolute directory")
    try:
        info = path.lstat()
    except FileNotFoundError as error:
        raise ProtocolError("request directory does not exist") from error
    _require(stat.S_ISDIR(info.st_mode), "request directory is not a directory")
    _require(not stat.S_ISLNK(info.st_mode), "request directory must not be a symlink")
    return path


def _open_directory(path: Path) -> int:
    _require(hasattr(os, "O_NOFOLLOW"), "safe symlink protection is unavailable")
    flags = os.O_RDONLY | os.O_NOFOLLOW
    if hasattr(os, "O_DIRECTORY"):
        flags |= os.O_DIRECTORY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    try:
        return os.open(path, flags)
    except OSError as error:
        raise ProtocolError("cannot open request directory safely") from error


def _read_child(directory_fd: int, name: str, limit: int) -> bytes:
    _require(
        hasattr(os, "O_NONBLOCK"), "safe nonblocking input protection is unavailable"
    )
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    try:
        fd = os.open(name, flags, dir_fd=directory_fd)
    except OSError as error:
        raise ProtocolError(f"cannot open {name}") from error
    with os.fdopen(fd, "rb") as handle:
        info = os.fstat(handle.fileno())
        _require(
            stat.S_ISREG(info.st_mode) and info.st_nlink == 1,
            f"{name} is not a single-link regular file",
        )
        _require(info.st_size <= limit, f"{name} exceeds its size limit")
        data = handle.read(limit + 1)
    _require(len(data) <= limit, f"{name} exceeds its size limit")
    return data


def _write_child(directory_fd: int, name: str, data: bytes) -> None:
    _require(len(data) <= MAX_RESPONSE_BYTES, f"{name} exceeds its size limit")
    # Request directories are single-use. O_EXCL avoids following a hard link
    # or replacing a previous response, and makes response.json the commit marker.
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    try:
        fd = os.open(name, flags, 0o600, dir_fd=directory_fd)
    except OSError as error:
        raise ProtocolError(f"cannot write {name}") from error
    with os.fdopen(fd, "wb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())


def _object(value: object, label: str) -> dict[str, Any]:
    _require(isinstance(value, dict), f"{label} must be an object")
    return value


def _exact_keys(value: dict[str, Any], expected: set[str], label: str) -> None:
    _require(set(value) == expected, f"{label} has unsupported or missing fields")


def _read_request(directory_fd: int) -> dict[str, Any]:
    data = _read_child(directory_fd, "request.json", MAX_REQUEST_BYTES)

    def no_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ProtocolError("request.json contains a duplicate field")
            result[key] = value
        return result

    try:
        parsed = json.loads(data, object_pairs_hook=no_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProtocolError("request.json is not valid JSON") from error
    request = _object(parsed, "request")
    _exact_keys(request, {"version", "width", "height", "channels"}, "request")
    _require(
        _is_int(request["version"]) and request["version"] == PROTOCOL_VERSION,
        "unsupported request version",
    )
    _require(
        _is_int(request["width"])
        and _is_int(request["height"])
        and request["width"] == WIDTH
        and request["height"] == HEIGHT,
        "request dimensions must be 128x128",
    )
    _require(isinstance(request["channels"], list), "channels must be an array")
    _require(
        0 < len(request["channels"]) <= MAX_CHANNELS, "channel count is out of bounds"
    )
    return request


def _validate_channel_descriptors(request: dict[str, Any]) -> None:
    """Check channel metadata and aggregate bounds before reading any array."""
    _require(
        0 < len(request["channels"]) <= MAX_CHANNELS, "channel count is out of bounds"
    )
    ids: set[str] = set()
    c_indices: set[int] = set()
    total_bytes = 0
    expected_keys = {
        "id",
        "c_index",
        "name",
        "sample_count",
        "pixel_max",
        "is_phase",
        "file",
    }
    for index, raw_channel in enumerate(request["channels"]):
        channel = _object(raw_channel, f"channels[{index}]")
        _exact_keys(channel, expected_keys, f"channels[{index}]")
        identifier = channel["id"]
        _require(
            isinstance(identifier, str) and bool(_NAME.fullmatch(identifier)),
            f"channels[{index}].id is invalid",
        )
        _require(identifier not in ids, "channel ids must be unique")
        ids.add(identifier)
        _require(
            _is_int(channel["c_index"]) and 0 <= channel["c_index"] <= 2**31 - 1,
            f"channels[{index}].c_index is invalid",
        )
        _require(channel["c_index"] not in c_indices, "c_index values must be unique")
        c_indices.add(channel["c_index"])
        _require(
            isinstance(channel["name"], str) and 0 < len(channel["name"]) <= 128,
            f"channels[{index}].name is invalid",
        )
        count = channel["sample_count"]
        _require(
            _is_int(count) and 0 < count <= MAX_SAMPLES_PER_CHANNEL,
            f"channels[{index}].sample_count is out of bounds",
        )
        pixel_max = channel["pixel_max"]
        _require(
            _is_int(pixel_max) and pixel_max in (255, int(UINT16_MAX)),
            f"channels[{index}].pixel_max must be 255 or 65535",
        )
        _require(
            isinstance(channel["is_phase"], bool),
            f"channels[{index}].is_phase is invalid",
        )
        expected_file = f"channel-{identifier}.u16le"
        _require(
            channel["file"] == expected_file,
            f"channels[{index}].file must be {expected_file}",
        )
        expected_bytes = count * PIXELS_PER_SAMPLE * np.dtype("<u2").itemsize
        total_bytes += expected_bytes
        _require(
            total_bytes <= MAX_INPUT_BYTES, "input arrays exceed the total size limit"
        )


def _load_channels(directory_fd: int, request: dict[str, Any]) -> list[ChannelRequest]:
    _validate_channel_descriptors(request)
    channels: list[ChannelRequest] = []
    for raw_channel in request["channels"]:
        channel = _object(raw_channel, "channel")
        identifier = channel["id"]
        count = channel["sample_count"]
        expected_file = channel["file"]
        expected_bytes = count * PIXELS_PER_SAMPLE * np.dtype("<u2").itemsize
        data = _read_child(directory_fd, expected_file, expected_bytes)
        _require(len(data) == expected_bytes, f"{expected_file} has an unexpected size")
        raw_samples = np.frombuffer(data, dtype="<u2").reshape((count, HEIGHT, WIDTH))
        _require(
            int(raw_samples.max()) <= channel["pixel_max"],
            f"{expected_file} exceeds pixel_max",
        )
        samples = np.asarray(raw_samples, dtype=np.float32) * (
            float(UINT16_MAX) / channel["pixel_max"]
        )
        channels.append(
            ChannelRequest(
                id=identifier,
                c_index=channel["c_index"],
                name=channel["name"],
                sample_count=count,
                pixel_max=channel["pixel_max"],
                is_phase=channel["is_phase"],
                file=expected_file,
                samples=samples,
            )
        )
    return channels


def _shared_support(
    channels: list[ChannelRequest],
) -> tuple[np.ndarray, ChannelRequest | None]:
    phase = [channel for channel in channels if channel.is_phase]
    _require(len(phase) <= 1, "at most one channel may be marked is_phase")
    if not phase:
        return np.ones((HEIGHT, WIDTH), dtype=bool), None
    # Match cache support derivation from detector-aligned Phase samples, while
    # also dropping positions saturated in every supplied observation.
    median_phase = np.median(phase[0].samples, axis=0)
    center = median_phase[HEIGHT // 4 : 3 * HEIGHT // 4, WIDTH // 4 : 3 * WIDTH // 4]
    center_median = float(np.median(center))
    _require(
        np.isfinite(center_median) and center_median > 0,
        "Phase samples have a nonpositive center median",
    )
    support = median_phase > SUPPORT_THRESHOLD_FRACTION * center_median
    support &= np.any(phase[0].samples < SATURATION_FRACTION * UINT16_MAX, axis=0)
    _require(
        int(np.sum(support)) >= PIXELS_PER_SAMPLE // 100,
        "Phase samples provide insufficient detector support",
    )
    phase_medians = np.median(phase[0].samples[:, support], axis=1)
    _require(
        np.all(np.isfinite(phase_medians) & (phase_medians > 0)),
        "Phase samples have a nonpositive support median",
    )
    return support, phase[0]


def _phase_masks(
    channels: list[ChannelRequest], phase: ChannelRequest | None, support: np.ndarray
) -> np.ndarray | None:
    if phase is None:
        return None
    stacks = {
        channel.id: np.asarray(channel.samples, dtype=np.float32)
        for channel in channels
    }
    indices = np.arange(phase.sample_count)
    references = _build_references(stacks, phase.id, support, indices)
    return _foreground_masks_for(
        stacks,
        phase.id,
        support,
        indices,
        references,
        mad_multiplier=4.0,
        dilation_radius=2,
        saturation_fraction=SATURATION_FRACTION,
    )


def _fit_channels(
    channels: list[ChannelRequest],
) -> list[tuple[ChannelRequest, np.ndarray, np.ndarray]]:
    _require(
        len({channel.sample_count for channel in channels}) == 1,
        "all channels must have the same sample_count",
    )
    support, phase = _shared_support(channels)
    masks = _phase_masks(channels, phase, support)
    fitted: list[tuple[ChannelRequest, np.ndarray, np.ndarray]] = []
    for channel in channels:
        images = np.asarray(channel.samples, dtype=np.float32)
        if channel.is_phase:
            assert masks is not None
            weights = np.broadcast_to(support, images.shape).copy()
            weights &= ~masks
        else:
            weights = valid_pixels(
                images, support, saturation_fraction=SATURATION_FRACTION
            )
        fit_support = support & np.any(weights, axis=0)
        _require(np.any(fit_support), f"channel {channel.id} has no fitting support")
        _require(np.any(weights), f"channel {channel.id} has no fitting pixels")
        profile, _details = _fit_basic(
            sanitize_images(images, weights),
            weights.astype(np.float32),
            WIDTH,
            fitting_mode="approximate",
            device="cpu",
        )
        raw_profile = np.asarray(profile)
        _require(
            raw_profile.shape == (HEIGHT, WIDTH),
            f"channel {channel.id} returned a wrong profile shape",
        )
        # The shared normalizer validates all supported values and replaces
        # unsupported positions with one, so emitted profiles stay finite.
        normalized, reason = _normalize_flatfield(raw_profile, fit_support, 1.0)
        _require(
            normalized is not None,
            f"channel {channel.id} returned an invalid profile: {reason}",
        )
        fitted.append((channel, normalized, fit_support))
    return fitted


def _response(
    fitted: list[tuple[ChannelRequest, np.ndarray, np.ndarray]],
) -> tuple[dict[str, Any], list[tuple[str, bytes]]]:
    channels: list[dict[str, Any]] = []
    files: list[tuple[str, bytes]] = []
    for channel, gain, support in fitted:
        values = gain[support]
        gain_file = f"gain-{channel.id}.f32le"
        support_file = f"support-{channel.id}.u8"
        files.extend(
            [
                (gain_file, np.asarray(gain, dtype="<f4").tobytes(order="C")),
                (support_file, np.asarray(support, dtype=np.uint8).tobytes(order="C")),
            ]
        )
        channels.append(
            {
                "id": channel.id,
                "method": "BaSiC approximate",
                "version": PROTOCOL_VERSION,
                "sample_count": channel.sample_count,
                "support_fraction": float(np.mean(support)),
                "gain_range": {
                    "min": float(np.min(values)),
                    "max": float(np.max(values)),
                },
                "gain_file": gain_file,
                "support_file": support_file,
            }
        )
    return {
        "version": PROTOCOL_VERSION,
        "status": "preview-not-held-out-validated",
        "darkfield_enabled": False,
        "channels": channels,
    }, files


def run(request_dir: Path) -> None:
    """Fit all requested profiles and write the version-1 response in place."""
    directory = _safe_request_dir(str(request_dir))
    directory_fd = _open_directory(directory)
    try:
        request = _read_request(directory_fd)
        channels = _load_channels(directory_fd, request)
        response, files = _response(_fit_channels(channels))
        encoded_response = json.dumps(
            response, allow_nan=False, separators=(",", ":")
        ).encode("utf-8")
        _require(
            len(encoded_response) <= MAX_RESPONSE_BYTES,
            "response.json exceeds its size limit",
        )
        for name, data in files:
            _write_child(directory_fd, name, data)
        _write_child(directory_fd, "response.json", encoded_response)
    finally:
        os.close(directory_fd)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Fit bounded viewer samples with BaSiCPy."
    )
    parser.add_argument(
        "--request-dir", required=True, help="absolute app-owned request directory"
    )
    args = parser.parse_args(argv)
    try:
        run(Path(args.request_dir))
    except Exception as error:  # noqa: BLE001 - command must keep BaSiCPy failures bounded
        message = str(error).replace("\n", " ")[:512]
        print(f"czi-basic-viewer-helper: {message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
