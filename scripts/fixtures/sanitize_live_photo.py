#!/usr/bin/env python3
"""Prepare the pinned flower Live Photo without transcoding its video or images."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

SOURCES = {
    "IMG_1853.HEIC": "8e6fac35b135ca6756ce8fb3368ec4dc3613ea5c0db34aa1eeefd3988e26c625",
    "IMG_1853.mov": "5d24de6ae010d9bdaff559ea8f736923967cbc8e3e382cc0489a5673adec436d",
}
PAIR_ID = "76300000-0000-4000-8000-000000000001"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def packets(path: Path) -> list[dict]:
    result = subprocess.check_output(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_packets",
            "-show_data_hash",
            "sha256",
            "-show_entries",
            "packet=stream_index,pos,size,data_hash,pts,dts,duration",
            "-of",
            "json",
            str(path),
        ]
    )
    return json.loads(result)["packets"]


def decoded(path: Path, *options: str) -> bytes:
    return subprocess.check_output(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-xerror",
            "-i",
            str(path),
            *options,
            "-",
        ]
    )


def sanitize(source: Path, output: Path, exiftool: str) -> None:
    originals = {name: (source / name).read_bytes() for name in SOURCES}
    for name, expected in SOURCES.items():
        if hashlib.sha256(originals[name]).hexdigest() != expected:
            raise ValueError(f"Unexpected original: {name}")
    output.mkdir(parents=True, exist_ok=False)
    still = output / "apple-live.heic"
    movie = output / "apple-live.mov"
    still.write_bytes(originals["IMG_1853.HEIC"])
    media = bytearray(originals["IMG_1853.mov"])
    before = packets(source / "IMG_1853.mov")
    # This pinned file uses little-endian signed 16-bit PCM on stream 1.
    # Zero only its sample bytes. Keep timing, sample tables, and all mebx tracks.
    for packet in before:
        if packet["stream_index"] == 1:
            start, size = int(packet["pos"]), int(packet["size"])
            media[start : start + size] = bytes(size)
    movie.write_bytes(media)
    subprocess.run(
        [
            exiftool,
            "-overwrite_original",
            "-GPS:all=",
            "-Keys:GPSCoordinates=",
            "-Keys:LocationAccuracyHorizontal=",
            "-Apple:PhotoIdentifier=",
            f"-Apple:ContentIdentifier={PAIR_ID}",
            f"-Keys:ContentIdentifier={PAIR_ID}",
            str(still),
            str(movie),
        ],
        check=True,
    )
    after = packets(movie)
    invariant = ("stream_index", "size", "pts", "dts", "duration")
    require(len(before) == len(after), "Packet count changed")
    for original, clean in zip(before, after, strict=True):
        require(
            all(original.get(k) == clean.get(k) for k in invariant),
            "Packet timing or size changed",
        )
        if original["stream_index"] != 1:
            require(
                original["data_hash"] == clean["data_hash"], "Non-audio packet changed"
            )
    audio = decoded(movie, "-map", "0:a:0", "-f", "s16le")
    require(bool(audio) and not any(audio), "Audio must be entirely silent")
    pixel_args = ("-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo")
    original_pixels = decoded(source / "IMG_1853.HEIC", *pixel_args)
    require(original_pixels == decoded(still, *pixel_args), "Still pixels changed")
    metadata = json.loads(
        subprocess.check_output(
            [
                exiftool,
                "-j",
                "-G1",
                "-a",
                "-ee3",
                str(still),
                str(movie),
            ]
        )
    )
    for record in metadata:
        require(
            not any(
                "GPS" in k or "Location" in k or "SerialNumber" in k for k in record
            ),
            "Private location or serial tags remain",
        )
    require(
        metadata[0]["Apple:ContentIdentifier"] == PAIR_ID,
        "Still pairing identifier mismatch",
    )
    require(
        metadata[1]["Keys:ContentIdentifier"] == PAIR_ID,
        "Movie pairing identifier mismatch",
    )
    require("Track5:StillImageTime" in metadata[1], "Still-image timing was lost")
    proof = {
        "sources_sha256": SOURCES,
        "pair_id": PAIR_ID,
        "all_packet_sizes_and_timing_preserved": True,
        "video_and_timed_metadata_packets_identical": True,
        "audio_decodes_to_silence": True,
        "still_pixels_identical": True,
        "still_rgb24_sha256": hashlib.sha256(original_pixels).hexdigest(),
        "gps_location_serial_tags_absent": True,
        "still_image_time_retained": True,
        "exiftool": subprocess.check_output([exiftool, "-ver"], text=True).strip(),
        "ffmpeg": subprocess.check_output(
            ["ffmpeg", "-version"], text=True
        ).splitlines()[0],
    }
    (output / "sanitization.json").write_text(json.dumps(proof, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--exiftool", default="exiftool")
    args = parser.parse_args()
    sanitize(args.source, args.output, args.exiftool)
