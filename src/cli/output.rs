use std::fmt;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::Value;

use super::CliError;

/// Set once stdout has gone away (`| head`, `| jq -m`, …): stop writing and
/// exit successfully instead of panicking on `EPIPE`.
static CLOSED: AtomicBool = AtomicBool::new(false);
static WRITE_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn write_stdout(args: fmt::Arguments<'_>, newline: bool) {
    if CLOSED.load(Ordering::Relaxed) {
        return;
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let result = (if newline {
        writeln!(lock, "{args}")
    } else {
        write!(lock, "{args}")
    })
    .and_then(|()| lock.flush());

    match result {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
            CLOSED.store(true, Ordering::Relaxed);
        }
        Err(error) => {
            let mut slot = WRITE_ERROR
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if slot.is_none() {
                *slot = Some(format!("failed to write stdout: {error}"));
            }
        }
    }
}

/// Write one line to stdout, stopping silently when the reader closed early.
pub fn line(args: fmt::Arguments<'_>) {
    write_stdout(args, true);
}

/// Write text to stdout without adding a newline.
pub fn text(args: fmt::Arguments<'_>) {
    write_stdout(args, false);
}

/// Write a line to stderr, ignoring a closed stderr rather than panicking.
pub fn error_line(args: fmt::Arguments<'_>) {
    let stderr = io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(lock, "{args}");
}

/// Propagate a non-BrokenPipe stdout failure after the command finishes.
pub fn finish() -> Result<(), CliError> {
    let mut slot = WRITE_ERROR
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    match slot.take() {
        Some(message) => Err(CliError::Io(message)),
        None => Ok(()),
    }
}

/// Print the API's own JSON unchanged. A 204 response has no body.
pub fn print_json(body: &str) {
    if body.is_empty() {
        return;
    }
    if body.ends_with('\n') {
        text(format_args!("{body}"));
    } else {
        text(format_args!("{body}\n"));
    }
}

/// Replace control characters so server-side text (card titles, comments) can
/// never inject ANSI/OSC sequences into the terminal. JSON output is untouched.
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn width(value: &str) -> usize {
    value.chars().count()
}

/// A plain, aligned table with no colour or ANSI escapes.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|header| width(header)).collect();
    for row in rows {
        for (index, value) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(width(value));
        }
    }

    let mut output = String::new();
    let mut push = |cells: &[String]| {
        let cleaned: Vec<String> = cells.iter().map(|cell| sanitize(cell)).collect();
        let mut line = String::new();
        for (index, cell) in cleaned.iter().enumerate().take(columns) {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let padding = widths[index].saturating_sub(width(cell));
            if index + 1 < columns {
                line.push_str(&" ".repeat(padding));
            }
        }
        output.push_str(line.trim_end());
        output.push('\n');
    };

    push(
        &headers
            .iter()
            .map(|header| (*header).to_owned())
            .collect::<Vec<_>>(),
    );
    for row in rows {
        let mut cells = row.clone();
        cells.resize(columns, String::new());
        push(&cells);
    }
    output
}

pub fn optional(value: Option<&str>) -> String {
    sanitize(value.unwrap_or("-"))
}

pub fn labels(value: &Value) -> String {
    let names: Vec<&str> = value
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        "-".to_owned()
    } else {
        sanitize(&names.join(", "))
    }
}

pub fn string(value: &Value, key: &str) -> String {
    sanitize(value.get(key).and_then(Value::as_str).unwrap_or("-"))
}

pub fn timestamp(value: &Value, key: &str) -> String {
    match value.get(key).and_then(Value::as_i64) {
        Some(millis) => crate::time::format_utc_millis(millis),
        None => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tables_align_columns_without_ansi() {
        let rendered = table(
            &["REF", "STATUS", "TITLE"],
            &[
                vec!["ALP-1".to_owned(), "todo".to_owned(), "Short".to_owned()],
                vec![
                    "ALP-42".to_owned(),
                    "in_progress".to_owned(),
                    "A longer title".to_owned(),
                ],
            ],
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "REF     STATUS       TITLE");
        assert_eq!(lines[1], "ALP-1   todo         Short");
        assert_eq!(lines[2], "ALP-42  in_progress  A longer title");
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn empty_rows_still_render_the_header() {
        assert_eq!(table(&["A", "B"], &[]), "A  B\n");
    }

    #[test]
    fn control_characters_are_stripped_from_plain_output() {
        assert_eq!(sanitize("a\u{1b}[31mred\u{7}"), "a [31mred ");
        let rendered = table(&["TITLE"], &[vec!["bad\u{1b}[2Jtitle".to_owned()]]);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn formats_labels_and_missing_values() {
        assert_eq!(labels(&json!([])), "-");
        assert_eq!(labels(&json!([{"name": "bug"}, {"name": "ui"}])), "bug, ui");
        assert_eq!(optional(None), "-");
        assert_eq!(optional(Some("build")), "build");
    }
}
