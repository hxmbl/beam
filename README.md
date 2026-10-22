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

Build from source (Rust 1.88+):

```bash
git clone https://github.com/hxmbl/beam && cd beam
cargo build --release
```

### On your machine (the one you ssh from)

```bash
cp target/release/beam-client ~/.local/bin/          # anywhere on PATH

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
| Hash mismatch or wrong architecture | 126 (nothing runs) |

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
- **Slow for big binaries.** Chunks are sent as JSON arrays, about 3–4× the file's size on the wire.
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

The end-to-end test starts its own throwaway `sshd`, drives the real wrapper and hook in a pseudo-terminal, and types the answers. It covers approve, deny, timeout, a binary swapped mid-prompt, a command missing on both ends, and zsh on both sides. CI runs all of it on every push.

The workspace is three crates: `beam-client`, `beam-server-handler` and `beam-common` (the protocol).

<details>
<summary>Protocol (v1.1)</summary>

JSON Lines over the tunnelled TCP connection. The server handler speaks first.

1. **Handshake:** magic bytes `BEAM\0` plus a length-prefixed protocol version.
2. **Request** (server → client): `{"type":"request","session_id":"…","command":"foo","args":[],"arch":"x86_64","os":"linux"}`
3. **Response** (client → server): `{"type":"response","session_id":"…","exists":true,"compatible":true,"size":12345,"blake3_hash":"…","error":null}`
4. **Approval** (client → server): `{"type":"approval","session_id":"…","approved":true,"reason":null}`. If not approved, `reason` says why and nothing follows.
5. **Binary chunks** (client → server): `{"type":"binary_chunk","session_id":"…","offset":0,"data":[…],"is_final":false}`, ending with `"is_final":true`. The server verifies size and hash before running anything.

The Rust type names are historical: `ClientMessage` is what the handler sends, `ServerMessage` is what `beam-client` sends.

</details>

## License

MIT — see [LICENSE](LICENSE).
