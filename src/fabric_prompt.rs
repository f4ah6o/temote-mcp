//! Explicit terminal selections. No text entry, input echo or input-value logs.
use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Toggle,
    Enter,
    Cancel,
    Untrusted,
}

struct Menu {
    roots: Vec<String>,
    selected: Vec<bool>,
    focus: usize,
    armed: bool,
}

impl Menu {
    fn new(roots: &[String]) -> Self {
        Self {
            roots: roots.to_vec(),
            selected: vec![false; roots.len()],
            focus: roots.len() + 1,
            armed: true,
        }
    }
    fn apply(&mut self, key: Key) -> Option<bool> {
        let choices = self.roots.len() + 2;
        match key {
            Key::Cancel => return Some(false),
            Key::Untrusted => {
                self.focus = choices - 1;
                self.armed = false;
            }
            Key::Up => {
                self.focus = (self.focus + choices - 1) % choices;
                self.armed = true;
            }
            Key::Down => {
                self.focus = (self.focus + 1) % choices;
                self.armed = true;
            }
            Key::Toggle if self.armed && self.focus < self.roots.len() => {
                self.selected[self.focus] = !self.selected[self.focus]
            }
            Key::Enter if self.armed && self.focus == choices - 1 => return Some(false),
            Key::Enter
                if self.armed
                    && self.focus == self.roots.len()
                    && (self.roots.is_empty()
                        || self.selected.iter().any(|selected| *selected)) =>
            {
                return Some(true);
            }
            _ => {}
        }
        None
    }
    fn approved_roots(&self) -> Vec<String> {
        self.roots
            .iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
            .map(|(root, _)| root.clone())
            .collect()
    }
    fn render(&self, out: &mut impl Write, previous_lines: usize) -> Result<usize> {
        let mut summary_lines = 0;
        if previous_lines > 0 {
            write!(out, "\x1b[{previous_lines}A\r\x1b[J")?;
        }
        writeln!(
            out,
            "Use arrow keys to select, Space to toggle roots, Enter to choose. Escape/Ctrl-C cancels."
        )?;
        writeln!(
            out,
            "Do not paste OAuth codes or type Host/root names here."
        )?;
        let start = self
            .focus
            .saturating_sub(3)
            .min(self.roots.len().saturating_sub(6));
        let end = (start + 6).min(self.roots.len());
        for index in start..end {
            writeln!(
                out,
                "{} [{}] {}",
                if self.focus == index { ">" } else { " " },
                if self.selected[index] { "x" } else { " " },
                self.roots[index]
            )?;
        }
        writeln!(
            out,
            "{} Approve{}",
            if self.focus == self.roots.len() {
                ">"
            } else {
                " "
            },
            if self.roots.is_empty() {
                ""
            } else {
                " selected root scope"
            }
        )?;
        writeln!(
            out,
            "{} Cancel",
            if self.focus == self.roots.len() + 1 {
                ">"
            } else {
                " "
            }
        )?;
        if !self.roots.is_empty() {
            writeln!(
                out,
                "Selected: {} (none are selected automatically)",
                self.selected.iter().filter(|selected| **selected).count()
            )?;
        }
        if !self.armed {
            writeln!(
                out,
                "Pasted/text input ignored. Use an arrow key to make an explicit selection."
            )?;
        }
        if self.focus == self.roots.len() && !self.roots.is_empty() {
            let approved = self.approved_roots();
            writeln!(out, "Selected scope to approve:")?;
            summary_lines += 1;
            for root in approved.iter().take(6) {
                writeln!(out, "  {root}")?;
                summary_lines += 1;
            }
            if approved.len() > 6 {
                writeln!(
                    out,
                    "  {} more selected roots; use arrows to review their checkboxes.",
                    approved.len() - 6
                )?;
                summary_lines += 1;
            }
        }
        out.flush()?;
        Ok(2 + end - start
            + 2
            + usize::from(!self.roots.is_empty())
            + usize::from(!self.armed)
            + summary_lines)
    }
}

#[cfg(unix)]
struct TerminalMode(libc::termios);

#[cfg(unix)]
impl TerminalMode {
    fn enter() -> Result<Self> {
        anyhow::ensure!(
            std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
                && std::io::stderr().is_terminal(),
            "Fabric approval requires an interactive terminal"
        );
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        anyhow::ensure!(
            unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } == 0,
            "terminal mode is unavailable"
        );
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        anyhow::ensure!(
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } == 0,
            "terminal selection mode is unavailable"
        );
        let mode = Self(original);
        std::io::stdout().write_all(b"\x1b[?2004h\x1b[?25l")?;
        std::io::stdout().flush()?;
        Ok(mode)
    }
}

#[cfg(unix)]
impl Drop for TerminalMode {
    fn drop(&mut self) {
        let cleanup = b"\x1b[?2004l\x1b[?25h";
        unsafe {
            libc::write(libc::STDOUT_FILENO, cleanup.as_ptr().cast(), cleanup.len());
            // Discard queued bytes between approval stages and restore on every
            // cancellation, error and normal return. Never echo queued secrets.
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0);
        }
    }
}

#[cfg(unix)]
fn byte(wait_ms: Option<i32>) -> Result<Option<u8>> {
    if let Some(wait_ms) = wait_ms {
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
        if ready == 0 {
            return Ok(None);
        }
        anyhow::ensure!(ready > 0, "terminal selection interrupted");
    }
    let mut value = 0u8;
    let count = unsafe { libc::read(libc::STDIN_FILENO, (&mut value as *mut u8).cast(), 1) };
    anyhow::ensure!(count >= 0, "terminal selection interrupted");
    Ok((count == 1).then_some(value))
}

fn decode_key(first: Option<u8>, mut following: impl FnMut() -> Result<Option<u8>>) -> Result<Key> {
    Ok(match first {
        None | Some(3 | 4) => Key::Cancel,
        Some(b'\r' | b'\n') => Key::Enter,
        Some(b' ') => Key::Toggle,
        Some(27) => {
            if following()? != Some(b'[') {
                return Ok(Key::Cancel);
            }
            match following()? {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                Some(b'2')
                    if following()? == Some(b'0')
                        && following()? == Some(b'0')
                        && following()? == Some(b'~') =>
                {
                    // Bracketed paste is data, including embedded Enter/Space
                    // and arrow sequences. It can never authorize a selection.
                    let mut ending = Vec::new();
                    for _ in 0..65536 {
                        let Some(value) = following()? else {
                            return Ok(Key::Cancel);
                        };
                        ending.push(value);
                        if ending.len() > 6 {
                            ending.remove(0);
                        }
                        if ending == b"\x1b[201~" {
                            return Ok(Key::Untrusted);
                        }
                    }
                    Key::Cancel
                }
                _ => Key::Untrusted,
            }
        }
        _ => Key::Untrusted,
    })
}

fn run_menu(roots: &[String]) -> Result<Vec<String>> {
    #[cfg(unix)]
    {
        let _mode = TerminalMode::enter()?;
        let mut menu = Menu::new(roots);
        let mut lines = 0;
        loop {
            lines = menu.render(&mut std::io::stdout().lock(), lines)?;
            let key = decode_key(byte(None)?, || byte(Some(100)))?;
            match menu.apply(key) {
                Some(true) => return Ok(menu.approved_roots()),
                Some(false) => {
                    anyhow::bail!("Fabric approval cancelled; no new Host grant was approved")
                }
                None => {}
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = roots;
        anyhow::bail!("Fabric terminal selection requires macOS or Linux/WSL")
    }
}

pub fn confirm_host() -> Result<()> {
    run_menu(&[]).map(|_| ())
}

pub fn select_roots(available: &[String]) -> Result<Vec<String>> {
    anyhow::ensure!(
        !available.is_empty() && available.len() <= 32,
        "configured root selection is unavailable"
    );
    for root in available {
        crate::named_roots::validate_root_name(root)
            .context("configured root display is invalid")?;
    }
    run_menu(available)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_requires_navigation_and_default_enter_cancels() {
        assert_eq!(Menu::new(&[]).apply(Key::Enter), Some(false));
        let mut menu = Menu::new(&[]);
        assert_eq!(menu.apply(Key::Up), None);
        assert_eq!(menu.apply(Key::Enter), Some(true));
    }
    #[test]
    fn pasted_text_disarms_approval_until_new_navigation() {
        let mut menu = Menu::new(&[]);
        menu.apply(Key::Up);
        menu.apply(Key::Untrusted);
        assert_eq!(menu.apply(Key::Enter), None);
        assert_eq!(menu.apply(Key::Toggle), None);
        menu.apply(Key::Up);
        assert_eq!(menu.apply(Key::Enter), Some(true));
        assert_eq!(Menu::new(&[]).apply(Key::Cancel), Some(false));
    }
    #[test]
    fn roots_require_explicit_toggle_and_scope_approval() {
        let mut menu = Menu::new(&["src".into(), "work".into()]);
        menu.apply(Key::Up);
        assert_eq!(menu.apply(Key::Enter), None);
        menu.apply(Key::Up);
        menu.apply(Key::Toggle);
        menu.apply(Key::Down);
        assert_eq!(menu.apply(Key::Enter), Some(true));
        assert_eq!(menu.approved_roots(), vec!["work"]);
    }
    #[test]
    fn bracketed_paste_discards_secrets_and_control_sequences() {
        let bytes = b"[200~SECRET_CODE\n \x1b[A\r\x1b[201~";
        let mut iter = bytes.iter().copied();
        assert!(decode_key(Some(27), || Ok(iter.next())).unwrap() == Key::Untrusted);
        assert!(iter.next().is_none());
        let mut out = Vec::new();
        let mut menu = Menu::new(&[]);
        menu.apply(Key::Untrusted);
        menu.render(&mut out, 0).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("SECRET_CODE"));
    }
    #[test]
    fn eof_and_incomplete_paste_cancel() {
        assert!(decode_key(None, || Ok(None)).unwrap() == Key::Cancel);
        let mut bytes = b"[200~INCOMPLETE".iter().copied();
        assert!(decode_key(Some(27), || Ok(bytes.next())).unwrap() == Key::Cancel);
    }

    // This ignored fixture runs only in the private PTY spawned by the test
    // below. It has no OAuth, credential store or Supervisor dependency.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn private_pty_selection_fixture() {
        if std::env::var_os("TEMOTE_TEST_SELECTION_PTY").is_none() {
            return;
        }
        let mut before = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(0, before.as_mut_ptr()) }, 0);
        let before = unsafe { before.assume_init() };
        assert!(confirm_host().is_err());
        let mut after = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(0, after.as_mut_ptr()) }, 0);
        let after = unsafe { after.assume_init() };
        assert_eq!(before.c_lflag, after.c_lflag);
        assert_eq!(before.c_iflag, after.c_iflag);
        assert_eq!(before.c_cc, after.c_cc);
        println!("PRIVATE_PTY_MODE_RESTORED");
    }

    #[cfg(unix)]
    #[test]
    fn private_pty_discards_paste_and_restores_terminal_on_cancel() {
        use std::fs::File;
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::process::{Command, Stdio};
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fabric_prompt::tests::private_pty_selection_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("TEMOTE_TEST_SELECTION_PTY", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        let mut output = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut read_until = |needle: &str, master: &mut File| {
            while !String::from_utf8_lossy(&output).contains(needle) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "private PTY fixture timed out"
                );
                let mut descriptor = libc::pollfd {
                    fd: master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut descriptor, 1, 100) } <= 0 {
                    continue;
                }
                let mut bytes = [0; 1024];
                let count = master.read(&mut bytes).unwrap_or(0);
                assert!(count > 0, "private PTY fixture closed unexpectedly");
                output.extend_from_slice(&bytes[..count]);
                assert!(output.len() < 65536);
            }
        };
        read_until("Cancel", &mut master);
        master
            .write_all(b"\x1b[200~PRIVATE_SECRET_CODE\n \x1b[A\r\x1b[201~")
            .unwrap();
        read_until("Pasted/text input ignored", &mut master);
        master.write_all(b"\x1b").unwrap();
        read_until("PRIVATE_PTY_MODE_RESTORED", &mut master);
        assert!(child.wait().unwrap().success());
        assert!(!String::from_utf8_lossy(&output).contains("PRIVATE_SECRET_CODE"));
    }
}
