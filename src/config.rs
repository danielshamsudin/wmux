//! A small tmux.conf-style config file.
//!
//! Loaded by the server (and by the CLI for `default-shell`) from
//! `%WMUX_CONFIG%`, else `%USERPROFILE%\.wmux.conf`. One command per line:
//!
//! ```text
//! # comment
//! set prefix C-a            # change the prefix key (default C-b)
//! set repeat-time 300       # ms an arrow repeat stays live
//! set history-limit 50000   # scrollback lines per pane
//! set default-shell pwsh -NoLogo
//! set status-bg 4           # status bar colours (0-255 or names)
//! set status-fg 7
//! set message-fg 7
//! set message-bg 1
//! bind | split-window -h    # rebind a prefix key to an action
//! bind _ split-window -v
//! unbind '"'
//! ```
//!
//! `set-option`/`setw` are accepted as aliases of `set`, and a leading `-g`
//! is ignored, so common tmux lines work unchanged.

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::render::Color;

/// A prefix command. Keys in the binding table map to one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SplitH,
    SplitV,
    NextPane,
    LastPane,
    KillPane,
    KillWindow,
    NewWindow,
    NextWindow,
    PrevWindow,
    LastWindow,
    SelectWindow(u8),
    Zoom,
    CopyMode,
    Paste,
    RenameWindow,
    Detach,
    ChooseSession,
    Help,
}

/// A normalized key: only Ctrl/Alt are kept, and a Ctrl+letter is lowercased,
/// so a binding matches however the terminal reports the keypress.
pub type Key = (KeyModifiers, KeyCode);

pub fn norm_key(mods: KeyModifiers, code: KeyCode) -> Key {
    let m = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT);
    let code = match code {
        KeyCode::Char(c) if m.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() => {
            KeyCode::Char(c.to_ascii_lowercase())
        }
        other => other,
    };
    (m, code)
}

pub struct Config {
    pub prefix: Key,
    /// The byte a doubled prefix sends to the shell (Ctrl+b -> 0x02).
    pub prefix_byte: Option<u8>,
    pub repeat_ms: u64,
    pub history: usize,
    pub default_shell: Option<Vec<String>>,
    pub status_fg: Color,
    pub status_bg: Color,
    pub message_fg: Color,
    pub message_bg: Color,
    /// The active window's label in the status bar.
    pub window_fg: Color,
    pub window_bg: Color,
    /// The active pane's border.
    pub border: Color,
    pub mouse: bool,
    pub binds: HashMap<Key, Action>,
    /// Problems found while parsing, "line N: ...", for the status bar.
    pub errors: Vec<String>,
}

fn default_binds() -> HashMap<Key, Action> {
    let mut b = HashMap::new();
    let n = KeyModifiers::NONE;
    let mut add = |c: char, a: Action| {
        b.insert((n, KeyCode::Char(c)), a);
    };
    add('%', Action::SplitH);
    add('|', Action::SplitH);
    add('"', Action::SplitV);
    add('-', Action::SplitV);
    add('o', Action::NextPane);
    add(';', Action::LastPane);
    add('x', Action::KillPane);
    add('&', Action::KillWindow);
    add('c', Action::NewWindow);
    add('n', Action::NextWindow);
    add('p', Action::PrevWindow);
    add('l', Action::LastWindow);
    add('z', Action::Zoom);
    add('[', Action::CopyMode);
    add(']', Action::Paste);
    add(',', Action::RenameWindow);
    add('d', Action::Detach);
    add('s', Action::ChooseSession);
    add('?', Action::Help);
    for d in 0..=9u8 {
        b.insert((n, KeyCode::Char((b'0' + d) as char)), Action::SelectWindow(d));
    }
    b
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: (KeyModifiers::CONTROL, KeyCode::Char('b')),
            prefix_byte: Some(0x02),
            repeat_ms: 500,
            history: 10_000,
            default_shell: None,
            status_fg: Color::Idx(0),
            status_bg: Color::Idx(2),
            message_fg: Color::Idx(0),
            message_bg: Color::Idx(3),
            window_fg: Color::Idx(0),
            window_bg: Color::Idx(2),
            border: Color::Idx(2),
            mouse: false,
            binds: default_binds(),
            errors: Vec::new(),
        }
    }
}

/// The colour knobs a theme sets: status fg/bg, message fg/bg, current-window
/// fg/bg, active border.
type Palette = [Color; 7];

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Built-in themes. Names are matched case-insensitively.
fn theme(name: &str) -> Option<Palette> {
    // [status_fg, status_bg, message_fg, message_bg, window_fg, window_bg, border]
    Some(match name.to_ascii_lowercase().as_str() {
        "default" => [
            Color::Idx(0), Color::Idx(2), Color::Idx(0), Color::Idx(3),
            Color::Idx(0), Color::Idx(2), Color::Idx(2),
        ],
        "nord" => [
            rgb(0xd8, 0xde, 0xe9), rgb(0x3b, 0x42, 0x52), rgb(0x2e, 0x34, 0x40), rgb(0x88, 0xc0, 0xd0),
            rgb(0x2e, 0x34, 0x40), rgb(0x88, 0xc0, 0xd0), rgb(0x88, 0xc0, 0xd0),
        ],
        "gruvbox" => [
            rgb(0xeb, 0xdb, 0xb2), rgb(0x3c, 0x38, 0x36), rgb(0x28, 0x28, 0x28), rgb(0xfa, 0xbd, 0x2f),
            rgb(0x28, 0x28, 0x28), rgb(0xb8, 0xbb, 0x26), rgb(0xb8, 0xbb, 0x26),
        ],
        "dracula" => [
            rgb(0xf8, 0xf8, 0xf2), rgb(0x44, 0x47, 0x5a), rgb(0x28, 0x2a, 0x36), rgb(0xbd, 0x93, 0xf9),
            rgb(0x28, 0x2a, 0x36), rgb(0x50, 0xfa, 0x7b), rgb(0xbd, 0x93, 0xf9),
        ],
        "catppuccin" => [
            rgb(0xcd, 0xd6, 0xf4), rgb(0x31, 0x32, 0x44), rgb(0x11, 0x11, 0x1b), rgb(0x89, 0xb4, 0xfa),
            rgb(0x11, 0x11, 0x1b), rgb(0xa6, 0xe3, 0xa1), rgb(0x89, 0xb4, 0xfa),
        ],
        "solarized" => [
            rgb(0x93, 0xa1, 0xa1), rgb(0x07, 0x36, 0x42), rgb(0x00, 0x2b, 0x36), rgb(0xb5, 0x89, 0x00),
            rgb(0x00, 0x2b, 0x36), rgb(0x85, 0x99, 0x00), rgb(0x26, 0x8b, 0xd2),
        ],
        "tokyonight" => [
            rgb(0xa9, 0xb1, 0xd6), rgb(0x24, 0x28, 0x3b), rgb(0x1a, 0x1b, 0x26), rgb(0x7a, 0xa2, 0xf7),
            rgb(0x1a, 0x1b, 0x26), rgb(0x9e, 0xce, 0x6a), rgb(0x7a, 0xa2, 0xf7),
        ],
        _ => return None,
    })
}

/// Map a tpm `@plugin 'owner/repo'` value to a built-in theme, when the repo is
/// a known theme plugin. Other plugins have no wmux equivalent.
fn theme_from_plugin(repo: &str) -> Option<&'static str> {
    let r = repo.to_ascii_lowercase();
    ["catppuccin", "nord", "dracula", "gruvbox", "tokyonight", "solarized"]
        .into_iter()
        .find(|&t| r.contains(t))
}

/// Which colour pair a style/`fg=`/`bg=` line touches.
#[derive(Clone, Copy)]
enum Slot {
    Status,
    Message,
    Window,
}

/// Where the config file lives: `$WMUX_CONFIG`, else `~/.wmux.conf`.
pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WMUX_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".wmux.conf"))
}

/// Load the config, falling back to defaults for anything unset or invalid.
pub fn load() -> Config {
    let mut cfg = Config::default();
    if let Some(p) = path()
        && let Ok(text) = std::fs::read_to_string(&p)
    {
        cfg.apply(&text);
    }
    cfg
}

/// Split a line into tokens, honouring '...' and "..." quotes. `#` starts a
/// comment except inside quotes.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None::<char>;
    let mut started = false;
    for c in line.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                // `#` starts a comment only at a token boundary, so a mid-token
                // `#` is literal (e.g. `bg=#1a1b26`). A standalone hex colour must
                // be quoted ("#1a1b26"), as in tmux.
                '#' if !started => break,
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            },
        }
    }
    if started {
        out.push(cur);
    }
    out
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    // #rrggbb (also accepts #RRGGBB) -> true colour.
    if let Some(hex) = s.strip_prefix('#')
        && hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    let body = s.strip_prefix("colour").or_else(|| s.strip_prefix("color")).unwrap_or(s);
    if let Ok(n) = body.parse::<u8>() {
        return Some(Color::Idx(n));
    }
    Some(Color::Idx(match s.to_ascii_lowercase().as_str() {
        "default" => return Some(Color::Default),
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        _ => return None,
    }))
}

/// Parse a key spec like `C-a`, `|`, `C-Space`, `Up`, `F5`.
pub fn parse_key(spec: &str) -> Option<Key> {
    let mut rest = spec;
    let mut mods = KeyModifiers::NONE;
    loop {
        if let Some(r) = rest.strip_prefix("C-").or_else(|| rest.strip_prefix("c-")) {
            mods |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-").or_else(|| rest.strip_prefix("m-")) {
            mods |= KeyModifiers::ALT;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-").or_else(|| rest.strip_prefix("s-")) {
            mods |= KeyModifiers::SHIFT;
            rest = r;
        } else {
            break;
        }
    }
    let code = match rest {
        "" => return None,
        "Space" | "space" => KeyCode::Char(' '),
        "Enter" | "enter" | "Return" => KeyCode::Enter,
        "Tab" | "tab" => KeyCode::Tab,
        "Up" | "up" => KeyCode::Up,
        "Down" | "down" => KeyCode::Down,
        "Left" | "left" => KeyCode::Left,
        "Right" | "right" => KeyCode::Right,
        "PageUp" | "PPage" => KeyCode::PageUp,
        "PageDown" | "NPage" => KeyCode::PageDown,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        s if s.len() >= 2 && (s.starts_with('F') || s.starts_with('f')) && s[1..].parse::<u8>().is_ok() => {
            KeyCode::F(s[1..].parse().unwrap())
        }
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => return None,
    };
    Some(norm_key(mods, code))
}

fn parse_action(tok: &[String]) -> Option<Action> {
    let name = tok.first()?.as_str();
    let has = |f: &str| tok.iter().any(|t| t == f);
    Some(match name {
        "split-window" | "splitw" => {
            if has("-h") {
                Action::SplitH
            } else {
                Action::SplitV // tmux default split is top/bottom
            }
        }
        "split-h" | "split-horizontal" => Action::SplitH,
        "split-v" | "split-vertical" => Action::SplitV,
        "next-pane" => Action::NextPane,
        "last-pane" => Action::LastPane,
        "kill-pane" => Action::KillPane,
        "kill-window" => Action::KillWindow,
        "new-window" | "neww" => Action::NewWindow,
        "next-window" | "next" => Action::NextWindow,
        "previous-window" | "prev" => Action::PrevWindow,
        "last-window" => Action::LastWindow,
        "select-window" | "selectw" => Action::SelectWindow(tok.get(1)?.parse().ok()?),
        "zoom" | "zoom-pane" => Action::Zoom,
        // resize-pane only maps when it is the zoom toggle (-Z); directional
        // resizes (resize-pane -L 5) are not representable, so they are ignored.
        "resize-pane" | "resizep" if has("-Z") => Action::Zoom,
        "copy-mode" | "copy" => Action::CopyMode,
        "paste" | "paste-buffer" => Action::Paste,
        "rename-window" | "rename" => Action::RenameWindow,
        "detach" | "detach-client" => Action::Detach,
        "choose-session" | "choose-tree" | "choose" => Action::ChooseSession,
        "list-keys" | "help" => Action::Help,
        _ => return None,
    })
}

impl Config {
    fn apply(&mut self, text: &str) {
        for (i, line) in text.lines().enumerate() {
            let tok = tokenize(line);
            if tok.is_empty() {
                continue;
            }
            if let Err(e) = self.command(&tok) {
                self.errors.push(format!("line {}: {e}", i + 1));
            }
        }
    }

    fn command(&mut self, tok: &[String]) -> Result<(), String> {
        match tok[0].as_str() {
            "set" | "set-option" | "setw" | "set-window-option" => {
                // Skip leading flags (-g, -a, ...), then: option name, then values.
                let mut rest = &tok[1..];
                while rest.first().is_some_and(|t| t.starts_with('-')) {
                    rest = &rest[1..];
                }
                let name = rest.first().ok_or("set needs an option name")?.as_str();
                let args = &rest[1..]; // value tokens, kept verbatim (may start with '-')
                let val = args.first().map(|s| s.as_str()).unwrap_or("");
                match name {
                    "prefix" => {
                        let key = parse_key(val).ok_or_else(|| format!("bad key: {val}"))?;
                        self.prefix = key;
                        self.prefix_byte = match key {
                            (m, KeyCode::Char(c)) if m.contains(KeyModifiers::CONTROL) && c.is_ascii() => {
                                Some((c as u8) & 0x1f)
                            }
                            _ => None,
                        };
                    }
                    "repeat-time" => self.repeat_ms = val.parse().map_err(|_| format!("bad number: {val}"))?,
                    "history-limit" => self.history = val.parse().map_err(|_| format!("bad number: {val}"))?,
                    "default-shell" | "default-command" => {
                        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
                        if v.is_empty() {
                            return Err("default-shell needs a command".into());
                        }
                        self.default_shell = Some(v);
                    }
                    "status-fg" => self.status_fg = parse_color(val).ok_or_else(|| format!("bad colour: {val}"))?,
                    "status-bg" => self.status_bg = parse_color(val).ok_or_else(|| format!("bad colour: {val}"))?,
                    "message-fg" => self.message_fg = parse_color(val).ok_or_else(|| format!("bad colour: {val}"))?,
                    "message-bg" => self.message_bg = parse_color(val).ok_or_else(|| format!("bad colour: {val}"))?,
                    // tmux combined styles: fg=... ,bg=... (hex and unknown names skipped).
                    "status-style" | "status-left-style" | "status-right-style" => {
                        self.set_style(args, Slot::Status)
                    }
                    "message-style" | "message-command-style" => self.set_style(args, Slot::Message),
                    "window-status-current-style" => self.set_style(args, Slot::Window),
                    // Themes: `set theme nord`. A named colour scheme in one line.
                    "theme" => {
                        if !self.apply_theme(val) {
                            return Err(format!("unknown theme: {val}"));
                        }
                    }
                    // tpm-style hooks: pick a built-in theme and ignore the rest.
                    "@theme" | "@wmux-theme" | "@wmux_theme" => {
                        self.apply_theme(val);
                    }
                    "@plugin" => {
                        if let Some(t) = theme_from_plugin(val) {
                            self.apply_theme(t);
                        }
                    }
                    "mouse" => {
                        self.mouse = match val {
                            "on" | "1" | "true" | "yes" => true,
                            "off" | "0" | "false" | "no" => false,
                            _ => return Err(format!("mouse wants on/off: {val}")),
                        }
                    }
                    // Any other tmux option (mode-keys, status-left, @plugin, ...) is
                    // ignored, so an existing .tmux.conf drops in without a wall of errors.
                    _ => {}
                }
            }
            "bind" | "bind-key" => {
                // Skip flags; -n or -T <non-prefix> means a non-prefix binding we
                // cannot represent, so ignore it. -r/-N and their args are skipped.
                let mut rest = &tok[1..];
                let mut prefix_table = true;
                while let Some(f) = rest.first().filter(|f| f.starts_with('-')).cloned() {
                    rest = &rest[1..];
                    match f.as_str() {
                        "-n" => prefix_table = false,
                        "-T" => {
                            if rest.first().map(|s| s.as_str()) != Some("prefix") {
                                prefix_table = false;
                            }
                            rest = &rest[1.min(rest.len())..];
                        }
                        "-N" => rest = &rest[1.min(rest.len())..], // note takes an arg
                        _ => {}
                    }
                }
                // Map what we can; an unknown key or action is ignored, not an error.
                if prefix_table
                    && let Some(key) = rest.first().and_then(|k| parse_key(k))
                    && let Some(action) = parse_action(&rest[1..])
                {
                    self.binds.insert(key, action);
                }
            }
            "unbind" | "unbind-key" => {
                let mut rest = &tok[1..];
                let (mut all, mut prefix_table) = (false, true);
                while let Some(f) = rest.first().filter(|f| f.starts_with('-')).cloned() {
                    rest = &rest[1..];
                    match f.as_str() {
                        "-a" => all = true,
                        "-n" => prefix_table = false,
                        "-T" => {
                            if rest.first().map(|s| s.as_str()) != Some("prefix") {
                                prefix_table = false;
                            }
                            rest = &rest[1.min(rest.len())..];
                        }
                        _ => {}
                    }
                }
                if all {
                    self.binds.clear();
                } else if prefix_table
                    && let Some(key) = rest.first().and_then(|k| parse_key(k))
                {
                    self.binds.remove(&key);
                }
            }
            // Any other tmux command (source-file, run-shell, if-shell, set-hook,
            // new-window, ...) is ignored for drop-in compatibility.
            _ => {}
        }
        Ok(())
    }

    /// Apply a named built-in theme. Returns false for an unknown name.
    fn apply_theme(&mut self, name: &str) -> bool {
        match theme(name) {
            Some(p) => {
                [
                    self.status_fg,
                    self.status_bg,
                    self.message_fg,
                    self.message_bg,
                    self.window_fg,
                    self.window_bg,
                    self.border,
                ] = p;
                true
            }
            None => false,
        }
    }

    /// Apply a tmux style string's `fg=`/`bg=` parts to one colour pair. Parts we
    /// cannot map (`default` aside, `bold` and other attributes) are skipped.
    fn set_style(&mut self, args: &[String], slot: Slot) {
        let (fg, bg) = match slot {
            Slot::Status => (&mut self.status_fg, &mut self.status_bg),
            Slot::Message => (&mut self.message_fg, &mut self.message_bg),
            Slot::Window => (&mut self.window_fg, &mut self.window_bg),
        };
        for part in args.iter().flat_map(|a| a.split(',')) {
            if let Some(v) = part.strip_prefix("fg=")
                && let Some(c) = parse_color(v)
            {
                *fg = c;
            } else if let Some(v) = part.strip_prefix("bg=")
                && let Some(c) = parse_color(v)
            {
                *bg = c;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str) -> Config {
        let mut c = Config::default();
        c.apply(text);
        c
    }

    #[test]
    fn options() {
        let c = apply("set prefix C-a\nset -g repeat-time 300\nset history-limit 50000\nset status-bg blue\n");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.prefix, (KeyModifiers::CONTROL, KeyCode::Char('a')));
        assert_eq!(c.prefix_byte, Some(0x01));
        assert_eq!(c.repeat_ms, 300);
        assert_eq!(c.history, 50_000);
        assert_eq!(c.status_bg, Color::Idx(4));
    }

    #[test]
    fn themes_and_hex() {
        let c = apply("set theme nord\n");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.status_bg, Color::Rgb(0x3b, 0x42, 0x52));
        assert_eq!(c.border, Color::Rgb(0x88, 0xc0, 0xd0));

        // An explicit colour after the theme overrides just that slot. A bare
        // hex value must be quoted so its `#` is not read as a comment; a hex
        // inside a style string (bg=#...) needs no quotes.
        let c = apply("set theme nord\nset status-bg red\nset message-bg \"#1a2b3c\"\nset status-style bg=#102030\n");
        assert_eq!(c.status_bg, Color::Rgb(0x10, 0x20, 0x30));
        assert_eq!(c.message_bg, Color::Rgb(0x1a, 0x2b, 0x3c));

        // An unknown theme name is reported (it is a wmux-native setting).
        let c = apply("set theme nope\n");
        assert_eq!(c.errors.len(), 1);
    }

    #[test]
    fn tpm_theme_plugins_map_to_a_theme() {
        let c = apply(
            "set -g @plugin 'catppuccin/tmux'\n\
             set -g @plugin 'tmux-plugins/tmux-resurrect'\n\
             run '~/.tmux/plugins/tpm/tpm'\n",
        );
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.status_bg, Color::Rgb(0x31, 0x32, 0x44)); // catppuccin
        let c = apply("set -g @wmux_theme dracula\n");
        assert_eq!(c.border, Color::Rgb(0xbd, 0x93, 0xf9));
    }

    #[test]
    fn default_shell_with_args() {
        let c = apply("set default-shell pwsh -NoLogo -NoProfile\n");
        assert_eq!(c.default_shell.unwrap(), vec!["pwsh", "-NoLogo", "-NoProfile"]);
    }

    #[test]
    fn binds() {
        let c = apply("bind v split-window -h\nunbind '\"'\nbind C-z zoom\n");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.binds.get(&(KeyModifiers::NONE, KeyCode::Char('v'))), Some(&Action::SplitH));
        assert_eq!(c.binds.get(&(KeyModifiers::NONE, KeyCode::Char('"'))), None);
        assert_eq!(c.binds.get(&(KeyModifiers::CONTROL, KeyCode::Char('z'))), Some(&Action::Zoom));
    }

    #[test]
    fn errors_are_collected_not_fatal() {
        // A malformed value for a supported option is reported; an unknown
        // command is ignored (drop-in). The good line still applies.
        let c = apply("set prefix\nbogus-command\nset repeat-time 100\n");
        assert_eq!(c.errors.len(), 1);
        assert_eq!(c.repeat_ms, 100);
    }

    #[test]
    fn tmux_conf_drops_in_without_errors() {
        let conf = "\
# a real-ish .tmux.conf
set -g prefix C-a
unbind C-b
set -g mouse on
setw -g mode-keys vi
set -g history-limit 20000
set -g status-style fg=white,bg=colour236
set -g message-style bg=red,fg=brightwhite
set -g status-left '#S '
bind -r H resize-pane -L 5
bind -n M-Left select-pane -L
bind -T copy-mode-vi v send -X begin-selection
bind | split-window -h
bind - split-window -v
bind r source-file ~/.tmux.conf
set -g @plugin 'tmux-plugins/tpm'
run -b '~/.tmux/plugins/tpm/tpm'
";
        let c = apply(conf);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.prefix, (KeyModifiers::CONTROL, KeyCode::Char('a')));
        assert!(c.mouse);
        assert_eq!(c.history, 20_000);
        assert_eq!(c.status_fg, Color::Idx(7)); // white
        assert_eq!(c.status_bg, Color::Idx(236)); // colour236
        assert_eq!(c.message_bg, Color::Idx(1)); // red (brightwhite unknown -> fg unchanged)
        // Supported binds applied; unmappable/non-prefix ones silently skipped.
        assert_eq!(c.binds.get(&(KeyModifiers::NONE, KeyCode::Char('|'))), Some(&Action::SplitH));
        assert_eq!(c.binds.get(&(KeyModifiers::NONE, KeyCode::Char('-'))), Some(&Action::SplitV));
        assert_eq!(c.binds.get(&(KeyModifiers::NONE, KeyCode::Char('H'))), None);
        assert_eq!(c.binds.get(&(KeyModifiers::NONE, KeyCode::Char('r'))), None);
    }

    #[test]
    fn comments_and_blank_lines() {
        let c = apply("# a comment\n\n   \nset repeat-time 42 # trailing\n");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.repeat_ms, 42);
    }
}
