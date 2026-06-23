# Ghidra Verification

## Rules

- Target program: `IM.dll` unless a finding explicitly names another QQ DLL.
- Use read-only Ghidra/MCP queries first.
- Pass the target program explicitly when multiple programs are open.
- Do not rename functions, change prototypes, apply types, or save the project
  unless the user explicitly asks.
- Record conclusions here or in subsystem docs, not only in `output/`.

## Evidence Sources

Formal docs summarize these local findings:

- `output/ghidra/msg3_msgcontent_findings.md`
- `output/ghidra/common_txdata_codec_plan.md`
- `output/ghidra/txdata_string_codec.md`
- `output/ghidra/info_storage_txdata_findings_2026-06-17.md`
- `output/ghidra/group_member_name_findings.md`
- `output/ghidra/group_avatar_hash_cache.md`
- `output/ghidra/file_recv_path_findings.md`
- `output/ghidra/offline_file_download_findings.md`
- `output/ghidra/mmt_nested_forwarding_20260618.md`
- `output/ghidra/sysface_resource_findings.md`
- `output/ghidra/bsAbstractText-behavior.md`

Raw decompile text files and helper Java scripts remain in `output/`.

## Required Record Format

For each behavior-level rule, keep:

- DLL/program.
- Function name or address.
- Relevant string/xref if applicable.
- Local sample row or asset path.
- Parser/service module that implements the rule.
- Validation command or API URL.

