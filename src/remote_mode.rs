//! Read the old Remote Desktop preference for configurations without the new key.

use std::{borrow::Cow, fs::File, io::Read, path::Path};

pub fn legacy_opt_out(config_path: &Path) -> bool {
    let Ok(file) = File::open(config_path.with_file_name("remote-mode.txt")) else {
        return false;
    };
    let mut value = String::new();
    file.take(65).read_to_string(&mut value).is_ok() && value.len() <= 64 && value.trim().eq_ignore_ascii_case("off")
}

pub fn first_run_config<'a>(defaults: &'a str, config_path: &Path) -> Cow<'a, str> {
    if legacy_opt_out(config_path) {
        Cow::Owned(defaults.replace("pause_on_rdp = true", "pause_on_rdp = false"))
    } else {
        Cow::Borrowed(defaults)
    }
}
