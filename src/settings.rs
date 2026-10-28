// Persisted user preferences (view mode, zoom, sort, hidden files, window size),
// stored as simple `key=value` lines next to the bookmarks file.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Grid,
    List,
    Columns,
}

impl ViewMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::List => "list",
            Self::Columns => "columns",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "grid" => Some(Self::Grid),
            "list" => Some(Self::List),
            "columns" => Some(Self::Columns),
            _ => None,
        }
    }
}

/// Preferences that survive a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub view_mode: ViewMode,
    pub preview_pane: bool,
    pub automatic_updates: bool,
    pub zoom: i32,
    /// 0 = name, 1 = size, 2 = modified.
    pub sort_key: u8,
    pub sort_desc: bool,
    pub show_hidden: bool,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            view_mode: ViewMode::Grid,
            preview_pane: false,
            automatic_updates: true,
            zoom: 80,
            sort_key: 0,
            sort_desc: false,
            show_hidden: false,
            width: 1120,
            height: 740,
            maximized: false,
        }
    }
}

/// Bounds for a sane zoom, mirroring the view's own limits.
const ZOOM_MIN: i32 = 48;
const ZOOM_MAX: i32 = 160;

/// Overwrite `slot` only when the value parsed, so a bad entry keeps its default.
fn set<T>(slot: &mut T, parsed: Option<T>) {
    if let Some(value) = parsed {
        *slot = value;
    }
}

impl Settings {
    /// Parse `key=value` lines. Unknown keys are ignored and anything missing or
    /// malformed keeps its default, so a hand-edited or older file still loads.
    pub fn parse(text: &str) -> Settings {
        let mut settings = Settings::default();
        let mut explicit_view = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue; // not a key=value line
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "is_list" => {
                    if let Ok(list) = value.parse::<bool>() {
                        settings.view_mode = if list { ViewMode::List } else { ViewMode::Grid };
                    }
                }
                "view_mode" => {
                    if let Some(mode) = ViewMode::parse(value) {
                        explicit_view = Some(mode);
                    }
                }
                "automatic_updates" => set(&mut settings.automatic_updates, value.parse().ok()),
                "preview_pane" => set(&mut settings.preview_pane, value.parse().ok()),
                "zoom" => set(
                    &mut settings.zoom,
                    value.parse().ok().map(|z: i32| z.clamp(ZOOM_MIN, ZOOM_MAX)),
                ),
                // Anything outside the three known columns falls back to name.
                "sort_key" => set(
                    &mut settings.sort_key,
                    value.parse().ok().filter(|k| *k <= 2),
                ),
                "sort_desc" => set(&mut settings.sort_desc, value.parse().ok()),
                "show_hidden" => set(&mut settings.show_hidden, value.parse().ok()),
                "width" => set(&mut settings.width, value.parse().ok()),
                "height" => set(&mut settings.height, value.parse().ok()),
                "maximized" => set(&mut settings.maximized, value.parse().ok()),
                _ => {}
            }
        }
        if let Some(mode) = explicit_view {
            settings.view_mode = mode;
        }
        settings
    }

    /// Render back to `key=value` lines.
    pub fn serialize(&self) -> String {
        [
            format!("view_mode={}", self.view_mode.name()),
            format!("preview_pane={}", self.preview_pane),
            format!("automatic_updates={}", self.automatic_updates),
            format!("zoom={}", self.zoom),
            format!("sort_key={}", self.sort_key),
            format!("sort_desc={}", self.sort_desc),
            format!("show_hidden={}", self.show_hidden),
            format!("width={}", self.width),
            format!("height={}", self.height),
            format!("maximized={}", self.maximized),
        ]
        .join("\n")
            + "\n"
    }
}

fn settings_file() -> PathBuf {
    let mut dir = gtk::glib::user_config_dir();
    dir.push("filescope");
    dir.push("settings");
    dir
}

/// Load saved preferences, falling back to defaults.
pub fn load() -> Settings {
    std::fs::read_to_string(settings_file())
        .map(|t| Settings::parse(&t))
        .unwrap_or_default()
}

/// Persist preferences (best-effort).
pub fn save(settings: &Settings) {
    let path = settings_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, settings.serialize());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_view_preferences_still_load() {
        assert_eq!(Settings::parse("is_list=true\n").view_mode, ViewMode::List);
        assert_eq!(Settings::parse("is_list=false\n").view_mode, ViewMode::Grid);
        assert_eq!(Settings::parse("garbage"), Settings::default());
        assert_eq!(
            Settings::parse("view_mode=columns\nis_list=true").view_mode,
            ViewMode::Columns
        );
        assert_eq!(
            Settings::parse("is_list=true\nview_mode=unknown").view_mode,
            ViewMode::List
        );
    }

    #[test]
    fn columns_view_preference_survives_round_trip() {
        let saved = Settings::parse("view_mode=columns\n").serialize();
        assert!(saved.contains("view_mode=columns\n"));
        assert_eq!(Settings::parse(&saved).serialize(), saved);
    }

    #[test]
    fn preview_pane_setting_survives_round_trip() {
        let saved = Settings::parse("preview_pane=true\n").serialize();
        assert!(saved.contains("preview_pane=true\n"));
        assert_eq!(Settings::parse(&saved).serialize(), saved);
        assert!(
            Settings::parse("")
                .serialize()
                .contains("preview_pane=false\n")
        );
    }

    #[test]
    fn round_trips_through_serialize_and_parse() {
        let settings = Settings {
            view_mode: ViewMode::List,
            preview_pane: true,
            automatic_updates: false,
            zoom: 128,
            sort_key: 2,
            sort_desc: true,
            show_hidden: true,
            width: 900,
            height: 600,
            maximized: true,
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
    }

    #[test]
    fn empty_input_gives_defaults() {
        assert_eq!(Settings::parse(""), Settings::default());
    }

    #[test]
    fn missing_and_malformed_keys_keep_their_defaults() {
        // Only `zoom` is valid here: the junk line, unknown key, and unparsable
        // number must all be ignored rather than losing the whole file.
        let parsed = Settings::parse("zoom=96\nnonsense\nunknown=1\nheight=abc\n");
        assert_eq!(parsed.zoom, 96);
        assert_eq!(parsed.height, Settings::default().height);
        assert_eq!(parsed.view_mode, Settings::default().view_mode);
    }

    #[test]
    fn zoom_is_clamped_to_the_supported_range() {
        assert_eq!(Settings::parse("zoom=9999").zoom, ZOOM_MAX);
        assert_eq!(Settings::parse("zoom=1").zoom, ZOOM_MIN);
    }

    #[test]
    fn sort_key_out_of_range_falls_back_to_name() {
        assert_eq!(Settings::parse("sort_key=7").sort_key, 0);
    }
}
