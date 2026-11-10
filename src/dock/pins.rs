//! Persistent app shortcuts in dock order.

use crate::indexer::AppEntry;
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Pins {
    pub apps: Vec<AppEntry>,
}

impl Pins {
    fn path() -> PathBuf {
        crate::util::data_dir().join("dock-pins.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.apps.iter().position(|app| app.id.eq_ignore_ascii_case(id))
    }

    pub fn toggle(&mut self, app: AppEntry) -> io::Result<()> {
        let mut apps = self.apps.clone();
        if let Some(index) = self.position(&app.id) {
            apps.remove(index);
        } else {
            apps.push(app);
        }
        self.save(apps)
    }

    pub fn reorder(&mut self, id: &str, target: &str) -> io::Result<()> {
        if let (Some(from), Some(to)) = (self.position(id), self.position(target))
            && from != to
        {
            let mut apps = self.apps.clone();
            let moved = apps.remove(from);
            apps.insert(to, moved);
            self.save(apps)?;
        }
        Ok(())
    }

    fn save(&mut self, apps: Vec<AppEntry>) -> io::Result<()> {
        self.save_to(apps, &Self::path())
    }

    fn save_to(&mut self, apps: Vec<AppEntry>, path: &Path) -> io::Result<()> {
        let next = Self { apps };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, toml::to_string(&next).map_err(io::Error::other)?)?;
        std::fs::rename(tmp, path)?;
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_order_replaces_previous_pins_and_survives_reload() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("wisp-dock-pins-{}", std::process::id()));
        std::fs::create_dir(&dir)?;
        let path = dir.join("dock-pins.toml");
        let app = |id: &str| AppEntry {
            id: id.into(),
            name: id.into(),
            path: None,
        };
        let mut pins = Pins::default();
        pins.save_to(vec![app("Editor"), app("Browser")], &path)?;
        pins.save_to(vec![app("Browser"), app("Editor")], &path)?;
        let reloaded = Pins::load_from(&path);
        std::fs::remove_file(&path)?;
        std::fs::remove_dir(dir)?;
        assert_eq!(reloaded.position("browser"), Some(0));
        assert_eq!(reloaded.position("editor"), Some(1));
        Ok(())
    }
}
