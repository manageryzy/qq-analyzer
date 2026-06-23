# Migration Map

## Rust Mainline

- `msg3_richtext_parser_rs.rs` -> `msg3` module tree.
- `rust-msg3-parser/src/info_storage.rs` -> `pcqq-storage`.
- `rust-msg3-parser/src/txdata_codec.rs` -> shared TXData module.
- `rust-msg3-parser/src/qq_hash.rs` -> shared QQ hash/resource helper.
- `rust-msg3-parser/src/msg3_log_service.rs` -> Rust web log service module,
  exposed as `qq_analyzer_rs serve`. The standalone `msg3_log_service_rs` bin
  remains only as a compatibility wrapper.
- `rust-msg3-parser/src/msg3_log_service_config.rs` -> service CLI argument
  and catalog-backed path resolution for the web log service.
- `rust-msg3-parser/src/msg3_log_service_http.rs` -> service URL/query
  parsing helpers with unit coverage for current request parsing behavior.
- `rust-msg3-parser/src/msg3_log_service_assets.rs` -> service asset URL
  resolution and candidate matching for rich message nodes.
- `rust-msg3-parser/src/msg3_log_service_asset_candidates.rs` -> service asset
  candidate generation for QQ local paths, thumbnails, built-in faces, and
  FileIndex/Resume TXData records.
- `rust-msg3-parser/src/msg3_log_service_asset_http.rs` -> service asset URL
  generation, local file response serving, content-type sniffing, and root
  boundary checks.
- `rust-msg3-parser/src/msg3_log_service_avatar.rs` -> service-side group and
  member avatar candidate generation, local hit detection, and qlogo fallback
  decisions.
- `rust-msg3-parser/src/msg3_log_service_conversations.rs` -> service
  conversation list/detail queries, conversation-label resolution, and
  conversation metadata caches.
- `rust-msg3-parser/src/msg3_log_service_info.rs` -> shared Msg3 Info blob
  field summary extraction for conversation labels and message payloads.
- `rust-msg3-parser/src/msg3_log_service_messages.rs` -> service message page
  queries, row-detail lookup, sender/contact prefetch, message item rendering,
  and message-side avatar/asset enrichment.
- `rust-msg3-parser/src/msg3_log_service_frontend.rs` -> embedded web UI HTML,
  CSS, and JavaScript for the Rust log service.
- `rust-msg3-parser/src/msg3_log_service_tables.rs` -> conversation table
  splitting, SQLite identifier quoting, and Msg3 conversation table discovery
  helpers.
- `rust-msg3-parser/src/msg3_log_service_time.rs` -> local timestamp formatting
  and conversation last-message time selection helpers.
- `rust-msg3-parser/src/msg3_log_service_text.rs` -> service display-name,
  sender-name, and member-label string helpers.
- `rust-msg3-parser/src/msg3_log_service_models.rs` -> shared service response
  models for conversation and message payloads.
- `rust-msg3-parser/src/msg3_log_service_rich.rs` -> service-side rich-node
  enrichment: nested forwarded-message display data, style metadata, and quote
  target annotations.
- `rust-msg3-parser/src/bin/qq_analyzer_rs.rs` -> unified analyzer CLI.
- `qq_analyzer_rs info ...` -> long-lived InfoStorage label/profile/stream
  inspection, replacing daily use of `pcqq_info_storage.py`.
- `qq_analyzer_rs msg3 row-parse/row-probe` -> unified Msg3 row diagnostics,
  replacing daily use of the standalone `msg3_row_parse` and `msg3_row_probe`
  bins.
- `qq_analyzer_rs msg3 info-parse` -> unified Msg3 Info blob diagnostics,
  replacing daily use of the standalone `msg3_info_parse` bin.
- `rust-msg3-parser/src/credentials.rs` -> credential capture/storage model.
- `rust-msg3-parser/src/capture.rs` -> hook script generation, Frida CLI
  attach loop, event JSONL capture, and credential normalization.
- `qq_analyzer_rs credentials import-key` -> manual PCQQ SQLite,
  InfoStorage TEA, and NTQQ SQLCipher key import into the shared Rust
  credential JSONL store, replacing Python-only key entry paths.
- `rust-msg3-parser/src/inventory.rs` -> evidence inventory.
- `rust-msg3-parser/src/migration_audit.rs` -> Rust-owned Python migration
  audit, exposed as `qq_analyzer_rs migration audit-python`.
- `rust-msg3-parser/src/preprocess.rs` -> DB discovery, classification,
  safe-copy/header-strip, NTQQ prefix preparation, decrypt/preprocess/catalog
  orchestration.
- `rust-msg3-parser/src/cfb.rs` -> QQ-tolerant CFB/OLE stream reader and
  extractor.
- `rust-msg3-parser/src/db_analysis.rs` -> SQLite/CFB/unknown DB Markdown
  reports, replacing `analyze_pcqq_databases.py`.
- `rust-msg3-parser/src/sqlite_tools.rs` -> read-only SQLite sampling and
  SenderUin lookup plus generic SQLite inspect/export, replacing
  `dump_sqlite_sample.py`, `find_sender_rows.py`, and the non-SQLCipher
  inspect/export portions of `qq_analyzer.py`.
- `rust-msg3-parser/src/msg3_samples.rs` -> explicit Msg3 rich-text parser
  sample TSV export, replacing `export_richtext_samples.py`.
- `rust-msg3-parser/src/msg3_index.rs` -> explicit MsgIndex account/LIKE/FTS
  query helper, replacing `query_msgindex_group.py`.
- `rust-msg3-parser/src/html_check.rs` -> generated HTML local-link checker,
  replacing `check_html_dead_links.py`.
- `rust-msg3-parser/src/asset_audit.rs` -> explicit-root asset basename
  matching, C2C unresolved-image MD5 hit checks, and unresolved candidate-rule
  probes, replacing `find_msg3_image_asset_names.py`,
  `check_c2c_md5_hits.py`, and `probe_image_candidate_rules.py`.
- `run_windows.ps1` -> Rust `qq_analyzer_rs` launcher by default. Python is
  available only through the explicit `legacy-python` subcommand.
- `run_windows_script.ps1` -> legacy-only Python probe launcher. It is kept for
  historical one-off scripts and must not become a main analyzer path.

## Rust Tools

Keep these as bins but route them through shared crates:

- parser self-tests.
- parser coverage checker.
- InfoStorage probes.
- file/cache/group-avatar probes.

Diagnostic bins must take account/table/row selectors from CLI options or the
shared `config` resolver. They must not carry local conversation defaults in
normal execution paths; fixed rows belong only in explicit sample coverage tests
or documentation of known parser fixtures.

## Python Legacy Areas

Python is not part of the final architecture. Existing scripts are migration
references only:

- old Frida/key capture scripts -> Rust `capture run`, `capture
  normalize-events`, and `credentials import-key` backends; keep only as
  comparison references until the Rust path has enough Windows attach coverage.
- `pcqq_decrypt_*.py` copy/header classification logic -> Rust preprocess
  `--prepare-pcqq-dbs`; KernelUtil rekey orchestration -> Rust
  `capture pcqq-rekey`.
- `qq_analyzer.py prepare` NTQQ 1024-byte prefix stripping -> Rust
  `qq_analyzer_rs preprocess --prepare-ntqq-dbs`.
- `qq_analyzer.py decrypt/export` NTQQ SQLCipher export remains a legacy
  reference until the Rust credentialed SQLCipher export stage is implemented.
- `extract_pcqq_cfb.py` / `qq_cfb_reader.py` -> Rust `cfb` module and
  `qq_analyzer_rs preprocess --extract-cfb`.
- `analyze_pcqq_databases.py` -> Rust `qq_analyzer_rs db analyze`.
- `dump_sqlite_sample.py` -> Rust `qq_analyzer_rs db sample`.
- `qq_analyzer.py inspect/export` for already-readable SQLite databases -> Rust
  `qq_analyzer_rs db inspect` and `qq_analyzer_rs db export`.
- `find_sender_rows.py` -> Rust `qq_analyzer_rs db sender-rows`.
- `export_richtext_samples.py` -> Rust `qq_analyzer_rs msg3 export-samples`.
- `query_msgindex_group.py` -> Rust `qq_analyzer_rs msg3 index-query`.
- standalone `msg3_info_parse` bin daily usage -> Rust
  `qq_analyzer_rs msg3 info-parse`.
- `pcqq_info_storage.py` common label/profile/stream usage -> Rust
  `qq_analyzer_rs info ...`; keep the Python file only as a migration
  reference until remaining legacy callers are gone.
- `check_html_dead_links.py` -> Rust `qq_analyzer_rs html check-links`.
- `find_msg3_image_asset_names.py` -> Rust
  `qq_analyzer_rs assets basename-match`.
- `check_c2c_md5_hits.py` -> Rust `qq_analyzer_rs assets c2c-md5-hits`.
- `probe_image_candidate_rules.py` -> Rust
  `qq_analyzer_rs assets candidate-rules`.
- old Python service/export/parser helpers -> Rust service/export.
- one-off audit/inspect/check/probe scripts are not automatically migrated.
  If a script captures durable behavior needed by the analyzer, move that
  behavior into Rust modules, tests, or docs. If it was only a local historical
  investigation with hardcoded samples, leave it as a legacy reference until it
  can be archived or deleted.

Do not add new Python features for the main analyzer path.

Run `qq_analyzer_rs migration audit-python --strict` as the Rust-side gate for
new Python files. Files must be classified as Rust-replaced, `todo_ntqq`, or
legacy one-off probes; unknown Python files are migration blockers.
Local-only archived Python scripts may exist under `archive/python-legacy/`.
That directory is ignored and must not be committed to the public source tree.

## Generated/Temporary Files

Root `.exe` files, parser executables, logs, caches, DBs, extracted resources,
and raw Ghidra dumps are generated artifacts. They should not become tracked
source files.
