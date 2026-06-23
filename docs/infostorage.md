# InfoStorage Notes

## Role

PCQQ InfoStorage provides conversation labels, group profiles, group-member
profiles, contact profiles, avatar metadata, and some custom resource indexes.

The Rust `InfoStorage` API currently exposes:

- `label(kind, ident)`
- `group_profile(group_id)`
- `group_member_profiles_for(group_id, wanted)`
- `contact_profiles_for(wanted)`
- `friend_social_image_profiles_for(wanted)`
- `entries_for_stream(rel, wanted)`

## Verified Name Behavior

Ghidra findings indicate group-member display names and titles are distinct.
Member title fields must not be used as the sender display name. Display names
come from the member/profile fields that QQ uses for card/name display; title
metadata should be shown separately.

Group labels prefer long/current group names from InfoStorage. System rename
messages may be used as a fallback, but must not override decoded current
metadata when InfoStorage is available.

## Keys and Evidence

InfoStorage decryption uses keys captured from the live QQ process. Do not
hard-code keys. The Rust credential model stores normalized records in
`output/<account>/credentials/credentials.jsonl`. Legacy
`pcqq_live/infostorage_keys.jsonl` files are not a runtime default; they may be
imported only as a migration source.
