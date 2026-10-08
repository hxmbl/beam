# Beam

**Work in Progress - Prototype Stage**

Beam streams missing binaries from your client to a remote server over an existing SSH connection.

> **Important**: This is currently a prototype implementation. The core protocol and components exist, but the end-to-end SSH integration and user approval flow are incomplete. See "Current Status" below for details.

## How It Works (Intended Design)

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
   - Client prompts user for approval *(not yet implemented)*
   - If approved, client streams binary with BLAKE3 verification
   - Server executes binary with original arguments, preserving stdin/stdout/stderr/exit code

## Current Status

### Implemented
- **Workspace structure**: Clean separation into `beam-client`, `beam-server-handler`, `beam-common`
- **Protocol design**: JSON Lines protocol with magic bytes and version negotiation
- **Client binary discovery**: Finds binaries in PATH and allowed paths
- **Binary metadata**: Size and BLAKE3 hash computation
- **Architecture/OS detection**: Basic compatibility checking
- **Binary streaming**: Chunked binary transfer with BLAKE3 verification
- **Server binary execution**: Temp file creation, execution, cleanup
- **Shell integration scripts**: Bash/zsh/fish wrapper scripts (skeletal)
- **Tests**: Unit tests for protocol, integration tests for handshake and streaming

### Incomplete/Prototype
- **User approval flow**: Approval message is created but not sent over the connection
- **SSH reverse port forwarding**: Basic implementation exists but not tested end-to-end
- **Shell integration**: Scripts exist but not integrated with actual SSH workflow
- **Error handling**: Basic error handling exists but needs refinement
- **Session management**: Session tracking is skeletal
- **SSH configuration**: Example config provided but not validated

### Not Implemented
- **Complete SSH wrapper integration**: The `beam_ssh` function in shell-init is not wired up
- **Environment variable propagation**: SendEnv/AcceptEnv not tested
- **ControlMaster handling**: Multiplexing behavior not implemented
- **Timeout handling**: Basic timeouts exist but need refinement
- **Retry logic**: No retry mechanism for failed connections
- **Caching**: Binary cache structure exists but not utilized

## Quick Start

Get started with the prototype in a few minutes:

```bash
# Clone and build
git clone <repo-url>
cd beam
cargo build --workspace

# Run tests to verify everything works
cargo test --workspace

# Test individual components
# Find a binary on your system
cargo run -p beam-client -- find-binary ls

# Compute BLAKE3 hash of a file
cargo run -p beam-client -- hash /bin/ls

# Test client-server connection (manual test)
# Terminal 1: Start client listener
cargo run -p beam-client -- listen --session-id test123 --port 0

# Terminal 2: Test server handler (use the port from Terminal 1)
BEAM_SESSION_ID=test123 BEAM_LISTENER_PORT=<port> cargo run -p beam-server-handler -- handle ls -la
```

## Usage (When Complete)

*This section describes the intended usage once implementation is complete.*

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

## Security Model (Intended)

*Note: Security features are not fully implemented yet.*

- Binary transfer only happens after explicit user approval on client *(not yet implemented)*
- BLAKE3 hash verification ensures binary integrity
- Reverse tunnel only binds to localhost on client *(basic implementation exists)*
- Server can only request binaries for the active SSH session *(session management incomplete)*
- No persistent daemon required on either side

## Limitations

1. **ControlMaster multiplexing**: When `ControlMaster=auto` (default), multiple SSH sessions to the same host share the same Beam session. For per-session isolation, use `ControlMaster=no` in SSH config.

2. **Shell integration required**: User must source the shell integration script. This is a one-time setup.

3. **SSH config required**: Client needs `PermitLocalCommand`, `SendEnv`, `ControlMaster` in `~/.ssh/config`. Server needs `AcceptEnv` in `sshd_config`.

4. **Architecture/OS compatibility**: Binary must match server architecture and OS. No cross-compilation or emulation.

5. **First connection latency**: Initial connection sets up reverse tunnel, adding ~100-500ms latency.

## Architecture (Intended)

```
┌─────────────────┐     SSH Connection      ┌─────────────────┐
│    Client       │ ◄─────────────────────► │    Server       │
│                 │                         │                 │
│  beam-client    │ ◄── Reverse Tunnel ───► │ beam-server-handler│
│  (listener)     │     (localhost:port)    │ handler         │
│                 │                         │                 │
│  ~/.bashrc:     │                         │  command_not_   │
│  ssh() wrapper  │                         │  found_handle   │
└─────────────────┘                         └─────────────────┘
```

**Current state**: Components exist but the SSH wrapper integration and reverse tunnel setup are not yet functional.

## Protocol (Current Implementation)

Custom JSON Lines protocol over TCP:

1. **Handshake**: Magic bytes (`BEAM\0`) + protocol version
2. **Request**: Client → Server: `{"type":"request","session_id":"...","command":"foo","args":[],"arch":"x86_64","os":"linux"}`
3. **Response**: Server → Client: `{"type":"response","session_id":"...","exists":true,"compatible":true,"size":12345,"blake3_hash":"..."}`
4. **Binary streaming**: Server → Client: Binary chunks `{"type":"binary_chunk","session_id":"...","offset":0,"data":[...],"is_final":false}`
5. **Final chunk**: `{"type":"binary_chunk","...","is_final":true}` triggers BLAKE3 verification

**Note**: The approve message (`{"type":"approve",...}`) is defined in the protocol but not currently used in the actual implementation.

## Testing

```bash
# Run all tests (unit + integration)
cargo test --workspace

# Run only unit tests
cargo test --workspace --lib

# Run only integration tests
cargo test --workspace --test integration

# Manual component testing
# These test individual components but not the full end-to-end flow

# Test client binary finding
cargo run -p beam-client -- find-binary ls

# Test client hashing
cargo run -p beam-client -- hash /bin/ls

# Test server connection (requires running client listener first)
# Terminal 1:
cargo run -p beam-client -- listen --session-id test123 --port 0
# Terminal 2:
BEAM_SESSION_ID=test123 BEAM_LISTENER_PORT=<port_from_terminal_1> cargo run -p beam-server-handler -- handle ls -la
```

## Development Priorities

To move from prototype to working implementation, the following areas need work:

1. **Complete approval flow**: Wire up the approve message exchange between client and server
2. **SSH integration**: Implement the actual SSH wrapper workflow with environment variables
3. **End-to-end testing**: Test the full flow from SSH connection to binary execution
4. **Error handling**: Add proper error recovery and user-friendly error messages
5. **Session management**: Implement proper session tracking and cleanup
6. **Shell integration**: Make the shell scripts actually work with SSH
7. **Configuration validation**: Validate SSH config and server setup

## License

MIT — see [LICENSE](LICENSE).
