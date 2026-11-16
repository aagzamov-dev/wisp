//! Media source identities shared by playback and Settings. No title/URL
//! heuristics: exclusions always compare complete source IDs or display names.

pub fn app_name(id: &str) -> String {
    let base = id.rsplit(['!', '\\']).next().unwrap_or(id);
    let base = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(base);
    let base = base.rsplit('.').next().unwrap_or(base);
    match base.to_ascii_lowercase().as_str() {
        "msedge" => "Microsoft Edge".into(),
        "chrome" => "Google Chrome".into(),
        "firefox" => "Firefox".into(),
        _ => base.into(),
    }
}

pub fn excluded(source: &str, app: &str, values: &[String]) -> bool {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .any(|value| value.eq_ignore_ascii_case(source.trim()) || value.eq_ignore_ascii_case(app.trim()))
}

pub fn add_exact(values: &[String], additions: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = values.to_vec();
    for value in additions {
        let value = value.trim();
        if !value.is_empty() && !values.iter().any(|known| known.trim().eq_ignore_ascii_case(value)) {
            values.push(value.into());
        }
    }
    values
}

/// Only hashes read from Mozilla's explicit Firefox TaskBarIDs registry key
/// belong here. A hex-looking media source alone never establishes its app.
pub fn registered_firefox_ids(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut ids = Vec::new();
    for value in values.into_iter().take(128) {
        let value = value.trim();
        if value.len() == 16 && value.bytes().all(|c| c.is_ascii_hexdigit()) {
            ids = add_exact(&ids, [value.to_ascii_uppercase()]);
        }
    }
    ids
}

pub fn is_registered_firefox(source: &str, registered: &[String]) -> bool {
    registered.iter().any(|id| {
        source.eq_ignore_ascii_case(id)
            || source
                .get(..8)
                .filter(|prefix| prefix.eq_ignore_ascii_case("Firefox."))
                .and_then(|_| source.get(8..))
                .is_some_and(|tail| tail.eq_ignore_ascii_case(id))
    })
}

pub fn firefox_additions(registered: &[String]) -> Vec<String> {
    let mut values = vec!["Firefox".into()];
    for id in registered_firefox_ids(registered.iter().cloned()) {
        values.push(id.clone());
        values.push(format!("Firefox.{id}"));
    }
    values
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub name: String,
}

pub fn source_list(ids: impl IntoIterator<Item = String>, firefox: &[String]) -> Vec<Source> {
    let mut sources: Vec<Source> = Vec::new();
    for id in ids.into_iter().take(128) {
        let id = id.trim();
        if id.is_empty() || id.len() > 1024 || sources.iter().any(|source| source.id.eq_ignore_ascii_case(id)) {
            continue;
        }
        sources.push(Source {
            id: id.into(),
            name: if is_registered_firefox(id, firefox) {
                "Firefox".into()
            } else {
                app_name(id)
            },
        });
    }
    sources.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    sources.truncate(32);
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn firefox_hash_requires_local_registration_and_preset_is_opt_in() {
        let hash = "308046B0AF4A39CB";
        assert!(!is_registered_firefox(hash, &[]));
        let registered = registered_firefox_ids([hash.into(), "bad".into(), hash.to_lowercase()]);
        assert_eq!(registered, [hash]);
        assert!(is_registered_firefox(hash, &registered));
        assert!(is_registered_firefox(&format!("firefox.{hash}"), &registered));
        assert!(!is_registered_firefox("FFFFFFFFFFFFFFFF", &registered));
        let existing = vec!["Spotify".into()];
        let updated = add_exact(&existing, firefox_additions(&registered));
        assert_eq!(existing, ["Spotify"]);
        assert!(excluded(hash, hash, &updated));
        assert!(excluded("firefox.exe", "Firefox", &updated));
        assert!(!excluded("FFFFFFFFFFFFFFFF", "FFFFFFFFFFFFFFFF", &updated));
        assert!(!excluded(hash, &app_name(hash), &["Firefox".into()]));
        assert_eq!(add_exact(&updated, firefox_additions(&registered)), updated);
    }
    #[test]
    fn list_is_bounded_deduplicated_and_keeps_same_named_exact_sources() {
        let ids = ["Spotify.exe", "spotify.EXE", "", "one!Player", "two!Player"].map(String::from);
        let sources = source_list(ids, &[]);
        assert_eq!(sources.len(), 3);
        assert_eq!(sources.iter().filter(|source| source.name == "Player").count(), 2);
        assert_eq!(source_list((0..100).map(|i| format!("app{i}")), &[]).len(), 32);
    }
}
