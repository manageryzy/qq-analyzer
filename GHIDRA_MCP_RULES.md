# Ghidra MCP Rules

Scope: analyze the PC QQ `IM.dll` selected by the local operator for this
database recovery task.

Rules:

- Use the local Ghidra MCP service only: `http://127.0.0.1:8089`.
- Prefer read-only analysis endpoints first: `list_open_programs`, `analysis_status`, `search_functions`, `search_strings`, `get_function_by_address`, `decompile_function`, xref/listing queries.
- Do not rename functions, set comments, change prototypes, apply data types, rebase, or save the project unless explicitly asked.
- Always pass the target `program` parameter when more than one program is open.
- Keep helper scripts inside `qq-analyzer`.
- Treat `IM.dll` as the target binary. If Ghidra reports another current program, switch to or import `IM.dll` before analysis.
- Record findings in `qq-analyzer/output/ghidra/` rather than modifying source QQ files.

Bridge:

- MCP bridge path: set `GHIDRA_MCP_BRIDGE` to the local bridge script path.
- Preferred bridge runtime from WSL: `qq-analyzer/.venv/bin/python`
- Direct Ghidra HTTP fallback: `http://127.0.0.1:8089`
- Optional local MCP config: `.codex/config.toml` (ignored, do not commit).

WSL venv setup:

```bash
python3 -m venv qq-analyzer/.venv
qq-analyzer/.venv/bin/python -m pip install 'mcp>=1.2.0,<2'
```

Bridge validation:

```bash
qq-analyzer/.venv/bin/python "$GHIDRA_MCP_BRIDGE" --help
GHIDRA_MCP_URL=http://127.0.0.1:8089 qq-analyzer/.venv/bin/python "$GHIDRA_MCP_BRIDGE" --no-lazy
```
