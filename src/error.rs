use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(String),
    /// The file is not a supported PST/OST.
    Format(String),
    /// Structural inconsistency. With a live OST this is usually a read racing
    /// a write by Outlook, so callers retry on it.
    Corrupt(String),
    NotFound(String),
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(m) => write!(f, "I/O error: {m}"),
            Error::Format(m) => write!(f, "unsupported file: {m}"),
            Error::Corrupt(m) => write!(f, "inconsistent data: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Usage(m) => write!(f, "{m}"),
        }
    }
}

impl Error {
    /// Process exit code reported to the calling agent.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            Error::NotFound(_) => 3,
            Error::Io(_) => 4,
            Error::Format(_) => 5,
            Error::Corrupt(_) => 6,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Error::Usage(_) => "usage",
            Error::NotFound(_) => "not_found",
            Error::Io(_) => "io",
            Error::Format(_) => "format",
            Error::Corrupt(_) => "inconsistent",
        }
    }
}
