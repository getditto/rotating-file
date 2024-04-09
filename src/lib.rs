//! A thread-safe rotating file with customizable rotation behavior.
//!
//! ## Example
//!
//! ```
//! use rotating_file::RotatingFile;
//! use std::io::Write;
//!
//! let root_dir = "./target/tmp";
//! let s = "The quick brown fox jumps over the lazy dog";
//! let _ = std::fs::remove_dir_all(root_dir);
//!
//! // rotated by 1 kilobyte, compressed with gzip
//! let mut rotating_file = RotatingFile::build(root_dir.into()).size(1).finish().unwrap();
//! for _ in 0..24 {
//!     writeln!(rotating_file, "{}", s).unwrap();
//! }
//! rotating_file.close();
//!
//! assert_eq!(2, std::fs::read_dir(root_dir).unwrap().count());
//! std::fs::remove_dir_all(root_dir).unwrap();
//! ```

#![allow(unreachable_code)]

use std::sync::Mutex;
use std::thread;
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File},
    io::{self, Write},
    num::NonZeroUsize,
    thread::ThreadId,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use std::{io::BufWriter, sync::Arc};
use std::{thread::JoinHandle, time::Duration};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use chrono::TimeZone;
use chrono::{DateTime, TimeDelta, Utc};
use flate2::{write::GzEncoder, Compression};
use tap::{Conv, TapFallible};
use tracing::{debug, error, trace, warn};

use crate::errors::{
    CloseError, CollectFilesError, CompressError, CutError, ExportError, NewError, NewFileError,
    RotateError,
};

/// The suffix that will be present in any filename for a file that has been compressed.
const COMPRESSED_SUFFIX: &'static str = "gz";

/// The date format used when writing timestamps into the names of log files with
/// [chrono](https://docs.rs/chrono/latest/chrono/format/strftime/).
const DATE_FORMAT: &'static str = "%Y-%m-%d-%H-%M-%S%.6f";

pub mod errors;

pub struct RotatingFile {
    storage: Storage,
    limits: Limits,
    current: Current,
    state: Arc<Mutex<State>>,
    work: Arc<Mutex<Work>>,
}

impl RotatingFile {
    pub fn try_init(storage: Storage, limits: Limits) -> Result<Self, NewError> {
        let output_file = Self::open_new_file(&storage.root_dir, &storage.prefix)?;

        let mut selph = Self {
            storage,
            limits,
            current: output_file,
            state: Arc::new(Mutex::new(State::default())),
            work: Arc::new(Mutex::new(Work::default())),
        };

        selph.collect_existing_files()?;

        Ok(selph)
    }

    fn collect_existing_files(&mut self) -> Result<(), CollectFilesError> {
        fs::read_dir(&self.storage.root_dir)
            .map_err(|err| CollectFilesError::ReadDir(self.storage.root_dir.to_path_buf(), err))?
            .flatten()
            .for_each(|entry| {
                // If we find a file that is uncompressed, we need to compress the file. Otherwise,
                // we just record the path of the file as-is.
                let path = entry.path();
                if path.extension() != Some(&COMPRESSED_SUFFIX.to_owned().conv::<OsString>()) {
                    let state = self.state.clone();
                    let work = self.work.clone();

                    self.work
                        .lock()
                        .unwrap()
                        .enqueue_compression(move |work_id| {
                            let result = Self::compress(path, state);
                            work.lock()
                                .unwrap()
                                .handle_event(WorkEvent::CompressionFinished(work_id));
                            result
                        });
                } else {
                    self.state
                        .lock()
                        .unwrap()
                        .handle_event(StateEvent::FoundFile {
                            path: path.into_os_string(),
                        });
                }
            });
        Ok(())
    }

    pub fn export<P: AsRef<Path>>(&self, dest: P) -> Result<u64, ExportError> {
        todo!()
    }

    fn compress(in_path: PathBuf, state: Arc<Mutex<State>>) -> Result<(), CompressError> {
        debug!(path = ?in_path, reason = %"found", "compressing log file");

        let mut out_path = in_path.clone();
        out_path.push(".");
        out_path.push(COMPRESSED_SUFFIX);

        let mut in_file = File::open(&in_path).map_err(|error| {
            warn!(path = ?in_path, %error, "failed to open input file for compression");
            CompressError::OpenInput(in_path.clone().into_os_string(), error)
        })?;

        let mut out_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .map_err(|error| {
                error!(path = ?out_path, %error, "failed to open output file for compression");
                CompressError::OpenOutput(out_path.clone().into_os_string(), error)
            })?;

        let mut encoder = GzEncoder::new(&mut out_file, Compression::default());
        io::copy(&mut in_file, &mut encoder).map_err(|error| {
            error!(path = ?out_path, %error, "failed to write compressed output to file");
            CompressError::Write(out_path.clone().into_os_string(), error)
        })?;
        encoder.flush().map_err(|error| {
            error!(path = ?out_path, %error, "failed to flush compressed output file");
            CompressError::FlushGZip(out_path.clone().into_os_string(), error)
        })?;

        drop(encoder);
        out_file.sync_all().map_err(|error| {
            error!(path = ?out_path, %error, "failed to sync compressed output file");
            CompressError::SyncFile(out_path.clone().into_os_string(), error)
        })?;

        fs::remove_file(&in_path).map_err(|error| {
            error!(path = ?in_path, %error, "failed to remove compression input file");
            CompressError::Remove(in_path.clone().into_os_string(), error)
        })?;

        // Unwrap safety: A valid file path always has a parent path
        let parent = Path::new(&in_path).parent().unwrap();
        sync_dir(parent).map_err(|error| {
            error!(path = ?parent, %error, "failed to sync parent directory");
            CompressError::SyncDir(parent.to_owned().into_os_string(), error)
        })?;

        state
            .lock()
            .unwrap()
            .handle_event(StateEvent::FileCompressed {
                old: in_path.into_os_string(),
                new: out_path.into_os_string(),
            });

        Ok(())
    }

    fn rotate(&mut self) -> Result<(), RotateError> {
        if let Err(error) = self.current.file.flush() {
            error!(path = ?self.current.path, %error, "failed to flush current file");
            return Err(RotateError::Flush(
                self.current.path.clone().into_os_string(),
                error,
            ));
        }

        if let Err(error) = self.current.file.get_mut().sync_all() {
            error!(path = ?self.current.path, %error, "failed to sync current file");
            return Err(RotateError::Sync(
                self.current.path.clone().into_os_string(),
                error,
            ));
        }

        if let Some(max_files) = self.limits.files {
            let mut state = self.state.lock().unwrap();

            while state.files.len() + 1 > max_files.into() {
                let Some(to_remove) = state.files.first() else {
                    break;
                };

                match fs::remove_file(to_remove) {
                    Ok(_) => {
                        // We succeeded in removing the file, so remove it from our list of files.
                        state.files.pop_first();
                    }
                    Err(error) => {
                        error!(path = ?to_remove, %error, "failed to remove oldest file on disk");
                        return Err(RotateError::Remove(to_remove.clone(), error));
                    }
                }
            }
        }

        let path = self.current.path.clone();
        let state = self.state.clone();
        let work = self.work.clone();

        self.work
            .lock()
            .unwrap()
            .enqueue_compression(move |work_id| {
                let result = Self::compress(path, state);
                work.lock()
                    .unwrap()
                    .handle_event(WorkEvent::CompressionFinished(work_id));
                result
            });

        self.current = Self::open_new_file(&self.storage.root_dir, &self.storage.prefix)?;

        Ok(())
    }

    fn open_new_file(root_dir: &Path, prefix: &str) -> Result<Current, NewFileError> {
        let timestamp = Utc::now();
        let timestamp_str = timestamp.format(DATE_FORMAT).to_string();

        let new_path = root_dir.join(format!("{}{}.log", prefix, timestamp_str));

        let new_file = File::options()
            .append(true)
            .create(true)
            .open(&new_path)
            .map_err(|error| {
                error!(path = ?new_path, %error, "failed to open new output file");
                NewFileError::Open(new_path.clone().into_os_string(), error)
            })?;

        Ok(Current {
            file: BufWriter::new(new_file),
            path: new_path,
            began: timestamp,
            bytes_written: 0,
        })
    }

    fn should_rotate(&self, bytes_to_write: usize, time: DateTime<Utc>) -> bool {
        let too_big = self
            .limits
            .size
            .map(|max_size| self.current.bytes_written + bytes_to_write > max_size.into())
            .unwrap_or_default();

        let too_old = self
            .limits
            .age
            .map(|max_age| time - self.current.began >= max_age)
            .unwrap_or_default();

        too_big || too_old
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.should_rotate(buf.len(), Utc::now()) {
            self.rotate()?;
        }

        match self.current.file.write(buf) {
            Ok(written) => {
                self.current.bytes_written += written;
                Ok(written)
            }
            Err(error) => {
                error!(
                    path = ?self.current.path,
                    %error,
                    "failed to write to file",
                );
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.current.file.flush()
    }
}

#[derive(Default)]
pub struct Limits {
    /// Max size (in bytes) of the file after which it will rotate.
    size: Option<NonZeroUsize>,

    /// The span of time since a file was created beyond which it will rotate.
    age: Option<TimeDelta>,

    /// Max number of files on disk after which the oldest will be deleted on rotation.
    ///
    /// In other words, if, after a rotation, the number of files on disk including the new file
    /// currently being written to would be greater than `files`, delete the oldest file during
    /// rotation.
    files: Option<NonZeroUsize>,
}

impl Limits {
    pub fn size(mut self, bytes: NonZeroUsize) -> Self {
        self.size = Some(bytes);
        self
    }

    pub fn age(mut self, max_age: TimeDelta) -> Self {
        self.age = Some(max_age);
        self
    }

    pub fn files(mut self, max_files: NonZeroUsize) -> Self {
        self.files = Some(max_files);
        self
    }
}

pub struct Storage {
    /// Root directory
    root_dir: PathBuf,

    /// File name prefix, default to empty
    prefix: String,
}

impl Storage {
    pub fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            prefix: "".into(),
        }
    }

    pub fn prefix(mut self, prefix: String) -> Self {
        self.prefix = prefix;
        self
    }
}

enum StateEvent {
    FoundFile { path: OsString },
    FileCompressed { old: OsString, new: OsString },
}

struct Current {
    file: BufWriter<File>,
    path: PathBuf,
    began: DateTime<Utc>,
    bytes_written: usize,
}

#[derive(Debug, Clone, Default)]
struct State {
    files: BTreeSet<OsString>,
}

impl State {
    fn handle_event(&mut self, event: StateEvent) {
        match event {
            StateEvent::FoundFile { path } => {
                self.files.insert(path);
            }
            StateEvent::FileCompressed { old, new } => {
                if !self.files.remove(&old) {
                    trace!(old_path = ?old, "path before compression was not in list of files");
                }
                self.files.insert(new);
            }
        }
    }
}

enum WorkEvent {
    CompressionFinished(WorkId),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct WorkId(ThreadId);

#[derive(Debug, Default)]
struct Work {
    compression: HashMap<WorkId, JoinHandle<Result<(), CompressError>>>,
}

impl Work {
    fn enqueue_compression<F>(&mut self, work: F) -> WorkId
    where
        F: FnOnce(WorkId) -> Result<(), CompressError> + Send + 'static,
    {
        // We want to both give the spawned work its own ID, but also know the ID ourselves so we
        // can store work keyed by its ID. That does necessitate getting the ID twice (so we can
        // know the value in both places) but that shouldn't be an issue.
        //
        // Giving the ID to the work itself lets it do things like deliver events to us later to
        // tell us that it's finished, so we can remove the handle.
        let handle = thread::spawn(move || {
            let id = WorkId(thread::current().id());
            work(id)
        });
        let id = WorkId(handle.thread().id());
        self.compression.insert(id, handle);
        id
    }

    fn handle_event(&mut self, event: WorkEvent) {
        match event {
            WorkEvent::CompressionFinished(id) => {
                self.compression.remove(&id);
            }
        }
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        for (_, handle) in self.compression.drain() {
            // Don't unwrap these, just log the errors so that we can continue to join on the other
            // threads.
            let _ = handle
                .join()
                .tap_err(|error| warn!(?error, "compression task failed"));
        }

        todo!("join on all the join handles");
    }
}

// struct CurrentContext {
//     file: BufWriter<File>,
//     file_path: OsString,
//     // list of paths to rotated or discovered log files on disk
//     file_history: VecDeque<OsString>,
//     timestamp: DateTime<Utc>,
//     total_written: usize,
// }
//
// /// A thread-safe rotating file with customizable rotation behavior.
// pub struct RotatingFile {
//     /// Root directory
//     root_dir: PathBuf,
//     /// Max size(in kilobytes) of the file after which it will rotate, 0 means unlimited
//     size: usize,
//     /// How often(in seconds) to rotate, 0 means unlimited
//     interval: u64,
//     /// Max number of files on disk after which the oldest will be deleted on rotation, 0 means
//     /// unlimited
//     files: usize,
//     /// Compression method, default to None
//     compression: Option<Compression>,
//
//     /// Format as used in chrono <https://docs.rs/chrono/latest/chrono/format/strftime/>, default to `%Y-%m-%d-%H-%M-%S`
//     date_format: String,
//     /// File name prefix, default to empty
//     prefix: String,
//     /// File name suffix, default to `.log`
//     suffix: String,
//
//     // current context
//     context: Mutex<CurrentContext>,
//     // compression threads
//     handles: Arc<Mutex<Vec<JoinHandle<Result<OsString, CompressError>>>>>,
// }
//
// /// Builder for a [`RotatingFile`].
// ///
// /// Created by the [`build()`](RotatingFile::build()) method.
// pub struct RotatingFileBuilder {
//     root_dir: PathBuf,
//     size: Option<usize>,
//     interval: Option<u64>,
//     files: Option<(usize, CleanupStrategy)>,
//     compression: Option<Compression>,
//     date_format: Option<String>,
//     prefix: Option<String>,
//     suffix: Option<String>,
// }
//
// impl RotatingFileBuilder {
//     /// Set the maximum size (in kilobytes) of any one file before rotating to the next file.
//     pub fn size(mut self, size: usize) -> Self {
//         self.size = Some(size);
//         self
//     }
//
//     /// Set the interval (in seconds) between file rotations.
//     pub fn interval(mut self, interval: u64) -> Self {
//         self.interval = Some(interval);
//         self
//     }
//
//     /// Set the type of [compression](Compression) to use for files when rotating away from them.
//     ///
//     /// Available values are:
//     ///
//     /// - `Compression::GZip`
//     /// - `Compression::Zip` (requires the `zip` feature)
//     pub fn compression(mut self, compression: Compression) -> Self {
//         self.compression = Some(compression);
//         self
//     }
//
//     /// Set the maximum number of files on disk before the oldest will be removed on rotation.
//     ///
//     /// You must also provide a [`CleanupStrategy`], which determines whether:
//     ///
//     /// - Only files with a matching [prefix](Self::prefix()) and [suffix](Self::suffix()) in their
//     ///   name will count towards the limit and be at risk of removal
//     ///   (`CleanupStrategy::MatchingName`), or
//     /// - All files in the root directory will count towards limit and be at risk of removal
//     ///   (`CleanupStrategy::All`).
//     pub fn files(mut self, files: usize, strategy: CleanupStrategy) -> Self {
//         self.files = Some((files, strategy));
//         self
//     }
//
//     /// Set the format to use for the date and time in filenames.
//     ///
//     /// Uses the syntax from [`chrono`].
//     pub fn date_format(mut self, date_format: String) -> Self {
//         self.date_format = Some(date_format);
//         self
//     }
//
//     /// Set the prefix string for the name of every file.
//     pub fn prefix(mut self, prefix: String) -> Self {
//         self.prefix = Some(prefix);
//         self
//     }
//
//     /// Set the suffix string for the name of every file.
//     pub fn suffix(mut self, suffix: String) -> Self {
//         self.suffix = Some(suffix);
//         self
//     }
//
//     /// Build the [`RotatingFile`].
//     pub fn finish(self) -> Result<RotatingFile, BuilderFinishError> {
//         let root_dir = self.root_dir;
//
//         fs::create_dir_all(&root_dir).map_err(|error| {
//             error!(?root_dir, %error, "failed to create root directory");
//             BuilderFinishError::CreateRootDir(root_dir.clone(), error)
//         })?;
//
//         let size = self.size.unwrap_or(0);
//         let interval = self.interval.unwrap_or(0);
//         let compression = self.compression;
//
//         let date_format = self
//             .date_format
//             .unwrap_or_else(|| "%Y-%m-%d-%H-%M-%S".to_string());
//         let prefix = self.prefix.unwrap_or_else(|| "".to_string());
//         let suffix = self.suffix.unwrap_or_else(|| ".log".to_string());
//
//         let handles = Arc::new(Mutex::new(Vec::new()));
//
//         let (files, file_history) = self.files.and_then(|(limit, strategy)| {
//             RotatingFile::collect_existing_files(&root_dir, &prefix, &suffix, strategy, compression, &handles)
//                 .tap_err(|error| warn!(?root_dir, %error, "unable to collect existing files in root directory; ignoring"))
//                 .ok().zip(Some(limit))
//         }).map_or_else(|| (0, VecDeque::new()), |(existing_files, limit)| (limit, existing_files));
//
//         let context = RotatingFile::create_context(
//             interval,
//             &root_dir,
//             file_history,
//             date_format.as_str(),
//             prefix.as_str(),
//             suffix.as_str(),
//         );
//
//         Ok(RotatingFile {
//             root_dir,
//             size,
//             interval,
//             files,
//             compression,
//             date_format,
//             prefix,
//             suffix,
//             context: Mutex::new(context),
//             handles,
//         })
//     }
// }
//
// impl RotatingFile {
//     pub fn build(root_dir: PathBuf) -> RotatingFileBuilder {
//         RotatingFileBuilder {
//             root_dir,
//             size: None,
//             interval: None,
//             files: None,
//             compression: None,
//             date_format: None,
//             prefix: None,
//             suffix: None,
//         }
//     }
//
//     pub fn root_dir(&self) -> &Path {
//         &self.root_dir
//     }
//
//     pub fn close(&self) -> Result<(), CloseError> {
//         self.close_inner(true)
//     }
//
//     fn close_inner(&self, fail_fast: bool) -> Result<(), CloseError> {
//         let mut guard = self.context.lock().unwrap();
//
//         if let Err(error) = guard.file.flush() {
//             error!(path = ?guard.file_path, %error, "failed to flush current file");
//             if fail_fast {
//                 return Err(CloseError::Flush(guard.file_path.clone(), error));
//             }
//         }
//
//         // compress in a background thread
//         if let Some(c) = self.compression {
//             let file_path = guard.file_path.clone();
//             let handles_clone = self.handles.clone();
//             let handle = std::thread::spawn(move || Self::compress(file_path, c, handles_clone));
//             self.handles.lock().unwrap().push(handle);
//         } else {
//             // no compression, we'll sync the original file
//             if let Err(error) = guard.file.get_ref().sync_all() {
//                 error!(path = ?guard.file_path, %error, "failed to sync current file");
//                 if fail_fast {
//                     return Err(CloseError::SyncFile(guard.file_path.clone(), error));
//                 }
//             }
//         }
//
//         // wait for compression threads
//         let mut handles = self.handles.lock().unwrap();
//         let mut compression_errors = vec![];
//         for handle in handles.drain(..) {
//             if let Err(error) = handle.join().unwrap() {
//                 compression_errors.push(error);
//             }
//         }
//         drop(handles);
//
//         if compression_errors.is_empty() {
//             // Unwrap safety: A valid file path always has a parent path
//             let path = Path::new(&guard.file_path).parent().unwrap();
//             if let Err(error) = sync_dir(path) {
//                 error!(path = ?guard.file_path.clone(), %error, "failed to sync parent directory");
//                 if fail_fast {
//                     return Err(CloseError::SyncDir(guard.file_path.clone(), error));
//                 }
//             }
//
//             Ok(())
//         } else {
//             Err(CloseError::Compression(compression_errors))
//         }
//     }
//
//     fn collect_existing_files(
//         root_dir: &Path,
//         prefix: &str,
//         suffix: &str,
//         strategy: CleanupStrategy,
//         compression: Option<Compression>,
//         handles: &Arc<Mutex<Vec<JoinHandle<Result<OsString, CompressError>>>>>,
//     ) -> Result<VecDeque<OsString>, CollectFilesError> {
//         let mut all_have_created_time = true;
//
//         let mut results = fs::read_dir(root_dir)
//             .map_err(|err| CollectFilesError::ReadDir(root_dir.to_path_buf(), err))?
//             .flatten()
//             .filter(|entry| match strategy {
//                 CleanupStrategy::MatchingFormat => {
//                     let name = entry.file_name();
//                     let name_str = name.to_string_lossy();
//                     name_str.starts_with(prefix)
//                         && (name_str.ends_with(suffix)
//                             || name_str.ends_with(&format!("{suffix}.zip"))
//                             || name_str.ends_with(&format!("{suffix}.gz")))
//                 }
//                 CleanupStrategy::All => true,
//             })
//             .map(|entry| {
//                 let created_time = entry.metadata().and_then(|meta| meta.created()).ok();
//                 all_have_created_time &= created_time.is_some();
//
//                 // Check if there are any files that are uncompressed and begin background
//                 // compression threads for each of them
//                 if let Some(compression) = compression {
//                     let file_path = entry.path();
//                     if file_path.extension()
//                         != Some(&compression.extension().to_owned().conv::<OsString>())
//                     {
//                         let handle = std::thread::spawn(move || {
//                             Self::compress(file_path, compression, handles.clone())
//                         });
//                         handles.lock().unwrap().push(handle);
//                     }
//                 }
//                 (entry, created_time)
//             })
//             .collect::<Vec<_>>();
//
//         if all_have_created_time {
//             debug!("sorting collected files by creation time");
//             // Unwrap safety: if `all_have_created_time` is true, then every timestamp value in the
//             // vec is `Some(_)`.
//             results.sort_by_key(|(_, created)| created.unwrap());
//         } else {
//             warn!("failed to read metadata for at least one file; sorting collected files by name");
//             results.sort_by_key(|(entry, _)| entry.file_name());
//         }
//
//         Ok(results
//             .into_iter()
//             .map(|(entry, _)| entry.path().as_os_str().to_os_string())
//             .collect::<VecDeque<_>>())
//     }
//
//     /// Initiate a rotation of the current file, regardless of whether any of the limits set in
//     /// `Self` have been reached, returning the path to the resulting file.
//     ///
//     /// This function will block to compress the file if compression is enabled, and will return a
//     /// path to the compressed file.
//     pub fn cut(&self) -> Result<OsString, CutError> {
//         let mut guard = self.context.lock().unwrap();
//
//         let file_path = match self.rotate(&mut guard, true)? {
//             Either::Left(uncompressed_path) => uncompressed_path,
//             Either::Right(compression_spawner) => compression_spawner().join().unwrap()?,
//         };
//
//         Ok(file_path)
//     }
//
//     /// TODO: document
//     pub fn export<P: AsRef<Path>>(&self, dest: P) -> Result<u64, ExportError> {
//         let mut guard = self.context.lock().unwrap();
//
//         let file_path = match self.rotate(&mut guard, true)? {
//             Either::Left(uncompressed_path) => uncompressed_path,
//             Either::Right(compression_spawner) => compression_spawner().join().unwrap()?,
//         };
//     }
//
//     /// Initiates a rotation of the file.
//     ///
//     /// The return type of this function is quite complicated, and here's why:
//     ///
//     /// - Sometimes, there's compression enabled for the file being rotated away from, and sometimes
//     ///   there isn't.
//     /// - If there's compression enabled, we sometimes want to run that compression in the
//     ///   background.
//     /// - But sometimes we need to know the path of the resulting file (such as in
//     ///   [`cut()`](RotatingFile::cut())).
//     /// - Therefore, in order to give the caller access to the path of the resulting file in all
//     ///   cases, we need to give them the opportunity to choose whether to join on the thread to get
//     ///   the resulting path (if compression is being performed) or simply release it to run in the
//     ///   background, storing the join handle for later.
//     /// - So we either:
//     ///   - Return an `Either::Left` containing the path to the file if we aren't performing
//     ///     compression, or
//     ///   - Return an `Either::Right` containing a function that will spawn the compression thread
//     ///     and return its join handle. The caller can then choose what to do with the join handle.
//     ///
//     /// As the caller, the important piece of all this (if you're not interested in the output path)
//     /// is that you must call the fn in the Either::Right if you receive it.
//     fn rotate(
//         &self,
//         guard: &mut MutexGuard<CurrentContext>,
//         fail_fast: bool,
//     ) -> Result<
//         Either<OsString, impl FnOnce() -> JoinHandle<Result<OsString, CompressError>>>,
//         RotateError,
//     > {
//         if let Err(error) = guard.file.flush() {
//             error!(path = ?guard.file_path, %error, "failed to flush completed file");
//             if fail_fast {
//                 return Err(RotateError::Flush(guard.file_path.clone(), error));
//             }
//         }
//
//         let old_file = guard.file_path.clone();
//
//         let history_file = if let Some(c) = self.compression {
//             let mut history_file = old_file.clone();
//             history_file.push(".");
//             history_file.push(c.extension());
//             history_file
//         } else {
//             // no compression, we'll sync the original file
//             if let Err(error) = guard.file.get_ref().sync_all() {
//                 error!(path = ?guard.file_path, %error, "failed to sync completed file");
//                 if fail_fast {
//                     return Err(RotateError::Sync(guard.file_path.clone(), error));
//                 }
//             }
//             old_file.clone()
//         };
//
//         let mut file_history = guard.file_history.clone();
//         file_history.push_back(history_file);
//
//         while self.files > 0 && file_history.len() >= self.files {
//             let Some(to_remove) = file_history.pop_front() else {
//                 break;
//             };
//             if let Err(error) = std::fs::remove_file(&to_remove) {
//                 error!("failed to remove completed file {:?}: {}", to_remove, error);
//                 if fail_fast {
//                     return Err(RotateError::Remove(to_remove, error));
//                 }
//             }
//         }
//
//         // reset context
//         **guard = Self::create_context(
//             self.interval,
//             &self.root_dir,
//             file_history,
//             self.date_format.as_str(),
//             self.prefix.as_str(),
//             self.suffix.as_str(),
//         );
//
//         // compress in a background thread
//         let path_or_thread_id = self
//             .compression
//             .map(|c| {
//                 let handles_clone = self.handles.clone();
//                 let old_file = old_file.clone();
//                 let c = c.clone();
//                 Either::Right(move || {
//                     std::thread::spawn(move || Self::compress(old_file, c, handles_clone))
//                 })
//             })
//             .unwrap_or_else(|| Either::Left(old_file));
//
//         Ok(path_or_thread_id)
//     }
//
//     fn create_context(
//         interval: u64,
//         root_dir: &Path,
//         file_history: VecDeque<OsString>,
//         date_format: &str,
//         prefix: &str,
//         suffix: &str,
//     ) -> CurrentContext {
//         let timestamp = Utc::now();
//         let timestamp_str = timestamp.format(date_format).to_string();
//
//         let mut file_name = format!("{}{}{}", prefix, timestamp_str, suffix);
//         let mut index = 1;
//         while root_dir.join(file_name.as_str()).exists()
//             || root_dir.join(file_name.clone() + ".gz").exists()
//             || root_dir.join(file_name.clone() + ".zip").exists()
//         {
//             file_name = format!("{}{}-{}{}", prefix, timestamp_str, index, suffix);
//             index += 1;
//         }
//
//         let file_path = Path::new(root_dir).join(file_name).into_os_string();
//
//         let file = fs::OpenOptions::new()
//             .append(true)
//             .create(true)
//             .open(file_path.as_os_str())
//             .unwrap();
//
//         CurrentContext {
//             file: BufWriter::new(file),
//             file_path,
//             file_history,
//             timestamp,
//             total_written: 0,
//         }
//     }
//
//     fn compress(
//         file_path: OsString,
//         compress: Compression,
//         handles: Arc<Mutex<Vec<JoinHandle<Result<OsString, CompressError>>>>>,
//     ) -> Result<OsString, CompressError> {
//         let thread_id = std::thread::current().id();
//         debug!(path = ?file_path, algorithm = %compress, ?thread_id, "compressing file");
//
//         let mut out_file_path = file_path.clone();
//         out_file_path.push(".");
//         out_file_path.push(compress.extension());
//
//         let mut out_file = fs::OpenOptions::new()
//             .write(true)
//             .create(true)
//             .truncate(true)
//             .open(out_file_path.as_os_str())
//             .map_err(|error| {
//                 error!(path = ?out_file_path, %error, "failed to open output file for compression");
//                 CompressError::OpenOutput(out_file_path.clone(), error)
//             })?;
//
//         let mut in_file = File::open(file_path.as_os_str()).map_err(|error| {
//             warn!(path = ?file_path, %error, "failed to open input file for compression");
//             CompressError::OpenInput(file_path.clone(), error)
//         })?;
//
//         match compress {
//             Compression::GZip => {
//                 let mut encoder = GzEncoder::new(&mut out_file, Compression::default());
//                 io::copy(&mut in_file, &mut encoder).map_err(|error| {
//                     error!(path = ?out_file_path, %error, "failed to write compressed output to file");
//                     CompressError::Write(out_file_path.clone(), error)
//                 })?;
//                 encoder.flush().map_err(|error| {
//                     error!(path = ?out_file_path, %error, "failed to flush compressed output file");
//                     CompressError::FlushGZip(out_file_path.clone(), error)
//                 })?;
//             }
//             #[cfg(feature = "zip")]
//             Compression::Zip => {
//                 let file_name = Path::new(file_path.as_os_str())
//                     .file_name()
//                     .unwrap()
//                     .to_str()
//                     .unwrap();
//                 let mut zip = zip::ZipWriter::new(&mut out_file);
//                 zip.start_file(file_name, zip::write::FileOptions::default())
//                     .map_err(|error| {
//                         error!(path = ?file, %error, "failed to compress in zip format");
//                         CompressError::Zip(file_path.clone(), error)
//                     })?;
//                 io::copy(&mut in_file, &mut zip).map_err(|error| {
//                     error!(path = ?out_file_path, %error, "failed to write compressed output to file");
//                     CompressError::Write(out_file_path.clone(), error)
//                 })?;
//                 zip.finish().map_err(|error| {
//                     error!(path = ?out_file_path, %error, "failed to finish writing zip-compressed output file");
//                     CompressError::FinishZip(out_file_path.clone(), error)
//                 })?;
//             }
//         }
//
//         let ret = (|| {
//             out_file.sync_all().map_err(|error| {
//                 error!(path = ?out_file_path, %error, "failed to sync current file");
//                 CompressError::SyncFile(file_path.clone(), error)
//             })?;
//
//             fs::remove_file(file_path.as_os_str()).map_err(|error| {
//                 error!(path = ?file_path, %error, "failed to remove compression input file");
//                 CompressError::Remove(file_path.clone(), error)
//             })?;
//
//             // Unwrap safety: A valid file path always has a parent path
//             let parent = Path::new(&file_path).parent().unwrap();
//             sync_dir(parent).map_err(|error| {
//                 error!(path = ?out_file_path, %error, "failed to sync parent directory");
//                 CompressError::SyncDir(file_path.clone(), error)
//             })
//         })();
//
//         // remove from the handles vector
//         if let Ok(ref mut guard) = handles.try_lock() {
//             let current_id = std::thread::current().id();
//             if let Some(pos) = guard.iter().position(|h| h.thread().id() == current_id) {
//                 guard.remove(pos);
//             }
//         }
//
//         ret.map(|()| out_file_path)
//     }
// }
//
// impl Drop for RotatingFile {
//     fn drop(&mut self) {
//         let _ = self.close_inner(false);
//     }
// }
//
// impl RotatingFile {
//     pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
//         let mut guard = self.context.lock().unwrap();
//
//         let now = Utc::now();
//
//         if (self.size > 0 && guard.total_written + buf.len() + 1 >= self.size * 1024)
//             || (self.interval > 0 && now >= (guard.timestamp + Duration::from_secs(self.interval)))
//         {
//             if let Either::Right(thread_spawner) = self.rotate(&mut guard, true)? {
//                 let handle = thread_spawner();
//                 self.handles.lock().unwrap().push(handle);
//             }
//         }
//
//         match guard.file.write(buf) {
//             Ok(written) => {
//                 guard.total_written += written;
//                 Ok(written)
//             }
//             Err(error) => {
//                 error!(
//                     path = ?guard.file_path,
//                     %error,
//                     "failed to write to file",
//                 );
//                 Err(error)
//             }
//         }
//     }
//
//     pub fn flush(&self) -> io::Result<()> {
//         let mut guard = self.context.lock().unwrap();
//         guard.file.flush()
//     }
// }
//
// impl Write for RotatingFile {
//     fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
//         RotatingFile::write(self, buf)
//     }
//
//     fn flush(&mut self) -> io::Result<()> {
//         RotatingFile::flush(self)
//     }
// }
//
// impl Write for &RotatingFile {
//     fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
//         RotatingFile::write(self, buf)
//     }
//
//     fn flush(&mut self) -> io::Result<()> {
//         RotatingFile::flush(self)
//     }
// }

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        // On unix directory opens must be read only.
        File::open(path)?.sync_all()
    }
    #[cfg(target_os = "windows")]
    {
        // On windows we must use FILE_FLAG_BACKUP_SEMANTICS to get a handle to the file
        // From: https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilea
        //
        // Beware that `fs::OpenOptionsExt::custom_flags` takes a `u32` for the Windows
        // implementation but an `i32` for the unix implementation.
        // https://doc.rust-lang.org/std/os/windows/fs/trait.OpenOptionsExt.html#tymethod.custom_flags
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
            .sync_all()
    }
}

#[cfg(test)]
struct MockWallClock {
    now: Mutex<DateTime<Utc>>,
}

#[cfg(test)]
impl Default for MockWallClock {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let duration_since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        #[cfg(target_arch = "wasm32")]
        let duration_since_epoch = Duration::from_millis(js_sys::Date::now() as u64);

        let now = Utc
            .timestamp_millis_opt(duration_since_epoch.as_millis() as i64)
            .single()
            .expect("current system clock should yield valid timestamp");

        Self {
            now: Mutex::new(now),
        }
    }
}

#[cfg(test)]
impl MockWallClock {
    fn duration_since_epoch(&self) -> Duration {
        Duration::from_millis(self.now.lock().unwrap().timestamp_millis() as u64)
    }

    fn current_timestamp_str(&self) -> String {
        self.now
            .lock()
            .unwrap()
            .format("%Y-%m-%d-%H-%M-%S")
            .to_string()
    }

    fn increment_by(&self, duration: Duration) {
        let mut now = self.now.lock().unwrap();
        *now += chrono::Duration::from_std(duration)
            .expect("stdlib Duration should fit within chrono Duration range");
    }
}

#[cfg(test)]
lazy_static::lazy_static! {
    static ref MOCK_WALL_CLOCK: MockWallClock = MockWallClock::default();
}

/// Returns duration since unix epoch.
pub fn wall_clock() -> Duration {
    #[cfg(test)]
    return MOCK_WALL_CLOCK.duration_since_epoch();

    #[cfg(all(not(target_arch = "wasm32"), not(test)))]
    return SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    #[cfg(all(target_arch = "wasm32", not(test)))]
    return Duration::from_millis(js_sys::Date::now() as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    use once_cell::sync::Lazy;
    use serial_test::serial;
    use std::time::Duration;
    use std::{io::Write, sync::Mutex};
    use std::{ops::Not, path::PathBuf};

    const TEXT: &'static str = "The quick brown fox jumps over the lazy dog";

    #[test]
    #[serial(mock_time)]
    fn rotate_by_size() {
        let root_dir = PathBuf::from("./target/tmp1");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .finish()
            .expect("able to build rotating file");

        for _ in 0..23 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp.clone() + ".log").exists());
        assert!(!root_dir.join(timestamp.clone() + "-1.log").exists());

        std::fs::remove_dir_all(&root_dir).unwrap();

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));
        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .finish()
            .expect("able to build rotating file");

        for _ in 0..24 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp.clone() + ".log").exists());
        assert!(root_dir.join(timestamp.clone() + "-1.log").exists());
        assert_eq!(
            format!("{}\n", TEXT),
            std::fs::read_to_string(root_dir.join(timestamp + "-1.log")).unwrap()
        );

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn rotate_by_time() {
        let root_dir = PathBuf::from("./target/tmp2");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .finish()
            .expect("able to build rotating file");

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));
        let timestamp1 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));
        let timestamp2 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp1 + ".log").exists());
        assert!(root_dir.join(timestamp2 + ".log").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn rotate_by_size_and_gzip() {
        let root_dir = PathBuf::from("./target/tmp3");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .compression(Compression::GZip)
            .finish()
            .expect("able to build rotating file");

        for _ in 0..24 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp.clone() + ".log.gz").exists());
        assert!(root_dir.join(timestamp + "-1.log.gz").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn rotate_by_time_and_gzip() {
        let root_dir = PathBuf::from("./target/tmp5");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .compression(Compression::GZip)
            .finish()
            .expect("able to build rotating file");

        let timestamp1 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));

        let timestamp2 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp1 + ".log.gz").exists());
        assert!(root_dir.join(timestamp2 + ".log.gz").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn rotate_by_max_files() {
        let root_dir = PathBuf::from("./target/tmp7");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .files(2, CleanupStrategy::All)
            .finish()
            .expect("able to build rotating file");

        let timestamp1 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir.join(timestamp1.clone() + ".log").exists());

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));
        let timestamp2 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir.join(timestamp1.clone() + ".log").exists());
        assert!(root_dir.join(timestamp2.clone() + ".log").exists());

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));
        let timestamp3 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir.join(timestamp1 + ".log").exists().not());
        assert!(root_dir.join(timestamp2 + ".log").exists());
        assert!(root_dir.join(timestamp3 + ".log").exists());

        rotating_file.close().expect("able to close rotating file");

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn referred_in_two_threads() {
        static ROOT_DIR: Lazy<PathBuf> = Lazy::new(|| "./target/tmp8".into());
        static ROTATING_FILE: Lazy<Mutex<RotatingFile>> = Lazy::new(|| {
            Mutex::new(
                RotatingFile::build(ROOT_DIR.clone())
                    .size(1)
                    .finish()
                    .expect("able to build rotating file"),
            )
        });
        let _ = std::fs::remove_dir_all(&*ROOT_DIR);

        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let handle1 = std::thread::spawn(move || {
            for _ in 0..23 {
                writeln!(ROTATING_FILE.lock().unwrap(), "{}", TEXT).unwrap();
            }
        });

        let handle2 = std::thread::spawn(move || {
            for _ in 0..23 {
                writeln!(ROTATING_FILE.lock().unwrap(), "{}", TEXT).unwrap();
            }
        });

        // trigger the third file creation
        writeln!(ROTATING_FILE.lock().unwrap(), "{}", TEXT).unwrap();

        let _ = handle1.join();
        let _ = handle2.join();

        ROTATING_FILE
            .lock()
            .unwrap()
            .close()
            .expect("able to close rotating file");

        assert!(ROOT_DIR.join(timestamp.clone() + ".log").exists());
        assert!(ROOT_DIR.join(timestamp.clone() + "-1.log").exists());

        let third_file = ROOT_DIR.join(timestamp.clone() + "-2.log");
        assert!(third_file.exists());
        assert_eq!(
            TEXT.len() + 1,
            std::fs::metadata(third_file).unwrap().len() as usize
        );

        std::fs::remove_dir_all(&*ROOT_DIR).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn clean_up_existing_matching_name() {
        let root_dir = PathBuf::from("./target/tmp9");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .prefix("test-logs-".into())
            .suffix(".test.log".into())
            .finish()
            .expect("able to build rotating file");

        let timestamp1 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());

        rotating_file.close().expect("able to close rotating file");

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));

        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .prefix("test-logs-".into())
            .suffix(".test.log".into())
            .interval(1)
            .files(2, CleanupStrategy::MatchingFormat)
            .finish()
            .expect("able to build rotating file");

        let guard = rotating_file.context.lock().unwrap();
        drop(guard);

        let timestamp2 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());
        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp2 + ".test.log")
            .exists());

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));

        let timestamp3 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists()
            .not());
        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp2 + ".test.log")
            .exists());
        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp3 + ".test.log")
            .exists());

        rotating_file.close().expect("able to close rotating file");

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn clean_up_existing_all() {
        let root_dir = PathBuf::from("./target/tmp10");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .prefix("test-logs-".into())
            .suffix(".test.log".into())
            .finish()
            .expect("able to build rotating file");

        let timestamp1 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());

        rotating_file.close().expect("able to close rotating file");

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));

        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .files(2, CleanupStrategy::All)
            .finish()
            .expect("able to build rotating file");

        let guard = rotating_file.context.lock().unwrap();
        drop(guard);

        let timestamp2 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());
        assert!(root_dir.join(timestamp2.clone() + ".log").exists());

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));

        let timestamp3 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists()
            .not());
        assert!(root_dir.join(timestamp2.clone() + ".log").exists());
        assert!(root_dir.join(timestamp3.clone() + ".log").exists());

        rotating_file.close().expect("able to close rotating file");

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn cut_file_works() {
        let root_dir = PathBuf::from("./target/tmp11");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .finish()
            .expect("able to build rotating file");

        // Write some stuff into the file, but not enough to cause it to rotate yet.
        for _ in 0..4 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        // Cut the file. This should cause the rotating file writer to move on to the next file.
        let resulting_path = rotating_file
            .cut()
            .expect("able to cut the rotating file at an arbitrary point");

        assert!(
            root_dir.join(timestamp.clone() + ".log").exists(),
            "the original file should exist"
        );

        assert_eq!(
            resulting_path,
            root_dir.join(timestamp.clone() + ".log"),
            "cut() should have returned a path to the original file"
        );

        rotating_file.close().expect("able to close rotating file");
        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    #[serial(mock_time)]
    fn cut_file_compresses() {
        let root_dir = PathBuf::from("./target/tmp12");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .compression(Compression::GZip)
            .finish()
            .expect("able to build rotating file");

        // Write some stuff into the file, but not enough to cause it to rotate yet.
        for _ in 0..4 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        // Cut the file. This should cause it to be compressed and for the rotating file writer to
        // move on to the next file.
        let resulting_path = rotating_file
            .cut()
            .expect("able to cut the rotating file at an arbitrary point");

        assert!(
            root_dir.join(timestamp.clone() + ".log.gz").exists(),
            "the original file should have been compressed after cutting it"
        );

        assert_eq!(
            resulting_path,
            root_dir.join(timestamp.clone() + ".log.gz"),
            "cut() should have returned a path to the compressed file"
        );

        assert!(
            root_dir.join(timestamp.clone() + ".log").exists().not(),
            "as part of the compression, the original uncompressed file should have been removed"
        );

        rotating_file.close().expect("able to close rotating file");
        std::fs::remove_dir_all(root_dir).unwrap();
    }
}
