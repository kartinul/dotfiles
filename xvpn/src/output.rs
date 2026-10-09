//! Consistent terminal output formatting for xvpn.

use std::io::{self, Write};

/// Print aligned key-value pairs. Keys left-aligned, values after a gap.
/// Usage: `kv("mode", "on")` → `mode      on`
pub fn kv(key: &str, value: &str) {
    println!("{:<10} {}", key, value);
}

/// Print aligned key-value with a section prefix.
/// Usage: `kv("mode", "on")` → `mode      on`
pub fn kv_prefix(prefix: &str, key: &str, value: &str) {
    println!("{} {:<10} {}", prefix, key, value);
}

/// Print a status line with a consistent prefix marker.
/// marker: "✓" (success), "✗" (error), "→" (info), "~" (pending)
pub fn status(marker: &str, msg: &str) {
    println!("{} {}", marker, msg);
}

/// Print a section header.
pub fn section(title: &str) {
    println!("{}", title);
}

/// Print a bullet list with consistent indent.
pub fn list(items: &[&str]) {
    if items.is_empty() {
        println!("  (none)");
    } else {
        for item in items {
            println!("  • {}", item);
        }
    }
}

/// Print a column-aligned table. Each row is a slice of column strings.
/// First row is treated as header if `header` is true.
pub fn table(rows: &[Vec<&str>], header: bool) {
    if rows.is_empty() {
        return;
    }
    let cols = rows[0].len();
    let mut widths = vec![0; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    for (ri, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (ci, cell) in row.iter().enumerate() {
            if ci > 0 {
                line.push_str("  ");
            }
            line.push_str(&format!("{:<width$}", cell, width = widths[ci]));
        }
        println!("{}", line);
        if header && ri == 0 {
            // underline header
            let mut sep = String::new();
            for (ci, w) in widths.iter().enumerate() {
                if ci > 0 {
                    sep.push_str("  ");
                }
                sep.push_str(&"─".repeat(*w));
            }
            println!("{}", sep);
        }
    }
}

/// Print an error message to stderr.
pub fn error(msg: &str) {
    eprintln!("✗ {}", msg);
}

/// Print a warning message to stderr.
pub fn warn(msg: &str) {
    eprintln!("~ {}", msg);
}

/// Print an info message.
pub fn info(msg: &str) {
    println!("→ {}", msg);
}

/// Ask for confirmation (y/N). Returns true if user confirms.
pub fn confirm(prompt: &str) -> bool {
    print!("{} [y/N]: ", prompt);
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}