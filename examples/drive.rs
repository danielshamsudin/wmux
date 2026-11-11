//! End-to-end test: drive wmux clients inside pseudo consoles, like a person
//! at a terminal, and print what the screen shows.
//! Usage: cargo build && cargo run --example drive

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

fn exe() -> std::path::PathBuf {
    std::env::current_dir().unwrap().join("target/debug/wmux.exe")
}

/// A fake terminal window: a pseudo console + a vt100 screen.
struct Term {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

impl Term {
    fn open(cols: u16, rows: u16, args: &[&str]) -> Term {
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new(exe());
        cmd.args(args);
        cmd.cwd(std::env::current_dir().unwrap());
        cmd.env_remove("WMUX");
        let child = pair.slave.spawn_command(cmd).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
            0,
        )));
        {
            let (parser, writer) = (parser.clone(), writer.clone());
            spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let chunk = &buf[..n];
                    // Act as the outer terminal: answer ConPTY's cursor query.
                    if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                        let (r, c) = parser.lock().unwrap().screen().cursor_position();
                        let _ = writer.lock().unwrap().write_all(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                    }
                    parser.lock().unwrap().process(chunk);
                }
            });
        }
        Term { parser, writer, child, _master: pair.master }
    }

    fn send(&self, s: &[u8], wait_ms: u64) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(s).unwrap();
        w.flush().unwrap();
        drop(w);
        sleep(Duration::from_millis(wait_ms));
    }

    fn rows(&self) -> Vec<String> {
        let p = self.parser.lock().unwrap();
        let (_, cols) = p.screen().size();
        p.screen().rows(0, cols.get()).collect()
    }

    fn show(&self, label: &str) {
        println!("===== {label} =====");
        for r in self.rows() {
            println!("|{}", r.trim_end());
        }
    }

    fn contains(&self, s: &str) -> bool {
        self.rows().iter().any(|r| r.contains(s))
    }
}

fn wmux(args: &[&str]) -> String {
    let out = Command::new(exe()).args(args).env_remove("WMUX").output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

fn check(ok: bool, what: &str) {
    println!("[{}] {what}", if ok { "PASS" } else { "FAIL" });
}

fn main() {
    let _ = wmux(&["kill-session", "-t", "drv"]);

    // 1. New session in an 80x20 terminal; split it and leave some text.
    let mut a = Term::open(80, 20, &["new", "-s", "drv", "cmd.exe"]);
    sleep(Duration::from_millis(2000));
    a.send(b"echo BEFORE-DETACH\r", 600);
    a.send(b"\x02%", 1200);
    a.send(b"echo RIGHT-PANE\r", 600);
    a.show("A: attached, split");
    check(a.contains("BEFORE-DETACH") && a.contains("RIGHT-PANE") && a.contains("[drv]"), "A shows both panes and [drv]");

    // 2. Detach.
    a.send(b"\x02d", 1500);
    a.show("A: after Ctrl-b d");
    check(a.contains("[detached (from session drv)]"), "A prints the detach message");
    check(a.child.try_wait().unwrap().is_some(), "A client process has exited");

    // 3. Session is still alive.
    let ls = wmux(&["ls"]);
    print!("ls: {ls}");
    check(ls.contains("drv: 1 windows") && !ls.contains("(attached)"), "ls shows drv, not attached");

    // 4. Reattach from a different, bigger terminal.
    let b = Term::open(110, 30, &["attach", "-t", "drv"]);
    sleep(Duration::from_millis(1500));
    b.send(b"echo AFTER-REATTACH\r", 800);
    b.show("B: reattached at 110x30");
    check(b.contains("BEFORE-DETACH") && b.contains("RIGHT-PANE"), "B sees the old pane contents");
    check(b.contains("AFTER-REATTACH"), "B input reaches the shell");
    check(b.rows()[29].contains("[drv]"), "status bar moved to row 30 (session resized)");
    check(wmux(&["ls"]).contains("(attached)"), "ls shows (attached)");

    // 5. Kill the client hard (like closing the terminal tab). Server must live.
    let mut b = b;
    b.child.kill().unwrap();
    sleep(Duration::from_millis(800));
    let ls = wmux(&["ls"]);
    print!("ls after client killed: {ls}");
    check(ls.contains("drv:") && !ls.contains("(attached)"), "server survives a killed client");

    // 6. Stop the session.
    print!("{}", wmux(&["kill-session", "-t", "drv"]));
    sleep(Duration::from_millis(500));
    let ls = wmux(&["ls"]);
    print!("ls after kill-session: {ls}");
    check(ls.contains("no server running"), "kill-session removes it");
    drop(a);

    // 7. Default shell (pwsh / powershell with PSReadLine), with a split.
    let p = Term::open(100, 24, &["new", "-s", "drvps"]);
    sleep(Duration::from_millis(5000));
    p.send(b"Write-Output ('PS-' + 'OK')\r", 1500);
    p.send(b"\x02\"", 5000);
    p.send(b"Write-Output ('SPLIT-' + 'OK')\r", 1500);
    p.show("PowerShell session, split top/bottom");
    check(p.contains("PS-OK") && p.contains("SPLIT-OK"), "PowerShell works in both panes");
    let _ = wmux(&["kill-session", "-t", "drvps"]);
    std::process::exit(0); // do not wait for ConPTY teardown
}
