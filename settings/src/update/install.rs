use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf, Prefix},
    process::{self, Command},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, FILETIME, HANDLE, HWND, LPARAM, WAIT_OBJECT_0,
            WPARAM,
        },
        System::{Diagnostics::ToolHelp::*, Threading::*},
        UI::{Shell::IsUserAnAdmin, WindowsAndMessaging::*},
    },
    core::{PCWSTR, PWSTR, w},
};

use super::feed::{self, Fingerprint, Release};

const LAUNCHER: &str = "wisp.exe";
const SETTINGS: &str = "wisp-settings.exe";
const HELPER: &str = "wisp-update-helper.exe";
const PROGRAMS: [&str; 2] = [LAUNCHER, SETTINGS];
const UPDATE_DIRECTORY: &str = ".wisp-update";
const APPLY: &str = "--apply-update";
const RECOVER: &str = "--recover-update";
const WAIT_MS: u32 = 15_000;
const REPARSE_POINT: u32 = 0x400;

pub fn background_allowed() -> Result<bool, String> {
    let install = current_install()?;
    let entries = installed_processes(&install)?;
    if other_settings_open(&entries, process::id()) {
        return Ok(false);
    }
    Ok(find_active_launcher(entries)?.is_some())
}

pub fn schedule(release: &Release, reopen_settings: bool) -> Result<(), String> {
    let install = current_install()?;
    let launcher = active_launcher(&install, process::id())?;
    if !reopen_settings && launcher.is_none() {
        return Err("Wisp closed before the automatic update could start.".into());
    }
    let workspace = Workspace::open(&install)?;
    let _lock = workspace.try_lock()?;
    if workspace.recovery_required() {
        return Err("An earlier update needs recovery. Close and reopen Wisp Settings to recover it.".into());
    }
    if installed_processes(&install)?.iter().any(|entry| entry.name == HELPER) {
        return Err("A Wisp update is already running.".into());
    }
    workspace.reset()?;
    feed::download(release, &workspace.directory)?;
    let fingerprints = release.fingerprints();
    feed::verify_files(&workspace.directory, &fingerprints)?;
    copy_synced(&install.join(SETTINGS), &workspace.directory.join(HELPER))?;
    let caller = Process::open(process::id()).map_err(|error| error.to_string())?;
    workspace.spawn_helper(
        APPLY,
        caller.identity()?,
        reopen_settings,
        &fingerprints_as_arguments(&fingerprints),
    )
}

pub fn dispatch() -> bool {
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    if matches!(command.as_deref().and_then(|arg| arg.to_str()), Some(APPLY | RECOVER)) {
        let apply = command.as_deref().is_some_and(|arg| arg == APPLY);
        let outcome = run_helper(apply, args.collect());
        if let Err((manual, error)) = outcome {
            report_failure(manual, &error);
        }
        return true;
    }
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let Some(install) = executable.parent() else {
        return false;
    };
    let directory = install.join(UPDATE_DIRECTORY);
    if !directory.join("prepared").exists() || directory.join("committed").exists() {
        return false;
    }
    let reopen_settings = command.as_deref().is_none_or(|arg| arg != "--update-background");
    let result: Result<bool, String> = (|| {
        let install = current_install()?;
        let workspace = Workspace::open(&install)?;
        let _lock = workspace.try_lock()?;
        if !workspace.recovery_required() {
            return Ok(false);
        }
        if installed_processes(&install)?.iter().any(|entry| entry.name == HELPER) {
            return Ok(true);
        }
        workspace.remove(HELPER)?;
        copy_synced(&install.join(SETTINGS), &workspace.directory.join(HELPER))?;
        let caller = Process::open(process::id()).map_err(|error| error.to_string())?;
        workspace.spawn_helper(RECOVER, caller.identity()?, reopen_settings, &[])?;
        Ok(true)
    })();
    match result {
        Ok(recovering) => recovering,
        Err(error) => {
            report_failure(reopen_settings, &error);
            true
        }
    }
}

fn run_helper(apply: bool, arguments: Vec<OsString>) -> Result<(), (bool, String)> {
    let manual = arguments.get(2).is_some_and(|arg| arg == "manual");
    let run = || {
        let args: Vec<_> = arguments
            .iter()
            .map(|argument| argument.to_str().ok_or("Wisp update request is invalid."))
            .collect::<Result<_, _>>()?;
        let [pid, created, mode, data @ ..] = args.as_slice() else {
            return Err("Wisp update request is incomplete.".into());
        };
        if !matches!(*mode, "manual" | "automatic") {
            return Err("Wisp update mode is invalid.".into());
        }
        let caller_id = (
            pid.parse::<u32>().map_err(|_| "Wisp update process ID is invalid.")?,
            created
                .parse::<u64>()
                .map_err(|_| "Wisp update process time is invalid.")?,
        );
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let install = executable
            .parent()
            .and_then(Path::parent)
            .ok_or("Cannot find the Wisp installation folder.")?;
        let install = validate_install(install)?;
        let workspace = Workspace::open(&install)?;
        if executable.canonicalize().map_err(|error| error.to_string())? != workspace.directory.join(HELPER) {
            return Err("The updater helper is outside this Wisp installation.".into());
        }

        let fingerprints = if apply {
            Some(parse_fingerprints(data)?)
        } else {
            if !data.is_empty() {
                return Err("Wisp recovery request is invalid.".into());
            }
            None
        };

        // Keep a handle open so a reused PID cannot be mistaken for the caller.
        // The caller may already have exited by the time Windows schedules us.
        let caller = match Process::open(caller_id.0) {
            Ok(caller) => {
                if caller.identity()? != caller_id {
                    return Err("Wisp could not verify the Settings process that requested this update.".into());
                }
                if caller.exited() {
                    None
                } else if caller
                    .matches_image(&install.join(SETTINGS))
                    .is_ok_and(|matches| matches)
                    || caller.exited()
                {
                    Some(caller)
                } else {
                    return Err("Wisp could not verify the Settings process that requested this update.".into());
                }
            }
            Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => None,
            Err(error) => return Err(format!("Cannot verify Wisp Settings before updating: {error}")),
        };
        let lock = workspace.wait_for_lock()?;
        let mut restart = false;
        let result = (|| {
            if let Some(caller) = caller {
                caller.wait_for_exit("Wisp Settings")?;
            }
            let launcher = active_launcher(&install, u32::MAX)?;
            if let Some(fingerprints) = fingerprints {
                if workspace.recovery_required() {
                    return Err("An earlier update needs recovery. Reopen Wisp Settings and try again.".into());
                }
                feed::verify_files(&workspace.directory, &fingerprints)?;
                if !manual && (launcher.is_none() || !super::background_enabled()?) {
                    return Err("Wisp is no longer available for automatic updates.".into());
                }
                if let Some(launcher) = launcher {
                    launcher.quit()?;
                    launcher.wait_for_exit()?;
                    restart = true;
                }
                workspace.prepare(restart)?;
                workspace.install_pair(
                    || check_stopped(&install),
                    || {
                        if restart {
                            start(&install, LAUNCHER, &["--skip-update-check"])
                        } else {
                            Ok(())
                        }
                    },
                )?;
                restart = false;
            } else if workspace.recovery_required() {
                // Make no changes while a launcher from either version is still open.
                check_stopped(&install)?;
                restart = workspace.prepared_restart()?;
                if let Err(error) = workspace.rollback(|| check_stopped(&install)) {
                    return Err(format!(
                        "Close Wisp and reopen Wisp Settings to retry recovery. {error}"
                    ));
                }
            }
            workspace.clean_finished();
            Ok(())
        })();
        let consistent = !workspace.recovery_required();
        drop(lock);

        let mut problems = Vec::new();
        if let Err(error) = result {
            problems.push(error);
        }
        if consistent && restart {
            // During a failed update the next automatic check should retain its normal cadence.
            if let Err(error) = start(&install, LAUNCHER, &["--skip-update-check"]) {
                problems.push(error);
            }
        }
        if consistent
            && manual
            && let Err(error) = start(&install, SETTINGS, &["--page", "about"])
        {
            problems.push(error);
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join(" "))
        }
    };
    run().map_err(|error| (manual, error))
}

fn report_failure(manual: bool, error: &str) {
    crate::logging::event("update.install.failed", format_args!("{error}"));
    if manual {
        let message: Vec<u16> = error.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(None, PCWSTR(message.as_ptr()), w!("Wisp update"), MB_OK | MB_ICONERROR);
        }
    }
}

fn start(install: &Path, name: &str, args: &[&str]) -> Result<(), String> {
    require_plain_file(&install.join(name))?;
    Command::new(install.join(name))
        .current_dir(install)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not reopen {name}. Open it from your Wisp folder. {error}"))
}

fn current_install() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    if executable
        .file_name()
        .is_none_or(|name| !name.eq_ignore_ascii_case(SETTINGS))
    {
        return Err("Open the installed wisp-settings.exe to update Wisp.".into());
    }
    validate_install(executable.parent().ok_or("Cannot find the Wisp installation folder.")?)
}

fn validate_install(path: &Path) -> Result<PathBuf, String> {
    if crate::channel::NIGHTLY || unsafe { IsUserAnAdmin().as_bool() } {
        return Err("Use a regular, non-administrator Wisp release to install updates from Settings.".into());
    }
    let profile = std::env::var_os("USERPROFILE").ok_or("Cannot find your Windows user folder.")?;
    let profile = PathBuf::from(profile);
    let local_path = |path: &Path| matches!(path.components().next(), Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)));
    let path = path
        .canonicalize()
        .map_err(|error| format!("Cannot open the Wisp installation folder: {error}"))?;
    let profile = profile
        .canonicalize()
        .map_err(|error| format!("Cannot open your Windows user folder: {error}"))?;
    if !local_path(&path) || !path.starts_with(&profile) || path == profile {
        return Err("Wisp updates need a local installation folder inside your Windows user folder.".into());
    }
    for name in PROGRAMS {
        require_plain_file(&path.join(name))?;
    }
    Ok(path)
}

fn require_plain_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Cannot access {}. Check that both Wisp executables are in the same folder. {error}",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;
    if !metadata.is_file() || metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err("The Wisp update requires ordinary files without redirects.".into());
    }
    Ok(())
}

struct Workspace {
    install: PathBuf,
    directory: PathBuf,
}

impl Workspace {
    fn open(install: &Path) -> Result<Self, String> {
        let directory = install.join(UPDATE_DIRECTORY);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "Wisp cannot stage the update in its installation folder: {error}"
                ));
            }
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
        if !metadata.is_dir() || metadata.file_attributes() & REPARSE_POINT != 0 {
            return Err("Wisp cannot use the update folder because it is redirected.".into());
        }
        Ok(Self {
            install: install.to_owned(),
            directory,
        })
    }

    fn lock(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(self.directory.join("lock"))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::other("the update lock is not an ordinary file"));
        }
        Ok(file)
    }

    fn try_lock(&self) -> Result<File, String> {
        self.lock()
            .map_err(|error| format!("Cannot lock the Wisp update folder. Another update may be running. {error}"))
    }

    fn wait_for_lock(&self) -> Result<File, String> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(WAIT_MS));
        loop {
            match self.lock() {
                Ok(lock) => return Ok(lock),
                Err(error) if error.raw_os_error() == Some(32) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(format!("Cannot lock the Wisp update folder: {error}")),
            }
        }
    }

    fn recovery_required(&self) -> bool {
        self.directory.join("prepared").exists() && !self.directory.join("committed").exists()
    }

    fn spawn_helper(&self, mode: &str, caller: (u32, u64), manual: bool, data: &[String]) -> Result<(), String> {
        let helper = self.directory.join(HELPER);
        require_plain_file(&helper)?;
        Command::new(helper)
            .current_dir(&self.install)
            .arg(mode)
            .arg(caller.0.to_string())
            .arg(caller.1.to_string())
            .arg(if manual { "manual" } else { "automatic" })
            .args(data)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Cannot start the Wisp updater. Close Wisp and retry Settings. {error}"))
    }

    fn reset(&self) -> Result<(), String> {
        self.remove("prepared")?;
        self.remove("committed")?;
        for name in [
            LAUNCHER,
            SETTINGS,
            HELPER,
            "old-wisp.exe",
            "old-wisp-settings.exe",
            "restore.tmp",
            "marker.tmp",
        ] {
            self.remove(name)?;
        }
        Ok(())
    }

    fn prepare(&self, restart: bool) -> Result<(), String> {
        for name in PROGRAMS {
            copy_synced(&self.install.join(name), &self.backup(name))?;
        }
        self.marker("prepared", if restart { b"restart" } else { b"stopped" })
    }

    fn prepared_restart(&self) -> Result<bool, String> {
        let prepared = self.directory.join("prepared");
        require_plain_file(&prepared)?;
        match fs::read(prepared).as_deref() {
            Ok(b"restart") => Ok(true),
            Ok(b"stopped") => Ok(false),
            _ => Err("Cannot read the Wisp recovery record.".into()),
        }
    }

    fn replace_pair(&self, verify_stopped: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        verify_stopped()?;
        // Replacing Settings first leaves the old, guarded launcher in place until both are available.
        for name in [SETTINGS, LAUNCHER] {
            replace(&self.directory.join(name), &self.install.join(name))?;
        }
        self.marker("committed", b"committed")
    }

    fn install_pair(
        &self,
        verify_stopped: impl Fn() -> Result<(), String>,
        restart: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if let Err(error) = self.replace_pair(&verify_stopped).and_then(|()| restart()) {
            // The launcher needs committed to start. Restore the startup guard
            // before rolling back a pair whose new launcher could not start.
            if let Err(rollback) = self.remove("committed").and_then(|()| self.rollback(verify_stopped)) {
                return Err(format!(
                    "{error}. Could not restore the previous version. Close Wisp and reopen Wisp Settings to retry recovery. {rollback}"
                ));
            }
            return Err(format!("{error}. Wisp restored the previous version."));
        }
        Ok(())
    }

    fn rollback(&self, verify_stopped: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        verify_stopped()?;
        for name in PROGRAMS {
            require_plain_file(&self.backup(name))?;
        }
        for name in [SETTINGS, LAUNCHER] {
            let temporary = self.directory.join("restore.tmp");
            self.remove("restore.tmp")?;
            // Keep both backups until recovery completes so another interruption is recoverable.
            copy_synced(&self.backup(name), &temporary)?;
            replace(&temporary, &self.install.join(name))?;
        }
        self.remove("prepared")
    }

    fn marker(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let temporary = self.directory.join("marker.tmp");
        let mut marker =
            File::create_new(&temporary).map_err(|error| format!("Cannot create update record: {error}"))?;
        marker
            .write_all(bytes)
            .and_then(|()| marker.sync_all())
            .map_err(|error| format!("Cannot save update record: {error}"))?;
        drop(marker);
        replace(&temporary, &self.directory.join(name))
    }

    fn backup(&self, name: &str) -> PathBuf {
        self.directory.join(format!("old-{name}"))
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        match fs::remove_file(self.directory.join(name)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Cannot clear update file {name}: {error}")),
        }
    }

    fn clean_finished(&self) {
        // A committed transaction becomes clean by removing prepared before committed.
        let result = self.remove("prepared").and_then(|()| self.remove("committed"));
        if result.is_ok() {
            for name in [
                LAUNCHER,
                SETTINGS,
                "old-wisp.exe",
                "old-wisp-settings.exe",
                "restore.tmp",
                "marker.tmp",
            ] {
                let _ = self.remove(name);
            }
        }
    }
}

fn copy_synced(source: &Path, destination: &Path) -> Result<(), String> {
    require_plain_file(source)?;
    let copy = || -> io::Result<()> {
        let mut source = File::open(source)?;
        let mut destination = File::create_new(destination)?;
        io::copy(&mut source, &mut destination)?;
        destination.sync_all()
    };
    copy().map_err(|error| {
        format!(
            "Cannot save a copy of {}: {error}",
            source.file_name().unwrap_or_default().to_string_lossy()
        )
    })
}

fn replace(source: &Path, destination: &Path) -> Result<(), String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: PCWSTR, destination: PCWSTR, flags: u32) -> i32;
    }
    require_plain_file(source)?;
    let name = destination.file_name().unwrap_or_default().to_string_lossy();
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<_> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    // Same-volume rename with MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH.
    if unsafe { MoveFileExW(PCWSTR(source.as_ptr()), PCWSTR(destination.as_ptr()), 0x9) } == 0 {
        return Err(format!("Cannot replace {name}: {}", io::Error::last_os_error()));
    }
    Ok(())
}

struct Process {
    id: u32,
    handle: HANDLE,
}

impl Process {
    fn open(id: u32) -> windows::core::Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, id)? };
        Ok(Self { id, handle })
    }

    fn identity(&self) -> Result<(u32, u64), String> {
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        unsafe { GetProcessTimes(self.handle, &mut created, &mut exited, &mut kernel, &mut user) }
            .map_err(|error| format!("Cannot identify the Wisp process: {error}"))?;
        Ok((
            self.id,
            u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime),
        ))
    }

    fn matches_image(&self, expected: &Path) -> Result<bool, String> {
        let image = self.image()?;
        let expected = expected
            .canonicalize()
            .map_err(|error| format!("Cannot open the installed Wisp executable: {error}"))?;
        Ok(image == expected)
    }

    fn image(&self) -> Result<PathBuf, String> {
        let mut path = vec![0u16; 32_768];
        let mut length = path.len() as u32;
        unsafe { QueryFullProcessImageNameW(self.handle, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut length) }
            .map_err(|error| format!("Cannot locate the running Wisp executable: {error}"))?;
        let path = PathBuf::from(OsString::from_wide(&path[..length as usize]));
        path.canonicalize()
            .map_err(|error| format!("Cannot open the running Wisp executable: {error}"))
    }

    fn wait_for_exit(&self, name: &str) -> Result<(), String> {
        self.wait_until(name, Instant::now() + Duration::from_millis(u64::from(WAIT_MS)))
    }

    fn wait_until(&self, name: &str, deadline: Instant) -> Result<(), String> {
        let remaining = deadline.saturating_duration_since(Instant::now()).as_millis() as u32;
        if unsafe { WaitForSingleObject(self.handle, remaining) } == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(format!(
                "{name} is still running. Close all Wisp windows before retrying the update."
            ))
        }
    }

    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

struct ProcessEntry {
    process: Process,
    parent_pid: u32,
    name: &'static str,
}

fn installed_processes(install: &Path) -> Result<Vec<ProcessEntry>, String> {
    let status = |error| format!("Cannot identify running Wisp processes: {error}");
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(status)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let entries = (|| {
        let mut entries = Vec::new();
        unsafe { Process32FirstW(snapshot, &mut entry) }.map_err(status)?;
        loop {
            let length = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let image = String::from_utf16_lossy(&entry.szExeFile[..length]);
            if let Some(name) = [LAUNCHER, SETTINGS, HELPER]
                .into_iter()
                .find(|name| image.eq_ignore_ascii_case(name))
            {
                // An inaccessible image cannot authorize shutdown or establish
                // a conflict. Windows will reject a swap if the file is in use.
                if let Ok(process) = Process::open(entry.th32ProcessID)
                    && let Ok(image) = process.image()
                    && is_installed_image(install, name, &image)
                {
                    entries.push(ProcessEntry {
                        process,
                        parent_pid: entry.th32ParentProcessID,
                        name,
                    });
                }
            }
            match unsafe { Process32NextW(snapshot, &mut entry) } {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(entries),
                Err(error) => return Err(status(error)),
            }
        }
    })();
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    entries
}

fn is_installed_image(install: &Path, name: &str, image: &Path) -> bool {
    let directory = if name == HELPER {
        install.join(UPDATE_DIRECTORY)
    } else {
        install.to_owned()
    };
    image.parent() == Some(&directory) && image.file_name().is_some_and(|file| file.eq_ignore_ascii_case(name))
}

struct ActiveLauncher {
    window: HWND,
    process: Process,
    children: Vec<Process>,
}

impl ActiveLauncher {
    fn quit(&self) -> Result<(), String> {
        if window_process(self.window) != self.process.id {
            return Err("The active Wisp process changed. Retry from Wisp Settings.".into());
        }
        // The Wisp lifecycle window sends WM_CLOSE and --quit through the same graceful shutdown.
        unsafe { PostMessageW(Some(self.window), WM_CLOSE, WPARAM(0), LPARAM(0)) }
            .map_err(|error| format!("Cannot ask Wisp to close for the update: {error}"))
    }

    fn wait_for_exit(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(WAIT_MS));
        self.process.wait_until("Wisp", deadline)?;
        for child in &self.children {
            child.wait_until("Wisp's taskbar watchdog", deadline)?;
        }
        Ok(())
    }
}

fn window_process(window: HWND) -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
    }
    pid
}

fn active_launcher(install: &Path, calling_settings: u32) -> Result<Option<ActiveLauncher>, String> {
    let entries = installed_processes(install)?;
    if other_settings_open(&entries, calling_settings) {
        return Err("Close the other Wisp Settings window before updating.".into());
    }
    find_active_launcher(entries)
}

fn other_settings_open(entries: &[ProcessEntry], caller: u32) -> bool {
    entries
        .iter()
        .any(|entry| entry.name == SETTINGS && entry.process.id != caller)
}

fn find_active_launcher(entries: Vec<ProcessEntry>) -> Result<Option<ActiveLauncher>, String> {
    let mut launchers = entries.into_iter().filter(|entry| entry.name == LAUNCHER).peekable();
    if launchers.peek().is_none() {
        return Ok(None);
    }
    let no_listener = || {
        "This Wisp installation is still running. Wait for it to finish starting or close it in any other Windows session before updating.".to_owned()
    };
    let Ok(window) = (unsafe { FindWindowW(crate::channel::LAUNCHER_CLASS, None) }) else {
        return Err(no_listener());
    };
    let launcher_pid = window_process(window);
    let mut process = None;
    let mut children = Vec::new();
    for entry in launchers {
        if entry.process.id != launcher_pid && entry.parent_pid != launcher_pid {
            return Err(no_listener());
        }
        if entry.process.id == launcher_pid {
            process = Some(entry.process);
        } else {
            // The running launcher starts its taskbar watchdog from the same executable.
            children.push(entry.process);
        }
    }
    let process = process.ok_or_else(no_listener)?;
    Ok(Some(ActiveLauncher {
        window,
        process,
        children,
    }))
}

fn check_stopped(install: &Path) -> Result<(), String> {
    if active_launcher(install, u32::MAX)?.is_some() {
        return Err("Wisp is still running. Close Wisp before retrying the update.".into());
    }
    Ok(())
}

fn fingerprints_as_arguments(fingerprints: &[Fingerprint; 2]) -> [String; 4] {
    let [launcher, settings] = fingerprints;
    let encode = |digest: &[u8; 32]| digest.iter().map(|byte| format!("{byte:02x}")).collect();
    [
        launcher.size.to_string(),
        encode(&launcher.digest),
        settings.size.to_string(),
        encode(&settings.digest),
    ]
}

fn parse_fingerprints(data: &[&str]) -> Result<[Fingerprint; 2], String> {
    let [launcher_size, launcher_digest, settings_size, settings_digest] = data else {
        return Err("Wisp update verification data is incomplete.".into());
    };
    let parse = |size: &str, hash: &str| -> Result<Fingerprint, String> {
        let invalid = "Wisp update verification data is invalid.";
        let size = size.parse().map_err(|_| invalid)?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid.into());
        }
        let mut digest = [0; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hash[index * 2..][..2], 16).map_err(|_| invalid)?;
        }
        Ok(Fingerprint { size, digest })
    };
    Ok([
        parse(launcher_size, launcher_digest)?,
        parse(settings_size, settings_digest)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        for name in PROGRAMS {
            fs::write(dir.path().join(name), format!("old {name}")).unwrap();
            fs::write(workspace.directory.join(name), format!("new {name}")).unwrap();
        }
        (dir, workspace)
    }

    #[test]
    fn restores_both_files_when_second_replacement_fails() {
        let (dir, workspace) = fixture();
        workspace.prepare(true).unwrap();
        fs::remove_file(workspace.directory.join(LAUNCHER)).unwrap();
        assert!(workspace.replace_pair(|| Ok(())).is_err());
        assert!(workspace.recovery_required());
        assert!(workspace.prepared_restart().unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(SETTINGS)).unwrap(),
            format!("new {SETTINGS}")
        );

        workspace.rollback(|| Err("Wisp is still running".into())).unwrap_err();
        assert!(workspace.recovery_required());
        workspace.rollback(|| Ok(())).unwrap();
        assert!(!workspace.recovery_required());
        for name in PROGRAMS {
            assert_eq!(
                fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("old {name}")
            );
        }
    }

    #[test]
    fn commit_and_cleanup_keep_both_new_files() {
        let (dir, workspace) = fixture();
        workspace.prepare(false).unwrap();
        workspace.replace_pair(|| Ok(())).unwrap();
        assert!(!workspace.recovery_required());
        workspace.clean_finished();
        for name in PROGRAMS {
            assert_eq!(
                fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("new {name}")
            );
            assert!(!workspace.backup(name).exists());
        }
    }

    #[test]
    fn a_failed_restart_restores_both_old_executables() {
        let (dir, workspace) = fixture();
        workspace.prepare(true).unwrap();
        let error = workspace
            .install_pair(
                || Ok(()),
                || {
                    assert!(!workspace.recovery_required());
                    assert_eq!(
                        fs::read_to_string(dir.path().join(LAUNCHER)).unwrap(),
                        format!("new {LAUNCHER}")
                    );
                    Err("Windows could not create the launcher process".into())
                },
            )
            .unwrap_err();
        assert!(error.contains("restored the previous version"));
        assert!(!workspace.recovery_required());
        for name in PROGRAMS {
            assert_eq!(
                fs::read_to_string(dir.path().join(name)).unwrap(),
                format!("old {name}")
            );
        }
    }

    #[test]
    fn missing_backup_aborts_recovery_before_overwriting_either_file() {
        let (dir, workspace) = fixture();
        workspace.prepare(false).unwrap();
        fs::remove_file(workspace.backup(LAUNCHER)).unwrap();
        fs::write(dir.path().join(SETTINGS), b"new settings").unwrap();
        assert!(workspace.rollback(|| Ok(())).is_err());
        assert!(workspace.recovery_required());
        assert_eq!(fs::read(dir.path().join(SETTINGS)).unwrap(), b"new settings");
    }

    #[test]
    fn transaction_lock_excludes_a_second_writer() {
        let (_dir, workspace) = fixture();
        let lock = workspace.try_lock().unwrap();
        assert!(workspace.try_lock().is_err());
        drop(lock);
        assert!(workspace.try_lock().is_ok());
    }

    #[test]
    fn inventory_matches_only_the_expected_executable_in_this_installation() {
        let install = Path::new(r"C:\Users\first\Programs\Wisp");
        let unrelated = Path::new(r"C:\Users\second\Programs\Wisp");
        for name in PROGRAMS {
            assert!(is_installed_image(install, name, &install.join(name)));
            assert!(!is_installed_image(install, name, &unrelated.join(name)));
        }
        let helper = Path::new(UPDATE_DIRECTORY).join(HELPER);
        assert!(is_installed_image(install, HELPER, &install.join(&helper)));
        assert!(!is_installed_image(install, HELPER, &unrelated.join(&helper)));
        assert!(!is_installed_image(install, HELPER, &install.join(HELPER)));
    }
}
