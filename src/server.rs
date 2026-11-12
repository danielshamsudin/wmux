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
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use uds_windows::{UnixListener, UnixStream};

use crate::config::{self, Action, Config, Key};
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
    /// Copy mode: move a cursor through the active pane's history, select,
    /// and yank. tmux's copy-mode.
    Scroll(Copy),
    /// Type a new window name.
    Rename(String),
    /// Pick another session to attach to (Ctrl-b s).
    Choose { names: Vec<String>, sel: usize },
}

/// Copy-mode cursor and selection. `line` is an absolute line number counted
/// from the oldest line in history, so it stays put when the viewport scrolls.
#[derive(Clone, Copy)]
struct Copy {
    line: usize,
    col: u16,
    /// Selection start (line, col) when a selection is active.
    anchor: Option<(usize, u16)>,
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
    cfg: Config,
    /// The last text yanked in copy mode. Ctrl-b ] pastes it.
    copy_buf: String,
    message: Option<(String, Instant)>,
    dirty: bool,
    /// Set when the server must stop. The text goes to attached clients.
    quit: Option<String>,
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
    /// Is this key the configured prefix? Accepts both the Ctrl+letter form the
    /// terminal usually sends and the raw control byte some terminals send.
    fn is_prefix(&self, k: &KeyEvent) -> bool {
        let (pm, pc) = self.cfg.prefix;
        if config::norm_key(k.modifiers, k.code) == (pm, pc) {
            return true;
        }
        if let (Some(b), KeyCode::Char(c)) = (self.cfg.prefix_byte, k.code) {
            return (c as u32) == b as u32 && (c as u32) < 0x20;
        }
        false
    }

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
                if self.cfg.mouse {
                    self.reply(id, proto::MOUSEON, &[1]);
                }
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
            proto::MOUSE => {
                let c = &self.clients[&id];
                if !c.attached {
                    return;
                }
                if self.latest != Some(id) {
                    let size = c.size;
                    self.latest = Some(id);
                    self.set_size(size);
                }
                if let Some(m) = proto::parse_mouse(&p) {
                    self.mouse(m);
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
        let pane = Pane::spawn(id, &self.cmd, self.area(), self.cfg.history, self.tx.clone())?;
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
            Pane::spawn(id, &self.cmd, rect, self.cfg.history, self.tx.clone())
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
        if let Some((x, y)) = probe
            && let Some(&(id, _)) = p.iter().find(|(_, r)| r.contains(x, y))
        {
            self.focus(id);
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
        if let Some(wi) = self.windows.iter().position(|w| w.root.contains(id))
            && self.windows[wi].remove_pane(id)
        {
            self.remove_window(wi);
        }
        if matches!(self.mode, Mode::Scroll(_)) {
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
                } else if let Some(w) = self.windows.iter_mut().find(|w| w.root.contains(id))
                    && !w.activity
                {
                    w.activity = true;
                    self.dirty = true;
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
                    self.mode = Mode::Repeat(Instant::now() + Duration::from_millis(self.cfg.repeat_ms));
                }
            }
            Mode::Repeat(until) => {
                if Instant::now() < until && arrow(&k).is_some() {
                    self.prefix_cmd(k);
                    self.mode = Mode::Repeat(Instant::now() + Duration::from_millis(self.cfg.repeat_ms));
                } else {
                    self.normal_key(k);
                }
            }
            Mode::Scroll(st) => self.scroll_key(k, st),
            Mode::Choose { names, sel } => self.choose_key(k, names, sel),
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
        if self.is_prefix(&k) {
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
        if self.is_prefix(&k) {
            if let Some(b) = self.cfg.prefix_byte {
                self.send_active(&[b]); // prefix twice sends a real prefix key
            }
            return false;
        }
        // Arrows move/resize the pane and may repeat; they are not rebindable.
        if let Some(d) = arrow(&k) {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                self.resize_pane(d)
            } else {
                self.select_dir(d)
            }
            return true;
        }
        if matches!(k.code, KeyCode::PageUp) {
            self.enter_copy();
            if let Mode::Scroll(st) = self.mode {
                self.scroll_key(k, st);
            }
            return false;
        }
        let key: Key = config::norm_key(k.modifiers, k.code);
        if let Some(&action) = self.cfg.binds.get(&key) {
            self.run_action(action);
        }
        false
    }

    /// Carry out a bound prefix command.
    fn run_action(&mut self, a: Action) {
        match a {
            Action::SplitH => self.split(Axis::Horizontal),
            Action::SplitV => self.split(Axis::Vertical),
            Action::NextPane => self.cycle_pane(),
            Action::LastPane => {
                if let Some(l) = self.win().last {
                    self.focus(l);
                }
            }
            Action::KillPane => {
                let id = self.win().active;
                self.remove_pane(id);
            }
            Action::KillWindow => self.kill_window(),
            Action::NewWindow => {
                if let Err(e) = self.new_window() {
                    self.msg(format!("new window failed: {e}"));
                }
            }
            Action::NextWindow => self.select_window((self.active + 1) % self.windows.len()),
            Action::PrevWindow => self.select_window((self.active + self.windows.len() - 1) % self.windows.len()),
            Action::LastWindow => {
                if let Some(l) = self.last_window {
                    self.select_window(l);
                }
            }
            Action::SelectWindow(n) => self.select_window(n as usize),
            Action::Zoom => {
                let w = self.win_mut();
                w.zoomed = !w.zoomed;
                self.apply_layout();
            }
            Action::CopyMode => self.enter_copy(),
            Action::Paste => {
                if !self.copy_buf.is_empty() {
                    let s = self.copy_buf.clone();
                    let bracketed = self.active_pane().is_some_and(|p| p.parser.lock().unwrap().screen().bracketed_paste());
                    if bracketed {
                        self.send_active(format!("\x1b[200~{s}\x1b[201~").as_bytes());
                    } else {
                        self.send_active(s.as_bytes());
                    }
                }
            }
            Action::RenameWindow => self.mode = Mode::Rename(String::new()),
            Action::Detach => {
                if let Some(id) = self.cur_client {
                    let reason = format!("[detached (from session {})]", self.name);
                    self.close_client(id, &reason);
                }
            }
            Action::ChooseSession => self.enter_choose(),
            Action::Help => self.msg(
                "% \" split | arrows move | C-arrows resize | o ; pane | x kill | z zoom | c n p l 0-9 window | & kill win | [ copy | ] paste | , rename | d detach",
            ),
        }
    }

    /// Enter copy mode: live view, cursor on the shell's cursor, no selection.
    fn enter_copy(&mut self) {
        let start = self.active_pane().map(|pane| {
            let p = pane.parser.lock().unwrap();
            let screen = p.screen();
            let (r, c) = screen.cursor_position();
            (screen.scrollback_len() + r as usize, c)
        });
        let Some((line, col)) = start else {
            self.mode = Mode::Normal;
            return;
        };
        self.set_scroll(0); // show the live view first
        self.mode = Mode::Scroll(Copy { line, col, anchor: None });
        self.dirty = true;
    }

    fn scroll_key(&mut self, k: KeyEvent, mut st: Copy) {
        // Pane geometry and history length, read once.
        let (rows, cols, cur_scroll, sb) = {
            let Some(pane) = self.active_pane() else {
                self.mode = Mode::Normal;
                return;
            };
            let p = pane.parser.lock().unwrap();
            (pane.rect.h.max(1) as usize, pane.rect.w.max(1), pane.scroll, p.screen().scrollback_len())
        };
        let max_line = sb + rows - 1; // last live row
        let page = rows.max(2) - 1;
        let half = page / 2;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        match k.code {
            KeyCode::Up | KeyCode::Char('k') => st.line = st.line.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => st.line = (st.line + 1).min(max_line),
            KeyCode::Left | KeyCode::Char('h') => st.col = st.col.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => st.col = (st.col + 1).min(cols - 1),
            KeyCode::Char('u') if ctrl => st.line = st.line.saturating_sub(half),
            KeyCode::Char('d') if ctrl => st.line = (st.line + half).min(max_line),
            KeyCode::PageUp | KeyCode::Char('b') => st.line = st.line.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char('f') => st.line = (st.line + page).min(max_line),
            KeyCode::Char('g') => st.line = 0,
            KeyCode::Char('G') => st.line = max_line,
            KeyCode::Home | KeyCode::Char('0') => st.col = 0,
            KeyCode::End | KeyCode::Char('$') => st.col = cols - 1,
            KeyCode::Char(' ') | KeyCode::Char('v') => {
                st.anchor = if st.anchor.is_some() { None } else { Some((st.line, st.col)) };
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let text = self.copy_text(&st);
                let n = text.chars().count();
                self.copy_buf = text.clone();
                let ok = crate::clip::set(&text);
                self.msg(format!("copied {n} chars{}", if ok { "" } else { " (clipboard busy)" }));
                self.mode = Mode::Normal;
                self.set_scroll(0);
                return;
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.set_scroll(0);
                self.dirty = true;
                return;
            }
            _ => {}
        }

        // Move the viewport the least amount that keeps the cursor on screen.
        let top = sb - cur_scroll;
        let new_scroll = if st.line < top {
            sb - st.line
        } else if st.line > top + rows - 1 {
            sb.saturating_sub(st.line + 1 - rows)
        } else {
            cur_scroll
        };
        self.set_scroll(new_scroll);
        self.mode = Mode::Scroll(st);
        self.dirty = true;
    }

    /// Read screen column `col`'s text for an absolute history line, one entry
    /// per column ("" for the second half of a wide character).
    fn read_abs_cols(&self, p: &mut crate::pane::Parser, sb: usize, line: usize, cols: u16) -> Vec<String> {
        let (scroll, row) = if line < sb { (sb - line, 0u16) } else { (0, (line - sb) as u16) };
        p.screen_mut().set_scrollback(scroll);
        let screen = p.screen();
        (0..cols)
            .map(|col| match screen.cell(row, col) {
                Some(cell) if cell.is_wide_continuation() => String::new(),
                Some(cell) if cell.has_contents() => cell.contents().to_string(),
                _ => " ".to_string(),
            })
            .collect()
    }

    /// The selected text (or the cursor's whole line when nothing is selected),
    /// with trailing blanks trimmed and lines joined by CR-LF.
    fn copy_text(&self, st: &Copy) -> String {
        let Some(pane) = self.active_pane() else { return String::new() };
        let cols = pane.rect.w.max(1);
        let mut p = pane.parser.lock().unwrap();
        let saved = p.screen().scrollback();
        let sb = p.screen().scrollback_len();
        let (a, b) = match st.anchor {
            Some(a) => (a, (st.line, st.col)),
            None => ((st.line, 0), (st.line, cols - 1)),
        };
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        for line in lo.0..=hi.0 {
            let row = self.read_abs_cols(&mut p, sb, line, cols);
            let start = if line == lo.0 { lo.1 as usize } else { 0 };
            let end = if line == hi.0 { hi.1 as usize } else { cols as usize - 1 };
            let seg: String = row.get(start..=end.min(row.len().saturating_sub(1))).unwrap_or(&[]).concat();
            out.push_str(seg.trim_end());
            if line != hi.0 {
                out.push_str("\r\n");
            }
        }
        p.screen_mut().set_scrollback(saved);
        out
    }

    // ---- choose session (Ctrl-b s) -------------------------------------

    fn enter_choose(&mut self) {
        let names = session::sessions();
        if names.len() <= 1 {
            self.msg("no other session");
            return;
        }
        let sel = names.iter().position(|n| *n == self.name).unwrap_or(0);
        self.mode = Mode::Choose { names, sel };
        self.dirty = true;
    }

    fn choose_key(&mut self, k: KeyEvent, names: Vec<String>, mut sel: usize) {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(names.len() - 1),
            KeyCode::Home | KeyCode::Char('g') => sel = 0,
            KeyCode::End | KeyCode::Char('G') => sel = names.len() - 1,
            KeyCode::Char(c @ '0'..='9') => {
                let i = c as usize - '0' as usize;
                if i < names.len() {
                    sel = i;
                }
            }
            KeyCode::Enter => {
                let target = names[sel].clone();
                self.mode = Mode::Normal;
                self.dirty = true;
                // Tell the client to re-attach, then drop the connection.
                if target != self.name
                    && let Some(id) = self.cur_client
                {
                    self.reply(id, proto::SWITCH, target.as_bytes());
                    self.client_gone(id);
                }
                return;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                self.dirty = true;
                return;
            }
            _ => {}
        }
        self.mode = Mode::Choose { names, sel };
        self.dirty = true;
    }

    // ---- mouse ---------------------------------------------------------

    fn pane_under(&self, x: u16, y: u16) -> Option<usize> {
        let (p, _) = self.window_layout(self.win());
        p.iter().find(|(_, r)| r.contains(x, y)).map(|x| x.0)
    }

    fn active_rect(&self) -> Option<Rect> {
        let (p, _) = self.window_layout(self.win());
        let active = self.win().active;
        p.iter().find(|(i, _)| *i == active).map(|x| x.1)
    }

    /// Map a screen cell to an (absolute line, column) in the active pane.
    fn cell_to_copy(&self, col: u16, row: u16) -> Option<(usize, u16)> {
        let rect = self.active_rect()?;
        if !rect.contains(col, row) {
            return None;
        }
        let pane = self.active_pane()?;
        let sb = pane.parser.lock().unwrap().screen().scrollback_len();
        Some(((sb - pane.scroll) + (row - rect.y) as usize, col - rect.x))
    }

    fn mouse(&mut self, m: MouseEvent) {
        let (col, row) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Mode::Scroll(mut st) = self.mode {
                    if let Some((line, c)) = self.cell_to_copy(col, row) {
                        st.line = line;
                        st.col = c;
                        st.anchor = Some((line, c));
                        self.mode = Mode::Scroll(st);
                        self.dirty = true;
                    }
                } else if let Some(id) = self.pane_under(col, row) {
                    self.focus(id);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Mode::Scroll(mut st) = self.mode
                    && let Some((line, c)) = self.cell_to_copy(col, row)
                {
                    if st.anchor.is_none() {
                        st.anchor = Some((st.line, st.col));
                    }
                    st.line = line;
                    st.col = c;
                    self.mode = Mode::Scroll(st);
                    self.dirty = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Mode::Scroll(st) = self.mode
                    && st.anchor.is_some()
                    && st.anchor != Some((st.line, st.col))
                {
                    let text = self.copy_text(&st);
                    let n = text.chars().count();
                    self.copy_buf = text.clone();
                    let ok = crate::clip::set(&text);
                    self.msg(format!("copied {n} chars{}", if ok { "" } else { " (clipboard busy)" }));
                    self.mode = Mode::Normal;
                    self.set_scroll(0);
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some(id) = self.pane_under(col, row) {
                    self.focus(id);
                }
                if !matches!(self.mode, Mode::Scroll(_)) {
                    self.enter_copy();
                }
                self.wheel(KeyCode::Up);
            }
            MouseEventKind::ScrollDown => {
                if matches!(self.mode, Mode::Scroll(_)) {
                    self.wheel(KeyCode::Down);
                }
            }
            _ => {}
        }
    }

    /// Move the copy cursor a few lines, as a wheel notch does.
    fn wheel(&mut self, code: KeyCode) {
        for _ in 0..3 {
            if let Mode::Scroll(st) = self.mode {
                self.scroll_key(KeyEvent::new(code, KeyModifiers::NONE), st);
            }
        }
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
        let copy = if let Mode::Scroll(st) = &self.mode { Some(*st) } else { None };
        let mut copy_cursor = None;

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
            let sb_len = screen.scrollback_len();
            // The selection, as absolute (line, col) bounds, for the active pane.
            let sel = if *id == win.active {
                copy.and_then(|st| {
                    st.anchor.map(|anc| {
                        let b = (st.line, st.col);
                        if anc <= b { (anc, b) } else { (b, anc) }
                    })
                })
            } else {
                None
            };
            for row in 0..rect.h {
                for col in 0..rect.w {
                    let Some(cell) = screen.cell(row, col) else { continue };
                    let mut attrs = 0;
                    if cell.bold() { attrs |= BOLD }
                    if cell.dim() { attrs |= DIM }
                    if cell.italic() { attrs |= ITALIC }
                    if cell.underline() { attrs |= UNDERLINE }
                    if cell.inverse() { attrs |= INVERSE }
                    if let Some((lo, hi)) = sel {
                        let pos = ((sb_len - pane.scroll) + row as usize, col);
                        if lo <= pos && pos <= hi {
                            attrs ^= INVERSE; // highlight, and keep already-inverse cells readable
                        }
                    }
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
            if let (true, Some(st)) = (*id == win.active, copy) {
                let top = sb_len - pane.scroll;
                if st.line >= top && st.line < top + rect.h as usize {
                    let cy = (st.line - top) as u16;
                    let cx = st.col.min(rect.w.saturating_sub(1));
                    copy_cursor = Some((rect.x + cx, rect.y + cy));
                }
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
        let bar = Style { fg: Color::Idx(self.cfg.status_fg), bg: Color::Idx(self.cfg.status_bg), attrs: 0 };
        let hi = Style { fg: Color::Idx(self.cfg.message_fg), bg: Color::Idx(self.cfg.message_bg), attrs: 0 };
        for x in 0..self.w {
            grid.set(x, y, " ", bar, false);
        }
        let plabel = match self.cfg.prefix {
            (m, KeyCode::Char(c)) if m.contains(KeyModifiers::CONTROL) => format!("^{}", c.to_ascii_uppercase()),
            (_, KeyCode::Char(c)) => c.to_string(),
            _ => "prefix".to_string(),
        };
        let mut x = match self.mode {
            Mode::Prefix | Mode::Repeat(_) => grid.text(0, y, &format!("[{plabel}] "), hi),
            Mode::Scroll(st) => {
                let s = if st.anchor.is_some() { "[copy  y:copy  q:exit] " } else { "[copy  v:select  q:exit] " };
                grid.text(0, y, s, hi)
            }
            Mode::Choose { .. } => grid.text(0, y, "[choose  enter:pick  q:exit] ", hi),
            _ => grid.text(0, y, &format!("[{}] ", self.name), bar),
        };
        if let Some((m, t)) = &self.message
            && t.elapsed() < Duration::from_secs(4)
        {
            grid.text(x, y, m, hi);
            x = self.w; // message replaces the window list
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

        // Session chooser overlay, drawn over the body.
        if let Mode::Choose { names, sel } = &self.mode {
            let title = "(choose-session)";
            let box_w = names.iter().map(|n| n.chars().count()).max().unwrap_or(0).max(title.len()) as u16 + 3;
            let norm = Style { fg: Color::Idx(self.cfg.status_fg), bg: Color::Idx(self.cfg.status_bg), attrs: 0 };
            let selbg = Style { fg: Color::Idx(self.cfg.message_fg), bg: Color::Idx(self.cfg.message_bg), attrs: 0 };
            let pad = |s: String, w: u16| {
                let mut s = s;
                while (s.chars().count() as u16) < w {
                    s.push(' ');
                }
                s
            };
            grid.text(2, 1, &pad(format!(" {title}"), box_w), norm);
            for (i, n) in names.iter().enumerate() {
                let marker = if *n == self.name { '*' } else { ' ' };
                let st = if i == *sel { selbg } else { norm };
                grid.text(2, 2 + i as u16, &pad(format!(" {marker}{n}"), box_w), st);
            }
        }

        let (cursor, shape) = match self.mode {
            Mode::Normal | Mode::Prefix | Mode::Repeat(_) => (cursor, shape),
            Mode::Scroll(_) => (copy_cursor, 2), // steady block on the copy cursor
            _ => (None, shape),
        };
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

    let cfg = config::load();
    for e in &cfg.errors {
        log!("config {e}");
    }
    let config_error = cfg.errors.first().cloned();
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
        cfg,
        copy_buf: String::new(),
        message: None,
        dirty: true,
        quit: None,
    };
    if let Some(e) = config_error {
        app.msg(format!("config {e}"));
    }
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
