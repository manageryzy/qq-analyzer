# Contributing

## Ground Rules

- Keep source QQ databases, media, received files, and installation binaries
  read-only.
- Do not commit real account ids, local absolute paths, database keys,
  credential/event logs, generated outputs, copied databases, or media files.
- Keep durable analyzer behavior in Rust. Python is only for local, ignored
  migration references or short-lived private probes.
- Prefer synthetic test fixtures. If a real sample is needed for local
  debugging, keep it under `output/` or another ignored path.

## Local Setup

```bash
cp .env.example .env.local
cd rust-msg3-parser
cargo test --workspace --bins --lib
```

Windows wrappers load `.env.local` automatically through `load_env.ps1`.

## Before Sending Changes

Run:

```bash
cargo fmt --all
cargo test --workspace --bins --lib
git diff --check
```

Also run the sanitization commands in the README before publishing or sending a
branch outside the local environment. Some field names such as `key_hex` are
normal in code; real values are not.

## Public Branch

Use the sanitized public branch or a freshly created clean-history branch for
publication. Do not publish local development history that once tracked ignored
archives or generated outputs.
