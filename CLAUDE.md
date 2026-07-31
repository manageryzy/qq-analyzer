# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Rust-first local forensic toolchain for recovering and analyzing Windows QQ (PCQQ Msg3.0) chat data: credential capture via Frida, database preprocessing/decryption, rich-message parsing, InfoStorage metadata lookup, local asset resolution, and a browser-based log viewer.

Python is legacy-only. All new analyzer behavior goes in Rust.

## Build, Test, Format

```bash
cd rust-msg3-parser
cargo build --workspace
cargo test --workspace --bins --lib
cargo fmt --package msg3_richtext_parser_rs
cargo fmt --package msg3_richtext_parser_rs -- --check  # CI check
cargo check --bins                # fast type-check after structural edits
```

CI (`.github/workflows/ci.yml`) checks and tests the Vite frontend, builds its
embedded bundle, runs Rust formatting/checks/tests with `web-ui,image-index`,
and then runs the Playwright browser integration suite.

## Architecture

The Rust crate (`rust-msg3-parser/`) is a single Cargo workspace member. Source is organized by subsystem under `src/`:

- **`msg3`** — `MsgContent` blob parser: text, system faces, images, file/audio, video, miniprograms, forwarded messages, rich-format context. Generates display text, element diagnostics, rich render nodes, and HTML. Entry: `src/msg3_parser.rs` plus `msg3_index.rs`, `msg3_samples.rs`.
- **`pcqq-storage`** — InfoStorage decoder for group/contact/member profiles, labels, avatars, resource indexes. Also QQ-tolerant CFB/OLE stream reader (`src/cfb.rs`), TXData codec (`src/txdata_codec.rs`), QQ hash helpers (`src/qq_hash.rs`).
- **`qq-web`** — Axum HTTP service at `127.0.0.1:8765` with JSON APIs (`/api/conversations`, `/api/messages`, `/api/message_detail`, `/asset/...`) and a Vite frontend from `web/`, embedded from `web/dist` with `rust-embed`. Sub-modules cover conversations, messages, rich content, assets, HTTP media handling, and metadata enrichment.
- **`image-index`** — Optional local similar-image and copy-detection index behind Cargo features. `src/image_index.rs` builds a read-only media manifest with SHA256, pHash, blur/quality metadata, optional MobileCLIP2/OpenCLIP semantic vectors, optional SSCD copy descriptors, and SQLite-backed query/status APIs. Windows GPU paths use ONNX Runtime CUDA or DirectML when the matching features are enabled.
- **`capture`** — Frida hook script generation, CLI attach loop, event JSONL capture, credential normalization (`src/capture.rs`, `src/credentials.rs`).
- **`preprocess`** — DB discovery, classification, safe copy, NTQQ 1024-byte header stripping, PCQQ encrypted SQLite rekey orchestration, CFB extraction (`src/preprocess.rs`, `src/catalog.rs`, `src/snapshot.rs`).
- **`db`** — Read-only SQLite tools: schema analysis/Markdown reports, sampling, inspection, CSV export, SenderUin row lookup (`src/db_analysis.rs`, `src/sqlite_tools.rs`).
- **`config`** — Path resolution and environment variable support (`src/config.rs`).

**Unified CLI:** `src/bin/qq_analyzer_rs.rs` — subcommands: `assets`, `inventory`, `capture`, `catalog`, `credentials`, `db`, `html`, `image-index`, `info`, `migration`, `msg3`, `preprocess`, `snapshot`, `serve`.

**Data flow:** `inventory` → `capture` (Frida keys) → `preprocess` (decrypt/copy DBs) → `serve` (rusqlite read-only → JSON API + HTML UI). InfoStorage enriches conversation labels and profiles. The Msg3 parser turns blobs into rich nodes, then asset resolution serves matched local media through `/asset/...`.

## Key Conventions

- **Source data is read-only evidence.** Never modify QQ databases, media, received files, or installation binaries. Open SQLite with read-only flags. Operate on copies under `output/`.
- **No secrets in code.** Database keys come from env vars, key files, Frida capture JSONL, or interactive prompts. Never hardcode keys, account ids, or local paths.
- **`output/` and `archive/` are gitignored.** Write all generated artifacts there. Run `cargo run --bin qq_analyzer_rs -- migration audit-python` after touching Python scripts.
- **Avoid `unwrap()`/`expect()` in service/parser paths.** Tests and one-off probes may use them.
- **Config via `config.rs`** — use CLI args, env vars (`.env.local`), or the credentials JSONL store. Do not hardcode ports (`8765`), accounts, or install paths in main code.
- **Windows wrappers** (`run_windows.ps1`, `run_rust_tool_windows.ps1`, `run_rust_service_windows.ps1`) load `.env.local` automatically. For WSL→Windows DB snapshot/copy operations, run the Windows executable so fallback copies stay on the Windows side.

## Ghidra Reverse Engineering

Target binary: `C:\Program Files (x86)\Tencent\QQ\Bin\IM.dll`. Bridge at `http://127.0.0.1:8089`. Rules in `GHIDRA_MCP_RULES.md` and `docs/ghidra-verification.md`:

- Prefer read-only endpoints: `list_open_programs`, `search_functions`, `decompile_function`, xref queries.
- Do not rename, comment, retype, rebase, or save unless explicitly asked.
- Record findings under `output/ghidra/`.
- Bridge runtime: `qq-analyzer/.venv/bin/python`. Dependencies in `requirements-ghidra-mcp.txt`.

## Public Release

The repo is WTFPL licensed. Publish from a clean-history `public-main` branch:

```bash
tree=$(git rev-parse HEAD^{tree})
commit=$(printf '%s\n' 'Public clean snapshot' | git commit-tree "$tree")
git branch -f public-main "$commit"
```

Before publishing, run the sanitization checks in the README (private scan patterns, key grep, public branch tree audit) and validate: `cargo fmt --all -- --check`, `cargo test --workspace --bins --lib`, `git diff --check`.
