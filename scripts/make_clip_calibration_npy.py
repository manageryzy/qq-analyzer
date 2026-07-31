from __future__ import annotations

import argparse
import json
import sqlite3
import time
from pathlib import Path
from typing import Any

import numpy as np
from PIL import Image


def load_clip_preprocess_config(model_dir: Path) -> dict[str, Any]:
    config_path = model_dir / "open_clip_config.json"
    if not config_path.is_file():
        return {
            "size": 256,
            "mean": [0.0, 0.0, 0.0],
            "std": [1.0, 1.0, 1.0],
            "interpolation": "bilinear",
            "resize_mode": "shortest",
        }
    config = json.loads(config_path.read_text(encoding="utf-8"))
    preprocess = config.get("preprocess_cfg") or {}
    vision = ((config.get("model_cfg") or {}).get("vision_cfg") or {})
    return {
        "size": int(vision.get("image_size") or 256),
        "mean": [float(x) for x in preprocess.get("mean", [0.0, 0.0, 0.0])],
        "std": [float(x) for x in preprocess.get("std", [1.0, 1.0, 1.0])],
        "interpolation": str(preprocess.get("interpolation") or "bilinear"),
        "resize_mode": str(preprocess.get("resize_mode") or "shortest"),
    }


def pil_resample(name: str) -> int:
    if name == "bicubic":
        return Image.Resampling.BICUBIC
    if name == "bilinear":
        return Image.Resampling.BILINEAR
    return Image.Resampling.NEAREST


def windows_path_to_wsl(path: str) -> Path:
    normalized = path
    if normalized.startswith("\\\\?\\"):
        normalized = normalized[4:]
    if len(normalized) >= 3 and normalized[1:3] == ":\\":
        drive = normalized[0].lower()
        rest = normalized[3:].replace("\\", "/")
        return Path(f"/mnt/{drive}/{rest}")
    return Path(normalized)


def manifest_image_paths(manifest: Path, limit: int) -> list[Path]:
    query_limit = max(limit * 4, limit + 256)
    with sqlite3.connect(manifest) as con:
        rows = con.execute(
            """
            select path
            from image_assets
            where stale=0
              and (error is null or error='')
            order by
              case
                when source_class in ('chat_pic', 'file_recv', 'legacy_image', 'image') then 0
                else 1
              end,
              path
            limit ?
            """,
            (query_limit,),
        ).fetchall()
    return [windows_path_to_wsl(str(row[0])) for row in rows]


def preprocess_image(path: Path, cfg: dict[str, Any], dtype: np.dtype[Any]) -> np.ndarray:
    size = int(cfg["size"])
    with Image.open(path) as image:
        image = image.convert("RGB")
        resample = pil_resample(str(cfg["interpolation"]))
        if str(cfg["resize_mode"]) == "squash":
            image = image.resize((size, size), resample=resample)
        else:
            width, height = image.size
            scale = size / min(width, height)
            scaled_width = round(width * scale)
            scaled_height = round(height * scale)
            image = image.resize((scaled_width, scaled_height), resample=resample)
            left = round((scaled_width - size) / 2)
            top = round((scaled_height - size) / 2)
            image = image.crop((left, top, left + size, top + size))
        pixels = np.asarray(image, dtype=np.float32) / 255.0
    mean = np.asarray(cfg["mean"], dtype=np.float32).reshape(1, 1, 3)
    std = np.asarray(cfg["std"], dtype=np.float32).reshape(1, 1, 3)
    pixels = (pixels - mean) / std
    chw = np.transpose(pixels, (2, 0, 1))
    return np.ascontiguousarray(chw.astype(dtype, copy=False))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--model-dir", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--limit", type=int, default=512)
    parser.add_argument("--dtype", choices=("float16", "float32"), default="float16")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    started = time.perf_counter()
    dtype = np.float16 if args.dtype == "float16" else np.float32
    cfg = load_clip_preprocess_config(args.model_dir)
    paths = manifest_image_paths(args.manifest, args.limit)
    samples: list[np.ndarray] = []
    errors: list[dict[str, str]] = []
    attempted = 0
    for path in paths:
        if len(samples) >= args.limit:
            break
        attempted += 1
        try:
            samples.append(preprocess_image(path, cfg, dtype))
        except Exception as exc:
            if len(errors) < 20:
                errors.append({"path": str(path), "error": f"{type(exc).__name__}: {exc}"})
    if not samples:
        raise RuntimeError(f"no images could be decoded from {args.manifest}")
    array = np.stack(samples, axis=0)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    np.save(args.out, array)
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    report = {
        "manifest": str(args.manifest.resolve()),
        "model_dir": str(args.model_dir.resolve()),
        "out": str(args.out.resolve()),
        "shape": list(array.shape),
        "dtype": str(array.dtype),
        "requested_limit": args.limit,
        "candidate_paths": len(paths),
        "attempted_paths": attempted,
        "samples": len(samples),
        "decode_errors": attempted - len(samples),
        "first_errors": errors,
        "elapsed_ms": elapsed_ms,
    }
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
