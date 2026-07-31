# Asset Resolution Notes

## Goal

Asset resolution should match local files by protocol evidence, not broad full
disk search. The web service should annotate rich nodes on demand and serve
matched local files through `/asset/...`.

## Known Resource Families

- User image paths such as `UserDataImage:...`.
- Custom faces and CFB-extracted resource databases.
- Group custom head images under extracted `Misc.db` / `MiscHead.db`.
- Classic system faces under `SysFaceResFileSystem:`.
- File-transfer metadata and received-file paths.
- Voice/video/image candidates from Msg3 rich nodes and TXData fields.

## SysFace

Ghidra findings show classic sysface resource data is constructed as:

- `SysFaceResFileSystem:<FaceId>.gif`
- `SysFaceResFileSystem:apng\<FaceId>.png`

For face id `212`, the IM.dll static shortcut table maps to `/qw`.

## Asset Policy

- Prefer exact protocol paths, decoded hashes, and known QQ resource roots.
- Avoid full-account scans when a database/index path is available.
- Do not render unmatched images as fake media boxes. Keep unmatched candidates
  in diagnostics.

## Similar Image Index

The optional Rust image index is built with:

```bash
cargo run --features image-index --bin qq_analyzer_rs -- image-index build --root ../.. --account <uin>
```

Generated state stays under `output/<account>/image-index/` and source media
remains read-only. The current implementation provides:

- `sha256` exact-file matching.
- `phash64` Hamming-distance near-duplicate candidates for re-encoding,
  resizing, and light visual edits. The current main hash is
  `phash_v2_fullframe_triangle32_whitealpha_oriented`, so hashes are compared
  only within the same `phash_algo`.
- `tile_hash_v1_triangle32` derived hashes for partial screenshots, crops, wide
  images, and image patches embedded in a larger canvas.
- `sscd_vec` copy-detection descriptors behind `image-index-sscd`, using a
  local SSCD ONNX export.
- `clip_vec` semantic descriptors behind `image-index-clip`, using a local
  OpenCLIP-compatible ONNX model directory.

Main `phash64` is still an entire-image signal. Partial screenshots and crops
are handled by the tile-hash layer and should later be confirmed with SSCD or
local-region verification when the candidate set is ambiguous. Local tests now
cover an end-to-end crop query through `mode=patch`, with the match reported as
`tile_hash`.

CLIP semantic indexing can be enabled with:

```bash
cargo run --features image-index-clip --bin qq_analyzer_rs -- image-index build \
  --root ../.. --account <uin> --model-dir <model-dir>
```

SSCD copy indexing can be enabled with:

```bash
cargo run --features image-index-sscd --bin qq_analyzer_rs -- image-index build \
  --root ../.. --account <uin> --sscd-model-dir <sscd-dir-or-onnx>
```

The SSCD ONNX model is generated from the official TorchScript release with:

```bash
scripts/export_sscd_onnx.py \
  --input output/_deps/models/sscd/sscd_disc_mixup.torchscript.pt \
  --output output/_deps/models/sscd/sscd_disc_mixup.onnx
```

GPU execution providers are opt-in compile features:

- CUDA: `--features image-index-clip-cuda` and/or `image-index-sscd-cuda`,
  plus `--ep cuda` or `--ep auto`.
- DirectML: `--features image-index-clip-directml` and/or
  `image-index-sscd-directml`, plus `--ep directml` or `--ep auto`.
- CPU: `--features image-index-clip` and/or `image-index-sscd`, plus `--ep cpu`
  or no `--ep`.

Build accepts `--clip-batch-size <n>` to control vision embedding batches. The
default is tuned for the current lightweight MobileCLIP2-S2 path: CPU uses 8,
CUDA uses 8, and DirectML uses 32. When DirectML is compiled in, `--ep auto`
tries DirectML before CUDA because it works across Windows GPUs and does not
require separate cuDNN runtime DLLs.
Manifest staging accepts `--manifest-workers <n>`. A value of 0 uses the local
default: available parallelism capped at 24 workers, while explicit values are
clamped to 32. On the current 16-core/32-thread validation host, 24 workers was
slightly faster than 32 for the full decode/hash/write pipeline. Manifest
SQLite writes use at least 1024 rows per batch, independent of the smaller CLIP
embedding batch defaults.
`--manifest-mode fast` is a metadata-only first pass for quickly discovering a
large image library. It writes path, root, size, mtime, `source_class`, and a
`manifest_pending` quality marker without decoding image bytes or computing
SHA256/pHash/blur. Run the default `--manifest-mode full` later, or run an
embedding stage, to replace pending rows with complete hash and quality data.

Full-library indexing uses a strict two-stage pipeline:

```bash
qq_analyzer_rs image-index build --root <workspace> --account <uin> \
  --pipeline full --stage manifest --asset-root <dir> --out manifest.json
qq_analyzer_rs image-index build --root <workspace> --account <uin> \
  --pipeline full --stage manifest --manifest-mode fast --asset-root <dir> \
  --out manifest-fast.json
qq_analyzer_rs image-index build --root <workspace> --account <uin> \
  --pipeline full --stage embeddings --model-dir <clip-dir> \
  --sscd-model-dir <sscd-dir> --ep cuda --out embeddings.json
```

`--stage all` is the default and runs `manifest` followed by `embeddings`.
In full mode, the manifest stage decodes every supported image once to write
SHA256, pHash, tile hashes, dimensions, `blur_score`, `blur_algo`,
`quality_flags`, and `source_class`.
In fast mode, the manifest stage only discovers files and writes pending rows;
exact, pHash, tile-hash, small/tiny, CLIP, and SSCD coverage are intentionally
reported as missing until a full/backfill pass processes those rows. The
embedding stage then backfills missing CLIP/SSCD rows from the manifest,
prioritizing normal chat/file images before thumbnail-like, small, tiny, or
legacy blurry images, but still treating those lower priority images as
in-scope.
Exact SHA256 duplicates reuse an already generated embedding row where
possible, while every active path still receives its own embedding row before
full coverage is considered complete.

Build reports include `elapsed_ms` so indexing speed can be compared without an
external timer, `backfilled_embeddings` when old rows were upgraded from stored
vector blobs, and `canonicalized_embeddings` when legacy `model:path` rows were
merged into the current logical model key. `stale_files` reports previously
indexed assets that were not seen in a complete rebuild and were marked stale,
so deleted or moved files stop participating in active queries. Stale cleanup is
skipped only when `--limit` leaves at least one discovered image unvisited,
because that build is only a partial scan; if the scan exhausts all roots exactly
at the limit, stale cleanup still runs.
The pHash DCT cosine table is precomputed once per process, and pHash converts
the resized grayscale pixels into a compact numeric buffer before the DCT loops
instead of repeatedly calling pixel accessors. Semantic/copy query scans compute
dot products directly from SQLite vector blobs without allocating a temporary
`Vec<f32>` for every candidate, and score the borrowed blob before materializing
path/hash metadata. Malformed vector blobs are skipped during scoring, and only
candidates that can still enter the bounded top-k buffer pay the extra row
materialization cost. SQLite query paths keep sorted bounded top-k candidate
sets, avoiding an O(N) result vector and full-result sort for each
semantic, copy, or pHash scan. Image `mode=all` result merging also keeps a
sorted bounded top-k set ordered by evidence class first (`exact`, `copy_sscd`,
`near_hash`/`tile_hash`, `semantic_clip`) and score within that class, so
combining exact, pHash, tile-hash, SSCD, and CLIP signals does not grow with
the expanded candidate pool.
`mode=all` executes layers in evidence-priority order and skips lower-priority
layers once the bounded result set is already full of stronger evidence. For
example, if exact duplicate rows fill the requested limit, the query does not
load SSCD or CLIP runtimes and does not report those skipped model layers as
unavailable. In that path the query starts with a SHA256-only fingerprint and
hydrates pHash/size metadata from exact manifest rows when available, so exact
duplicate lookups can avoid decoding the query image entirely.
Build walks image roots as fair streaming directory iterators instead of
collecting every candidate path into memory before indexing. Each root gets a
bounded directory-entry budget before the scanner rotates to the next root, so a
wide hash-fanout `Image` tree cannot block `nt_qq/Pic`, `Video`, or `FileRecv`
progress. It stores seen-path/deduplication state in a SQLite temp table instead
of a full Rust `HashSet`. The per-file
insert uses SQLite statement caching, so limited runs and large trees can start
processing immediately while avoiding repeated prepare overhead and keeping lower
peak memory. Rebuild unchanged checks also use cached SQLite statements for the
asset metadata and required embedding lookups, which reduces overhead when
rescanning an already indexed tree. Batch asset writes, embedding upserts, exact
lookup, pHash scan, and semantic full/bucket scans use the same statement cache
for fixed SQL shapes, reducing repeated prepare work during both indexing and
interactive search. Each successful flush also reuses one timestamp string for
the asset row and any CLIP/SSCD embedding rows in that batch, avoiding repeated
time formatting in the write path. Manifest connections enable a 5 second busy
timeout, WAL journaling when SQLite supports it, `synchronous=NORMAL`, and
in-memory temp storage to reduce writer stalls and repeated rebuild overhead.
JPEG manifest fingerprinting uses `zune-jpeg` directly for the no-embedding
path. It requests `ColorSpace::Luma`, decodes to a grayscale buffer, then uses
full-frame Triangle resize for the 32x32 pHash and tile hashes. Blur is
computed from a separate max-side-256 grayscale input, so it no longer shares
the pHash's 32x32 low-frequency image. Unsupported JPEG variants or files with
non-trivial EXIF orientation fall back to the normal `DynamicImage` path. This
reduces CPU work, allocation, and RGB conversion while keeping the pHash
semantics full-frame; threshold and duplicate-group baselines should still be
treated as versioned calibration.
Exact-only `query-image --mode exact` computes only SHA256 and does not decode
the query image, so byte-level lookup remains fast and can still work for files
that are present in the manifest but no longer decodable as images. Image/text
query limits are normalized to `1..=1000`; semantic, SSCD, and pHash candidate
pools expand from the normalized limit with saturating arithmetic, preventing
oversized requests from overflowing or forcing unbounded result buffers.
Semantic/SSCD and pHash scans keep their bounded top-k buffers sorted while
scanning, so once the buffer is full they can reject non-competitive candidates
by comparing only against the current worst result. Same-path semantic rows are
still deduplicated, and a better legacy/current row is reinserted into the
sorted top-k buffer. The pHash scan reads and parses the stored hash before
materializing path metadata, so rows outside the Hamming threshold, invalid hash
rows, and candidates already worse than the current top-k tail avoid extra path
and SHA allocation.
Embedding rows also store a deterministic `sketch64_hex` and `bucket12`.
The random projection signs used to derive `sketch64_hex` are cached per vector
dimension, so 512-dimensional CLIP/SSCD embeddings do not recompute the same
64 projection masks for every indexed image or metadata backfill row. The sketch
calculation scans each embedding vector once while accumulating all 64
projections, and metadata backfill computes sketches directly from vector blobs
without first allocating a temporary `Vec<f32>`.
Semantic and SSCD query commands default to `--query-strategy exact`, which
scores every matching vector row and preserves recall. `--query-strategy fast`
probes the matching bucket and Hamming-1 neighbor buckets first; if that
shortlist is large enough, only those candidates are scored, otherwise the code
falls back to the exact full scan. Existing manifests are migrated in place, and
the next build refills missing sketch metadata from existing vector blobs before
file scanning. This backfill does not run CLIP/SSCD inference. Build also loads
ONNX runtimes lazily, only when at least one image actually needs a fresh
embedding.
New embeddings store the logical model name in `image_embeddings.model` rather
than the local ONNX path. Query and unchanged checks still accept old
`model:path` rows, so manifests built with relative model paths continue to work
when later queried with absolute paths or after the model directory is moved.
Each build now canonicalizes legacy rows for the currently configured CLIP/SSCD
models before file scanning; if a current row already exists, the newest
`updated_at` row wins. When a canonical embedding is written for a path,
same-path legacy rows for that descriptor are removed, and semantic/copy top-k
selection still deduplicates current and legacy rows by path for older manifests.
If a file can no longer be decoded, the build clears its exact/pHash lookup data
and embeddings; if only embedding generation fails, exact/pHash data is kept but
old CLIP/SSCD embeddings are removed so semantic/copy queries never reuse stale
vectors for a changed file.

`image-index status` exposes an `embeddings` block with total rows, active
rows, CLIP/SSCD row counts, sketch coverage, missing sketch rows, and active
bucket count, plus model-agnostic `active_images_missing_clip` and
`active_images_missing_sscd` coverage counters. The adjacent `quality` block
reports active image count, exact SHA/pHash duplicate groups, and blurry,
small, tiny, or thumbnail-like totals. New blur v2 manifests write `blur_score`
and `blur_algo` but do not set `blurry` before calibration, so `blurry` is
currently a legacy or explicitly calibrated flag. This is the quickest way to
confirm that a manifest has been migrated, that the `fast` query strategy has
enough metadata to work, and that full-library vector coverage has actually
finished.

Explicit GPU provider requests are strict. `--ep cuda` and `--ep directml`
return an error if ONNX Runtime cannot register that provider, instead of
silently falling back to CPU. `model_status.execution_provider` is the requested
provider; `model_status.execution_provider_status` records whether strict
registration is required.

The CLIP model directory must contain `visual.onnx`, `visual.onnx.data`,
`text.onnx`, `text.onnx.data`, `open_clip_config.json`, `model_config.json`,
and `tokenizer.json`. Query commands accept the same `--model-dir`. Without a
complete local model directory, semantic routes return `semantic_descriptor:
clip_vec` as unavailable.

The SSCD model path is resolved from `--sscd-model-dir`,
`QQ_ANALYZER_SSCD_MODEL_DIR`, or
`output/_deps/models/sscd/sscd_disc_mixup.onnx`. The official SSCD README
recommends either resizing the small edge to 288 or resizing to a square tensor;
the Rust implementation uses the efficient `320x320` square tensor path with
ImageNet normalization.

Result consumers must keep `match_kind` separate:

- `exact`: byte-identical files.
- `near_hash`: perceptual-hash candidate, needs threshold review.
- `copy_sscd`: SSCD copy descriptor result.
- `semantic_clip`: CLIP/MobileCLIP content similarity, not evidence of common
  origin.

When `mode=all` returns the same path from multiple signals, the result keeps
the stronger evidence class first: `exact`, then `copy_sscd`, then `near_hash`,
then `semantic_clip`. Scores are only directly comparable inside the same
descriptor family.

### Windows Validation

Windows/MSVC builds were verified with Rust `stable-x86_64-pc-windows-msvc`.
Because Windows Cargo had repeated crates.io schannel handshake failures, a
local vendored dependency source was generated under `output/_deps/cargo-vendor`
and used through `output/_deps/cargo-vendor-config-win.toml`.

The downloaded MobileCLIP2-S2 ONNX model lives under
`output/_deps/models/mobileclip2-s2/`. The official SSCD
`sscd_disc_mixup.torchscript.pt` model was also downloaded under
`output/_deps/models/sscd/` and exported to
`output/_deps/models/sscd/sscd_disc_mixup.onnx`. The export produced 512
dimensional descriptors; ONNX Runtime validation matched TorchScript with max
absolute difference `0.00000028` and L2 norms from `0.99999988` to `1.0`.

CUDA requires the NVIDIA runtime DLLs to be loadable by the Rust process. This
machine already had CUDA Toolkit 12.x DLLs such as `cudart64_12.dll`,
`cublas64_12.dll`, and `cufft64_11.dll`, but was missing `cudnn64_9.dll`. The
Windows cuDNN 9 wheel was downloaded to `output/_deps/pip-wheels-win/` and
extracted to `output/_deps/nvidia-cu12-win/`; CUDA runs were launched with those
`nvidia/*/bin` directories prepended to `PATH`.

Original single-image embedding measurements on Windows:

- CPU CLIP build, 8 images: 3.63 s.
- DirectML CLIP build, 8 images: 6.53 s.
- CUDA CLIP build, 8 images: 8.91 s.
- CPU CLIP build, 100 images: 18.47 s.
- CUDA CLIP build, 100 images: 18.71 s.
- CUDA CLIP build, 500 images: 90.25 s.

After batching embedding, reusing the decoded image for pHash and CLIP, and
writing each batch in one SQLite transaction:

- CPU CLIP build, 100 images, batch 8: 12.86 s.
- CPU CLIP build, 500 images, batch 8: 53.57 s.
- CUDA CLIP build, 100 images, batch 8: 11.21 s.
- CUDA CLIP build, 100 images, batch 32: 13.71 s.
- CUDA CLIP build, 500 images, batch 32: 62.78 s.
- DirectML CLIP build, 100 images, batch 32: 4.81 s.
- DirectML CLIP build, 500 images, batch 32: 15.19 s.

Release Windows measurements after strict EP checks and local cuDNN setup:

- CPU CLIP build, 500 images, batch 8: 44.62 s.
- DirectML CLIP build, 500 images, batch 32: 4.85 s.
- DirectML CLIP build, 5000 images, batch 32: 28.56 s.
- CUDA CLIP build, 500 images, batch 8: 6.45 s.
- CUDA CLIP build, 5000 images, batch 8: 27.04 s.
- DirectML CLIP+SSCD build, 7 E2E fixture images, batch 4: 4.46 s.
- CUDA CLIP+SSCD build, 7 E2E fixture images, batch 4: 2.97 s.
- DirectML SSCD copy query, screenshot fixture: 1.53 s.
- CUDA SSCD copy query, text-overlay fixture: 1.47 s.
- DirectML CLIP+SSCD build, 5000 images, batch 32: 40.46 s.
- CUDA CLIP+SSCD build, 5000 images, batch 8: 41.46 s.
- CUDA CLIP+SSCD fixture build, 500 images: batch 8 = 8.64 s, batch 16 =
  8.36 s, batch 32 = 8.15 s. This small probe was run while unrelated GPU
  users were active, so it is useful for batch-size direction only, not as a
  stable benchmark.
- After batch timestamp reuse, a CUDA CLIP+SSCD smoke run indexed 500 fixture
  images with batch 32, 0 errors, in 8.89 s.
- After single-pass vector sketch calculation and allocation-free sketch
  backfill, a CUDA CLIP+SSCD smoke run indexed 500 fixture images with batch
  32, 0 errors, in 5.857 s. This is a same-machine smoke result, not a
  controlled benchmark.
- A later CUDA verification run indexed 1500 fixture images with CLIP+SSCD,
  batch 16, 0 errors, in 16.023 s while unrelated GUI/video GPU users were
  active.
- The medium-account validation target is a local `<validation-account>`, using
  `Image`, `nt_qq`, `Video`, `FileRecv`, and `MyCollection` as roots. A bounded
  read-only directory profile found `Image` to be a sparse hash-fanout tree,
  `nt_qq` to contain thousands of real images, and `Video` to contain hundreds
  of thumbnails, which exercises the full-library staging and prioritization
  paths.
- Windows-native manifest staging on that target, limited to 1000 discovered
  images across those roots, scanned 1000 files, indexed 998, recorded 2 decode
  errors, and finished in 21.090 s, or 47.42 images/s. The quality block reported
  5 duplicate SHA groups covering 12 files, 12 exact pHash groups covering 26
  files, 7 blurry files, 15 small files, 7 tiny files, and 666 thumbnail-like
  files. The fair scanner split the sample across `Image` 331, `nt_qq` 334,
  `Video` 334, and `MyCollection` 1, while `FileRecv` had no supported image in
  that bounded sample.
- A CUDA embeddings stage on the same Windows manifest first wrote 8 CLIP rows
  plus 8 SSCD rows in 2.787 s, then 64 more image pairs in 6.167 s. A longer
  typeperf-verified CUDA run wrote 256 more image pairs in 14.611 s. After those
  runs, status reported 328 active CLIP rows and 328 active SSCD rows, with 670
  active images still missing each descriptor because the validation was
  intentionally bounded.
- Strict Windows-native full validation on the same account then scanned all
  configured roots with `--limit 1000000`, so the build did not stop at the
  default limit. The manifest stage found 75,288 image records, indexed 75,239
  decodable images, recorded 49 decode errors, and finished in 1,926.320 s, or
  39.08 images/s. Most images came from the sparse `Image` hash-fanout tree:
  68,580 scanned, 68,552 indexed, and 28 decode errors. `nt_qq` contributed
  5,965 scanned, `Video` 742, and `MyCollection` 1.
- After parallel manifest workers, single-pass 32x32 luma sampling, and a
  two-pass pHash DCT, Windows-native release full manifest runs on the same
  validation root produced the following current throughput:

  | workers | scanned | indexed | errors | elapsed | rate |
  | ---: | ---: | ---: | ---: | ---: | ---: |
  | 16 | 75,323 | 75,274 | 49 | 89.414 s | 842.41 images/s |
  | 24 | 75,323 | 75,274 | 49 | 84.141 s | 895.20 images/s |
  | 32 | 75,323 | 75,274 | 49 | 85.578 s | 880.17 images/s |

  These measurements are full manifest runs, not enum-only probes: each image is
  read, decoded, SHA256-hashed, pHash/blur-classified, and batch-written to
  SQLite. The current implementation is therefore roughly 22.9x faster than the
  original 39.08 images/s baseline, but it is not yet a 1,800 images/s full
  manifest pipeline. Reaching that target requires a separate fast manifest
  phase or further decoder/preprocess changes, not just more worker threads.
- The fast/deferred manifest pass on the same Windows-native release binary and
  validation root scanned and indexed 75,323 metadata rows in 27.781 s by report
  time, or 2,711.31 images/s. External wall-clock time was 28.830 s, or
  2,612.66 images/s. This pass recorded 0 decode errors because it does not
  decode image bytes; all 75,323 rows were marked `manifest_pending`, with
  SHA256 and pHash coverage intentionally at 0 until a later full/backfill pass.
- The full manifest quality block reported 670 duplicate SHA groups covering
  1,395 files, 5,194 exact pHash groups covering 11,100 files, 190 blurry files,
  4,294 small files, 1,828 tiny files, and 5,660 thumbnail-like files. Final
  status reported 74,514 exact hashes and 75,239 pHashes.
- Full CUDA CLIP+SSCD coverage was completed on that manifest. The final status
  reported 150,478 active embedding rows: 75,239 CLIP rows and 75,239 SSCD rows,
  with 150,478 sketch rows, 4,094 active buckets, and both
  `active_images_missing_clip` and `active_images_missing_sscd` equal to 0.
  The last full CUDA completion pass wrote 44,427 fresh image embeddings and
  reused 308 exact-SHA duplicate embeddings in 16,314.753 s. Earlier probes and
  interrupted resumable passes had already filled the rest. The observed full
  account embedding rate started near 18-19 images/s on the first batches but
  dropped to roughly 3-4 images/s on the tail, which is consistent with slower
  large/cache images and CPU-side decode/preprocess/I/O becoming dominant.

The cleaned full-account baseline is kept outside Git under
`output/image-index-baseline/<validation-account>/`. The SQLite manifest lives at
`index-db/image-index/manifest.sqlite`, and the matching performance reports and
status JSON snapshots live under `perf/`. This directory is intentionally an
ignored local artifact: use it to compare future optimization runs, but do not
commit generated manifests, model files, GPU DLLs, or progress logs.

The 32 minute full manifest number is a mixed-path measurement, not a pure
directory enumeration measurement. That stage currently includes directory
walking, supported-extension filtering, path canonicalization, file metadata,
full file reads, image decoding, SHA256, pHash, blur/quality classification, and
SQLite writes. Use the manifest micro-benchmark before attributing latency to
hash-fanout directory traversal:

```bash
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode enum_only --limit 1000000 --out enum.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode enum_plus_open0 --limit 1000000 --out open0.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode enum_plus_fileinfo --limit 1000000 --out fileinfo.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode enum_plus_header --header-bytes 4096 \
  --limit 1000000 --out header.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode enum_plus_decode --limit 1000000 --out decode.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode full_manifest_no_sqlite --limit 1000000 \
  --out no-sqlite.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode fingerprint_no_sqlite --limit 1000000 \
  --out fingerprint-no-sqlite.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode sqlite_batch --batch-size 1000 \
  --limit 1000000 --out sqlite.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode decode_profile --limit 1000000 \
  --out decode-profile.json
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode jpeg_luma_decode_profile --limit 1000000 \
  --out jpeg-luma-decode-profile.json
QQ_ANALYZER_TURBOJPEG_DLL=<path-to-turbojpeg.dll> \
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode turbojpeg_decode_profile --limit 1000000 \
  --out turbojpeg-decode-profile.json
QQ_ANALYZER_TURBOJPEG_DLL=<path-to-turbojpeg.dll> \
qq_analyzer_rs image-index bench-manifest --root <workspace> --account <uin> \
  --asset-root <dir> --bench-mode turbojpeg_fingerprint_no_sqlite \
  --limit 1000000 --out turbojpeg-fingerprint-no-sqlite.json
```

The report records `directories_seen`, `files_seen`, `find_first_count`,
`find_next_count`, `create_file_count`, `fileinfo_count`, `read_count`,
`decode_count`, `sqlite_insert_count`, error counters, `bytes_read`, and a
`timing` block with elapsed milliseconds for `find`, `canonicalize`,
`metadata`, `createfile`, `fileinfo`, `read`, `decode`, `hash`,
`phash_quality`, and `sqlite`. On Windows, `enum_backend` is
`win32_find_first_file_ex` and the pure enumeration path uses
`FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)` plus
`FindNextFileW`. Non-Windows builds use the portable `std::fs::read_dir`
fallback, so use Windows-native runs for NTFS conclusions.

`fingerprint_no_sqlite` measures the current no-SQLite manifest fingerprint
path. It reads the full file, computes SHA256, then computes dimensions, pHash,
blur score, and quality flags through the same fast JPEG luma path used by
manifest indexing. In that mode, `elapsed_decode_ms` should be interpreted as
fingerprint decode plus 32x32 pHash/quality work.

`decode_profile` is the format and size attribution pass. It reads and decodes
each supported image once, then emits `decode_profile.by_format`,
`by_format_megapixels`, `by_format_file_size`, and `by_format_animation` rows.
Each row contains count, total bytes, total decoded pixels, total/avg/p50/p95/p99
decode time, p95 bytes, p95 pixels, animated count, and decode error count. The
megapixel buckets are `<0.5MP`, `0.5-2MP`, `2-8MP`, `8-20MP`, and `>20MP`; the
file-size buckets are `<256KB`, `256KB-1MB`, `1-4MB`, `4-16MB`, and `>16MB`.

Windows-native release micro-benchmarks on the same full validation roots
processed 75,288 supported image-extension files across 71,543 directories and
78,381 total files. The corrected explicit-root benchmark results were:

| mode | elapsed | rate | key timing |
| --- | ---: | ---: | --- |
| `enum_only` | 11.759 s | 6,402 files/s | `find` 9.384 s |
| `enum_plus_open0` | 17.199 s | 4,377 files/s | `find` 9.093 s, `createfile` 6.358 s |
| `enum_plus_fileinfo` | 16.944 s | 4,443 files/s | `find` 9.184 s, `createfile` 2.467 s, `fileinfo` 0.181 s |
| `enum_plus_header` | 41.337 s | 1,821 files/s | 303.8 MB read, `read` 7.392 s |
| `enum_plus_decode` | 776.898 s | 96.9 files/s | 22.31 GiB read, `decode` 704.001 s |
| `full_manifest_no_sqlite` | 1,999.574 s | 37.7 files/s | `decode` 688.128 s, `phash_quality` 1,220.525 s |
| `sqlite_batch` | 22.986 s | 3,275 files/s | `sqlite` 0.972 s |

The practical conclusion is that the original 32 minute manifest result is not
directory traversal limited. Pure Win32 enumeration is seconds, and open/file
information/header reads stay below one minute. Full-image decode accounts for
roughly 11-12 minutes, while pHash plus blur/quality work accounts for roughly
20 minutes. SQLite batch write overhead is negligible in this measurement. The
next optimization target is therefore the image CPU pipeline: cheaper dimension
probing before full decode, optional blur deferral, faster pHash/blur
implementation, and slow-file sampling for oversized or pathological images.

A later current-code 10,000-image Windows-native micro-benchmark on the same
validation root showed the post-optimization shape more clearly:

| mode | elapsed | rate | key timing |
| --- | ---: | ---: | --- |
| `enum_plus_header` | 3.046 s | 3,282.99 files/s | 38.5 MiB read, `read` 0.219 s |
| `enum_plus_decode` | 56.577 s | 176.75 files/s | 2.25 GiB read, `decode` 50.597 s |
| `full_manifest_no_sqlite` | 63.135 s | 158.39 files/s | `decode` 52.339 s, `hash` 1.365 s, `phash_quality` 0.329 s |
| `sqlite_batch` | 3.006 s | 3,326.68 files/s | `sqlite` 0.107 s |

On this current path, single-threaded full decode dominates the no-SQLite
pipeline: decode was 52.339 s of the 63.135 s no-SQLite probe, or 82.9%.
Full-file reads were only 1.405 s inside that same probe, and SHA/pHash/quality
work was no longer the primary cost after the manifest optimizations.

After adding direct JPEG luma fingerprinting, a Windows-native release 10,000
image comparison on the same root showed:

| mode | elapsed | rate | key timing |
| --- | ---: | ---: | --- |
| `full_manifest_no_sqlite` | 67.202 s | 148.81 files/s | `read` 1.479 s, `hash` 1.358 s, `decode` 54.844 s, `phash_quality` 0.329 s |
| `fingerprint_no_sqlite` | 52.134 s | 191.81 files/s | `read` 1.343 s, `hash` 1.356 s, `fingerprint/decode` 42.864 s |

This is a 22.4% elapsed-time reduction for the no-SQLite probe and a 28.9%
throughput increase. The fingerprint/decode portion fell by 21.8%, which is
consistent with removing RGB conversion and `DynamicImage` construction for
JPEG manifest fingerprinting while still decoding full-resolution luma pixels.

The same change improved the full Windows-native 24-worker manifest run from
84.141 s / 895.20 images/s to 77.514 s / 971.10 images/s on 75,323 scanned
files, with 75,274 indexed files and 49 decode errors. This is an 8.5%
throughput increase for the complete read/hash/fingerprint/SQLite pipeline. It
does not reach the 1,800 images/s target; getting closer likely needs scaled
IDCT through TurboJPEG/libjpeg-turbo, more aggressive full-decode skipping, or a
separate quality backfill queue.

After switching the production manifest semantics to full-frame pHash,
blur256, and 21 tile hashes per decodable image, the Windows-native full v2
manifest validation used a fresh account `_image_index_1423159039_v2_full` and
the same five explicit roots: `Image`, `nt_qq`, `Video`, `FileRecv`, and
`MyCollection`. It scanned 75,288 supported image-extension files, indexed
75,239 decodable images, recorded 49 decode errors, and finished in 729.432 s,
or 103.20 scanned images/s. The resulting manifest contained 1,580,019 tile
hash rows, exactly 21 tile hashes for each decodable image. Format coverage in
the manifest was 61,067 JPEG, 9,867 PNG, 4,305 GIF, and 49 undecodable/error
rows. This current v2 run is still 2.64x faster than the original 1,926.320 s
strict baseline, but it is much slower than the earlier 77.514 s luma-only full
manifest because the current manifest now computes and writes tile hashes for
patch/crop/screenshot search.

libjpeg-turbo 3.2.0 was installed under
`output/_deps/libjpeg-turbo/vc-x64/` and loaded dynamically through
`QQ_ANALYZER_TURBOJPEG_DLL=...\bin\turbojpeg.dll`; the analyzer does not require
a system-wide install. The benchmark modes are Windows-only for TurboJPEG and
are intentionally separate from the default manifest path. They fail fast if
`QQ_ANALYZER_TURBOJPEG_DLL` is unset, cannot be loaded, or does not expose the
required TurboJPEG 3 API symbols, so a report labeled `turbojpeg_*` should not
silently fall back to the zune baseline because of a missing backend. The
official project
describes libjpeg-turbo as a SIMD-accelerated JPEG codec and says it is commonly
2-6x faster than traditional libjpeg on supported CPUs, but the relevant local
comparison is against the current zune-jpeg luma path, not traditional libjpeg.

Full-account strict decode profile results on 2026-07-06:

| mode | files | elapsed | rate | decode time | errors |
| --- | ---: | ---: | ---: | ---: | ---: |
| `jpeg_luma_decode_profile` | 75,288 | 718.482 s | 104.79 files/s | 634.421 s | 68 |
| `turbojpeg_decode_profile` | 75,288 | 570.432 s | 131.98 files/s | 499.780 s | 106 |

TurboJPEG saved 148.050 s wall time, or 20.6%, and reduced accumulated decode
time by 21.2%. The JPEG bucket specifically went from 576.062 s to 444.544 s,
or a 22.8% reduction:

| JPEG metric | zune luma | TurboJPEG luma |
| --- | ---: | ---: |
| count | 61,101 | 61,101 |
| total decode | 576.062 s | 444.544 s |
| avg | 9.428 ms | 7.276 ms |
| p50 | 3.636 ms | 2.515 ms |
| p95 | 34.429 ms | 27.017 ms |
| p99 | 88.465 ms | 70.468 ms |
| errors | 53 | 91 |

The extra 38 TurboJPEG errors are all in JPEG. Therefore TurboJPEG should not
be made the unconditional default until the failing JPEGs are classified and
the production path records fallback counts. A safe integration shape is
TurboJPEG first, zune/dynamic-image fallback on any TurboJPEG error, and an
explicit report field for `turbojpeg_fallback_count`.

A clean 1,000-image no-SQLite complete fingerprint sample measured the current
full v2 fingerprint chain, including SHA256, full-frame pHash, blur256, and 21
tile hashes per image:

| mode | files | elapsed | rate | fingerprint/decode time | errors |
| --- | ---: | ---: | ---: | ---: | ---: |
| `fingerprint_no_sqlite` | 1,000 | 60.224 s | 16.60 files/s | 58.781 s | 0 |
| `turbojpeg_fingerprint_no_sqlite` | 1,000 | 54.031 s | 18.51 files/s | 52.751 s | 0 |

This sample improved by 10.3% elapsed time. The smaller gain compared with the
strict decode profile is expected because the complete fingerprint path includes
tile/blur/pHash work and because `turbojpeg_fingerprint_no_sqlite` falls back to
the current path on TurboJPEG failures. The present TurboJPEG implementation
still decodes full-resolution luma. The larger potential optimization remains
scaled IDCT luma decode plus full-frame resize, so pHash and blur do not pay the
cost of expanding every source pixel.

The rebuilt manifest's quality counters changed: exact pHash groups were 2,912
groups covering 6,201 files, and blurry files fell to 55. The prior RGB-based
manifest baseline reported 5,194 exact pHash groups covering 11,100 files and
190 blurry files. This is expected because JPEG luma is now taken directly from
the decoder's grayscale output instead of RGB pixels converted back to luma.
Treat pHash Hamming thresholds and blur thresholds as versioned calibration
parameters when comparing old and new manifests.

The full `decode_profile` run on the validation account was run with the
Windows-native release binary against 75,323 supported files. It took 743.696 s
by report time and 744.714 s wall time, processed 75,323 images, recorded 49
decode errors, read 22.31 GiB, and reported 657.947 s inside image decode versus
12.292 s inside file reads. That means decode was 88.5% of the profile's report
time, reads were 1.7%, and the remaining 9.8% was path metadata, canonicalize,
format/animation probing, enumeration, and reporting overhead.

Top format buckets from that full profile:

| format | count | bytes | pixels | decode total | avg | p50 | p95 | p99 | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| JPEG | 61,103 | 13.99 GiB | 89.71 Gpx | 606.310 s | 9.923 ms | 4.122 ms | 34.965 ms | 91.468 ms | 34 |
| PNG | 9,909 | 5.12 GiB | 7.96 Gpx | 48.890 s | 4.934 ms | 1.616 ms | 20.084 ms | 48.711 ms | 13 |
| GIF | 4,310 | 3.20 GiB | 0.45 Gpx | 2.744 s | 0.637 ms | 0.417 ms | 1.880 ms | 4.149 ms | 2 |
| WebP | 1 | 8.1 KiB | 0.00013 Gpx | 0.003 s | 2.931 ms | 2.931 ms | 2.931 ms | 2.931 ms | 0 |

JPEG accounts for roughly 81.1% of files and 92.2% of total decode time, so the
next decoder optimization should target JPEG first. PNG is the second target at
7.4% of decode time. GIF/WebP are not relevant to manifest decode latency in
this corpus; animated GIF counts are high, but the manifest currently decodes
only the still image representation needed for hashing/quality.

Highest-cost megapixel buckets:

| bucket | count | decode total | avg | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| JPEG `2-8MP` | 11,357 | 260.606 s | 22.947 ms | 52.633 ms | 83.889 ms |
| JPEG `0.5-2MP` | 30,285 | 193.653 s | 6.394 ms | 15.737 ms | 21.501 ms |
| JPEG `8-20MP` | 1,322 | 106.764 s | 80.759 ms | 142.148 ms | 205.280 ms |
| JPEG `<0.5MP` | 18,008 | 25.492 s | 1.416 ms | 3.742 ms | 5.134 ms |
| PNG `2-8MP` | 1,083 | 20.462 s | 18.894 ms | 38.831 ms | 57.376 ms |
| JPEG `>20MP` | 97 | 19.795 s | 204.068 ms | 384.788 ms | 1,260.333 ms |

The main cost is not only the extreme long tail. The biggest win is reducing
ordinary JPEG decode work in the `0.5-8MP` range, because that range contributes
454.259 s of decode time. Very large images matter for p95/p99 and memory
pressure, but the total time share is smaller because there are few of them.

Highest-cost file-size buckets:

| bucket | count | decode total | avg | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| JPEG `<256KB` | 50,168 | 255.097 s | 5.085 ms | 16.728 ms | 24.274 ms |
| JPEG `256KB-1MB` | 8,642 | 211.591 s | 24.484 ms | 65.034 ms | 101.041 ms |
| JPEG `1-4MB` | 1,927 | 91.978 s | 47.731 ms | 120.084 ms | 174.730 ms |
| JPEG `4-16MB` | 356 | 44.484 s | 124.954 ms | 242.925 ms | 380.969 ms |
| PNG `1-4MB` | 1,096 | 18.932 s | 17.274 ms | 29.630 ms | 44.465 ms |

This confirms that reading bytes is cheap relative to expanding pixels. Small
JPEG files dominate total time by count, while larger JPEGs create the high
latency tail. The next practical changes are JPEG-specific decode evaluation,
dimension/header probing to skip full decode where possible, and keeping the
full manifest as a worker pipeline tuned around decode concurrency rather than
file I/O.

Process-level GPU verification was done by filtering Windows GPU Engine
counters and `nvidia-smi pmon` by the target `qq_analyzer_rs.exe` PID, not by
whole-card utilization. Current 3000-image CLIP+SSCD release probes confirmed
CUDA PID `20248` with GPU Engine samples up to 37.71% and `nvidia-smi pmon`
compute hits with non-zero SM/memory samples. DirectML PID `65272` produced GPU
Engine samples up to 28.93%; DirectML is verified through Windows GPU Engine
counters rather than `nvidia-smi pmon`. A later CUDA recheck while other GPU
processes were active used analyzer PID `30532`; `nvidia-smi pmon` showed that
same PID as a compute process with SM samples up to 31% and memory samples up to
24%, while Windows GPU Engine counters for the same PID showed 21/21 active 3D
samples, max 53.48%, average 46.85%.
Another CUDA recheck with unrelated active GPU users (`dandanplay`, `mstsc`,
and `WindowsTerminal`) used analyzer PID `91372`; `nvidia-smi pmon` reported the
same PID as compute process `qq_analyzer_rs` with SM samples 13%, 20%, and 28%,
and Windows GPU Engine counters for the same PID reached 60.66%.
The latest CUDA check used analyzer PID `77408`; `nvidia-smi --query-compute-apps`
reported that PID during the run, total GPU memory rose from about 3046 MiB to
about 5502 MiB, and power rose from about 24 W to 130-146 W. After the analyzer
exited, `qq_analyzer_rs.exe` disappeared from the process list while unrelated
desktop, browser, video, emulator, WeChat, and wallpaper processes remained as
GPU users.

The repeatable validation script is `scripts/measure_gpu_typeperf.ps1`; it
starts the analyzer, records the analyzer PID, captures Windows GPU Engine
counters with `typeperf`, and then parses only counters containing that PID. It
can run either the generated fixture benchmark or a real manifest by passing
`-Root`, `-Account`, `-AssetRoot`, `-OutputRoot`, and `-Stage embeddings`.
For long full-library runs, `scripts/run_image_index_with_progress.ps1` wraps
`qq_analyzer_rs.exe`, periodically runs `image-index status`, records baseline
and final coverage, and samples GPU Engine counters for the analyzer PID. Use a
longer `-PollSeconds` on large manifests; frequent status scans become
measurable SQLite/I/O overhead once the embedding table has tens of thousands of
rows.
For quick ad-hoc checks, `scripts/sample-gpu-process.ps1` samples the Windows
process table plus GPU Engine counters for a single process name, which is useful
for separating `qq_analyzer_rs.exe` from unrelated 3D/video/desktop GPU users.
Current CLIP+SSCD release runs on 5000 images showed:

- DirectML PID `77056`: 5000 indexed, 0 errors, 3D Engine max 43.63%, average
  39.47%, 35/35 active samples.
- CUDA PID `73368`: 5000 indexed, 0 errors, 3D Engine max 71.17%, average
  63.83%, 36/36 active samples.
- Real-account CUDA embeddings PID `69284` on `<validation-account>`: 256 images embedded,
  0 errors, 3D Engine max 18.59%, average 10.60%, 10/10 active samples. This
  confirms the analyzer PID used GPU even though unrelated GUI/video/browser GPU
  processes were also present on the card.
- Full-account CUDA completion PID `86680` on `<validation-account>`: the long run
  completed with 0 embedding errors and status coverage at 75,239 CLIP plus
  75,239 SSCD rows. Progress sampling repeatedly captured the analyzer PID on
  GPU Engine `3d`, with observed samples such as 29.91%, 49.70%, 42.38%, and
  34.91% during the tail.

For MobileCLIP2-S2 on this machine, both GPU providers are useful versus CPU.
CUDA is slightly faster in the 5000-image release run once cuDNN is supplied,
while DirectML is simpler to deploy and faster in the 500-image run. The
remaining indexing bottlenecks are image decode, preprocessing, and SQLite
writes.
Indexing and non-exact image queries read each image file into memory once, then
reuse the same bytes for SHA256 and image decoding. Exact-only queries keep the
streaming SHA256 path so they can avoid decoding and avoid loading arbitrary
non-image files into memory.
The long-lived HTTP service now keeps CLIP/SSCD query runtimes cached after the
first image/text query. CLI queries still load models per process invocation,
which is expected for one-shot diagnostics.
SQLite remains a local lightweight backend. For very large collections, Qdrant
Edge/Server or another ANN backend is still the stronger long-term replacement,
but the manifest backend now avoids full-result sorting and offers explicit
bucketed shortlists with `--query-strategy fast` when lower latency is more
important than exact recall.
