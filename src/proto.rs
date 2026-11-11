//! Client/server wire protocol.
//!
//! Each message: [tag: u8][len: u32 little endian][payload: len bytes].

use std::io::{self, Read, Write};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

// client -> server
pub const HELLO: u8 = 1; // attach; payload = size
pub const KEY: u8 = 2; // payload = key
pub const RESIZE: u8 = 3; // payload = size
pub const PASTE: u8 = 4; // payload = utf-8 text
pub const LIST: u8 = 5; // ask for a one-line session summary
pub const KILL: u8 = 6; // stop the session

// server -> client
pub const FRAME: u8 = 101; // raw VT bytes to write to the terminal
pub const EXIT: u8 = 102; // payload = reason, e.g. "[detached (from session 0)]"
pub const TEXT: u8 = 103; // reply to LIST / KILL

const MAX_LEN: usize = 64 << 20;

pub fn encode(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(tag);
    buf.extend((payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub fn send(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&encode(tag, payload))?;
    w.flush()
}

pub fn recv(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

pub fn size(w: u16, h: u16) -> Vec<u8> {
    let mut v = w.to_le_bytes().to_vec();
    v.extend(h.to_le_bytes());
    v
}

pub fn parse_size(p: &[u8]) -> Option<(u16, u16)> {
    if p.len() != 4 {
        return None;
    }
    Some((u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]])))
}

/// Key layout: [code tag u8][value u32][modifier bits u8][kind u8]
pub fn key(k: &KeyEvent) -> Option<Vec<u8>> {
    let (tag, val): (u8, u32) = match k.code {
        KeyCode::Char(c) => (0, c as u32),
        KeyCode::F(n) => (1, n as u32),
        KeyCode::Enter => (2, 0),
        KeyCode::Tab => (3, 0),
        KeyCode::BackTab => (4, 0),
        KeyCode::Backspace => (5, 0),
        KeyCode::Esc => (6, 0),
        KeyCode::Up => (7, 0),
        KeyCode::Down => (8, 0),
        KeyCode::Left => (9, 0),
        KeyCode::Right => (10, 0),
        KeyCode::Home => (11, 0),
        KeyCode::End => (12, 0),
        KeyCode::Insert => (13, 0),
        KeyCode::Delete => (14, 0),
        KeyCode::PageUp => (15, 0),
        KeyCode::PageDown => (16, 0),
        _ => return None,
    };
    let kind = match k.kind {
        KeyEventKind::Press => 0,
        KeyEventKind::Repeat => 1,
        KeyEventKind::Release => 2,
    };
    let mut v = vec![tag];
    v.extend(val.to_le_bytes());
    v.push(k.modifiers.bits());
    v.push(kind);
    Some(v)
}

pub fn parse_key(p: &[u8]) -> Option<KeyEvent> {
    if p.len() != 7 {
        return None;
    }
    let val = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
    let code = match p[0] {
        0 => KeyCode::Char(char::from_u32(val)?),
        1 => KeyCode::F(val as u8),
        2 => KeyCode::Enter,
        3 => KeyCode::Tab,
        4 => KeyCode::BackTab,
        5 => KeyCode::Backspace,
        6 => KeyCode::Esc,
        7 => KeyCode::Up,
        8 => KeyCode::Down,
        9 => KeyCode::Left,
        10 => KeyCode::Right,
        11 => KeyCode::Home,
        12 => KeyCode::End,
        13 => KeyCode::Insert,
        14 => KeyCode::Delete,
        15 => KeyCode::PageUp,
        16 => KeyCode::PageDown,
        _ => return None,
    };
    let kind = match p[6] {
        0 => KeyEventKind::Press,
        1 => KeyEventKind::Repeat,
        _ => KeyEventKind::Release,
    };
    Some(KeyEvent::new_with_kind(code, KeyModifiers::from_bits_truncate(p[5]), kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let k = KeyEvent::new(KeyCode::Char('é'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(parse_key(&key(&k).unwrap()), Some(k));
        let k = KeyEvent::new(KeyCode::F(11), KeyModifiers::SHIFT);
        assert_eq!(parse_key(&key(&k).unwrap()), Some(k));
        let mut buf = Vec::new();
        send(&mut buf, FRAME, b"hello").unwrap();
        assert_eq!(recv(&mut &buf[..]).unwrap(), (FRAME, b"hello".to_vec()));
        assert_eq!(parse_size(&size(120, 40)), Some((120, 40)));
    }
}
