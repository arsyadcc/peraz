//! Human-readable units.
//!
//! Peraz uses SI units everywhere (1 kB = 1,000 bytes, 1 MB = 1,000,000 bytes), which also
//! matches the `k`/`M` suffixes FFmpeg uses for bit rates. Mixing decimal and binary units is a
//! classic source of "target size" bugs, so there is deliberately no binary variant.

/// Formats a byte count with SI units, e.g. `12.3 MB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)] // display only
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    if value < 10.0 {
        format!("{value:.2} {unit}")
    } else if value < 100.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

/// Formats a bit rate with SI units, e.g. `9.65 Mb/s`.
pub fn format_bit_rate(bits_per_second: u64) -> String {
    let bytes_style = format_bytes(bits_per_second);
    // Reuse the byte formatter, then swap the unit for bits per second.
    let (number, unit) = bytes_style.split_once(' ').unwrap_or((&bytes_style, "B"));
    let unit = match unit {
        "B" => "b/s",
        "kB" => "kb/s",
        "MB" => "Mb/s",
        "GB" => "Gb/s",
        _ => "Tb/s",
    };
    format!("{number} {unit}")
}

/// Formats seconds as `m:ss.s` or `h:mm:ss`.
pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "?".to_owned();
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // checked non-negative
    let whole = seconds as u64;
    let (h, m, s) = (whole / 3600, (whole / 60) % 60, whole % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        #[allow(clippy::cast_precision_loss)]
        let frac = seconds - (whole - whole % 60) as f64;
        format!("{m}:{frac:04.1}")
    }
}

/// Parses an SI size (`25MB`, `8 MB`, `500kB`, `1.5GB`, `900000B`) into bytes. Binary units
/// (`MiB`) are rejected with an explanation rather than silently reinterpreted.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let text = s.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("`{s}` is not a size like 25MB"))?;
    let scale = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" | "k" => 1e3,
        "mb" | "m" => 1e6,
        "gb" | "g" => 1e9,
        "kib" | "mib" | "gib" => {
            return Err(format!(
                "`{s}` uses binary units; Peraz uses SI units (kB, MB, GB)"
            ));
        }
        "" => return Err(format!("`{s}` needs a unit: B, kB, MB, or GB")),
        other => return Err(format!("unknown size unit `{other}`; use B, kB, MB, or GB")),
    };
    let bytes = value * scale;
    if !bytes.is_finite() || bytes < 1.0 {
        return Err(format!("`{s}` is not a usable size"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // checked positive and finite
    Ok(bytes.floor() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_in_si_units() {
        assert_eq!(parse_size("25MB"), Ok(25_000_000));
        assert_eq!(parse_size("8 mb"), Ok(8_000_000));
        assert_eq!(parse_size("1.5GB"), Ok(1_500_000_000));
        assert_eq!(parse_size("500kB"), Ok(500_000));
        assert!(parse_size("25MiB").unwrap_err().contains("SI"));
        assert!(parse_size("25").unwrap_err().contains("unit"));
    }

    #[test]
    fn bytes_use_si_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1000), "1.00 kB");
        assert_eq!(format_bytes(1024), "1.02 kB");
        assert_eq!(format_bytes(12_345_678), "12.3 MB");
        assert_eq!(format_bytes(250_000_000), "250 MB");
        assert_eq!(format_bytes(3_000_000_000), "3.00 GB");
    }

    #[test]
    fn bit_rates() {
        assert_eq!(format_bit_rate(192_000), "192 kb/s");
        assert_eq!(format_bit_rate(9_654_676), "9.65 Mb/s");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(10.0), "0:10.0");
        assert_eq!(format_duration(75.25), "1:15.2");
        assert_eq!(format_duration(3725.0), "1:02:05");
        assert_eq!(format_duration(f64::NAN), "?");
    }
}
