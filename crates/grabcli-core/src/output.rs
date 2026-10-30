//! Output context: human-readable lines vs. a single `--json` document, and a
//! uniform way to emit structured errors.

use std::io::{self, Write};

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct Output {
    pub format: Format,
}

impl Output {
    pub fn new(json: bool) -> Self {
        Self {
            format: if json { Format::Json } else { Format::Human },
        }
    }

    pub fn is_json(&self) -> bool {
        self.format == Format::Json
    }

    /// Print a value as pretty JSON (only in JSON mode).
    pub fn json(&self, value: &serde_json::Value) {
        if self.is_json()
            && let Ok(s) = serde_json::to_string_pretty(value)
        {
            println!("{s}");
        }
    }

    /// Print a human line (only in human mode).
    pub fn line(&self, text: impl AsRef<str>) {
        if !self.is_json() {
            println!("{}", text.as_ref());
        }
    }

    /// A warning on stderr (both modes, so it never corrupts `--json` stdout).
    pub fn warn(&self, text: impl AsRef<str>) {
        let _ = writeln!(io::stderr(), "{}", text.as_ref());
    }

    /// Emit an error to stderr in the active format and return its exit code.
    pub fn emit_error(&self, err: &Error) -> i32 {
        match self.format {
            Format::Json => {
                if let Ok(s) = serde_json::to_string_pretty(&err.to_json()) {
                    let _ = writeln!(io::stderr(), "{s}");
                }
            }
            Format::Human => {
                let _ = writeln!(
                    io::stderr(),
                    "error ({}): {}",
                    err.kind.as_str(),
                    err.message
                );
            }
        }
        err.exit_code()
    }
}
