use beam_common::{
    ClientMessage, MAGIC_BYTES, PROTOCOL_VERSION, ServerMessage, compute_blake3,
    compute_blake3_file, frame, read_handshake, write_handshake,
};
use std::io::Write;
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;

#[tokio::test]
async fn test_client_server_handshake() {
    // Start a mock server
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let session_id = "test-session-123".to_string();
    let session_id_for_server = session_id.clone();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();

        // Read magic bytes
        let mut magic = [0u8; 5];
        std::io::Read::read_exact(&mut stream, &mut magic).unwrap();
        assert_eq!(&magic, MAGIC_BYTES);

        // Read protocol version
        let mut version_len = [0u8; 1];
        std::io::Read::read_exact(&mut stream, &mut version_len).unwrap();
        let mut version_buf = vec![0u8; version_len[0] as usize];
        std::io::Read::read_exact(&mut stream, &mut version_buf).unwrap();
        let version = String::from_utf8(version_buf).unwrap();
        assert_eq!(version, PROTOCOL_VERSION);

        // Read request
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut line).unwrap();

        let msg: ClientMessage = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(msg.session_id(), session_id_for_server);

        // Send response
        let response = ServerMessage::Response {
            session_id: session_id_for_server.clone(),
            exists: true,
            compatible: true,
            size: Some(100),
            blake3_hash: Some("abc123".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        std::io::Write::write_all(&mut reader.get_mut(), json.as_bytes()).unwrap();
        std::io::Write::write_all(&mut reader.get_mut(), b"\n").unwrap();
        std::io::Write::flush(&mut reader.get_mut()).unwrap();
    });

    // Client connects
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Send handshake
    stream.write_all(MAGIC_BYTES).await.unwrap();
    let version_bytes = PROTOCOL_VERSION.as_bytes();
    stream
        .write_all(&[version_bytes.len() as u8])
        .await
        .unwrap();
    stream.write_all(version_bytes).await.unwrap();
    stream.flush().await.unwrap();

    // Send request
    let msg = ClientMessage::Request {
        session_id: session_id.clone(),
        command: "test".to_string(),
        args: vec![],
        arch: "x86_64".to_string(),
        os: "linux".to_string(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    stream.write_all(json.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    stream.flush().await.unwrap();

    // Read response
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    let response: ServerMessage = serde_json::from_str(line.trim()).unwrap();
    match response {
        ServerMessage::Response {
            exists,
            compatible,
            size,
            blake3_hash,
            ..
        } => {
            assert!(exists);
            assert!(compatible);
            assert_eq!(size, Some(100));
            assert_eq!(blake3_hash, Some("abc123".to_string()));
        }
        _ => panic!("Expected Response"),
    }

    server_handle.join().unwrap();
}

/// What the handler side saw on the wire after its request.
struct Transfer {
    received: Vec<u8>,
    /// Every byte beam-client sent: response, approval and the frames.
    wire_bytes: u64,
    /// Just the binary part (frames plus end marker).
    binary_wire_bytes: u64,
}

/// Counts bytes read through it.
struct Counting<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Counting<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        r
    }
}

/// Run one approved transfer over a real TCP connection: a mock beam-client
/// serves `binary`, and the handler side receives and verifies it with the
/// same code the real handler uses.
async fn approved_transfer(binary: &[u8]) -> Transfer {
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(binary).unwrap();
    temp_file.flush().unwrap();
    let binary_path = temp_file.path().to_path_buf();
    let expected_hash = compute_blake3_file(&binary_path).unwrap();
    let expected_size = binary.len() as u64;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session_id = "test-binary-stream".to_string();

    // Mock beam-client.
    let sid = session_id.clone();
    let hash = expected_hash.clone();
    let path = binary_path.clone();
    let client = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(read_handshake(&mut stream).await.unwrap(), PROTOCOL_VERSION);
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let msg: ClientMessage = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(msg.session_id(), sid);

        for msg in [
            ServerMessage::Response {
                session_id: sid.clone(),
                exists: true,
                compatible: true,
                size: Some(expected_size),
                blake3_hash: Some(hash.clone()),
                error: None,
            },
            ServerMessage::Approval {
                session_id: sid.clone(),
                approved: true,
                reason: None,
            },
        ] {
            let json = serde_json::to_string(&msg).unwrap();
            wr.write_all(json.as_bytes()).await.unwrap();
            wr.write_all(b"\n").await.unwrap();
        }
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let (sent_hash, sent) = frame::send_binary(&mut file, &mut wr).await.unwrap();
        assert_eq!(sent_hash, hash);
        assert_eq!(sent, expected_size);
        wr.shutdown().await.unwrap();
    });

    // Handler side.
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let count = Arc::new(AtomicU64::new(0));
    let mut reader = BufReader::new(Counting {
        inner: rd,
        count: count.clone(),
    });
    write_handshake(&mut wr).await.unwrap();
    let msg = ClientMessage::Request {
        session_id: session_id.clone(),
        command: "test".to_string(),
        args: vec![],
        arch: "x86_64".to_string(),
        os: "linux".to_string(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    wr.write_all(json.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();
    wr.flush().await.unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let (size, hash) = match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
        ServerMessage::Response {
            size, blake3_hash, ..
        } => (size.unwrap(), blake3_hash.unwrap()),
        other => panic!("Expected Response, got {other:?}"),
    };
    assert_eq!(hash, expected_hash);
    assert_eq!(size, expected_size);

    // Approval arrives before any binary frame.
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
        ServerMessage::Approval { approved, .. } => assert!(approved),
        other => panic!("Expected Approval, got {other:?}"),
    }
    // Bytes of the two JSON lines (BufReader may have read ahead, so count
    // them from the lines themselves).
    let control_bytes = {
        let resp = serde_json::to_string(&ServerMessage::Response {
            session_id: session_id.clone(),
            exists: true,
            compatible: true,
            size: Some(expected_size),
            blake3_hash: Some(expected_hash.clone()),
            error: None,
        })
        .unwrap();
        (resp.len() + 1 + line.len()) as u64
    };

    let mut received = Vec::new();
    let n = frame::receive_binary(
        &mut reader,
        &mut received,
        size,
        &hash,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(n, expected_size);
    client.await.unwrap();

    // Anything after the end marker would be a protocol error.
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());

    let wire_bytes = count.load(Ordering::Relaxed);
    Transfer {
        received,
        wire_bytes,
        binary_wire_bytes: wire_bytes - control_bytes,
    }
}

fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            // xorshift64
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

#[tokio::test]
async fn test_binary_streaming() {
    let binary = b"fake binary content".to_vec();
    let t = approved_transfer(&binary).await;
    assert_eq!(t.received, binary);
    assert_eq!(compute_blake3(&t.received), compute_blake3(&binary));
    // One frame plus the end marker.
    assert_eq!(
        t.binary_wire_bytes,
        binary.len() as u64 + 2 * frame::FRAME_HEADER_LEN as u64
    );
}

#[tokio::test]
async fn test_multi_mb_binary_is_about_1x_on_the_wire() {
    // 6 MiB + a bit, so the last frame is partial.
    let binary = pseudo_random(6 * 1024 * 1024 + 4321, 0x9e37_79b9_7f4a_7c15);
    let t = approved_transfer(&binary).await;
    assert_eq!(t.received.len(), binary.len());
    assert!(t.received == binary, "received bytes differ");

    let frames = binary.len().div_ceil(frame::MAX_FRAME_SIZE as usize) as u64;
    let framing = (frames + 1) * frame::FRAME_HEADER_LEN as u64;
    assert_eq!(t.binary_wire_bytes, binary.len() as u64 + framing);

    // Everything on the wire, JSON control lines included.
    let ratio = t.wire_bytes as f64 / binary.len() as f64;
    println!(
        "file {} bytes, wire {} bytes (binary part {}), ratio {ratio:.5}",
        binary.len(),
        t.wire_bytes,
        t.binary_wire_bytes
    );
    assert!(ratio < 1.001, "wire/file ratio {ratio}");

    // The 1.x encoding (a JSON array of numbers per 64 KiB chunk) for the
    // same file, for comparison: several times the size.
    let old: usize = binary
        .chunks(64 * 1024)
        .map(|c| serde_json::to_string(c).unwrap().len())
        .sum();
    let old_ratio = old as f64 / binary.len() as f64;
    println!("1.x JSON arrays would be about {old} bytes, ratio {old_ratio:.2}");
    assert!(old_ratio > 3.0);
}
