use futures::prelude::*;
use std::io;

/// Reads a length-prefixed bincode-encoded message (big-endian u32 length header).
pub async fn read_length_prefixed<T, M>(io: &mut T, max_size: usize) -> io::Result<M>
where
    T: AsyncRead + Unpin + Send,
    M: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len} > {max_size}"),
        ));
    }
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes a length-prefixed bincode-encoded message (big-endian u32 length header).
///
/// Enforces `max_size` at the sender so oversized messages fail early
/// rather than wasting bandwidth only to be rejected by the receiver.
pub async fn write_length_prefixed<T, M>(io: &mut T, msg: &M, max_size: usize) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
    M: serde::Serialize,
{
    let data =
        bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if data.len() > max_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialized message too large: {} > {max_size}", data.len()),
        ));
    }
    let len_u32 = u32::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialized message length {} overflows u32", data.len()),
        )
    })?;
    io.write_all(&len_u32.to_be_bytes()).await?;
    io.write_all(&data).await?;
    io.flush().await
}
