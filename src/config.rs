//! TOML config in `%APPDATA%\wisp\config.toml`.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;

use crate::{dock_gradient::Gradient, util};

/// wisp-settings includes the same file, so both apps create the same config.
const DEFAULT_CONFIG: &str = include_str!("default-config.toml");
/// Values for missing keys that differ from the default config, shared with wisp-settings.
const MISSING_KEYS: &str = include_str!("config-0.3.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    Acrylic,
    Mica,
    Solid,
    Transparent,
    Glass,
    #[serde(rename = "liquid-glass")]
    LiquidGlass,
    Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[serde(alias = "top")]
    Center,
    /// The query box sits at the bottom of the screen, where the dock is.
    Bottom,
}

fn start_position<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Position, D::Error> {
    let value = String::deserialize(deserializer)?;
    match value.as_str() {
        "center" => Ok(Position::Center),
        "bottom" => Ok(Position::Bottom),
        _ => Err(serde::de::Error::unknown_variant(&value, &["center", "bottom"])),
    }
}

/// Match Settings' TOML integer range before converting to the renderer's u32.
fn backdrop_transparency<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    Option::<i64>::deserialize(deserializer)?
        .map(|value| {
            if value < 0 {
                Err(serde::de::Error::custom(
                    "backdrop_transparency must be a non-negative whole number",
                ))
            } else {
                Ok(value.min(100) as u32)
            }
        })
        .transpose()
}

/// When hovering an app shows its window list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WindowListMode {
    Multiple,
    #[default]
    Always,
    Off,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockAnimation {
    #[default]
    Slide,
    Fade,
    RiseFade,
    ScaleFade,
    SoftRise,
    BottomReveal,
    SoftScale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DockBackground {
    SharedMaterial,
    Gradient,
}

fn deserialize_gradient<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Gradient, D::Error> {
    #[derive(Deserialize)]
    struct Fields {
        start: String,
        end: String,
        direction: String,
        transparency: i64,
    }

    let fields = Fields::deserialize(deserializer)?;
    Gradient::parse(&fields.start, &fields.end, &fields.direction, fields.transparency)
        .map_err(serde::de::Error::custom)
}

/// Keys missing from the user's file take their value from `config-0.3.toml`,
/// then from the default config, as in wisp-settings, so both apps agree on
/// what an old config means.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub pause_on_rdp: bool,
    pub automatic_updates: bool,
    pub hide_taskbar: bool,
    pub win_start: bool,
    pub music_notch: bool,
    /// Exact source app IDs or display names, case insensitive; no URL guessing.
    pub music_excluded_apps: Vec<String>,
    pub topbar: bool,
    pub topbar_island_blur: bool,
    pub topbar_height: u32,
    pub topbar_opacity: u32,
    pub topbar_island_opacity: u32,
    pub topbar_monitors: Vec<String>,
    pub backdrop_color: String,
    #[serde(default, deserialize_with = "backdrop_transparency")]
    pub backdrop_transparency: Option<u32>,
    #[serde(default)]
    pub backdrop_blur: Option<bool>,
    pub dock: bool,
    pub(crate) dock_background: DockBackground,
    #[serde(default, deserialize_with = "deserialize_gradient")]
    pub(crate) dock_gradient: Gradient,
    pub dock_autohide: bool,
    /// Physical pixels from the monitor bottom, independent of Windows scaling.
    pub dock_reveal_height: u32,
    pub dock_smart_hide: bool,
    pub dock_current_monitor_only: bool,
    pub dock_preview_current_monitor_only: bool,
    pub dock_icon_size: usize,
    pub dock_open_ms: u32,
    pub dock_close_ms: u32,
    pub dock_animation: DockAnimation,
    pub hotkey: String,
    pub quit_hotkey: String,
    pub max_results: usize,
    pub theme: Theme,
    pub backdrop: Backdrop,
    pub position: Position,
    /// Wisp Start placement, independent of the compact launcher.
    #[serde(deserialize_with = "start_position")]
    pub start_position: Position,
    pub show_recent: bool,
    pub file_search: bool,
    pub file_search_exclude: Vec<String>,
    pub window_list: WindowListMode,
}

impl Default for Config {
    fn default() -> Self {
        Self::parse(DEFAULT_CONFIG).expect("valid default config")
    }
}

impl Config {
    pub fn dock_gradient(&self) -> Option<Gradient> {
        (self.dock_background == DockBackground::Gradient).then_some(self.dock_gradient)
    }

    pub fn material_options(&self) -> crate::theme::MaterialOptions {
        crate::theme::MaterialOptions {
            transparency: self.backdrop_transparency,
            blur: self.backdrop_blur,
        }
    }

    pub fn path() -> PathBuf {
        util::data_dir().join("config.toml")
    }

    pub fn modified() -> Option<SystemTime> {
        std::fs::metadata(Self::path()).and_then(|m| m.modified()).ok()
    }

    /// Creates the default config on first run. Returns defaults and an error
    /// if the file cannot be read or parsed.
    pub fn load() -> (Config, Option<String>) {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let defaults = crate::remote_mode::first_run_config(DEFAULT_CONFIG, &path);
                let _ = std::fs::write(&path, defaults.as_bytes());
                defaults.into_owned()
            }
            // Preserve unreadable files, including locked files and invalid UTF-8.
            Err(e) => return (Config::default(), Some(format!("config.toml: {e}"))),
        };
        match Self::parse_for_path(&text, Some(&path)) {
            Ok(mut config) => {
                config.max_results = config.max_results.clamp(1, 20);
                config.dock_icon_size = config.dock_icon_size.clamp(24, 40);
                config.dock_open_ms = config.dock_open_ms.min(500);
                config.dock_close_ms = config.dock_close_ms.min(500);
                (config, None)
            }
            Err(e) => (Config::default(), Some(format!("config.toml: {}", e.message()))),
        }
    }

    /// Reads a config, taking each missing key from `config-0.3.toml` or the
    /// default config.
    fn parse(text: &str) -> Result<Config, toml::de::Error> {
        Self::parse_for_path(text, None)
    }

    fn parse_for_path(text: &str, path: Option<&std::path::Path>) -> Result<Config, toml::de::Error> {
        let mut table: toml::Table = toml::from_str(DEFAULT_CONFIG)?;
        let saved: toml::Table = toml::from_str(text)?;
        table.extend(toml::from_str::<toml::Table>(MISSING_KEYS)?);
        if !saved.contains_key("pause_on_rdp") && path.is_some_and(crate::remote_mode::legacy_opt_out) {
            table.insert("pause_on_rdp".into(), toml::Value::Boolean(false));
        }
        table.extend(saved);
        let mut config: Config = table.try_into()?;
        config.dock_reveal_height = config.dock_reveal_height.clamp(1, 128);
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gradient_is_dock_only_and_uses_shared_material_by_default() {
        use crate::dock_gradient::{Direction, Gradient};

        assert_eq!(super::Config::default().dock_gradient(), None);
        assert_eq!(super::Config::parse("backdrop='glass'").unwrap().dock_gradient(), None);
        let default_gradient = super::Config::parse("dock_background='gradient'").unwrap();
        assert_eq!(default_gradient.dock_gradient(), Some(Gradient::default()));

        let saved = "backdrop='glass'\ndock_background='gradient'\n[dock_gradient]\nstart='#12343A'\nend='#233F4D'\ndirection='diagonal'\ntransparency=100";
        let config = super::Config::parse(saved).unwrap();
        assert_eq!(config.backdrop, super::Backdrop::Glass);
        assert_eq!(
            config.dock_gradient(),
            Some(Gradient {
                start: [0x12, 0x34, 0x3A],
                end: [0x23, 0x3F, 0x4D],
                direction: Direction::Diagonal,
                transparency: 100,
            })
        );
        for (valid, invalid) in [
            ("#12343A", "#123"),
            ("#233F4D", "#233F4Z"),
            ("diagonal", "sideways"),
            ("100", "101"),
            ("100", "-1"),
        ] {
            assert!(
                super::Config::parse(&saved.replace(valid, invalid)).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn legacy_and_new_dock_animation_keys_parse() {
        use super::DockAnimation::*;

        for (key, style) in [
            ("slide", Slide),
            ("fade", Fade),
            ("rise-fade", RiseFade),
            ("scale-fade", ScaleFade),
            ("soft-rise", SoftRise),
            ("bottom-reveal", BottomReveal),
            ("soft-scale", SoftScale),
        ] {
            let config = super::Config::parse(&format!("dock_animation='{key}'")).unwrap();
            assert_eq!(config.dock_animation, style);
        }
    }

    #[test]
    fn shared_material_options_load_old_and_new_configs() {
        use super::Config;
        for mode in ["transparent", "glass"] {
            let old = Config::parse(&format!("backdrop='{mode}'")).unwrap();
            assert_eq!(old.backdrop_transparency, None);
            assert_eq!(old.backdrop_blur, None);
            assert_eq!(
                old.material_options().transparency(old.backdrop),
                if mode == "glass" { 36 } else { 85 }
            );
            assert_eq!(old.material_options().blur(old.backdrop), mode == "glass");
            for (value, expected) in [
                (0i64, 0),
                (85, 85),
                (100, 100),
                (999, 100),
                (u32::MAX as i64 + 1, 100),
                (i64::MAX, 100),
            ] {
                for blur in [false, true] {
                    let config = Config::parse(&format!(
                        "backdrop='{mode}'\nbackdrop_transparency={value}\nbackdrop_blur={blur}\nfuture_key=42"
                    ))
                    .unwrap();
                    assert_eq!(config.backdrop_transparency, Some(expected));
                    assert_eq!(config.backdrop_blur, Some(blur));
                }
            }
        }
        for invalid in [
            "backdrop_transparency=-1",
            "backdrop_transparency=3.5",
            "backdrop_transparency='85'",
            "backdrop_blur='false'",
            "backdrop_blur=0",
        ] {
            assert!(Config::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn dock_reveal_height_defaults_and_bounds_match_physical_pixel_setting() {
        use super::Config;
        assert_eq!(Config::parse("hotkey='Alt+Space'").unwrap().dock_reveal_height, 8);
        for (value, expected) in [(0, 1), (1, 1), (8, 8), (128, 128), (999, 128)] {
            assert_eq!(
                Config::parse(&format!("dock_reveal_height={value}"))
                    .unwrap()
                    .dock_reveal_height,
                expected
            );
        }
        assert!(Config::parse("dock_reveal_height=-1").is_err());
        assert!(Config::parse("dock_reveal_height='8'").is_err());
    }
    #[test]
    fn preview_monitor_filter_is_opt_in_and_independent_of_icon_filter() {
        let old = super::Config::parse("hotkey='Alt+Space'").unwrap();
        assert!(!old.dock_preview_current_monitor_only);
        let configured =
            super::Config::parse("dock_current_monitor_only=false\ndock_preview_current_monitor_only=true").unwrap();
        assert!(configured.dock_preview_current_monitor_only);
        assert!(!configured.dock_current_monitor_only);
        assert!(super::Config::parse("dock_preview_current_monitor_only='true'").is_err());
    }
    #[test]
    fn island_blur_is_opt_in_and_validates_legacy_configs() {
        assert!(!super::Config::parse("hotkey='Alt+Space'").unwrap().topbar_island_blur);
        assert!(
            super::Config::parse("topbar_island_blur=true")
                .unwrap()
                .topbar_island_blur
        );
        assert!(super::Config::parse("topbar_island_blur='true'").is_err());
    }
    #[test]
    fn media_exclusion_config_preserves_defaults_and_roundtrips() {
        assert!(
            super::Config::parse("hotkey='Alt+Space'")
                .unwrap()
                .music_excluded_apps
                .is_empty()
        );
        let config = super::Config::parse("music_excluded_apps=['Google Chrome','Spotify.exe']").unwrap();
        assert_eq!(config.music_excluded_apps, ["Google Chrome", "Spotify.exe"]);
        assert!(super::Config::parse("music_excluded_apps=[42]").is_err());
        assert!(
            super::Config::parse("music_excluded_apps=[]")
                .unwrap()
                .music_excluded_apps
                .is_empty()
        );
    }
    use super::*;
    #[test]
    fn topbar_is_opt_in_for_existing_configs() {
        let old = Config::parse("hotkey='Alt+Space'").unwrap();
        assert!(!old.topbar);
        assert_eq!(
            (old.topbar_height, old.topbar_opacity, old.topbar_island_opacity),
            (32, 0, 70)
        );
        assert_eq!(old.topbar_monitors, ["primary"]);
        assert!(Config::parse("topbar_height=-1").is_err());
        assert!(Config::parse("topbar_monitors=[42]").is_err());
    }

    #[test]
    fn start_placement_is_independent_of_compact_launcher_placement() {
        let old = Config::parse("position='center'").unwrap();
        assert_eq!(old.start_position, Position::Bottom);
        let configured = Config::parse("position='bottom'\nstart_position='center'").unwrap();
        assert_eq!(configured.position, Position::Bottom);
        assert_eq!(configured.start_position, Position::Center);
        assert!(Config::parse("start_position='invalid'").is_err());
        assert!(Config::parse("start_position='top'").is_err());
        assert_eq!(Config::parse("position='top'").unwrap().position, Position::Center);
    }

    #[test]
    fn legacy_pins_are_ignored() {
        let config = Config::parse("dock = true\ndock_apps = ['old.app']").unwrap();
        assert!(config.dock);
        assert_eq!(config.window_list, super::WindowListMode::Always);
    }

    #[test]
    fn automatic_updates_require_an_explicit_boolean_opt_in() {
        assert!(!Config::default().automatic_updates);
        assert!(!Config::parse("theme = 'dark'").unwrap().automatic_updates);
        assert!(Config::parse("automatic_updates = true").unwrap().automatic_updates);
        assert!(Config::parse("automatic_updates = 'true'").is_err());
    }

    #[test]
    fn window_list_modes_deserialize() {
        for (value, expected) in [
            ("multiple", super::WindowListMode::Multiple),
            ("always", super::WindowListMode::Always),
            ("off", super::WindowListMode::Off),
        ] {
            let config = Config::parse(&format!("window_list = '{value}'")).unwrap();
            assert_eq!(config.window_list, expected);
        }
    }

    #[test]
    fn missing_keys_keep_a_config_from_before_the_dock() {
        // A Wisp 0.3 config has none of the dock's keys and keeps the taskbar.
        let config = Config::parse("hotkey = \"Ctrl+Space\"").unwrap();
        assert_eq!((config.hotkey.as_str(), config.max_results), ("Ctrl+Space", 8));
        assert_eq!(config.quit_hotkey, "Ctrl+Win+Alt+X");
        let edited = Config::parse("quit_hotkey = 'Win+Alt+Q'").unwrap();
        assert_eq!(edited.quit_hotkey, "Win+Alt+Q");
        assert!(!config.dock && !config.hide_taskbar);
        assert_eq!(config.position, Position::Center);
        // A new config has them all.
        let default = Config::default();
        assert!(default.dock && default.hide_taskbar);
        assert_eq!(default.dock_icon_size, 24);
        assert_eq!(default.position, Position::Bottom);
        assert!(Config::parse("dock = \"yes\"").is_err());
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;

    #[test]
    fn remote_pause_defaults_on_and_only_adopts_an_explicit_legacy_opt_out() {
        let root = std::env::temp_dir().join(format!(
            "wisp-rdp-config-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("config.toml");
        let legacy = root.join("remote-mode.txt");
        let parse = |text| Config::parse_for_path(text, Some(&path)).unwrap().pause_on_rdp;
        assert!(Config::default().pause_on_rdp);
        assert!(parse("hide_taskbar = false"));

        for (legacy_value, expected) in [("Auto", true), ("On", true), ("  oFF\r\n", false), ("bad", true)] {
            std::fs::write(&legacy, legacy_value).unwrap();
            assert_eq!(parse("hide_taskbar = false"), expected);
            assert!(parse("pause_on_rdp = true"));
            assert!(!parse("pause_on_rdp = false"));
        }
        std::fs::write(&legacy, "Off").unwrap();
        let defaults = crate::remote_mode::first_run_config(DEFAULT_CONFIG, &path);
        assert!(!parse(&defaults));
        assert!(Config::parse_for_path("pause_on_rdp = 'false'", Some(&path)).is_err());
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "Off");
        std::fs::remove_file(&legacy).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
