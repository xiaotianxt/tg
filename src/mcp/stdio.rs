//! stdio transport: newline-delimited JSON-RPC on stdin/stdout. stdout carries
//! protocol messages only; logs go to stderr.

use std::io::{self, BufRead, Write};

use super::protocol;
use super::tools::ToolContext;

pub(crate) fn serve(ctx: &ToolContext) -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("Read stdin: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = protocol::handle_bytes(ctx, line.as_bytes()).response {
            serde_json::to_writer(&mut stdout, &response)
                .map_err(|error| format!("Write response: {error}"))?;
            stdout
                .write_all(b"\n")
                .and_then(|()| stdout.flush())
                .map_err(|error| format!("Write response: {error}"))?;
        }
    }
    Ok(())
}
