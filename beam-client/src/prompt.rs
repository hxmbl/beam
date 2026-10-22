//! Approval prompt on the client's controlling terminal.
//!
//! The prompt is answered on the client's own TTY, never through the SSH
//! session, so a remote host cannot approve its own request.
//!
//! When `beam-client ssh` runs ssh as its child, ssh is also reading the same
//! terminal. To make sure the answer reaches us (and not the remote shell),
//! the ssh child is paused with SIGSTOP for the length of the prompt and
//! resumed with SIGCONT afterwards. Pending typeahead is discarded before the
//! prompt is shown so keys typed for the remote side can never approve a
//! request by accident.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::time::Duration;

/// Outcome of asking the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Approved,
    Denied,
    TimedOut,
    /// No usable terminal (not attached, or not in the foreground).
    NoTty(String),
}

impl Decision {
    pub fn approved(&self) -> bool {
        matches!(self, Decision::Approved)
    }

    /// Reason sent to the server when the request is not approved.
    pub fn reason(&self) -> Option<String> {
        match self {
            Decision::Approved => None,
            Decision::Denied => Some("denied by user on the client".to_string()),
            Decision::TimedOut => Some("approval prompt timed out on the client".to_string()),
            Decision::NoTty(why) => Some(format!("no terminal for approval on the client ({why})")),
        }
    }
}

/// Settings shared by every prompt a listener shows.
#[derive(Debug, Clone)]
pub struct PromptConfig {
    /// Terminal to prompt on (default `/dev/tty`, override with `BEAM_PROMPT_TTY`).
    pub tty: PathBuf,
    /// How long to wait for an answer before denying (`BEAM_PROMPT_TIMEOUT`, seconds).
    pub timeout: Duration,
    /// PID of the ssh child to pause while prompting (0 = none).
    pub ssh_pid: Arc<AtomicI32>,
    /// True while a prompt has the ssh child stopped, so the wrapper's wait
    /// loop does not mistake our SIGSTOP for the user suspending ssh.
    pub prompting: Arc<AtomicBool>,
}

impl Default for PromptConfig {
    fn default() -> Self {
        let tty = std::env::var_os("BEAM_PROMPT_TTY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/dev/tty"));
        let timeout = std::env::var("BEAM_PROMPT_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30));
        Self {
            tty,
            timeout,
            ssh_pid: Arc::new(AtomicI32::new(0)),
            prompting: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Only one prompt at a time, even with several remote requests in flight.
static PROMPT_LOCK: Mutex<()> = Mutex::new(());

/// Parse an answer line. Only an explicit yes approves.
pub fn parse_answer(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Show `text` on the terminal and wait for y/N. Blocking: call it from
/// `spawn_blocking`.
pub fn ask(config: &PromptConfig, text: &str) -> Decision {
    let _guard = PROMPT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    imp::ask(config, text)
}

#[cfg(unix)]
mod imp {
    use super::{Decision, PromptConfig, parse_answer};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    /// Restores the terminal and resumes ssh, whatever happens.
    struct Restore {
        fd: libc::c_int,
        saved: Option<libc::termios>,
        ssh_pid: i32,
        config: PromptConfig,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                if let Some(t) = self.saved.as_ref() {
                    libc::tcsetattr(self.fd, libc::TCSANOW, t);
                }
                if self.ssh_pid > 0 {
                    libc::kill(self.ssh_pid, libc::SIGCONT);
                }
                libc::close(self.fd);
            }
            self.config.prompting.store(false, Ordering::SeqCst);
        }
    }

    fn write_all(fd: libc::c_int, s: &str) {
        let mut buf = s.as_bytes();
        while !buf.is_empty() {
            let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
            if n <= 0 {
                return;
            }
            buf = &buf[n as usize..];
        }
    }

    pub fn ask(config: &PromptConfig, text: &str) -> Decision {
        let path = match CString::new(config.tty.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => return Decision::NoTty("bad tty path".into()),
        };
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if fd < 0 {
            return Decision::NoTty(format!(
                "cannot open {}: {}",
                config.tty.display(),
                std::io::Error::last_os_error()
            ));
        }

        let is_tty = unsafe { libc::isatty(fd) } == 1;
        if is_tty {
            // Reading the terminal from a background process group would stop
            // us with SIGTTIN, so refuse instead of hanging.
            let fg = unsafe { libc::tcgetpgrp(fd) };
            let ours = unsafe { libc::getpgrp() };
            if fg >= 0 && fg != ours {
                unsafe { libc::close(fd) };
                return Decision::NoTty("beam-client is not in the terminal's foreground".into());
            }
        }

        let ssh_pid = config.ssh_pid.load(Ordering::SeqCst);
        config.prompting.store(true, Ordering::SeqCst);
        if ssh_pid > 0 {
            unsafe { libc::kill(ssh_pid, libc::SIGSTOP) };
        }
        let mut restore = Restore {
            fd,
            saved: None,
            ssh_pid,
            config: config.clone(),
        };

        if is_tty {
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut t) == 0 {
                    restore.saved = Some(t);
                    // Line mode with echo; ssh may have left the terminal raw.
                    // ISIG off so Ctrl-C answers "no" instead of killing us.
                    t.c_lflag |= libc::ICANON | libc::ECHO | libc::ECHOE;
                    t.c_lflag &= !libc::ISIG;
                    t.c_iflag |= libc::ICRNL;
                    t.c_oflag |= libc::OPOST | libc::ONLCR;
                    libc::tcsetattr(fd, libc::TCSANOW, &t);
                }
                // Drop typeahead meant for the remote side.
                libc::tcflush(fd, libc::TCIFLUSH);
            }
        }

        // On a real terminal the prompt goes to the terminal. Anything else
        // (a FIFO in tests) is input only, so show the prompt on stderr.
        let out_fd = if is_tty { fd } else { libc::STDERR_FILENO };
        write_all(out_fd, text);

        let deadline = Instant::now() + config.timeout;
        let mut line = Vec::new();
        let decision = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Decision::TimedOut;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break Decision::NoTty("poll failed".into());
            }
            if r == 0 {
                break Decision::TimedOut;
            }
            let mut buf = [0u8; 256];
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break Decision::Denied;
            }
            line.extend_from_slice(&buf[..n as usize]);
            if let Some(pos) = line.iter().position(|&b| b == b'\n' || b == b'\r') {
                let answer = String::from_utf8_lossy(&line[..pos]).to_string();
                break if parse_answer(&answer) {
                    Decision::Approved
                } else {
                    Decision::Denied
                };
            }
        };

        let note = match &decision {
            Decision::Approved => "beam: approved\n",
            Decision::Denied => "beam: denied\n",
            Decision::TimedOut => "\nbeam: no answer, denied\n",
            Decision::NoTty(_) => "\nbeam: denied\n",
        };
        write_all(out_fd, note);
        drop(restore);
        decision
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Decision, PromptConfig};
    pub fn ask(_config: &PromptConfig, _text: &str) -> Decision {
        Decision::NoTty("approval prompt is only implemented on Unix".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_yes_approves() {
        assert!(parse_answer("y"));
        assert!(parse_answer("Y\n"));
        assert!(parse_answer(" yes "));
        assert!(!parse_answer(""));
        assert!(!parse_answer("n"));
        assert!(!parse_answer("yep"));
        assert!(!parse_answer("\u{3}"));
    }

    #[test]
    fn missing_tty_denies() {
        let cfg = PromptConfig {
            tty: PathBuf::from("/nonexistent/beam-tty"),
            timeout: Duration::from_millis(50),
            ..PromptConfig::default()
        };
        assert!(matches!(ask(&cfg, "?"), Decision::NoTty(_)));
    }

    #[cfg(unix)]
    #[test]
    fn non_tty_file_reads_answer_and_times_out() {
        use std::io::Write;
        // A FIFO stands in for the terminal: answer "y".
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("answer");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let cfg = PromptConfig {
            tty: fifo.clone(),
            timeout: Duration::from_secs(5),
            ..PromptConfig::default()
        };
        let writer = std::thread::spawn({
            let fifo = fifo.clone();
            move || {
                let mut f = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                f.write_all(b"y\n").unwrap();
                // keep it open briefly so the reader sees data, not EOF
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        assert_eq!(ask(&cfg, ""), Decision::Approved);
        writer.join().unwrap();

        // Nobody answers: the prompt times out and denies.
        let cfg = PromptConfig {
            timeout: Duration::from_millis(300),
            ..cfg
        };
        let holder = std::thread::spawn({
            let fifo = fifo.clone();
            move || {
                let _f = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
                std::thread::sleep(Duration::from_millis(800));
            }
        });
        assert_eq!(ask(&cfg, ""), Decision::TimedOut);
        holder.join().unwrap();
    }
}
