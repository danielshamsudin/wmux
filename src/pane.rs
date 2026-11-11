//! One pane = one shell running inside a Windows pseudo console (ConPTY).
//!
//! Data flow:
//!   shell --(VT bytes)--> ConPTY --> reader thread --> vt100::Parser (screen grid)
//!   keyboard --> main loop --> writer --> ConPTY --> shell

use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;

use crate::conpty::{self, Process, PseudoConsole};
use crate::layout::Rect;
use crate::server::Event;

/// vt100 callbacks. We use them to answer terminal queries, because some
/// programs wait for an answer before they continue.
#[derive(Default)]
pub struct Term {
    replies: Vec<u8>,
}

impl vt100::Callbacks for Term {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, p0) {
            // DSR 6: "where is the cursor?"
            (None, 'n', 6) => {
                let (r, col) = screen.cursor_position();
                self.replies.extend(format!("\x1b[{};{}R", r + 1, col + 1).bytes());
            }
            // DSR 5: "are you OK?"
            (None, 'n', 5) => self.replies.extend(b"\x1b[0n"),
            // DA1: "what terminal are you?" -> VT100 with advanced video.
            (None, 'c', 0) => self.replies.extend(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

pub type Parser = vt100::Parser<Term>;

pub struct Pane {
    pub parser: Arc<Mutex<Parser>>,
    writer: Arc<Mutex<File>>,
    console: PseudoConsole,
    process: Arc<Process>,
    pub rect: Rect,
    /// Lines scrolled back from the live view (0 = live).
    pub scroll: usize,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl Pane {
    pub fn spawn(id: usize, cmd: &[String], rect: Rect, history: usize, tx: Sender<Event>) -> Result<Pane> {
        let conpty::Spawned { console, process, mut reader, writer } = conpty::spawn(cmd, rect.w, rect.h)?;
        let process = Arc::new(process);
        let writer = Arc::new(Mutex::new(writer));
        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            nz(rect.h),
            nz(rect.w),
            history,
            Term::default(),
        )));

        // Reader thread: feed shell output into the emulator.
        {
            let parser = parser.clone();
            let writer = writer.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let replies = {
                        let mut p = parser.lock().unwrap();
                        p.process(&buf[..n]);
                        std::mem::take(&mut p.callbacks_mut().replies)
                    };
                    if !replies.is_empty() {
                        let _ = writer.lock().unwrap().write_all(&replies);
                    }
                    if tx.send(Event::Output(id)).is_err() {
                        break;
                    }
                }
            });
        }

        // Wait thread: tell the main loop when the shell exits.
        {
            let process = process.clone();
            thread::spawn(move || {
                process.wait();
                let _ = tx.send(Event::Exit(id));
            });
        }

        Ok(Pane { parser, writer, console, process, rect, scroll: 0 })
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    pub fn resize(&mut self, rect: Rect) {
        if rect == self.rect {
            return;
        }
        self.rect = rect;
        self.console.resize(rect.w, rect.h);
        self.parser.lock().unwrap().set_size(nz(rect.h), nz(rect.w));
    }

    pub fn kill(&mut self) {
        self.process.kill();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.kill();
    }
}
