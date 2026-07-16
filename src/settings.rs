// Persisted user preferences (view mode, zoom, sort, hidden files, window size),
// stored as simple `key=value` lines next to the bookmarks file.

use std::path::PathBuf;

/// Preferences that survive a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub is_list: bool,
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
            is_list: false,
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
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue; // not a key=value line
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "is_list" => set(&mut settings.is_list, value.parse().ok()),
                "zoom" => {
                    set(&mut settings.zoom, value.parse().ok().map(|z: i32| z.clamp(ZOOM_MIN, ZOOM_MAX)))
                }
                // Anything outside the three known columns falls back to name.
                "sort_key" => set(&mut settings.sort_key, value.parse().ok().filter(|k| *k <= 2)),
                "sort_desc" => set(&mut settings.sort_desc, value.parse().ok()),
                "show_hidden" => set(&mut settings.show_hidden, value.parse().ok()),
                "width" => set(&mut settings.width, value.parse().ok()),
                "height" => set(&mut settings.height, value.parse().ok()),
                "maximized" => set(&mut settings.maximized, value.parse().ok()),
                _ => {}
            }
        }
        settings
    }

    /// Render back to `key=value` lines.
    pub fn serialize(&self) -> String {
        [
            format!("is_list={}", self.is_list),
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
    std::fs::read_to_string(settings_file()).map(|t| Settings::parse(&t)).unwrap_or_default()
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
    fn round_trips_through_serialize_and_parse() {
        let settings = Settings {
            is_list: true,
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
        assert_eq!(parsed.is_list, Settings::default().is_list);
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
