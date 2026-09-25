use std::io;

use crate::System;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid tuning parameters: {0}")]
    InvalidParams(&'static str),
    #[error("the tuner cannot receive {0:?}")]
    Unsupported(System),
    #[error("the tuner could not lock on the signal")]
    NoLock,
    #[error("no such device or tuner: {0}")]
    NotFound(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
