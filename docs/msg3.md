# Msg3 Parser Notes

## Verified Abstract Behavior

Ghidra findings show `IM.dll` reads `MsgContent` as a blob, creates a message
pack through `KernelUtil.dll`, and obtains abstracts with `GetMsgAbstract`.
`GetMsgAbstract` first checks custom data such as `bsAbstractText`; otherwise it
iterates elements and calls `GetMsgAbstarctByElement`.

Current parser rules:

- Type `1`: text.
- Type `2`: system face.
- Type `3`, `5`, `6`: image/custom-face style elements. Structured image
  metadata is diagnostic unless it is the explicit visible path/summary.
- Type `7`, `0x11`: file/audio/struct-style elements.
- Type `0x0c`: rich-format context. It participates in nearby element behavior
  but should not become visible body text by itself.
- Type `0x14`, `0x16`, `0x18`, `0x19`, `0x15`: rich/specialized elements.
  Render only confirmed visible behavior.
- Type `0x1a`: video message.
- Type `0x1b`: Ark app/mini-program style element.
- Type `0x1e`: multi-message/forwarded-message container.

## Current Rust API

The current public parser surface is:

- `parse_msgcontent_outputs(data) -> (text, elements_json, rich_nodes_json, rich_html)`
- `parse_info_json(data) -> String`
- `parse_info_mmp_items_json(data) -> String`

The next refactor should keep this surface compatible while moving the parser
from the root `msg3_richtext_parser_rs.rs` file into normal Rust modules.

## Validation

Use both quality and coverage checks. Quality checks can pass while coverage
still reports unclassified binary elements, so treat them as separate gates.

