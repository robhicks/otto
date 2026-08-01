//! otto's mark, rendered for a tty.
//!
//! The art is a pixel grid drawn with half-block glyphs: each terminal cell carries two stacked
//! pixels (`▀` foreground over background), so pixels come out square on a terminal whose cells
//! are twice as tall as they are wide. 50×36 pixels becomes 50 columns by 18 rows.
//!
//! What it depicts, deliberately: an orchestrator head whose visor is lit with code, flanked by
//! silicon wired into it, mounted on a platform whose front panel runs the four-agent spine —
//! Planner, ContextFinder, Coder, Verifier — as packages on a bus.
//!
//! Colour degrades in three steps — 24-bit → xterm-256 → uncoloured blocks — so the mark survives
//! a pipe, a dumb `TERM`, and `NO_COLOR` alike.
//!
//! The pixels themselves live in [`art`], which is generated from
//! `crates/engine/assets/otto-mark.png`. Edit the PNG, not the grid — see that module's header for
//! the re-bake command.

mod art;

use std::io::IsTerminal;

pub use art::{ART, ART_W, PALETTE};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rgb(u8, u8, u8);

impl Rgb {
    /// Nearest colour in the xterm-256 6×6×6 cube.
    fn to_ansi256(self) -> u8 {
        let q = |c: u8| ((c as u16 * 5 + 127) / 255) as u8;
        16 + 36 * q(self.0) + 6 * q(self.1) + q(self.2)
    }
}

/// The colour a grid character paints, or `None` if it paints nothing.
fn color(ch: u8) -> Option<Rgb> {
    PALETTE
        .iter()
        .find(|(c, _, _)| *c == ch)
        .map(|(_, [r, g, b], _)| Rgb(*r, *g, *b))
}

/// Whether a grid character is drawn at all once colour is gone.
///
/// `Mono` has no way to tell the screen wells apart from the shell around them, so the palette
/// marks them as cut-outs instead: the visor and the front panel become holes, and the eyes, code
/// and spine nodes inside them stay solid. The machine still reads as a machine.
fn is_ink(ch: u8) -> bool {
    PALETTE
        .iter()
        .find(|(c, _, _)| *c == ch)
        .is_some_and(|(_, _, ink)| *ink)
}

/// How much colour the destination terminal can take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    /// Uncoloured block glyphs.
    Mono,
}

impl ColorMode {
    /// Pick a mode from the environment. Not a terminal, or `NO_COLOR` set, means no escapes at
    /// all; `COLORTERM` advertising 24-bit unlocks truecolor; otherwise assume the 256-colour cube.
    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some() || !std::io::stderr().is_terminal() {
            return Self::Mono;
        }
        match std::env::var("COLORTERM") {
            Ok(v) if v.contains("truecolor") || v.contains("24bit") => Self::TrueColor,
            _ => Self::Ansi256,
        }
    }

    fn fg(self, c: Rgb) -> String {
        match self {
            Self::TrueColor => format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2),
            Self::Ansi256 => format!("\x1b[38;5;{}m", c.to_ansi256()),
            Self::Mono => String::new(),
        }
    }

    fn bg(self, c: Rgb) -> String {
        match self {
            Self::TrueColor => format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2),
            Self::Ansi256 => format!("\x1b[48;5;{}m", c.to_ansi256()),
            Self::Mono => String::new(),
        }
    }

    fn reset(self) -> &'static str {
        match self {
            Self::Mono => "",
            _ => "\x1b[0m",
        }
    }
}

/// One pixel: its colour, or `None` where nothing is drawn.
fn pixel(row: &str, x: usize, mode: ColorMode) -> Option<Rgb> {
    let ch = *row.as_bytes().get(x)?;
    if mode == ColorMode::Mono {
        return is_ink(ch).then_some(Rgb(0, 0, 0));
    }
    color(ch)
}

/// Render one terminal row from a pair of pixel rows.
fn row(top: &str, bottom: &str, mode: ColorMode) -> String {
    let mut out = String::new();
    for x in 0..ART_W {
        match (pixel(top, x, mode), pixel(bottom, x, mode)) {
            (None, None) => out.push(' '),
            (Some(t), None) => {
                out.push_str(mode.reset());
                out.push_str(&mode.fg(t));
                out.push('▀');
            }
            (None, Some(b)) => {
                out.push_str(mode.reset());
                out.push_str(&mode.fg(b));
                out.push('▄');
            }
            // Mono has no background to set, so a full cell is just a solid block.
            (Some(_), Some(_)) if mode == ColorMode::Mono => out.push('█'),
            (Some(t), Some(b)) => {
                out.push_str(&mode.fg(t));
                out.push_str(&mode.bg(b));
                out.push('▀');
            }
        }
    }
    out.push_str(mode.reset());
    out
}

/// The mark alone, one string per terminal row, each padded to the full grid width so a caller
/// laying it out in a column gets predictable geometry.
pub fn art(mode: ColorMode) -> Vec<String> {
    ART.chunks(2)
        .map(|pair| row(pair[0], pair[1], mode))
        .collect()
}

/// Centre `text` (of `visible` printed columns) under the art.
fn centered(text: &str, visible: usize) -> String {
    format!("{}{text}", " ".repeat(ART_W.saturating_sub(visible) / 2))
}

/// The mark with the wordmark set beneath it — what `otto` prints.
pub fn banner(mode: ColorMode) -> String {
    const NAME: &str = "otto";
    const TAGLINE: &str = "agentic coding engine";

    let accent = color(b'B').expect("B is in the palette");
    let dim = color(b'D').expect("D is in the palette");
    let (bold, faint) = match mode {
        ColorMode::Mono => ("", ""),
        _ => ("\x1b[1m", "\x1b[2m"),
    };

    let mut lines: Vec<String> = art(mode)
        .into_iter()
        .map(|r| r.trim_end().to_string())
        .collect();
    lines.push(String::new());
    lines.push(centered(
        &format!("{bold}{}{NAME}{}", mode.fg(accent), mode.reset()),
        NAME.len(),
    ));
    lines.push(centered(
        &format!("{faint}{}{TAGLINE}{}", mode.fg(dim), mode.reset()),
        TAGLINE.len(),
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminal rows the art occupies: two pixel rows per row.
    const ART_ROWS: usize = ART.len() / 2;

    #[test]
    fn art_grid_is_rectangular_and_pairs_evenly() {
        // Half-block rendering consumes pixel rows two at a time, so an odd row count would
        // silently drop the last one.
        assert_eq!(ART.len() % 2, 0, "art needs an even number of rows");
        for (i, r) in ART.iter().enumerate() {
            assert_eq!(r.len(), ART_W, "row {i} is not {ART_W} wide");
            assert!(
                r.bytes().all(|b| b == b'.' || color(b).is_some()),
                "row {i} uses a character outside the palette"
            );
        }
    }

    #[test]
    fn palette_has_no_duplicate_characters_or_colours() {
        // A duplicate would make the baked grid ambiguous, and would mean the generator's
        // character assignment collided.
        for (i, (ch, rgb, _)) in PALETTE.iter().enumerate() {
            for (other, orgb, _) in &PALETTE[i + 1..] {
                assert_ne!(ch, other, "duplicate grid character {:?}", *ch as char);
                assert_ne!(rgb, orgb, "duplicate colour {rgb:?}");
            }
        }
        // `.` is transparent and must never be given a colour.
        assert!(color(b'.').is_none());
        assert!(!is_ink(b'.'));
    }

    #[test]
    fn every_mode_renders_one_row_per_pixel_pair() {
        for mode in [ColorMode::TrueColor, ColorMode::Ansi256, ColorMode::Mono] {
            let rows = art(mode);
            assert_eq!(rows.len(), ART_ROWS, "{mode:?}");
            if mode == ColorMode::Mono {
                for (i, r) in rows.iter().enumerate() {
                    assert_eq!(r.chars().count(), ART_W, "row {i} is not {ART_W} wide");
                }
            }
        }
    }

    #[test]
    fn mono_emits_no_escape_sequences() {
        // The whole point of Mono is that it is safe to pipe into a file or a dumb terminal.
        let out = banner(ColorMode::Mono);
        assert!(
            !out.contains('\x1b'),
            "mono banner leaked an escape: {out:?}"
        );
    }

    #[test]
    fn mono_silhouette_reads_as_the_machine() {
        let rows: Vec<String> = art(ColorMode::Mono)
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        // The beacon on top of the antenna.
        assert_eq!(rows[0], " ".repeat(23) + "████");
        // The visor: shell posts either side, two eyes cut into the dark screen.
        assert_eq!(rows[5], "    ▄█▄▄▄█▄  ███  █████    █████  ███   ▄█▄▄▄█▄");
        // The front panel: four spine packages sitting in the well.
        assert_eq!(rows[14], "  ███████   █▀█    █▀█    █▀█    █▀█     ███████");
    }

    #[test]
    fn truecolor_uses_the_ui_palette() {
        let out = banner(ColorMode::TrueColor);
        assert!(out.contains("38;2;122;162;247"), "missing --accent shell");
        assert!(out.contains("38;2;125;207;255"), "missing cyan eyes");
        // The four spine stages each carry their own colour.
        for c in *b"CVGA" {
            let Rgb(r, g, b) = color(c).unwrap();
            assert!(out.contains(&format!("{r};{g};{b}")), "spine {c} missing");
        }
    }

    #[test]
    fn ansi256_quantises_into_the_colour_cube() {
        // 16..=231 is the 6x6x6 cube; anything outside it means the quantiser is broken.
        for (ch, _, _) in PALETTE {
            let c = color(ch).unwrap();
            let i = c.to_ansi256();
            assert!((16..=231).contains(&i), "{c:?} quantised out of range: {i}");
        }
        assert!(banner(ColorMode::Ansi256).contains("38;5;"));
    }

    #[test]
    fn banner_sets_the_wordmark_centred_under_the_art() {
        let out = banner(ColorMode::Mono);
        let lines: Vec<&str> = out.lines().collect();
        // Art, a blank spacer, then the two wordmark lines.
        assert_eq!(lines.len(), ART_ROWS + 3);
        assert_eq!(lines[ART_ROWS], "");
        assert_eq!(lines[ART_ROWS + 1], " ".repeat(23) + "otto");
        assert_eq!(
            lines[ART_ROWS + 2],
            " ".repeat(14) + "agentic coding engine"
        );
        // Nothing overflows the art's own width.
        for (i, l) in lines.iter().enumerate() {
            assert!(l.chars().count() <= ART_W, "line {i} overflows: {l:?}");
        }
    }
}
