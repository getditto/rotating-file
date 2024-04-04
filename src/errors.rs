use std::{ffi::OsString, io, path::PathBuf};

#[cfg(feature = "zip")]
use zip::result::ZipError;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum BuilderFinishError {
    #[error("failed to create root directory {0}: {1}")]
    CreateRootDir(PathBuf, #[source] io::Error),
}

impl BuilderFinishError {
    pub fn io_cause(&self) -> Option<&io::Error> {
        match self {
            BuilderFinishError::CreateRootDir(_, cause) => Some(cause),
        }
    }
}

#[derive(Error, Debug)]
pub enum CollectFilesError {
    #[error("failed to read contents of dir {0}: {1}")]
    ReadDir(PathBuf, #[source] io::Error),
}

impl CollectFilesError {
    pub fn io_cause(&self) -> Option<&io::Error> {
        match self {
            CollectFilesError::ReadDir(_, cause) => Some(cause),
        }
    }
}

#[derive(Error, Debug)]
pub enum CutError {
    #[error(transparent)]
    Rotate(#[from] RotateError),

    #[error(transparent)]
    Compress(#[from] CompressError),
}

impl CutError {
    pub fn io_cause(&self) -> Option<&io::Error> {
        match self {
            CutError::Rotate(source) => source.io_cause(),
            CutError::Compress(source) => source.io_cause(),
        }
    }
}

#[derive(Error, Debug)]
pub enum RotateError {
    #[error("failed to flush completed file {0:?}: {1}")]
    Flush(OsString, #[source] io::Error),

    #[error("failed to sync completed file {0:?}: {1}")]
    Sync(OsString, #[source] io::Error),

    #[error("failed to remove completed file {0:?}: {1}")]
    Remove(OsString, #[source] io::Error),
}

impl RotateError {
    pub fn io_cause(&self) -> Option<&io::Error> {
        match self {
            RotateError::Flush(_, cause) => Some(cause),
            RotateError::Sync(_, cause) => Some(cause),
            RotateError::Remove(_, cause) => Some(cause),
        }
    }
}

impl From<RotateError> for io::Error {
    fn from(err: RotateError) -> Self {
        match err {
            RotateError::Flush(_, err) => err,
            RotateError::Sync(_, err) => err,
            RotateError::Remove(_, err) => err,
        }
    }
}

#[derive(Error, Debug)]
pub enum CompressError {
    #[error("failed to open output file {0:?} for compression: {1}")]
    OpenOutput(OsString, #[source] io::Error),

    #[error("failed to open input file {0:?} for compression: {1}")]
    OpenInput(OsString, #[source] io::Error),

    #[error("failed to write compressed output to file {0:?}: {1}")]
    Write(OsString, #[source] io::Error),

    #[error("failed to flush gzip-compressed output file {0:?}: {1}")]
    FlushGZip(OsString, #[source] io::Error),

    #[cfg(feature = "zip")]
    #[error("failed to compress input file {0:?} in zip format: {1}")]
    Zip(OsString, #[source] ZipError),

    #[cfg(feature = "zip")]
    #[error("failed to finish writing zip-compressed output file {0:?}: {1}")]
    FinishZip(OsString, #[source] ZipError),

    #[error("failed to remove compression input file {0:?}: {1}")]
    Remove(OsString, #[source] io::Error),

    #[error("failed to sync file {0:?}: {1}")]
    SyncFile(OsString, #[source] io::Error),

    #[error("failed to sync directory {0:?}: {1}")]
    SyncDir(OsString, #[source] io::Error),
}

impl CompressError {
    pub fn io_cause(&self) -> Option<&io::Error> {
        match self {
            CompressError::OpenOutput(_, cause) => Some(cause),
            CompressError::OpenInput(_, cause) => Some(cause),
            CompressError::Write(_, cause) => Some(cause),
            CompressError::FlushGZip(_, cause) => Some(cause),
            #[cfg(feature = "zip")]
            CompressError::Zip(_, _) => None,
            #[cfg(feature = "zip")]
            CompressError::FinishZip(_, _) => None,
            CompressError::Remove(_, cause) => Some(cause),
            CompressError::SyncFile(_, cause) => Some(cause),
            CompressError::SyncDir(_, cause) => Some(cause),
        }
    }
}

#[derive(Error, Debug)]
pub enum CloseError {
    #[error("failed to flush current file {0:?}: {1}")]
    Flush(OsString, #[source] io::Error),

    #[error("failed to sync file {0:?}: {1}")]
    SyncFile(OsString, #[source] io::Error),

    #[error("failed to sync directory {0:?}: {1}")]
    SyncDir(OsString, #[source] io::Error),

    #[error("at least one compression thread failed: {0:?}")]
    Compression(Vec<CompressError>),
}

impl CloseError {
    pub fn io_cause(&self) -> Option<&io::Error> {
        match self {
            CloseError::Flush(_, cause) => Some(cause),
            CloseError::SyncFile(_, cause) => Some(cause),
            CloseError::SyncDir(_, cause) => Some(cause),
            CloseError::Compression(errors) => errors.first().and_then(|err| err.io_cause()),
        }
    }
}
