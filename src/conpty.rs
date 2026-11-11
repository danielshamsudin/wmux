//! Minimal Windows pseudo console (ConPTY). This is the Windows version of a
//! Unix pty pair:
//!
//!   we write keys --> [input pipe]  --> ConPTY --> console app (shell)
//!   we read VT    <-- [output pipe] <-- ConPTY <-- console app
//!
//! We pass flags = 0 to CreatePseudoConsole on purpose:
//!   - no PSEUDOCONSOLE_RESIZE_QUIRK: after a resize ConPTY repaints the whole
//!     screen. Our vt100 emulator does not reflow text, so it needs that repaint.
//!   - no PSEUDOCONSOLE_INHERIT_CURSOR: ConPTY does not send a startup cursor
//!     query and wait for the answer.

use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

fn coord(cols: u16, rows: u16) -> COORD {
    COORD { X: cols.max(1) as i16, Y: rows.max(1) as i16 }
}

fn raw(h: &OwnedHandle) -> HANDLE {
    h.as_raw_handle() as HANDLE
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w): (HANDLE, HANDLE) = (null_mut(), null_mut());
    if unsafe { CreatePipe(&mut r, &mut w, null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { Ok((OwnedHandle::from_raw_handle(r as RawHandle), OwnedHandle::from_raw_handle(w as RawHandle))) }
}

/// Quote one argument by the MSVC command-line rules.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut s = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        let n = if c == '"' { slashes * 2 + 1 } else { slashes };
        s.extend(std::iter::repeat_n('\\', n));
        slashes = 0;
        s.push(c);
    }
    s.extend(std::iter::repeat_n('\\', slashes * 2));
    s.push('"');
    s
}

pub struct PseudoConsole(HPCON);

impl PseudoConsole {
    pub fn resize(&self, cols: u16, rows: u16) {
        unsafe { ResizePseudoConsole(self.0, coord(cols, rows)) };
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // Ends the attached processes and closes ConPTY's end of the pipes,
        // so our reader thread gets end-of-file.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

pub struct Process(OwnedHandle);

impl Process {
    pub fn wait(&self) {
        unsafe { WaitForSingleObject(raw(&self.0), INFINITE) };
    }
    pub fn kill(&self) {
        unsafe { TerminateProcess(raw(&self.0), 1) };
    }
}

pub struct Spawned {
    pub console: PseudoConsole,
    pub process: Process,
    /// Read VT output from here.
    pub reader: File,
    /// Write VT input (keys) here.
    pub writer: File,
}

/// Run `cmd` attached to a new pseudo console of size cols x rows.
/// The child inherits this process's environment and current directory.
pub fn spawn(cmd: &[String], cols: u16, rows: u16) -> io::Result<Spawned> {
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;

    let mut hpc: HPCON = 0;
    let hr = unsafe { CreatePseudoConsole(coord(cols, rows), raw(&in_read), raw(&out_write), 0, &mut hpc) };
    if hr != 0 {
        return Err(io::Error::other(format!("CreatePseudoConsole failed: HRESULT {hr:#x}")));
    }
    let console = PseudoConsole(hpc);

    // Attribute list with one entry: "attach the child to this pseudo console".
    let mut size = 0usize;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
    let mut buf = vec![0u64; size.div_ceil(8)]; // 8-byte aligned storage
    let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let ok = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const _, // the value is the HPCON itself, not a pointer to it
            size_of::<HPCON>(),
            null_mut(),
            null(),
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(list) };
        return Err(e);
    }

    let mut si: STARTUPINFOEXW = unsafe { zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    // Without this, a child of a process with redirected stdio (our server:
    // stdio = NUL) inherits those handles instead of the pseudo console.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = list;

    let line = cmd.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    let mut wide: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    let ok = unsafe {
        CreateProcessW(
            null(),
            wide.as_mut_ptr(),
            null(),
            null(),
            0, // do not inherit handles
            EXTENDED_STARTUPINFO_PRESENT,
            null(), // inherit environment
            null(), // inherit current directory
            &si.StartupInfo,
            &mut pi,
        )
    };
    let err = io::Error::last_os_error();
    unsafe { DeleteProcThreadAttributeList(list) };
    if ok == 0 {
        return Err(io::Error::new(err.kind(), format!("cannot start `{line}`: {err}")));
    }
    let process = unsafe {
        drop(OwnedHandle::from_raw_handle(pi.hThread as RawHandle));
        Process(OwnedHandle::from_raw_handle(pi.hProcess as RawHandle))
    };

    // ConPTY has its own copies of its pipe ends; close ours.
    drop(in_read);
    drop(out_write);

    Ok(Spawned { console, process, reader: File::from(out_read), writer: File::from(in_write) })
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quoting() {
        assert_eq!(quote("cmd.exe"), "cmd.exe");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(""), "\"\"");
    }
}
