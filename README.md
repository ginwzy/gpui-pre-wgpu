# gpui-pre-wgpu synthetic oblique patch

This repository contains the published `gpui-pre-wgpu` 0.3.5 snapshot of
Zed's Apache-2.0 `gpui_wgpu` crate at
`d89e9c2124b2786a390c7a451c7488601b4da2e1`, with synthetic oblique text.
The initial commit imports the crates.io package unchanged. Modified files
carry a notice; the original license is preserved in `LICENSE-APACHE`.

When italic text resolves to a face without italics, the cosmic-text system
asks cosmic-text for italic attributes, and every glyph it marks as fake
italic, fallbacks included, is drawn with a separate oblique `FontId` sheared
by 14° through swash. Shaping and advances are unchanged. Emoji are never
slanted. The test fonts under `assets/fonts` are OFL copies of IBM Plex Sans
and Lilex; the published package referenced them outside the crate. The
shader tests' `naga` dev-dependency, dropped from the published manifest, is
restored. It keeps the package version and all other GPUI snapshot
dependencies unchanged.

Consumers use a Cargo `[patch.crates-io]` entry pinned to the fix commit.
Run checks with `cargo test --lib` and
`cargo clippy --lib --tests -- -D warnings`.
