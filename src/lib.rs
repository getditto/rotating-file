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
//! let mut rotating_file = RotatingFile::build(root_dir.into()).size(1).finish();
//! for _ in 0..24 {
//!     writeln!(rotating_file, "{}", s).unwrap();
//! }
//! rotating_file.close();
//!
//! assert_eq!(2, std::fs::read_dir(root_dir).unwrap().count());
//! std::fs::remove_dir_all(root_dir).unwrap();
//! ```

use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::VecDeque, io::Write};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use std::{fs, io::Error, sync::Mutex};
use std::{io, thread::JoinHandle};
use std::{io::BufWriter, sync::Arc};

use chrono::{DateTime, NaiveDateTime, Utc};
use flate2::write::GzEncoder;
use log::*;
use thiserror::Error;

#[derive(Copy, Clone)]
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
    handles: Arc<Mutex<Vec<JoinHandle<Result<(), Error>>>>>,
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
    pub fn finish(self) -> RotatingFile {
        let root_dir = self.root_dir;

        if let Err(err) = fs::create_dir_all(&root_dir) {
            error!("{err}");
        }

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
                Err(err) => {
                    error!("unable to collect existing files from {root_dir:?}: {err}");
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

        RotatingFile {
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
        }
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

    pub fn close(&self) {
        let mut guard = self.context.lock().unwrap();
        if let Err(e) = guard.file.flush() {
            error!("{}", e);
        }
        if let Err(e) = guard.file.get_ref().sync_all() {
            error!("{}", e);
        }

        // compress in a background thread
        if let Some(c) = self.compression {
            let file_path = guard.file_path.clone();
            let handles_clone = self.handles.clone();
            let handle = std::thread::spawn(move || Self::compress(file_path, c, handles_clone));
            self.handles.lock().unwrap().push(handle);
        }

        // wait for compression threads
        let mut handles = self.handles.lock().unwrap();
        for handle in handles.drain(..) {
            if let Err(e) = handle.join().unwrap() {
                error!("{}", e);
            }
        }
        drop(handles);
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
            results.sort_by_key(|(_, created)| created.unwrap());
        } else {
            results.sort_by_key(|(entry, _)| entry.file_name());
        }

        Ok(results
            .into_iter()
            .map(|(entry, _)| entry.path().as_os_str().to_os_string())
            .collect::<VecDeque<_>>())
    }

    fn create_context(
        interval: u64,
        root_dir: &Path,
        file_history: VecDeque<OsString>,
        date_format: &str,
        prefix: &str,
        suffix: &str,
    ) -> CurrentContext {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
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
        handles: Arc<Mutex<Vec<JoinHandle<Result<(), Error>>>>>,
    ) -> Result<(), Error> {
        let mut out_file_path = file.clone();
        out_file_path.push(compress.extension());

        let out_file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(out_file_path.as_os_str())?;

        let input_buf = fs::read(file.as_os_str())?;

        match compress {
            Compression::GZip => {
                let mut encoder = GzEncoder::new(out_file, flate2::Compression::new(9));
                encoder.write_all(&input_buf)?;
                encoder.flush()?;
            }
            #[cfg(feature = "zip")]
            Compression::Zip => {
                let file_name = Path::new(file.as_os_str())
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap();
                let mut zip = zip::ZipWriter::new(out_file);
                zip.start_file(file_name, zip::write::FileOptions::default())?;
                zip.write_all(&input_buf)?;
                zip.finish()?;
            }
        }

        let ret = fs::remove_file(file.as_os_str());

        // remove from the handles vector
        if let Ok(ref mut guard) = handles.try_lock() {
            let current_id = std::thread::current().id();
            if let Some(pos) = guard.iter().position(|h| h.thread().id() == current_id) {
                guard.remove(pos);
            }
        }

        ret
    }
}

impl Drop for RotatingFile {
    fn drop(&mut self) {
        self.close();
    }
}

impl RotatingFile {
    pub fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.context.lock().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if (self.size > 0 && guard.total_written + buf.len() + 1 >= self.size * 1024)
            || (self.interval > 0 && now >= (guard.timestamp + self.interval))
        {
            guard.file.flush()?;
            guard.file.get_ref().sync_all()?;

            let old_file = guard.file_path.clone();

            let history_file = if let Some(c) = self.compression {
                let mut history_file = old_file.clone();
                history_file.push(c.extension());
                history_file
            } else {
                old_file.clone()
            };

            let mut file_history = guard.file_history.clone();
            file_history.push_back(history_file);

            while self.files > 0 && file_history.len() >= self.files {
                let Some(to_remove) = file_history.pop_front() else {
                    break;
                };
                if let Err(err) = std::fs::remove_file(&to_remove) {
                    error!(
                        "failed to remove old file {}: {}",
                        to_remove.to_string_lossy(),
                        err
                    );
                }
            }

            // reset context
            *guard = Self::create_context(
                self.interval,
                &self.root_dir,
                file_history,
                self.date_format.as_str(),
                self.prefix.as_str(),
                self.suffix.as_str(),
            );

            // compress in a background thread
            if let Some(c) = self.compression {
                let handles_clone = self.handles.clone();
                let handle = std::thread::spawn(move || Self::compress(old_file, c, handles_clone));
                self.handles.lock().unwrap().push(handle);
            }
        }

        match guard.file.write(buf) {
            Ok(written) => {
                guard.total_written += written;
                Ok(written)
            }
            Err(e) => {
                error!(
                    "Failed to write to file {}: {}",
                    guard.file_path.to_str().unwrap(),
                    e
                );
                Err(e)
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

#[derive(Error, Debug)]
pub enum CollectFilesError {
    #[error("failed to read contents of dir {0}: {1}")]
    ReadDir(PathBuf, #[source] io::Error),
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use once_cell::sync::Lazy;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::{io::Write, sync::Mutex};
    use std::{ops::Not, path::PathBuf};

    use crate::{CleanupStrategy, Compression, RotatingFile};

    const TEXT: &'static str = "The quick brown fox jumps over the lazy dog";

    #[test]
    fn rotate_by_size() {
        let root_dir = PathBuf::from("./target/tmp1");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone()).size(1).finish();

        for _ in 0..23 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close();

        assert!(root_dir.join(timestamp.clone() + ".log").exists());
        assert!(!root_dir.join(timestamp.clone() + "-1.log").exists());

        std::fs::remove_dir_all(&root_dir).unwrap();

        let timestamp = current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone()).size(1).finish();

        for _ in 0..24 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close();

        assert!(root_dir.join(timestamp.clone() + ".log").exists());
        assert!(root_dir.join(timestamp.clone() + "-1.log").exists());
        assert_eq!(
            format!("{}\n", TEXT),
            std::fs::read_to_string(root_dir.join(timestamp + "-1.log")).unwrap()
        );

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn rotate_by_time() {
        let root_dir = PathBuf::from("./target/tmp2");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone()).interval(1).finish();

        let timestamp1 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        std::thread::sleep(Duration::from_secs(1));

        let timestamp2 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        rotating_file.close();

        assert!(root_dir.join(timestamp1 + ".log").exists());
        assert!(root_dir.join(timestamp2 + ".log").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn rotate_by_size_and_gzip() {
        let root_dir = PathBuf::from("./target/tmp3");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .compression(Compression::GZip)
            .finish();

        for _ in 0..24 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close();

        assert!(root_dir.join(timestamp.clone() + ".log.gz").exists());
        assert!(root_dir.join(timestamp + "-1.log.gz").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[cfg(feature = "zip")]
    #[test]
    fn rotate_by_size_and_zip() {
        let root_dir = PathBuf::from("./target/tmp4");
        let _ = std::fs::remove_dir_all(&root_dir);
        let timestamp = current_timestamp_str();
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .size(1)
            .compression(Compression::Zip)
            .finish();

        for _ in 0..24 {
            writeln!(rotating_file, "{}", TEXT).unwrap();
        }

        rotating_file.close();

        assert!(root_dir.join(timestamp.clone() + ".log.zip").exists());
        assert!(root_dir.join(timestamp + "-1.log.zip").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn rotate_by_time_and_gzip() {
        let root_dir = PathBuf::from("./target/tmp5");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .compression(Compression::GZip)
            .finish();

        let timestamp1 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        std::thread::sleep(Duration::from_secs(1));

        let timestamp2 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        rotating_file.close();

        assert!(root_dir.join(timestamp1 + ".log.gz").exists());
        assert!(root_dir.join(timestamp2 + ".log.gz").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[cfg(feature = "zip")]
    #[test]
    fn rotate_by_time_and_zip() {
        let root_dir = PathBuf::from("./target/tmp6");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .compression(Compression::Zip)
            .finish();

        let timestamp1 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        std::thread::sleep(Duration::from_secs(1));

        let timestamp2 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        rotating_file.close();

        assert!(root_dir.join(timestamp1 + ".log.zip").exists());
        assert!(root_dir.join(timestamp2 + ".log.zip").exists());

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn rotate_by_max_files() {
        let root_dir = PathBuf::from("./target/tmp7");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .files(2, CleanupStrategy::All)
            .finish();

        let timestamp1 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir.join(timestamp1.clone() + ".log").exists());

        std::thread::sleep(Duration::from_secs(1));

        let timestamp2 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir.join(timestamp1.clone() + ".log").exists());
        assert!(root_dir.join(timestamp2.clone() + ".log").exists());

        std::thread::sleep(Duration::from_secs(1));

        let timestamp3 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir.join(timestamp1 + ".log").exists().not());
        assert!(root_dir.join(timestamp2 + ".log").exists());
        assert!(root_dir.join(timestamp3 + ".log").exists());

        rotating_file.close();

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn referred_in_two_threads() {
        static ROOT_DIR: Lazy<PathBuf> = Lazy::new(|| "./target/tmp8".into());
        static ROTATING_FILE: Lazy<Mutex<RotatingFile>> =
            Lazy::new(|| Mutex::new(RotatingFile::build(ROOT_DIR.clone()).size(1).finish()));
        let _ = std::fs::remove_dir_all(&*ROOT_DIR);

        let timestamp = current_timestamp_str();
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

        ROTATING_FILE.lock().unwrap().close();

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
    fn clean_up_existing_matching_name() {
        let root_dir = PathBuf::from("./target/tmp9");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .prefix("test-logs-".into())
            .suffix(".test.log".into())
            .finish();

        let timestamp1 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());

        rotating_file.close();

        std::thread::sleep(Duration::from_secs(1));

        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .prefix("test-logs-".into())
            .suffix(".test.log".into())
            .interval(1)
            .files(2, CleanupStrategy::MatchingFormat)
            .finish();

        let guard = rotating_file.context.lock().unwrap();
        drop(guard);

        let timestamp2 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());
        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp2 + ".test.log")
            .exists());

        std::thread::sleep(Duration::from_secs(1));

        let timestamp3 = current_timestamp_str();
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

        rotating_file.close();

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn clean_up_existing_all() {
        let root_dir = PathBuf::from("./target/tmp10");
        let _ = std::fs::remove_dir_all(&root_dir);
        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .prefix("test-logs-".into())
            .suffix(".test.log".into())
            .finish();

        let timestamp1 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());

        rotating_file.close();

        std::thread::sleep(Duration::from_secs(1));

        let mut rotating_file = RotatingFile::build(root_dir.clone())
            .interval(1)
            .files(2, CleanupStrategy::All)
            .finish();

        let guard = rotating_file.context.lock().unwrap();
        drop(guard);

        let timestamp2 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists());
        assert!(root_dir.join(timestamp2.clone() + ".log").exists());

        std::thread::sleep(Duration::from_secs(1));

        let timestamp3 = current_timestamp_str();
        writeln!(rotating_file, "{}", TEXT).unwrap();

        assert!(root_dir
            .join("test-logs-".to_string() + &timestamp1 + ".test.log")
            .exists()
            .not());
        assert!(root_dir.join(timestamp2.clone() + ".log").exists());
        assert!(root_dir.join(timestamp3.clone() + ".log").exists());

        rotating_file.close();

        std::fs::remove_dir_all(root_dir).unwrap();
    }

    fn current_timestamp_str() -> String {
        let dt: DateTime<Utc> = SystemTime::now().into();
        let dt_str = dt.format("%Y-%m-%d-%H-%M-%S").to_string();
        dt_str
    }
}
