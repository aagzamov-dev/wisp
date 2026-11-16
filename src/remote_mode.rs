//! Shared choice only; the normal configuration is never rewritten.
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Auto,
    On,
    Off,
}
impl Mode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::On, Self::Off];
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::On => "On",
            Self::Off => "Off",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.name().eq_ignore_ascii_case(value.trim()))
    }
    pub fn active(self, rdp: bool) -> bool {
        match self {
            Self::Auto => rdp,
            Self::On => true,
            Self::Off => false,
        }
    }
}
pub fn load(path: &Path) -> io::Result<Mode> {
    let read = (|| {
        let mut value = String::new();
        fs::File::open(path)?.take(65).read_to_string(&mut value)?;
        if value.len() > 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Remote mode value is too long",
            ));
        }
        Ok(value)
    })();
    match read {
        Ok(value) => Mode::parse(&value)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Remote mode must be Auto, On or Off")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Mode::Auto),
        Err(error) => Err(error),
    }
}
pub fn save(path: &Path, mode: Mode) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing mode folder"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".remote-mode-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let result = (|| {
        writeln!(file, "{}", mode.name())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn choices_and_session_matrix() {
        for (mode, local, remote) in [
            (Mode::Auto, false, true),
            (Mode::On, true, true),
            (Mode::Off, false, false),
        ] {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
            assert_eq!(mode.active(false), local);
            assert_eq!(mode.active(true), remote);
        }
        assert_eq!(Mode::parse("  auto\n"), Some(Mode::Auto));
        assert_eq!(Mode::parse("enabled"), None);
    }
    #[test]
    fn persistence_and_failed_replace_preserve_old_choice() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("remote-mode-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("choice.txt");
        assert_eq!(load(&path).unwrap(), Mode::Auto);
        save(&path, Mode::On).unwrap();
        save(&path, Mode::Off).unwrap();
        assert_eq!(load(&path).unwrap(), Mode::Off);
        use std::os::windows::fs::OpenOptionsExt;
        let locked = fs::OpenOptions::new().read(true).share_mode(3).open(&path).unwrap();
        assert!(save(&path, Mode::On).is_err());
        drop(locked);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(load(&path).unwrap(), Mode::Off);
        fs::write(&path, "invalid").unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
