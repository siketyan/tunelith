use std::io;

use crate::System;

/// An error of Tunelith.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O error, including those tunelithd reports.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The [`TuneParams`](crate::TuneParams) do not go together.
    #[error("invalid tuning parameters: {0}")]
    InvalidParams(&'static str),
    /// The tuner does not receive the system.
    #[error("the tuner cannot receive {0:?}")]
    Unsupported(System),
    /// The tuner found no signal to lock on.
    #[error("the tuner could not lock on the signal")]
    NoLock,
    /// No device or tuner has the id.
    #[error("no such device or tuner: {0}")]
    NotFound(String),
}

/// The result of Tunelith, failing with an [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Whether the tuner, or every tuner that could serve, is in use.
    pub fn is_busy(&self) -> bool {
        matches!(self, Error::Io(e) if e.kind() == io::ErrorKind::ResourceBusy)
    }
}
