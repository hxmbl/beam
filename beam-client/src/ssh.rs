//! `beam-client ssh -- <ssh args>`: what the shell wrapper runs instead of ssh.
//!
//! It starts the local listener, then runs the real ssh as a child with a
//! reverse forward to that listener (`-R port:127.0.0.1:port`) and with
//! `BEAM_SESSION_ID` / `BEAM_LISTENER_PORT` exported and sent via `SendEnv`.
//! Staying the parent of ssh is what lets the approval prompt pause ssh and
//! read the answer from the terminal itself.

use std::process::Command;
use std::sync::atomic::Ordering;

use crate::prompt::PromptConfig;

/// Options that make ssh do something other than open a session. For these
/// the wrapper gets out of the way and runs ssh unchanged.
pub fn is_passthrough(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "-V" || a == "-G" || a.starts_with("-O") || a.starts_with("-Q"))
}

/// The ssh program to run (`BEAM_SSH` overrides it, mainly for tests).
pub fn ssh_program() -> String {
    std::env::var("BEAM_SSH").unwrap_or_else(|_| "ssh".to_string())
}

/// Pick the port sshd should listen on for the reverse forward. It is chosen
/// independently of the local listener's port: on the server the local port
/// number may be taken (and is, when client and server are the same host).
/// If the server cannot bind it, ssh warns and Beam is unavailable for this
/// session; the handler then reports that it cannot reach the client.
pub fn pick_remote_port(local_port: u16) -> u16 {
    const LOW: u16 = 20000;
    const SPAN: u16 = 40000;
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut port = LOW + u16::from_le_bytes([bytes[0], bytes[1]]) % SPAN;
    if port == local_port {
        port = LOW + (port - LOW + 1) % SPAN;
    }
    port
}

/// Arguments for the real ssh: Beam's forward and env options, then the user's.
pub fn ssh_args(remote_port: u16, local_port: u16, user_args: &[String]) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "SendEnv=BEAM_SESSION_ID".to_string(),
        "-o".to_string(),
        "SendEnv=BEAM_LISTENER_PORT".to_string(),
        "-R".to_string(),
        format!("{remote_port}:127.0.0.1:{local_port}"),
    ];
    args.extend(user_args.iter().cloned());
    args
}

/// Run ssh unchanged and return its exit code.
pub fn exec_plain(user_args: &[String]) -> i32 {
    match Command::new(ssh_program()).args(user_args).status() {
        Ok(status) => exit_code(status),
        Err(e) => {
            eprintln!("beam: failed to run {}: {e}", ssh_program());
            255
        }
    }
}

/// Spawn ssh for a Beam session and wait for it. The listener must already be
/// accepting on `local_port` in this process. `BEAM_LISTENER_PORT` is the
/// port the handler connects to on the server, i.e. the remote end.
pub fn run_session(
    session_id: &str,
    local_port: u16,
    user_args: &[String],
    prompt: &PromptConfig,
) -> i32 {
    let remote_port = pick_remote_port(local_port);
    let mut cmd = Command::new(ssh_program());
    cmd.args(ssh_args(remote_port, local_port, user_args))
        .env("BEAM_SESSION_ID", session_id)
        .env("BEAM_LISTENER_PORT", remote_port.to_string());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("beam: failed to run {}: {e}", ssh_program());
            return 255;
        }
    };
    prompt.ssh_pid.store(child.id() as i32, Ordering::SeqCst);
    imp::wait(child, prompt)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    255
}

#[cfg(unix)]
mod imp {
    use super::PromptConfig;
    use std::sync::atomic::Ordering;

    /// Wait for ssh like a shell would: Ctrl-C and Ctrl-\ belong to ssh, and
    /// when the user suspends ssh (`~^Z`) we suspend too so the shell's job
    /// control sees it. Stops caused by our own approval prompt are ignored.
    pub fn wait(child: std::process::Child, prompt: &PromptConfig) -> i32 {
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        }
        loop {
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
            if r < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return 255;
            }
            if libc::WIFEXITED(status) {
                return libc::WEXITSTATUS(status);
            }
            if libc::WIFSIGNALED(status) {
                return 128 + libc::WTERMSIG(status);
            }
            if libc::WIFSTOPPED(status) && !prompt.prompting.load(Ordering::SeqCst) {
                unsafe {
                    libc::raise(libc::SIGTSTP);
                    // Resumed by the shell (`fg`): make sure ssh runs again.
                    libc::kill(pid, libc::SIGCONT);
                }
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::PromptConfig;
    pub fn wait(mut child: std::process::Child, _prompt: &PromptConfig) -> i32 {
        match child.wait() {
            Ok(s) => super::exit_code(s),
            Err(_) => 255,
        }
    }
}

pub fn shell_init(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" | "zsh" => Some(
            r#"# Beam client integration (bash/zsh).
# Load it with:  eval "$(beam-client shell-init --shell bash)"   (or --shell zsh)
# `ssh` then runs through beam-client, which adds the reverse forward and
# SendEnv options itself. Set BEAM_DISABLE=1 to bypass it for a command.
function beam_ssh {
    command beam-client ssh -- "$@"
}
function ssh {
    if [ -n "${BEAM_DISABLE:-}" ] || ! command -v beam-client >/dev/null 2>&1; then
        command ssh "$@"
    else
        beam_ssh "$@"
    fi
}
"#,
        ),
        "fish" => Some(
            r#"# Beam client integration (fish).
# Load it with:  beam-client shell-init --shell fish | source
# Set BEAM_DISABLE=1 to bypass it for a command.
function beam_ssh --wraps ssh --description 'ssh with Beam'
    command beam-client ssh -- $argv
end
function ssh --description 'ssh with Beam'
    if set -q BEAM_DISABLE; or not command -q beam-client
        command ssh $argv
    else
        beam_ssh $argv
    end
end
"#,
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn adds_forward_and_sendenv_before_user_args() {
        let args = ssh_args(40001, 4242, &v(&["-p", "2222", "host", "ls"]));
        assert_eq!(
            args,
            v(&[
                "-o",
                "SendEnv=BEAM_SESSION_ID",
                "-o",
                "SendEnv=BEAM_LISTENER_PORT",
                "-R",
                "40001:127.0.0.1:4242",
                "-p",
                "2222",
                "host",
                "ls"
            ])
        );
    }

    #[test]
    fn remote_port_is_in_range_and_differs_from_local() {
        for _ in 0..1000 {
            let p = pick_remote_port(30000);
            assert!((20000..60000).contains(&p));
            assert_ne!(p, 30000);
        }
    }

    #[test]
    fn control_and_query_commands_pass_through() {
        assert!(is_passthrough(&v(&["-O", "exit", "host"])));
        assert!(is_passthrough(&v(&["-Ocheck", "host"])));
        assert!(is_passthrough(&v(&["-V"])));
        assert!(is_passthrough(&v(&["-G", "host"])));
        assert!(!is_passthrough(&v(&["-p", "22", "host"])));
    }

    #[test]
    fn shell_scripts_exist_for_supported_shells() {
        for sh in ["bash", "zsh", "fish"] {
            let s = shell_init(sh).unwrap();
            assert!(s.contains("beam-client ssh --"));
            assert!(s.contains("BEAM_DISABLE"));
        }
        assert!(shell_init("tcsh").is_none());
    }
}
