# Asset Resolution Notes

## Goal

Asset resolution should match local files by protocol evidence, not broad full
disk search. The web service should annotate rich nodes on demand and serve
matched local files through `/asset/...`.

## Known Resource Families

- User image paths such as `UserDataImage:...`.
- Custom faces and CFB-extracted resource databases.
- Group custom head images under extracted `Misc.db` / `MiscHead.db`.
- Classic system faces under `SysFaceResFileSystem:`.
- File-transfer metadata and received-file paths.
- Voice/video/image candidates from Msg3 rich nodes and TXData fields.

## SysFace

Ghidra findings show classic sysface resource data is constructed as:

- `SysFaceResFileSystem:<FaceId>.gif`
- `SysFaceResFileSystem:apng\<FaceId>.png`

For face id `212`, the IM.dll static shortcut table maps to `/qw`.

## Asset Policy

- Prefer exact protocol paths, decoded hashes, and known QQ resource roots.
- Avoid full-account scans when a database/index path is available.
- Do not render unmatched images as fake media boxes. Keep unmatched candidates
  in diagnostics.

