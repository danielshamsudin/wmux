//! The attach client: a thin pipe between your terminal and the server.
//!   keyboard/resize/mouse --> socket --> server
//!   server frames         --> socket --> stdout
//!
//! The client can be told to re-attach to another session (Ctrl-b s), so it
//! runs one session at a time in a loop instead of exiting on the first one.

use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::{execute, terminal};

use crate::{proto, session};

/// What ended a session from the client's point of view.
enum Flow {
    /// Detach / server stop: print this reason and quit.
    Exit(String),
    /// Re-attach to this other session.
    Switch(String),
}

fn setup_terminal() -> Result<()> {
    crossterm::ansi_support::supports_ansi(); // turns on VT output processing
    terminal::enable_raw_mode()?;
    // Write the alternate-screen enter as raw ANSI straight to stdout, exactly
    // as the server frames are written. crossterm's own screen commands can take
    // a Windows console path that does not reach a redirected stdout.
    let mut out = io::stdout();
    out.write_all(b"\x1b[?1049h")?;
    out.flush()?;
    Ok(())
}

pub fn restore_terminal() {
    // Leave the alternate screen, then reset colours and show the cursor, as raw
    // ANSI (see setup_terminal). Mouse-disable is best effort and separate, so
    // its failure cannot stop the screen being restored.
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?1049l\x1b[0m\x1b[0 q\x1b[?25h");
    let _ = out.flush();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = terminal::disable_raw_mode();
}

/// Run one attached session. Returns how it ended.
fn run_session(name: &str) -> Result<Flow> {
    let stream = session::connect(name).map_err(|_| anyhow!("can't find session: {name}"))?;
    let mut writer = stream.try_clone()?;
    let mut reader = stream;

    let (w, h) = terminal::size()?;
    proto::send(&mut writer, proto::HELLO, &proto::size(w, h))?;

    // Server -> terminal, on its own thread. It reports how the session ended
    // through the channel instead of touching the terminal or exiting.
    let (ftx, frx) = mpsc::channel::<Flow>();
    thread::spawn(move || {
        let mut out = io::stdout();
        let flow = loop {
            match proto::recv(&mut reader) {
                Ok((proto::FRAME, p)) => {
                    let _ = out.write_all(&p);
                    let _ = out.flush();
                }
                Ok((proto::MOUSEON, p)) => {
                    let _ = if p.first() == Some(&1) {
                        execute!(out, EnableMouseCapture)
                    } else {
                        execute!(out, DisableMouseCapture)
                    };
                }
                Ok((proto::EXIT, p)) => break Flow::Exit(String::from_utf8_lossy(&p).into_owned()),
                Ok((proto::SWITCH, p)) => break Flow::Switch(String::from_utf8_lossy(&p).into_owned()),
                Ok(_) => {}
                Err(_) => break Flow::Exit("[lost server]".to_string()),
            }
        };
        let _ = ftx.send(flow);
    });

    // Terminal -> server. Poll so we can also notice the session ending.
    loop {
        if let Ok(flow) = frx.try_recv() {
            return Ok(flow);
        }
        if event::poll(Duration::from_millis(50))? {
            let sent = match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => match proto::key(&k) {
                    Some(p) => proto::send(&mut writer, proto::KEY, &p),
                    None => Ok(()),
                },
                Event::Resize(w, h) => proto::send(&mut writer, proto::RESIZE, &proto::size(w, h)),
                Event::Paste(s) => proto::send(&mut writer, proto::PASTE, s.as_bytes()),
                Event::Mouse(m) => proto::send(&mut writer, proto::MOUSE, &proto::mouse(&m)),
                _ => Ok(()),
            };
            if sent.is_err() {
                // The server went away; take the reason the reader will send.
                return Ok(frx.recv().unwrap_or(Flow::Exit("[lost server]".to_string())));
            }
        }
    }
}

pub fn attach(name: &str) -> Result<()> {
    // Fail cleanly before touching the terminal if the session is not there.
    drop(session::connect(name).map_err(|_| anyhow!("can't find session: {name}"))?);

    setup_terminal()?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut target = name.to_string();
    let reason = loop {
        match run_session(&target) {
            Ok(Flow::Switch(next)) => target = next,
            Ok(Flow::Exit(r)) => break r,
            Err(e) => break format!("{e:#}"),
        }
    };

    restore_terminal();
    println!("{reason}");
    Ok(())
}
