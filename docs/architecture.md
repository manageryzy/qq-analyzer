# QQ Analyzer Architecture

This project is a forensic QQ chat-log analyzer for the local Tencent Files
workspace. Source QQ databases, media files, received files, and QQ installation
binaries are evidence and must stay read-only.

## Direction

The active implementation direction is fully Rust. Python is legacy-only while
the Rust implementation absorbs credential capture, preprocessing/decryption,
parsing, storage lookup, asset resolution, web serving, exports, and validation.

The Rust side currently lives in `rust-msg3-parser/` and has three practical
subsystems plus the new analyzer entrypoint:

- `msg3`: Msg3.0 `MsgContent` and `Info` parsing, rich-node generation, and
  parser coverage/self-test tools.
- `pcqq-storage`: PCQQ InfoStorage decoding, group/contact/member profiles, QQ
  hash helpers, and resource index parsing.
- `qq-web`: local Rust HTTP service, JSON APIs, media/asset serving, and the
  HTML log UI.
- `qq-analyzer-rs`: inventory, credential records, preprocess/catalog
  integration, and Rust-owned credential capture orchestration.
- `capture`: Rust-owned hook script generation, Frida CLI attach loop, event
  JSONL capture, and credential normalization. Frida remains the external
  instrumentation backend.

The current code is not fully split into those crates yet. Until then, use the
names above as ownership boundaries when moving code.

## Runtime Data Flow

1. `qq_analyzer_rs inventory` records evidence paths under `output/<account>/inventory/`.
2. Rust capture generates hook scripts, runs the Frida CLI attach loop when
   requested, and records Frida `send(...)` events as JSONL.
3. Rust credential capture writes normalized JSONL records under
   `output/<account>/credentials/`.
4. `qq_analyzer_rs preprocess` validates prepared databases, InfoStorage roots,
   generated catalog reports, and classic PCQQ DB preparation without modifying
   source evidence.
5. Classic PCQQ DB preprocessing classifies SQLite, encrypted PCQQ SQLite, and
   OLE/CFB containers. Actual copy/strip work is gated behind
   `--prepare-pcqq-dbs`; CFB extraction is gated behind `--extract-cfb`.
   Encrypted SQLite rekey uses a temporary encrypted working copy: try a
   Windows block clone first, fall back to a normal Windows copy if unsupported,
   then delete the encrypted temporary file after the stripped plaintext SQLite
   validates. Encrypted SQLite access is Rust-orchestrated through the cached
   Frida runner against copied DB files. `capture pcqq-query` is the verified
   read-only bridge for QQ's 32-bit `KernelUtil.dll` codec and persisted PCQQ
   keys.
   `capture pcqq-rekey` remains available for copied DBs that accept QQ's rekey
   routine, but Msg3-sized databases may reject rekey and must be read through
   the codec bridge or a future native codec port.
6. The Rust service asks the catalog for the effective Msg3/InfoStorage paths.
   Standard prepared SQLite paths are opened read-only with `rusqlite`.
   Encrypted PCQQ SQLite paths require the codec bridge until the codec is
   ported natively.
7. Conversation metadata is derived from Msg3 tables and InfoStorage where
   available.
8. Each message row reads `MsgContent` and `Info` blobs.
9. `MsgContent` is parsed into:
   - display text,
   - element diagnostics,
   - rich render nodes,
   - legacy HTML fragments.
10. `Info` is parsed for sender/receiver names and nested forwarded-message
   records.
11. Asset resolution annotates rich nodes on demand using protocol paths,
   InfoStorage fields, extracted CFB/RDB resources, and known QQ local paths.
12. The web service returns paged rowid-based JSON and serves matched local
   assets through `/asset/...`.

## Evidence Policy

Every uncertain QQ behavior must be verified against Ghidra before becoming a
parser rule. Record the source DLL/function/address/string and at least one
local sample row when possible.

Formal conclusions live in `docs/`. Raw decompile dumps, Java scripts, ad hoc
JSON output, and one-off probes stay under `output/`.
