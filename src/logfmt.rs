use serde_json::Value;

fn color_for_level(level: &str) -> &'static str {
    match level {
        "ERROR" => "31",
        "WARN" => "33",
        "INFO" => "32",
        "DEBUG" => "36",
        _ => "0",
    }
}

pub fn format_log_line(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };

    let get = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let timestamp = get("@timestamp");
    let level = get("level");
    let thread = get("thread");
    let class = get("class");
    let message = get("message");
    let throwable = get("throwable");

    let color = color_for_level(&level);
    let throwable_suffix = if throwable.is_empty() {
        String::new()
    } else {
        format!("\n{throwable}")
    };

    format!(
        "\u{1b}[{color}m{timestamp} {level} [{thread}] {class} - {message}{throwable_suffix}\u{1b}[0m"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_info_line_green() {
        let raw = r#"{"@timestamp":"2026-09-01T10:00:00.000Z","level":"INFO","thread":"main","class":"Node","message":"Started"}"#;
        let out = format_log_line(raw);
        assert!(out.starts_with("\u{1b}[32m"), "expected green color code, got: {out}");
        assert!(out.contains("2026-09-01T10:00:00.000Z INFO [main] Node - Started"));
        assert!(out.ends_with("\u{1b}[0m"));
    }

    #[test]
    fn formats_error_line_red_with_throwable() {
        let raw = r#"{"@timestamp":"t","level":"ERROR","thread":"t1","class":"C","message":"boom","throwable":"java.lang.RuntimeException: boom\n\tat C.run"}"#;
        let out = format_log_line(raw);
        assert!(out.starts_with("\u{1b}[31m"));
        assert!(out.contains("t ERROR [t1] C - boom\njava.lang.RuntimeException: boom"));
    }

    #[test]
    fn formats_warn_yellow_and_debug_cyan() {
        let warn = r#"{"@timestamp":"t","level":"WARN","thread":"t1","class":"C","message":"m"}"#;
        assert!(format_log_line(warn).starts_with("\u{1b}[33m"));

        let debug = r#"{"@timestamp":"t","level":"DEBUG","thread":"t1","class":"C","message":"m"}"#;
        assert!(format_log_line(debug).starts_with("\u{1b}[36m"));
    }

    #[test]
    fn unknown_level_has_no_color_code() {
        let raw = r#"{"@timestamp":"t","level":"TRACE","thread":"t1","class":"C","message":"m"}"#;
        let out = format_log_line(raw);
        assert!(out.starts_with("\u{1b}[0m"));
    }

    #[test]
    fn missing_throwable_has_no_extra_line() {
        let raw = r#"{"@timestamp":"t","level":"INFO","thread":"t1","class":"C","message":"m"}"#;
        let out = format_log_line(raw);
        assert_eq!(out.matches('\n').count(), 0);
    }

    #[test]
    fn malformed_json_passes_through_unchanged() {
        let raw = "not json at all";
        assert_eq!(format_log_line(raw), "not json at all");
    }
}
