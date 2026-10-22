//! The real `beam-client` and `beam-server-handler` talking over plain TCP on
//! localhost, without ssh. Runs in every `cargo test`.
//!
//! A small proxy sits between them and counts the bytes beam-client sends, so
//! the test can check the wire size of a multi-MB binary. The approval prompt
//! reads its answer from a FIFO (`BEAM_PROMPT_TTY`), and with no terminal it
//! prints the prompt to stderr.

use beam_tests::bin_dir;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SESSION: &str = "loopback-session";
const PROMPT: &str = "allow? [y/N]";

struct Client {
    child: Child,
    port: u16,
    stderr: PathBuf,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn start(bins: &Path, client_dir: &Path, fifo: &Path, stderr: PathBuf) -> Client {
        let mut child = Command::new(bins.join("beam-client"))
            .args(["listen", "--session-id", SESSION, "--port", "0"])
            .env(
                "PATH",
                format!("{}:/usr/local/bin:/usr/bin:/bin", client_dir.display()),
            )
            .env("BEAM_PROMPT_TTY", fifo)
            .env("BEAM_PROMPT_TIMEOUT", "20")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .expect("start beam-client listen");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .trim()
            .strip_prefix("Listening on port ")
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("unexpected listen output: {line:?}"));
        Client {
            child,
            port,
            stderr,
        }
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

/// Forward one connection from a fresh port to `target`, counting the bytes
/// that come back from `target` (what beam-client sends).
fn counting_proxy(target: u16) -> (u16, Arc<AtomicU64>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicU64::new(0));
    let c = count.clone();
    let handle = std::thread::spawn(move || {
        let (down, _) = listener.accept().unwrap();
        let up = TcpStream::connect(("127.0.0.1", target)).unwrap();
        let (mut down_r, mut up_w) = (down.try_clone().unwrap(), up.try_clone().unwrap());
        let to_client = std::thread::spawn(move || {
            let _ = std::io::copy(&mut down_r, &mut up_w);
            let _ = up_w.shutdown(std::net::Shutdown::Write);
        });
        let (mut up_r, mut down_w) = (up, down);
        let mut buf = [0u8; 64 * 1024];
        loop {
            match up_r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    c.fetch_add(n as u64, Ordering::Relaxed);
                    if down_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = down_w.shutdown(std::net::Shutdown::Write);
        let _ = to_client.join();
    });
    (port, count, handle)
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run the handler for `command` as the server's shell hook would.
fn handle(bins: &Path, port: u16, command: &str, args: &[&str]) -> Child {
    Command::new(bins.join("beam-server-handler"))
        .arg("handle")
        .arg(command)
        .args(args)
        .env("BEAM_SESSION_ID", SESSION)
        .env("BEAM_LISTENER_PORT", port.to_string())
        .env("PATH", format!("{}:/usr/bin:/bin", bins.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start beam-server-handler")
}

fn finish(child: Child) -> Run {
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn write_script(path: &Path, body: &[u8]) {
    std::fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A shell script of about `payload` bytes: it prints a line and exits before
/// the filler text that follows.
fn big_script(name: &str, payload: usize) -> Vec<u8> {
    let mut s = format!("#!/bin/sh\necho \"{name} ok: $*\"\nexit 0\n").into_bytes();
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    while s.len() < payload {
        for _ in 0..76 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            s.push(alphabet[(seed % 64) as usize]);
        }
        s.push(b'\n');
    }
    s
}

fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_binaries_over_tcp() {
    let bins = bin_dir();
    let tmp = tempfile::tempdir().unwrap();
    let client_dir = tmp.path().join("client-bin");
    std::fs::create_dir_all(&client_dir).unwrap();
    let fifo = tmp.path().join("answers");
    mkfifo(&fifo);
    // Held open read-write so answers wait in the pipe until the prompt reads them.
    let mut answers = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    let client = Client::start(&bins, &client_dir, &fifo, tmp.path().join("client.err"));

    // 1. A multi-MB binary transfers, runs, and costs ~1x its size on the wire.
    let big = big_script("beam-big", 5 * 1024 * 1024);
    write_script(&client_dir.join("beam-big"), &big);
    answers.write_all(b"y\n").unwrap();
    let (proxy, count, proxy_thread) = counting_proxy(client.port);
    let started = Instant::now();
    let run = finish(handle(&bins, proxy, "beam-big", &["--flag", "x y"]));
    let took = started.elapsed();
    proxy_thread.join().unwrap();
    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert_eq!(run.stdout, "beam-big ok: --flag x y\n");
    let wire = count.load(Ordering::Relaxed);
    let ratio = wire as f64 / big.len() as f64;
    println!(
        "multi-MB: file {} bytes, beam-client sent {wire} bytes (ratio {ratio:.5}) in {took:?}",
        big.len()
    );
    assert!(wire > big.len() as u64);
    assert!(ratio < 1.001, "wire/file ratio {ratio}");

    // 2. Swapped mid-prompt for a *bigger* file: the handler refuses at the
    //    first byte past the approved size, exits 126, and nothing runs.
    let path = client_dir.join("beam-swap");
    write_script(&path, b"#!/bin/sh\necho ORIGINAL\nexit 0\n");
    let child = handle(&bins, client.port, "beam-swap", &[]);
    let before = client.stderr().matches(PROMPT).count();
    wait_for("the swap prompt", || {
        client.stderr().matches(PROMPT).count() > before
    });
    write_script(&path, &big_script("TAMPERED", 300 * 1024));
    answers.write_all(b"y\n").unwrap();
    let run = finish(child);
    assert_eq!(run.code, 126, "stderr:\n{}", run.stderr);
    assert!(
        run.stderr.contains("integrity check failed"),
        "{}",
        run.stderr
    );
    assert!(!run.stdout.contains("TAMPERED") && !run.stdout.contains("ORIGINAL"));

    // 3. Denied: exit 127, nothing sent or run.
    write_script(&client_dir.join("beam-deny"), b"#!/bin/sh\necho RAN\n");
    answers.write_all(b"n\n").unwrap();
    let run = finish(handle(&bins, client.port, "beam-deny", &[]));
    assert_eq!(run.code, 127);
    assert!(
        run.stderr
            .contains("not run (denied by user on the client)")
    );
    assert!(!run.stdout.contains("RAN"));

    // 4. A 1.x handler talks to this client: beam-client says why on its own
    //    terminal and answers with an `error` naming both versions, which
    //    every handler version prints.
    let mut s = TcpStream::connect(("127.0.0.1", client.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"BEAM\0\x031.1").unwrap();
    s.write_all(
        format!(
            "{{\"type\":\"request\",\"session_id\":\"{SESSION}\",\"command\":\"ls\",\
             \"args\":[],\"arch\":\"x86_64\",\"os\":\"linux\"}}\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).unwrap();
    println!("reply to a 1.1 handler: {line}");
    assert!(line.contains("\"type\":\"error\""), "{line}");
    assert!(line.contains(SESSION), "{line}");
    assert!(
        line.contains("protocol version mismatch") && line.contains("2.0") && line.contains("1.1"),
        "{line}"
    );
    wait_for("the client's mismatch message", || {
        client.stderr().contains("protocol version mismatch")
    });

    // 5. This handler talks to a 1.x client, which reads our version and hangs
    //    up without a word: the handler says what most likely happened.
    let old = TcpListener::bind("127.0.0.1:0").unwrap();
    let old_port = old.local_addr().unwrap().port();
    let old_client = std::thread::spawn(move || {
        let (mut s, _) = old.accept().unwrap();
        let mut hs = [0u8; 9];
        s.read_exact(&mut hs).unwrap();
        assert_eq!(&hs, b"BEAM\0\x032.0");
        // A 1.1 client returns here and drops the connection.
    });
    let run = finish(handle(&bins, old_port, "beam-big", &[]));
    old_client.join().unwrap();
    println!("handler vs a 1.1 client: {}", run.stderr);
    assert_eq!(run.code, 127);
    assert!(
        run.stderr.contains("older Beam protocol") && run.stderr.contains("2.0"),
        "{}",
        run.stderr
    );

    // 6. A client on another version answers like case 4: the handler prints it.
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    let other_client = std::thread::spawn(move || {
        let (s, _) = other.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut hs = [0u8; 9];
        r.read_exact(&mut hs).unwrap();
        let mut req = String::new();
        r.read_line(&mut req).unwrap();
        let mut w = s;
        w.write_all(
            format!(
                "{{\"type\":\"error\",\"session_id\":\"{SESSION}\",\"message\":\"protocol \
                 version mismatch: this side speaks 3.0, the other side speaks 2.0; install \
                 the same Beam version on both ends\"}}\n"
            )
            .as_bytes(),
        )
        .unwrap();
    });
    let run = finish(handle(&bins, other_port, "beam-big", &[]));
    other_client.join().unwrap();
    assert_eq!(run.code, 127);
    assert!(
        run.stderr
            .contains("not run (client error: protocol version mismatch"),
        "{}",
        run.stderr
    );

    drop(client);
}
