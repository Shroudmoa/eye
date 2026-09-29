//! A virtual terminal screen.
//!
//! `eye` never needs this to record a session — the raw PTY byte stream is the
//! recording, and replaying it reproduces the session exactly. The screen
//! exists so that the *other* views have something structured to work from:
//! HTML export, "what did the screen actually say" inspection, search over
//! visible text, and (later) grid-rendered replay.
//!
//! It implements the common core of xterm: scrolling regions, insert/delete
//! line and character, erase, the alternate screen buffer, tab stops and SGR.

use super::attrs::{Attrs, Cell, Color};
use super::parser::{Action, Params, Parser};
use crate::event::Size;

/// A cursor position. Zero-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
}

/// Which buffer is currently displayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Buffer {
    Primary,
    Alternate,
}

/// A shell-integration mark, from OSC 133.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellMark {
    /// `A` prompt start, `B` command start, `C` command output,
    /// `D` prompt end (with exit status).
    pub kind: char,
    /// Exit status carried by a `D` mark.
    pub status: Option<i32>,
}

/// Terminal screen state fed by a byte stream.
pub struct Screen {
    cols: u16,
    rows: u16,
    primary: Vec<Cell>,
    alternate: Vec<Cell>,
    buffer: Buffer,
    cursor: Cursor,
    saved: Cursor,
    /// Cursor and attributes saved by `ESC 7` / `DECSC`.
    saved_full: SavedState,
    fg: Color,
    bg: Color,
    attrs: Attrs,
    /// Scroll region, inclusive, zero-based.
    scroll_top: u16,
    scroll_bottom: u16,
    tabs: Vec<bool>,
    /// Whether the cursor is inside the scroll region.
    wrap_pending: bool,
    autowrap: bool,
    origin_mode: bool,
    /// DECTCEM: whether the cursor is currently drawn.
    cursor_visible: bool,
    /// Did the terminal alternate buffers at least once?
    pub used_alt_screen: bool,
    /// Window / icon title last set through OSC 0 or 2.
    pub title: Option<String>,
    /// OSC 133 shell marks in arrival order.
    pub shell_marks: Vec<ShellMark>,
    /// Reply bytes owed to the child (device status reports).
    pub replies: Vec<u8>,
    /// Cursor positions where the cursor was drawn, for later inspection.
    parser: Parser,
}

/// What `ESC 7` / `DECSC` saves and `ESC 8` / `DECRC` restores.
///
/// The cursor is kept separately in [`Screen::saved`] because restore has to
/// clamp it to the current screen size, which a plain copy cannot do.
#[derive(Clone, Copy, Debug, Default)]
struct SavedState {
    fg: Color,
    bg: Color,
    attrs: Attrs,
    origin_mode: bool,
}

impl Screen {
    /// A blank screen of the given size.
    pub fn new(size: Size) -> Self {
        let cells = Cell::default();
        Screen {
            cols: size.cols.max(1),
            rows: size.rows.max(1),
            primary: vec![cells; (size.cols.max(1) as usize) * (size.rows.max(1) as usize)],
            alternate: Vec::new(),
            buffer: Buffer::Primary,
            cursor: Cursor { row: 0, col: 0 },
            saved: Cursor { row: 0, col: 0 },
            saved_full: SavedState::default(),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            tabs: vec![false; size.cols.max(1) as usize],
            wrap_pending: false,
            autowrap: true,
            origin_mode: false,
            cursor_visible: true,
            used_alt_screen: false,
            title: None,
            shell_marks: Vec::new(),
            replies: Vec::new(),
            parser: Parser::new(),
        }
        .with_default_tabs()
    }

    fn with_default_tabs(mut self) -> Self {
        self.reset_tabs();
        self
    }

    pub fn size(&self) -> Size {
        Size::new(self.cols, self.rows)
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// True while the alternate screen buffer is the one being shown.
    ///
    /// Replay uses this to know that it must switch back before handing the
    /// terminal to the user: a session that ended inside `vim` would
    /// otherwise leave them staring at an alternate screen with no shell.
    pub fn in_alt_screen(&self) -> bool {
        matches!(self.buffer, Buffer::Alternate)
    }

    /// Whether the cursor is currently visible.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Current foreground, background and style.
    pub fn pen(&self) -> (Color, Color, Attrs) {
        (self.fg, self.bg, self.attrs)
    }

    fn len(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    fn grid(&self) -> &Vec<Cell> {
        match self.buffer {
            Buffer::Primary => &self.primary,
            Buffer::Alternate => &self.alternate,
        }
    }

    fn grid_mut(&mut self) -> &mut Vec<Cell> {
        match self.buffer {
            Buffer::Primary => &mut self.primary,
            Buffer::Alternate => &mut self.alternate,
        }
    }

    /// Feed raw terminal output.
    pub fn feed(&mut self, bytes: &[u8]) {
        for action in self.parser.feed(bytes) {
            self.apply(action);
        }
    }

    /// Cell at a position, if it exists.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.grid().get(row as usize * self.cols as usize + col as usize)
    }

    /// The whole screen as text rows, trailing blanks trimmed.
    pub fn text_rows(&self) -> Vec<String> {
        (0..self.rows)
            .map(|r| {
                let mut s = String::new();
                for c in 0..self.cols {
                    let cell = self.cell(r, c).copied().unwrap_or_default();
                    // A zero-width cell is the tail of a double-width glyph and
                    // must not add a character of its own.
                    if cell.width != 0 {
                        s.push(cell.ch);
                    }
                }
                s.trim_end().to_owned()
            })
            .collect()
    }

    /// The whole screen as one newline-separated string.
    pub fn text(&self) -> String {
        self.text_rows().join("\n")
    }

    /// A copy of one row of cells.
    pub fn row_cells(&self, row: u16) -> Vec<Cell> {
        (0..self.cols).map(|c| self.cell(row, c).copied().unwrap_or_default()).collect()
    }

    /// Take any replies owed to the child (e.g. cursor position reports).
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    // -- geometry ---------------------------------------------------------

    /// Resize, preserving content where it overlaps. Programs that care about
    /// size re-read `TIOCGWINSZ`, and get it from the real terminal, so this
    /// only needs to be reasonable, not exact.
    pub fn resize(&mut self, size: Size) {
        let cols = size.cols.max(1);
        let rows = size.rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        for buffer in [&mut self.primary, &mut self.alternate] {
            if buffer.is_empty() {
                continue;
            }
            let old_cols = self.cols as usize;
            let old = std::mem::take(buffer);
            let mut next = vec![Cell::default(); cols as usize * rows as usize];
            let old_rows = old.len() / old_cols.max(1);
            for row in 0..(rows as usize).min(old_rows) {
                for col in 0..cols as usize {
                    if let Some(cell) = old.get(row * old_cols + col) {
                        next[row * cols as usize + col] = *cell;
                    }
                }
            }
            *buffer = next;
        }
        self.alternate = if self.alternate.is_empty() && self.buffer == Buffer::Primary {
            Vec::new()
        } else {
            vec![Cell::default(); cols as usize * rows as usize]
        };
        self.cols = cols;
        self.rows = rows;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.tabs = vec![false; cols as usize];
        self.reset_tabs();
    }

    fn reset_tabs(&mut self) {
        for (i, slot) in self.tabs.iter_mut().enumerate() {
            *slot = i > 0 && i % 8 == 0;
        }
    }

    // -- actions ----------------------------------------------------------

    fn apply(&mut self, action: Action) {
        match action {
            Action::Print(c) => self.print(c),
            Action::Cr => {
                self.cursor.col = 0;
                self.wrap_pending = false;
            }
            Action::Lf => self.line_feed(),
            Action::ReverseIndex => {
                if self.cursor.row == self.scroll_top {
                    self.scroll_down(1);
                } else {
                    self.cursor.row = self.cursor.row.saturating_sub(1);
                }
            }
            Action::NextLine => {
                self.cursor.col = 0;
                self.line_feed();
            }
            Action::Tab(n) => self.tab(n),
            Action::BackTab(n) => {
                for _ in 0..n.max(1) {
                    let mut col = self.cursor.col;
                    while col > 0 && !self.tab_at(col - 1) {
                        col -= 1;
                    }
                    self.cursor.col = col.saturating_sub(1);
                }
                self.wrap_pending = false;
            }
            Action::Backspace => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.wrap_pending = false;
            }
            Action::Bell | Action::VTab | Action::Ignored | Action::CharsetDesignate => {}
            Action::Esc(b) => self.esc(b),
            Action::Csi(p) => self.csi(&p),
            Action::Osc { code, data } => self.osc(code, &data),
            Action::DeviceReport(params) => self.device_report(&params),
        }
    }

    fn tab_at(&self, col: u16) -> bool {
        self.tabs.get(col as usize).copied().unwrap_or(false)
    }

    fn tab(&mut self, n: u16) {
        for _ in 0..n.max(1) {
            let mut col = self.cursor.col;
            loop {
                col += 1;
                if col >= self.cols {
                    self.cursor.col = self.cols - 1;
                    return;
                }
                if self.tab_at(col) {
                    break;
                }
            }
            self.cursor.col = col;
        }
        self.wrap_pending = false;
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    fn scroll_up(&mut self, n: u16) {
        if self.scroll_bottom < self.scroll_top {
            return;
        }
        let region = (self.scroll_bottom - self.scroll_top + 1) as usize;
        let n = (n as usize).min(region);
        let cols = self.cols as usize;
        let top = self.scroll_top as usize;
        let blank = Cell::default();
        let grid = self.grid_mut();
        let start = top * cols;
        let end = start + region * cols;
        if n < region {
            grid.copy_within(start + n * cols..end, start);
        }
        for cell in &mut grid[end - n * cols..end] {
            *cell = blank;
        }
    }

    fn scroll_down(&mut self, n: u16) {
        if self.scroll_bottom < self.scroll_top {
            return;
        }
        let region = (self.scroll_bottom - self.scroll_top + 1) as usize;
        let n = (n as usize).min(region);
        let cols = self.cols as usize;
        let top = self.scroll_top as usize;
        let blank = Cell::default();
        let grid = self.grid_mut();
        let start = top * cols;
        let end = start + region * cols;
        grid.copy_within(start..end - n * cols, start + n * cols);
        for cell in &mut grid[start..start + n * cols] {
            *cell = blank;
        }
    }

    fn print(&mut self, c: char) {
        if self.wrap_pending {
            self.cursor.col = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        if c == '\u{0}' {
            return;
        }
        let width = crate::term::char_width(c);
        if width == 0 {
            return;
        }
        if self.cursor.col + width as u16 > self.cols {
            if self.autowrap {
                self.cursor.col = 0;
                self.line_feed();
            } else {
                self.cursor.col = self.cols.saturating_sub(width as u16);
            }
        }
        let cell = Cell { ch: c, fg: self.fg, bg: self.bg, attrs: self.attrs, width };
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        let row = self.cursor.row as usize;
        let col = self.cursor.col as usize;
        let grid = self.grid_mut();
        if row < rows && col < cols {
            grid[row * cols + col] = cell;
            if width == 2 && col + 1 < cols {
                grid[row * cols + col + 1] = Cell { ch: ' ', fg: cell.fg, bg: cell.bg, attrs: cell.attrs, width: 0 };
            }
        }
        self.cursor.col += width as u16;
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols - 1;
            self.wrap_pending = self.autowrap;
        }
    }

    fn esc(&mut self, b: u8) {
        match b {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.line_feed(),
            b'E' => {
                self.cursor.col = 0;
                self.line_feed();
            }
            b'M' => {
                if self.cursor.row == self.scroll_top {
                    self.scroll_down(1);
                } else {
                    self.cursor.row = self.cursor.row.saturating_sub(1);
                }
            }
            b'c' => {
                let size = self.size();
                *self = Screen::new(size);
            }
            b'H' => {
                if let Some(col) = self.tabs.iter().position(|t| *t) {
                    self.tabs[col] = false;
                }
            }
            _ => {}
        }
    }

    fn save_cursor(&mut self) {
        self.saved = self.cursor;
        self.saved_full = SavedState {
            fg: self.fg,
            bg: self.bg,
            attrs: self.attrs,
            origin_mode: self.origin_mode,
        };
    }

    fn restore_cursor(&mut self) {
        self.cursor = self.saved;
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
        let saved = self.saved_full;
        self.fg = saved.fg;
        self.bg = saved.bg;
        self.attrs = saved.attrs;
        self.origin_mode = saved.origin_mode;
    }

    fn move_to(&mut self, row: u16, col: u16) {
        let (top, bottom) = if self.origin_mode {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.rows - 1)
        };
        let row = (top + row).min(bottom);
        self.cursor.row = row;
        self.cursor.col = col.min(self.cols.saturating_sub(1));
        self.wrap_pending = false;
    }

    fn csi(&mut self, p: &Params) {
        let f = p.final_byte;
        let n = p.get_raw(0, 1);
        match (p.private, f) {
            (Some(b'?'), b'h') => self.set_modes(p, true),
            (Some(b'?'), b'l') => self.set_modes(p, false),
            (Some(b'?'), b'J') | (Some(b'?'), b'K') => {
                // DEC selective clear: treat as the ANSI form.
                self.csi(&Params {
                    private: None,
                    intermediates: p.intermediates.clone(),
                    groups: p.groups.clone(),
                    final_byte: f,
                });
            }
            (Some(b'>'), _) | (Some(b'='), _) | (Some(b'<'), _) => {}
            _ => match f {
                b'@' => self.insert_chars(n),
                b'A' => {
                    self.cursor.row = self.cursor.row.saturating_sub(n.max(1));
                    self.wrap_pending = false;
                }
                b'B' => {
                    let limit = self.rows - 1;
                    self.cursor.row = (self.cursor.row + n.max(1)).min(limit);
                    self.wrap_pending = false;
                }
                b'C' => {
                    let limit = self.cols - 1;
                    self.cursor.col = (self.cursor.col + n.max(1)).min(limit);
                    self.wrap_pending = false;
                }
                b'D' => {
                    self.cursor.col = self.cursor.col.saturating_sub(n.max(1));
                    self.wrap_pending = false;
                }
                b'E' => {
                    let limit = self.rows - 1;
                    self.cursor.row = (self.cursor.row + n.max(1)).min(limit);
                    self.cursor.col = 0;
                    self.wrap_pending = false;
                }
                b'F' => {
                    self.cursor.row = self.cursor.row.saturating_sub(n.max(1));
                    self.cursor.col = 0;
                    self.wrap_pending = false;
                }
                b'G' | b'`' => {
                    self.cursor.col = n.max(1).saturating_sub(1).min(self.cols - 1);
                    self.wrap_pending = false;
                }
                b'H' | b'f' => {
                    let row = p.get(0, 1).saturating_sub(1);
                    let col = p.get(1, 1).saturating_sub(1);
                    self.move_to(row, col);
                }
                b'I' => {
                    for _ in 0..n.max(1) {
                        let mut col = self.cursor.col;
                        loop {
                            col += 1;
                            if col >= self.cols || self.tab_at(col) {
                                break;
                            }
                        }
                        self.cursor.col = col.min(self.cols - 1);
                    }
                    self.wrap_pending = false;
                }
                b'J' => self.erase_in_display(p.get_raw(0, 0)),
                b'K' => self.erase_in_line(p.get_raw(0, 0)),
                b'L' => self.insert_lines(n),
                b'M' => self.delete_lines(n),
                b'P' => self.delete_chars(n),
                b'S' => self.scroll_up(n),
                b'T' => self.scroll_down(n),
                b'X' => self.erase_chars(n),
                b'Z' => self.apply(Action::BackTab(n)),
                b'b' => self.repeat_last(n),
                b'd' => {
                    let row = p.get(0, 1).saturating_sub(1);
                    self.move_to(row, self.cursor.col);
                }
                b'm' => self.sgr(p),
                b'r' => {
                    let top = p.get(0, 1).saturating_sub(1);
                    let bottom = p.get(1, self.rows).saturating_sub(1);
                    if top < bottom && bottom < self.rows {
                        self.scroll_top = top;
                        self.scroll_bottom = bottom;
                    }
                    self.move_to(0, 0);
                }
                b's' => self.save_cursor(),
                b'u' => self.restore_cursor(),
                _ => {}
            },
        }
    }

    fn set_modes(&mut self, p: &Params, enable: bool) {
        for group in &p.groups {
            for mode in group {
                match *mode {
                    6 => self.origin_mode = enable,
                    7 => self.autowrap = enable,
                    // 12 and 1048 only blink or save the cursor.
                    12 | 1048 => {}
                    // 25 is DECTCEM: whether the cursor is drawn. It has no
                    // effect on the grid contents, but a replay that left the
                    // cursor hidden would need to know about it.
                    25 => self.cursor_visible = enable,
                    // Alternate screen buffer, xterm and linux variants.
                    47 | 1047 => self.switch_buffer(Buffer::Alternate, enable, false),
                    1049 => self.switch_buffer(Buffer::Alternate, enable, true),
                    _ => {}
                }
            }
        }
    }

    fn switch_buffer(&mut self, target: Buffer, enable: bool, save_restore: bool) {
        let want = if enable { target } else { Buffer::Primary };
        if want == self.buffer {
            return;
        }
        if self.alternate.is_empty() {
            self.alternate = vec![Cell::default(); self.len()];
        }
        if want == Buffer::Alternate {
            if save_restore {
                self.save_cursor();
            }
            self.alternate.iter_mut().for_each(|c| *c = Cell::default());
            self.used_alt_screen = true;
            self.buffer = Buffer::Alternate;
            self.move_to(0, 0);
        } else {
            if save_restore {
                self.restore_cursor();
            }
            self.buffer = Buffer::Primary;
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let cols = self.cols as usize;
        let blank = Cell::default();
        let grid = self.grid_mut();
        match mode {
            0 => {
                let start = row as usize * cols + col as usize;
                for cell in &mut grid[start..] {
                    *cell = blank;
                }
            }
            1 => {
                let end = (row as usize * cols + (col as usize + 1).min(cols)).min(grid.len());
                for cell in &mut grid[..end] {
                    *cell = blank;
                }
            }
            2 | 3 => grid.iter_mut().for_each(|c| *c = blank),
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let cols = self.cols as usize;
        let blank = Cell::default();
        let grid = self.grid_mut();
        let base = row as usize * cols;
        if base >= grid.len() {
            return;
        }
        let (from, to) = match mode {
            0 => (col as usize, cols),
            1 => (0, (col as usize + 1).min(cols)),
            2 => (0, cols),
            _ => return,
        };
        for cell in &mut grid[base + from..base + to] {
            *cell = blank;
        }
    }

    fn erase_chars(&mut self, n: u16) {
        let cols = self.cols as usize;
        let (row, col) = (self.cursor.row as usize, self.cursor.col as usize);
        let grid = self.grid_mut();
        let start = row * cols + col;
        let end = (start + n as usize).min(start + (cols - col));
        for cell in &mut grid[start..end] {
            *cell = Cell::default();
        }
    }

    fn insert_chars(&mut self, n: u16) {
        let cols = self.cols as usize;
        let (row, col) = (self.cursor.row as usize, self.cursor.col as usize);
        let n = (n as usize).min(cols - col);
        let grid = self.grid_mut();
        let base = row * cols;
        grid[base + col..base + cols].rotate_right(n);
        for cell in &mut grid[base + col..base + col + n] {
            *cell = Cell::default();
        }
    }

    fn delete_chars(&mut self, n: u16) {
        let cols = self.cols as usize;
        let (row, col) = (self.cursor.row as usize, self.cursor.col as usize);
        let n = (n as usize).min(cols - col);
        let grid = self.grid_mut();
        let base = row * cols;
        grid[base + col..base + cols].rotate_left(n);
        for cell in &mut grid[base + cols - n..base + cols] {
            *cell = Cell::default();
        }
    }

    fn insert_lines(&mut self, n: u16) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let cols = self.cols as usize;
        let scroll_top = self.scroll_top as usize;
        let scroll_bottom = self.scroll_bottom as usize;
        let top = self.cursor.row as usize;
        let region = scroll_bottom - scroll_top + 1;
        let n = (n as usize).min(region - (top - scroll_top));
        let grid = self.grid_mut();
        let start = top * cols;
        let end = (scroll_bottom + 1) * cols;
        grid[start..end].rotate_right(n * cols);
        for cell in &mut grid[start..start + n * cols] {
            *cell = Cell::default();
        }
    }

    fn delete_lines(&mut self, n: u16) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        let cols = self.cols as usize;
        let scroll_top = self.scroll_top as usize;
        let scroll_bottom = self.scroll_bottom as usize;
        let top = self.cursor.row as usize;
        let region = scroll_bottom - scroll_top + 1;
        let n = (n as usize).min(region - (top - scroll_top));
        let grid = self.grid_mut();
        let start = top * cols;
        let end = (scroll_bottom + 1) * cols;
        grid[start..end].rotate_left(n * cols);
        for cell in &mut grid[end - n * cols..end] {
            *cell = Cell::default();
        }
    }

    fn repeat_last(&mut self, n: u16) {
        if self.cursor.col == 0 {
            return;
        }
        let cols = self.cols as usize;
        let (row, col) = (self.cursor.row as usize, self.cursor.col as usize);
        let last = *self.grid().get(row * cols + col - 1).unwrap_or(&Cell::default());
        for i in 0..n as usize {
            let target = col + i;
            if target >= cols {
                break;
            }
            self.grid_mut()[row * cols + target] = Cell { width: 1, ..last };
        }
        self.cursor.col = ((col + n as usize).min(cols - 1)) as u16;
    }

    /// SGR: colours and styles. Unknown parameters are ignored, as a real
    /// terminal would.
    ///
    /// SGR is the one sequence where the semicolon form (`38;2;r;g;b`) and the
    /// colon form (`38:2:r:g:b`) both appear in the wild, and both must work.
    fn sgr(&mut self, p: &Params) {
        if p.groups.is_empty() {
            self.reset_pen();
            return;
        }
        let mut i = 0;
        while i < p.groups.len() {
            let group = p.groups[i].clone();
            let Some(&code) = group.first() else {
                i += 1;
                continue;
            };
            match code {
                0 => self.reset_pen(),
                1 => self.attrs.set(Attrs::BOLD, true),
                2 => self.attrs.set(Attrs::FAINT, true),
                3 => self.attrs.set(Attrs::ITALIC, true),
                4 => self.attrs.set(Attrs::UNDERLINE, true),
                5 | 6 => self.attrs.set(Attrs::BLINK, true),
                7 => self.attrs.set(Attrs::INVERSE, true),
                8 => self.attrs.set(Attrs::HIDDEN, true),
                9 => self.attrs.set(Attrs::STRIKE, true),
                21 | 22 => {
                    self.attrs.set(Attrs::BOLD, false);
                    self.attrs.set(Attrs::FAINT, false);
                }
                23 => self.attrs.set(Attrs::ITALIC, false),
                24 => self.attrs.set(Attrs::UNDERLINE, false),
                25 => self.attrs.set(Attrs::BLINK, false),
                27 => self.attrs.set(Attrs::INVERSE, false),
                28 => self.attrs.set(Attrs::HIDDEN, false),
                29 => self.attrs.set(Attrs::STRIKE, false),
                30..=37 => self.fg = Color::Indexed((code - 30) as u8),
                39 => self.fg = Color::Default,
                40..=47 => self.bg = Color::Indexed((code - 40) as u8),
                49 => self.bg = Color::Default,
                38 | 48 => {
                    let foreground = code == 38;
                    let (color, consumed) = extended_color(&p.groups, i);
                    if let Some(color) = color {
                        if foreground {
                            self.fg = color;
                        } else {
                            self.bg = color;
                        }
                    }
                    i += consumed;
                }
                90..=97 => self.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.bg = Color::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    fn reset_pen(&mut self) {
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.attrs = Attrs::empty();
    }

    fn osc(&mut self, code: u8, data: &str) {
        match code {
            0 | 2 => self.title = Some(data.to_owned()),
            // OSC 133: shell integration marks (iTerm2 / finalterm protocol).
            133 => {
                let (kind, rest) = match data.split_once(';') {
                    Some((k, r)) => (k, r),
                    None => (data, ""),
                };
                let kind = kind.chars().next().unwrap_or('A');
                let status = rest.trim().parse::<i32>().ok();
                self.shell_marks.push(ShellMark { kind: kind.to_ascii_uppercase(), status });
            }
            _ => {}
        }
    }

    fn device_report(&mut self, groups: &[Vec<u16>]) {
        let code = groups.first().and_then(|g| g.first()).copied().unwrap_or(0);
        match code {
            // Cursor position report.
            6 => {
                let row = self.cursor.row + 1;
                let col = self.cursor.col + 1;
                self.replies.extend(format!("\x1b[{row};{col}R").into_bytes());
            }
            // Primary device attributes: answer as a VT102 with no options.
            _ => self.replies.extend_from_slice(b"\x1b[?6c"),
        }
    }
}

/// Decode `38`/`48` (and their background counterparts) in either the colon
/// form `38:2:r:g:b` or the semicolon form `38;2;r;g;b`.
///
/// Returns the colour and how many parameter groups were consumed.
fn extended_color(groups: &[Vec<u16>], i: usize) -> (Option<Color>, usize) {
    // Colon form: everything lives in this one group.
    let g = &groups[i];
    if g.len() >= 5 && g[1] == 2 {
        return (Some(Color::Rgb(g[2] as u8, g[3] as u8, g[4] as u8)), 1);
    }
    if g.len() >= 3 && g[1] == 5 {
        return (Some(Color::Indexed(g[2] as u8)), 1);
    }

    // Semicolon form: the selector and its arguments are separate groups.
    let at = |n: usize| groups.get(i + n).and_then(|g| g.first()).copied();
    match at(1) {
        Some(2) => {
            if let (Some(r), Some(g), Some(b)) = (at(2), at(3), at(4)) {
                (Some(Color::Rgb(r as u8, g as u8, b as u8)), 5)
            } else {
                (None, 1)
            }
        }
        Some(5) => match at(2) {
            Some(index) => (Some(Color::Indexed(index as u8)), 3),
            None => (None, 1),
        },
        _ => (None, 1),
    }
}

/// Display width of a character: 0 for combining marks, 2 for wide glyphs.
///
/// Deliberately conservative — an exact `wcwidth` needs Unicode tables that
/// would dwarf this crate, and the only consumer is a text preview.
pub fn char_width(c: char) -> u8 {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    if (0x0300..=0x036f).contains(&cp) {
        return 0;
    }
    if cp < 0x1100 {
        return 1;
    }
    let wide = matches!(cp,
        0x1100..=0x115f
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd);
    if wide {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(cols: u16, rows: u16) -> Screen {
        Screen::new(Size::new(cols, rows))
    }

    #[test]
    fn printing_advances_and_wraps() {
        let mut s = screen(4, 2);
        s.feed(b"abcd");
        assert_eq!(s.text_rows()[0], "abcd");
        assert_eq!(s.cursor(), Cursor { row: 0, col: 3 });
        s.feed(b"e");
        assert_eq!(s.text_rows()[0], "abcd");
        assert_eq!(s.text_rows()[1], "e");
    }

    #[test]
    fn carriage_return_and_line_feed_position_correctly() {
        let mut s = screen(10, 3);
        s.feed(b"one\r\ntwo");
        assert_eq!(s.text_rows()[0], "one");
        assert_eq!(s.text_rows()[1], "two");
        assert_eq!(s.cursor(), Cursor { row: 1, col: 3 });
    }

    #[test]
    fn line_feed_at_the_bottom_scrolls() {
        let mut s = screen(10, 2);
        s.feed(b"a\r\nb\r\nc");
        assert_eq!(s.text_rows(), vec!["b", "c"]);
    }

    #[test]
    fn erase_display_and_line() {
        let mut s = screen(10, 2);
        s.feed(b"abcdef\r\nghijkl");
        s.feed(b"\x1b[1;4H\x1b[K");
        assert_eq!(s.text_rows()[0], "abc");
        s.feed(b"\x1b[2;1H\x1b[J");
        assert_eq!(s.text_rows()[1], "");
    }

    #[test]
    fn cursor_positioning_clamps_inside_the_screen() {
        let mut s = screen(10, 3);
        s.feed(b"\x1b[99;99H");
        assert_eq!(s.cursor(), Cursor { row: 2, col: 9 });
        s.feed(b"\x1b[0;0H");
        assert_eq!(s.cursor(), Cursor { row: 0, col: 0 });
    }

    #[test]
    fn scroll_region_limits_scrolling() {
        let mut s = screen(10, 4);
        // Region is rows 2..4 (1-based). Put a marker on row 1.
        s.feed(b"\x1b[1;1Htop");
        s.feed(b"\x1b[2;4r"); // set region
        s.feed(b"\x1b[4;1Hx\r\ny\r\nz\r\nw");
        let rows = s.text_rows();
        assert_eq!(rows[0], "top", "row 1 is outside the region and must not scroll");
        assert_eq!(rows[3], "w");
    }

    #[test]
    fn insert_and_delete_lines_within_the_region() {
        let mut s = screen(6, 3);
        s.feed(b"a\r\nb\r\nc");
        s.feed(b"\x1b[1;1H\x1b[L");
        assert_eq!(s.text_rows(), vec!["", "a", "b"]);
        // Deleting the blank row we just inserted gives a, b and a trailing
        // blank: that is exactly what `DL` does.
        s.feed(b"\x1b[1;1H\x1b[M");
        assert_eq!(s.text_rows(), vec!["a", "b", ""]);
    }

    #[test]
    fn delete_and_insert_characters_shift_the_line() {
        let mut s = screen(10, 1);
        s.feed(b"abcdef");
        s.feed(b"\x1b[1;3H\x1b[2P");
        assert_eq!(s.text_rows()[0], "abef");
        s.feed(b"\x1b[1;3H\x1b[2@");
        assert_eq!(s.text_rows()[0], "ab  ef");
    }

    #[test]
    fn erase_characters_blanks_in_place() {
        let mut s = screen(10, 1);
        s.feed(b"abcdef");
        s.feed(b"\x1b[1;2H\x1b[3X");
        assert_eq!(s.text_rows()[0], "a   ef");
    }

    #[test]
    fn alternate_screen_is_separate_and_restores() {
        let mut s = screen(8, 2);
        s.feed(b"primary");
        s.feed(b"\x1b[?1049h");
        assert!(s.used_alt_screen);
        assert_eq!(s.text_rows()[0], "");
        s.feed(b"in vim");
        assert_eq!(s.text_rows()[0], "in vim");
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.text_rows()[0], "primary");
    }

    #[test]
    fn sgr_colours_and_styles_land_on_cells() {
        let mut s = screen(10, 1);
        s.feed(b"\x1b[1;31mX");
        let cell = s.cell(0, 0).copied().unwrap();
        assert_eq!(cell.ch, 'X');
        assert_eq!(cell.fg, Color::Indexed(1));
        assert!(cell.attrs.contains(Attrs::BOLD));

        s.feed(b"\x1b[0m\x1b[38;5;208mY");
        let cell = s.cell(0, 1).copied().unwrap();
        assert_eq!(cell.fg, Color::Indexed(208));
        assert!(cell.attrs.is_plain());
    }

    #[test]
    fn truecolor_in_both_sgr_encodings() {
        let mut s = screen(10, 1);
        s.feed(b"\x1b[38;2;10;20;30mA");
        assert_eq!(s.cell(0, 0).unwrap().fg, Color::Rgb(10, 20, 30));
        s.feed(b"\x1b[48:2:1:2:3mB");
        assert_eq!(s.cell(0, 1).unwrap().bg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn save_and_restore_cursor() {
        let mut s = screen(10, 5);
        s.feed(b"\x1b[2;3H\x1b7\x1b[4;5H\x1b8");
        assert_eq!(s.cursor(), Cursor { row: 1, col: 2 });
    }

    #[test]
    fn tabs_move_by_eight() {
        let mut s = screen(40, 1);
        s.feed(b"a\tb\tc");
        assert_eq!(s.text_rows()[0], "a       b       c");
    }

    #[test]
    fn tab_stop_can_be_set_and_cleared() {
        let mut s = screen(20, 1);
        // Move to column 9 (index 8) and clear the stop that `HTS` would leave
        // there. The next tab must then land on column 17.
        s.feed(b"\x1b[9G\x1bH");
        s.feed(b"\x1b[1;1Ha\tb");
        assert_eq!(s.text_rows()[0], "a               b");
    }

    #[test]
    fn title_and_shell_marks_are_captured() {
        let mut s = screen(10, 2);
        s.feed(b"\x1b]0;my session\x07");
        assert_eq!(s.title.as_deref(), Some("my session"));
        s.feed(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;D;42\x07");
        assert_eq!(
            s.shell_marks,
            vec![
                ShellMark { kind: 'A', status: None },
                ShellMark { kind: 'B', status: None },
                ShellMark { kind: 'D', status: Some(42) },
            ]
        );
    }

    #[test]
    fn cursor_position_report_is_queued_as_a_reply() {
        let mut s = screen(20, 5);
        s.feed(b"\x1b[3;7H\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[3;7R".to_vec());
        assert!(s.take_replies().is_empty());
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let mut s = screen(6, 1);
        s.feed("한한".as_bytes());
        assert_eq!(s.text_rows()[0], "한한");
        // Each wide glyph claims two cells; the cursor ends on the last one.
        assert_eq!(s.cursor().col, 4);
        assert_eq!(s.cell(0, 1).unwrap().width, 0);
    }

    #[test]
    fn narrow_arrows_and_symbols_stay_one_cell() {
        let mut s = screen(6, 1);
        s.feed("→→".as_bytes());
        assert_eq!(s.cursor().col, 2);
    }

    #[test]
    fn resize_keeps_overlapping_content() {
        let mut s = screen(10, 3);
        s.feed(b"hello");
        s.resize(Size::new(5, 3));
        assert_eq!(s.size(), Size::new(5, 3));
        assert_eq!(s.text_rows()[0], "hello");
        s.resize(Size::new(20, 4));
        assert_eq!(s.text_rows()[0], "hello");
        assert_eq!(s.text_rows().len(), 4);
    }

    #[test]
    fn full_reset_clears_everything() {
        let mut s = screen(10, 2);
        s.feed(b"junk\x1b[1;31m");
        s.feed(b"\x1bc");
        assert_eq!(s.text_rows(), vec!["", ""]);
        assert_eq!(s.cursor(), Cursor { row: 0, col: 0 });
        assert_eq!(s.pen(), (Color::Default, Color::Default, Attrs::empty()));
    }

    #[test]
    fn hard_reset_after_a_partial_sequence_does_not_wedge_the_screen() {
        let mut s = screen(10, 1);
        s.feed(b"\x1b[38;5");
        s.feed(b"\x1bc");
        s.feed(b"ok");
        assert_eq!(s.text_rows()[0], "ok");
    }

    #[test]
    fn text_rows_trim_trailing_blanks_only() {
        let mut s = screen(8, 1);
        s.feed(b"  a  ");
        assert_eq!(s.text_rows()[0], "  a");
    }

    #[test]
    fn char_width_matches_common_cases() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('→'), 1);
        assert_eq!(char_width('한'), 2);
        assert_eq!(char_width('🌍'), 2);
        assert_eq!(char_width('\u{0301}'), 0);
    }
}
