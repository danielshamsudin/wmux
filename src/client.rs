//! The attach client: a thin pipe between your terminal and the server.
//!   keyboard/resize --> socket --> server
//!   server frames   --> socket --> stdout

use std::io::{self, Write};
use std::thread;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{execute, style::Print, terminal};

use crate::{proto, session};

fn setup_terminal() -> Result<()> {
    crossterm::ansi_support::supports_ansi(); // turns on VT output processing
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen)?;
    Ok(())
}

pub fn restore_terminal() {
    let _ = execute!(io::stdout(), Print("\x1b[0m\x1b[0 q\x1b[?25h"), terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

pub fn attach(name: &str) -> Result<()> {
    let stream = session::connect(name).map_err(|_| anyhow::anyhow!("can't find session: {name}"))?;
    let mut writer = stream.try_clone()?;
    let mut reader = stream;

    let (w, h) = terminal::size()?;
    setup_terminal()?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    proto::send(&mut writer, proto::HELLO, &proto::size(w, h))?;

    // Server -> terminal. This thread also ends the process.
    thread::spawn(move || {
        let mut out = io::stdout();
        let reason = loop {
            match proto::recv(&mut reader) {
                Ok((proto::FRAME, p)) => {
                    let _ = out.write_all(&p);
                    let _ = out.flush();
                }
                Ok((proto::EXIT, p)) => break String::from_utf8_lossy(&p).into_owned(),
                Ok(_) => {}
                Err(_) => break "[lost server]".to_string(),
            }
        };
        restore_terminal();
        println!("{reason}");
        std::process::exit(0);
    });

    // Terminal -> server.
    loop {
        let sent = match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => match proto::key(&k) {
                Some(p) => proto::send(&mut writer, proto::KEY, &p),
                None => Ok(()),
            },
            Event::Resize(w, h) => proto::send(&mut writer, proto::RESIZE, &proto::size(w, h)),
            Event::Paste(s) => proto::send(&mut writer, proto::PASTE, s.as_bytes()),
            _ => Ok(()),
        };
        if sent.is_err() {
            // The reader thread prints the reason and exits; wait for it.
            thread::park();
        }
    }
}
