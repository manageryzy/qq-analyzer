# Coding Standards

## Rust

- Prefer Rust for all new parser, credential capture, preprocessing/decryption,
  storage, asset, service, export, and test work.
- Keep QQ source data read-only. Open SQLite databases with read-only flags
  unless a tool explicitly writes to `qq-analyzer/output/`.
- Avoid hard-coded accounts, host paths, QQ install paths, and ports in main
  code. Use config, CLI arguments, or environment variables.
- Do not use broad string cleanup as parsing. If bytes have unknown semantics,
  expose them as diagnostics or fail validation rather than rendering guessed
  visible text.
- Service/parser paths should not use `unwrap()` or `expect()` for recoverable
  failures. Tests and one-off probes may use them when the failure is the test
  outcome.
- Run `cargo fmt` before committing Rust changes and at least
  `cargo check --bins` after structural edits.

## Python

- Python is legacy-only and must not become a dependency of the final analyzer
  workflow.
- Keep existing Python hook scripts isolated as migration references until Rust
  capture replaces them.
- Do not add new long-lived parser behavior to Python unless explicitly scoped
  as a temporary probe.

## Paths and IO

- Large SQL or filesystem IO should run in native Windows when practical.
- Do not scan the whole account tree when a database/index/path rule can answer
  the query.
- Generated reports and extracted resources belong under `qq-analyzer/output/`.

## Tests

Parser changes need representative row-based validation, but public
documentation must not contain real conversation table ids, account ids, or
private row manifests. Keep local fixture selectors under ignored `output/`
reports or local-only notes.

Coverage should include these behavior categories:

- built-in system faces.
- file metadata and file-card rendering.
- inline style metadata.
- quote and folded forwarding nodes.
- mini-program and link cards.
- nested forwarded messages with rich child nodes.
