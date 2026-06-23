# Security Policy

This project is intended for local analysis of data that the operator is
authorized to access. Do not submit private QQ data, database keys, captured
Frida event logs, local paths, account ids, screenshots, exported HTML, media,
or generated `output/` artifacts in issues or pull requests.

## Reporting

For public reports, describe the bug with synthetic examples whenever possible.
If a report requires private evidence, redact all account ids, keys, names,
file paths, URLs, and message contents before sharing it.

## Local Secrets

Use `.env.local` or normal process environment variables for local settings.
Only `.env.example` is meant to be committed. The following files and
directories are intentionally ignored:

- `.env.local`
- `archive/`
- `output/`
- captured credential/event JSONL files
- copied or decrypted databases
- extracted media and generated HTML

## Supported Versions

This repository is currently pre-1.0. Security fixes target the latest public
snapshot only.
