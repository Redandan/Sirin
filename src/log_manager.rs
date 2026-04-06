use std::cmp::Ordering;

/// Represents different levels of logging severity.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl PartialOrd for LogLevel {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LogLevel {
    fn cmp(&self, other: &Self) -> Ordering {
        // Define order: Error > Warn > Info > Debug
        match (self, other) {
            (LogLevel::Error, LogLevel::Error) => Ordering::Equal,
            (LogLevel::Error, _) => Ordering::Greater,
            (_, LogLevel::Error) => Ordering::Less,

            (LogLevel::Warn, LogLevel::Warn) => Ordering::Equal,
            (LogLevel::Warn, _) => Ordering::Greater,
            (_, LogLevel::Warn) => Ordering::Less,

            (LogLevel::Info, LogLevel::Info) => Ordering::Equal,
            (LogLevel::Info, _) => Ordering::Greater,
            (_, LogLevel::Info) => Ordering::Less,

            (LogLevel::Debug, LogLevel::Debug) => Ordering::Equal,
        }
    }
}

impl LogLevel {
    /// Tries to parse a LogLevel from a string prefix (e.g., "[ERROR]").
    pub fn from_log_prefix(log_line: &str) -> Option<Self> {
        if log_line.starts_with("[ERROR]") {
            Some(LogLevel::Error)
        } else if log_line.starts_with("[WARN]") {
            Some(LogLevel::Warn)
        } else if log_line.starts_with("[INFO]") {
            Some(LogLevel::Info)
        } else if log_line.starts_with("[DEBUG]") {
            Some(LogLevel::Debug)
        } else {
            None
        }
    }
}

/// Filters a slice of log strings based on a minimum `LogLevel` and returns the `n` most recent matching logs.
/// Logs are expected to start with a level prefix like "[ERROR]", "[WARN]", etc.
pub fn filtered_recent(logs: &[String], min_level: LogLevel, n: usize) -> Vec<String> {
    logs.iter()
        .rev()
        .filter(|log_line| {
            LogLevel::from_log_prefix(log_line)
                .map_or(false, |level| level >= min_level)
        })
        .take(n)
        .cloned()
        .collect::<Vec<String>>()
        .into_iter()
        .rev()
        .collect() // Re-reverse to maintain original chronological order of the 'recent' items
}

/// Exports the `n` most recent log entries from a slice of log strings into a single string,
/// with each log entry separated by a newline.
pub fn export_to_string(logs: &[String], n: usize) -> String {
    logs.iter()
        .rev()
        .take(n)
        .cloned()
        .collect::<Vec<String>>() // Collect into a Vec to reverse properly
        .into_iter()
        .rev() // Reverse back to chronological order
        .collect::<Vec<String>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loglevel_comparison() {
        assert!(LogLevel::Error > LogLevel::Warn);
        assert!(LogLevel::Warn > LogLevel::Info);
        assert!(LogLevel::Info > LogLevel::Debug);
        assert!(LogLevel::Error > LogLevel::Debug);
        assert!(LogLevel::Error == LogLevel::Error);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Debug <= LogLevel::Info);
    }

    #[test]
    fn test_loglevel_from_log_prefix() {
        assert_eq!(LogLevel::from_log_prefix("[ERROR] Something went wrong"), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_log_prefix("[WARN] Something unusual"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::from_log_prefix("[INFO] An event occurred"), Some(LogLevel::Info));
        assert_eq!(LogLevel::from_log_prefix("[DEBUG] Debug message"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::from_log_prefix("No prefix message"), None);
        assert_eq!(LogLevel::from_log_prefix("[TRACE] Trace message"), None);
    }

    #[test]
    fn test_filtered_recent() {
        let logs = vec![
            "[DEBUG] Debug 1".to_string(),
            "[INFO] Info 1".to_string(),
            "[WARN] Warn 1".to_string(),
            "[ERROR] Error 1".to_string(),
            "[DEBUG] Debug 2".to_string(),
            "[INFO] Info 2".to_string(),
            "[WARN] Warn 2".to_string(),
            "[ERROR] Error 2".to_string(),
        ];

        // Filter for Info and higher, last 3
        let result = filtered_recent(&logs, LogLevel::Info, 3);
        assert_eq!(
            result,
            vec![
                "[INFO] Info 2".to_string(),
                "[WARN] Warn 2".to_string(),
                "[ERROR] Error 2".to_string()
            ]
        );

        // Filter for Error, last 1
        let result = filtered_recent(&logs, LogLevel::Error, 1);
        assert_eq!(
            result,
            vec!["[ERROR] Error 2".to_string()]
        );

        // Filter for Debug, last 5
        let result = filtered_recent(&logs, LogLevel::Debug, 5);
        assert_eq!(
            result,
            vec![
                "[INFO] Info 2".to_string(),
                "[WARN] Warn 2".to_string(),
                "[ERROR] Error 2".to_string(),
                "[DEBUG] Debug 2".to_string(),
                "[INFO] Info 1".to_string()
            ]
        );

        // No matching logs
        let empty_logs: Vec<String> = vec![
            "[DEBUG] Debug 1".to_string(),
            "[INFO] Info 1".to_string(),
        ];
        let result = filtered_recent(&empty_logs, LogLevel::Error, 5);
        assert!(result.is_empty());

        // Not enough logs to satisfy n
        let result = filtered_recent(&logs, LogLevel::Error, 5);
        assert_eq!(
            result,
            vec!["[ERROR] Error 1".to_string(), "[ERROR] Error 2".to_string()]
        );
    }

    #[test]
    fn test_export_to_string() {
        let logs = vec![
            "Log line 1".to_string(),
            "Log line 2".to_string(),
            "Log line 3".to_string(),
            "Log line 4".to_string(),
        ];

        let expected = "Log line 3\nLog line 4".to_string();
        assert_eq!(export_to_string(&logs, 2), expected);

        let expected_all = "Log line 1\nLog line 2\nLog line 3\nLog line 4".to_string();
        assert_eq!(export_to_string(&logs, 4), expected_all);

        let expected_more_than_available = "Log line 1\nLog line 2\nLog line 3\nLog line 4".to_string();
        assert_eq!(export_to_string(&logs, 10), expected_more_than_available);

        let expected_empty = "".to_string();
        assert_eq!(export_to_string(&vec![], 5), expected_empty);

        let expected_single = "Log line 4".to_string();
        assert_eq!(export_to_string(&logs, 1), expected_single);
    }
}