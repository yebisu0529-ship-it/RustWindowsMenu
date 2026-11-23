use std::io::Write;

use serde::Serialize;

use crate::error::CliResult;

/// Writes to stdout and ignores failures. A closed pipe (`rwm status | more`, then quit) is
/// not worth the panic that `println!` raises.
pub fn print(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

pub fn print_line(text: impl AsRef<str>) {
    print(&format!("{}\n", text.as_ref()));
}

pub fn print_error_line(text: impl AsRef<str>) {
    let _ = writeln!(std::io::stderr().lock(), "{}", text.as_ref());
}

pub fn print_json(value: &impl Serialize) -> CliResult<()> {
    print_line(serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Renders `label  value` lines with the values in one column.
pub fn fields(rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| display_width(label))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|(label, value)| {
            let pad = " ".repeat(width - display_width(label));
            format!("{label}{pad}  {value}\n")
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A plain text table. The last column is never padded, so long values such as DLL paths
/// wrap at the terminal edge without dragging trailing spaces along.
pub struct Table {
    columns: Vec<(String, Align)>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(columns: &[(&str, Align)]) -> Self {
        Self {
            columns: columns
                .iter()
                .map(|(name, align)| ((*name).to_string(), *align))
                .collect(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: impl IntoIterator<Item = String>) {
        let mut cells: Vec<String> = cells.into_iter().collect();
        cells.resize(self.columns.len(), String::new());
        self.rows.push(cells);
    }

    pub fn render(&self) -> String {
        let widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, (name, _))| {
                self.rows
                    .iter()
                    .map(|row| display_width(&row[index]))
                    .chain([display_width(name)])
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let headers: Vec<String> = self.columns.iter().map(|(name, _)| name.clone()).collect();
        let rules: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();

        let mut text = String::new();
        for cells in [&headers, &rules].into_iter().chain(&self.rows) {
            text.push_str(&self.render_line(cells, &widths));
        }
        text
    }

    fn render_line(&self, cells: &[String], widths: &[usize]) -> String {
        let last = cells.len().saturating_sub(1);
        let mut line = String::new();
        for (index, cell) in cells.iter().enumerate() {
            let pad = " ".repeat(widths[index] - display_width(cell));
            match self.columns[index].1 {
                Align::Right => {
                    line.push_str(&pad);
                    line.push_str(cell);
                }
                Align::Left => {
                    line.push_str(cell);
                    if index != last {
                        line.push_str(&pad);
                    }
                }
            }
            if index != last {
                line.push_str("  ");
            }
        }
        line.push('\n');
        line
    }
}

/// Terminal columns a string occupies. CJK and other full-width characters take two.
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Shortens `text` to at most `max` terminal columns, marking the cut with `…`.
pub fn truncate_to_width(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut used = 0;
    let mut shortened = String::new();
    for ch in text.chars() {
        let width = char_width(ch);
        if used + width > budget {
            break;
        }
        used += width;
        shortened.push(ch);
    }
    shortened.push('…');
    shortened
}

fn char_width(ch: char) -> usize {
    let code = ch as u32;
    let zero_width =
        ch.is_control() || matches!(code, 0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F);
    let wide = matches!(
        code,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD
    );
    if zero_width {
        0
    } else if wide {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns_and_leaves_the_last_one_unpadded() {
        let mut table = Table::new(&[
            ("name", Align::Left),
            ("ms", Align::Right),
            ("path", Align::Left),
        ]);
        table.row([
            "7-Zip".to_string(),
            "12.500".to_string(),
            r"C:\a.dll".to_string(),
        ]);
        table.row([
            "Git".to_string(),
            "3.250".to_string(),
            r"C:\b\c.dll".to_string(),
        ]);

        assert_eq!(
            table.render(),
            "name       ms  path\n\
             -----  ------  ----------\n\
             7-Zip  12.500  C:\\a.dll\n\
             Git     3.250  C:\\b\\c.dll\n"
        );
    }

    #[test]
    fn table_pads_full_width_text_by_terminal_columns() {
        let mut table = Table::new(&[("name", Align::Left), ("items", Align::Right)]);
        table.row(["拡張メニュー".to_string(), "4".to_string()]);
        table.row(["Git".to_string(), "12".to_string()]);

        let rendered = table.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[2], "拡張メニュー      4");
        assert_eq!(lines[3], "Git              12");
        assert_eq!(display_width(lines[2]), display_width(lines[3]));
    }

    #[test]
    fn truncation_respects_terminal_columns() {
        assert_eq!(truncate_to_width("short", 10), "short");
        assert_eq!(truncate_to_width("abcdefghij", 6), "abcde…");
        assert_eq!(truncate_to_width("拡張メニュー", 7), "拡張メ…");
        assert!(display_width(&truncate_to_width("拡張メニュー", 7)) <= 7);
    }

    #[test]
    fn fields_align_their_values() {
        let rendered = fields(&[
            ("Windows", "11".to_string()),
            ("Kill switch", "off".to_string()),
        ]);
        assert_eq!(rendered, "Windows      11\nKill switch  off\n");
    }
}
