use std::time::Duration;

/// Reads `name` as whole seconds, falling back to `default_secs` and clamping to `cap_secs`.
pub fn env_duration(name: &str, default_secs: u64, cap_secs: Option<u64>) -> Duration {
    let raw = std::env::var(name).ok();
    let (secs, warning) = parse_secs(raw.as_deref(), default_secs, cap_secs);
    if let Some(warning) = warning {
        tracing::warn!(var = name, "{name}: {warning}");
    }
    Duration::from_secs(secs)
}

/// Reads a test-only millisecond hook; `None` when unset or unparsable.
pub fn test_hook_millis(name: &str) -> Option<Duration> {
    let raw = std::env::var(name).ok()?;
    raw.trim()
        .parse()
        .inspect_err(|e| tracing::warn!(var = name, value = %raw, error = %e, "ignoring test hook"))
        .ok()
        .map(Duration::from_millis)
}

fn parse_secs(raw: Option<&str>, default: u64, cap: Option<u64>) -> (u64, Option<String>) {
    let Some(raw) = raw else {
        return (default, None);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => (
            default,
            Some(format!("rejected value {raw:?}, using {default}")),
        ),
        Err(e) => (
            default,
            Some(format!("rejected value {raw:?} ({e}), using {default}")),
        ),
        Ok(secs) => match cap {
            Some(cap) if secs > cap => (cap, Some(format!("{secs} is above the cap, using {cap}"))),
            _ => (secs, None),
        },
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn unset_is_silent_default() {
        assert_eq!(parse_secs(None, 600, None), (600, None));
    }

    #[test]
    fn zero_falls_back_with_warning() {
        let (secs, warning) = parse_secs(Some("0"), 600, None);
        assert_eq!(secs, 600);
        assert!(warning.is_some());
    }

    #[test]
    fn unparsable_falls_back_with_warning_naming_value() {
        let (secs, warning) = parse_secs(Some("abc"), 600, None);
        assert_eq!(secs, 600);
        assert!(warning.is_some_and(|w| w.contains("abc")));
    }

    #[test]
    fn above_cap_is_capped() {
        assert_eq!(parse_secs(Some("5000"), 1800, Some(3000)).0, 3000);
    }

    #[test]
    fn valid_value_is_used() {
        assert_eq!(parse_secs(Some("42"), 600, Some(3000)), (42, None));
    }
}
