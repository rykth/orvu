//! Splits an async byte stream into separate messages, called frames.
//!
//! Each frame is a `u32` big-endian length followed by that many bytes. The
//! protobuf codec also puts a length in front of each message, but that is a
//! separate thing. Here we frame whole messages that the caller hands us, and
//! the transport never looks inside them.

use crate::error::{Result, TransportError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame allowed, so a hostile peer cannot make us hold an
/// unlimited amount of data.
///
/// It is public and re-exported from the crate root so a sender can check a
/// message's size before handing it to a writer. `write_frame` returns an
/// error for an oversized frame. That error ends the writer, and the writer's
/// owner then closes the connection. So one message that is too big costs a
/// whole peer connection, along with whatever was queued for that peer.
/// Whether the link comes back is up to the caller. A caller that reconnects
/// has only wasted a connection, and one that does not has lost the link.
/// Readers and writers both use this one definition.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Writes one frame and flushes it. Refuses a frame larger than `MAX_FRAME`.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(TransportError::FrameTooLarge(bytes.len()));
    }
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame. Returns `Closed` when the peer shut its side cleanly
/// between two frames. A stream that ends partway through a frame is an I/O
/// error.
///
/// Memory is taken as the bytes arrive, not from the length the peer claims.
/// A peer that announces a large frame and then sends nothing costs nothing.
/// This function has no time limit of its own, so a caller that wants one has
/// to add it.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let cut_short = || TransportError::Io(std::io::ErrorKind::UnexpectedEof.into());

    let mut len_buf = [0u8; 4];
    let mut have = 0;
    while have < len_buf.len() {
        match r.read(&mut len_buf[have..]).await? {
            // The stream ended. That is a clean close only if no byte of a new
            // frame had arrived.
            0 if have == 0 => return Err(TransportError::Closed),
            0 => return Err(cut_short()),
            n => have += n,
        }
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(TransportError::FrameTooLarge(len));
    }
    // `read_to_end` grows the buffer as data comes in, and `take` stops it at
    // the end of this frame.
    let mut buf = Vec::new();
    (&mut *r).take(len as u64).read_to_end(&mut buf).await?;
    if buf.len() < len {
        return Err(cut_short());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_in_order() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            write_frame(&mut a, b"hello").await.unwrap();
            write_frame(&mut a, b"world").await.unwrap();
        });
        assert_eq!(read_frame(&mut b).await.unwrap(), b"hello");
        assert_eq!(read_frame(&mut b).await.unwrap(), b"world");
    }

    #[tokio::test]
    async fn clean_close_reports_closed() {
        let (a, mut b) = tokio::io::duplex(1024);
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(TransportError::Closed)));
    }

    // A stream that stops partway through a frame did not end cleanly, whether
    // it stops inside the length or inside the body.
    #[tokio::test]
    async fn a_stream_cut_mid_frame_is_an_error_not_a_clean_close() {
        use tokio::io::AsyncWriteExt;

        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&[0, 0]).await.unwrap();
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(TransportError::Io(_))), "cut inside the length");

        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&[0, 0, 0, 5, b'h', b'i']).await.unwrap();
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(TransportError::Io(_))), "cut inside the body");
    }

    #[tokio::test]
    async fn empty_frame_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, b"").await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), b"");
    }
}
