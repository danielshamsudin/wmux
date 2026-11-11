//! wmux: a small tmux-style terminal multiplexer for Windows.
//!
//!   wmux                         new session, attach
//!   wmux new [-s name] [-d] [command...]
//!   wmux attach [-t name]        (also: a, at)
//!   wmux ls
//!   wmux kill-session [-t name]
//!   wmux kill-server

/// Debug log. Set WMUX_LOG=<file> to write trace lines (server and client).
macro_rules! log {
    ($($t:tt)*) => {
        if let Some(path) = std::env::var_os("WMUX_LOG") {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                use std::io::Write as _;
                let _ = writeln!(f, "{} [{}] {}", chrono::Local::now().format("%H:%M:%S%.3f"), std::process::id(), format!($($t)*));
            }
        }
    };
}

mod clip;
mod client;
mod conpty;
mod keys;
mod layout;
mod pane;
mod proto;
mod render;
mod server;
mod session;

use std::time::Duration;

use anyhow::{bail, Result};

const USAGE: &str = "usage:
  wmux                                 new session and attach
  wmux new [-s name] [-d] [command...] new session (-d: do not attach)
  wmux attach [-t name]                attach (aliases: a, at)
  wmux ls                              list sessions
  wmux kill-session [-t name]          stop a session
  wmux kill-server                     stop all sessions
  --force (first argument)             allow use inside a wmux pane";

fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
        .unwrap_or(false)
}

fn default_shell() -> Vec<String> {
    if let Ok(s) = std::env::var("WMUX_SHELL") {
        let v: Vec<String> = s.split_whitespace().map(String::from).collect();
        if !v.is_empty() {
            return v;
        }
    }
    for exe in ["pwsh.exe", "powershell.exe"] {
        if on_path(exe) {
            return vec![exe.into()];
        }
    }
    vec![std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())]
}

fn check_nested(force: bool) -> Result<()> {
    if std::env::var_os("WMUX").is_some() && !force {
        bail!("sessions should be nested with care (use --force to nest)");
    }
    Ok(())
}

/// Read "-t name" from args, else use the newest live session.
fn target(args: &[String]) -> Result<String> {
    if let Some(i) = args.iter().position(|a| a == "-t") {
        return args.get(i + 1).cloned().ok_or_else(|| anyhow::anyhow!("-t needs a session name"));
    }
    match session::sessions().pop() {
        Some(n) => Ok(n),
        None => bail!("no sessions"),
    }
}

/// Send one request to a session and wait for its TEXT reply.
fn request(name: &str, tag: u8) -> Result<String> {
    let mut s = session::connect(name).map_err(|_| anyhow::anyhow!("can't find session: {name}"))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    proto::send(&mut s, tag, &[])?;
    loop {
        let (t, p) = proto::recv(&mut s)?;
        if t == proto::TEXT {
            return Ok(String::from_utf8_lossy(&p).into_owned());
        }
    }
}

fn new_session(args: &[String], force: bool) -> Result<()> {
    let (mut name, mut detached, mut i) = (None, false, 0);
    while i < args.len() {
        match args[i].as_str() {
            "-s" => {
                name = args.get(i + 1).cloned();
                i += 2;
            }
            "-d" => {
                detached = true;
                i += 1;
            }
            "--" => {
                i += 1;
                break;
            }
            a if a.starts_with('-') => bail!("unknown option: {a}\n{USAGE}"),
            _ => break,
        }
    }
    let cmd: Vec<String> = if i < args.len() { args[i..].to_vec() } else { default_shell() };
    if !detached {
        check_nested(force)?;
    }
    let name = name.unwrap_or_else(session::next_name);
    if !session::valid_name(&name) {
        bail!("bad session name: {name} (use letters, digits, - _ .)");
    }
    if session::sessions().contains(&name) {
        bail!("duplicate session: {name}");
    }
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    session::spawn_server(&name, w, h, &cmd)?;
    if detached {
        return Ok(());
    }
    client::attach(&name)
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let force = args.first().is_some_and(|a| a == "--force");
    if force {
        args.remove(0);
    }
    let sub = args.first().cloned().unwrap_or_default();
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub.as_str() {
        "" => new_session(&[], force),
        "new" | "new-session" => new_session(rest, force),
        "attach" | "attach-session" | "a" | "at" => {
            check_nested(force)?;
            client::attach(&target(rest)?)
        }
        "ls" | "list-sessions" => {
            let all = session::sessions();
            if all.is_empty() {
                bail!("no server running on {}", session::dir().display());
            }
            for name in all {
                match request(&name, proto::LIST) {
                    Ok(line) => println!("{line}"),
                    Err(e) => println!("{name}: not responding ({e})"),
                }
            }
            Ok(())
        }
        "kill-session" => {
            request(&target(rest)?, proto::KILL)?;
            Ok(())
        }
        "kill-server" => {
            for name in session::sessions() {
                let _ = request(&name, proto::KILL);
            }
            Ok(())
        }
        "__server" => {
            let name = rest.first().cloned().unwrap_or_default();
            let w = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(80);
            let h = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(24);
            let cmd = if rest.len() > 3 { rest[3..].to_vec() } else { default_shell() };
            server::run(name, w, h, cmd)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command: {other}\n{USAGE}"),
    }
}

fn main() {
    if let Err(e) = run() {
        log!("error: {e:#}");
        eprintln!("wmux: {e:#}");
        std::process::exit(1);
    }
}
