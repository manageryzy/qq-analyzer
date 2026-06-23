# Web UI Notes

## Current Contract

The Rust web service currently owns the active UI. Keep these routes compatible
during refactors:

- `/`
- `/api/status`
- `/api/conversations`
- `/api/conversation_detail`
- `/api/conversation_details`
- `/api/messages`
- `/api/message_detail`
- `/asset/...`

The UI uses rowid-based paging for large tables. `offset` is treated as a rowid
cursor in the Rust service.

## Rendering Rules

- Rich nodes are the display source of truth.
- Diagnostics are loaded on demand through `/api/message_detail`.
- Images/faces can render inline only when a local asset is matched.
- Media/file controls should be available for matched assets.
- Clicking a message may update the URL offset/hash, but media/link clicks must
  not be hijacked.

## Refactor Direction

Move inline HTML/CSS/JS out of the service binary into a template/static module
first. Do not redesign the UI in the same step as server/module splitting.

