use beam_common::{ClientMessage, ServerMessage, compute_blake3, compute_blake3_file, MAGIC_BYTES, PROTOCOL_VERSION};
use std::io::Write;
use std::net::TcpListener;
use std::thread;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    stream.write_all(&[version_bytes.len() as u8]).await.unwrap();
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
        ServerMessage::Response { exists, compatible, size, blake3_hash, .. } => {
            assert!(exists);
            assert!(compatible);
            assert_eq!(size, Some(100));
            assert_eq!(blake3_hash, Some("abc123".to_string()));
        }
        _ => panic!("Expected Response"),
    }
    
    server_handle.join().unwrap();
}

#[tokio::test]
async fn test_binary_streaming() {
    // Create a test binary
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(b"fake binary content").unwrap();
    temp_file.flush().unwrap();
    let binary_path = temp_file.path().to_path_buf();
    let expected_hash = compute_blake3_file(&binary_path).unwrap();
    let expected_size = std::fs::metadata(&binary_path).unwrap().len();
    
    // Start mock client (server side of the tunnel)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let session_id = "test-binary-stream".to_string();
    let session_id_for_server = session_id.clone();
    let expected_hash_for_server = expected_hash.clone();
    let binary_path_for_server = binary_path.clone();
    
    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        
        // Handshake
        let mut magic = [0u8; 5];
        std::io::Read::read_exact(&mut stream, &mut magic).unwrap();
        let mut version_len = [0u8; 1];
        std::io::Read::read_exact(&mut stream, &mut version_len).unwrap();
        let mut version_buf = vec![0u8; version_len[0] as usize];
        std::io::Read::read_exact(&mut stream, &mut version_buf).unwrap();
        
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
            size: Some(expected_size),
            blake3_hash: Some(expected_hash_for_server.clone()),
            error: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        std::io::Write::write_all(&mut reader.get_mut(), json.as_bytes()).unwrap();
        std::io::Write::write_all(&mut reader.get_mut(), b"\n").unwrap();
        std::io::Write::flush(&mut reader.get_mut()).unwrap();
        
        // Read approve
        line.clear();
        std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
        let _msg: ClientMessage = serde_json::from_str(line.trim()).unwrap();
        
        // Stream binary
        let mut file = std::fs::File::open(&binary_path_for_server).unwrap();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut offset = 0u64;
        let session_id_clone = session_id_for_server.clone();
        
        loop {
            let n = std::io::Read::read(&mut file, &mut buffer).unwrap();
            if n == 0 { break; }
            
            let chunk = &buffer[..n];
            let msg = ServerMessage::BinaryChunk {
                session_id: session_id_clone.clone(),
                offset,
                data: chunk.to_vec(),
                is_final: false,
            };
            let json = serde_json::to_string(&msg).unwrap();
            std::io::Write::write_all(&mut reader.get_mut(), json.as_bytes()).unwrap();
            std::io::Write::write_all(&mut reader.get_mut(), b"\n").unwrap();
            std::io::Write::flush(&mut reader.get_mut()).unwrap();
            
            offset += n as u64;
        }
        
        // Final chunk
        let msg = ServerMessage::BinaryChunk {
            session_id: session_id_for_server.clone(),
            offset,
            data: vec![],
            is_final: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        std::io::Write::write_all(&mut reader.get_mut(), json.as_bytes()).unwrap();
        std::io::Write::write_all(&mut reader.get_mut(), b"\n").unwrap();
        std::io::Write::flush(&mut reader.get_mut()).unwrap();
    });
    
    thread::sleep(std::time::Duration::from_millis(100));
    
    // Client (beam-server-handler) connects
    let mut stream = TcpStream::connect(addr).await.unwrap();
    
    // Handshake
    stream.write_all(MAGIC_BYTES).await.unwrap();
    let version_bytes = PROTOCOL_VERSION.as_bytes();
    stream.write_all(&[version_bytes.len() as u8]).await.unwrap();
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
    
    // Read response
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    
    // Write request through the reader's underlying stream
    reader.get_mut().write_all(json.as_bytes()).await.unwrap();
    reader.get_mut().write_all(b"\n").await.unwrap();
    reader.get_mut().flush().await.unwrap();
    
    reader.read_line(&mut line).await.unwrap();
    
    let response: ServerMessage = serde_json::from_str(line.trim()).unwrap();
    let (size, hash) = match response {
        ServerMessage::Response { size, blake3_hash, .. } => (size.unwrap(), blake3_hash.unwrap()),
        _ => panic!("Expected Response"),
    };
    
    assert_eq!(hash, expected_hash);
    assert_eq!(size, expected_size);
    
    // Send approve
    let msg = ClientMessage::Approve {
        session_id: session_id.clone(),
        approved: true,
    };
    let json = serde_json::to_string(&msg).unwrap();
    reader.get_mut().write_all(json.as_bytes()).await.unwrap();
    reader.get_mut().write_all(b"\n").await.unwrap();
    reader.get_mut().flush().await.unwrap();
    
    // Read binary chunks
    let mut received = Vec::new();
    let mut total_received = 0u64;
    
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let msg: ServerMessage = serde_json::from_str(line.trim()).unwrap();
        
        match msg {
            ServerMessage::BinaryChunk { offset, data, is_final, .. } => {
                assert_eq!(offset, total_received);
                total_received += data.len() as u64;
                received.extend(data);
                if is_final { break; }
            }
            _ => panic!("Expected BinaryChunk"),
        }
    }
    
    // Verify hash
    let actual_hash = compute_blake3(&received);
    assert_eq!(actual_hash, expected_hash);
    assert_eq!(total_received, expected_size);
    
    server_handle.join().unwrap();
}
