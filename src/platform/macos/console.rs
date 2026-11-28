//! macOS terminal input for interactive login.
use anyhow::{Context, Result};
use std::{io, mem::MaybeUninit};

/// Read without echo on a terminal; fall back to a plain read when redirected.
pub(crate) fn read_hidden_line(line: &mut String) -> Result<()> {
    let echo = EchoGuard::hide().context("无法访问控制台输入")?;
    let result = io::stdin().read_line(line);
    if let Some(echo) = echo {
        drop(echo);
        println!();
    }
    result.context("读取密码失败")?;
    Ok(())
}

// Ctrl+C with the default SIGINT action skips Drop and can leave echo off.
// Run `stty echo` to recover.
struct EchoGuard(libc::termios);

impl EchoGuard {
    fn hide() -> io::Result<Option<Self>> {
        // SAFETY: isatty only queries the given descriptor.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(None);
        }
        let mut original = MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes original on success.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            // Like Windows, fall back to a plain read if the mode is unavailable.
            return Ok(None);
        }
        let original = unsafe { original.assume_init() };
        let mut quiet = original;
        // The prompt prints its own newline after restoring echo.
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
        set_mode(&quiet)?;
        Ok(Some(Self(original)))
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = set_mode(&self.0);
    }
}

fn set_mode(mode: &libc::termios) -> io::Result<()> {
    loop {
        // TCSANOW changes only the mode; keep pending input like Windows.
        // SAFETY: mode is an initialized termios and lives through the call.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, mode) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
