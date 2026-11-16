//! What is playing, and play, pause, next and previous, through the system
//! media controls that Windows shows in its volume flyout.

use crate::indexer::IconPixels;
use std::sync::Arc;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as Properties,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    ForTrack {
        source: String,
        title: String,
        control: Box<Control>,
    },
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

impl Control {
    pub fn for_track(track: &State, control: Control) -> Self {
        Self::ForTrack {
            source: track.source.clone(),
            title: track.title.clone(),
            control: Box::new(control),
        }
    }
}

#[allow(dead_code)]
#[path = "../media_identity.rs"]
mod identity;
use identity::app_name;
pub use identity::excluded;

fn matches_track(expected_source: &str, expected_title: &str, source: &str, title: &str) -> bool {
    !expected_source.is_empty() && expected_source == source && expected_title == title
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
    Some(read_state(&session, properties.as_ref()))
}

fn read_state(session: &Session, properties: Option<&Properties>) -> State {
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
    State {
        app: app_name(&source),
        source,
        title: properties.as_ref().map(|p| text(p.Title())).unwrap_or_default(),
        artist: properties.as_ref().map(|p| text(p.Artist())).unwrap_or_default(),
        playing,
        timeline,
    }
}

#[derive(Clone)]
pub struct NotchTrack {
    pub media: State,
    pub artwork: Option<Arc<IconPixels>>,
}

#[derive(Default)]
pub struct NotchReader {
    key: Option<(String, String, String)>,
    artwork: Option<Arc<IconPixels>>,
    artwork_attempts: u8,
}

impl NotchReader {
    pub fn read(&mut self, exclusions: &[String]) -> Option<NotchTrack> {
        let Some(session) = session().ok() else {
            *self = Self::default();
            return None;
        };
        let properties = session.TryGetMediaPropertiesAsync().and_then(|op| op.join()).ok();
        let media = read_state(&session, properties.as_ref());
        if excluded(&media.source, &media.app, exclusions) {
            *self = Self::default();
            return None;
        }
        let key = (media.source.clone(), media.title.clone(), media.artist.clone());
        if self.key.as_ref() != Some(&key) {
            self.artwork = None;
            self.artwork_attempts = 0;
            self.key = Some(key);
        }
        // Some players publish the title before the cover.
        if self.artwork.is_none() && self.artwork_attempts < 4 {
            self.artwork_attempts += 1;
            self.artwork = properties.as_ref().and_then(artwork).map(Arc::new);
        }
        Some(NotchTrack {
            media,
            artwork: self.artwork.clone(),
        })
    }
}

fn artwork(properties: &Properties) -> Option<IconPixels> {
    use windows::Graphics::Imaging::*;
    let stream = properties.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
    if stream.Size().ok()? > 8 * 1024 * 1024 {
        return None;
    }
    let decoder = BitmapDecoder::CreateAsync(&stream).ok()?.join().ok()?;
    let (width, height) = (decoder.PixelWidth().ok()?, decoder.PixelHeight().ok()?);
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 16_000_000 {
        return None;
    }
    let scale = (192. / f64::from(width.max(height))).min(1.);
    let width = (f64::from(width) * scale).round().max(1.) as u32;
    let height = (f64::from(height) * scale).round().max(1.) as u32;
    let transform = BitmapTransform::new().ok()?;
    transform.SetScaledWidth(width).ok()?;
    transform.SetScaledHeight(height).ok()?;
    let pixels = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
        .ok()?
        .join()
        .ok()?
        .DetachPixelData()
        .ok()?;
    (pixels.len() == (width * height * 4) as usize).then(|| IconPixels {
        width,
        height,
        bgra: pixels.to_vec(),
    })
}

pub fn control(control: &Control) -> Result<(), String> {
    let session = session().map_err(|_| "Nothing is playing.".to_string())?;
    let control = if let Control::ForTrack { source, title, control } = control {
        let current_source = session.SourceAppUserModelId().map_err(|e| e.message())?.to_string();
        let current_title = session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.join())
            .and_then(|properties| properties.Title())
            .map_err(|e| e.message())?
            .to_string();
        if !matches_track(source, title, &current_source, &current_title) {
            return Err("The current media item changed.".into());
        }
        control.as_ref()
    } else {
        control
    };
    let request = match control {
        Control::ForTrack { .. } => return Err("Invalid media request.".into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_match_complete_names_or_source_ids_and_can_be_removed() {
        let mut exclusions = vec!["  google CHROME  ".into(), "Spotify.exe".into(), " ".into()];
        assert!(excluded("Chrome.exe", "Google Chrome", &exclusions));
        assert!(excluded("spotify.EXE", "Spotify", &exclusions));
        assert!(!excluded("ChromeBeta.exe", "Google Chrome Beta", &exclusions));
        assert!(!excluded("YouTube.exe", "YouTube", &exclusions));
        exclusions.clear();
        assert!(!excluded("Chrome.exe", "Google Chrome", &exclusions));
        exclusions.push("Chrome.exe".into());
        assert!(excluded("Chrome.exe", "Google Chrome", &exclusions));
    }

    #[test]
    fn displayed_source_binding_rejects_unknown_or_changed_media() {
        assert!(matches_track("Chrome.exe", "Song", "Chrome.exe", "Song"));
        assert!(!matches_track("Chrome.exe", "Song", "Spotify.exe", "Song"));
        assert!(!matches_track("Chrome.exe", "Song", "Chrome.exe", "Another song"));
        assert!(!matches_track("", "Song", "", "Song"));
        // The UI applies current settings again when an older worker result arrives.
        let exclusions = vec!["Google Chrome".into()];
        assert!(excluded("Chrome.exe", "Google Chrome", &exclusions));
    }

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
