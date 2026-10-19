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

/// A short, friendly local-time label, regardless of the timestamp's source zone.
pub fn modified(dt: &gtk::glib::DateTime) -> String {
    dt.to_local().and_then(|local| local.format("%Y-%m-%d %H:%M")).map(|s| s.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn timestamps_use_one_local_timezone() {
        let utc = gtk::glib::DateTime::from_iso8601("2024-01-02T00:00:00Z", None).unwrap();
        let offset = gtk::glib::DateTime::from_iso8601("2024-01-02T05:30:00+05:30", None).unwrap();
        assert_eq!(super::modified(&utc), super::modified(&offset));
        assert!(!super::modified(&utc).is_empty());
    }

    #[test]
    fn scales_through_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 kB");
        assert_eq!(human_size(1_500_000), "1.5 MB");
        assert_eq!(human_size(2_000_000_000), "2.0 GB");
    }
}
