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
    /// `exists && compatible`. Binary chunks follow only when `approved` is true.
    #[serde(rename = "approval")]
    Approval {
        session_id: String,
        approved: bool,
        /// Why the request was denied (user said no, prompt timed out, no TTY).
        reason: Option<String>,
    },
    #[serde(rename = "binary_chunk")]
    BinaryChunk {
        session_id: String,
        offset: u64,
        data: Vec<u8>,
        is_final: bool,
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
            ServerMessage::BinaryChunk { session_id, .. } => session_id,
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
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

/// 1.1: adds the `approval` message between `response` and the binary stream.
pub const PROTOCOL_VERSION: &str = "1.1";
pub const MAGIC_BYTES: &[u8] = b"BEAM\0";

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
