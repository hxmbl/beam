//! Raw binary frames, used for the file after an approved `approval`.
//!
//! Each frame is a `u32` big-endian length followed by that many bytes. A
//! zero-length frame ends the stream. Frames are at most [`MAX_FRAME_SIZE`]
//! bytes; a reader refuses a larger length before reading the body.

use crate::{ProtocolError, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame body either side sends or accepts (64 KiB).
pub const MAX_FRAME_SIZE: u32 = 64 * 1024;

/// Bytes of framing per frame (the length prefix).
pub const FRAME_HEADER_LEN: usize = 4;

/// Write one frame. `data` must be non-empty and at most [`MAX_FRAME_SIZE`].
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    if data.is_empty() || data.len() > MAX_FRAME_SIZE as usize {
        return Err(ProtocolError::InvalidFormat);
    }
    // One write per frame: header and body together.
    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await?;
    Ok(())
}

/// Write the end-of-stream marker (a zero-length frame) and flush.
pub async fn write_end<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    w.write_all(&0u32.to_be_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame into `buf` (resized to fit). Returns `Ok(None)` at the end
/// marker. A length over [`MAX_FRAME_SIZE`] is refused before its body is read.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<usize>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    read_exact_or_closed(r, &mut header).await?;
    let len = u32::from_be_bytes(header);
    if len == 0 {
        return Ok(None);
    }
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            len,
            max: MAX_FRAME_SIZE,
        });
    }
    buf.resize(len as usize, 0);
    read_exact_or_closed(r, buf).await?;
    Ok(Some(len as usize))
}

async fn read_exact_or_closed<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> Result<()> {
    match r.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(ProtocolError::ConnectionClosed)
        }
        Err(e) => Err(e.into()),
    }
}

/// Send everything `src` yields as frames, then the end marker. Returns the
/// BLAKE3 hash and size of what was sent, so the sender can tell if the file
/// changed since it was hashed.
pub async fn send_binary<R, W>(src: &mut R, w: &mut W) -> Result<(String, u64)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; MAX_FRAME_SIZE as usize];
    let mut hasher = blake3::Hasher::new();
    let mut sent = 0u64;
    loop {
        // Fill the frame as far as the source allows, so short reads do not
        // turn into many small frames.
        let mut filled = 0;
        while filled < buf.len() {
            let n = src.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        hasher.update(&buf[..filled]);
        write_frame(w, &buf[..filled]).await?;
        sent += filled as u64;
        if filled < buf.len() {
            break;
        }
    }
    write_end(w).await?;
    Ok((hasher.finalize().to_hex().to_string(), sent))
}

/// Receive a framed binary into `sink`, checking it against the size and
/// BLAKE3 hash the user approved. Nothing past `expected_size` is written:
/// a stream that runs over is refused at the frame that would overflow.
/// `frame_wait` bounds the wait for each frame.
pub async fn receive_binary<R, W>(
    r: &mut R,
    sink: &mut W,
    expected_size: u64,
    expected_hash: &str,
    frame_wait: Duration,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: std::io::Write,
{
    let mut hasher = blake3::Hasher::new();
    let mut buf = Vec::with_capacity(MAX_FRAME_SIZE as usize);
    let mut received = 0u64;
    loop {
        let frame = tokio::time::timeout(frame_wait, read_frame(r, &mut buf))
            .await
            .map_err(|_| ProtocolError::ConnectionClosed)??;
        let Some(n) = frame else { break };
        if received + n as u64 > expected_size {
            return Err(ProtocolError::SizeMismatch {
                expected: expected_size,
                actual: received + n as u64,
            });
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n])?;
        received += n as u64;
    }
    if received != expected_size {
        return Err(ProtocolError::SizeMismatch {
            expected: expected_size,
            actual: received,
        });
    }
    let actual = hasher.finalize().to_hex().to_string();
    if actual != expected_hash {
        return Err(ProtocolError::Blake3Mismatch {
            expected: expected_hash.to_string(),
            actual,
        });
    }
    sink.flush()?;
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_blake3;

    const WAIT: Duration = Duration::from_secs(5);

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    async fn encode(data: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut src = data;
        send_binary(&mut src, &mut wire).await.unwrap();
        wire
    }

    #[tokio::test]
    async fn roundtrip_and_overhead() {
        let data = pattern(3 * MAX_FRAME_SIZE as usize + 123);
        let wire = encode(&data).await;
        // 4 frames (3 full + 1 partial) plus the end marker.
        assert_eq!(wire.len(), data.len() + 5 * FRAME_HEADER_LEN);

        let mut out = Vec::new();
        let n = receive_binary(
            &mut wire.as_slice(),
            &mut out,
            data.len() as u64,
            &compute_blake3(&data),
            WAIT,
        )
        .await
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn exact_multiple_of_frame_size() {
        let data = pattern(2 * MAX_FRAME_SIZE as usize);
        let wire = encode(&data).await;
        assert_eq!(wire.len(), data.len() + 3 * FRAME_HEADER_LEN);
        let mut out = Vec::new();
        receive_binary(
            &mut wire.as_slice(),
            &mut out,
            data.len() as u64,
            &compute_blake3(&data),
            WAIT,
        )
        .await
        .unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn empty_binary_is_just_the_end_marker() {
        let wire = encode(&[]).await;
        assert_eq!(wire, 0u32.to_be_bytes());
        let mut out = Vec::new();
        receive_binary(
            &mut wire.as_slice(),
            &mut out,
            0,
            &compute_blake3(&[]),
            WAIT,
        )
        .await
        .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn oversize_frame_is_refused_before_its_body() {
        let mut wire = (MAX_FRAME_SIZE + 1).to_be_bytes().to_vec();
        // No body at all: the reader must fail on the header alone.
        let mut buf = Vec::new();
        match read_frame(&mut wire.as_slice(), &mut buf).await {
            Err(ProtocolError::FrameTooLarge { len, max }) => {
                assert_eq!(len, MAX_FRAME_SIZE + 1);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
        assert!(buf.is_empty());

        wire = u32::MAX.to_be_bytes().to_vec();
        let mut out = Vec::new();
        let r = receive_binary(&mut wire.as_slice(), &mut out, 10, "x", WAIT).await;
        assert!(matches!(r, Err(ProtocolError::FrameTooLarge { .. })));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn writer_refuses_oversize_and_empty_frames() {
        let mut wire = Vec::new();
        let big = vec![0u8; MAX_FRAME_SIZE as usize + 1];
        assert!(write_frame(&mut wire, &big).await.is_err());
        assert!(write_frame(&mut wire, &[]).await.is_err());
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn more_bytes_than_approved_are_not_written() {
        let data = pattern(1000);
        let wire = encode(&data).await;
        let mut out = Vec::new();
        let r = receive_binary(
            &mut wire.as_slice(),
            &mut out,
            999,
            &compute_blake3(&data),
            WAIT,
        )
        .await;
        assert!(matches!(
            r,
            Err(ProtocolError::SizeMismatch {
                expected: 999,
                actual: 1000
            })
        ));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn short_stream_is_a_size_mismatch() {
        let data = pattern(1000);
        let wire = encode(&data).await;
        let mut out = Vec::new();
        let r = receive_binary(
            &mut wire.as_slice(),
            &mut out,
            1001,
            &compute_blake3(&data),
            WAIT,
        )
        .await;
        assert!(matches!(r, Err(ProtocolError::SizeMismatch { .. })));
    }

    #[tokio::test]
    async fn same_size_different_bytes_is_a_hash_mismatch() {
        let approved = pattern(5000);
        let mut swapped = approved.clone();
        swapped[4000] ^= 0xff;
        let wire = encode(&swapped).await;
        let mut out = Vec::new();
        let r = receive_binary(
            &mut wire.as_slice(),
            &mut out,
            approved.len() as u64,
            &compute_blake3(&approved),
            WAIT,
        )
        .await;
        assert!(matches!(r, Err(ProtocolError::Blake3Mismatch { .. })));
    }

    #[tokio::test]
    async fn truncated_frame_or_missing_end_marker_is_connection_closed() {
        let data = pattern(100);
        let wire = encode(&data).await;
        // Cut inside the body, then just before the end marker.
        for cut in [50, wire.len() - FRAME_HEADER_LEN] {
            let mut out = Vec::new();
            let r = receive_binary(
                &mut &wire[..cut],
                &mut out,
                100,
                &compute_blake3(&data),
                WAIT,
            )
            .await;
            assert!(
                matches!(r, Err(ProtocolError::ConnectionClosed)),
                "cut {cut}: {r:?}"
            );
        }
    }
}
