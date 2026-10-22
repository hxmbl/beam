use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "request")]
    Request {
        session_id: String,
        command: String,
        args: Vec<String>,
        arch: String,
        os: String,
    },
    #[serde(rename = "ping")]
    Ping { session_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "response")]
    Response {
        session_id: String,
        exists: bool,
        compatible: bool,
        size: Option<u64>,
        blake3_hash: Option<String>,
        error: Option<String>,
    },
    /// The user's decision on the client, sent after a `Response` with
    /// `exists && compatible`. When `approved` is true the binary follows as
    /// raw length-prefixed frames (see [`frame`]), not as JSON.
    #[serde(rename = "approval")]
    Approval {
        session_id: String,
        approved: bool,
        /// Why the request was denied (user said no, prompt timed out, no TTY).
        reason: Option<String>,
    },
    #[serde(rename = "error")]
    Error { session_id: String, message: String },
    #[serde(rename = "pong")]
    Pong { session_id: String },
}

impl ClientMessage {
    pub fn session_id(&self) -> &str {
        match self {
            ClientMessage::Request { session_id, .. } => session_id,
            ClientMessage::Ping { session_id } => session_id,
        }
    }
}

impl ServerMessage {
    pub fn session_id(&self) -> &str {
        match self {
            ServerMessage::Response { session_id, .. } => session_id,
            ServerMessage::Approval { session_id, .. } => session_id,
            ServerMessage::Error { session_id, .. } => session_id,
            ServerMessage::Pong { session_id } => session_id,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Invalid message format")]
    InvalidFormat,
    #[error("Session mismatch: expected {expected}, got {actual}")]
    SessionMismatch { expected: String, actual: String },
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("BLAKE3 verification failed: expected {expected}, got {actual}")]
    Blake3Mismatch { expected: String, actual: String },
    #[error("size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("binary frame of {len} bytes is over the {max}-byte limit")]
    FrameTooLarge { len: u32, max: u32 },
    #[error(
        "protocol version mismatch: this side speaks {ours}, the other side speaks {theirs}; \
         install the same Beam version on both ends"
    )]
    VersionMismatch { ours: String, theirs: String },
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

/// 1.1: adds the `approval` message between `response` and the binary stream.
/// 2.0: the binary is sent as raw length-prefixed frames instead of JSON
/// `binary_chunk` messages. Not compatible with 1.x.
pub const PROTOCOL_VERSION: &str = "2.0";
pub const MAGIC_BYTES: &[u8] = b"BEAM\0";

/// Message for a peer whose handshake named another protocol version.
pub fn version_mismatch_message(ours: &str, theirs: &str) -> String {
    ProtocolError::VersionMismatch {
        ours: ours.to_string(),
        theirs: theirs.to_string(),
    }
    .to_string()
}

pub mod frame;

/// Write the handshake: magic bytes, then our protocol version (u8 length + bytes).
pub async fn write_handshake<W: tokio::io::AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    w.write_all(MAGIC_BYTES).await?;
    let version = PROTOCOL_VERSION.as_bytes();
    w.write_all(&[version.len() as u8]).await?;
    w.write_all(version).await?;
    w.flush().await?;
    Ok(())
}

/// Read a handshake and return the peer's protocol version. Fails on wrong
/// magic bytes; the caller compares the version with [`PROTOCOL_VERSION`].
pub async fn read_handshake<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut magic = [0u8; 5];
    r.read_exact(&mut magic).await?;
    if magic != MAGIC_BYTES {
        return Err(ProtocolError::InvalidFormat);
    }
    let mut len = [0u8; 1];
    r.read_exact(&mut len).await?;
    let mut version = vec![0u8; len[0] as usize];
    r.read_exact(&mut version).await?;
    String::from_utf8(version).map_err(|_| ProtocolError::InvalidFormat)
}

pub fn compute_blake3(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

pub fn compute_blake3_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_serialization() {
        let msg = ClientMessage::Request {
            session_id: "test123".to_string(),
            command: "foo".to_string(),
            args: vec!["bar".to_string()],
            arch: "x86_64".to_string(),
            os: "linux".to_string(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ClientMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.session_id(), "test123");
    }

    #[test]
    fn test_blake3_hash() {
        let data = b"hello world";
        let hash = compute_blake3(data);
        assert_eq!(hash.len(), 64); // BLAKE3 produces 32 bytes = 64 hex chars
    }

    #[tokio::test]
    async fn test_blake3_file() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"test content").unwrap();

        let hash = compute_blake3_file(file.path()).unwrap();
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn test_server_message_session_id() {
        let msg = ServerMessage::Response {
            session_id: "abc".to_string(),
            exists: true,
            compatible: true,
            size: Some(100),
            blake3_hash: Some("hash".to_string()),
            error: None,
        };
        assert_eq!(msg.session_id(), "abc");
    }

    #[test]
    fn test_version_is_2_and_mismatch_message_names_both() {
        assert_eq!(PROTOCOL_VERSION, "2.0");
        let m = version_mismatch_message("2.0", "1.1");
        assert!(m.contains("2.0") && m.contains("1.1"), "{m}");
        assert!(m.contains("same Beam version"), "{m}");
    }

    #[test]
    fn test_binary_chunk_is_gone() {
        let old =
            r#"{"type":"binary_chunk","session_id":"s","offset":0,"data":[1,2],"is_final":true}"#;
        assert!(serde_json::from_str::<ServerMessage>(old).is_err());
    }

    #[tokio::test]
    async fn test_handshake_roundtrip() {
        let mut wire = Vec::new();
        write_handshake(&mut wire).await.unwrap();
        assert!(wire.starts_with(MAGIC_BYTES));
        assert_eq!(read_handshake(&mut wire.as_slice()).await.unwrap(), "2.0");
        let mut bad = wire.clone();
        bad[0] = b'X';
        assert!(read_handshake(&mut bad.as_slice()).await.is_err());
    }

    #[test]
    fn test_approval_roundtrip() {
        let msg = ServerMessage::Approval {
            session_id: "s".to_string(),
            approved: false,
            reason: Some("denied by user".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"approval\""));
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Approval {
                approved, reason, ..
            } => {
                assert!(!approved);
                assert_eq!(reason.as_deref(), Some("denied by user"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
