// Small value formatters for the directory view.

/// Human-readable byte size, e.g. `4.0 kB`, `1.3 GB` (SI units, like GNOME).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A short, friendly modified-time label from a GLib `DateTime`.
pub fn modified(dt: &gtk::glib::DateTime) -> String {
    dt.format("%Y-%m-%d %H:%M").map(|s| s.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn scales_through_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 kB");
        assert_eq!(human_size(1_500_000), "1.5 MB");
        assert_eq!(human_size(2_000_000_000), "2.0 GB");
    }
}
