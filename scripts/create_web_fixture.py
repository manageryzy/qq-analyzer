#!/usr/bin/env python3
"""Create a tiny generated-output fixture for Axum/Playwright integration tests."""

from __future__ import annotations

import binascii
import hashlib
import sqlite3
import struct
import zlib
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
ROOT = REPO / "output" / "_web-fixture-workspace"
ACCOUNT = "10001"
ACCOUNT_OUTPUT = ROOT / "qq-analyzer" / "output" / ACCOUNT
CHAT_DB = ACCOUNT_OUTPUT / "prepared" / "pcqq" / "db" / "Msg3.0.db"
MANIFEST = ACCOUNT_OUTPUT / "image-index" / "manifest.sqlite"
IMAGE_DIR = ROOT / ACCOUNT / "Image" / "fixture"
ASSET_COUNT = 125


def recreate(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()
    return sqlite3.connect(path)


def png_chunk(kind: bytes, data: bytes) -> bytes:
    checksum = binascii.crc32(kind + data) & 0xFFFFFFFF
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", checksum)


def fixture_png(seed: int) -> bytes:
    # Two rows of two RGB pixels, each prefixed by PNG filter type 0.
    rgb = bytes(
        ((seed * factor + offset) % 256)
        for factor, offset in ((17, 23), (29, 71), (43, 11), (61, 37), (73, 89), (97, 13),
                               (31, 53), (47, 19), (59, 101), (83, 7), (103, 41), (113, 67))
    )
    pixels = b"\x00" + rgb[:6] + b"\x00" + rgb[6:]
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress(pixels))
        + png_chunk(b"IEND", b"")
    )


def main() -> None:
    IMAGE_DIR.mkdir(parents=True, exist_ok=True)
    images = []
    for seed in range(1, ASSET_COUNT + 1):
        image = IMAGE_DIR / f"fixture-{seed:03}.png"
        image.write_bytes(fixture_png(seed))
        images.append(image)
    chat = recreate(CHAT_DB)
    chat.execute(
        "create table group_20001(Time integer, Rand integer, SenderUin integer, MsgContent blob, Info blob)"
    )
    chat.executemany(
        "insert into group_20001 values(?,?,?,?,?)",
        [
            (1_700_000_000 + rowid * 86_400, rowid, 10001 if rowid % 2 else 20002, b"", b"")
            for rowid in range(1, 81)
        ],
    )
    chat.commit()
    chat.close()

    manifest = recreate(MANIFEST)
    manifest.executescript(
        """
        create table image_assets(
            id integer primary key autoincrement, path text not null unique,
            source_root text not null default '', file_size integer not null default 0,
            mtime_unix integer not null default 0, sha256_hex text not null default '',
            phash_hex text, phash_algo text not null default '', width integer,
            height integer, blur_score real, blur_algo text not null default '',
            quality_flags text not null default '', source_class text not null default '',
            detected_format text not null default '', has_alpha integer not null default 0,
            orientation_applied integer not null default 0,
            fingerprint_version text not null default '', indexed_at text not null default '',
            stale integer not null default 0, error text
        );
        create table image_asset_occurrences(
            asset_id integer not null,
            conversation_table text not null,
            message_rowid integer not null,
            linked_at text not null default '',
            primary key(asset_id, conversation_table, message_rowid)
        );
        create table image_embeddings(
            path text not null,
            kind text not null,
            model text not null,
            dim integer not null,
            vec blob not null,
            sketch64_hex text not null default '',
            bucket12 integer,
            updated_at text not null default '',
            primary key(path, kind, model)
        );
        """
    )
    manifest.executemany(
        """insert into image_assets(
             path,source_root,file_size,mtime_unix,sha256_hex,phash_hex,phash_algo,
             width,height,blur_score,blur_algo,quality_flags,source_class,
             detected_format,fingerprint_version,indexed_at
           ) values(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        [
            (
                str(image), str(image.parent), image.stat().st_size, int(image.stat().st_mtime),
                hashlib.sha256(image.read_bytes()).hexdigest(), f"{seed:016x}", "fixture-phash",
                2, 2, 1.0, "fixture-blur", "", "fixture", "PNG", "fixture-v1",
                "2026-01-01T00:00:00Z",
            )
            for seed, image in enumerate(images, 1)
        ],
    )
    manifest.executemany(
        "insert into image_asset_occurrences values(?,?,?,?)",
        [
            (ASSET_COUNT, "group_20001", rowid, "2026-01-01T00:00:00Z")
            for rowid in (42, 43, 60)
        ],
    )
    manifest.execute(
        "insert into image_embeddings values(?,?,?,?,?,?,?,?)",
        (
            str(images[-1]),
            "sscd",
            "fixture-sscd-v1",
            4,
            struct.pack("<4f", 1.0, 0.0, 0.0, 0.0),
            "0000000000000001",
            1,
            "2026-01-01T00:00:00Z",
        ),
    )
    manifest.commit()
    manifest.close()
    print(ROOT)


if __name__ == "__main__":
    main()
