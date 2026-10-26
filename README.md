# Beam

**Run a command over SSH that the server doesn't have. Beam sends it from your machine, with your say-so.**

You're on a server and `rg` isn't installed. Your laptop has it. With Beam, you just type `rg`: your laptop asks whether to send it, and if you say yes, the binary goes over the SSH connection you already have, gets checked, and runs on the server as if it had been there all along.

Roughly what that looks like:

```text
you@laptop$ ssh myserver
myserver$ rg TODO src/

beam: the remote host asked to run `rg TODO src/`, which it does not have.
beam:   send /usr/bin/rg (5674208 bytes, blake3 a3f19c07e2b84d51…) and run it there?
beam: allow? [y/N] (denies in 30s) y
beam: approved
src/main.rs:12:    // TODO: handle errors
```

Nothing is installed on the server, no daemon runs anywhere, and nothing is sent without you approving it on your own terminal.

> **Early days.** The full flow is tested end to end against a real `sshd`, but Beam is young. Read [Limitations](#limitations) before you lean on it.

## Install

Download a prebuilt release from the [Releases page](https://github.com/hxmbl/beam/releases). Each archive is named `beam-<version>-<target>.tar.gz` and holds both binaries (`beam-client`, `beam-server-handler`) plus `LICENSE` and `README.md`. `SHA256SUMS` lists the checksums.

| Target | Platform |
| --- | --- |
| `x86_64-unknown-linux-musl` | Linux x86_64 (static) |
| `aarch64-unknown-linux-musl` | Linux arm64 (static) |
| `x86_64-apple-darwin` | macOS Intel |
| `aarch64-apple-darwin` | macOS Apple silicon |

The Linux builds are statically linked, so `beam-server-handler` can be copied onto any Linux host of that architecture.

```bash
v=0.1.11 t=x86_64-unknown-linux-musl
curl -LO https://github.com/hxmbl/beam/releases/download/v$v/beam-$v-$t.tar.gz
curl -LO https://github.com/hxmbl/beam/releases/download/v$v/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS      # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
tar -xzf beam-$v-$t.tar.gz                         # -> beam-$v-$t/beam-client, beam-$v-$t/beam-server-handler
```

Or build from source (Rust 1.88+); the binaries land in `target/release/`:

```bash
git clone https://github.com/hxmbl/beam && cd beam
cargo build --release
```

Both ends need the same Beam version.

### On your machine (the one you ssh from)

```bash
cp beam-client ~/.local/bin/                         # anywhere on PATH

# ~/.bashrc
eval "$(beam-client shell-init --shell bash)"
# zsh:  eval "$(beam-client shell-init --shell zsh)"   in ~/.zshrc
# fish: beam-client shell-init --shell fish | source   (partial support)
```

That's it for the client. No `~/.ssh/config` changes needed.

### On the server (the one you ssh to)

```bash
# /etc/ssh/sshd_config, then reload sshd
AcceptEnv BEAM_SESSION_ID BEAM_LISTENER_PORT
AllowTcpForwarding remote      # or yes

# put beam-server-handler on PATH, then add the hook to ~/.bashrc:
eval "$(beam-server-handler shell-init --shell bash)"
# or append it once: beam-server-handler install --shell bash
```

[`ssh_config.example`](ssh_config.example) has the details, including ControlMaster and non-interactive bash notes.

## How it works

1. Your shell's `ssh` becomes a thin wrapper around `beam-client ssh -- <your args>`. It opens a listener on `127.0.0.1`, then runs the real `ssh` with a reverse forward (`-R`) back to that listener and a session ID passed through `SendEnv`.
2. On the server, a command-not-found hook calls `beam-server-handler`, which asks your machine for the missing command through the tunnel.
3. Your machine finds the binary, checks it matches the server's OS and architecture, and asks you. While it asks, ssh is paused, so your answer stays local and can't be typed by the server.
4. If you approve, the binary is streamed and the server verifies its size and BLAKE3 hash before running it with your arguments, stdin, stdout, stderr and exit code intact.

| Outcome | Exit code |
|---|---|
| Ran | the program's own |
| Denied, timed out, or not on your machine either | 127 |
| Beam versions differ on the two ends | 127 (with a message saying so) |
| Size or hash mismatch, malformed stream, or wrong architecture | 126 (nothing runs) |

## Security

- **You approve every send**, on the client's own terminal. Default is no, and silence is no.
- **The server can't approve itself.** The prompt is read locally while ssh is paused; keys typed before the prompt are thrown away.
- **Integrity is checked.** The hash shown in the prompt is the one the server verifies. A binary swapped after approval is refused.
- **Loopback only.** The listener binds to `127.0.0.1`, and the reverse forward binds to loopback on the server (keep `GatewayPorts no`).
- **Session-scoped.** Requests must carry the session ID of the ssh run that started the listener.
- **Shared servers:** other local users could connect to the forwarded loopback port. They'd still need the session ID and your approval, but a Unix-socket forward would be stricter. It's on the list.

## Settings

| Variable | Side | Default | What it does |
|---|---|---|---|
| `BEAM_PROMPT_TIMEOUT` | client | `30` | Seconds before an unanswered prompt denies |
| `BEAM_PROMPT_TTY` | client | `/dev/tty` | Terminal to prompt on |
| `BEAM_SSH` | client | `ssh` | The ssh program the wrapper runs |
| `BEAM_DISABLE` | client | unset | Set it to skip Beam for one ssh |

`ssh -O …`, `-V`, `-G` and `-Q` always go straight to ssh.

## Limitations

- **Same OS and architecture** on both ends. No emulation or cross-compiling.
- **One file only.** Shared libraries and interpreters aren't sent, so dynamically linked tools need their libraries on the server.
- **Both ends need the same Beam version.** Protocol 2.0 doesn't talk to 1.x; a mismatch is refused with a message on both sides.
- **No caching.** Every run streams the binary again, and approvals aren't remembered.
- **fish** works, but the exit code is always 127 (fish forces it after its not-found handler). Not covered by tests.
- **ControlMaster** should work but isn't tested. With `ControlPersist`, a finished session's forward lingers until the master exits.
- **Port clashes:** the server-side port is random; if it's taken, Beam is off for that session.
- **`noexec` temp dirs** on the server stop the binary from running.
- **`ssh host 'cmd'` on bash** only uses the hook if `~/.bashrc` loads it before any "not interactive, return" line.
- Tested on Linux only.

## Development

```bash
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# End to end against a real sshd on localhost (needs openssh-server; zsh optional)
BEAM_E2E_SSHD=1 cargo test -p beam-tests -- --nocapture
```

The end-to-end test starts its own throwaway `sshd`, drives the real wrapper and hook in a pseudo-terminal, and types the answers. It covers approve, deny, timeout, a binary swapped mid-prompt, a command missing on both ends, a 4 MiB binary, and zsh on both sides. A second test (`tests/tests/loopback.rs`, part of `cargo test`) runs the real binaries over plain TCP and checks that a 5 MiB binary costs about 1× its size on the wire, that a bigger file swapped in mid-prompt is refused with 126, and the version-mismatch messages. CI runs all of it on every push.

The workspace is three crates: `beam-client`, `beam-server-handler` and `beam-common` (the protocol).

<details>
<summary>Protocol (v2.0)</summary>

One TCP connection through the tunnel. The server handler speaks first. Control messages are JSON Lines; the binary itself is raw frames.

1. **Handshake** (server → client): magic bytes `BEAM\0`, then the protocol version as one length byte plus text (`2.0`). On a mismatch `beam-client` says so on its terminal and replies with an `error` message naming both versions; a handler whose client hangs up during the handshake reports that the client probably runs an older Beam.
2. **Request** (server → client): `{"type":"request","session_id":"…","command":"foo","args":[],"arch":"x86_64","os":"linux"}`
3. **Response** (client → server): `{"type":"response","session_id":"…","exists":true,"compatible":true,"size":12345,"blake3_hash":"…","error":null}`
4. **Approval** (client → server): `{"type":"approval","session_id":"…","approved":true,"reason":null}`. If not approved, `reason` says why and nothing follows.
5. **Binary** (client → server, only after an approved `approval`): raw frames, each a `u32` big-endian length followed by that many bytes, at most 64 KiB per frame. A zero-length frame ends the stream. The server refuses a frame over 64 KiB or any byte past the approved size, then checks the total size and BLAKE3 hash before running anything. Overhead is 4 bytes per 64 KiB, so the wire size is about 1× the file.

Any step can be answered with `{"type":"error","session_id":"…","message":"…"}`, which the handler prints. The Rust type names are historical: `ClientMessage` is what the handler sends, `ServerMessage` is what `beam-client` sends.

1.x sent the binary as JSON `binary_chunk` messages with the bytes as an array of numbers, about 3–3.5× the file's size.

</details>

## License

MIT — see [LICENSE](LICENSE).
