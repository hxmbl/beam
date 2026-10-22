//! Full end-to-end test against a real sshd on localhost.
//!
//! Runs only with `BEAM_E2E_SSHD=1` (needs `sshd` and `ssh-keygen`; set
//! `BEAM_E2E_SSHD_BIN` if sshd is not at /usr/sbin/sshd). It works as an
//! unprivileged user: sshd runs on a free port with its own host key and
//! authorized_keys in a temp dir, and logs in as the current user.
//!
//! Each case runs the client side in a pseudo-terminal, exactly like a user:
//! the shell loads `beam-client shell-init`, `ssh` goes through the wrapper,
//! the remote command loads `beam-server-handler shell-init` and runs a
//! command that only exists on the client. The test answers the approval
//! prompt by typing into the terminal, then checks output and exit code.
//!
//! Client and server are the same machine here, so "missing on the server"
//! means: only on the client's PATH (a temp dir), not on the server's.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROMPT: &str = "allow? [y/N]";

fn enabled() -> bool {
    std::env::var("BEAM_E2E_SSHD").as_deref() == Ok("1")
}

/// Directory holding the beam binaries, built on demand.
fn bin_dir() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "-q",
            "-p",
            "beam-client",
            "-p",
            "beam-server-handler",
        ])
        .status()
        .expect("run cargo build");
    assert!(status.success(), "building beam binaries failed");
    // target/<profile>/deps/sshd_e2e-<hash> -> target/<profile>
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    assert!(
        dir.join("beam-client").exists(),
        "no beam-client in {dir:?}"
    );
    assert!(dir.join("beam-server-handler").exists());
    dir
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

struct Sshd {
    dir: tempfile::TempDir,
    port: u16,
    child: Child,
    bins: PathBuf,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Sshd {
    fn start(bins: PathBuf) -> Sshd {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for key in ["host_key", "user_key"] {
            let st = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(d.join(key))
                .status()
                .expect("ssh-keygen");
            assert!(st.success());
        }
        std::fs::copy(d.join("user_key.pub"), d.join("authorized_keys")).unwrap();

        let port = free_port();
        let config = format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {d}/host_key\n\
             PidFile {d}/sshd.pid\n\
             AuthorizedKeysFile {d}/authorized_keys\n\
             StrictModes no\n\
             UsePAM no\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\n\
             AcceptEnv BEAM_SESSION_ID BEAM_LISTENER_PORT\n\
             AllowTcpForwarding remote\n",
            d = d.display()
        );
        std::fs::write(d.join("sshd_config"), config).unwrap();

        // Server-side shell setup: beam-server-handler on PATH and the hook
        // loaded. The client-only bin dir is deliberately not on this PATH.
        for shell in ["bash", "zsh"] {
            let rc = format!(
                "export PATH={bins}:/usr/local/bin:/usr/bin:/bin\n\
                 eval \"$(beam-server-handler shell-init --shell {shell})\"\n",
                bins = bins.display()
            );
            std::fs::write(d.join(format!("server_rc.{shell}")), rc).unwrap();
        }

        let sshd_bin =
            std::env::var("BEAM_E2E_SSHD_BIN").unwrap_or_else(|_| "/usr/sbin/sshd".into());
        let log = std::fs::File::create(d.join("sshd.log")).unwrap();
        let child = Command::new(sshd_bin)
            .args(["-D", "-e", "-f"])
            .arg(d.join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start sshd");

        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "sshd did not start: {}", {
                std::fs::read_to_string(d.join("sshd.log")).unwrap_or_default()
            });
            std::thread::sleep(Duration::from_millis(50));
        }
        Sshd {
            dir,
            port,
            child,
            bins,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn ssh_opts(&self) -> String {
        format!(
            "-F /dev/null -p {} -i {}/user_key -o StrictHostKeyChecking=no \
             -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o LogLevel=ERROR",
            self.port,
            self.path().display()
        )
    }
}

/// A client-only directory with a `beam-hello` script in it.
fn client_bin(sshd: &Sshd, name: &str) -> PathBuf {
    let dir = sshd.path().join(format!("client-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("beam-hello");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         echo \"hello from client: $*\"\n\
         read line && echo \"stdin: $line\"\n\
         exit 3\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

struct Case<'a> {
    /// Local shell that loads the client wrapper (bash or zsh).
    shell: &'a str,
    /// Remote shell whose hook handles the missing command.
    remote_shell: &'a str,
    /// Command to run remotely, after the hook is loaded.
    remote_cmd: &'a str,
    /// What to type at the prompt (None: never answer).
    answer: Option<&'a str>,
    prompt_timeout: u64,
    client_dir: PathBuf,
    /// Runs when the prompt appears, before answering.
    on_prompt: Option<Box<dyn FnOnce() + 'a>>,
}

struct Outcome {
    code: i32,
    output: String,
    prompted: bool,
}

fn run(sshd: &Sshd, case: Case) -> Outcome {
    let rc = sshd.path().join(format!("server_rc.{}", case.remote_shell));
    let remote = format!(
        "{} -c {}",
        case.remote_shell,
        sh_quote(&format!("source {}; {}", rc.display(), case.remote_cmd))
    );
    let script = format!(
        "eval \"$(beam-client shell-init --shell {sh})\"; ssh {opts} 127.0.0.1 {remote}",
        sh = case.shell,
        opts = sshd.ssh_opts(),
        remote = sh_quote(&remote),
    );

    // Pseudo-terminal for the client side.
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(r, 0, "openpty failed");
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };

    let path = format!(
        "{}:{}:/usr/local/bin:/usr/bin:/bin",
        case.client_dir.display(),
        sshd.bins.display()
    );
    let mut cmd = Command::new(case.shell);
    cmd.arg("-c")
        .arg(&script)
        .env("PATH", path)
        .env("BEAM_PROMPT_TIMEOUT", case.prompt_timeout.to_string())
        .env("TERM", "dumb")
        .env_remove("BEAM_DISABLE")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            // New session with the pty as controlling terminal, like a login.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn client shell");
    drop(cmd);

    let mut reader = std::fs::File::from(master.try_clone().unwrap());
    let mut writer = std::fs::File::from(master);
    let output = Arc::new(Mutex::new(Vec::new()));
    let out2 = output.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out2.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });

    let text = || String::from_utf8_lossy(&output.lock().unwrap()).to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut prompted = false;
    let mut on_prompt = case.on_prompt;
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code().unwrap_or(-1);
        }
        if !prompted && text().contains(PROMPT) {
            prompted = true;
            if let Some(f) = on_prompt.take() {
                f();
            }
            if let Some(answer) = case.answer {
                std::thread::sleep(Duration::from_millis(200));
                writer.write_all(answer.as_bytes()).unwrap();
                writer.write_all(b"\n").unwrap();
            }
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("case timed out; output so far:\n{}", text());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(writer);
    // The reader ends once every slave fd is closed (the child has exited).
    let _ = reader_thread.join();
    Outcome {
        code,
        output: text().replace('\r', ""),
        prompted,
    }
}

fn case<'a>(client_dir: PathBuf, remote_cmd: &'a str, answer: Option<&'a str>) -> Case<'a> {
    Case {
        shell: "bash",
        remote_shell: "bash",
        remote_cmd,
        answer,
        prompt_timeout: 20,
        client_dir,
        on_prompt: None,
    }
}

#[test]
fn ssh_end_to_end_with_real_sshd() {
    if !enabled() {
        eprintln!("skipping: set BEAM_E2E_SSHD=1 to run the real-sshd end-to-end test");
        return;
    }
    let sshd = Sshd::start(bin_dir());
    let piped = "echo piped-line | beam-hello -x \"a b\"";

    // 1. Approve: the client's binary runs on the server with its args, stdin
    //    and exit code intact.
    let out = run(&sshd, case(client_bin(&sshd, "approve"), piped, Some("y")));
    println!("--- approve ---\n{}", out.output);
    assert!(out.prompted, "no prompt:\n{}", out.output);
    assert!(out.output.contains("hello from client: -x a b"));
    assert!(out.output.contains("stdin: piped-line"));
    assert_eq!(out.code, 3, "exit code not preserved:\n{}", out.output);

    // 2. Deny: clear message, exit 127, nothing runs.
    let out = run(&sshd, case(client_bin(&sshd, "deny"), piped, Some("n")));
    println!("--- deny ---\n{}", out.output);
    assert!(out.prompted);
    assert!(
        out.output
            .contains("beam-hello: not run (denied by user on the client)")
    );
    assert!(!out.output.contains("hello from client"));
    assert_eq!(out.code, 127);

    // 3. No answer: the prompt times out and denies.
    let mut c = case(client_bin(&sshd, "timeout"), piped, None);
    c.prompt_timeout = 2;
    let out = run(&sshd, c);
    println!("--- timeout ---\n{}", out.output);
    assert!(out.prompted);
    assert!(
        out.output
            .contains("approval prompt timed out on the client")
    );
    assert!(!out.output.contains("hello from client"));
    assert_eq!(out.code, 127);

    // 4. Hash mismatch: the binary changes on the client after its hash was
    //    shown and before it is streamed. The server must refuse to run it.
    let dir = client_bin(&sshd, "mismatch");
    let target = dir.join("beam-hello");
    let mut c = case(dir.clone(), piped, Some("y"));
    c.on_prompt = Some(Box::new(move || {
        std::fs::write(&target, "#!/bin/sh\necho TAMPERED\nexit 0\n").unwrap();
    }));
    let out = run(&sshd, c);
    println!("--- mismatch ---\n{}", out.output);
    assert!(out.prompted);
    assert!(out.output.contains("integrity check failed"));
    assert!(!out.output.contains("TAMPERED"));
    assert!(!out.output.contains("hello from client"));
    assert_eq!(out.code, 126);

    // 5. Missing on the client too: plain command-not-found, no prompt.
    let out = run(
        &sshd,
        case(client_bin(&sshd, "missing"), "beam-nope-xyz", Some("y")),
    );
    println!("--- missing ---\n{}", out.output);
    assert!(!out.prompted);
    assert!(out.output.contains("not found here or on the client"));
    assert_eq!(out.code, 127);

    // 6. zsh on both sides.
    if Command::new("zsh").arg("-c").arg("true").status().is_ok() {
        let mut c = case(client_bin(&sshd, "zsh"), piped, Some("y"));
        c.shell = "zsh";
        c.remote_shell = "zsh";
        let out = run(&sshd, c);
        println!("--- zsh ---\n{}", out.output);
        assert!(out.prompted);
        assert!(out.output.contains("hello from client: -x a b"));
        assert_eq!(out.code, 3);
    } else {
        eprintln!("zsh not installed: skipping the zsh case");
    }
}
