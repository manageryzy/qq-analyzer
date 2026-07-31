Qdrant source is vendored from tag `v1.16.3`, commit
`bd49f45a8a2d4e4774cac50fa29507c4e8375af2`.

Only the Rust libraries used by `image-index-qdrant` are linked into QQ
Analyzer. The `segment` dependency is changed to use the adjacent vendored
`rust-stemmers` source instead of Git.

`rust-stemmers` is vendored from Qdrant's tag `v1.2.1`, commit
`aee4c73b4012230b1163bf82d086cbf4b3f1102e`.

The upstream Apache-2.0 and MIT license files remain in this directory.
