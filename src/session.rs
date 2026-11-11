//! Session sockets. Same idea as tmux's /tmp/tmux-UID/: one AF_UNIX socket per
//! session, in a folder that only this user can read (%LOCALAPPDATA%\wmux).

use std::io;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use uds_windows::UnixStream;

pub fn dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let d = base.join("wmux");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn socket_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.sock"))
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub fn connect(name: &str) -> io::Result<UnixStream> {
    UnixStream::connect(socket_path(name))
}

/// Live sessions, oldest first. Removes socket files of dead servers.
pub fn sessions() -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir()) else { return Vec::new() };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("sock") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue };
        if UnixStream::connect(&path).is_ok() {
            let created = e.metadata().and_then(|m| m.created()).ok();
            found.push((created, name));
        } else {
            let _ = std::fs::remove_file(&path); // stale: server died
        }
    }
    found.sort();
    found.into_iter().map(|(_, n)| n).collect()
}

/// First free numeric name, like tmux: "0", "1", ...
pub fn next_name() -> String {
    let live = sessions();
    (0..).map(|i: u32| i.to_string()).find(|n| !live.contains(n)).unwrap()
}

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Start `wmux __server ...` as a background process with no console, so it
/// lives on after the terminal window closes. Wait until its socket is ready.
pub fn spawn_server(name: &str, w: u16, h: u16, cmd: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let build = || {
        let mut c = Command::new(&exe);
        c.arg("__server").arg(name).arg(w.to_string()).arg(h.to_string()).args(cmd);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c
    };
    // Try to leave the terminal's job object too (if any), so closing the
    // terminal cannot kill the server. Some jobs forbid this; then retry without.
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if build().creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn().is_err() {
        build().creation_flags(flags).spawn()?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if connect(name).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    bail!("server did not start (set WMUX_LOG=<file> to see why)")
}
