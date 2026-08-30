# Beam

Beam streams missing binaries from your client to a remote server over an existing SSH connection.

## How It Works

1. **Client side**: When you run `ssh server`, a shell wrapper starts `beam-client` which:
   - Generates a unique session ID
   - Listens on a random local TCP port
   - Sets up a reverse port forward via SSH (`ssh -O forward -R port:localhost:port`)
   - Exports `BEAM_SESSION_ID` and `BEAM_LISTENER_PORT` environment variables

2. **SSH transport**: The environment variables are sent to the server via `SendEnv`/`AcceptEnv`

3. **Server side**: When a command is not found, the shell's `command_not_found_handle` invokes `beam-server-handler` which:
   - Reads `BEAM_SESSION_ID` and `BEAM_LISTENER_PORT` from environment
   - Connects to `localhost:BEAM_LISTENER_PORT` (tunneled to client's `beam-client`)
   - Requests the binary with command name, args, architecture, OS
   - Client checks if binary exists and is compatible
   - Client prompts user for approval
   - If approved, client streams binary with BLAKE3 verification
   - Server executes binary with original arguments, preserving stdin/stdout/stderr/exit code

## Installation

### Client (your machine)

```bash
# Build and install
cargo install --path beam-client

# Add shell integration (bash, zsh, or fish)
beam-client shell-init --shell bash >> ~/.bashrc
# or
beam-client shell-init --shell zsh >> ~/.zshrc
# or
beam-client shell-init --shell fish >> ~/.config/fish/conf.d/beam.fish

# Add SSH config
cat ssh_config.example >> ~/.ssh/config
```

### Server (remote machine)

```bash
# Build and install
cargo install --path beam-server

# Install shell integration (system-wide or user)
sudo beam-server-handler install --shell bash --system
# or
beam-server-handler install --shell bash

# Configure sshd to accept Beam environment variables
# Add to /etc/ssh/sshd_config:
# AcceptEnv BEAM_SESSION_ID BEAM_LISTENER_PORT
# AllowTcpForwarding yes
# PermitOpen localhost:*
# Then restart sshd: sudo systemctl restart sshd
```

## Usage

Just use `ssh` normally:

```bash
$ ssh myserver
$ some-command-not-on-server
# Beam detects it's missing, finds it on client, prompts for approval, streams and executes
```

## Requirements

- **Client**: OpenSSH 7.6+ (for `ssh -O forward`), bash/zsh/fish
- **Server**: OpenSSH server with `AcceptEnv`, `AllowTcpForwarding`, `PermitOpen`
- **Both**: Rust 1.70+ to build (or pre-built binaries)

## Security Model

- Binary transfer only happens after explicit user approval on client
- BLAKE3 hash verification ensures binary integrity
- Reverse tunnel only binds to localhost on client
- Server can only request binaries for the active SSH session
- No persistent daemon required on either side

## Limitations

1. **ControlMaster multiplexing**: When `ControlMaster=auto` (default), multiple SSH sessions to the same host share the same Beam session. For per-session isolation, use `ControlMaster=no` in SSH config.

2. **Shell integration required**: User must source the shell integration script. This is a one-time setup.

3. **SSH config required**: Client needs `PermitLocalCommand`, `SendEnv`, `ControlMaster` in `~/.ssh/config`. Server needs `AcceptEnv` in `sshd_config`.

4. **Architecture/OS compatibility**: Binary must match server architecture and OS. No cross-compilation or emulation.

5. **First connection latency**: Initial connection sets up reverse tunnel, adding ~100-500ms latency.

## Architecture

```
┌─────────────────┐     SSH Connection      ┌─────────────────┐
│    Client       │ ◄─────────────────────► │    Server       │
│                 │                         │                 │
│  beam-client    │ ◄── Reverse Tunnel ───► │ beam-server-    │
│  (listener)     │     (localhost:port)    │ handler         │
│                 │                         │                 │
│  ~/.bashrc:     │                         │  command_not_   │
│  ssh() wrapper  │                         │  found_handle   │
└─────────────────┘                         └─────────────────┘
```

## Protocol

Custom JSON Lines protocol over TCP:

1. Client → Server: `{"type":"request","session_id":"...","command":"foo","args":[],"arch":"x86_64","os":"linux"}`
2. Server → Client: `{"type":"response","session_id":"...","exists":true,"compatible":true,"size":12345,"blake3_hash":"..."}`
3. Client → Server: `{"type":"approve","session_id":"...","approved":true}`
4. Server → Client: Binary chunks `{"type":"binary_chunk","session_id":"...","offset":0,"data":[...],"is_final":false}`
5. Final chunk with `is_final:true` triggers BLAKE3 verification

## Testing

```bash
# Run unit tests
cargo test --workspace

# Manual test: start client listener
beam-client listen --session-id test123 --port 0

# In another terminal, test server handler
BEAM_SESSION_ID=test123 BEAM_LISTENER_PORT=<port> beam-server-handler handle ls -la
```
