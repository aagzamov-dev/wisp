//! Bounded local diagnostics. Never initializes itself from a log call: tests
//! and library-like users write no profile files. No stdout interception,
//! uploaded telemetry, console allocation, search text or frame polling.

use std::{
    backtrace::Backtrace,
    cell::Cell,
    fmt::{self, Write as _},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const FILE_BYTES: u64 = 2 * 1024 * 1024;
const ROLE_FILES: usize = 5;
const RECORD_BYTES: usize = 64 * 1024;
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
static LOGGER: OnceLock<Logger> = OnceLock::new();
static HOOK_SET: AtomicBool = AtomicBool::new(false);
thread_local! {
    static WRITING: Cell<bool> = const { Cell::new(false) };
    static PANICKING: Cell<bool> = const { Cell::new(false) };
}

struct Entered(&'static std::thread::LocalKey<Cell<bool>>);
impl Entered {
    fn new(key: &'static std::thread::LocalKey<Cell<bool>>) -> Option<Self> {
        key.try_with(|busy| (!busy.replace(true)).then_some(Self(key)))
            .ok()
            .flatten()
    }
}
impl Drop for Entered {
    fn drop(&mut self) {
        let _ = self.0.try_with(|busy| busy.set(false));
    }
}

struct Limited {
    text: String,
    limit: usize,
    truncated: bool,
}
impl Limited {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            truncated: false,
        }
    }
    fn finish(mut self) -> String {
        if self.truncated {
            const MARK: &str = "\n[record truncated]";
            let mut end = self.text.len().min(self.limit.saturating_sub(MARK.len()));
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.truncate(end);
            self.text.push_str(MARK);
        }
        self.text.push('\n');
        self.text
    }
}
impl fmt::Write for Limited {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let remaining = self.limit.saturating_sub(self.text.len());
        let mut end = remaining.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.truncated |= end != text.len();
        // Formatting is best effort. Exhausting the record is not a fmt error
        // that a Display implementation might unwrap and turn into a panic.
        Ok(())
    }
}

fn timestamp(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs();
    let days = (seconds / 86_400) as i64;
    // Gregorian civil date from Unix days; independent of locale and timezone.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        elapsed.subsec_millis()
    )
}

fn record(level: &str, context: &str, args: fmt::Arguments<'_>) -> String {
    let mut text = Limited::new(RECORD_BYTES - 1);
    let _ = write!(
        text,
        "{} pid={} {level} {context}: {args}",
        timestamp(SystemTime::now()),
        std::process::id()
    );
    text.finish()
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ: readers may inspect diagnostics; rotation must not
        // unlink an active writer from another launcher/watchdog process.
        options.share_mode(1);
    }
    options
}

fn rotation_lock(directory: &Path) -> io::Result<File> {
    let mut options = options();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    // OS handle exclusivity, not a create_new sentinel. An abort releases the
    // lock even though this harmless empty file remains on disk.
    options
        .create(true)
        .truncate(false)
        .open(directory.join(".rotation.lock"))
}

struct Sink {
    directory: PathBuf,
    role: String,
    session: String,
    file: Option<File>,
    bytes: u64,
    file_limit: u64,
    file_count: usize,
    header: String,
}
impl Sink {
    fn new(directory: &Path, role: &str, session: &str, file_limit: u64, file_count: usize) -> io::Result<Self> {
        if role.is_empty()
            || !role.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || session.is_empty()
            || !session.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || file_limit == 0
            || file_count == 0
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid diagnostic sink"));
        }
        fs::create_dir_all(directory)?;
        let mut sink = Self {
            directory: directory.into(),
            role: role.into(),
            session: session.into(),
            file: None,
            bytes: 0,
            file_limit,
            file_count,
            header: String::new(),
        };
        sink.rotate()?;
        Ok(sink)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Release our old file so the quota can reclaim it if every other
        // retained file is still active. Other processes keep their files.
        self.file.take();
        let _lock = rotation_lock(&self.directory)?;
        let prefix = format!("wisp-{}-", self.role);
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(".log") && entry.file_type()?.is_file() {
                files.push(entry.path());
            }
        }
        files.sort();
        let mut retained = files.len();
        for path in files {
            if retained < self.file_count {
                break;
            }
            // Active Windows files deny deletion. Keep looking for an inactive
            // older segment; never exceed the quota to work around a failure.
            if fs::remove_file(path).is_ok() {
                retained -= 1;
            }
        }
        if retained >= self.file_count {
            return Err(io::Error::other("diagnostic file quota is occupied"));
        }
        let index = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = self.directory.join(format!("{prefix}{}-{index:012}.log", self.session));
        self.file = Some(options().create_new(true).open(path)?);
        self.bytes = 0;
        if !self.header.is_empty() {
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| io::Error::other("diagnostic sink unavailable"))?;
            self.bytes = self.header.len() as u64;
            file.write_all(self.header.as_bytes())?;
            file.flush()?;
        }
        Ok(())
    }

    fn set_header(&mut self, header: String) -> io::Result<()> {
        if header.len() as u64 >= self.file_limit {
            return Err(io::Error::other("diagnostic header exceeds file limit"));
        }
        self.append(&header, false)?;
        self.header = header;
        Ok(())
    }

    fn append(&mut self, text: &str, durable: bool) -> io::Result<()> {
        if text.len() as u64 + self.header.len() as u64 > self.file_limit {
            return Err(io::Error::other("diagnostic record exceeds file limit"));
        }
        if self.file.is_none() || self.bytes.saturating_add(text.len() as u64) > self.file_limit {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("diagnostic sink unavailable"))?;
        // Count attempted bytes conservatively even after a partial I/O error.
        self.bytes = self.bytes.saturating_add(text.len() as u64);
        file.write_all(text.as_bytes())?;
        file.flush()?;
        if durable {
            file.sync_all()?;
        }
        Ok(())
    }
}

struct Logger {
    normal: Mutex<Sink>,
    // A preopened independent file lets the abort hook save a durable record
    // even when a different thread currently owns the normal writer mutex.
    critical: Mutex<Sink>,
}
impl Logger {
    fn write(&self, text: &str, critical: bool) {
        let mutex = if critical { &self.critical } else { &self.normal };
        if critical {
            match mutex.try_lock() {
                Ok(mut sink) => {
                    let _ = sink.append(text, true);
                }
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    let _ = error.into_inner().append(text, true);
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
        } else {
            match mutex.lock() {
                Ok(mut sink) => {
                    let _ = sink.append(text, false);
                }
                Err(error) => {
                    let _ = error.into_inner().append(text, false);
                }
            }
        }
    }
}

/// Called once at process entry, before watchdog dispatch or any native work.
/// Missing/unwritable APPDATA disables file diagnostics without affecting Wisp.
pub fn init(role: &str) -> io::Result<PathBuf> {
    install_hook();
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is unavailable"))?;
    let directory = base.join(crate::channel::DATA_DIR).join("logs");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let session = format!(
        "{:020}-{:09}-{:010}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id()
    );
    if LOGGER.get().is_none() {
        let mut normal = Sink::new(&directory, role, &session, FILE_BYTES, ROLE_FILES)?;
        let mut critical = Sink::new(&directory, role, &session, FILE_BYTES, ROLE_FILES)?;
        let image = std::env::current_exe().ok();
        let metadata = image.as_ref().and_then(|path| fs::metadata(path).ok());
        let mut header = Limited::new(4096);
        let _ = write!(
            header,
            "{} pid={} INFO session.header: role={role} session={session} version={} build={} debug={} arch={}\nimage={} bytes={} modified={}\n",
            timestamp(SystemTime::now()),
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
            option_env!("WISP_BUILD_ID").unwrap_or("unlabelled"),
            cfg!(debug_assertions),
            std::env::consts::ARCH,
            image
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown".into()),
            metadata.as_ref().map_or(0, |m| m.len()),
            metadata
                .and_then(|m| m.modified().ok())
                .map(timestamp)
                .unwrap_or_else(|| "unknown".into())
        );
        let header = header.finish();
        normal.set_header(header.clone())?;
        critical.set_header(header)?;
        let _ = LOGGER.set(Logger {
            normal: Mutex::new(normal),
            critical: Mutex::new(critical),
        });
    }
    event(
        "session.start",
        format_args!(
            "role={role} session={session} version={} build={} debug={} arch={}",
            env!("CARGO_PKG_VERSION"),
            option_env!("WISP_BUILD_ID").unwrap_or("unlabelled"),
            cfg!(debug_assertions),
            std::env::consts::ARCH
        ),
    );
    if let Ok(path) = std::env::current_exe() {
        let metadata = fs::metadata(&path).ok();
        event(
            "session.image",
            format_args!(
                "path={} bytes={} modified={}",
                path.display(),
                metadata.as_ref().map_or(0, |m| m.len()),
                metadata
                    .and_then(|m| m.modified().ok())
                    .map(timestamp)
                    .unwrap_or_else(|| "unknown".into())
            ),
        );
    }
    Ok(directory)
}

fn emit(level: &str, context: &str, args: fmt::Arguments<'_>) {
    let Some(_entered) = Entered::new(&WRITING) else {
        return;
    };
    let text = record(level, context, args);
    if let Some(logger) = LOGGER.get() {
        logger.write(&text, false);
    }
    #[cfg(debug_assertions)]
    {
        let _ = io::stderr().write_all(text.as_bytes());
    }
}
pub fn diagnostic(context: &str, args: fmt::Arguments<'_>) {
    emit("DIAG", context, args);
}
pub fn event(context: &str, args: fmt::Arguments<'_>) {
    emit("INFO", context, args);
}
pub fn error(context: &str, args: fmt::Arguments<'_>) {
    emit("ERROR", context, args);
}

fn install_hook() {
    if HOOK_SET.swap(true, Ordering::AcqRel) {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let Some(_entered) = Entered::new(&PANICKING) else {
            return;
        };
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        let mut bounded_payload = Limited::new(4096);
        let _ = bounded_payload.write_str(payload);
        let text = record(
            "PANIC",
            "unhandled",
            format_args!(
                "location={} payload={}\nforced_backtrace:\n{}",
                info.location()
                    .map(|location| location.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                bounded_payload.finish().trim_end(),
                Backtrace::force_capture()
            ),
        );
        if let Some(logger) = LOGGER.get() {
            logger.write(&text, true);
        }
        #[cfg(debug_assertions)]
        {
            let _ = io::stderr().write_all(text.as_bytes());
        }
        // Preserve release panic=abort. No recovery/unwind wrapper, restart or
        // retry policy is introduced; this hook returns after flush+sync.
    }));
}

pub fn finish(outcome: &'static str) {
    event("session.end", format_args!("outcome={outcome}"));
    if let Some(logger) = LOGGER.get()
        && let Ok(mut sink) = logger.normal.lock()
        && let Some(file) = sink.file.as_mut()
    {
        let _ = file.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let parent = std::env::temp_dir().canonicalize().unwrap();
            let path = parent.join(format!(
                "wisp-log-unit-{}-{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn texts(&self) -> Vec<String> {
            fs::read_dir(&self.0)
                .unwrap()
                .filter_map(|entry| {
                    let path = entry.ok()?.path();
                    (path.extension().and_then(|extension| extension.to_str()) == Some("log"))
                        .then(|| fs::read_to_string(path).unwrap())
                })
                .collect()
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            // Only this explicitly created, canonical-temp child is removed.
            if self.0.parent() == std::env::temp_dir().canonicalize().ok().as_deref()
                && self
                    .0
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("wisp-log-unit-"))
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn test_logger(path: &Path) -> Logger {
        Logger {
            normal: Mutex::new(Sink::new(path, "test", "00000000000000000001", 1024 * 1024, 5).unwrap()),
            critical: Mutex::new(Sink::new(path, "test", "00000000000000000001", 1024 * 1024, 5).unwrap()),
        }
    }

    #[test]
    fn utc_timestamp_handles_epoch_and_leap_day() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            timestamp(UNIX_EPOCH + std::time::Duration::from_millis(1_709_251_199_123)),
            "2024-02-29T23:59:59.123Z"
        );
    }

    #[test]
    fn oversized_multibyte_records_are_bounded_without_broken_utf8() {
        let text = record("ERROR", "unit", format_args!("{}", "界".repeat(RECORD_BYTES)));
        assert!(text.len() <= RECORD_BYTES);
        assert!(text.ends_with("[record truncated]\n"));
        assert!(text.contains("ERROR unit:"));
    }

    #[test]
    fn concurrent_multiline_records_are_atomic_and_complete() {
        let directory = TestDirectory::new();
        let logger = Arc::new(test_logger(&directory.0));
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let logger = logger.clone();
                std::thread::spawn(move || {
                    for sequence in 0..40 {
                        logger.write(
                            &format!("BEGIN {thread}:{sequence}\nframe one\nEND {thread}:{sequence}\n"),
                            false,
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        drop(logger);
        let text = directory.texts().join("");
        assert_eq!(text.matches("BEGIN ").count(), 320);
        for thread in 0..8 {
            for sequence in 0..40 {
                assert!(text.contains(&format!(
                    "BEGIN {thread}:{sequence}\nframe one\nEND {thread}:{sequence}\n"
                )));
            }
        }
    }

    #[test]
    fn rotation_is_bounded_and_preserves_other_roles_and_unrelated_files() {
        let directory = TestDirectory::new();
        fs::write(directory.0.join("user-notes.txt"), "keep").unwrap();
        let mut watchdog = Sink::new(&directory.0, "watchdog", "00000000000000000001", 64, 3).unwrap();
        watchdog.append("watchdog event\n", false).unwrap();
        let mut sink = Sink::new(&directory.0, "launcher", "00000000000000000001", 64, 3).unwrap();
        for index in 0..30 {
            sink.append(&format!("record={index:02} {}\n", "x".repeat(40)), false)
                .unwrap();
        }
        let files: Vec<_> = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let launcher_files: Vec<_> = files
            .iter()
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("wisp-launcher-")
            })
            .collect();
        assert_eq!(launcher_files.len(), 3);
        assert!(
            launcher_files
                .iter()
                .all(|path| fs::metadata(path).unwrap().len() <= 64)
        );
        assert_eq!(fs::read_to_string(directory.0.join("user-notes.txt")).unwrap(), "keep");
        assert!(directory.texts().iter().any(|text| text.contains("watchdog event")));
        drop(sink);
        drop(watchdog);
    }

    #[test]
    fn every_rotated_and_reserved_file_keeps_build_and_session_metadata() {
        let directory = TestDirectory::new();
        let mut normal = Sink::new(&directory.0, "test", "session", 96, 4).unwrap();
        let mut critical = Sink::new(&directory.0, "test", "session", 96, 4).unwrap();
        normal.set_header("build=test session=unit\n".into()).unwrap();
        critical.set_header("build=test session=unit\n".into()).unwrap();
        for _ in 0..12 {
            normal
                .append("multiline record\nframe one\nframe two\n", false)
                .unwrap();
        }
        assert!(
            directory
                .texts()
                .iter()
                .all(|text| text.starts_with("build=test session=unit\n"))
        );
        assert!(directory.texts().iter().all(|text| text.len() <= 96));
    }

    #[test]
    fn filesystem_errors_and_rotation_lock_contention_return_without_recursion() {
        let directory = TestDirectory::new();
        let not_directory = directory.0.join("file");
        fs::write(&not_directory, "not a folder").unwrap();
        assert!(Sink::new(&not_directory, "test", "session", 64, 3).is_err());
        let mut sink = Sink::new(&directory.0, "test", "session", 64, 3).unwrap();
        assert!(sink.append(&"x".repeat(65), false).is_err());
        let lock = rotation_lock(&directory.0).unwrap();
        #[cfg(windows)]
        {
            assert!(Sink::new(&directory.0, "test", "contended", 64, 3).is_err());
        }
        drop(lock);
        assert!(Sink::new(&directory.0, "test", "after-lock-drop", 64, 3).is_ok());
    }

    #[test]
    fn panic_sink_writes_durably_while_normal_writer_is_locked() {
        let directory = TestDirectory::new();
        let logger = test_logger(&directory.0);
        let normal = logger.normal.lock().unwrap();
        logger.write("PANIC unit\nforced_backtrace:\nframe zero\n", true);
        assert!(
            directory
                .texts()
                .iter()
                .any(|text| text.contains("PANIC unit\nforced_backtrace:\nframe zero\n"))
        );
        drop(normal);
    }

    #[test]
    fn reentrant_and_poisoned_writers_do_not_deadlock_or_panic_again() {
        let outer = Entered::new(&WRITING).unwrap();
        assert!(Entered::new(&WRITING).is_none());
        assert!(Entered::new(&PANICKING).is_some());
        drop(outer);
        assert!(Entered::new(&WRITING).is_some());
        let directory = TestDirectory::new();
        let logger = Arc::new(test_logger(&directory.0));
        let poisoned = logger.clone();
        // Only an isolated Rust test thread unwinds; release still aborts.
        let _ = std::thread::spawn(move || {
            let _lock = poisoned.normal.lock().unwrap();
            panic!("poison only the explicit temporary test sink");
        })
        .join();
        logger.write("after poisoned writer\n", false);
        assert!(
            directory
                .texts()
                .iter()
                .any(|text| text.contains("after poisoned writer"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn active_process_files_cannot_be_pruned_to_exceed_quota() {
        let directory = TestDirectory::new();
        let first = Sink::new(&directory.0, "test", "first", 64, 2).unwrap();
        let second = Sink::new(&directory.0, "test", "second", 64, 2).unwrap();
        assert!(Sink::new(&directory.0, "test", "third", 64, 2).is_err());
        drop(first);
        assert!(Sink::new(&directory.0, "test", "third", 64, 2).is_ok());
        drop(second);
        assert!(directory.texts().len() <= 2);
    }
}
