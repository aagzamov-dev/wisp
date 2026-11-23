//! Read the latest config before each edit, then replace it atomically.

use std::{io::Write, path::PathBuf};

use toml_edit::{DocumentMut, Item, Value};

/// The launcher creates its config from the same file.
pub const DEFAULT_CONFIG: &str = include_str!("../../src/default-config.toml");
/// Values for missing keys that differ from the default config. The launcher
/// reads the same file.
const MISSING_KEYS: &str = include_str!("../../src/config-0.3.toml");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub hotkey: String,
    pub quit_hotkey: String,
    pub max_results: i64,
    pub theme: String,
    pub backdrop: String,
    pub backdrop_color: String,
    pub backdrop_transparency: Option<i64>,
    pub backdrop_blur: Option<bool>,
    pub win_start: bool,
    pub pause_on_rdp: bool,
    pub automatic_updates: bool,
    pub music_notch: bool,
    pub music_excluded_apps: Vec<String>,
    pub topbar: bool,
    pub topbar_island_blur: bool,
    pub topbar_height: i64,
    pub topbar_opacity: i64,
    pub topbar_island_opacity: i64,
    pub topbar_monitors: Vec<String>,
    pub show_recent: bool,
    pub file_search: bool,
    pub file_search_exclude: Vec<String>,
    pub position: String,
    pub start_position: String,
    pub dock: bool,
    pub dock_autohide: bool,
    pub dock_reveal_height: i64,
    pub dock_smart_hide: bool,
    pub dock_current_monitor_only: bool,
    pub dock_preview_current_monitor_only: bool,
    pub dock_icon_size: i64,
    pub dock_open_ms: i64,
    pub dock_close_ms: i64,
    pub dock_animation: String,
    pub hide_taskbar: bool,
    pub window_list: String,
}

impl Default for Config {
    fn default() -> Self {
        Self::from_document(&DEFAULT_CONFIG.parse().expect("valid default config")).expect("valid default values")
    }
}

impl Config {
    pub fn material_transparency(&self) -> i64 {
        self.backdrop_transparency
            .unwrap_or(if self.backdrop == "glass" { 36 } else { 85 })
    }
    pub fn material_blur(&self) -> bool {
        self.backdrop_blur.unwrap_or(self.backdrop == "glass")
    }

    fn from_document(doc: &DocumentMut) -> Result<Self, String> {
        let defaults: DocumentMut = DEFAULT_CONFIG.parse().expect("valid default config");
        let missing: DocumentMut = MISSING_KEYS.parse().expect("valid missing keys");
        let get = |key: &str| doc.get(key).or_else(|| missing.get(key)).unwrap_or(&defaults[key]);
        let string = |key: &str| {
            get(key)
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must be a string."))
        };
        let boolean = |key: &str| {
            get(key)
                .as_bool()
                .ok_or_else(|| format!("{key} must be true or false."))
        };
        let theme = string("theme")?;
        choice(&theme, &["system", "dark", "light"], "theme")?;
        choice(
            &string("dock_animation")?,
            &["slide", "fade", "rise-fade", "scale-fade"],
            "dock_animation",
        )?;
        let backdrop = string("backdrop")?;
        choice(
            &backdrop,
            &[
                "acrylic",
                "mica",
                "solid",
                "transparent",
                "glass",
                "liquid-glass",
                "color",
            ],
            "backdrop",
        )?;
        let backdrop_color = string("backdrop_color")?;
        if !valid_color(&backdrop_color) {
            return Err("Enter a color as #RRGGBB.".into());
        }
        let position = string("position")?;
        choice(&position, &["center", "bottom"], "position")?;
        let start_position = string("start_position")?;
        choice(&start_position, &["center", "bottom"], "start_position")?;
        let window_list = string("window_list")?;
        choice(&window_list, &["always", "multiple", "off"], "window_list")?;
        let dock_icon_size = get("dock_icon_size")
            .as_integer()
            .filter(|value| *value >= 0)
            .ok_or("dock_icon_size must be a positive whole number of DIP.")?
            .clamp(24, 40);
        let duration = |key| {
            get(key)
                .as_integer()
                .filter(|value| (0..=i64::from(u32::MAX)).contains(value))
                .map(|value| value.min(500))
                .ok_or_else(|| format!("{key} must be a non-negative whole number of milliseconds."))
        };
        let max_results = get("max_results")
            .as_integer()
            .filter(|value| *value >= 0)
            .ok_or("max_results must be a positive whole number.")?
            .clamp(1, 20);
        let file_search_exclude = get("file_search_exclude")
            .as_array()
            .ok_or("file_search_exclude must be a list of folder names.")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "Each excluded folder name must be a string.".into())
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            hotkey: string("hotkey")?,
            quit_hotkey: string("quit_hotkey")?,
            max_results,
            theme,
            backdrop,
            backdrop_color,
            backdrop_transparency: doc
                .get("backdrop_transparency")
                .map(|item| bounded_integer(item, "backdrop_transparency", 0, 100))
                .transpose()?,
            backdrop_blur: doc
                .get("backdrop_blur")
                .map(|item| {
                    item.as_bool()
                        .ok_or_else(|| "backdrop_blur must be true or false.".to_string())
                })
                .transpose()?,
            win_start: boolean("win_start")?,
            pause_on_rdp: boolean("pause_on_rdp")?,
            automatic_updates: boolean("automatic_updates")?,
            music_notch: boolean("music_notch")?,
            music_excluded_apps: get("music_excluded_apps")
                .as_array()
                .ok_or("Excluded media apps must be a list.")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "Excluded media apps must be text.".to_string())
                })
                .collect::<Result<_, _>>()?,
            topbar: boolean("topbar")?,
            topbar_island_blur: boolean("topbar_island_blur")?,
            topbar_height: bounded_integer(get("topbar_height"), "topbar_height", 24, 48)?,
            topbar_opacity: bounded_integer(get("topbar_opacity"), "topbar_opacity", 0, 100)?,
            topbar_island_opacity: bounded_integer(get("topbar_island_opacity"), "topbar_island_opacity", 0, 100)?,
            topbar_monitors: get("topbar_monitors")
                .as_array()
                .ok_or("Choose displays for the top bar.")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "Display names must be text.".to_string())
                })
                .collect::<Result<_, _>>()?,
            show_recent: boolean("show_recent")?,
            file_search: boolean("file_search")?,
            file_search_exclude,
            position,
            start_position,
            dock: boolean("dock")?,
            dock_autohide: boolean("dock_autohide")?,
            dock_reveal_height: bounded_integer(get("dock_reveal_height"), "dock_reveal_height", 1, 128)?,
            dock_current_monitor_only: boolean("dock_current_monitor_only")?,
            dock_preview_current_monitor_only: boolean("dock_preview_current_monitor_only")?,
            dock_smart_hide: boolean("dock_smart_hide")?,
            dock_icon_size,
            dock_open_ms: duration("dock_open_ms")?,
            dock_close_ms: duration("dock_close_ms")?,
            dock_animation: string("dock_animation")?,
            hide_taskbar: boolean("hide_taskbar")?,
            window_list,
        })
    }
}

fn valid_color(color: &str) -> bool {
    color
        .strip_prefix('#')
        .is_some_and(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn bounded_integer(item: &Item, key: &str, min: i64, max: i64) -> Result<i64, String> {
    item.as_integer()
        .filter(|n| *n >= 0)
        .map(|n| n.clamp(min, max))
        .ok_or_else(|| format!("{key} must be a non-negative whole number."))
}

fn choice(value: &str, choices: &[&str], key: &str) -> Result<(), String> {
    if choices.contains(&value) {
        Ok(())
    } else {
        Err(format!("{key} must be {}.", choices.join(", ")))
    }
}

pub fn path() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|root| PathBuf::from(root).join(crate::channel::DATA_DIR).join("config.toml"))
        .ok_or_else(|| "Windows did not provide the AppData folder.".into())
}

pub struct Store {
    pub path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn document(&self) -> Result<DocumentMut, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::remote_mode::first_run_config(DEFAULT_CONFIG, &self.path).into_owned()
            }
            Err(error) => return Err(format!("Cannot read config.toml: {error}")),
        };
        text.parse()
            .map_err(|error| format!("Cannot parse config.toml: {error}"))
    }

    pub fn load(&self) -> Result<Config, String> {
        self.config(&self.document()?)
    }

    fn config(&self, doc: &DocumentMut) -> Result<Config, String> {
        let mut config = Config::from_document(doc)?;
        if !doc.contains_key("pause_on_rdp") && crate::remote_mode::legacy_opt_out(&self.path) {
            config.pause_on_rdp = false;
        }
        Ok(config)
    }

    /// Create the default file only when no config exists.
    pub fn ensure_exists(&self) -> Result<(), String> {
        if self.path.exists() {
            return Ok(());
        }
        let parent = self.path.parent().ok_or("The config path has no folder.")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        let defaults = crate::remote_mode::first_run_config(DEFAULT_CONFIG, &self.path);
        file.write_all(defaults.as_bytes())
            .and_then(|()| file.as_file().sync_all())
            .map_err(|error| error.to_string())?;
        match file.persist_noclobber(&self.path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(format!("Cannot create config.toml: {}", error.error)),
        }
    }

    pub fn set(&self, key: &str, mut value: Value) -> Result<Config, String> {
        validate_edit(key, &value)?;
        let mut doc = self.document()?;
        // Refuse to replace a config whose existing fields have invalid types.
        Config::from_document(&doc)?;
        if let Some(old) = doc.get(key).and_then(Item::as_value) {
            if let (Some(previous), Some(next)) = (old.as_array(), value.as_array_mut()) {
                // Keep comments and quoting on folder names that remain in the list.
                for entry in next.iter_mut() {
                    if let Some(existing) = previous.iter().find(|existing| existing.as_str() == entry.as_str()) {
                        *entry = existing.clone();
                    }
                }
                next.set_trailing(previous.trailing().clone());
                next.set_trailing_comma(previous.trailing_comma());
            }
            *value.decor_mut() = old.decor().clone();
        }
        doc[key] = Item::Value(value);
        let config = self.config(&doc)?;
        match key {
            "hotkey" => validate_shortcut(&config.hotkey, &config.quit_hotkey)?,
            "quit_hotkey" => validate_shortcut(&config.quit_hotkey, &config.hotkey)?,
            _ => {}
        }
        let parent = self.path.parent().ok_or("The config path has no folder.")?;
        std::fs::create_dir_all(parent).map_err(|error| format!("Cannot create the config folder: {error}"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        file.write_all(doc.to_string().as_bytes())
            .and_then(|()| file.as_file().sync_all())
            .map_err(|error| format!("Cannot write config.toml: {error}"))?;
        // persist replaces the destination on Windows without a delete-then-rename gap.
        file.persist(&self.path)
            .map_err(|error| format!("Cannot replace config.toml: {}", error.error))?;
        Ok(config)
    }
}

pub fn validate_shortcut(spec: &str, other: &str) -> Result<(), String> {
    let shortcut = crate::hotkey::parse(spec)?;
    if crate::hotkey::parse(other).is_ok_and(|candidate| candidate == shortcut) {
        return Err("Choose different shortcuts for the launcher and Quit Wisp.".into());
    }
    Ok(())
}

fn validate_edit(key: &str, value: &Value) -> Result<(), String> {
    match key {
        "hotkey" | "quit_hotkey" => {
            crate::hotkey::parse(value.as_str().ok_or("Shortcut must be text.")?)?;
        }
        "max_results" if !value.as_integer().is_some_and(|value| (1..=20).contains(&value)) => {
            return Err("Choose between 1 and 20 results.".into());
        }
        "position" if !matches!(value.as_str(), Some("center" | "bottom")) => {
            return Err("position must be center or bottom.".into());
        }
        "dock_icon_size" if !value.as_integer().is_some_and(|size| (24..=40).contains(&size)) => {
            return Err("Choose an app icon size between 24 and 40 DIP.".into());
        }
        "dock_reveal_height" if !value.as_integer().is_some_and(|height| (1..=128).contains(&height)) => {
            return Err("Choose a reveal strip height between 1 and 128 physical pixels.".into());
        }
        "dock_open_ms" | "dock_close_ms" if !value.as_integer().is_some_and(|ms| (0..=500).contains(&ms)) => {
            return Err("Choose an animation time between 0 and 500 ms.".into());
        }
        "backdrop_color" if !value.as_str().is_some_and(valid_color) => {
            return Err("Enter a color as #RRGGBB.".into());
        }
        "topbar_height" if !value.as_integer().is_some_and(|height| (24..=48).contains(&height)) => {
            return Err("Choose a top bar height between 24 and 48 px.".into());
        }
        "backdrop_transparency" | "topbar_opacity" | "topbar_island_opacity"
            if !value.as_integer().is_some_and(|opacity| (0..=100).contains(&opacity)) =>
        {
            return Err("Choose an opacity between 0 and 100%.".into());
        }
        "theme"
        | "topbar"
        | "topbar_island_blur"
        | "topbar_height"
        | "topbar_opacity"
        | "topbar_island_opacity"
        | "topbar_monitors"
        | "backdrop_color"
        | "backdrop_transparency"
        | "backdrop_blur"
        | "win_start"
        | "pause_on_rdp"
        | "automatic_updates"
        | "music_notch"
        | "music_excluded_apps"
        | "backdrop"
        | "max_results"
        | "show_recent"
        | "file_search"
        | "file_search_exclude"
        | "position"
        | "start_position"
        | "dock"
        | "dock_autohide"
        | "dock_reveal_height"
        | "dock_current_monitor_only"
        | "dock_preview_current_monitor_only"
        | "dock_smart_hide"
        | "dock_icon_size"
        | "dock_open_ms"
        | "dock_close_ms"
        | "dock_animation"
        | "hide_taskbar" => {}
        "window_list" if !matches!(value.as_str(), Some("always" | "multiple" | "off")) => {
            return Err("window_list must be always, multiple or off.".into());
        }
        "window_list" => {}
        _ => return Err(format!("Unknown setting: {key}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn automatic_update_preference_is_off_until_saved() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("config.toml"));
        std::fs::write(&store.path, "theme = 'light'\n").unwrap();
        assert!(!store.load().unwrap().automatic_updates);
        assert!(store.set("automatic_updates", true.into()).unwrap().automatic_updates);
        assert!(store.load().unwrap().automatic_updates);
        assert!(store.set("automatic_updates", "true".into()).is_err());
        assert!(!store.set("automatic_updates", false.into()).unwrap().automatic_updates);
    }

    #[test]
    fn remote_pause_uses_legacy_off_until_the_new_preference_is_saved() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().join("config.toml"));
        std::fs::write(&store.path, "theme = 'light'\n").unwrap();
        let legacy = directory.path().join("remote-mode.txt");
        std::fs::write(&legacy, "Off\n").unwrap();

        assert!(!store.load().unwrap().pause_on_rdp);
        assert!(!store.set("theme", "dark".into()).unwrap().pause_on_rdp);
        assert!(store.set("pause_on_rdp", true.into()).unwrap().pause_on_rdp);
        assert!(store.load().unwrap().pause_on_rdp);
        assert!(store.set("pause_on_rdp", "false".into()).is_err());
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "Off\n");

        std::fs::remove_file(&store.path).unwrap();
        store.ensure_exists().unwrap();
        assert!(!store.load().unwrap().pause_on_rdp);
    }

    #[test]
    fn shared_material_roundtrip_preserves_missing_defaults_comments_and_unknown_fields() {
        for mode in ["transparent", "glass"] {
            let old = Config::from_document(&format!("backdrop='{mode}'").parse().unwrap()).unwrap();
            assert_eq!(old.backdrop_transparency, None);
            assert_eq!(old.backdrop_blur, None);
            assert_eq!(old.material_transparency(), if mode == "glass" { 36 } else { 85 });
            assert_eq!(old.material_blur(), mode == "glass");
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        std::fs::write(&store.path, "# Keep note\nbackdrop='transparent'\nfuture_key=42\n").unwrap();
        for value in [0, 85, 100] {
            let edited = store.set("backdrop_transparency", value.into()).unwrap();
            assert_eq!(edited.backdrop_transparency, Some(value));
            assert_eq!(store.load().unwrap(), edited);
        }
        for blur in [true, false] {
            let edited = store.set("backdrop_blur", blur.into()).unwrap();
            assert_eq!(edited.backdrop_blur, Some(blur));
            assert_eq!(store.load().unwrap(), edited);
        }
        // Changing the preset keeps explicit overrides and its exact saved percent.
        let glass = store.set("backdrop", "glass".into()).unwrap();
        assert_eq!(glass.material_transparency(), 100);
        assert!(!glass.material_blur());
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(saved.contains("# Keep note") && saved.contains("future_key=42"));
        for invalid in [(-1).into(), 101.into(), "85".into(), 0.5.into()] {
            assert!(store.set("backdrop_transparency", invalid).is_err());
        }
        assert!(store.set("backdrop_blur", "true".into()).is_err());
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), saved);
        for (value, expected) in [
            (0i64, 0),
            (85, 85),
            (100, 100),
            (999, 100),
            (u32::MAX as i64 + 1, 100),
            (i64::MAX, 100),
        ] {
            let read = Config::from_document(&format!("backdrop_transparency={value}").parse().unwrap()).unwrap();
            assert_eq!(read.backdrop_transparency, Some(expected));
        }
        for invalid in [
            "backdrop_transparency=-1",
            "backdrop_transparency=3.5",
            "backdrop_transparency='85'",
            "backdrop_blur='false'",
            "backdrop_blur=0",
        ] {
            assert!(Config::from_document(&invalid.parse().unwrap()).is_err(), "{invalid}");
        }
    }

    #[test]
    fn start_position_is_separate_and_all_saved_materials_remain_valid() {
        let config = Config::from_document(&"position='bottom'\nstart_position='center'".parse().unwrap()).unwrap();
        assert_eq!(config.start_position, "center");
        assert_eq!(config.position, "bottom");
        for invalid in ["top", "invalid"] {
            assert!(Config::from_document(&format!("start_position='{invalid}'").parse().unwrap()).is_err());
        }
        for value in [
            "acrylic",
            "mica",
            "solid",
            "transparent",
            "glass",
            "liquid-glass",
            "color",
        ] {
            assert_eq!(
                Config::from_document(&format!("backdrop='{value}'").parse().unwrap())
                    .unwrap()
                    .backdrop,
                value
            );
        }
    }

    #[test]
    fn dock_reveal_height_roundtrips_exact_pixels_and_preserves_legacy_defaults() {
        assert_eq!(
            Config::from_document(&"hotkey='Alt+Space'".parse().unwrap())
                .unwrap()
                .dock_reveal_height,
            8
        );
        for (value, expected) in [(0, 1), (1, 1), (8, 8), (128, 128), (999, 128)] {
            let doc = format!("dock_reveal_height={value}").parse().unwrap();
            assert_eq!(Config::from_document(&doc).unwrap().dock_reveal_height, expected);
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        std::fs::write(&store.path, "# Keep note\ndock_autohide=true\ncustom=42\n").unwrap();
        for value in [1, 9, 128] {
            assert_eq!(
                store
                    .set("dock_reveal_height", value.into())
                    .unwrap()
                    .dock_reveal_height,
                value
            );
        }
        for value in [(-1).into(), 0.into(), 129.into(), "8".into()] {
            assert!(store.set("dock_reveal_height", value).is_err());
        }
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(saved.contains("# Keep note") && saved.contains("custom=42"));
        assert_eq!(store.load().unwrap().dock_reveal_height, 128);
    }
    #[test]
    fn quit_shortcut_persists_and_rejects_the_launcher_chord() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        std::fs::write(&store.path, "# Keep note\nhotkey='Ctrl+Space'\n").unwrap();
        assert_eq!(store.load().unwrap().quit_hotkey, "Ctrl+Win+Alt+X");
        assert_eq!(
            store.set("quit_hotkey", "Win+Alt+Q".into()).unwrap().quit_hotkey,
            "Win+Alt+Q"
        );
        assert!(store.set("quit_hotkey", "Alt".into()).is_err());
        assert!(store.set("quit_hotkey", "control + SPACE".into()).is_err());
        assert!(store.set("hotkey", "Alt+Super+q".into()).is_err());
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(saved.contains("# Keep note"));
        assert!(saved.contains("hotkey='Ctrl+Space'"));
        assert!(saved.contains("Win+Alt+Q"));
    }

    #[test]
    fn preview_monitor_switch_defaults_off_and_preserves_icon_switch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        assert!(!store.load().unwrap().dock_preview_current_monitor_only);
        let config = store.set("dock_preview_current_monitor_only", true.into()).unwrap();
        assert!(config.dock_preview_current_monitor_only);
        assert!(!config.dock_current_monitor_only);
        assert!(store.set("dock_preview_current_monitor_only", "yes".into()).is_err());
        assert!(store.load().unwrap().dock_preview_current_monitor_only);
    }
    #[test]
    fn island_blur_roundtrip_preserves_legacy_values_and_comments() {
        let mut doc: toml_edit::DocumentMut = "# user notes\nhotkey='Ctrl+Space' # keep shortcut\n".parse().unwrap();
        assert!(!Config::from_document(&doc).unwrap().topbar_island_blur);
        validate_edit("topbar_island_blur", &true.into()).unwrap();
        doc["topbar_island_blur"] = toml_edit::value(true);
        let text = doc.to_string();
        assert!(text.contains("# user notes"));
        assert!(text.contains("# keep shortcut"));
        let read = Config::from_document(&text.parse().unwrap()).unwrap();
        assert!(read.topbar_island_blur);
        assert_eq!(read.hotkey, "Ctrl+Space");
        doc["topbar_island_blur"] = toml_edit::value(false);
        assert!(!Config::from_document(&doc).unwrap().topbar_island_blur);
        assert!(Config::from_document(&"topbar_island_blur=42".parse().unwrap()).is_err());
    }
    #[test]
    fn media_exclusion_config_roundtrip_and_removal() {
        let mut doc: toml_edit::DocumentMut = "hotkey='Alt+Space'".parse().unwrap();
        assert!(Config::from_document(&doc).unwrap().music_excluded_apps.is_empty());
        let list: toml_edit::Array = ["Google Chrome", "Spotify.exe"].into_iter().collect();
        let value = toml_edit::Value::Array(list);
        validate_edit("music_excluded_apps", &value).unwrap();
        doc["music_excluded_apps"] = toml_edit::value(value);
        let reloaded = Config::from_document(&doc.to_string().parse().unwrap()).unwrap();
        assert_eq!(reloaded.music_excluded_apps, ["Google Chrome", "Spotify.exe"]);
        doc["music_excluded_apps"] = toml_edit::value(toml_edit::Array::new());
        assert!(Config::from_document(&doc).unwrap().music_excluded_apps.is_empty());
        assert!(Config::from_document(&"music_excluded_apps=[42]".parse().unwrap()).is_err());
    }
    use super::*;
    #[test]
    fn topbar_defaults_preserve_old_configs_and_validate_edits() {
        let old = Config::from_document(&"hotkey='Alt+Space'".parse().unwrap()).unwrap();
        assert!(!old.topbar);
        assert_eq!(
            (old.topbar_height, old.topbar_opacity, old.topbar_island_opacity),
            (32, 0, 70)
        );
        assert_eq!(old.topbar_monitors, ["primary"]);
        let doc = "topbar=true\ntopbar_height=999\ntopbar_opacity=999\ntopbar_monitors=['\\\\.\\DISPLAY2']"
            .parse()
            .unwrap();
        let clamped = Config::from_document(&doc).unwrap();
        assert_eq!((clamped.topbar_height, clamped.topbar_opacity), (48, 100));
        assert!(validate_edit("topbar_height", &23.into()).is_err());
        assert!(validate_edit("topbar_opacity", &101.into()).is_err());
        assert!(Config::from_document(&"topbar_height=-1".parse().unwrap()).is_err());
        assert!(Config::from_document(&"topbar_monitors=[42]".parse().unwrap()).is_err());
    }

    #[test]
    fn defaults_match_launcher() {
        let config = Config::default();
        assert_eq!(config.hotkey, "Alt+Space");
        assert_eq!(config.max_results, 8);
        assert_eq!(config.theme, "system");
        assert_eq!(config.backdrop, "acrylic");
        assert!(config.show_recent && config.file_search);
        assert_eq!(config.file_search_exclude.len(), 14);
        assert_eq!(config.position, "bottom");
        assert_eq!(config.start_position, "bottom");
        assert!(config.dock && config.dock_autohide && config.hide_taskbar);
        assert_eq!(config.dock_icon_size, 24);
        assert_eq!(
            (
                config.dock_open_ms,
                config.dock_close_ms,
                config.dock_animation.as_str()
            ),
            (180, 140, "slide")
        );
        // A config from Wisp 0.3 has no dock keys and keeps the taskbar, as in the launcher.
        let old = Config::from_document(&"hotkey = 'Alt+Space'".parse().unwrap()).unwrap();
        assert!(!old.dock && !old.hide_taskbar);
        assert_eq!(old.position, "center");
        assert_eq!(old.start_position, "bottom");
        assert_eq!(old.max_results, config.max_results);
        assert_eq!(config.window_list, "always");
        assert_eq!(Config::from_document(&DEFAULT_CONFIG.parse().unwrap()).unwrap(), config);
    }

    #[test]
    fn preserves_comments_unknown_keys_and_latest_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        std::fs::write(
            &store.path,
            "# User note\nhotkey = 'Alt+Space' # Keep this\ncustom = 42\n",
        )
        .unwrap();
        store.set("hotkey", "Ctrl+F12".into()).unwrap();
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(saved.contains("# User note"));
        assert!(saved.contains("# Keep this"));
        assert!(saved.contains("custom = 42"));
        std::fs::write(&store.path, saved.replace("custom = 42", "custom = 99")).unwrap();
        store.set("show_recent", false.into()).unwrap();
        assert!(std::fs::read_to_string(&store.path).unwrap().contains("custom = 99"));
        assert!(!store.load().unwrap().show_recent);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn never_overwrites_broken_or_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        for text in [
            "theme = [",
            "show_recent = 'yes'",
            "theme = 'blue'",
            "file_search_exclude = [42]",
        ] {
            std::fs::write(&store.path, text).unwrap();
            assert!(store.load().is_err());
            assert!(store.set("hotkey", "Ctrl+Space".into()).is_err());
            assert_eq!(std::fs::read_to_string(&store.path).unwrap(), text);
        }
        std::fs::write(&store.path, [0xff, 0xfe]).unwrap();
        assert!(store.set("hotkey", "Ctrl+Space".into()).is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn keeps_comments_inside_folder_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        std::fs::write(
            &store.path,
            "file_search_exclude = [\n  # Keep generated files out\n  'target',\n  # Keep this note too\n]\n",
        )
        .unwrap();
        let array: toml_edit::Array = ["target", "build"].into_iter().collect();
        store.set("file_search_exclude", Value::Array(array)).unwrap();
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(saved.contains("# Keep generated files out"));
        assert!(saved.contains("'target'"));
        assert!(saved.contains("# Keep this note too"));
        assert_eq!(store.load().unwrap().file_search_exclude, ["target", "build"]);
    }

    #[test]
    fn creates_defaults_without_replacing_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("nested/config.toml"));
        store.ensure_exists().unwrap();
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), DEFAULT_CONFIG);
        std::fs::write(&store.path, "# Keep my file").unwrap();
        store.ensure_exists().unwrap();
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "# Keep my file");
    }

    #[test]
    fn validates_edits_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("config.toml"));
        for (key, value) in [
            ("hotkey", Value::from("Ctrl")),
            ("search_url", "https://example.com".into()),
            ("max_results", 0.into()),
            ("max_results", 21.into()),
            ("theme", "blue".into()),
            ("window_list", "sometimes".into()),
            ("dock_icon_size", 23.into()),
            ("dock_icon_size", 41.into()),
            ("unknown", true.into()),
        ] {
            assert!(store.set(key, value).is_err(), "{key}");
        }
        assert!(!store.path.exists());
        assert_eq!(store.set("max_results", 20.into()).unwrap().max_results, 20);
        assert_eq!(store.set("dock_icon_size", 40.into()).unwrap().dock_icon_size, 40);
    }

    #[test]
    fn matches_launcher_result_clamping_and_rejects_negative_numbers() {
        for (value, expected) in [(0, 1), (30, 20), (5, 5)] {
            let doc = format!("max_results = {value}").parse().unwrap();
            assert_eq!(Config::from_document(&doc).unwrap().max_results, expected);
        }
        assert!(Config::from_document(&"max_results = -1".parse().unwrap()).is_err());
        // The launcher reads the size as unsigned, so Settings rejects a negative one too.
        assert!(Config::from_document(&"dock_icon_size = -1".parse().unwrap()).is_err());
    }

    #[test]
    fn clamps_app_icon_size_to_supported_range() {
        for (value, expected) in [(0, 24), (24, 24), (32, 32), (40, 40), (100, 40)] {
            let doc = format!("dock_icon_size = {value}").parse().unwrap();
            assert_eq!(Config::from_document(&doc).unwrap().dock_icon_size, expected);
        }
    }
}
