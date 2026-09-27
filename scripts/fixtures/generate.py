#!/usr/bin/env python3
"""Generate independent, non-sensitive media into a new directory, never during tests."""

import argparse
import json
import subprocess
from pathlib import Path

import numpy as np
import PIL
import pillow_heif
import tifffile
from PIL import Image, features


def generate(output: Path, exiftool: str) -> None:
    output.mkdir(parents=True, exist_ok=False)
    y, x = np.indices((32, 48))
    pixels = np.stack(((x * 5) % 256, (y * 7) % 256, ((x + y) * 3) % 256), axis=2)
    image = Image.fromarray(pixels.astype(np.uint8))
    image.save(output / "pattern.jpg", quality=90, subsampling=0)
    image.save(output / "pattern.png")
    image.save(output / "pattern.avif", quality=80, max_threads=1)
    pillow_heif.from_pillow(image).save(output / "pattern.heic", quality=80)
    image.save(output / "metadata.jpg", quality=90, subsampling=0)
    subprocess.run(
        [
            exiftool,
            "-overwrite_original",
            "-DateTimeOriginal=2024:01:02 03:04:05",
            "-OffsetTimeOriginal=+00:00",
            "-Orientation#=6",
            "-XMP:Rating=4",
            "-XMP-dc:Description=Kei synthetic café fixture",
            "-GPSLatitude=0",
            "-GPSLatitudeRef=N",
            "-GPSLongitude=0",
            "-GPSLongitudeRef=E",
            str(output / "metadata.jpg"),
        ],
        check=True,
    )
    for extension in ("mov", "mp4"):
        subprocess.run(
            [
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=48x32:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=mono:sample_rate=8000",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-map_metadata",
                "-1",
                "-fflags",
                "+bitexact",
                str(output / f"pattern.{extension}"),
            ],
            check=True,
        )
    # An independently encoded, uncompressed Bayer DNG. No camera serial or GPS.
    raw = ((np.indices((32, 32)).sum(axis=0) + 1) * 700).astype(np.uint16)
    identity_matrix = tuple(v for n in (1, 0, 0, 0, 1, 0, 0, 0, 1) for v in (n, 1))
    tifffile.imwrite(
        output / "pattern.dng",
        raw,
        photometric=32803,
        metadata=None,
        extratags=[
            (271, "s", 0, "Kei", False),
            (272, "s", 0, "Synthetic Bayer", False),
            (33421, "H", 2, (2, 2), False),
            (33422, "B", 4, (0, 1, 1, 2), False),
            (50706, "B", 4, (1, 4, 0, 0), False),
            (50707, "B", 4, (1, 1, 0, 0), False),
            (50708, "s", 0, "Kei synthetic Bayer fixture", False),
            (50714, "I", 1, 0, False),
            (50717, "I", 1, 65535, False),
            (50721, "2i", 9, identity_matrix, False),
            (50728, "2I", 3, (1, 1, 1, 1, 1, 1), False),
            (50778, "H", 1, 21, False),
        ],
    )
    versions = {
        "pillow": PIL.__version__,
        "pillow_codecs": {
            name: features.version(name) for name in ("avif", "jpg", "zlib")
        },
        "pillow_heif": pillow_heif.__version__,
        "numpy": np.__version__,
        "tifffile": tifffile.__version__,
        "libheif": pillow_heif.libheif_info(),
        "ffmpeg": subprocess.check_output(
            ["ffmpeg", "-version"], text=True
        ).splitlines()[0],
        "exiftool": subprocess.check_output([exiftool, "-ver"], text=True).strip(),
    }
    (output / "encoders.json").write_text(json.dumps(versions, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--exiftool", default="exiftool")
    args = parser.parse_args()
    generate(args.output, args.exiftool)
