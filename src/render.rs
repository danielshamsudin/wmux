//! Off-screen cell grid with a diff against the last frame.
//! We only send the cells that changed. This keeps redraws fast and stops flicker.

use std::fmt::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for Color {
    fn from(c: vt100::Color) -> Self {
        match c {
            vt100::Color::Default => Color::Default,
            vt100::Color::Idx(i) => Color::Idx(i),
            vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDERLINE: u8 = 8;
pub const INVERSE: u8 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: u8,
}

impl Style {
    pub const DEFAULT: Style = Style { fg: Color::Default, bg: Color::Default, attrs: 0 };
    pub fn fg(c: Color) -> Style {
        Style { fg: c, ..Style::DEFAULT }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    /// Text in the cell. Empty = second half of a wide character.
    pub ch: String,
    pub style: Style,
    pub wide: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: " ".into(), style: Style::DEFAULT, wide: false }
    }
}

#[derive(Clone)]
pub struct Grid {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
}

impl Grid {
    pub fn new(w: u16, h: u16) -> Grid {
        Grid { w, h, cells: vec![Cell::default(); w as usize * h as usize] }
    }

    pub fn clear(&mut self) {
        for c in &mut self.cells {
            c.ch.clear();
            c.ch.push(' ');
            c.style = Style::DEFAULT;
            c.wide = false;
        }
    }

    pub fn set(&mut self, x: u16, y: u16, ch: &str, style: Style, wide: bool) {
        if x >= self.w || y >= self.h {
            return;
        }
        let c = &mut self.cells[y as usize * self.w as usize + x as usize];
        c.ch.clear();
        c.ch.push_str(ch);
        c.style = style;
        c.wide = wide;
    }

    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.w || y >= self.h {
            return None;
        }
        Some(&self.cells[y as usize * self.w as usize + x as usize])
    }

    /// Write a string from (x, y). Returns the next column.
    pub fn text(&mut self, mut x: u16, y: u16, s: &str, style: Style) -> u16 {
        for ch in s.chars() {
            let mut b = [0u8; 4];
            self.set(x, y, ch.encode_utf8(&mut b), style, false);
            x += 1;
        }
        x
    }
}

/// Diff state for one client: the last frame that client received.
#[derive(Default)]
pub struct Renderer {
    prev: Option<Grid>,
}

fn sgr_color(out: &mut String, c: Color, base: u8) {
    match c {
        Color::Default => {}
        Color::Idx(i) if i < 8 => { let _ = write!(out, ";{}", base + i); }
        Color::Idx(i) if i < 16 => { let _ = write!(out, ";{}", base + 60 + i - 8); }
        Color::Idx(i) => { let _ = write!(out, ";{};5;{}", base + 8, i); }
        Color::Rgb(r, g, b) => { let _ = write!(out, ";{};2;{};{};{}", base + 8, r, g, b); }
    }
}

fn sgr(out: &mut String, s: Style) {
    out.push_str("\x1b[0");
    for (bit, code) in [(BOLD, "1"), (DIM, "2"), (ITALIC, "3"), (UNDERLINE, "4"), (INVERSE, "7")] {
        if s.attrs & bit != 0 {
            out.push(';');
            out.push_str(code);
        }
    }
    sgr_color(out, s.fg, 30);
    sgr_color(out, s.bg, 40);
    out.push('m');
}

impl Renderer {
    /// Forget the last frame. The next flush redraws everything.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Build the escape sequence string that moves the real terminal from the
    /// last frame to the current frame.
    pub fn flush(&mut self, grid: &Grid, cursor: Option<(u16, u16)>, cursor_shape: u8) -> String {
        if self.prev.as_ref().is_some_and(|p| p.w != grid.w || p.h != grid.h) {
            self.prev = None;
        }
        let mut out = String::with_capacity(4096);
        out.push_str("\x1b[?2026h\x1b[?25l"); // begin synchronized update, hide cursor
        let full = self.prev.is_none();
        if full {
            out.push_str("\x1b[0m\x1b[2J");
        }
        let mut pos: Option<(u16, u16)> = None;
        let mut cur_style: Option<Style> = None;
        for y in 0..grid.h {
            let mut x = 0;
            while x < grid.w {
                let cell = grid.get(x, y).unwrap();
                let changed = full || self.prev.as_ref().and_then(|p| p.get(x, y)) != Some(cell);
                if !changed || cell.ch.is_empty() {
                    x += 1;
                    continue;
                }
                if pos != Some((x, y)) {
                    // For a short gap on the same row, print the unchanged cells
                    // again. That is shorter than a cursor move.
                    let short_gap = match pos {
                        Some((px, py)) if py == y && px < x && x - px <= 4 => (px..x).all(|i| {
                            let c = grid.get(i, y).unwrap();
                            !c.ch.is_empty() && !c.wide
                        }),
                        _ => false,
                    };
                    if short_gap {
                        let px = pos.unwrap().0;
                        for i in px..x {
                            let c = grid.get(i, y).unwrap();
                            if cur_style != Some(c.style) {
                                sgr(&mut out, c.style);
                                cur_style = Some(c.style);
                            }
                            out.push_str(&c.ch);
                        }
                    } else {
                        let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                    }
                }
                if cur_style != Some(cell.style) {
                    sgr(&mut out, cell.style);
                    cur_style = Some(cell.style);
                }
                out.push_str(&cell.ch);
                let adv = if cell.wide { 2 } else { 1 };
                pos = Some((x + adv, y));
                x += 1;
            }
        }
        out.push_str("\x1b[0m");
        if let Some((cx, cy)) = cursor {
            let _ = write!(out, "\x1b[{} q\x1b[{};{}H\x1b[?25h", cursor_shape, cy + 1, cx + 1);
        }
        out.push_str("\x1b[?2026l");
        // Keep this frame as `prev`. clone_from reuses the String buffers.
        match &mut self.prev {
            Some(p) => p.cells.clone_from(&grid.cells),
            None => self.prev = Some(grid.clone()),
        }
        out
    }
}
