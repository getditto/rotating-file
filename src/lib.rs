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

use std::sync::MutexGuard;
use std::{collections::VecDeque, fmt, io::Write};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use std::{fs, sync::Mutex};
use std::{io::BufWriter, sync::Arc};
use std::{thread::JoinHandle, time::Duration};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use chrono::TimeZone;
use chrono::{DateTime, NaiveDateTime, Utc};
use either::Either;
use flate2::write::GzEncoder;
use tracing::{debug, error, warn};

use crate::errors::{
    BuilderFinishError, CloseError, CollectFilesError, CompressError, CutError, RotateError,
};

pub mod errors;

#[derive(Debug, Copy, Clone)]
pub enum Compression {
    GZip,
    #[cfg(feature = "zip")]
    Zip,
}

impl Compression {
    fn extension(self) -> &'static str {
        match self {
            Compression::GZip => ".gz",
            #[cfg(feature = "zip")]
            Compression::Zip => ".zip",
        }
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Compression::GZip => write!(f, "gzip"),
            #[cfg(feature = "zip")]
            Compression::Zip => write!(f, "zip"),
        }
    }
}

#[derive(Copy, Clone)]
pub enum CleanupStrategy {
    MatchingFormat,
    All,
}

struct CurrentContext {
    file: BufWriter<fs::File>,
    file_path: OsString,
    // list of paths to rotated or discovered log files on disk
    file_history: VecDeque<OsString>,
    timestamp: u64,
    total_written: usize,
}

/// A thread-safe rotating file with customizable rotation behavior.
pub struct RotatingFile {
    /// Root directory
    root_dir: PathBuf,
    /// Max size(in kilobytes) of the file after which it will rotate, 0 means unlimited
    size: usize,
    /// How often(in seconds) to rotate, 0 means unlimited
    interval: u64,
    /// Max number of files on disk after which the oldest will be deleted on rotation, 0 means
    /// unlimited
    files: usize,
    /// Compression method, default to None
    compression: Option<Compression>,

    /// Format as used in chrono <https://docs.rs/chrono/latest/chrono/format/strftime/>, default to `%Y-%m-%d-%H-%M-%S`
    date_format: String,
    /// File name prefix, default to empty
    prefix: String,
    /// File name suffix, default to `.log`
    suffix: String,

    // current context
    context: Mutex<CurrentContext>,
    // compression threads
    handles: Arc<Mutex<Vec<JoinHandle<Result<OsString, CompressError>>>>>,
}

/// Builder for a [`RotatingFile`].
///
/// Created by the [`build()`](RotatingFile::build()) method.
pub struct RotatingFileBuilder {
    root_dir: PathBuf,
    size: Option<usize>,
    interval: Option<u64>,
    files: Option<(usize, CleanupStrategy)>,
    compression: Option<Compression>,
    date_format: Option<String>,
    prefix: Option<String>,
    suffix: Option<String>,
}

impl RotatingFileBuilder {
    /// Set the maximum size (in kilobytes) of any one file before rotating to the next file.
    pub fn size(mut self, size: usize) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the interval (in seconds) between file rotations.
    pub fn interval(mut self, interval: u64) -> Self {
        self.interval = Some(interval);
        self
    }

    /// Set the type of [compression](Compression) to use for files when rotating away from them.
    ///
    /// Available values are:
    ///
    /// - `Compression::GZip`
    /// - `Compression::Zip` (requires the `zip` feature)
    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = Some(compression);
        self
    }

    /// Set the maximum number of files on disk before the oldest will be removed on rotation.
    ///
    /// You must also provide a [`CleanupStrategy`], which determines whether:
    ///
    /// - Only files with a matching [prefix](Self::prefix()) and [suffix](Self::suffix()) in their
    ///   name will count towards the limit and be at risk of removal
    ///   (`CleanupStrategy::MatchingName`), or
    /// - All files in the root directory will count towards limit and be at risk of removal
    ///   (`CleanupStrategy::All`).
    pub fn files(mut self, files: usize, strategy: CleanupStrategy) -> Self {
        self.files = Some((files, strategy));
        self
    }

    /// Set the format to use for the date and time in filenames.
    ///
    /// Uses the syntax from [`chrono`].
    pub fn date_format(mut self, date_format: String) -> Self {
        self.date_format = Some(date_format);
        self
    }

    /// Set the prefix string for the name of every file.
    pub fn prefix(mut self, prefix: String) -> Self {
        self.prefix = Some(prefix);
        self
    }

    /// Set the suffix string for the name of every file.
    pub fn suffix(mut self, suffix: String) -> Self {
        self.suffix = Some(suffix);
        self
    }

    /// Build the [`RotatingFile`].
    pub fn finish(self) -> Result<RotatingFile, BuilderFinishError> {
        let root_dir = self.root_dir;

        fs::create_dir_all(&root_dir).map_err(|error| {
            error!(?root_dir, %error, "failed to create root directory");
            BuilderFinishError::CreateRootDir(root_dir.clone(), error)
        })?;

        let size = self.size.unwrap_or(0);
        let interval = self.interval.unwrap_or(0);
        let compression = self.compression;

        let date_format = self
            .date_format
            .unwrap_or_else(|| "%Y-%m-%d-%H-%M-%S".to_string());
        let prefix = self.prefix.unwrap_or_else(|| "".to_string());
        let suffix = self.suffix.unwrap_or_else(|| ".log".to_string());

        let (files, file_history) = if let Some((limit, strategy)) = self.files {
            match RotatingFile::collect_existing_files(&root_dir, &prefix, &suffix, strategy) {
                Ok(existing_files) => (limit, existing_files),
                Err(error) => {
                    warn!(?root_dir, %error, "unable to collect existing files in root directory; ignoring");
                    (limit, VecDeque::new())
                }
            }
        } else {
            (0, VecDeque::new())
        };

        let context = RotatingFile::create_context(
            interval,
            &root_dir,
            file_history,
            date_format.as_str(),
            prefix.as_str(),
            suffix.as_str(),
        );

        Ok(RotatingFile {
            root_dir,
            size,
            interval,
            files,
            compression,
            date_format,
            prefix,
            suffix,
            context: Mutex::new(context),
            handles: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

impl RotatingFile {
    pub fn build(root_dir: PathBuf) -> RotatingFileBuilder {
        RotatingFileBuilder {
            root_dir,
            size: None,
            interval: None,
            files: None,
            compression: None,
            date_format: None,
            prefix: None,
            suffix: None,
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn close(&self) -> Result<(), CloseError> {
        self.close_inner(true)
    }

    fn close_inner(&self, fail_fast: bool) -> Result<(), CloseError> {
        let mut guard = self.context.lock().unwrap();

        if let Err(error) = guard.file.flush() {
            error!(path = ?guard.file_path, %error, "failed to flush current file");
            if fail_fast {
                return Err(CloseError::Flush(guard.file_path.clone(), error));
            }
        }

        // compress in a background thread
        if let Some(c) = self.compression {
            let file_path = guard.file_path.clone();
            let handles_clone = self.handles.clone();
            let handle = std::thread::spawn(move || Self::compress(file_path, c, handles_clone));
            self.handles.lock().unwrap().push(handle);
        } else {
            // no compression, we'll sync the original file
            if let Err(error) = guard.file.get_ref().sync_all() {
                error!(path = ?guard.file_path, %error, "failed to sync current file");
                if fail_fast {
                    return Err(CloseError::SyncFile(guard.file_path.clone(), error));
                }
            }
        }

        // wait for compression threads
        let mut handles = self.handles.lock().unwrap();
        let mut compression_errors = vec![];
        for handle in handles.drain(..) {
            if let Err(error) = handle.join().unwrap() {
                compression_errors.push(error);
            }
        }
        drop(handles);

        if compression_errors.is_empty() {
            // Unwrap safety: A valid file path always has a parent path
            let path = Path::new(&guard.file_path).parent().unwrap();
            if let Err(error) = sync_dir(path) {
                error!(path = ?guard.file_path.clone(), %error, "failed to sync parent directory");
                if fail_fast {
                    return Err(CloseError::SyncDir(guard.file_path.clone(), error));
                }
            }

            Ok(())
        } else {
            Err(CloseError::Compression(compression_errors))
        }
    }

    fn collect_existing_files(
        root_dir: &Path,
        prefix: &str,
        suffix: &str,
        strategy: CleanupStrategy,
    ) -> Result<VecDeque<OsString>, CollectFilesError> {
        let mut all_have_created_time = true;

        let mut results = fs::read_dir(root_dir)
            .map_err(|err| CollectFilesError::ReadDir(root_dir.to_path_buf(), err))?
            .flatten()
            .filter(|entry| match strategy {
                CleanupStrategy::MatchingFormat => {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    name_str.starts_with(prefix) && name_str.ends_with(suffix)
                }
                CleanupStrategy::All => true,
            })
            .map(|entry| {
                let created_time = entry.metadata().and_then(|meta| meta.created()).ok();
                all_have_created_time &= created_time.is_some();
                (entry, created_time)
            })
            .collect::<Vec<_>>();

        if all_have_created_time {
            debug!("sorting collected files by creation time");
            // Unwrap safety: if `all_have_created_time` is true, then every timestamp value in the
            // vec is `Some(_)`.
            results.sort_by_key(|(_, created)| created.unwrap());
        } else {
            warn!("failed to read metadata for at least one file; sorting collected files by name");
            results.sort_by_key(|(entry, _)| entry.file_name());
        }

        Ok(results
            .into_iter()
            .map(|(entry, _)| entry.path().as_os_str().to_os_string())
            .collect::<VecDeque<_>>())
    }

    /// Initiate a rotation of the current file, regardless of whether any of the limits set in
    /// `Self` have been reached, returning the path to the resulting file.
    ///
    /// This function will block to compress the file if compression is enabled, and will return a
    /// path to the compressed file.
    pub fn cut(&self) -> Result<OsString, CutError> {
        let mut guard = self.context.lock().unwrap();

        let file_path = match self.rotate(&mut guard, true)? {
            Either::Left(uncompressed_path) => uncompressed_path,
            Either::Right(compression_spawner) => compression_spawner().join().unwrap()?,
        };

        Ok(file_path)
    }

    /// Initiates a rotation of the file.
    ///
    /// The return type of this function is quite complicated, and here's why:
    ///
    /// - Sometimes, there's compression enabled for the file being rotated away from, and sometimes
    ///   there isn't.
    /// - If there's compression enabled, we sometimes want to run that compression in the
    ///   background.
    /// - But sometimes we need to know the path of the resulting file (such as in
    ///   [`cut()`](RotatingFile::cut())).
    /// - Therefore, in order to give the caller access to the path of the resulting file in all
    ///   cases, we need to give them the opportunity to choose whether to join on the thread to get
    ///   the resulting path (if compression is being performed) or simply release it to run in the
    ///   background, storing the join handle for later.
    /// - So we either:
    ///   - Return an `Either::Left` containing the path to the file if we aren't performing
    ///     compression, or
    ///   - Return an `Either::Right` containing a function that will spawn the compression thread
    ///     and return its join handle. The caller can then choose what to do with the join handle.
    ///
    /// As the caller, the important piece of all this (if you're not interested in the output path)
    /// is that you must call the fn in the Either::Right if you receive it.
    fn rotate(
        &self,
        guard: &mut MutexGuard<CurrentContext>,
        fail_fast: bool,
    ) -> Result<
        Either<OsString, impl FnOnce() -> JoinHandle<Result<OsString, CompressError>>>,
        RotateError,
    > {
        if let Err(error) = guard.file.flush() {
            error!(path = ?guard.file_path, %error, "failed to flush completed file");
            if fail_fast {
                return Err(RotateError::Flush(guard.file_path.clone(), error));
            }
        }

        let old_file = guard.file_path.clone();

        let history_file = if let Some(c) = self.compression {
            let mut history_file = old_file.clone();
            history_file.push(c.extension());
            history_file
        } else {
            // no compression, we'll sync the original file
            if let Err(error) = guard.file.get_ref().sync_all() {
                error!(path = ?guard.file_path, %error, "failed to sync completed file");
                if fail_fast {
                    return Err(RotateError::Sync(guard.file_path.clone(), error));
                }
            }
            old_file.clone()
        };

        let mut file_history = guard.file_history.clone();
        file_history.push_back(history_file);

        while self.files > 0 && file_history.len() >= self.files {
            let Some(to_remove) = file_history.pop_front() else {
                break;
            };
            if let Err(error) = std::fs::remove_file(&to_remove) {
                error!("failed to remove completed file {:?}: {}", to_remove, error);
                if fail_fast {
                    return Err(RotateError::Remove(to_remove, error));
                }
            }
        }

        // reset context
        **guard = Self::create_context(
            self.interval,
            &self.root_dir,
            file_history,
            self.date_format.as_str(),
            self.prefix.as_str(),
            self.suffix.as_str(),
        );

        // compress in a background thread
        let path_or_thread_id = self
            .compression
            .map(|c| {
                let handles_clone = self.handles.clone();
                let old_file = old_file.clone();
                let c = c.clone();
                Either::Right(move || {
                    std::thread::spawn(move || Self::compress(old_file, c, handles_clone))
                })
            })
            .unwrap_or_else(|| Either::Left(old_file));

        Ok(path_or_thread_id)
    }

    fn create_context(
        interval: u64,
        root_dir: &Path,
        file_history: VecDeque<OsString>,
        date_format: &str,
        prefix: &str,
        suffix: &str,
    ) -> CurrentContext {
        let now = wall_clock().as_secs();
        let timestamp = if interval > 0 {
            now / interval * interval
        } else {
            now
        };

        let dt = DateTime::<Utc>::from_utc(
            NaiveDateTime::from_timestamp_opt(timestamp as i64, 0).unwrap(),
            Utc,
        );
        let dt_str = dt.format(date_format).to_string();

        let mut file_name = format!("{}{}{}", prefix, dt_str, suffix);
        let mut index = 1;
        while root_dir.join(file_name.as_str()).exists()
            || root_dir.join(file_name.clone() + ".gz").exists()
            || root_dir.join(file_name.clone() + ".zip").exists()
        {
            file_name = format!("{}{}-{}{}", prefix, dt_str, index, suffix);
            index += 1;
        }

        let file_path = Path::new(root_dir).join(file_name).into_os_string();

        let file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(file_path.as_os_str())
            .unwrap();

        CurrentContext {
            file: BufWriter::new(file),
            file_path,
            file_history,
            timestamp,
            total_written: 0,
        }
    }

    fn compress(
        file: OsString,
        compress: Compression,
        handles: Arc<Mutex<Vec<JoinHandle<Result<OsString, CompressError>>>>>,
    ) -> Result<OsString, CompressError> {
        let thread_id = std::thread::current().id();
        debug!(path = ?file, algorithm = %compress, ?thread_id, "compressing file");

        let mut out_file_path = file.clone();
        out_file_path.push(compress.extension());

        let mut out_file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(out_file_path.as_os_str())
            .map_err(|error| {
                error!(path = ?out_file_path, %error, "failed to open output file for compression");
                CompressError::OpenOutput(out_file_path.clone(), error)
            })?;

        let mut in_file = fs::File::open(file.as_os_str()).map_err(|error| {
            warn!(path = ?file, %error, "failed to open input file for compression");
            CompressError::OpenInput(file.clone(), error)
        })?;

        match compress {
            Compression::GZip => {
                let mut encoder = GzEncoder::new(&mut out_file, flate2::Compression::default());
                std::io::copy(&mut in_file, &mut encoder).map_err(|error| {
                    error!(path = ?out_file_path, %error, "failed to write compressed output to file");
                    CompressError::Write(out_file_path.clone(), error)
                })?;
                encoder.flush().map_err(|error| {
                    error!(path = ?out_file_path, %error, "failed to flush compressed output file");
                    CompressError::FlushGZip(out_file_path.clone(), error)
                })?;
            }
            #[cfg(feature = "zip")]
            Compression::Zip => {
                let file_name = Path::new(file.as_os_str())
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap();
                let mut zip = zip::ZipWriter::new(&mut out_file);
                zip.start_file(file_name, zip::write::FileOptions::default())
                    .map_err(|error| {
                        error!(path = ?file, %error, "failed to compress in zip format");
                        CompressError::Zip(file.clone(), error)
                    })?;
                std::io::copy(&mut in_file, &mut zip).map_err(|error| {
                    error!(path = ?out_file_path, %error, "failed to write compressed output to file");
                    CompressError::Write(out_file_path.clone(), error)
                })?;
                zip.finish().map_err(|error| {
                    error!(path = ?out_file_path, %error, "failed to finish writing zip-compressed output file");
                    CompressError::FinishZip(out_file_path.clone(), error)
                })?;
            }
        }

        let ret = (|| {
            out_file.sync_all().map_err(|error| {
                error!(path = ?out_file_path, %error, "failed to sync current file");
                CompressError::SyncFile(file.clone(), error)
            })?;

            fs::remove_file(file.as_os_str()).map_err(|error| {
                error!(path = ?file, %error, "failed to remove compression input file");
                CompressError::Remove(file.clone(), error)
            })?;

            // Unwrap safety: A valid file path always has a parent path
            let parent = Path::new(&file).parent().unwrap();
            sync_dir(parent).map_err(|error| {
                error!(path = ?out_file_path, %error, "failed to sync parent directory");
                CompressError::SyncDir(file.clone(), error)
            })
        })();

        // remove from the handles vector
        if let Ok(ref mut guard) = handles.try_lock() {
            let current_id = std::thread::current().id();
            if let Some(pos) = guard.iter().position(|h| h.thread().id() == current_id) {
                guard.remove(pos);
            }
        }

        ret.map(|()| out_file_path)
    }
}

impl Drop for RotatingFile {
    fn drop(&mut self) {
        let _ = self.close_inner(false);
    }
}

impl RotatingFile {
    pub fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.context.lock().unwrap();

        let now = wall_clock().as_secs();

        if (self.size > 0 && guard.total_written + buf.len() + 1 >= self.size * 1024)
            || (self.interval > 0 && now >= (guard.timestamp + self.interval))
        {
            if let Either::Right(thread_spawner) = self.rotate(&mut guard, true)? {
                let handle = thread_spawner();
                self.handles.lock().unwrap().push(handle);
            }
        }

        match guard.file.write(buf) {
            Ok(written) => {
                guard.total_written += written;
                Ok(written)
            }
            Err(error) => {
                error!(
                    path = ?guard.file_path,
                    %error,
                    "failed to write to file",
                );
                Err(error)
            }
        }
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let mut guard = self.context.lock().unwrap();
        guard.file.flush()
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        RotatingFile::write(self, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        RotatingFile::flush(self)
    }
}

impl Write for &RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        RotatingFile::write(self, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        RotatingFile::flush(self)
    }
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        // On unix directory opens must be read only.
        fs::File::open(path)?.sync_all()
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

    #[cfg(feature = "zip")]
    #[test]
    #[serial(mock_time)]
    fn rotate_by_size_and_zip() {
        let root_dir = PathBuf::from("./target/tmp4");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = MOCK_WALL_CLOCK.current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .compression(Compression::Zip)
            .finish()
            .expect("able to build rotating file");

        for _ in 0..24 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp.clone() + ".log.zip").exists());
        assert!(root_dir.join(timestamp + "-1.log.zip").exists());

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

    #[cfg(feature = "zip")]
    #[test]
    #[serial(mock_time)]
    fn rotate_by_time_and_zip() {
        let root_dir = PathBuf::from("./target/tmp6");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .compression(Compression::Zip)
            .finish()
            .expect("able to build rotating file");

        let timestamp1 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        MOCK_WALL_CLOCK.increment_by(Duration::from_secs(1));

        let timestamp2 = MOCK_WALL_CLOCK.current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        rotating_file.close().expect("able to close rotating file");

        assert!(root_dir.join(timestamp1 + ".log.zip").exists());
        assert!(root_dir.join(timestamp2 + ".log.zip").exists());

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
