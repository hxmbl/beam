use beam_common::{
    ClientMessage, MAGIC_BYTES, PROTOCOL_VERSION, ProtocolError, Result, ServerMessage,
};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::TcpStream as AsyncTcpStream;
use tokio::time::timeout;

mod hook;

/// Exit code for "command not found", matching the shell's own.
const EXIT_NOT_FOUND: i32 = 127;
/// Exit code for "found but cannot be run" (arch mismatch, failed integrity check).
const EXIT_CANNOT_RUN: i32 = 126;
/// Longest we wait for the user to answer on the client. The client denies on
/// its own after its prompt timeout (30s by default), so this is only a backstop.
const APPROVAL_WAIT: Duration = Duration::from_secs(300);

#[derive(Parser)]
#[command(
    name = "beam-server-handler",
    version,
    about = "Beam server handler - invoked by shell command_not_found hook"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Handle a command-not-found event
    Handle {
        /// Command name
        command: String,
        /// Command arguments (passed through untouched, including ones that start with '-')
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the command-not-found hook for a shell (eval it from the shell rc)
    ShellInit {
        /// Shell type (bash, zsh, fish)
        #[arg(long, default_value = "bash")]
        shell: String,
    },
    /// Append the command-not-found hook to the shell rc file
    Install {
        /// Shell type (bash, zsh, fish)
        #[arg(long, default_value = "bash")]
        shell: String,
        /// Install system-wide (requires root)
        #[arg(long)]
        system: bool,
    },
    /// Test connection to client
    TestConnection {
        /// Session ID
        #[arg(long)]
        session_id: String,
        /// Listener port
        #[arg(long)]
        port: u16,
    },
}

struct ServerHandler {
    session_id: String,
    listener_port: u16,
    arch: String,
    os: String,
}

impl ServerHandler {
    fn from_env() -> std::result::Result<Self, String> {
        let session_id = std::env::var("BEAM_SESSION_ID")
            .map_err(|_| "BEAM_SESSION_ID is not set (is AcceptEnv configured?)".to_string())?;
        let listener_port = std::env::var("BEAM_LISTENER_PORT")
            .map_err(|_| "BEAM_LISTENER_PORT is not set (is AcceptEnv configured?)".to_string())?
            .parse()
            .map_err(|_| "BEAM_LISTENER_PORT is not a port number".to_string())?;

        Ok(Self {
            session_id,
            listener_port,
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
        })
    }

    async fn connect_to_client(&self) -> Result<AsyncTcpStream> {
        let addr = format!("127.0.0.1:{}", self.listener_port);
        let stream = timeout(Duration::from_secs(5), AsyncTcpStream::connect(&addr))
            .await
            .map_err(|_| ProtocolError::ConnectionClosed)??;
        Ok(stream)
    }

    async fn send_handshake_to_writer(
        &self,
        writer: &mut tokio::io::WriteHalf<AsyncTcpStream>,
    ) -> Result<()> {
        writer.write_all(MAGIC_BYTES).await?;
        let version_bytes = PROTOCOL_VERSION.as_bytes();
        writer.write_all(&[version_bytes.len() as u8]).await?;
        writer.write_all(version_bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn send_request_to_writer(
        &self,
        writer: &mut tokio::io::WriteHalf<AsyncTcpStream>,
        command: &str,
        args: &[String],
    ) -> Result<()> {
        let msg = ClientMessage::Request {
            session_id: self.session_id.clone(),
            command: command.to_string(),
            args: args.to_vec(),
            arch: self.arch.clone(),
            os: self.os.clone(),
        };

        let json = serde_json::to_string(&msg)?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_message<R: AsyncRead + Unpin>(
        &self,
        reader: &mut BufReader<R>,
        wait: Duration,
    ) -> Result<ServerMessage> {
        let mut line = String::new();

        let n = timeout(wait, reader.read_line(&mut line))
            .await
            .map_err(|_| ProtocolError::ConnectionClosed)??;
        if n == 0 {
            return Err(ProtocolError::ConnectionClosed);
        }

        let msg: ServerMessage = serde_json::from_str(line.trim())?;
        if msg.session_id() != self.session_id {
            return Err(ProtocolError::SessionMismatch {
                expected: self.session_id.clone(),
                actual: msg.session_id().to_string(),
            });
        }
        Ok(msg)
    }

    async fn handle_binary_stream<R: AsyncRead + Unpin>(
        &self,
        reader: &mut BufReader<R>,
        expected_hash: &str,
        expected_size: u64,
    ) -> Result<PathBuf> {
        let mut temp_file = NamedTempFile::with_prefix("beam-")?;
        let temp_path = temp_file.path().to_path_buf();
        let mut hasher = blake3::Hasher::new();
        let mut received = 0u64;

        loop {
            match self.read_message(reader, Duration::from_secs(30)).await? {
                ServerMessage::BinaryChunk {
                    offset,
                    data,
                    is_final,
                    ..
                } => {
                    if offset != received {
                        return Err(ProtocolError::InvalidFormat);
                    }
                    if received + data.len() as u64 > expected_size {
                        // More bytes than advertised: refuse before writing them.
                        return Err(ProtocolError::InvalidFormat);
                    }

                    if !data.is_empty() {
                        hasher.update(&data);
                        temp_file.write_all(&data)?;
                        received += data.len() as u64;
                    }

                    if is_final {
                        break;
                    }
                }
                _ => return Err(ProtocolError::InvalidFormat),
            }
        }

        // Verify hash (the temp file is deleted on drop if this fails)
        let actual_hash = hasher.finalize().to_hex().to_string();
        if actual_hash != expected_hash {
            return Err(ProtocolError::Blake3Mismatch {
                expected: expected_hash.to_string(),
                actual: actual_hash,
            });
        }

        // Verify size
        if received != expected_size {
            return Err(ProtocolError::InvalidFormat);
        }

        // Make executable
        let mut perms = std::fs::metadata(&temp_path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o700);
        }
        std::fs::set_permissions(&temp_path, perms)?;

        // Keep the temp file; the caller removes it after running it. The
        // write handle is closed here so exec does not fail with ETXTBSY.
        temp_file.disable_cleanup(true);
        Ok(temp_path)
    }

    fn execute_binary(&self, path: &Path, command: &str, args: &[String]) -> Result<i32> {
        let mut cmd = Command::new(path);
        cmd.args(args);
        #[cfg(unix)]
        {
            // argv[0] is the name the user typed, as if it were installed.
            use std::os::unix::process::CommandExt;
            cmd.arg0(command);
        }
        #[cfg(not(unix))]
        let _ = command;

        // Preserve stdin, stdout, stderr
        cmd.stdin(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());

        let status = cmd.status()?;
        if let Some(code) = status.code() {
            return Ok(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return Ok(128 + sig);
            }
        }
        Ok(1)
    }

    pub async fn handle_command(&self, command: &str, args: &[String]) -> Result<i32> {
        let stream = self.connect_to_client().await?;

        // Split stream into reader and writer halves
        let (reader_half, writer_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader_half);
        let mut writer = writer_half;

        // Write handshake and request directly to writer
        self.send_handshake_to_writer(&mut writer).await?;
        self.send_request_to_writer(&mut writer, command, args)
            .await?;

        let response = self
            .read_message(&mut reader, Duration::from_secs(30))
            .await?;

        let (size, hash) = match response {
            ServerMessage::Response {
                exists,
                compatible,
                size,
                blake3_hash,
                ..
            } => {
                if !exists {
                    eprintln!("beam: {command}: command not found here or on the client");
                    return Ok(EXIT_NOT_FOUND);
                }

                if !compatible {
                    eprintln!(
                        "beam: {command}: the client's binary does not match this server ({}/{})",
                        self.arch, self.os
                    );
                    return Ok(EXIT_CANNOT_RUN);
                }

                (
                    size.ok_or(ProtocolError::InvalidFormat)?,
                    blake3_hash.ok_or(ProtocolError::InvalidFormat)?,
                )
            }
            ServerMessage::Error { message, .. } => {
                eprintln!("beam: client error: {message}");
                return Ok(EXIT_NOT_FOUND);
            }
            _ => return Err(ProtocolError::InvalidFormat),
        };

        // The client is now asking the user on its own terminal (with ssh
        // paused, so nothing we print would show up yet). Wait for the decision.
        match self.read_message(&mut reader, APPROVAL_WAIT).await? {
            ServerMessage::Approval { approved: true, .. } => {}
            ServerMessage::Approval {
                approved: false,
                reason,
                ..
            } => {
                let why = reason.unwrap_or_else(|| "denied on the client".to_string());
                eprintln!("beam: {command}: not run ({why})");
                return Ok(EXIT_NOT_FOUND);
            }
            ServerMessage::Error { message, .. } => {
                eprintln!("beam: {command}: not run (client error: {message})");
                return Ok(EXIT_NOT_FOUND);
            }
            _ => return Err(ProtocolError::InvalidFormat),
        }

        let binary_path = match self.handle_binary_stream(&mut reader, &hash, size).await {
            Ok(p) => p,
            Err(ProtocolError::Blake3Mismatch { expected, actual }) => {
                eprintln!(
                    "beam: {command}: integrity check failed, not running it \
                     (BLAKE3 expected {}…, got {}…)",
                    &expected[..16.min(expected.len())],
                    &actual[..16.min(actual.len())]
                );
                return Ok(EXIT_CANNOT_RUN);
            }
            Err(e) => return Err(e),
        };
        drop(reader);
        drop(writer);

        let result = self.execute_binary(&binary_path, command, args);

        // Cleanup temp file
        let _ = std::fs::remove_file(&binary_path);

        result
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Handle { command, args } => {
            let handler = match ServerHandler::from_env() {
                Ok(h) => h,
                Err(why) => {
                    eprintln!("beam: {command}: command not found ({why})");
                    std::process::exit(EXIT_NOT_FOUND);
                }
            };
            let rt = tokio::runtime::Runtime::new()?;
            let exit_code = match rt.block_on(handler.handle_command(&command, &args)) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("beam: {command}: command not found (beam: {e})");
                    EXIT_NOT_FOUND
                }
            };
            std::process::exit(exit_code);
        }
        Commands::ShellInit { shell } => match hook::script(&shell) {
            Some(script) => print!("{script}"),
            None => {
                eprintln!("beam: unsupported shell '{shell}' (use bash, zsh or fish)");
                std::process::exit(2);
            }
        },
        Commands::Install { shell, system } => {
            let (Some(script), Some(rc)) = (hook::script(&shell), hook::rc_path(&shell, system))
            else {
                eprintln!("beam: unsupported shell '{shell}' (use bash, zsh or fish)");
                std::process::exit(2);
            };
            let path = shellexpand::tilde(rc).to_string();

            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if existing.contains(hook::MARKER) {
                println!("Beam hook for {shell} is already in {path}");
                return Ok(());
            }

            // Create directory if needed
            if let Some(parent) = Path::new(&path).parent() {
                std::fs::create_dir_all(parent).ok();
            }

            // Append to file
            use std::fs::OpenOptions;
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            writeln!(file, "\n{}", hook::MARKER)?;
            file.write_all(script.as_bytes())?;
            writeln!(file, "# <<< beam command-not-found hook <<<")?;

            println!("Installed the Beam hook for {shell} in {path}");
        }
        Commands::TestConnection { session_id, port } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let addr = format!("127.0.0.1:{}", port);
                let mut stream = timeout(Duration::from_secs(5), AsyncTcpStream::connect(&addr))
                    .await
                    .map_err(|_| ProtocolError::ConnectionClosed)??;

                stream.write_all(MAGIC_BYTES).await?;
                let version_bytes = PROTOCOL_VERSION.as_bytes();
                stream.write_all(&[version_bytes.len() as u8]).await?;
                stream.write_all(version_bytes).await?;
                stream.flush().await?;

                let msg = ClientMessage::Ping { session_id };
                let json = serde_json::to_string(&msg)?;
                stream.write_all(json.as_bytes()).await?;
                stream.write_all(b"\n").await?;
                stream.flush().await?;

                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                    .map_err(|_| ProtocolError::ConnectionClosed)??;

                let response: ServerMessage = serde_json::from_str(line.trim())?;
                match response {
                    ServerMessage::Pong { session_id } => {
                        println!("Connection successful: session {}", session_id);
                    }
                    _ => {
                        eprintln!("Unexpected response");
                        std::process::exit(1);
                    }
                }
                Ok::<(), ProtocolError>(())
            })?;
        }
    }

    Ok(())
}
