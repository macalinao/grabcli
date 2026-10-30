//! Errors inside the crate: rusqlite errors convert into core errors (kind
//! `io`, "cache database: …") at the crate boundary.

use grabcli_core::error::{Error, ErrorKind};

#[derive(Debug)]
pub struct DbError(pub Error);

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        Self(Error::new(ErrorKind::Io, format!("cache database: {e}")))
    }
}

impl From<Error> for DbError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        Self(e.into())
    }
}

impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        Self(e.into())
    }
}

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        e.0
    }
}

pub type Result<T> = std::result::Result<T, DbError>;
