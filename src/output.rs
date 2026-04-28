use std::fmt;
use std::io::{self, Write};

pub struct Output<W: Write> {
    writer: io::BufWriter<W>,
}

impl<W: Write> Output<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: io::BufWriter::new(writer),
        }
    }

    pub fn line(&mut self, args: fmt::Arguments<'_>) -> Result<(), String> {
        self.writer
            .write_fmt(args)
            .and_then(|_| self.writer.write_all(b"\n"))
            .map_err(|e| format!("Write output: {}", e))
    }

    pub fn blank_line(&mut self) -> Result<(), String> {
        self.writer
            .write_all(b"\n")
            .map_err(|e| format!("Write output: {}", e))
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.writer
            .flush()
            .map_err(|e| format!("Flush output: {}", e))
    }
}

pub fn stdout_line(args: fmt::Arguments<'_>) -> Result<(), String> {
    let stdout = io::stdout();
    let mut out = Output::new(stdout.lock());
    out.line(args)?;
    out.flush()
}
