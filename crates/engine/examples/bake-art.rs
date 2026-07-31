//! Bake otto's terminal mark from a PNG, or export the current one back to a PNG.
//!
//! The mark's pixels live in `src/banner/art.rs`, which this tool generates. Edit the art in any
//! pixel editor and re-bake rather than hand-editing the grid.
//!
//! ```text
//! cargo run -p otto-engine --example bake-art -- bake <in.png> [flags]
//! cargo run -p otto-engine --example bake-art -- export <out.png> [--scale <n>]
//! ```
//!
//! `bake` flags:
//!   --out <path>          where to write the generated module (default: src/banner/art.rs)
//!   --map '#rrggbb=C'     pin a colour to a grid character (repeatable)
//!   --hole '#rrggbb'      treat a colour as a cut-out rather than ink in Mono (repeatable)
//!
//! Pinning with `--map` keeps re-bakes reviewable: without it, characters are assigned by pixel
//! frequency, so touching one pixel can reshuffle the whole grid. Without any `--hole`, cut-outs
//! are inferred from luminance.
//!
//! The tool is deliberately strict — it refuses art it cannot represent faithfully (non-uniform
//! pixel blocks, an odd row count, more colours than the alphabet) rather than silently
//! approximating, because the result is baked into source and is easy to not notice.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use image::{Rgba, RgbaImage};
use otto_engine::banner::{ART, ART_W, PALETTE};

type Err = Box<dyn std::error::Error>;

/// Characters the generator draws from, in order. `.` is reserved for transparency.
const ALPHABET: &[u8] = b"BLbKCVGADEFHIJMNOPQRSTUWXYZacdefghijklmnopqrstuvwxyz0123456789";

/// Below this relative luminance a colour is assumed to be a screen well — a cut-out in Mono —
/// unless `--hole` says otherwise. Chosen to sit between otto's near-black wells and its shadows.
const HOLE_LUMA: f64 = 0.05;

fn main() -> Result<(), Err> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bake") => bake(&args[1..]),
        Some("export") => export(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  bake-art bake <in.png> [--out <path>] [--map '#rrggbb=C']... \
                 [--hole '#rrggbb']...\n  bake-art export <out.png> [--scale <n>]"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// bake: PNG -> Rust
// ---------------------------------------------------------------------------------------------

fn bake(args: &[String]) -> Result<(), Err> {
    let mut input = None;
    let mut out = PathBuf::from("crates/engine/src/banner/art.rs");
    let mut pins: BTreeMap<[u8; 3], u8> = BTreeMap::new();
    let mut holes: Vec<[u8; 3]> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(next(&mut it, "--out")?),
            "--map" => {
                let raw = next(&mut it, "--map")?;
                let (hex, ch) = raw
                    .split_once('=')
                    .ok_or_else(|| format!("--map wants '#rrggbb=C', got {raw:?}"))?;
                let ch = ch.as_bytes();
                if ch.len() != 1 || ch[0] == b'.' {
                    return Err(
                        format!("--map target must be one character, not '.': {raw:?}").into(),
                    );
                }
                pins.insert(parse_hex(hex)?, ch[0]);
            }
            "--hole" => holes.push(parse_hex(&next(&mut it, "--hole")?)?),
            other if other.starts_with("--") => return Err(format!("unknown flag {other}").into()),
            other => input = Some(PathBuf::from(other)),
        }
    }
    let input = input.ok_or("bake needs an input .png")?;

    let img = image::open(&input)?.to_rgba8();
    let scale = detect_scale(&img)?;
    let cells = downsample(&img, scale);
    let (w, h) = (cells[0].len(), cells.len());
    if h % 2 != 0 {
        return Err(format!(
            "art is {h} pixels tall; half-block rendering needs an even number of rows"
        )
        .into());
    }

    let (cells, note) = flatten(cells);
    let palette = assign(&cells, &pins, &holes)?;
    let source = emit(&input, scale, w, h, &cells, note, &palette)?;
    std::fs::write(&out, source)?;
    eprintln!(
        "baked {} ({w}x{h} px at scale {scale}, {} colours) -> {}",
        input.display(),
        palette.len(),
        out.display()
    );
    Ok(())
}

/// The largest `n` for which every `n`×`n` block is a single colour — the art's true pixel size.
///
/// Refuses anything that is not cleanly blocky: a resampled or anti-aliased PNG would otherwise
/// bake into a grid that silently differs from what the author drew.
fn detect_scale(img: &RgbaImage) -> Result<u32, Err> {
    let (w, h) = img.dimensions();
    for n in (1..=w.min(h)).rev() {
        if w % n != 0 || h % n != 0 || !uniform(img, n) {
            continue;
        }
        return Ok(n);
    }
    Err("could not find a uniform pixel size; is the PNG anti-aliased or resampled?".into())
}

fn uniform(img: &RgbaImage, n: u32) -> bool {
    let (w, h) = img.dimensions();
    (0..h / n).all(|by| {
        (0..w / n).all(|bx| {
            let first = img.get_pixel(bx * n, by * n);
            (0..n).all(|j| (0..n).all(|i| img.get_pixel(bx * n + i, by * n + j) == first))
        })
    })
}

fn downsample(img: &RgbaImage, n: u32) -> Vec<Vec<[u8; 4]>> {
    let (w, h) = img.dimensions();
    (0..h / n)
        .map(|y| (0..w / n).map(|x| img.get_pixel(x * n, y * n).0).collect())
        .collect()
}

/// Resolve which cells are transparent, returning `None` for each.
///
/// Alpha is the authority: it is the one channel that cannot collide with a colour the art
/// actually uses. `export` writes transparency as alpha, and every pixel editor round-trips it.
///
/// A fully opaque PNG (someone flattened it, or drew the art from scratch on a solid backdrop)
/// has no alpha to read, so the border-majority colour is treated as the backdrop instead. That
/// heuristic is why alpha is preferred: if the backdrop colour also appears inside the art — as
/// otto's near-black screen wells do — flattening silently merges the two.
fn flatten(cells: Vec<Vec<[u8; 4]>>) -> (Vec<Vec<Option<[u8; 3]>>>, &'static str) {
    let rgb = |c: [u8; 4]| [c[0], c[1], c[2]];
    if cells.iter().flatten().any(|c| c[3] < 128) {
        let out = cells
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|c| (c[3] >= 128).then(|| rgb(c)))
                    .collect()
            })
            .collect();
        return (out, "alpha");
    }

    let (h, w) = (cells.len(), cells[0].len());
    let mut tally: BTreeMap<[u8; 3], usize> = BTreeMap::new();
    for (y, row) in cells.iter().enumerate() {
        for (x, c) in row.iter().enumerate() {
            if y == 0 || y == h - 1 || x == 0 || x == w - 1 {
                *tally.entry(rgb(*c)).or_default() += 1;
            }
        }
    }
    let bg = tally
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| c)
        .expect("art has at least one pixel");
    let out = cells
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|c| (rgb(c) != bg).then(|| rgb(c)))
                .collect()
        })
        .collect();
    (
        out,
        "border colour (the PNG is fully opaque — no alpha to read)",
    )
}

/// Relative luminance, used to guess which colours are screen wells.
fn luma([r, g, b]: [u8; 3]) -> f64 {
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// Map every opaque colour to a grid character and an ink flag.
/// Ordered by descending pixel count so the busiest colours get the earliest characters.
fn assign(
    cells: &[Vec<Option<[u8; 3]>>],
    pins: &BTreeMap<[u8; 3], u8>,
    holes: &[[u8; 3]],
) -> Result<Vec<(u8, [u8; 3], bool)>, Err> {
    let mut tally: BTreeMap<[u8; 3], usize> = BTreeMap::new();
    for c in cells.iter().flatten().flatten() {
        *tally.entry(*c).or_default() += 1;
    }
    let mut order: Vec<_> = tally.into_iter().collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Pinned characters are claimed up front, so what remains is fixed and can be handed out in
    // order — no need to re-check for collisions as we go.
    let pinned: Vec<u8> = pins.values().copied().collect();
    let free: Vec<u8> = ALPHABET
        .iter()
        .copied()
        .filter(|c| !pinned.contains(c))
        .collect();
    let mut free = free.into_iter();
    let mut out = Vec::new();
    for (rgb, _) in order {
        let ch = match pins.get(&rgb) {
            Some(c) => *c,
            None => free
                .next()
                .ok_or("art has more colours than the generator's alphabet")?,
        };
        let ink = if holes.is_empty() {
            luma(rgb) >= HOLE_LUMA
        } else {
            !holes.contains(&rgb)
        };
        out.push((ch, rgb, ink));
    }
    Ok(out)
}

fn emit(
    src: &Path,
    scale: u32,
    w: usize,
    h: usize,
    cells: &[Vec<Option<[u8; 3]>>],
    transparency: &str,
    palette: &[(u8, [u8; 3], bool)],
) -> Result<String, Err> {
    let name = src.file_name().unwrap_or(src.as_os_str()).to_string_lossy();
    let mut s = String::new();
    writeln!(s, "//! @generated — do not edit by hand.")?;
    writeln!(s, "//!")?;
    writeln!(
        s,
        "//! Baked from `{name}` ({w}×{h} pixels at scale {scale})."
    )?;
    writeln!(
        s,
        "//! To change the mark, edit that PNG in any pixel editor and re-bake:"
    )?;
    writeln!(s, "//!")?;
    writeln!(s, "//! ```text")?;
    writeln!(
        s,
        "//! cargo run -p otto-engine --example bake-art -- bake <path/to/{name}> \\"
    )?;
    let pins: Vec<String> = palette
        .iter()
        .map(|(ch, rgb, _)| format!("--map '{}={}'", hex(*rgb), *ch as char))
        .collect();
    writeln!(s, "//!     {}", pins.join(" "))?;
    let holes: Vec<String> = palette
        .iter()
        .filter(|(_, _, ink)| !ink)
        .map(|(_, rgb, _)| format!("--hole '{}'", hex(*rgb)))
        .collect();
    if !holes.is_empty() {
        writeln!(s, "//!     {}", holes.join(" "))?;
    }
    writeln!(s, "//! ```")?;
    writeln!(s, "//!")?;
    writeln!(
        s,
        "//! The `--map` flags pin each colour to a stable grid character so re-baking produces a"
    )?;
    writeln!(
        s,
        "//! readable diff instead of reshuffling the grid. Transparency came from {transparency}."
    )?;
    writeln!(s)?;
    writeln!(s, "/// Grid width, in pixels.")?;
    writeln!(s, "pub const ART_W: usize = {w};")?;
    writeln!(s)?;
    writeln!(
        s,
        "/// The palette: grid character, RGB, and whether it is ink once colour is gone."
    )?;
    writeln!(s, "///")?;
    writeln!(
        s,
        "/// `.` is absent by design — it is transparent, so it is neither coloured nor inked."
    )?;
    writeln!(
        s,
        "pub const PALETTE: [(u8, [u8; 3], bool); {}] = [",
        palette.len()
    )?;
    // rustfmt aligns trailing comments in an array literal to a common column. Match it, so that
    // re-baking leaves the tree already formatted and `cargo fmt --check` stays green.
    let entries: Vec<(String, String)> = palette
        .iter()
        .map(|(ch, rgb, ink)| {
            let [r, g, b] = *rgb;
            (
                format!("    (b'{}', [{r}, {g}, {b}], {ink}),", *ch as char),
                hex(*rgb),
            )
        })
        .collect();
    let width = entries.iter().map(|(e, _)| e.len()).max().unwrap_or(0);
    for (entry, rgb) in &entries {
        writeln!(s, "{entry:<width$} // {rgb}")?;
    }
    writeln!(s, "];")?;
    writeln!(s)?;
    writeln!(
        s,
        "/// The mark, {w} pixels wide by {h} tall. `.` is transparent."
    )?;
    writeln!(s, "#[rustfmt::skip]")?;
    writeln!(s, "pub const ART: [&str; {h}] = [")?;
    for row in cells {
        let line: String = row
            .iter()
            .map(|c| match c {
                None => '.',
                Some(c) => palette
                    .iter()
                    .find(|(_, rgb, _)| rgb == c)
                    .map(|(ch, _, _)| *ch as char)
                    .expect("every opaque colour is in the palette"),
            })
            .collect();
        writeln!(s, "    \"{line}\",")?;
    }
    writeln!(s, "];")?;
    Ok(s)
}

// ---------------------------------------------------------------------------------------------
// export: Rust -> PNG
// ---------------------------------------------------------------------------------------------

/// Write the mark currently baked into `banner::art` back out as a PNG, so the art can be round
/// tripped through an editor without keeping a separate master file in sync by hand.
fn export(args: &[String]) -> Result<(), Err> {
    let mut out = None;
    let mut scale: u32 = 8;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--scale" => scale = next(&mut it, "--scale")?.parse()?,
            other if other.starts_with("--") => return Err(format!("unknown flag {other}").into()),
            other => out = Some(PathBuf::from(other)),
        }
    }
    let out = out.ok_or("export needs an output .png")?;
    if scale == 0 {
        return Err("--scale must be at least 1".into());
    }

    // Transparency is written as alpha, never as a backdrop colour. Painting the backdrop with a
    // real colour would be indistinguishable from art drawn in that same colour — otto's screen
    // wells are near-black, so a black backdrop would swallow them on the next bake.
    let (w, h) = (ART_W as u32 * scale, ART.len() as u32 * scale);
    let mut img = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    for (y, row) in ART.iter().enumerate() {
        for (x, ch) in row.bytes().enumerate() {
            let Some((_, [r, g, b], _)) = PALETTE.iter().find(|(c, _, _)| *c == ch) else {
                continue; // transparent
            };
            for dy in 0..scale {
                for dx in 0..scale {
                    img.put_pixel(
                        x as u32 * scale + dx,
                        y as u32 * scale + dy,
                        Rgba([*r, *g, *b, 255]),
                    );
                }
            }
        }
    }
    img.save(&out)?;
    eprintln!(
        "exported {}x{} px ({}x{} at scale {scale}) -> {}",
        w,
        h,
        ART_W,
        ART.len(),
        out.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------

fn next<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, Err> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value").into())
}

fn parse_hex(s: &str) -> Result<[u8; 3], Err> {
    let h = s.trim_start_matches('#');
    if h.len() != 6 {
        return Err(format!("expected #rrggbb, got {s:?}").into());
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16);
    Ok([byte(0)?, byte(2)?, byte(4)?])
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}
