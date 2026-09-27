"""Sanitize the maintainer-contributed iCloud HEIC without transcoding."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

from sanitize_live_photo import decoded, packets, require

SOURCE_SHA256 = "57b8560b2abd3e49ecfbf73d9eb1a8ca453ab161966ec06124cf46698b6d86c9"


def sanitize(source: Path, output: Path, exiftool: str) -> None:
    original = source.read_bytes()
    require(hashlib.sha256(original).hexdigest() == SOURCE_SHA256, "Unexpected source")
    output.mkdir(parents=True, exist_ok=False)
    target = output / "icloud-still.heic"
    target.write_bytes(original)
    subprocess.run(
        [
            exiftool,
            "-overwrite_original",
            "-GPS:all=",
            "-MakerNotes=",
            "-XMP:all=",
            "-Apple:PhotoIdentifier=",
            "-Apple:ContentIdentifier=",
            "-AllDates=2000:01:01 00:00:00",
            "-OffsetTime=+00:00",
            "-OffsetTimeOriginal=+00:00",
            "-OffsetTimeDigitized=+00:00",
            "-SubSecTimeOriginal=",
            "-SubSecTimeDigitized=",
            str(target),
        ],
        check=True,
    )
    pixels = ("-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo")
    before = decoded(source, *pixels)
    require(before == decoded(target, *pixels), "Decoded pixels changed")
    packet_fields = ("stream_index", "size", "data_hash", "pts", "dts", "duration")
    original_packets = packets(source)
    clean_packets = packets(target)
    require(bool(original_packets), "Source has no image packets")
    require(len(original_packets) == len(clean_packets), "Image packet count changed")
    require(
        all(
            all(a.get(k) == b.get(k) for k in packet_fields)
            for a, b in zip(original_packets, clean_packets, strict=True)
        ),
        "Compressed image packets changed",
    )
    metadata = json.loads(
        subprocess.check_output([exiftool, "-j", "-G1", "-a", "-ee3", str(target)])
    )[0]
    private = (
        "GPS",
        "Location",
        "PhotoIdentifier",
        "ContentIdentifier",
        "SerialNumber",
        "OwnerName",
    )
    require(
        not any(any(word in tag for word in private) for tag in metadata),
        "Private tags remain",
    )
    auxiliary_xmp = (
        "XMP-x:",
        "XMP-semanticSegmentationMatte:",
        "XMP-apdi:",
        "XMP-depthData:",
        "XMP-depthBlurEffect:",
        "XMP-portraitLightingEffect:",
        "XMP-HDRGainMap:",
    )
    require(
        all(
            not tag.startswith("XMP-") or tag.startswith(auxiliary_xmp)
            for tag in metadata
        ),
        "Personal or unknown XMP remains",
    )
    source_metadata = json.loads(
        subprocess.check_output([exiftool, "-j", "-G1", "-a", "-ee3", str(source)])
    )[0]
    require(
        {k: v for k, v in metadata.items() if k.startswith(auxiliary_xmp)}
        == {k: v for k, v in source_metadata.items() if k.startswith(auxiliary_xmp)},
        "Auxiliary XMP changed",
    )
    proof = {
        "source_sha256": SOURCE_SHA256,
        "source": "Downloaded from the maintainer's shared iCloud Photos library with kei on 2026-09-27",
        "permission": "Maintainer confirmed ownership and authorized sanitized samples under MIT on 2026-09-27",
        "privacy_review": "User-selected sunset, trees and yard; no people, documents or visible addresses; GPS removed and capture times replaced",
        "decoded_pixels_identical": True,
        "compressed_image_packets_identical": True,
        "rgb24_sha256": hashlib.sha256(before).hexdigest(),
        "gps_location_identifiers_serial_absent": True,
        "personal_xmp_removed": True,
        "auxiliary_xmp_identical": True,
        "exiftool": subprocess.check_output([exiftool, "-ver"], text=True).strip(),
        "ffmpeg": subprocess.check_output(
            ["ffmpeg", "-version"], text=True
        ).splitlines()[0],
    }
    (output / "icloud-sanitization.json").write_text(json.dumps(proof, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--exiftool", default="exiftool")
    args = parser.parse_args()
    sanitize(args.source, args.output, args.exiftool)
