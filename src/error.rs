use std::fmt;
use std::time::Duration;

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

/// Run `f`, turning a panic into an inconsistency error. Bounds are checked
/// throughout the parser, but the file is live: a structure rewritten by
/// Outlook in the middle of a read must never crash the process.
pub fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "internal error".into());
        Err(Error::Corrupt(msg))
    })
}

/// Retry an operation when it reads inconsistent data, which happens when
/// Outlook rewrites part of the file during the read.
pub fn with_retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 0;
    loop {
        match guarded(&mut f) {
            Err(Error::Corrupt(_)) if attempt < 5 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50 * attempt));
            }
            r => return r,
        }
    }
}
