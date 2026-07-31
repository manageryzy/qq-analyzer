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

The UI lives in `web/` and is built with Vite. `npm run build` writes
`web/dist`; the Rust `web-ui` feature embeds that directory with `rust-embed`.
`web-ui` is a default crate feature, so the documented `cargo run ... serve`
command serves the UI without an extra feature flag. Run `npm run check`,
`npm test`, and `npm run build` after frontend changes.
