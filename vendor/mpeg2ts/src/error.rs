/// Possible error kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum ErrorKind {
    InvalidInput,
    Unsupported,
    Other,
}

/// Error type.
///
/// This struct captures the error kind, reason, source location, and backtrace
/// to provide comprehensive error diagnostics.
pub struct Error {
    /// The kind of error that occurred.
    pub kind: ErrorKind,

    /// The reason why the error occurred.
    pub reason: String,

    /// The source code location where the error was created.
    pub location: &'static std::panic::Location<'static>,

    /// A backtrace showing the call stack at the point of error.
    ///
    /// The backtrace is only captured if the `RUST_BACKTRACE` environment variable is set.
    pub backtrace: std::backtrace::Backtrace,

    /// The underlying IO error, if this error originated from an IO operation.
    pub io_error: Option<std::io::Error>,
}

impl Error {
    #[track_caller]
    fn new<T: Into<String>>(kind: ErrorKind, reason: T) -> Self {
        Self {
            kind,
            reason: reason.into(),
            location: std::panic::Location::caller(),
            backtrace: std::backtrace::Backtrace::capture(),
            io_error: None,
        }
    }

    #[track_caller]
    pub(crate) fn invalid_input<T: Into<String>>(reason: T) -> Self {
        Self::new(ErrorKind::InvalidInput, reason)
    }

    #[track_caller]
    pub(crate) fn unsupported<T: Into<String>>(reason: T) -> Self {
        Self::new(ErrorKind::Unsupported, reason)
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.reason)?;
        write!(f, " (at {}:{})", self.location.file(), self.location.line())?;
        if self.backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            write!(f, "\n\nBacktrace:\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.io_error.as_ref().map(|e| e as &dyn std::error::Error)
    }
}

impl From<std::io::Error> for Error {
    #[track_caller]
    fn from(io_error: std::io::Error) -> Self {
        Self {
            kind: ErrorKind::Other,
            reason: io_error.to_string(),
            location: std::panic::Location::caller(),
            backtrace: std::backtrace::Backtrace::capture(),
            io_error: Some(io_error),
        }
    }
}
