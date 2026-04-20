//! Unix-socket client. Open, write one JSON line, read one JSON line.
//! For log streaming, the read side is a loop until EOF or error.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use orchestrator::ipc::proto::{Command, Response};

/// One-shot command: send, read one response, return.
pub fn send(socket_path: &Path, cmd: Command) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket_path)?;
    let json = serde_json::to_string(&cmd).unwrap();
    writeln!(stream, "{}", json)?;

    let reader = BufReader::new(stream);
    let mut line = String::new();
    let mut reader = reader;
    reader.read_line(&mut line)?;

    serde_json::from_str(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Streaming: send command, call callback for each response line
/// until EOF or error. Used for `logs --follow`.
pub fn stream(
    socket_path: &Path,
    cmd: Command,
    mut on_response: impl FnMut(Response),
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    let json = serde_json::to_string(&cmd).unwrap();
    writeln!(stream, "{}", json)?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() { continue; }
        match serde_json::from_str::<Response>(&line) {
            Ok(resp) => on_response(resp),
            Err(_) => break,
        }
    }
    Ok(())
}
