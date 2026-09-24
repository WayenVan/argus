//! Frame format: `[len: u32 BE][type: u8][payload: len-1 bytes]`.
//!
//! `len` counts the type byte. A single frame is at most [`MAX_FRAME`] bytes.

use std::io::{self, Read, Write};

use serde::Serialize;

pub const MAX_FRAME: usize = 1 << 20;

/// Frame type bytes.
pub mod ty {
    /// JSON control message.
    pub const CONTROL: u8 = 0x01;
    /// Raw PTY bytes. Holder → output subscriber frames start with a u64 BE offset.
    pub const DATA: u8 = 0x10;
    /// client → holder: `rows: u16, cols: u16`.
    pub const RESIZE: u8 = 0x11;
    /// client → holder: empty.
    pub const DETACH: u8 = 0x12;
    /// holder → subscribers: `code: i32 BE` (signal exit is `128 + sig`).
    pub const EXIT: u8 = 0x13;
    /// holder → client: disconnected by `--steal`.
    pub const KICKED: u8 = 0x14;
    /// client → holder: terminal gained focus, claim size ownership.
    pub const FOCUS: u8 = 0x15;
    /// holder → subscriber: `from: u64 BE, to: u64 BE`, backlog dropped.
    pub const SKIPPED: u8 = 0x16;
}

#[derive(Debug)]
pub enum FrameError {
    Empty,
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Empty => write!(f, "frame has no type byte"),
            FrameError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<FrameError> for io::Error {
    fn from(e: FrameError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

pub fn encode(ty: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 1;
    debug_assert!(len <= MAX_FRAME);
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.push(ty);
    out.extend_from_slice(payload);
    out
}

pub fn encode_json<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = serde_json::to_vec(msg).expect("protocol messages always serialize");
    encode(ty::CONTROL, &body)
}

/// A decoded frame: type, payload, and bytes consumed from the buffer.
pub type Decoded<'a> = (u8, &'a [u8], usize);

/// Decodes one frame from the front of `buf`.
///
/// Returns `Ok(None)` when more bytes are needed, otherwise the frame type,
/// its payload and the number of bytes consumed.
pub fn try_decode(buf: &[u8]) -> Result<Option<Decoded<'_>>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    Ok(Some((buf[4], &buf[5..4 + len], 4 + len)))
}

/// Blocking read of one frame. `Ok(None)` means clean EOF between frames.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 4];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = check_len(u32::from_be_bytes(head) as usize)?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let ty = body.remove(0);
    Ok(Some((ty, body)))
}

pub fn write_frame<W: Write>(w: &mut W, ty: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&encode(ty, payload))?;
    w.flush()
}

pub fn write_json<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode_json(msg))?;
    w.flush()
}

fn check_len(len: usize) -> io::Result<usize> {
    if len == 0 {
        return Err(FrameError::Empty.into());
    }
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len).into());
    }
    Ok(len)
}

/// Async counterparts for the manager.
#[cfg(feature = "tokio")]
pub mod aio {
    use std::io;

    use serde::Serialize;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
        let mut head = [0u8; 4];
        match r.read_exact(&mut head).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = super::check_len(u32::from_be_bytes(head) as usize)?;
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        let ty = body.remove(0);
        Ok(Some((ty, body)))
    }

    pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
        w.write_all(&super::encode_json(msg)).await?;
        w.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let bytes = encode(ty::DATA, b"hello");
        let (t, payload, used) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!((t, payload, used), (ty::DATA, &b"hello"[..], bytes.len()));
        assert!(try_decode(&bytes[..bytes.len() - 1]).unwrap().is_none());

        let mut cursor = io::Cursor::new(bytes);
        assert_eq!(read_frame(&mut cursor).unwrap(), Some((ty::DATA, b"hello".to_vec())));
        assert_eq!(read_frame(&mut cursor).unwrap(), None);
    }

    #[test]
    fn rejects_oversized() {
        let head = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(try_decode(&head).is_err());
    }
}
