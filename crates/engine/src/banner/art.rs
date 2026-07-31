//! @generated — do not edit by hand.
//!
//! Baked from `otto-mark.png` (50×36 pixels at scale 8).
//! To change the mark, edit that PNG in any pixel editor and re-bake:
//!
//! ```text
//! cargo run -p otto-engine --example bake-art -- bake <path/to/otto-mark.png> \
//!     --map '#7aa2f7=B' --map '#0b0d10=K' --map '#3d59a1=b' --map '#a9c4ff=L' --map '#7dcfff=C' --map '#6b7480=D' --map '#e0af68=A' --map '#bb9af7=V' --map '#9ece6a=G'
//!     --hole '#0b0d10'
//! ```
//!
//! The `--map` flags pin each colour to a stable grid character so re-baking produces a
//! readable diff instead of reshuffling the grid. Transparency came from alpha.

/// Grid width, in pixels.
pub const ART_W: usize = 50;

/// The palette: grid character, RGB, and whether it is ink once colour is gone.
///
/// `.` is absent by design — it is transparent, so it is neither coloured nor inked.
pub const PALETTE: [(u8, [u8; 3], bool); 9] = [
    (b'B', [122, 162, 247], true), // #7aa2f7
    (b'K', [11, 13, 16], false),   // #0b0d10
    (b'b', [61, 89, 161], true),   // #3d59a1
    (b'L', [169, 196, 255], true), // #a9c4ff
    (b'C', [125, 207, 255], true), // #7dcfff
    (b'D', [107, 116, 128], true), // #6b7480
    (b'A', [224, 175, 104], true), // #e0af68
    (b'V', [187, 154, 247], true), // #bb9af7
    (b'G', [158, 206, 106], true), // #9ece6a
];

/// The mark, 50 pixels wide by 36 tall. `.` is transparent.
#[rustfmt::skip]
pub const ART: [&str; 36] = [
    ".......................AAAA.......................",
    ".......................AAAA.......................",
    "........................bb........................",
    "........................bb........................",
    "........................bb........................",
    "................BLLLLLLLLLLLLLLLLB................",
    "...............BBBBBBBBBBBBBBBBBBBB...............",
    "..............BBBBBBBBBBBBBBBBBBBBBB..............",
    "......bbb....BBB.KKKKKKKKKKKKKKKK.BBB.....bbb.....",
    "....bbbbbbb..BBBKKKKKKKKKKKKKKKKKKBBB...bbbbbbb...",
    ".....bKKKb...BBBKKLCCCCKKKKLCCCCKKBBB....bKKKb....",
    "....bbbbbbb..BBBKKCCCCCKKKKCCCCCKKBBB...bbbbbbb...",
    ".....bbbbbDDDBBBKKCCCCCKKKKCCCCCKKBBBDDD.bbbbb....",
    "....bbbbbbb..BBBKKKKKKKKKKKKKKKKKKBBB...bbbbbbb...",
    ".....bKKbb...BBBKKKKKKKKKKKKKKKKKKBBB....bKKbb....",
    "....bbbbbbb..BBBKKVVVVKDDDDDDKGGKKBBB...bbbbbbb...",
    "......bbb....BBBKKKKKKKKKKKKKKKKKKBBB.....bbb.....",
    ".............BBB.KKKKKKKKKKKKKKKK.BBB.............",
    "..............BBBBBBBBBBBBBBBBBBBBBB..............",
    "...............BBBBBBBBBBBBBBBBBBBB...............",
    "...............bbbbbbbbbbbbbbbbbbbb...............",
    "......................bbbbbb......................",
    "......................bbbbbb......................",
    "......................bbbbbb......................",
    ".....BBBLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLBBB.....",
    "....BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB....",
    "...BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB...",
    "..BBBBBBB.KKKKKKKKKKKKKKKKKKKKKKKKKKKKKK.BBBBBBB..",
    "..BBbbbbBKKKCCCKKKKVVVKKKKGGGKKKKAAAKKKKKBbbbbBB..",
    "..BBbBBBBKKKCKCKKKKVKVKKKKGKGKKKKAKAKKKKKBBBBbBB..",
    "..BBbbBBBKKKCCCDDDDVVVDDDDGGGDDDDAAADKKKKBBBbbBB..",
    "..BBBBBBBKKKCCCKKKKVVVKKKKGGGKKKKAAAKKKKKBBBBBBB..",
    "...BBBBBB.KKKKKKKKKKKKKKKKKKKKKKKKKKKKKK.BBBBBB...",
    "....BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB....",
    ".....BbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbB.....",
    ".......bbbbbb........................bbbbbb.......",
];
