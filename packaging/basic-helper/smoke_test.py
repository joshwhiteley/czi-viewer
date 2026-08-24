"""Exercise the frozen BaSiCPy helper through protocol v1."""

from __future__ import annotations

import json
import struct
import subprocess
import sys
import tempfile
from pathlib import Path


def main() -> int:
    helper = Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="czi-basic-smoke-") as temporary:
        request_dir = Path(temporary)
        samples = bytearray()
        for sample in range(8):
            for y in range(128):
                for x in range(128):
                    samples.extend(
                        struct.pack("<H", 1_000 + 2 * x + 3 * y + 20 * sample)
                    )
        (request_dir / "channel-fluor.u16le").write_bytes(samples)
        (request_dir / "request.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "width": 128,
                    "height": 128,
                    "channels": [
                        {
                            "id": "fluor",
                            "c_index": 0,
                            "name": "Green",
                            "sample_count": 8,
                            "pixel_max": 65535,
                            "is_phase": False,
                            "file": "channel-fluor.u16le",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        completed = subprocess.run(
            [str(helper), "--request-dir", str(request_dir)],
            check=False,
            capture_output=True,
            env={},
            timeout=120,
        )
        if completed.returncode != 0:
            sys.stderr.buffer.write(completed.stderr)
            return completed.returncode
        response = json.loads(
            (request_dir / "response.json").read_text(encoding="utf-8")
        )
        assert response["status"] == "preview-not-held-out-validated"
        assert response["channels"][0]["id"] == "fluor"
        assert len((request_dir / "gain-fluor.f32le").read_bytes()) == 128 * 128 * 4
        assert len((request_dir / "support-fluor.u8").read_bytes()) == 128 * 128
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
