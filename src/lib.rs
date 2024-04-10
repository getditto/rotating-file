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
use std::{io::BufReader, sync::Mutex};
use std::{io::BufWriter, sync::Arc};
use std::{thread::JoinHandle, time::Duration};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use chrono::TimeZone;
use chrono::{DateTime, TimeDelta, Utc};
use flate2::{write::GzEncoder, Compression};
use tap::{Conv, TapFallible};
use tracing::{debug, error, instrument::Instrumented, trace, warn, Instrument};

use crate::errors::{
    CloseError, CollectFilesError, CompressError, CutError, ExportError, NewError, NewFileError,
    RotateError,
};

/// The suffix that will be present in any filename for a file that has been compressed.
const COMPRESSED_SUFFIX: &'static str = "gz";

const COMPRESSED_EXT: &'static str = "log.gz";

const UNCOMPRESSED_EXT: &'static str = "log";

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
        trace!(root_dir = ?self.storage.root_dir, "collecting existing files from the root directory");

        for entry in fs::read_dir(&self.storage.root_dir)
            .map_err(|err| CollectFilesError::ReadDir(self.storage.root_dir.clone(), err))?
        {
            let entry = entry.map_err(CollectFilesError::ReadDirEntry)?;
            let metadata = entry.metadata().map_err(|error| {
                CollectFilesError::AccessMetadata(entry.path().to_owned(), error)
            })?;

            if !metadata.is_file() {
                debug!(path = ?entry.path(), "found entry in root dir that is not a file; ignoring it");
                continue;
            }

            // If we find a file that is uncompressed, we need to compress the file. Otherwise,
            // we just record the path of the file as-is.
            let path = entry.path();

            if path == self.current.path {
                debug!(?path, "found our currently active log file; ignoring it");
                continue;
            }

            match path.extension() {
                Some(ext) if ext == &COMPRESSED_SUFFIX.to_owned().conv::<OsString>() => {
                    trace!(?path, "found compressed file");
                    self.state.lock().unwrap().found_file(path);
                }
                Some(ext) if ext == &UNCOMPRESSED_EXT.to_owned().conv::<OsString>() => {
                    trace!(?path, "found uncompressed file; compressing it");

                    let state = self.state.clone();
                    let work = self.work.clone();

                    self.work
                        .lock()
                        .unwrap()
                        .enqueue_compression(move |work_id| {
                            trace!(?work_id, "starting compression task");
                            let result = Self::compress(path, state);
                            trace!(?work_id, "compression task finished");
                            work.lock().unwrap().compression_finished(work_id);
                            result
                        });
                }
                Some(_) | None => {
                    debug!(
                        ?path,
                        "found file in root dir with an unrecognised or no extension; ignoring it"
                    );
                }
            }
        }

        Ok(())
    }

    pub fn export<P: AsRef<Path>>(&mut self, dest: P) -> Result<u64, ExportError> {
        trace!(destination = ?dest.as_ref(), "starting export");

        // When we export, we want the most recent possible log data to be included. So rotate away
        // from the current file, which will also kick off a compression of that file (and the join
        // handle for that compression task will be added to `self.work`).
        self.rotate()?;

        // Wait for all ongoing work, including the compression task we just created, to finish.
        let mut work_guard = self.work.lock().unwrap();
        let handles = work_guard.drain_handles();
        drop(work_guard);

        for handle in handles {
            handle.into_inner().join().unwrap()?;
        }

        let out_file = File::options()
            .write(true)
            .create_new(true)
            .open(dest.as_ref())
            .map_err(|error| ExportError::CreateOutput(dest.as_ref().to_owned(), error))?;
        let mut out_writer = BufWriter::new(out_file);

        let mut bytes_copied = 0;
        for in_file_path in self.state.lock().unwrap().files() {
            let in_file = File::options()
                .read(true)
                .open(in_file_path)
                .map_err(|error| ExportError::OpenInput(in_file_path.to_owned(), error))?;
            let mut in_reader = BufReader::new(in_file);

            bytes_copied += io::copy(&mut in_reader, &mut out_writer).map_err(|error| {
                ExportError::Copy(in_file_path.to_owned(), dest.as_ref().to_owned(), error)
            })?;
        }

        out_writer
            .flush()
            .map_err(|error| ExportError::Flush(dest.as_ref().to_owned(), error))?;
        out_writer
            .get_mut()
            .sync_all()
            .map_err(|error| ExportError::Sync(dest.as_ref().to_owned(), error))?;

        Ok(bytes_copied)
    }

    fn compress(in_path: PathBuf, state: Arc<Mutex<State>>) -> Result<(), CompressError> {
        debug_assert_eq!(
            in_path.extension(),
            Some(&*OsString::from("log")),
            "files being compressed should always have the extension `.log`, otherwise their \
             extension will not be correctly set to `.log.gz`"
        );

        let mut out_path = in_path.clone();
        out_path.set_extension(COMPRESSED_EXT);

        debug!(input = ?in_path, destination = ?out_path, "compressing log file");

        let mut in_file = File::open(&in_path).map_err(|error| {
            warn!(path = ?in_path, %error, "failed to open input file for compression");
            CompressError::OpenInput(in_path.clone(), error)
        })?;

        let mut out_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .map_err(|error| {
                error!(path = ?out_path, %error, "failed to open output file for compression");
                CompressError::OpenOutput(out_path.clone(), error)
            })?;

        let mut encoder = GzEncoder::new(&mut out_file, Compression::default());
        io::copy(&mut in_file, &mut encoder).map_err(|error| {
            error!(path = ?out_path, %error, "failed to write compressed output to file");
            CompressError::Write(out_path.clone(), error)
        })?;
        encoder.flush().map_err(|error| {
            error!(path = ?out_path, %error, "failed to flush compressed output file");
            CompressError::FlushGZip(out_path.clone(), error)
        })?;

        drop(encoder);
        out_file.sync_all().map_err(|error| {
            error!(path = ?out_path, %error, "failed to sync compressed output file");
            CompressError::SyncFile(out_path.clone(), error)
        })?;

        fs::remove_file(&in_path).map_err(|error| {
            error!(path = ?in_path, %error, "failed to remove compression input file");
            CompressError::Remove(in_path.clone(), error)
        })?;

        // Unwrap safety: A valid file path always has a parent path
        let parent = Path::new(&in_path).parent().unwrap();
        sync_dir(parent).map_err(|error| {
            error!(path = ?parent, %error, "failed to sync parent directory");
            CompressError::SyncDir(parent.to_owned(), error)
        })?;

        state.lock().unwrap().file_compressed(in_path, out_path);

        Ok(())
    }

    fn rotate(&mut self) -> Result<(), RotateError> {
        if let Err(error) = self.current.file.flush() {
            error!(path = ?self.current.path, %error, "failed to flush current file");
            return Err(RotateError::Flush(self.current.path.clone(), error));
        }

        if let Err(error) = self.current.file.get_mut().sync_all() {
            error!(path = ?self.current.path, %error, "failed to sync current file");
            return Err(RotateError::Sync(self.current.path.clone(), error));
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
                work.lock().unwrap().compression_finished(work_id);
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
            .write(true)
            .create_new(true)
            .open(&new_path)
            .map_err(|error| {
                error!(path = ?new_path, %error, "failed to open new output file");
                NewFileError::Open(new_path.clone(), error)
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

#[derive(Default)]
pub struct Limits {
    /// Max size (in bytes) of the file after which it will rotate.
    size: Option<NonZeroUsize>,

    /// The span of time since a file was created beyond which it will rotate.
    age: Option<TimeDelta>,

    /// Max number of files on disk after which the oldest will be deleted on rotation.
    ///
    /// In other words, if, before a rotation, the number of files on disk including the new file
    /// that will about to start being written to would be greater than `files`, delete the oldest
    /// file.
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

struct Current {
    file: BufWriter<File>,
    path: PathBuf,
    began: DateTime<Utc>,
    bytes_written: usize,
}

impl Drop for Current {
    fn drop(&mut self) {
        if let Err(error) = self.file.flush() {
            error!(path = ?self.path, %error, "failed to flush current output file on drop");
        }

        if let Err(error) = self.file.get_mut().sync_all() {
            error!(path = ?self.path, %error, "failed to sync current output file on drop");
        }
    }
}

#[derive(Debug, Clone, Default)]
struct State {
    files: BTreeSet<PathBuf>,
}

impl State {
    fn found_file(&mut self, path: PathBuf) {
        self.files.insert(path);
    }

    fn file_compressed(&mut self, old: PathBuf, new: PathBuf) {
        self.files.remove(&old);
        self.files.insert(new);
    }

    /// Return an iterator over all paths stored in the state, ordered by filename.
    ///
    /// This, in theory, means they're ordered by the timestamps in their names, provided the
    /// prefixes of all the filenames are the same.
    fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(PathBuf::as_path)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct WorkId(ThreadId);

#[derive(Debug, Default)]
struct Work {
    compression: HashMap<WorkId, Instrumented<JoinHandle<Result<(), CompressError>>>>,
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
        // Giving the ID to the work itself lets it notify us later that it's finished, so we can
        // remove the handle.
        let handle = thread::spawn(move || {
            let id = WorkId(thread::current().id());
            work(id)
        })
        .in_current_span();
        let id = WorkId(handle.inner().thread().id());
        self.compression.insert(id, handle);
        id
    }

    fn compression_finished(&mut self, work_id: WorkId) {
        self.compression.remove(&work_id);
    }

    fn drain_handles(&mut self) -> Vec<Instrumented<JoinHandle<Result<(), CompressError>>>> {
        self.compression.drain().map(|(_, handle)| handle).collect()
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        // FIXME: fix the serious deadlock here because we have a &mut self, but all the threads
        // will want to lock our mutex

        for (_, handle) in self.compression.drain() {
            // Don't unwrap the inner errors (the ones that actually come from the threads), just
            // log them so that we can continue to join on the other threads.
            //
            // We _do_ unwrap the outer errors, which will occur if the thread panicked, because
            // panics will nearly always be the result of poisoned mutexes in this crate, and we
            // should propagate those panics between threads.
            let _ = handle
                .into_inner()
                .join()
                .unwrap()
                .tap_err(|error| warn!(?error, "compression task failed"));
        }
    }
}

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

#[cfg(foooooooooooooooo)]
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
