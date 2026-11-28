//! Windows console input for interactive login.
use anyhow::{Context, Result};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};

/// Read without echo on a console; fall back to a plain read when redirected.
pub(crate) fn read_hidden_line(line: &mut String) -> Result<()> {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("无法访问控制台输入")?;
    let mut mode = CONSOLE_MODE::default();
    let console = unsafe { GetConsoleMode(handle, &mut mode) }.is_ok();
    if console {
        let _ = unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) };
    }
    let result = std::io::stdin().read_line(line);
    if console {
        let _ = unsafe { SetConsoleMode(handle, mode) };
        println!();
    }
    result.context("读取密码失败")?;
    Ok(())
}
