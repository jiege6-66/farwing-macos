//! `dev input`: scripted keyboard and mouse input over a real controller
//! session, for verifying host-side injection (`docs/protocol/v2-design.md`
//! §7) without a viewer window.
//!
//! Keys are named by their physical position on a US layout and sent as HID
//! usages, just like viewer input, so the host's layout and IME decide what
//! they produce.
use crate::features::remote_input::keys::{CONSUMER as C, KEYBOARD as K};
use crate::features::remote_input::{MouseMode, RemoteInput};
use crate::protocol::input::SystemCommand;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::time::Duration;

/// Input owner of the script (a viewer uses its window handle).
const OWNER: u64 = 0x6465_7669;
const LEFT_SHIFT: u32 = K | 0xe1;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Key {
    pub name: String,
    pub usage: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl Button {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "left" => Self::Left,
            "right" => Self::Right,
            "middle" => Self::Middle,
            "x1" => Self::X1,
            "x2" => Self::X2,
            other => bail!("未知鼠标按钮 {other}（left/right/middle/x1/x2）"),
        })
    }
    /// Button mask of `MouseButton` (§7).
    fn mask(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 16,
            Self::X1 => 32,
            Self::X2 => 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    /// US-layout text; `\n` is Enter and `\t` Tab.
    Text(String),
    /// Keys pressed in order and released in reverse.
    Chord(Vec<Key>),
    Down(Key),
    Up(Key),
    Repeat { key: Key, count: u32, interval: Duration },
    Lock([bool; 3]),
    Move(f64, f64),
    Rel(i32, i32),
    Click(Button, u32),
    Press(Button),
    Release(Button),
    Wheel { dy: i32, dx: i32 },
    System(SystemCommand),
    Sleep(Duration),
    #[cfg(windows)]
    Pull,
}

/// HID usage of a US-layout character and whether it needs Shift.
fn char_usage(c: char) -> Option<(u32, bool)> {
    let (id, shift) = match c {
        'a'..='z' => (0x04 + u32::from(c as u8 - b'a'), false),
        'A'..='Z' => (0x04 + u32::from(c as u8 - b'A'), true),
        '1'..='9' => (0x1e + u32::from(c as u8 - b'1'), false),
        '0' => (0x27, false),
        ')' => (0x27, true),
        '!' => (0x1e, true),
        '@' => (0x1f, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        '-' => (0x2d, false),
        '_' => (0x2d, true),
        '=' => (0x2e, false),
        '+' => (0x2e, true),
        '[' => (0x2f, false),
        '{' => (0x2f, true),
        ']' => (0x30, false),
        '}' => (0x30, true),
        '\\' => (0x31, false),
        '|' => (0x31, true),
        ';' => (0x33, false),
        ':' => (0x33, true),
        '\'' => (0x34, false),
        '"' => (0x34, true),
        '`' => (0x35, false),
        '~' => (0x35, true),
        ',' => (0x36, false),
        '<' => (0x36, true),
        '.' => (0x37, false),
        '>' => (0x37, true),
        '/' => (0x38, false),
        '?' => (0x38, true),
        ' ' => (0x2c, false),
        '\n' => (0x28, false),
        '\t' => (0x2b, false),
        _ => return None,
    };
    Some((K | id, shift))
}

/// HID usage of a named key, including consumer-page media keys.
fn named_usage(name: &str) -> Option<u32> {
    Some(match name {
        "enter" | "return" => K | 0x28,
        "esc" | "escape" => K | 0x29,
        "tab" => K | 0x2b,
        "backspace" => K | 0x2a,
        "space" => K | 0x2c,
        "equal" => K | 0x2e,
        "capslock" => K | 0x39,
        "numlock" => K | 0x53,
        "scrolllock" => K | 0x47,
        "pause" => K | 0x48,
        "printscreen" => K | 0x46,
        "insert" => K | 0x49,
        "delete" | "del" => K | 0x4c,
        "home" => K | 0x4a,
        "end" => K | 0x4d,
        "pageup" => K | 0x4b,
        "pagedown" => K | 0x4e,
        "up" => K | 0x52,
        "down" => K | 0x51,
        "left" => K | 0x50,
        "right" => K | 0x4f,
        "apps" | "menu" => K | 0x65,
        "ctrl" | "lctrl" => K | 0xe0,
        "rctrl" => K | 0xe4,
        "shift" | "lshift" => LEFT_SHIFT,
        "rshift" => K | 0xe5,
        "alt" | "lalt" => K | 0xe2,
        "ralt" | "altgr" => K | 0xe6,
        "win" | "lwin" => K | 0xe3,
        "rwin" => K | 0xe7,
        "volumeup" => C | 0xe9,
        "volumedown" => C | 0xea,
        "mute" | "volumemute" => C | 0xe2,
        "mediaplaypause" | "playpause" => C | 0xcd,
        "mediastop" => C | 0xb7,
        "medianext" | "nexttrack" => C | 0xb5,
        "mediaprev" | "prevtrack" => C | 0xb6,
        "kpenter" => K | 0x58,
        "kpplus" => K | 0x57,
        "kpminus" => K | 0x56,
        "kpmultiply" => K | 0x55,
        "kpdivide" => K | 0x54,
        "kpdecimal" => K | 0x63,
        "kp0" => K | 0x62,
        "kp1" => K | 0x59,
        "kp2" => K | 0x5a,
        "kp3" => K | 0x5b,
        "kp4" => K | 0x5c,
        "kp5" => K | 0x5d,
        "kp6" => K | 0x5e,
        "kp7" => K | 0x5f,
        "kp8" => K | 0x60,
        "kp9" => K | 0x61,
        _ => {
            let number = name.strip_prefix('f')?.parse::<u16>().ok()?;
            match number {
                1..=12 => K | (0x39 + u32::from(number)),
                _ => return None,
            }
        }
    })
}

/// A key by name, or a single character key (Shift not included: `key a`
/// presses the A key; use `text A` or `key shift+a` for a capital).
fn parse_key(name: &str) -> Result<Key> {
    let lower = name.to_ascii_lowercase();
    let usage = named_usage(&lower)
        .or_else(|| {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => char_usage(c.to_ascii_lowercase()).map(|(usage, _)| usage),
                _ => None,
            }
        })
        .with_context(|| format!("未知按键 {name}"))?;
    Ok(Key { name: lower, usage })
}

/// The keys of one character of `text`: optional Shift, then the key.
pub(crate) fn char_keys(c: char) -> Result<(Option<u32>, u32)> {
    let (usage, shift) = char_usage(c).with_context(|| format!("无法按美式键位输入字符 {c:?}"))?;
    Ok((shift.then_some(LEFT_SHIFT), usage))
}

fn unescape(text: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            other => bail!("无效转义 \\{}", other.map(String::from).unwrap_or_default()),
        }
    }
    Ok(out)
}

fn on_off(value: &str) -> Result<bool> {
    match value {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        other => bail!("锁定状态应为 on/off：{other}"),
    }
}

fn number<T: std::str::FromStr>(word: Option<&str>, what: &str) -> Result<T> {
    word.with_context(|| format!("缺少{what}"))?
        .parse()
        .map_err(|_| anyhow!("{what}无效"))
}

fn parse_line(line: &str, allow_disruptive: bool) -> Result<Option<Step>> {
    let line = line.trim_end_matches(['\r', '\n']);
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    // `text` keeps its argument verbatim, including spaces.
    if let Some(text) = trimmed.strip_prefix("text ") {
        return Ok(Some(Step::Text(unescape(text)?)));
    }
    let mut words = trimmed.split_whitespace();
    let command = words.next().unwrap_or_default();
    let step = match command {
        "key" => {
            let chord = words.next().context("缺少按键")?;
            let keys = chord.split('+').map(parse_key).collect::<Result<Vec<_>>>()?;
            ensure!(!keys.is_empty(), "缺少按键");
            Step::Chord(keys)
        }
        "down" => Step::Down(parse_key(words.next().context("缺少按键")?)?),
        "up" => Step::Up(parse_key(words.next().context("缺少按键")?)?),
        "repeat" => Step::Repeat {
            key: parse_key(words.next().context("缺少按键")?)?,
            count: number(words.next(), "次数")?,
            interval: Duration::from_millis(number(words.next(), "间隔")?),
        },
        "lock" => {
            let mut state = [false; 3];
            for word in words.by_ref() {
                let (name, value) = word.split_once('=').context("锁定状态应写成 caps=on")?;
                let index = match name {
                    "caps" => 0,
                    "num" => 1,
                    "scroll" => 2,
                    other => bail!("未知锁定键 {other}"),
                };
                state[index] = on_off(value)?;
            }
            Step::Lock(state)
        }
        "move" => {
            let x: f64 = number(words.next(), "横坐标")?;
            let y: f64 = number(words.next(), "纵坐标")?;
            ensure!(
                (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                "坐标应在 0..1 之间"
            );
            Step::Move(x, y)
        }
        "rel" => Step::Rel(number(words.next(), "横向位移")?, number(words.next(), "纵向位移")?),
        "click" => {
            let button = Button::parse(words.next().unwrap_or("left"))?;
            let count = words.next().map(str::parse).transpose().map_err(|_| anyhow!("次数无效"))?;
            Step::Click(button, count.unwrap_or(1))
        }
        "press" => Step::Press(Button::parse(words.next().unwrap_or("left"))?),
        "release" => Step::Release(Button::parse(words.next().unwrap_or("left"))?),
        "wheel" => Step::Wheel {
            dy: number(words.next(), "滚动量")?,
            dx: words.next().map(str::parse).transpose().map_err(|_| anyhow!("横向滚动量无效"))?.unwrap_or(0),
        },
        "system" => {
            let command = match words.next().context("缺少系统命令")? {
                "taskmgr" => SystemCommand::TaskManager,
                "desktop" => SystemCommand::ShowDesktop,
                "taskview" => SystemCommand::TaskView,
                "sas" => SystemCommand::Sas,
                "lock" => SystemCommand::Lock,
                other => bail!("未知系统命令 {other}"),
            };
            ensure!(
                allow_disruptive || !matches!(command, SystemCommand::Sas | SystemCommand::Lock),
                "sas / lock 会锁定本机桌面，需要 --allow-disruptive"
            );
            Step::System(command)
        }
        "sleep" => Step::Sleep(Duration::from_millis(number(words.next(), "毫秒数")?)),
        #[cfg(windows)]
        "pull" => Step::Pull,
        other => bail!("未知命令 {other}"),
    };
    ensure!(words.next().is_none(), "多余的参数");
    Ok(Some(step))
}

/// Parse a script: one step per line, `#` comments; errors name the line.
pub(crate) fn parse_script(text: &str, allow_disruptive: bool) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if let Some(step) =
            parse_line(line, allow_disruptive).with_context(|| format!("第 {} 行：{line}", index + 1))?
        {
            steps.push(step);
        }
    }
    Ok(steps)
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |keys: &[Key]| keys.iter().map(|k| k.name.as_str()).collect::<Vec<_>>().join("+");
        match self {
            Self::Text(text) => write!(f, "text {text:?}"),
            Self::Chord(keys) => write!(f, "key {}", names(keys)),
            Self::Down(key) => write!(f, "down {} ({:#x})", key.name, key.usage),
            Self::Up(key) => write!(f, "up {} ({:#x})", key.name, key.usage),
            Self::Repeat { key, count, interval } => {
                write!(f, "repeat {} ×{count} every {} ms", key.name, interval.as_millis())
            }
            Self::Lock([caps, num, scroll]) => write!(f, "lock caps={caps} num={num} scroll={scroll}"),
            Self::Move(x, y) => write!(f, "move {x:.4} {y:.4}"),
            Self::Rel(dx, dy) => write!(f, "rel {dx} {dy}"),
            Self::Click(button, count) => write!(f, "click {button:?} ×{count}"),
            Self::Press(button) => write!(f, "press {button:?}"),
            Self::Release(button) => write!(f, "release {button:?}"),
            Self::Wheel { dy, dx } => write!(f, "wheel dy={dy} dx={dx}"),
            Self::System(command) => write!(f, "system {command:?}"),
            Self::Sleep(duration) => write!(f, "sleep {} ms", duration.as_millis()),
            #[cfg(windows)]
            Self::Pull => write!(f, "pull"),
        }
    }
}

pub(crate) struct Options {
    pub screen: Option<i32>,
    pub hold: Duration,
}

/// Runs a parsed script on an established session.
pub(crate) struct Runner {
    input: RemoteInput,
    #[cfg(windows)]
    exchange: crate::features::clipboard::Clipboard,
    screen: i32,
    hold: Duration,
}

impl Runner {
    /// Wait for the input channel and the host's screens, enable control and
    /// check that the host granted it.
    pub(crate) async fn start(
        handle: crate::features::stream_control::StreamControlHandle,
        #[cfg(windows)] exchange: crate::features::clipboard::Clipboard,
        options: &Options,
    ) -> Result<Self> {
        let input = handle.mouse().clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut last = None;
        loop {
            let snapshot = handle.snapshot();
            if snapshot.hello_answered && !snapshot.screens.is_empty() {
                // Absolute mouse, host cursor left alone (§7).
                match handle.set_mouse_mode(MouseMode::Local) {
                    Ok(()) => break,
                    Err(error) => last = Some(error),
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(last.unwrap_or_else(|| anyhow!("被控端没有提供显示器列表")))
                    .context("键鼠控制未能开启");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let grants = loop {
            if let Some(grants) = handle.granted() {
                break grants;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "被控端没有报告本会话的能力"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        println!("granted: {}", grants.join(","));
        ensure!(
            grants.iter().any(|cap| cap == farwing_protocol::caps::CONTROL),
            "被控端没有授予键鼠控制（control）"
        );
        // One participant controls at a time: wait for the control token.
        loop {
            if handle.participants().holds_control() {
                break;
            }
            if input.mode() == MouseMode::View {
                bail!(
                    "被控端没有交出控制权：{}",
                    handle
                        .participants()
                        .controlled_by_other()
                        .map(|m| crate::features::stream_control::control_turn::held_by(&m.nickname))
                        .unwrap_or_else(|| "请求被拒绝".into())
                );
            }
            ensure!(tokio::time::Instant::now() < deadline, "等待控制权超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        println!("control token held");
        // Nothing is held on this side: the activation may send at once.
        input.confirm_neutral(input.activation_generation());
        ensure!(!input.waiting_for_neutral(), "键鼠控制未能就绪");
        let snapshot = handle.snapshot();
        let screen = match options.screen {
            Some(id) => snapshot
                .screens
                .iter()
                .find(|s| s.id == id)
                .with_context(|| format!("被控端没有屏幕 {id}"))?,
            None => snapshot
                .screens
                .iter()
                .find(|s| s.primary)
                .or(snapshot.screens.first())
                .context("被控端没有屏幕")?,
        };
        println!(
            "screen: {} ({}×{}{})",
            screen.id,
            screen.width,
            screen.height,
            if screen.primary { ", primary" } else { "" }
        );
        Ok(Self {
            screen: screen.id,
            input,
            #[cfg(windows)]
            exchange,
            hold: options.hold,
        })
    }

    async fn flush(&self) -> Result<()> {
        ensure!(
            self.input.flushed(Duration::from_secs(3)).await,
            "输入未能在 3 秒内送出"
        );
        if let Some(error) = self.input.error() {
            bail!(error);
        }
        Ok(())
    }

    async fn key(&self, usage: u32, down: bool) -> Result<()> {
        ensure!(
            self.input.key(OWNER, usage, down),
            "按键 {usage:#x} {} 未被接受",
            if down { "按下" } else { "抬起" }
        );
        self.flush().await
    }

    async fn tap(&self, shift: Option<u32>, usage: u32) -> Result<()> {
        if let Some(shift) = shift {
            self.key(shift, true).await?;
        }
        self.key(usage, true).await?;
        tokio::time::sleep(self.hold).await;
        self.key(usage, false).await?;
        if let Some(shift) = shift {
            self.key(shift, false).await?;
        }
        tokio::time::sleep(self.hold).await;
        Ok(())
    }

    async fn button(&self, button: Button, down: bool) -> Result<()> {
        self.input.button(OWNER, button.mask(), down);
        self.flush().await
    }

    pub(crate) async fn run(&self, step: &Step) -> Result<()> {
        match step {
            Step::Text(text) => {
                for c in text.chars() {
                    let (shift, usage) = char_keys(c)?;
                    self.tap(shift, usage).await?;
                }
            }
            Step::Chord(keys) => {
                for key in keys {
                    self.key(key.usage, true).await?;
                }
                tokio::time::sleep(self.hold).await;
                for key in keys.iter().rev() {
                    self.key(key.usage, false).await?;
                }
                tokio::time::sleep(self.hold).await;
            }
            Step::Down(key) => self.key(key.usage, true).await?,
            Step::Up(key) => self.key(key.usage, false).await?,
            Step::Repeat { key, count, interval } => {
                self.key(key.usage, true).await?;
                for _ in 0..*count {
                    tokio::time::sleep(*interval).await;
                    // Down while already down is sent as an auto-repeat.
                    self.key(key.usage, true).await?;
                }
                self.key(key.usage, false).await?;
            }
            Step::Lock(state) => {
                self.input.sync_locks(OWNER, *state);
                self.flush().await?;
            }
            Step::Move(x, y) => {
                self.input.set_relative_mode(false);
                // Absolute positions are 0 <= v < 1 of the screen.
                let clamp = |v: f64| v.min(1.0 - 1e-6);
                self.input.absolute(OWNER, self.screen, clamp(*x), clamp(*y));
                self.flush().await?;
            }
            Step::Rel(dx, dy) => {
                self.input.set_relative_mode(true);
                self.input.relative(OWNER, *dx, *dy);
                let flushed = self.flush().await;
                self.input.set_relative_mode(false);
                flushed?;
            }
            Step::Click(button, count) => {
                for _ in 0..*count {
                    self.button(*button, true).await?;
                    tokio::time::sleep(self.hold).await;
                    self.button(*button, false).await?;
                    tokio::time::sleep(self.hold).await;
                }
            }
            Step::Press(button) => self.button(*button, true).await?,
            Step::Release(button) => self.button(*button, false).await?,
            Step::Wheel { dy, dx } => {
                self.input.wheel(OWNER, *dx, *dy);
                self.flush().await?;
            }
            Step::System(command) => {
                self.input.system_command(OWNER, *command)?;
                self.flush().await?;
            }
            Step::Sleep(duration) => tokio::time::sleep(*duration).await,
            #[cfg(windows)]
            Step::Pull => {
                let exchange = self.exchange.clone();
                let text = tokio::task::spawn_blocking(move || pull_text(&exchange))
                    .await
                    .context("获取剪贴板的线程异常结束")??;
                println!("clipboard: {text:?}");
            }
        }
        Ok(())
    }

    /// Release everything still held on the host and stop input.
    pub(crate) async fn finish(&self) {
        self.input.close().await;
    }
}

#[cfg(windows)]
/// The host clipboard as text, through `Pull` (§8.2).
fn pull_text(exchange: &crate::features::clipboard::Clipboard) -> Result<String> {
    let (before, _) = exchange
        .memory_wait_offer(0, Duration::from_millis(1))
        .unwrap_or((0, Vec::new()));
    exchange.pull()?;
    let (offer, formats) = exchange.memory_wait_offer(before, Duration::from_secs(5))?;
    let format = formats
        .iter()
        .find(|format| format.id == 13)
        .cloned()
        .context("被控剪贴板没有文本")?;
    let bytes = exchange.memory_fetch(offer, &format)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_maps_to_us_positions() {
        assert_eq!(char_keys('a').unwrap(), (None, K | 0x04));
        assert_eq!(char_keys('A').unwrap(), (Some(K | 0xe1), K | 0x04));
        assert_eq!(char_keys('z').unwrap(), (None, K | 0x1d));
        assert_eq!(char_keys('1').unwrap(), (None, K | 0x1e));
        assert_eq!(char_keys('0').unwrap(), (None, K | 0x27));
        assert_eq!(char_keys('!').unwrap(), (Some(K | 0xe1), K | 0x1e));
        assert_eq!(char_keys(')').unwrap(), (Some(K | 0xe1), K | 0x27));
        assert_eq!(char_keys('\n').unwrap(), (None, K | 0x28));
        assert_eq!(char_keys('\t').unwrap(), (None, K | 0x2b));
        assert_eq!(char_keys(' ').unwrap(), (None, K | 0x2c));
        assert_eq!(char_keys(';').unwrap(), (None, K | 0x33));
        assert_eq!(char_keys(':').unwrap(), (Some(K | 0xe1), K | 0x33));
        assert_eq!(char_keys('?').unwrap(), (Some(K | 0xe1), K | 0x38));
        assert_eq!(char_keys('"').unwrap(), (Some(K | 0xe1), K | 0x34));
        assert_eq!(char_keys('~').unwrap(), (Some(K | 0xe1), K | 0x35));
        assert_eq!(char_keys('|').unwrap(), (Some(K | 0xe1), K | 0x31));
        assert!(char_keys('é').is_err());
        // Every printable ASCII character has a key.
        for c in (b' '..=b'~').map(char::from) {
            assert!(char_keys(c).is_ok(), "{c:?}");
        }
    }

    #[test]
    fn script_parses_keyboard_and_mouse_steps() {
        let script = "\
# Notepad check
text Hello, World! 123\\n
key ctrl+a
key ctrl+shift+esc
down shift
up shift
repeat a 5 30
lock caps=on num=off
move 0.5 0.25
rel -10 5
click left 2
click right
press middle
release middle
wheel -120
wheel 0 240
system taskmgr
key volumeup
key f12
key kp7
sleep 100
";
        let steps = parse_script(script, false).unwrap();
        assert_eq!(steps.len(), 20);
        assert_eq!(steps[0], Step::Text("Hello, World! 123\n".into()));
        let Step::Chord(keys) = &steps[2] else { panic!() };
        assert_eq!(
            keys.iter().map(|k| k.usage).collect::<Vec<_>>(),
            vec![K | 0xe0, K | 0xe1, K | 0x29]
        );
        assert!(matches!(&steps[5], Step::Repeat { key, count: 5, interval } if key.usage == K | 0x04 && *interval == Duration::from_millis(30)));
        assert_eq!(steps[6], Step::Lock([true, false, false]));
        assert_eq!(steps[7], Step::Move(0.5, 0.25));
        assert_eq!(steps[8], Step::Rel(-10, 5));
        assert_eq!(steps[9], Step::Click(Button::Left, 2));
        assert_eq!(steps[10], Step::Click(Button::Right, 1));
        assert_eq!(steps[13], Step::Wheel { dy: -120, dx: 0 });
        assert_eq!(steps[14], Step::Wheel { dy: 0, dx: 240 });
        assert_eq!(steps[15], Step::System(SystemCommand::TaskManager));
        let Step::Chord(keys) = &steps[16] else { panic!() };
        assert_eq!(keys[0].usage, C | 0xe9);
        let Step::Chord(keys) = &steps[17] else { panic!() };
        assert_eq!(keys[0].usage, K | 0x45);
        let Step::Chord(keys) = &steps[18] else { panic!() };
        assert_eq!(keys[0].usage, K | 0x5f);
        assert_eq!(steps[19], Step::Sleep(Duration::from_millis(100)));
    }

    #[test]
    fn clipboard_pull_is_windows_only() {
        #[cfg(windows)]
        assert_eq!(parse_script("pull", false).unwrap(), [Step::Pull]);
        #[cfg(not(windows))]
        assert!(parse_script("pull", false).is_err());
    }

    #[tokio::test]
    async fn script_emits_windows_input_frames() {
        use crate::features::remote_input::InputWork;
        use crate::protocol::input::{self as wire, InputKind};

        let input = RemoteInput::default();
        input.set_keyboard_platform(farwing_protocol::Platform::Windows);
        input.set_ready(true);
        input.enable(MouseMode::Local, false).unwrap();
        input.confirm_neutral(input.activation_generation());
        let runner = Runner {
            input: input.clone(),
            #[cfg(windows)]
            exchange: crate::features::clipboard::Clipboard::memory(
                crate::transport::transfer::kind::CLIPBOARD,
            ),
            screen: 1,
            hold: Duration::ZERO,
        };
        let steps = parse_script(
            "text A\nkey rctrl+volumeup\nclick x1\nrel -10 5\nwheel -120 240",
            false,
        )
        .unwrap();
        let key = |usage, down| {
            InputKind::Key(wire::Key {
                usage,
                down,
                repeat: false,
            })
        };
        let button = |down| InputKind::MouseButton(wire::MouseButton { button: 32, down });
        let expected = [
            key(K | 0xe1, true),
            key(K | 0x04, true),
            key(K | 0x04, false),
            key(K | 0xe1, false),
            key(K | 0xe4, true),
            key(C | 0xe9, true),
            key(C | 0xe9, false),
            key(K | 0xe4, false),
            button(true),
            button(false),
            InputKind::MouseRelative(wire::MouseRelative { dx: -10, dy: 5 }),
            InputKind::MouseWheel(wire::MouseWheel { dx: 240, dy: -120 }),
        ];
        let script = async {
            for step in &steps {
                runner.run(step).await.unwrap();
            }
        };
        let transport = async {
            let mut sent = Vec::new();
            for _ in &expected {
                let InputWork::Event(event) = input.next().await else {
                    panic!("unexpected recovery");
                };
                sent.push(
                    wire::decode(&event.event.encode())
                        .unwrap()
                        .unwrap()
                        .kind
                        .unwrap(),
                );
                input.complete(&event, Ok(()));
            }
            sent
        };
        let ((), sent) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(script, transport)
        })
        .await
        .unwrap();
        assert_eq!(sent, expected);
    }

    #[test]
    fn text_keeps_spaces_and_escapes() {
        let steps = parse_script("text   two  spaces\\tand\\\\slash", false).unwrap();
        assert_eq!(steps, vec![Step::Text("  two  spaces\tand\\slash".into())]);
        assert!(parse_script("text bad\\q", false).is_err());
    }

    #[test]
    fn errors_name_the_line_and_disruptive_commands_need_consent() {
        let error = parse_script("key ctrl+a\nfrobnicate\n", false).unwrap_err();
        assert!(format!("{error:#}").contains("第 2 行"));
        assert!(parse_script("system sas", false).is_err());
        assert!(parse_script("system lock", false).is_err());
        assert!(parse_script("system lock", true).is_ok());
        assert!(parse_script("key nosuchkey", false).is_err());
        assert!(parse_script("move 1.5 0", false).is_err());
        assert!(parse_script("lock caps=maybe", false).is_err());
        assert!(parse_script("click left 2 extra", false).is_err());
    }
}
