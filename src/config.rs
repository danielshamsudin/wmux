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
    pub status_fg: u8,
    pub status_bg: u8,
    pub message_fg: u8,
    pub message_bg: u8,
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
            status_fg: 0,
            status_bg: 2,
            message_fg: 0,
            message_bg: 3,
            mouse: false,
            binds: default_binds(),
            errors: Vec::new(),
        }
    }
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
                '#' => break,
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

fn parse_color(s: &str) -> Option<u8> {
    let s = s.trim();
    let s = s.strip_prefix("colour").or_else(|| s.strip_prefix("color")).unwrap_or(s);
    if let Ok(n) = s.parse::<u8>() {
        return Some(n);
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        _ => return None,
    })
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
        "zoom" | "zoom-pane" | "resize-pane" => Action::Zoom,
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
                    "mouse" => {
                        self.mouse = match val {
                            "on" | "1" | "true" | "yes" => true,
                            "off" | "0" | "false" | "no" => false,
                            _ => return Err(format!("mouse wants on/off: {val}")),
                        }
                    }
                    _ => return Err(format!("unknown option: {name}")),
                }
            }
            "bind" | "bind-key" => {
                let key = tok.get(1).ok_or("bind needs a key")?;
                let key = parse_key(key).ok_or_else(|| format!("bad key: {key}"))?;
                let action = parse_action(&tok[2..]).ok_or_else(|| {
                    format!("unknown action: {}", tok[2..].join(" "))
                })?;
                self.binds.insert(key, action);
            }
            "unbind" | "unbind-key" => {
                let key = tok.get(1).ok_or("unbind needs a key")?;
                let key = parse_key(key).ok_or_else(|| format!("bad key: {key}"))?;
                self.binds.remove(&key);
            }
            other => return Err(format!("unknown command: {other}")),
        }
        Ok(())
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
        assert_eq!(c.status_bg, 4);
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
        let c = apply("set prefix\nbogus-command\nset repeat-time 100\n");
        assert_eq!(c.errors.len(), 2);
        assert_eq!(c.repeat_ms, 100); // the good line still applied
    }

    #[test]
    fn comments_and_blank_lines() {
        let c = apply("# a comment\n\n   \nset repeat-time 42 # trailing\n");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.repeat_ms, 42);
    }
}
