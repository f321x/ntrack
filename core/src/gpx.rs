//! GPX 1.1 serialization for exporting a received track.
//!
//! Pure and dependency-free: `ntrack-core` carries no date/time crate, so the
//! UTC timestamp formatting is hand-rolled with Howard Hinnant's
//! `civil_from_days` algorithm (integer math only). The output is a single
//! `<trk>` with one `<trkseg>`; splitting a track into multiple segments on
//! STOP boundaries or large gaps is a possible later refinement and would be a
//! pure function of the same point list.

/// Build a GPX 1.1 document for a track named `name` from `points`, each a
/// `(lat, lng, ts)` triple where `ts` is the capture time in Unix seconds.
///
/// Points are emitted verbatim, in the order given (the caller sorts and
/// dedups). Coordinates are written with 7 decimal places (~11 mm). An empty
/// `points` slice yields a well-formed document with an empty `<trkseg>`.
pub fn build_gpx(name: &str, points: &[(f64, f64, u64)]) -> String {
    let mut s = String::with_capacity(256 + points.len() * 96);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str(
        "<gpx version=\"1.1\" creator=\"ntrack\" xmlns=\"http://www.topografix.com/GPX/1/1\">\n",
    );
    s.push_str("  <trk>\n");
    s.push_str("    <name>");
    s.push_str(&xml_escape(name));
    s.push_str("</name>\n");
    s.push_str("    <trkseg>\n");
    for (lat, lng, ts) in points {
        s.push_str(&format!(
            "      <trkpt lat=\"{lat:.7}\" lon=\"{lng:.7}\"><time>{}</time></trkpt>\n",
            iso8601_utc(*ts)
        ));
    }
    s.push_str("    </trkseg>\n");
    s.push_str("  </trk>\n");
    s.push_str("</gpx>\n");
    s
}

/// Format Unix seconds as an ISO-8601 UTC instant: `YYYY-MM-DDTHH:MM:SSZ`.
fn iso8601_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Format Unix seconds as a compact UTC calendar date `YYYYMMDD`, for use in
/// export filenames.
pub fn yyyymmdd_utc(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// Howard Hinnant's `civil_from_days`: convert a day count since the Unix
/// epoch (1970-01-01) into a `(year, month, day)` proleptic-Gregorian triple.
/// Integer-only; correct for all dates we can encounter (`days >= 0`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// Escape the five XML predefined entities so a label can't break the document.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_epoch_zero() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_known_timestamp() {
        // The timestamp used throughout the protocol tests.
        assert_eq!(iso8601_utc(1_722_173_222), "2024-07-28T13:27:02Z");
    }

    #[test]
    fn iso8601_leap_day() {
        // 2024-02-29T00:00:00Z — a leap day, which a buggy date routine skips.
        assert_eq!(iso8601_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        // One second before is still Feb 29 (the 23:59:59 boundary).
        assert_eq!(iso8601_utc(1_709_251_199), "2024-02-29T23:59:59Z");
    }

    #[test]
    fn yyyymmdd_is_compact_utc_date() {
        assert_eq!(yyyymmdd_utc(0), "19700101");
        assert_eq!(yyyymmdd_utc(1_722_173_222), "20240728");
        assert_eq!(yyyymmdd_utc(1_709_164_800), "20240229");
    }

    #[test]
    fn iso8601_year_boundary() {
        // 2023-12-31T23:59:59Z rolls to 2024-01-01T00:00:00Z one second later.
        assert_eq!(iso8601_utc(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(iso8601_utc(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn empty_points_is_well_formed_with_empty_trkseg() {
        let gpx = build_gpx("Trip", &[]);
        assert!(gpx.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(gpx.contains("<gpx version=\"1.1\" creator=\"ntrack\""));
        assert!(gpx.contains("<name>Trip</name>"));
        assert!(gpx.contains("<trkseg>"));
        assert!(gpx.contains("</trkseg>"));
        assert!(!gpx.contains("<trkpt"));
        assert!(gpx.trim_end().ends_with("</gpx>"));
    }

    #[test]
    fn trkpts_preserve_order_and_carry_lat_lon_time() {
        let pts = [
            (48.137_43, 11.575_49, 1_722_173_222),
            (48.140_00, 11.580_00, 1_722_173_282),
        ];
        let gpx = build_gpx("walk", &pts);
        let first = gpx.find("48.1374300").unwrap();
        let second = gpx.find("48.1400000").unwrap();
        assert!(first < second, "trkpts must keep the given order");
        assert!(gpx.contains(
            "<trkpt lat=\"48.1374300\" lon=\"11.5754900\"><time>2024-07-28T13:27:02Z</time></trkpt>"
        ));
        assert!(gpx.contains("<time>2024-07-28T13:28:02Z</time>"));
    }

    #[test]
    fn coordinates_use_seven_decimals() {
        let gpx = build_gpx("p", &[(1.0, -2.5, 0)]);
        assert!(gpx.contains("lat=\"1.0000000\" lon=\"-2.5000000\""));
    }

    #[test]
    fn name_is_xml_escaped() {
        let gpx = build_gpx("Anna & \"Bob\" <3 >_<", &[]);
        assert!(gpx.contains("<name>Anna &amp; &quot;Bob&quot; &lt;3 &gt;_&lt;</name>"));
        // The raw, unescaped characters must not appear inside the element.
        assert!(!gpx.contains("<name>Anna & "));
    }

    #[test]
    fn xml_escape_covers_all_five_entities() {
        assert_eq!(xml_escape("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(xml_escape("plain"), "plain");
    }
}
