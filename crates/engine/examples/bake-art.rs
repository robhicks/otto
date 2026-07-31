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
//!   --scale <n>           assert the art is drawn at n×n blocks, instead of auto-detecting
//!
//! Pinning with `--map` keeps re-bakes reviewable: without it, characters are assigned by pixel
//! frequency, so touching one pixel can reshuffle the whole grid. Without any `--hole`, cut-outs
//! are inferred from luminance.
//!
//! The result is baked into source and easy to not look at closely, so the tool refuses what it
//! cannot represent faithfully rather than approximating: an odd row count (half-block rendering
//! consumes rows in pairs), more colours than its alphabet, a `--map` target that is unsafe to
//! splice into Rust or that would make one character mean two colours, and — when `--scale` is
//! given — art that is not actually drawn at that block size.
//!
//! Auto-detection cannot supply that last check on its own: `n = 1` is trivially uniform, so an
//! anti-aliased or resampled PNG detects as scale 1 rather than failing. Pass `--scale` to assert
//! the size you drew at.

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
    let mut asserted: Option<u32> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(next(&mut it, "--out")?),
            "--map" => add_pin(&next(&mut it, "--map")?, &mut pins)?,
            "--hole" => holes.push(parse_hex(&next(&mut it, "--hole")?)?),
            "--scale" => asserted = Some(next(&mut it, "--scale")?.parse()?),
            other if other.starts_with("--") => return Err(format!("unknown flag {other}").into()),
            other => input = Some(PathBuf::from(other)),
        }
    }
    let input = input.ok_or("bake needs an input .png")?;

    let img = image::open(&input)?.to_rgba8();
    let scale = match asserted {
        Some(n) => {
            verify_scale(&img, n)?;
            n
        }
        None => detect_scale(&img),
    };
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

/// Parse one `--map '#rrggbb=C'` and record the pin.
///
/// The target character is restricted to ASCII alphanumerics for two reasons, both of which are
/// silent corruption rather than a visible failure if left unchecked:
///
/// * It is spliced unescaped into generated Rust — as a `b'C'` literal and into the `ART` string
///   literals — so a quote or backslash would emit source that does not compile.
/// * A character already pinned to a *different* colour would put two entries in `PALETTE` under
///   one character, leaving the grid unable to identify a colour uniquely. Re-pinning the same
///   colour to the same character is idempotent and allowed, so repeating a flag is harmless.
fn add_pin(raw: &str, pins: &mut BTreeMap<[u8; 3], u8>) -> Result<(), Err> {
    let (rgb_hex, ch) = raw
        .split_once('=')
        .ok_or_else(|| format!("--map wants '#rrggbb=C', got {raw:?}"))?;
    let ch = ch.as_bytes();
    if ch.len() != 1 || !ch[0].is_ascii_alphanumeric() {
        return Err(format!(
            "--map target must be a single ASCII letter or digit (it is spliced into generated \
             Rust), got {raw:?}"
        )
        .into());
    }
    let rgb = parse_hex(rgb_hex)?;
    if let Some((other, _)) = pins.iter().find(|(c, t)| **t == ch[0] && **c != rgb) {
        return Err(format!(
            "--map target '{}' is already pinned to {}; one character cannot mean two colours",
            ch[0] as char,
            hex(*other)
        )
        .into());
    }
    pins.insert(rgb, ch[0]);
    Ok(())
}

/// The largest `n` for which every `n`×`n` block is a single colour — the art's true pixel size.
///
/// Always succeeds: `n = 1` is trivially uniform, so a resampled or anti-aliased PNG detects as
/// scale 1 rather than failing. That is deliberate (1:1 art is legitimate — `export --scale 1`
/// produces it), but it means auto-detection alone cannot tell pixel art from a photograph. Pass
/// `--scale` to assert the size you drew at and have [`verify_scale`] reject anything else.
fn detect_scale(img: &RgbaImage) -> u32 {
    let (w, h) = img.dimensions();
    (1..=w.min(h))
        .rev()
        .find(|n| w % n == 0 && h % n == 0 && uniform(img, *n))
        .unwrap_or(1)
}

/// Confirm the art really is drawn at `n`×`n` blocks, so a resampled or anti-aliased PNG fails
/// loudly instead of baking into a grid that silently differs from what the author drew.
fn verify_scale(img: &RgbaImage, n: u32) -> Result<(), Err> {
    let (w, h) = img.dimensions();
    if n == 0 {
        return Err("--scale must be at least 1".into());
    }
    if w % n != 0 || h % n != 0 {
        return Err(format!("--scale {n} does not divide the image ({w}x{h})").into());
    }
    if !uniform(img, n) {
        return Err(format!(
            "--scale {n} was given but the image is not uniform at that block size; is the PNG \
             anti-aliased or resampled? (largest uniform block is {})",
            detect_scale(img)
        )
        .into());
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    const BLUE: [u8; 3] = [122, 162, 247];
    const WELL: [u8; 3] = [11, 13, 16];
    const GREY: [u8; 3] = [107, 116, 128];

    /// Build an image by scaling a character grid up by `n`, mapping via `f`.
    fn img(rows: &[&str], n: u32, f: impl Fn(char) -> [u8; 4]) -> RgbaImage {
        let (w, h) = (rows[0].len() as u32 * n, rows.len() as u32 * n);
        let mut im = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                for dy in 0..n {
                    for dx in 0..n {
                        im.put_pixel(x as u32 * n + dx, y as u32 * n + dy, Rgba(f(ch)));
                    }
                }
            }
        }
        im
    }

    fn opaque(c: [u8; 3]) -> [u8; 4] {
        [c[0], c[1], c[2], 255]
    }

    // --- --map validation ---------------------------------------------------------------------

    #[test]
    fn pin_records_a_colour_and_is_idempotent() {
        let mut pins = BTreeMap::new();
        add_pin("#7aa2f7=B", &mut pins).expect("valid pin");
        // Repeating the identical flag must not be an error — re-baking passes the same command.
        add_pin("#7aa2f7=B", &mut pins).expect("repeat of the same pin");
        assert_eq!(pins.get(&BLUE), Some(&b'B'));
        assert_eq!(pins.len(), 1);
    }

    #[test]
    fn pin_rejects_one_character_meaning_two_colours() {
        // Would otherwise emit two PALETTE entries under 'X', leaving the grid unable to identify
        // a colour uniquely — and `bake` would report success.
        let mut pins = BTreeMap::new();
        add_pin("#111111=X", &mut pins).expect("first pin");
        let err = add_pin("#222222=X", &mut pins).expect_err("duplicate target must be refused");
        assert!(err.to_string().contains("already pinned"), "{err}");
        assert_eq!(pins.len(), 1, "the rejected pin must not be recorded");
    }

    #[test]
    fn pin_rejects_characters_that_would_break_generated_rust() {
        // These are spliced unescaped into `b'C'` literals and into the ART string literals.
        for bad in [
            "#ffffff='",
            "#ffffff=\"",
            "#ffffff=\\",
            "#ffffff=.",
            "#ffffff=ab",
            "#ffffff=",
        ] {
            let mut pins = BTreeMap::new();
            assert!(
                add_pin(bad, &mut pins).is_err(),
                "{bad:?} should be refused"
            );
        }
        // A digit is fine: it is alphanumeric and safe to splice.
        let mut pins = BTreeMap::new();
        add_pin("#ffffff=7", &mut pins).expect("digits are valid targets");
    }

    #[test]
    fn pin_rejects_malformed_input() {
        let mut pins = BTreeMap::new();
        assert!(add_pin("no-equals-sign", &mut pins).is_err());
        assert!(add_pin("#12345=B", &mut pins).is_err(), "short hex");
        assert!(add_pin("#gggggg=B", &mut pins).is_err(), "non-hex digits");
    }

    // --- scale detection ----------------------------------------------------------------------

    #[test]
    fn detect_scale_finds_the_true_pixel_size() {
        let im = img(&["AB", "BA"], 8, |c| {
            if c == 'A' { opaque(BLUE) } else { opaque(WELL) }
        });
        assert_eq!(detect_scale(&im), 8);
        verify_scale(&im, 8).expect("8 is the real block size");
    }

    #[test]
    fn detect_scale_cannot_by_itself_reject_art_that_is_not_blocky() {
        // One stray pixel: an anti-aliased or resampled PNG. `n = 1` is trivially uniform, so
        // auto-detection quietly reports scale 1 rather than failing. This is the limitation that
        // `--scale` exists to cover, and why the module documents it explicitly.
        let mut im = img(&["AA", "AA"], 4, |_| opaque(BLUE));
        im.put_pixel(1, 1, Rgba(opaque(GREY)));
        assert_eq!(detect_scale(&im), 1);
    }

    #[test]
    fn verify_scale_rejects_art_not_drawn_at_the_asserted_size() {
        let mut im = img(&["AA", "AA"], 4, |_| opaque(BLUE));
        im.put_pixel(1, 1, Rgba(opaque(GREY)));
        let err = verify_scale(&im, 4).expect_err("not uniform at 4");
        assert!(
            err.to_string().contains("anti-aliased or resampled"),
            "{err}"
        );
        // A scale that does not divide the image is refused before uniformity is considered.
        assert!(verify_scale(&im, 5).is_err());
        assert!(verify_scale(&im, 0).is_err());
    }

    // --- transparency -------------------------------------------------------------------------

    #[test]
    fn flatten_prefers_alpha_over_any_colour_heuristic() {
        // The well colour also sits on the border. Alpha must still be what decides, otherwise
        // the wells get merged into the background — the bug this design exists to prevent.
        let cells = downsample(
            &img(&["..", "KK"], 2, |c| match c {
                'K' => opaque(WELL),
                _ => [0, 0, 0, 0],
            }),
            2,
        );
        let (flat, note) = flatten(cells);
        assert_eq!(note, "alpha");
        assert_eq!(flat[0], vec![None, None]);
        assert_eq!(flat[1], vec![Some(WELL), Some(WELL)], "wells must survive");
    }

    #[test]
    fn flatten_falls_back_to_the_border_colour_when_fully_opaque() {
        let cells = downsample(
            &img(&["GGGG", "GBBG", "GBBG", "GGGG"], 2, |c| {
                if c == 'G' { opaque(GREY) } else { opaque(BLUE) }
            }),
            2,
        );
        let (flat, note) = flatten(cells);
        assert!(note.starts_with("border colour"), "{note}");
        assert_eq!(flat[0], vec![None; 4]);
        assert_eq!(flat[1][1], Some(BLUE));
    }

    // --- palette assignment -------------------------------------------------------------------

    #[test]
    fn assign_honours_pins_and_fills_the_rest_by_frequency() {
        let cells = vec![
            vec![Some(BLUE), Some(BLUE), Some(BLUE)],
            vec![Some(GREY), None, Some(BLUE)],
        ];
        let pins = BTreeMap::from([(GREY, b'D')]);
        let palette = assign(&cells, &pins, &[]).expect("assign");
        assert_eq!(palette.len(), 2);
        // BLUE is the most common, so it takes the first free alphabet character.
        assert_eq!(palette[0].0, b'B');
        assert_eq!(palette[0].1, BLUE);
        // The pinned character is honoured and never handed out to another colour.
        assert!(palette.iter().any(|(c, rgb, _)| *c == b'D' && *rgb == GREY));
    }

    #[test]
    fn assign_infers_cut_outs_from_luminance_but_an_explicit_hole_wins() {
        let cells = vec![vec![Some(BLUE), Some(WELL)]];
        // No --hole: the near-black well falls below the luminance threshold, the shell does not.
        let inferred = assign(&cells, &BTreeMap::new(), &[]).expect("assign");
        let ink = |p: &Vec<(u8, [u8; 3], bool)>, rgb: [u8; 3]| {
            p.iter().find(|(_, c, _)| *c == rgb).expect("present").2
        };
        assert!(ink(&inferred, BLUE), "the shell is ink");
        assert!(!ink(&inferred, WELL), "the near-black well is a cut-out");
        // An explicit --hole replaces the luminance rule outright.
        let explicit = assign(&cells, &BTreeMap::new(), &[BLUE]).expect("assign");
        assert!(!ink(&explicit, BLUE), "explicitly holed");
        assert!(ink(&explicit, WELL), "not in the hole list, so ink");
    }

    #[test]
    fn assign_refuses_more_colours_than_the_alphabet() {
        // One colour past the alphabet: baking would otherwise run out of characters silently.
        let cells: Vec<Vec<Option<[u8; 3]>>> = (0..=ALPHABET.len())
            .map(|i| vec![Some([(i / 256) as u8, (i % 256) as u8, 0])])
            .collect();
        assert!(assign(&cells, &BTreeMap::new(), &[]).is_err());
    }

    // --- emitted source -----------------------------------------------------------------------

    #[test]
    fn emit_writes_the_grid_and_palette_it_was_given() {
        let cells = vec![vec![Some(BLUE), None], vec![Some(WELL), Some(BLUE)]];
        let palette = vec![(b'B', BLUE, true), (b'K', WELL, false)];
        let src = emit(Path::new("mark.png"), 8, 2, 2, &cells, "alpha", &palette).expect("emit");

        assert!(src.contains("@generated"), "must be marked generated");
        assert!(src.contains("pub const ART_W: usize = 2;"));
        assert!(src.contains("pub const PALETTE: [(u8, [u8; 3], bool); 2] = ["));
        assert!(src.contains("(b'B', [122, 162, 247], true)"));
        assert!(src.contains("(b'K', [11, 13, 16], false)"));
        // Transparent cells become `.`; every other cell takes its palette character.
        assert!(src.contains("\"B.\","), "row 0: {src}");
        assert!(src.contains("\"KB\","), "row 1: {src}");
        // The header records the re-bake command, including the hole we passed.
        assert!(src.contains("--map '#7aa2f7=B'"));
        assert!(src.contains("--hole '#0b0d10'"));
        // `#[rustfmt::skip]` keeps the grid from being reflowed into unreadability.
        assert!(src.contains("#[rustfmt::skip]"));
    }

    #[test]
    fn emit_aligns_trailing_comments_so_re_baking_leaves_the_tree_formatted() {
        // rustfmt aligns trailing comments in an array literal. If the generator did not match
        // that, every re-bake would dirty the tree and break `cargo fmt --check`.
        let cells = vec![vec![Some(BLUE)], vec![Some(WELL)]];
        let palette = vec![(b'B', BLUE, true), (b'K', WELL, false)];
        let src = emit(Path::new("m.png"), 1, 1, 2, &cells, "alpha", &palette).expect("emit");
        let cols: Vec<usize> = src
            .lines()
            .filter(|l| l.trim_start().starts_with("(b'"))
            .map(|l| {
                l.find("//")
                    .expect("each palette line has a trailing comment")
            })
            .collect();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], cols[1], "comment columns must line up");
    }

    // --- round trip ---------------------------------------------------------------------------

    #[test]
    fn png_round_trips_through_the_whole_pipeline() {
        // The property the PNG-as-source-of-truth workflow rests on: scale detection, alpha,
        // palette assignment and emission together reproduce the grid that was drawn.
        let rows = ["B.KB", "KBBK", "..BB", "BKKB"];
        let im = img(&rows, 8, |c| match c {
            'B' => opaque(BLUE),
            'K' => opaque(WELL),
            _ => [0, 0, 0, 0],
        });
        let scale = detect_scale(&im);
        assert_eq!(scale, 8);
        verify_scale(&im, scale).expect("drawn at 8x8 blocks");
        let (cells, note) = flatten(downsample(&im, scale));
        assert_eq!(note, "alpha");
        let pins = BTreeMap::from([(BLUE, b'B'), (WELL, b'K')]);
        let palette = assign(&cells, &pins, &[WELL]).expect("assign");
        let src = emit(Path::new("m.png"), scale, 4, 4, &cells, note, &palette).expect("emit");
        for row in rows {
            assert!(
                src.contains(&format!("\"{row}\",")),
                "lost row {row}:\n{src}"
            );
        }
    }
}
