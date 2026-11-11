//! The session server. It runs in the background with no console and owns
//! every pane. Clients attach over the session socket, send keys, and get frames.
//!
//! Threads:
//!   - accept thread:        new socket connection   -> Event::Conn, then one reader thread
//!   - one reader/client:    socket messages         -> Event::Msg / Event::Gone
//!   - one writer/client:    queued frames           -> socket (a slow client never blocks the server)
//!   - one reader/pane:      ConPTY output -> vt100  -> Event::Output
//!   - one waiter/pane:      shell exit              -> Event::Exit
//!   - main thread:          owns all state, handles events, draws frames.

use std::collections::HashMap;
use std::io::Write;
use std::net::Shutdown;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use uds_windows::{UnixListener, UnixStream};

use crate::keys;
use crate::layout::{self, Axis, Node, Rect};
use crate::pane::Pane;
use crate::proto;
use crate::render::{Color, Grid, Renderer, Style, BOLD, DIM, INVERSE, ITALIC, UNDERLINE};
use crate::session;

const FRAME: Duration = Duration::from_millis(8);

pub enum Event {
    Output(usize),
    Exit(usize),
    Conn(usize, UnixStream),
    Msg(usize, u8, Vec<u8>),
    Gone(usize),
}

#[derive(Clone, Copy)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

enum Mode {
    Normal,
    /// Prefix key (Ctrl-b) was pressed. The next key is a command.
    Prefix,
    /// After a repeatable command (arrows), more of them work without the
    /// prefix until this deadline. Same as tmux `repeat-time`.
    Repeat(Instant),
    /// Scroll through the active pane's history.
    Scroll,
    /// Type a new window name.
    Rename(String),
}

struct Window {
    root: Node,
    active: usize,
    last: Option<usize>,
    zoomed: bool,
    name: String,
    activity: bool,
}

impl Window {
    /// Remove a pane from the tree. Returns true when the window is now empty.
    fn remove_pane(&mut self, id: usize) -> bool {
        let root = std::mem::replace(&mut self.root, Node::Leaf(usize::MAX));
        let Some(root) = root.remove(id) else { return true };
        self.root = root;
        if self.last == Some(id) {
            self.last = None;
        }
        if self.active == id {
            self.zoomed = false;
            self.active = self.last.take().unwrap_or_else(|| self.root.first());
        }
        false
    }
}

struct Client {
    tx: Sender<Vec<u8>>,
    writer: JoinHandle<()>,
    attached: bool,
    size: (u16, u16),
    renderer: Renderer,
}

fn start_writer(mut s: UnixStream) -> (Sender<Vec<u8>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let h = thread::spawn(move || {
        for buf in rx {
            if s.write_all(&buf).is_err() {
                break;
            }
        }
        let _ = s.shutdown(Shutdown::Both);
    });
    (tx, h)
}

struct App {
    name: String,
    created: chrono::DateTime<chrono::Local>,
    panes: HashMap<usize, Pane>,
    windows: Vec<Window>,
    active: usize,
    last_window: Option<usize>,
    next_id: usize,
    mode: Mode,
    w: u16,
    h: u16,
    grid: Grid,
    clients: HashMap<usize, Client>,
    /// The client whose terminal size the session uses (tmux "latest").
    latest: Option<usize>,
    /// The client that sent the event we are handling now.
    cur_client: Option<usize>,
    tx: Sender<Event>,
    cmd: Vec<String>,
    message: Option<(String, Instant)>,
    dirty: bool,
    /// Set when the server must stop. The text goes to attached clients.
    quit: Option<String>,
}

fn is_prefix(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('b') | KeyCode::Char('B') | KeyCode::Char('\x02'))
}

fn arrow(k: &KeyEvent) -> Option<Dir> {
    match k.code {
        KeyCode::Left => Some(Dir::Left),
        KeyCode::Right => Some(Dir::Right),
        KeyCode::Up => Some(Dir::Up),
        KeyCode::Down => Some(Dir::Down),
        _ => None,
    }
}

impl App {
    fn area(&self) -> Rect {
        Rect { x: 0, y: 0, w: self.w, h: self.h.saturating_sub(1) } // last row = status bar
    }

    fn win(&self) -> &Window {
        &self.windows[self.active]
    }

    fn win_mut(&mut self) -> &mut Window {
        &mut self.windows[self.active]
    }

    fn msg(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), Instant::now()));
        self.dirty = true;
    }

    fn window_layout(&self, win: &Window) -> (Vec<(usize, Rect)>, Vec<layout::Border>) {
        let (mut p, mut b) = (vec![], vec![]);
        win.root.layout(self.area(), &mut p, &mut b);
        if win.zoomed {
            for (id, r) in p.iter_mut() {
                if *id == win.active {
                    *r = self.area();
                }
            }
            b.clear();
        }
        (p, b)
    }

    /// Give every pane in every window its correct size.
    fn apply_layout(&mut self) {
        for wi in 0..self.windows.len() {
            let (p, _) = self.window_layout(&self.windows[wi]);
            for (id, r) in p {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.resize(r);
                }
            }
        }
        self.dirty = true;
    }

    fn set_size(&mut self, (w, h): (u16, u16)) {
        if w == 0 || h == 0 || (w, h) == (self.w, self.h) {
            return;
        }
        self.w = w;
        self.h = h;
        self.grid = Grid::new(w, h);
        self.apply_layout();
    }

    fn alloc_id(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    // ---- clients -------------------------------------------------------

    fn reply(&self, id: usize, tag: u8, payload: &[u8]) {
        if let Some(c) = self.clients.get(&id) {
            let _ = c.tx.send(proto::encode(tag, payload));
        }
    }

    /// Send the client an exit reason and close its connection.
    fn close_client(&mut self, id: usize, reason: &str) {
        self.reply(id, proto::EXIT, reason.as_bytes());
        self.client_gone(id); // dropping the sender lets the writer flush and shut down
    }

    fn client_gone(&mut self, id: usize) {
        self.clients.remove(&id);
        if self.latest == Some(id) {
            self.latest = self.clients.iter().find(|(_, c)| c.attached).map(|(i, _)| *i);
            if let Some(size) = self.latest.map(|l| self.clients[&l].size) {
                self.set_size(size);
            }
        }
    }

    fn summary(&self) -> String {
        let attached = self.clients.values().filter(|c| c.attached).count();
        format!(
            "{}: {} windows (created {}){}",
            self.name,
            self.windows.len(),
            self.created.format("%a %b %e %H:%M:%S %Y"),
            if attached > 0 { " (attached)" } else { "" }
        )
    }

    fn client_msg(&mut self, id: usize, tag: u8, p: Vec<u8>) {
        if !self.clients.contains_key(&id) {
            return;
        }
        self.cur_client = Some(id);
        match tag {
            proto::HELLO => {
                let Some(size) = proto::parse_size(&p) else { return };
                let c = self.clients.get_mut(&id).unwrap();
                c.attached = true;
                c.size = size;
                c.renderer.reset();
                self.latest = Some(id);
                self.set_size(size);
                self.dirty = true;
            }
            proto::RESIZE => {
                let Some(size) = proto::parse_size(&p) else { return };
                let c = self.clients.get_mut(&id).unwrap();
                c.size = size;
                c.renderer.reset(); // the terminal may have reflowed; redraw it all
                if self.latest == Some(id) {
                    self.set_size(size);
                }
                self.dirty = true;
            }
            proto::KEY => {
                let Some(k) = proto::parse_key(&p) else { return };
                let c = &self.clients[&id];
                if !c.attached || k.kind == KeyEventKind::Release {
                    return;
                }
                if self.latest != Some(id) {
                    let size = c.size;
                    self.latest = Some(id);
                    self.set_size(size);
                }
                self.key(k);
            }
            proto::PASTE => {
                let s = String::from_utf8_lossy(&p);
                let bracketed = self.active_pane().is_some_and(|p| p.parser.lock().unwrap().screen().bracketed_paste());
                if bracketed {
                    self.send_active(format!("\x1b[200~{s}\x1b[201~").as_bytes());
                } else {
                    self.send_active(s.as_bytes());
                }
            }
            proto::LIST => self.reply(id, proto::TEXT, self.summary().as_bytes()),
            proto::KILL => {
                self.reply(id, proto::TEXT, b"");
                self.quit = Some("[exited]".into());
            }
            _ => {}
        }
    }

    // ---- windows -------------------------------------------------------

    fn new_window(&mut self) -> Result<()> {
        let id = self.alloc_id();
        let pane = Pane::spawn(id, &self.cmd, self.area(), self.tx.clone())?;
        self.panes.insert(id, pane);
        let name = std::path::Path::new(&self.cmd[0])
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "shell".into());
        self.windows.push(Window { root: Node::Leaf(id), active: id, last: None, zoomed: false, name, activity: false });
        self.select_window(self.windows.len() - 1);
        Ok(())
    }

    fn select_window(&mut self, i: usize) {
        if i < self.windows.len() && i != self.active {
            self.last_window = Some(self.active);
            self.active = i;
        }
        if let Some(w) = self.windows.get_mut(self.active) {
            w.activity = false;
        }
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    fn remove_window(&mut self, i: usize) {
        self.windows.remove(i);
        if self.windows.is_empty() {
            self.quit = Some("[exited]".into());
            return;
        }
        let adj = |x: usize| if x > i { x - 1 } else { x };
        let lw = self.last_window.filter(|&l| l != i).map(adj);
        if self.active == i {
            self.active = lw.unwrap_or(i.min(self.windows.len() - 1));
            self.last_window = None;
        } else {
            self.active = adj(self.active);
            self.last_window = lw;
        }
        self.mode = Mode::Normal;
    }

    fn kill_window(&mut self) {
        let (p, _) = self.window_layout(self.win());
        for (id, _) in p {
            self.panes.remove(&id); // Drop kills the shell and closes the pseudo console
        }
        self.remove_window(self.active);
        self.apply_layout();
    }

    // ---- panes ---------------------------------------------------------

    fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.win().active)
    }

    fn send_active(&self, bytes: &[u8]) {
        if let Some(p) = self.active_pane() {
            p.write(bytes);
        }
    }

    fn split(&mut self, axis: Axis) {
        let id = self.alloc_id();
        let win = self.win_mut();
        win.zoomed = false;
        let target = win.active;
        win.root.split(target, id, axis);
        let (p, _) = self.window_layout(self.win());
        let rect = p.iter().find(|(i, _)| *i == id).map(|x| x.1).unwrap_or_default();
        let result = if rect.w < 2 || rect.h < 2 {
            Err(anyhow::anyhow!("pane too small"))
        } else {
            Pane::spawn(id, &self.cmd, rect, self.tx.clone())
        };
        match result {
            Ok(pane) => {
                self.panes.insert(id, pane);
                let win = self.win_mut();
                win.last = Some(target);
                win.active = id;
            }
            Err(e) => {
                self.win_mut().remove_pane(id);
                self.msg(format!("split failed: {e}"));
            }
        }
        self.apply_layout();
    }

    fn focus(&mut self, id: usize) {
        let win = self.win_mut();
        if win.active != id {
            win.last = Some(win.active);
            win.active = id;
        }
        win.zoomed = false;
        self.apply_layout();
    }

    /// Find the pane next to the active one: probe one cell past the border,
    /// on the row/column of the cursor. This is close to what tmux does.
    fn select_dir(&mut self, d: Dir) {
        // Use the unzoomed layout, so directions work while zoomed.
        let zoomed = std::mem::replace(&mut self.win_mut().zoomed, false);
        let (p, _) = self.window_layout(self.win());
        self.win_mut().zoomed = zoomed;
        let active = self.win().active;
        let Some(&(_, r)) = p.iter().find(|(i, _)| *i == active) else { return };
        let (crow, ccol) = self.active_pane().map(|p| p.parser.lock().unwrap().screen().cursor_position()).unwrap_or((0, 0));
        let cx = r.x + ccol.min(r.w.saturating_sub(1));
        let cy = r.y + crow.min(r.h.saturating_sub(1));
        let probe = match d {
            Dir::Left => r.x.checked_sub(2).map(|x| (x, cy)),
            Dir::Right => Some((r.x + r.w + 1, cy)),
            Dir::Up => r.y.checked_sub(2).map(|y| (cx, y)),
            Dir::Down => Some((cx, r.y + r.h + 1)),
        };
        if let Some((x, y)) = probe {
            if let Some(&(id, _)) = p.iter().find(|(_, r)| r.contains(x, y)) {
                self.focus(id);
            }
        }
    }

    fn cycle_pane(&mut self) {
        let (p, _) = self.window_layout(self.win());
        let ids: Vec<usize> = p.iter().map(|x| x.0).collect();
        let i = ids.iter().position(|&x| x == self.win().active).unwrap_or(0);
        self.focus(ids[(i + 1) % ids.len()]);
    }

    fn resize_pane(&mut self, d: Dir) {
        let step_x = 2.0 / self.w.max(1) as f32;
        let step_y = 2.0 / self.h.max(1) as f32;
        let (axis, delta) = match d {
            Dir::Left => (Axis::Horizontal, -step_x),
            Dir::Right => (Axis::Horizontal, step_x),
            Dir::Up => (Axis::Vertical, -step_y),
            Dir::Down => (Axis::Vertical, step_y),
        };
        let win = self.win_mut();
        let active = win.active;
        win.root.resize(active, axis, delta);
        self.apply_layout();
    }

    fn remove_pane(&mut self, id: usize) {
        self.panes.remove(&id);
        if let Some(wi) = self.windows.iter().position(|w| w.root.contains(id)) {
            if self.windows[wi].remove_pane(id) {
                self.remove_window(wi);
            }
        }
        if matches!(self.mode, Mode::Scroll) {
            self.mode = Mode::Normal;
        }
        self.apply_layout();
    }

    fn set_scroll(&mut self, n: usize) {
        let id = self.win().active;
        if let Some(pane) = self.panes.get_mut(&id) {
            let mut p = pane.parser.lock().unwrap();
            p.screen_mut().set_scrollback(n);
            pane.scroll = p.screen().scrollback(); // clamped value
            p.screen_mut().set_scrollback(0);
        }
        self.dirty = true;
    }

    // ---- events --------------------------------------------------------

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Conn(id, stream) => {
                let (tx, writer) = start_writer(stream);
                self.clients.insert(id, Client { tx, writer, attached: false, size: (0, 0), renderer: Renderer::default() });
            }
            Event::Gone(id) => {
                if self.clients.contains_key(&id) {
                    self.client_gone(id);
                }
            }
            _ if self.windows.is_empty() => {} // shutting down
            Event::Msg(id, tag, p) => self.client_msg(id, tag, p),
            Event::Output(id) => {
                if self.win().root.contains(id) {
                    self.dirty = true;
                } else if let Some(w) = self.windows.iter_mut().find(|w| w.root.contains(id)) {
                    if !w.activity {
                        w.activity = true;
                        self.dirty = true;
                    }
                }
            }
            Event::Exit(id) => {
                if self.panes.contains_key(&id) {
                    self.remove_pane(id);
                }
            }
        }
    }

    fn key(&mut self, k: KeyEvent) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => self.normal_key(k),
            Mode::Prefix => {
                self.dirty = true;
                if self.prefix_cmd(k) {
                    self.mode = Mode::Repeat(Instant::now() + Duration::from_millis(500));
                }
            }
            Mode::Repeat(until) => {
                if Instant::now() < until && arrow(&k).is_some() {
                    self.prefix_cmd(k);
                    self.mode = Mode::Repeat(Instant::now() + Duration::from_millis(500));
                } else {
                    self.normal_key(k);
                }
            }
            Mode::Scroll => self.scroll_key(k),
            Mode::Rename(mut buf) => {
                match k.code {
                    KeyCode::Enter => {
                        if !buf.is_empty() {
                            self.win_mut().name = buf;
                        }
                        return self.dirty = true;
                    }
                    KeyCode::Esc => return self.dirty = true,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                self.mode = Mode::Rename(buf);
                self.dirty = true;
            }
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        if is_prefix(&k) {
            self.mode = Mode::Prefix;
            self.dirty = true;
            return;
        }
        let app_cursor = self.active_pane().is_some_and(|p| p.parser.lock().unwrap().screen().application_cursor());
        if let Some(bytes) = keys::encode(k, app_cursor) {
            self.send_active(&bytes);
        }
    }

    /// Run one prefix command. Returns true if the command can repeat.
    fn prefix_cmd(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if is_prefix(&k) {
            self.send_active(&[0x02]); // Ctrl-b Ctrl-b sends a real Ctrl-b
            return false;
        }
        if let Some(d) = arrow(&k) {
            if ctrl { self.resize_pane(d) } else { self.select_dir(d) }
            return true;
        }
        match k.code {
            KeyCode::Char('%') | KeyCode::Char('|') => self.split(Axis::Horizontal),
            KeyCode::Char('"') | KeyCode::Char('-') => self.split(Axis::Vertical),
            KeyCode::Char('o') => self.cycle_pane(),
            KeyCode::Char(';') => {
                if let Some(l) = self.win().last {
                    self.focus(l);
                }
            }
            KeyCode::Char('x') => {
                let id = self.win().active;
                self.remove_pane(id);
            }
            KeyCode::Char('&') => self.kill_window(),
            KeyCode::Char('c') => {
                if let Err(e) = self.new_window() {
                    self.msg(format!("new window failed: {e}"));
                }
            }
            KeyCode::Char('n') => self.select_window((self.active + 1) % self.windows.len()),
            KeyCode::Char('p') => self.select_window((self.active + self.windows.len() - 1) % self.windows.len()),
            KeyCode::Char('l') => {
                if let Some(l) = self.last_window {
                    self.select_window(l);
                }
            }
            KeyCode::Char(c @ '0'..='9') => self.select_window(c as usize - '0' as usize),
            KeyCode::Char('z') => {
                let w = self.win_mut();
                w.zoomed = !w.zoomed;
                self.apply_layout();
            }
            KeyCode::Char('[') => self.mode = Mode::Scroll,
            KeyCode::PageUp => {
                self.mode = Mode::Scroll;
                self.scroll_key(k);
            }
            KeyCode::Char(',') => self.mode = Mode::Rename(String::new()),
            KeyCode::Char('d') => {
                if let Some(id) = self.cur_client {
                    let reason = format!("[detached (from session {})]", self.name);
                    self.close_client(id, &reason);
                }
            }
            KeyCode::Char('?') => self.msg(
                "% \" split | arrows move | C-arrows resize | o ; pane | x kill | z zoom | c n p l 0-9 window | & kill win | [ scroll | , rename | d detach",
            ),
            _ => {}
        }
        false
    }

    fn scroll_key(&mut self, k: KeyEvent) {
        self.mode = Mode::Scroll;
        let Some(pane) = self.active_pane() else { return };
        let cur = pane.scroll;
        let page = (pane.rect.h as usize).max(2) - 1;
        let half = page / 2;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = match k.code {
            KeyCode::Up | KeyCode::Char('k') => cur + 1,
            KeyCode::Down | KeyCode::Char('j') => cur.saturating_sub(1),
            KeyCode::Char('u') if ctrl => cur + half,
            KeyCode::Char('d') if ctrl => cur.saturating_sub(half),
            KeyCode::PageUp | KeyCode::Char('b') => cur + page,
            KeyCode::PageDown | KeyCode::Char('f') => cur.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => usize::MAX,
            KeyCode::Char('G') | KeyCode::End => 0,
            KeyCode::Char('q') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                0
            }
            _ => cur,
        };
        self.set_scroll(n);
    }

    // ---- drawing -------------------------------------------------------

    fn draw(&mut self) {
        if self.windows.is_empty() || !self.clients.values().any(|c| c.attached) {
            return;
        }
        let (panes, borders) = self.window_layout(self.win());
        let win = &self.windows[self.active];
        let grid = &mut self.grid;
        grid.clear();

        let mut cursor = None;
        let mut shape = 0u8;
        let mut active_rect = Rect::default();

        for (id, rect) in &panes {
            if win.zoomed && *id != win.active {
                continue;
            }
            let Some(pane) = self.panes.get(id) else { continue };
            if *id == win.active {
                active_rect = *rect;
            }
            let mut p = pane.parser.lock().unwrap();
            p.screen_mut().set_scrollback(pane.scroll);
            let screen = p.screen();
            for row in 0..rect.h {
                for col in 0..rect.w {
                    let Some(cell) = screen.cell(row, col) else { continue };
                    let mut attrs = 0;
                    if cell.bold() { attrs |= BOLD }
                    if cell.dim() { attrs |= DIM }
                    if cell.italic() { attrs |= ITALIC }
                    if cell.underline() { attrs |= UNDERLINE }
                    if cell.inverse() { attrs |= INVERSE }
                    let style = Style { fg: cell.fgcolor().into(), bg: cell.bgcolor().into(), attrs };
                    let ch = if cell.is_wide_continuation() {
                        ""
                    } else if cell.has_contents() {
                        cell.contents()
                    } else {
                        " "
                    };
                    grid.set(rect.x + col, rect.y + row, ch, style, cell.is_wide());
                }
            }
            if *id == win.active && pane.scroll == 0 && !screen.hide_cursor() {
                let (r, c) = screen.cursor_position();
                if r < rect.h && c < rect.w {
                    cursor = Some((rect.x + c, rect.y + r));
                    #[allow(unreachable_patterns)]
                    {
                        shape = match screen.cursor_style() {
                            vt100::CursorStyle::BlinkBlock => 1,
                            vt100::CursorStyle::SteadyBlock => 2,
                            vt100::CursorStyle::BlinkUnderline => 3,
                            vt100::CursorStyle::SteadyUnderline => 4,
                            vt100::CursorStyle::BlinkVerticalBar => 5,
                            vt100::CursorStyle::SteadyVerticalBar => 6,
                            _ => 0,
                        };
                    }
                }
            }
            if pane.scroll > 0 {
                let label = format!("[{}/{}]", pane.scroll, screen.scrollback_len());
                let x = (rect.x + rect.w).saturating_sub(label.len() as u16);
                grid.text(x, rect.y, &label, Style { fg: Color::Idx(0), bg: Color::Idx(3), attrs: 0 });
            }
            p.screen_mut().set_scrollback(0);
        }

        // Borders: mark border cells, then pick box-drawing chars from neighbours
        // so that junctions look right (├ ┤ ┬ ┴ ┼).
        let (w, h) = (grid.w as usize, grid.h as usize);
        let mut mask = vec![false; w * h];
        for b in &borders {
            for i in 0..b.len {
                let (x, y) = if b.vertical { (b.x, b.y + i) } else { (b.x + i, b.y) };
                if (x as usize) < w && (y as usize) < h {
                    mask[y as usize * w + x as usize] = true;
                }
            }
        }
        let is = |x: i32, y: i32| x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && mask[y as usize * w + x as usize];
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                if !is(x, y) {
                    continue;
                }
                let ch = match (is(x, y - 1), is(x, y + 1), is(x - 1, y), is(x + 1, y)) {
                    (true, true, true, true) => "┼",
                    (true, true, false, true) => "├",
                    (true, true, true, false) => "┤",
                    (false, true, true, true) => "┬",
                    (true, false, true, true) => "┴",
                    (true, false, false, true) => "└",
                    (true, false, true, false) => "┘",
                    (false, true, false, true) => "┌",
                    (false, true, true, false) => "┐",
                    (_, _, false, false) => "│",
                    _ => "─",
                };
                let near_active = [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]
                    .iter()
                    .any(|&(nx, ny)| nx >= 0 && ny >= 0 && active_rect.contains(nx as u16, ny as u16));
                let style = if near_active { Style::fg(Color::Idx(2)) } else { Style::DEFAULT };
                grid.set(x as u16, y as u16, ch, style, false);
            }
        }

        // Status bar, tmux style.
        let y = self.h.saturating_sub(1);
        let bar = Style { fg: Color::Idx(0), bg: Color::Idx(2), attrs: 0 };
        let hi = Style { fg: Color::Idx(0), bg: Color::Idx(3), attrs: 0 };
        for x in 0..self.w {
            grid.set(x, y, " ", bar, false);
        }
        let mut x = match self.mode {
            Mode::Prefix | Mode::Repeat(_) => grid.text(0, y, "[^B] ", hi),
            Mode::Scroll => grid.text(0, y, "[scroll q:exit] ", hi),
            _ => grid.text(0, y, &format!("[{}] ", self.name), bar),
        };
        if let Some((m, t)) = &self.message {
            if t.elapsed() < Duration::from_secs(4) {
                grid.text(x, y, m, hi);
                x = self.w; // message replaces the window list
            }
        }
        if let Mode::Rename(buf) = &self.mode {
            grid.text(x, y, &format!("(rename-window) {buf}_"), hi);
        } else if x < self.w {
            for (i, wd) in self.windows.iter().enumerate() {
                let flag = if i == self.active {
                    "*"
                } else if Some(i) == self.last_window {
                    "-"
                } else if wd.activity {
                    "#"
                } else {
                    ""
                };
                let zoom = if wd.zoomed { "Z" } else { "" };
                x = grid.text(x, y, &format!("{i}:{}{flag}{zoom} ", wd.name), bar);
            }
        }
        let clock = chrono::Local::now().format(" %H:%M %d-%b-%y ").to_string();
        let cx = self.w.saturating_sub(clock.len() as u16);
        if cx > x {
            grid.text(cx, y, &clock, bar);
        }

        let cursor = if matches!(self.mode, Mode::Normal | Mode::Prefix | Mode::Repeat(_)) { cursor } else { None };
        for c in self.clients.values_mut().filter(|c| c.attached) {
            let frame = c.renderer.flush(&self.grid, cursor, shape);
            let _ = c.tx.send(proto::encode(proto::FRAME, frame.as_bytes()));
        }
        log!("draw to {} client(s)", self.clients.values().filter(|c| c.attached).count());
    }
}

pub fn run(name: String, w: u16, h: u16, cmd: Vec<String>) -> Result<()> {
    // Shells inherit this, so a nested `wmux` can see it runs inside wmux.
    // SAFETY: no other threads exist yet.
    unsafe { std::env::set_var("WMUX", session::socket_path(&name)) };
    let path = session::socket_path(&name);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    log!("server {name} listening on {}", path.display());

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { continue };
                let id = i + 1;
                let Ok(mut reader) = stream.try_clone() else { continue };
                if tx.send(Event::Conn(id, stream)).is_err() {
                    break;
                }
                let tx = tx.clone();
                thread::spawn(move || {
                    while let Ok((tag, p)) = proto::recv(&mut reader) {
                        if tx.send(Event::Msg(id, tag, p)).is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(Event::Gone(id));
                });
            }
        });
    }

    let mut app = App {
        name,
        created: chrono::Local::now(),
        panes: HashMap::new(),
        windows: Vec::new(),
        active: 0,
        last_window: None,
        next_id: 0,
        mode: Mode::Normal,
        w,
        h,
        grid: Grid::new(w, h),
        clients: HashMap::new(),
        latest: None,
        cur_client: None,
        tx,
        cmd,
        message: None,
        dirty: true,
        quit: None,
    };
    if let Err(e) = app.new_window() {
        log!("first window failed: {e}");
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }

    let mut last_draw = Instant::now() - FRAME;
    while app.quit.is_none() {
        let timeout = if app.dirty { FRAME.saturating_sub(last_draw.elapsed()) } else { Duration::from_secs(1) };
        match rx.recv_timeout(timeout) {
            Ok(ev) => {
                app.handle(ev);
                // Take a burst of events in one go, but keep drawing during a flood.
                for _ in 0..1024 {
                    if app.quit.is_some() {
                        break;
                    }
                    let Ok(ev) = rx.try_recv() else { break };
                    app.handle(ev);
                }
            }
            Err(RecvTimeoutError::Timeout) => app.dirty = true, // clock / message expiry
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if app.quit.is_none() && app.dirty && last_draw.elapsed() >= FRAME {
            app.draw();
            app.dirty = false;
            last_draw = Instant::now();
        }
    }

    // Shut down: tell clients why, let writers flush, stop shells, remove socket.
    let reason = app.quit.take().unwrap_or_else(|| "[server exited]".into());
    log!("server stopping: {reason}");
    for (_, c) in app.clients.drain() {
        if c.attached {
            let _ = c.tx.send(proto::encode(proto::EXIT, reason.as_bytes()));
        }
        drop(c.tx);
        let _ = c.writer.join();
    }
    let _ = std::fs::remove_file(&path);
    app.panes.clear();
    std::process::exit(0); // the accept thread is still blocked in accept()
}
