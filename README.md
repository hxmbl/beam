# Beam

**Work in Progress - early, but the end-to-end flow works**

Beam streams missing binaries from your client to a remote server over an existing SSH connection.

You type a command on the server that isn't installed there; if your own machine has it, Beam asks you (on your machine) whether to send it. Say yes and it is streamed over the SSH connection, checked with BLAKE3 and run on the server with your arguments, stdin/stdout/stderr and exit code.

> **Status**: approve, deny, prompt timeout and integrity-failure paths are tested end to end against a real `sshd` (see "Current Status" and "Testing"). It is still a prototype: see the limitations before relying on it.

## How It Works

1. **Client side**: your shell's `ssh` is a small function (from `beam-client shell-init`) that runs `beam-client ssh -- <your args>`, which:
   - Generates a unique session ID
   - Listens on a random port on `127.0.0.1`
   - Runs the real `ssh` as its child with a reverse forward (`-R <remote port>:127.0.0.1:<listener port>`) and `SendEnv` for the two variables below
   - Exports `BEAM_SESSION_ID` and `BEAM_LISTENER_PORT` (the *remote* end of the forward) to that ssh

2. **SSH transport**: the environment variables reach the server through `SendEnv`/`AcceptEnv`; the reverse forward carries Beam's traffic back to the client.

3. **Server side**: when a command is not found, the shell's command-not-found hook (from `beam-server-handler shell-init`) invokes `beam-server-handler`, which:
   - Reads `BEAM_SESSION_ID` and `BEAM_LISTENER_PORT` from the environment
   - Connects to `127.0.0.1:BEAM_LISTENER_PORT` (tunneled to the client's `beam-client`)
   - Requests the binary with command name, args, architecture, OS
   - The client checks that the binary exists and is compatible
   - **The client asks you on its own terminal**: `allow? [y/N]`, default no, denied automatically after 30s
   - If approved, the client streams the binary; the server verifies size and BLAKE3 hash
   - The server runs it with the original arguments, preserving stdin/stdout/stderr and exit code
   - If denied (or timed out) the server prints why and exits 127, like command-not-found

From the end-to-end test (`beam-hello` exists only on the client):

```text
$ ssh myserver 'echo piped-line | beam-hello -x "a b"'

beam: the remote host asked to run `beam-hello -x a b`, which it does not have.
beam:   send /tmp/.tmpWJ6RRZ/client-approve/beam-hello (79 bytes, blake3 48d1852c0afd4bd3…) and run it there?
beam: allow? [y/N] (denies in 20s) y
beam: approved
hello from client: -x a b
stdin: piped-line
$ echo $?
3
```

While the prompt is up, `beam-client` pauses its ssh child (SIGSTOP) so your answer is read on the client and never reaches the remote shell; ssh resumes as soon as you answer. Keys typed before the prompt appeared are discarded so they cannot approve anything.

## Current Status

### Works (tested end to end against a real sshd)
- **Approval flow**: TTY prompt on the client (y/N, default deny, timeout denies), decision sent to the server as an `approval` message; the server prints a clear message and exits 127 on denial or timeout
- **SSH wrapper**: `beam-client ssh` + `shell-init` for bash and zsh: reverse forward, `SendEnv`, env export, exit code passthrough
- **Server hook**: `command_not_found_handle` (bash) / `command_not_found_handler` (zsh), only active inside a Beam session, keeps any handler you already had
- **Env propagation**: `SendEnv`/`AcceptEnv` of `BEAM_SESSION_ID` / `BEAM_LISTENER_PORT`
- **Binary streaming with BLAKE3 verification**: a binary that changes after you approved it is refused (exit 126) and nothing runs
- **Execution**: args (including ones starting with `-`), stdin, stdout/stderr and exit code preserved; signals map to 128+n
- **Interactive sessions**: checked by hand with `ssh -tt` into an interactive remote bash (remote pty, local terminal in raw mode): the prompt shows, the answer is read locally, the session keeps working afterwards
- **CI**: GitHub Actions runs fmt, clippy `-D warnings`, tests, and the real-sshd end-to-end test

### Partial
- **fish**: client wrapper and server hook exist and parse, but are not covered by the e2e test. fish forces `$status` to 127 after its command-not-found handler, so the beamed program's output arrives but its exit code is lost.
- **ControlMaster**: works in principle (the extra `-R` and the env go through the master), but is not covered by tests. With `ControlPersist`, a finished session's reverse forward stays open on the server until the master exits.
- **Remote port choice**: the server-side forward port is picked at random (20000-59999). If it is taken on the server, Beam is unavailable for that session (the handler says it cannot reach the client); there is no retry.
- **Error handling**: failures degrade to exit 127 with a one-line message; there is no retry or reconnect.
- **Session management**: one listener per `ssh` run; nothing tracks or cleans up across sessions.

### Not Implemented
- **Caching**: every run streams the binary again (the client only caches the hash). No server-side cache.
- **Remembered approvals** ("always allow rg on this host")
- **Shared libraries / dependencies**: only the single file is sent. Dynamically linked binaries need the same libraries on the server; scripts need their interpreter there.
- **Cross-architecture**: client and server must match architecture and OS (checked).
- **macOS/BSD testing**: the code is Unix-generic, but only Linux has been tested.

## Setup

### Client (the machine you ssh from)

```bash
cargo build --release
cp target/release/beam-client ~/.local/bin/        # anywhere on PATH

# ~/.bashrc  (zsh: --shell zsh in ~/.zshrc)
eval "$(beam-client shell-init --shell bash)"
# fish: beam-client shell-init --shell fish | source
```

No `~/.ssh/config` changes are needed; the wrapper adds `-R` and `SendEnv` itself. `BEAM_DISABLE=1 ssh host` bypasses it. `ssh -O …`, `-V`, `-G` and `-Q` always go straight to ssh.

### Server (the machine you ssh to)

```bash
# /etc/ssh/sshd_config, then reload sshd
AcceptEnv BEAM_SESSION_ID BEAM_LISTENER_PORT
AllowTcpForwarding remote      # or yes

# put beam-server-handler on PATH, then in ~/.bashrc (or --shell zsh / fish):
eval "$(beam-server-handler shell-init --shell bash)"
# or once: beam-server-handler install --shell bash
```

See [`ssh_config.example`](ssh_config.example) for details, including ControlMaster notes and the non-interactive bash caveat.

### Settings

| Variable | Side | Default | Meaning |
|---|---|---|---|
| `BEAM_PROMPT_TIMEOUT` | client | `30` | Seconds before an unanswered prompt denies |
| `BEAM_PROMPT_TTY` | client | `/dev/tty` | Terminal to prompt on |
| `BEAM_SSH` | client | `ssh` | ssh program the wrapper runs |
| `BEAM_DISABLE` | client | unset | Set to bypass the wrapper |

## Quick Start (local, without SSH)

```bash
cargo build --workspace
cargo test --workspace

# Find a binary / hash a file
cargo run -p beam-client -- find-binary ls
cargo run -p beam-client -- hash /bin/ls

# Terminal 1: a listener (prompts on this terminal)
cargo run -p beam-client -- listen --session-id test123 --port 0
# Terminal 2: ask it for a command (use the port from terminal 1)
BEAM_SESSION_ID=test123 BEAM_LISTENER_PORT=<port> cargo run -p beam-server-handler -- handle ls -la
```

## Requirements

- **Client**: OpenSSH client, bash/zsh (fish partially)
- **Server**: OpenSSH server with `AcceptEnv` and remote TCP forwarding allowed
- **Both**: Rust 1.88+ to build (edition 2024)

## Security Model

- A binary is only sent after explicit approval on the client's own terminal. The answer is read on the client, not through the SSH session, so the server cannot approve its own request. Default is deny; no answer within the timeout is a deny.
- BLAKE3 hash and size are shown in the prompt and verified by the server; a mismatch means nothing runs.
- The listener binds to `127.0.0.1` on the client; the reverse forward binds to loopback on the server (keep `GatewayPorts no`).
- Requests must carry the session ID of the ssh run that started the listener.
- Caveat: on a multi-user server, other local users can connect to the forwarded loopback port. They still need the session ID, and you still have to approve, but a Unix-socket forward would be stricter (see open decisions in the PR).
- No persistent daemon on either side.

## Limitations

1. **Shell integration required** on both sides (one line in each rc file).
2. **Non-interactive commands on bash** (`ssh host 'cmd'`) only use the hook if your `~/.bashrc` loads it before any "not interactive, return" check.
3. **Architecture/OS must match**; no emulation or cross-compilation.
4. **Single-file binaries only**: libraries and interpreters are not sent.
5. **`noexec` /tmp**: the server writes the binary to the temp dir (`$TMPDIR`); if that is mounted `noexec`, running it fails.
6. **Transfer encoding**: chunks are JSON arrays of numbers, roughly 3-4x the binary's size on the wire. Fine for small tools, slow for big ones.

## Architecture

```
┌──────────────────────┐        SSH connection         ┌──────────────────────┐
│ Client               │ ◄───────────────────────────► │ Server               │
│                      │                               │                      │
│ ssh() → beam-client  │ ◄──── reverse tunnel (-R) ──► │ beam-server-handler  │
│   ssh -- <args>      │   remote 127.0.0.1:<rport>    │                      │
│   listener + prompt  │   → client 127.0.0.1:<lport>  │ command_not_found    │
│   └─ ssh (child)     │                               │ hook                 │
└──────────────────────┘                               └──────────────────────┘
```

## Protocol

Custom JSON Lines protocol over TCP (version `1.1`). The handler (server side) speaks first:

1. **Handshake**: magic bytes (`BEAM\0`) + length-prefixed protocol version
2. **Request** (handler → client): `{"type":"request","session_id":"...","command":"foo","args":[],"arch":"x86_64","os":"linux"}`
3. **Response** (client → handler): `{"type":"response","session_id":"...","exists":true,"compatible":true,"size":12345,"blake3_hash":"...","error":null}`
4. **Approval** (client → handler, after the user answers): `{"type":"approval","session_id":"...","approved":true,"reason":null}`; when `approved` is false, `reason` says why and nothing follows
5. **Binary chunks** (client → handler): `{"type":"binary_chunk","session_id":"...","offset":0,"data":[...],"is_final":false}`
6. **Final chunk**: `{"type":"binary_chunk",...,"is_final":true}`; the handler then verifies size and BLAKE3 before running anything

(The Rust type names are historical: `ClientMessage` is what the handler sends, `ServerMessage` what `beam-client` sends.)

## Testing

```bash
# Unit + integration tests (no sshd needed)
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# End-to-end against a real sshd on localhost (runs as your user;
# needs openssh-server and ssh-keygen; zsh optional)
BEAM_E2E_SSHD=1 cargo test -p beam-tests -- --nocapture
```

The end-to-end test starts its own `sshd` on a free port with a throwaway host key and `authorized_keys`, then drives the client in a pseudo-terminal through the real wrapper and hook, answering the prompt by typing. Cases: approve (output, args, stdin, exit code 3 kept), deny (127), prompt timeout (127), binary replaced while the prompt is up (integrity failure, 126, nothing runs), command missing on the client too (127, no prompt), and zsh on both sides. Set `BEAM_E2E_SSHD_BIN` if `sshd` is not at `/usr/sbin/sshd`.

## License

MIT — see [LICENSE](LICENSE).
