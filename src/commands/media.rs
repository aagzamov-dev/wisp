//! What is playing, and play, pause, next and previous, through the system
//! media controls that Windows shows in its volume flyout.

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    Seek {
        source: String,
        title: String,
        position: i64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub source: String,
    pub title: String,
    pub artist: String,
    /// The app that plays, such as "Spotify".
    pub app: String,
    pub playing: bool,
    pub timeline: Option<Timeline>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeline {
    pub start: i64,
    pub end: i64,
    pub position: i64,
    pub updated: i64,
    pub rate: f64,
    pub seek: Option<(i64, i64)>,
}

impl Timeline {
    pub fn position(self, playing: bool) -> i64 {
        let elapsed = if playing && self.updated > 0 {
            const WINDOWS_EPOCH: i64 = 116_444_736_000_000_000;
            let now = std::time::UNIX_EPOCH.elapsed().unwrap_or_default();
            let ticks = (now.as_nanos() / 100) as i64 + WINDOWS_EPOCH;
            (ticks.saturating_sub(self.updated).max(0) as f64 * self.rate) as i64
        } else {
            0
        };
        self.position.saturating_add(elapsed).clamp(self.start, self.end)
    }
}

fn session() -> windows::core::Result<Session> {
    Manager::RequestAsync()?.join()?.GetCurrentSession()
}

/// `None` if nothing is playing or paused.
pub fn state() -> Option<State> {
    let session = session().ok()?;
    let properties = session.TryGetMediaPropertiesAsync().and_then(|op| op.join()).ok();
    let text = |value: windows::core::Result<windows::core::HSTRING>| value.map(|v| v.to_string()).unwrap_or_default();
    let playback = session.GetPlaybackInfo().ok();
    let playing = playback.as_ref().and_then(|info| info.PlaybackStatus().ok()) == Some(Status::Playing);
    let timeline = (|| {
        let values = session.GetTimelineProperties().ok()?;
        let start = values.StartTime().ok()?.Duration;
        let end = values.EndTime().ok()?.Duration;
        if start < 0 || end <= start {
            return None;
        }
        let rate = playback
            .as_ref()
            .and_then(|info| info.PlaybackRate().ok())
            .and_then(|rate| rate.Value().ok())
            .filter(|rate| rate.is_finite())
            .unwrap_or(1.);
        let seek = (|| {
            if !playback.as_ref()?.Controls().ok()?.IsPlaybackPositionEnabled().ok()? {
                return None;
            }
            let min = values.MinSeekTime().ok()?.Duration.max(start);
            let max = values.MaxSeekTime().ok()?.Duration.min(end);
            (min < max).then_some((min, max))
        })();
        Some(Timeline {
            start,
            end,
            seek,
            rate,
            position: values.Position().ok()?.Duration,
            updated: values.LastUpdatedTime().ok()?.UniversalTime,
        })
    })();
    let source = text(session.SourceAppUserModelId());
    Some(State {
        app: app_name(&source),
        source,
        title: properties.as_ref().map(|p| text(p.Title())).unwrap_or_default(),
        artist: properties.as_ref().map(|p| text(p.Artist())).unwrap_or_default(),
        playing,
        timeline,
    })
}

pub fn control(control: &Control) -> Result<(), String> {
    let session = session().map_err(|_| "Nothing is playing.".to_string())?;
    let request = match control {
        Control::PlayPause => session.TryTogglePlayPauseAsync(),
        Control::Play => session.TryPlayAsync(),
        Control::Pause => session.TryPauseAsync(),
        Control::Next => session.TrySkipNextAsync(),
        Control::Previous => session.TrySkipPreviousAsync(),
        Control::Seek {
            source,
            title,
            position,
        } => {
            let matches = session.SourceAppUserModelId().is_ok_and(|id| id == source.as_str())
                && session
                    .TryGetMediaPropertiesAsync()
                    .and_then(|op| op.join())
                    .and_then(|p| p.Title())
                    .is_ok_and(|current| current == title.as_str());
            if !matches {
                return Err("The current media item changed.".into());
            }
            session.TryChangePlaybackPositionAsync(*position)
        }
    };
    match request.and_then(|op| op.join()) {
        Ok(true) => Ok(()),
        Ok(false) => Err("The app that is playing did not accept the request.".into()),
        Err(error) => Err(error.message()),
    }
}

/// "Spotify.exe" → "Spotify"; "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic" → "ZuneMusic".
fn app_name(id: &str) -> String {
    let base = id.rsplit(['!', '\\']).next().unwrap_or(id);
    let base = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(base);
    let base = base.rsplit('.').next().unwrap_or(base);
    match base.to_lowercase().as_str() {
        "msedge" => "Microsoft Edge".into(),
        "chrome" => "Google Chrome".into(),
        _ => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_apps_from_their_ids() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(
            app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "ZuneMusic"
        );
        assert_eq!(app_name("MSEdge"), "Microsoft Edge");
        assert_eq!(app_name(r"C:\Apps\helium.exe"), "helium");
    }
}
