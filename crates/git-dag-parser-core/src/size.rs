//! Byte sizes on the command line and in messages. The multiple is 1024,
//! as for git's `core.deltaBaseCacheLimit`.

/// A size such as `512`, `8 kb`, `1536mb` or `1GB`: a whole number of
/// bytes, or of the unit `b`, `k`/`kb`, `m`/`mb` or `g`/`gb`, in any case.
/// Spaces before the unit are permitted; decimals are not.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("size is empty".to_string());
    }
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (number, unit) = s.split_at(digits);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("size '{s}' does not start with a whole number"))?;
    if unit.starts_with(['.', ',']) {
        return Err(format!(
            "size '{s}' has a decimal; use a smaller unit, such as 1536mb for 1.5gb"
        ));
    }
    let shift = match unit.trim_start().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" => 10,
        "m" | "mb" => 20,
        "g" | "gb" => 30,
        other => return Err(format!("size '{s}' has an unknown unit '{other}'")),
    };
    number
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("size '{s}' is too large"))
}

/// `bytes` in the largest of B, KB, MB and GB in which it is 1 or more,
/// with two decimals; bytes have none. For example `512 B`, `23.40 KB`,
/// `1.18 GB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u32); 3] = [("GB", 30), ("MB", 20), ("KB", 10)];
    for (unit, shift) in UNITS {
        if bytes >= 1 << shift {
            return format!("{:.2} {unit}", bytes as f64 / (1u64 << shift) as f64);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_reads_bytes_without_a_unit() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size(" 512 "), Ok(512));
    }

    #[test]
    fn parse_size_reads_each_unit_in_any_case() {
        assert_eq!(parse_size("1b"), Ok(1));
        assert_eq!(parse_size("1k"), Ok(1024));
        assert_eq!(parse_size("1kb"), Ok(1024));
        assert_eq!(parse_size("1m"), Ok(1 << 20));
        assert_eq!(parse_size("1mb"), Ok(1 << 20));
        assert_eq!(parse_size("1g"), Ok(1 << 30));
        assert_eq!(parse_size("1gb"), Ok(1 << 30));
        assert_eq!(parse_size("1GB"), Ok(1 << 30));
        assert_eq!(parse_size("1Gb"), Ok(1 << 30));
        assert_eq!(parse_size("1536mb"), Ok(1536 << 20));
        assert_eq!(parse_size("512b"), Ok(512));
        assert_eq!(parse_size("8kb"), Ok(8192));
        assert_eq!(parse_size("8K"), Ok(8192));
        assert_eq!(parse_size("16mb"), Ok(16 << 20));
    }

    #[test]
    fn parse_size_permits_spaces_before_the_unit() {
        assert_eq!(parse_size("8 kb"), Ok(8192));
        assert_eq!(parse_size("8  KB"), Ok(8192));
    }

    #[test]
    fn parse_size_rejects_an_empty_value() {
        assert_eq!(parse_size(""), Err("size is empty".to_string()));
        assert_eq!(parse_size("   "), Err("size is empty".to_string()));
    }

    #[test]
    fn parse_size_rejects_a_value_that_is_not_a_whole_number() {
        for bad in ["-1", "kb", "abc", "+1"] {
            let err = parse_size(bad).unwrap_err();
            assert!(err.contains(bad), "{bad}: {err}");
            assert!(err.contains("whole number"), "{bad}: {err}");
        }
    }

    #[test]
    fn parse_size_rejects_decimals() {
        for bad in ["1.5gb", "1.0", "2,5mb"] {
            let err = parse_size(bad).unwrap_err();
            assert!(err.contains(bad), "{bad}: {err}");
            assert!(err.contains("decimal"), "{bad}: {err}");
        }
    }

    #[test]
    fn parse_size_rejects_an_unknown_unit() {
        for bad in ["1tb", "10tb", "1 bytes", "1x", "12x", "1 k b"] {
            let err = parse_size(bad).unwrap_err();
            assert!(err.contains(bad), "{bad}: {err}");
            assert!(err.contains("unknown unit"), "{bad}: {err}");
        }
    }

    #[test]
    fn parse_size_rejects_a_result_too_large_for_u64() {
        assert_eq!(parse_size("16gb"), Ok(16 << 30));
        let err = parse_size("99999999999gb").unwrap_err();
        assert!(err.contains("too large"), "{err}");
        // 2^34 * 2^30 is 2^64, one more than u64::MAX
        let err = parse_size("17179869184gb").unwrap_err();
        assert!(err.contains("too large"), "{err}");
        let err = parse_size("99999999999999999999").unwrap_err();
        assert!(err.contains("whole number"), "{err}");
    }

    #[test]
    fn format_size_uses_the_largest_unit_of_one_or_more() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(23962), "23.40 KB");
        assert_eq!(format_size(1 << 20), "1.00 MB");
        assert_eq!(format_size(1 << 30), "1.00 GB");
        assert_eq!(format_size(1_267_000_000), "1.18 GB");
        assert_eq!(format_size(1 << 40), "1024.00 GB");
    }
}
