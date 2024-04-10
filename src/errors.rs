use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum NewError {
    #[error("failed to create root directory {0}: {1}")]
    CreateRootDir(PathBuf, #[source] io::Error),

    #[error(transparent)]
    CollectExistingFiles(#[from] CollectFilesError),

    #[error(transparent)]
    NewFile(#[from] NewFileError),
}

#[derive(Error, Debug)]
pub enum CollectFilesError {
    #[error("failed to read contents of dir {0}: {1}")]
    ReadDir(PathBuf, #[source] io::Error),

    #[error("failed to read dir entry: {0}")]
    ReadDirEntry(#[source] io::Error),

    #[error("failed to access metadata of filesystem entity at path {0}: {1}")]
    AccessMetadata(PathBuf, #[source] io::Error),
}

#[derive(Error, Debug)]
pub enum CutError {
    #[error(transparent)]
    Rotate(#[from] RotateError),

    #[error("at least one compression task failed: {0:?}")]
    Compression(Vec<CompressError>),
}

#[derive(Error, Debug)]
pub enum ExportError {
    #[error(transparent)]
    Rotate(#[from] RotateError),

    #[error("a compression task failed: {0}")]
    Compression(#[from] CompressError),

    #[error("failed to create output file {0:?} for export: {1}")]
    CreateOutput(PathBuf, #[source] io::Error),

    #[error("failed to read input file {0:?} for export: {1}")]
    OpenInput(PathBuf, #[source] io::Error),

    #[error("failed to copy data from input file {0:?} to output file {1:?} for export: {2}")]
    Copy(PathBuf, PathBuf, #[source] io::Error),

    #[error("failed to flush output file {0:?}: {1}")]
    Flush(PathBuf, #[source] io::Error),

    #[error("failed to sync output file {0:?}: {1}")]
    Sync(PathBuf, #[source] io::Error),
}

#[derive(Error, Debug)]
pub enum RotateError {
    #[error("failed to flush completed file {0:?}: {1}")]
    Flush(PathBuf, #[source] io::Error),

    #[error("failed to sync completed file {0:?}: {1}")]
    Sync(PathBuf, #[source] io::Error),

    #[error("failed to remove completed file {0:?}: {1}")]
    Remove(PathBuf, #[source] io::Error),

    #[error(transparent)]
    NewFile(#[from] NewFileError),
}

impl From<RotateError> for io::Error {
    fn from(err: RotateError) -> Self {
        match err {
            RotateError::Flush(_, err) => err,
            RotateError::Sync(_, err) => err,
            RotateError::Remove(_, err) => err,
            RotateError::NewFile(err) => err.into(),
        }
    }
}

#[derive(Error, Debug)]
pub enum NewFileError {
    #[error("failed to open new output file {0:?}: {1}")]
    Open(PathBuf, #[source] io::Error),
}

impl From<NewFileError> for io::Error {
    fn from(err: NewFileError) -> Self {
        match err {
            NewFileError::Open(_, err) => err,
        }
    }
}

#[derive(Error, Debug)]
pub enum CompressError {
    #[error("failed to open output file {0:?} for compression: {1}")]
    OpenOutput(PathBuf, #[source] io::Error),

    #[error("failed to open input file {0:?} for compression: {1}")]
    OpenInput(PathBuf, #[source] io::Error),

    #[error("failed to write compressed output to file {0:?}: {1}")]
    Write(PathBuf, #[source] io::Error),

    #[error("failed to flush gzip-compressed output file {0:?}: {1}")]
    FlushGZip(PathBuf, #[source] io::Error),

    #[error("failed to remove compression input file {0:?}: {1}")]
    Remove(PathBuf, #[source] io::Error),

    #[error("failed to sync file {0:?}: {1}")]
    SyncFile(PathBuf, #[source] io::Error),

    #[error("failed to sync directory {0:?}: {1}")]
    SyncDir(PathBuf, #[source] io::Error),
}

#[derive(Error, Debug)]
pub enum CloseError {
    #[error("failed to flush current file {0:?}: {1}")]
    Flush(PathBuf, #[source] io::Error),

    #[error("failed to sync file {0:?}: {1}")]
    SyncFile(PathBuf, #[source] io::Error),

    #[error("failed to sync directory {0:?}: {1}")]
    SyncDir(PathBuf, #[source] io::Error),

    #[error("at least one compression thread failed: {0:?}")]
    Compression(Vec<CompressError>),
}
