use std::{ffi::OsString, io, path::PathBuf};

#[cfg(feature = "zip")]
use zip::result::ZipError;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum BuilderFinishError {
    #[error("failed to create root directory {0}: {1}")]
    CreateRootDir(PathBuf, #[source] io::Error),
}

#[derive(Error, Debug)]
pub enum CollectFilesError {
    #[error("failed to read contents of dir {0}: {1}")]
    ReadDir(PathBuf, #[source] io::Error),
}

#[derive(Error, Debug)]
pub enum RotateError {
    #[error("failed to flush completed file {0:?}: {1}")]
    Flush(OsString, #[source] io::Error),

    #[error("failed to sync metadata for completed file {0:?}: {1}")]
    Sync(OsString, #[source] io::Error),

    #[error("failed to remove completed file {0:?}: {1}")]
    Remove(OsString, #[source] io::Error),
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
    Open(OsString, #[source] io::Error),

    #[error("failed to read input file {0:?} for compression: {1}")]
    Read(OsString, #[source] io::Error),

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

    #[error("failed to sync output file {0:?}: {1}")]
    Sync(OsString, #[source] io::Error),
}

#[derive(Error, Debug)]
pub enum CloseError {
    #[error("failed to flush current file {0:?}: {1}")]
    Flush(OsString, #[source] io::Error),

    #[error("failed to sync current file {0:?}: {1}")]
    Sync(OsString, #[source] io::Error),

    #[error("at least one compression thread failed: {0:?}")]
    Compression(Vec<CompressError>),
}
