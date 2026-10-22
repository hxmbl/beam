use beam_common::{
    ClientMessage, MAGIC_BYTES, PROTOCOL_VERSION, ProtocolError, Result, ServerMessage,
};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::TcpStream as AsyncTcpStream;
use tokio::time::timeout;

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
        /// Command arguments
        args: Vec<String>,
    },
    /// Install shell integration
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
    fn new() -> Result<Self> {
        let session_id =
            std::env::var("BEAM_SESSION_ID").map_err(|_| ProtocolError::InvalidFormat)?;
        let listener_port = std::env::var("BEAM_LISTENER_PORT")
            .map_err(|_| ProtocolError::InvalidFormat)?
            .parse()
            .map_err(|_| ProtocolError::InvalidFormat)?;

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

    async fn send_handshake(&self, stream: &mut AsyncTcpStream) -> Result<()> {
        stream.write_all(MAGIC_BYTES).await?;
        let version_bytes = PROTOCOL_VERSION.as_bytes();
        stream.write_all(&[version_bytes.len() as u8]).await?;
        stream.write_all(version_bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn send_request(
        &self,
        stream: &mut AsyncTcpStream,
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
        stream.write_all(json.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        Ok(())
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

    async fn read_response<R: AsyncRead + Unpin>(
        &self,
        reader: &mut BufReader<R>,
    ) -> Result<ServerMessage> {
        let mut line = String::new();

        let n = timeout(Duration::from_secs(30), reader.read_line(&mut line))
            .await
            .map_err(|_| ProtocolError::ConnectionClosed)??;
        if n == 0 {
            return Err(ProtocolError::ConnectionClosed);
        }

        let msg: ServerMessage = serde_json::from_str(line.trim())?;
        Ok(msg)
    }

    async fn handle_binary_stream<R: AsyncRead + Unpin>(
        &self,
        reader: &mut BufReader<R>,
        expected_hash: &str,
        expected_size: u64,
    ) -> Result<PathBuf> {
        let mut temp_file = NamedTempFile::new()?;
        let temp_path = temp_file.path().to_path_buf();
        let mut hasher = blake3::Hasher::new();
        let mut received = 0u64;
        let mut line = String::new();

        loop {
            line.clear();
            let n = timeout(Duration::from_secs(30), reader.read_line(&mut line))
                .await
                .map_err(|_| ProtocolError::ConnectionClosed)??;
            if n == 0 {
                return Err(ProtocolError::ConnectionClosed);
            }

            let msg: ServerMessage = serde_json::from_str(line.trim())?;

            match msg {
                ServerMessage::BinaryChunk {
                    session_id,
                    offset,
                    data,
                    is_final,
                } => {
                    if session_id != self.session_id {
                        return Err(ProtocolError::SessionMismatch {
                            expected: self.session_id.clone(),
                            actual: session_id,
                        });
                    }

                    if offset != received {
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
                ServerMessage::Error { message: _, .. } => {
                    return Err(ProtocolError::InvalidFormat);
                }
                _ => {
                    return Err(ProtocolError::InvalidFormat);
                }
            }
        }

        // Verify hash
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
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(&temp_path, perms)?;

        // Keep the temp file
        temp_file.disable_cleanup(true);
        Ok(temp_path)
    }

    async fn execute_binary(&self, path: &Path, args: &[String]) -> Result<i32> {
        let mut cmd = Command::new(path);
        cmd.args(args);

        // Preserve stdin, stdout, stderr
        cmd.stdin(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());

        let status = cmd.status()?;
        Ok(status.code().unwrap_or(-1))
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

        // Use the reader for all reads
        let response = self.read_response(&mut reader).await?;

        match response {
            ServerMessage::Response {
                exists,
                compatible,
                size,
                blake3_hash,
                error,
                ..
            } => {
                if !exists {
                    eprintln!("beam: command '{}' not found on client", command);
                    if let Some(err) = error {
                        eprintln!("beam: {}", err);
                    }
                    return Ok(127); // command not found
                }

                if !compatible {
                    eprintln!(
                        "beam: binary architecture/OS mismatch (client: {}/{}, server: {}/{})",
                        self.arch,
                        self.os,
                        blake3_hash.unwrap_or_default(),
                        ""
                    );
                    return Ok(126); // command found but not executable
                }

                let size = size.ok_or(ProtocolError::InvalidFormat)?;
                let hash = blake3_hash.ok_or(ProtocolError::InvalidFormat)?;

                // Prompt user for approval (via client)
                // The client will prompt and send Approve message
                // We wait for the binary stream

                let binary_path = self.handle_binary_stream(&mut reader, &hash, size).await?;
                let exit_code = self.execute_binary(&binary_path, args).await?;

                // Cleanup temp file
                let _ = std::fs::remove_file(&binary_path);

                Ok(exit_code)
            }
            ServerMessage::Error { message, .. } => {
                eprintln!("beam: server error: {}", message);
                Ok(1)
            }
            _ => Err(ProtocolError::InvalidFormat),
        }
    }

    fn install_shell_integration(shell: &str, system: bool) -> String {
        let _install_path = if system {
            match shell {
                "bash" => "/etc/bash.bashrc".to_string(),
                "zsh" => "/etc/zsh/zshrc".to_string(),
                "fish" => "/etc/fish/conf.d/beam.fish".to_string(),
                _ => return String::new(),
            }
        } else {
            match shell {
                "bash" => "~/.bashrc".to_string(),
                "zsh" => "~/.zshrc".to_string(),
                "fish" => "~/.config/fish/conf.d/beam.fish".to_string(),
                _ => return String::new(),
            }
        };

        match shell {
            "bash" | "zsh" => format!(
                r#"
# Beam command-not-found handler
beam_command_not_found_handle() {{
    local cmd="$1"
    shift
    if command -v beam-server-handler >/dev/null 2>&1; then
        beam-server-handler handle "$cmd" "$@"
        return $?
    fi
    return 127
}}

# Bash
if [ -n "$BASH_VERSION" ]; then
    command_not_found_handle() {{
        beam_command_not_found_handle "$@"
    }}
fi

# Zsh
if [ -n "$ZSH_VERSION" ]; then
    command_not_found_handler() {{
        beam_command_not_found_handle "$@"
    }}
fi
"#
            ),
            "fish" => r#"
# Beam command-not-found handler for fish
function fish_command_not_found --on-event fish_command_not_found
    set -l cmd $argv[1]
    set -l args $argv[2..]
    if command -v beam-server-handler >/dev/null 2>&1
        beam-server-handler handle $cmd $args
        return $status
    end
    return 127
end
"#
            .to_string(),
            _ => String::new(),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Handle { command, args } => {
            let rt = tokio::runtime::Runtime::new()?;
            let handler = ServerHandler::new()?;
            let exit_code = rt.block_on(handler.handle_command(&command, &args))?;
            std::process::exit(exit_code);
        }
        Commands::Install { shell, system } => {
            let integration = ServerHandler::install_shell_integration(&shell, system);
            let path = shellexpand::tilde(&if system {
                match shell.as_str() {
                    "bash" => "/etc/bash.bashrc".to_string(),
                    "zsh" => "/etc/zsh/zshrc".to_string(),
                    "fish" => "/etc/fish/conf.d/beam.fish".to_string(),
                    _ => return Ok(()),
                }
            } else {
                match shell.as_str() {
                    "bash" => "~/.bashrc".to_string(),
                    "zsh" => "~/.zshrc".to_string(),
                    "fish" => "~/.config/fish/conf.d/beam.fish".to_string(),
                    _ => return Ok(()),
                }
            })
            .to_string();

            // Create directory if needed
            if let Some(parent) = Path::new(&path).parent() {
                std::fs::create_dir_all(parent).ok();
            }

            // Append to file
            use std::fs::OpenOptions;
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            file.write_all(b"\n# Beam integration\n")?;
            file.write_all(integration.as_bytes())?;

            println!("Installed beam integration for {} at {}", shell, path);
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
