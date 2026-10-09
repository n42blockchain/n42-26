//! Streaming Keccak-256 helper for the offline H2 receipt audit.
//! Reads raw bytes from stdin and writes exactly 32 raw digest bytes.
use std::io::{self, Read, Write};
use tiny_keccak::{Hasher, Keccak};

fn main() -> io::Result<()> {
    if std::env::args_os().len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "takes no arguments",
        ));
    }
    let mut hasher = Keccak::v256();
    let mut input = io::stdin().lock();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut result = [0; 32];
    hasher.finalize(&mut result);
    io::stdout().lock().write_all(&result)
}
