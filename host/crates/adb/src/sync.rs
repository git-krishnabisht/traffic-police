//! The adb sync protocol for pushing files to a device (ARCHITECTURE.md §5.5).
//!
//! Each file is sent as SEND + DATA chunks + DONE over an open `sync:` service connection.
//! Chunks are at most 64 KiB.

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::{AdbError, Result};

const CHUNK: usize = 64 * 1024;

/// Pushes one file over an open sync connection.
pub(crate) async fn push_file(s: &mut TcpStream, data: &[u8], remote_path: &str, mode: u32, mtime: u32) -> Result<()> {
    let header = format!("{remote_path},{mode}");
    send_cmd(s, b"SEND", header.as_bytes()).await?;

    for chunk in data.chunks(CHUNK) {
        send_cmd(s, b"DATA", chunk).await?;
    }

    s.write_all(b"DONE").await?;
    s.write_all(&mtime.to_le_bytes()).await?;

    read_status(s).await
}

async fn send_cmd(s: &mut TcpStream, id: &[u8; 4], payload: &[u8]) -> Result<()> {
    s.write_all(id).await?;
    s.write_all(&(payload.len() as u32).to_le_bytes()).await?;
    if !payload.is_empty() {
        s.write_all(payload).await?;
    }
    Ok(())
}

async fn read_status(s: &mut TcpStream) -> Result<()> {
    let mut id = [0u8; 4];
    crate::read_exact(s, &mut id).await?;
    let mut len_buf = [0u8; 4];
    crate::read_exact(s, &mut len_buf).await?;
    let n = u32::from_le_bytes(len_buf) as usize;

    match &id {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let mut msg = vec![0u8; n];
            crate::read_exact(s, &mut msg).await?;
            Err(AdbError::Fail(String::from_utf8_lossy(&msg).into_owned()))
        }
        other => Err(AdbError::Protocol(format!("unexpected sync response {:?}", String::from_utf8_lossy(other)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn push_sends_correct_protocol() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let data = vec![0xABu8; 70_000]; // > 64 KiB to test chunking

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();

            // SEND
            let mut id = [0u8; 4];
            s.read_exact(&mut id).await.unwrap();
            assert_eq!(&id, b"SEND");
            let mut len = [0u8; 4];
            s.read_exact(&mut len).await.unwrap();
            let n = u32::from_le_bytes(len) as usize;
            let mut header = vec![0u8; n];
            s.read_exact(&mut header).await.unwrap();
            // 0o100755 = 33261 decimal (regular file + rwxr-xr-x)
            assert_eq!(String::from_utf8(header).unwrap(), "/tmp/test.bin,33261");

            // DATA chunks
            loop {
                s.read_exact(&mut id).await.unwrap();
                s.read_exact(&mut len).await.unwrap();
                let n = u32::from_le_bytes(len) as usize;
                if &id == b"DONE" {
                    // n is actually the mtime here
                    break;
                }
                assert_eq!(&id, b"DATA");
                let mut chunk = vec![0u8; n];
                s.read_exact(&mut chunk).await.unwrap();
                received.extend_from_slice(&chunk);
            }

            assert_eq!(received.len(), 70_000);

            // OKAY
            s.write_all(b"OKAY\0\0\0\0").await.unwrap();
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        push_file(&mut stream, &data, "/tmp/test.bin", 0o100755, 1000).await.unwrap();
        server.await.unwrap();
    }
}
