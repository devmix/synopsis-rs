//! Presentation layer for human-readable one-shot CLI output (design D1-D5).
//!
//! [`Console`] renders box-drawing tables, key-value blocks, section headers
//! and status lines into [`String`]s: pure formatting, no I/O, no config, no
//! globals. The `&mut dyn Write` seam and error handling stay in the command
//! modules (`model.rs`, `queue.rs`, ...), which write the returned strings.
//!
//! Color is applied via owo-colors only when the console was built with
//! `color = true` (TTY stdout without `NO_COLOR`, see [`Console::stdout`]);
//! tabled's `ansi` feature keeps width measurement correct for pre-styled
//! (ANSI) cell strings. The width adapts to the terminal on TTY and is fixed
//! (120) otherwise, so piped and test output is plain and byte-deterministic.

use std::io::IsTerminal;

use owo_colors::{AnsiColors, Effect, OwoColorize, Style as OwoStyle};
use tabled::builder::Builder;
use tabled::settings::object::Columns;
use tabled::settings::{Alignment, Padding, Style, Width};

/// Fallback max width when stdout is not a TTY or the size query fails (D3).
const DEFAULT_MAX_WIDTH: usize = 120;
/// Floor applied to the terminal-width query on TTY (D3).
const MIN_TERMINAL_WIDTH: usize = 40;

/// Semantic palette of the console layer (design D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Success / installed.
    Green,
    /// Error.
    Red,
    /// Pending.
    Yellow,
    /// Processing.
    Blue,
    /// Secondary text (e.g. `not installed`).
    Dim,
    /// Section headers.
    Bold,
}

impl Color {
    /// The owo-colors runtime style for this semantic color.
    fn owo_style(self) -> OwoStyle {
        match self {
            Color::Green => OwoStyle::new().color(AnsiColors::Green),
            Color::Red => OwoStyle::new().color(AnsiColors::Red),
            Color::Yellow => OwoStyle::new().color(AnsiColors::Yellow),
            Color::Blue => OwoStyle::new().color(AnsiColors::Blue),
            Color::Dim => OwoStyle::new().effect(Effect::Dimmed),
            Color::Bold => OwoStyle::new().bold(),
        }
    }
}

/// One-shot CLI console (design D4).
///
/// All render methods are pure: they return the formatted [`String`] and
/// never perform I/O. Production code constructs via [`Console::stdout`];
/// tests and non-TTY rendering use [`Console::plain`].
#[derive(Debug, Clone, Copy)]
pub struct Console {
    /// Whether owo-colors escape sequences may be emitted.
    color: bool,
    /// Maximum display width (columns) of any rendered table line.
    max_width: usize,
}

impl Console {
    /// Production constructor (design D2/D3).
    ///
    /// `color` is on when stdout is a TTY and `NO_COLOR` is not set (per
    /// no-color.org, presence of the variable with any value disables
    /// color). `max_width` is `max(terminal width, 40)` when stdout is a TTY
    /// and the size query succeeds, else 120.
    pub fn stdout() -> Self {
        let is_tty = std::io::stdout().is_terminal();
        let color = is_tty && std::env::var_os("NO_COLOR").is_none();
        let max_width = if is_tty {
            match terminal_size::terminal_size() {
                Some((width, _)) => (width.0 as usize).max(MIN_TERMINAL_WIDTH),
                None => DEFAULT_MAX_WIDTH,
            }
        } else {
            DEFAULT_MAX_WIDTH
        };
        Self { color, max_width }
    }

    /// Deterministic constructor (tests and non-TTY rendering): no color,
    /// fixed `max_width`.
    pub fn plain(max_width: usize) -> Self {
        Self {
            color: false,
            max_width,
        }
    }

    /// Render a box-drawing table (design D5).
    ///
    /// `Style::modern()` borders, a header row, `right_aligned` columns
    /// (e.g. numeric DIM/ATTEMPTS) right-aligned, and a whole-table
    /// `Width::wrap(max_width)`: content-fit when narrower, wrapped when
    /// wider — no line exceeds `max_width` display columns. Cells may be
    /// pre-styled with [`Console::style`]; the `ansi` feature measures
    /// through the escapes, so alignment stays correct.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>], right_aligned: &[usize]) -> String {
        let mut builder = Builder::with_capacity(rows.len() + 1, headers.len());
        builder.push_record(headers.to_vec());
        for row in rows {
            builder.push_record(row.clone());
        }
        let mut table = builder.build();
        table.with(Style::modern());
        for &column in right_aligned {
            table.modify(Columns::one(column), Alignment::right());
        }
        table.with(Width::wrap(self.max_width));
        table.to_string()
    }

    /// Render a borderless key-value block (design D5): `Label:   value`
    /// rows, label column auto-width, values left-aligned; a whole-table
    /// `Width::wrap(max_width)` keeps long values wrapped instead of
    /// overflowing.
    pub fn kv(&self, pairs: &[(&str, &str)]) -> String {
        let mut builder = Builder::with_capacity(pairs.len(), 2);
        for (label, value) in pairs {
            builder.push_record(vec![format!("{label}: "), value.to_string()]);
        }
        let mut table = builder.build();
        table.with(Style::empty());
        table.with(Padding::new(0, 0, 0, 0));
        table.with(Width::wrap(self.max_width));
        table.to_string()
    }

    /// Render a section header: a single title line, bold when color is on
    /// (design D5 — the old dash separators are gone).
    pub fn header(&self, title: &str) -> String {
        self.style(title, Color::Bold)
    }

    /// Render a success line: `✓ ` prefix (green when color is on).
    pub fn success(&self, msg: &str) -> String {
        self.status_line("✓", Color::Green, msg)
    }

    /// Render a warning line: `⚠ ` prefix (yellow when color is on).
    pub fn warn(&self, msg: &str) -> String {
        self.status_line("⚠", Color::Yellow, msg)
    }

    /// Render a failure line: `✗ ` prefix (red when color is on).
    pub fn fail(&self, msg: &str) -> String {
        self.status_line("✗", Color::Red, msg)
    }

    /// Render an info line: `• ` prefix (blue when color is on).
    pub fn info(&self, msg: &str) -> String {
        self.status_line("•", Color::Blue, msg)
    }

    /// Render a plain line (no styling, no padding): file lists, benchmark
    /// hardware lines, footer counts (design D4).
    pub fn line(&self, text: &str) -> String {
        text.to_string()
    }

    /// Apply a semantic color (design D2): owo-colors SGR escapes when
    /// color is on, the input unchanged otherwise. Callers use this for
    /// per-cell styling (e.g. the STATUS column) before passing rows to
    /// [`Console::table`]; tabled's `ansi` feature measures the escapes.
    pub fn style(&self, text: &str, c: Color) -> String {
        if !self.color {
            return text.to_string();
        }
        text.style(c.owo_style()).to_string()
    }

    /// `symbol + " " + msg` with the symbol colored (no color when off).
    fn status_line(&self, symbol: &str, color: Color, msg: &str) -> String {
        format!("{} {msg}", self.style(symbol, color))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    /// A color-enabled console with a fixed width (tests only; production
    /// gating lives in [`Console::stdout`]).
    fn colored(max_width: usize) -> Console {
        Console {
            color: true,
            max_width,
        }
    }

    /// Strip SGR escape sequences (`\x1b[...x`) from `text`.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// Display width of a line (ANSI excluded); all test content is narrow.
    fn display_width(line: &str) -> usize {
        strip_ansi(line).chars().count()
    }

    fn sample_headers() -> &'static [&'static str] {
        &["NAME", "DISPLAY", "DIM"]
    }

    fn sample_rows() -> Vec<Vec<String>> {
        vec![
            vec![
                "bge-m3".to_string(),
                "BGE M3".to_string(),
                "1024".to_string(),
            ],
            vec![
                "bge-small".to_string(),
                "BGE Small".to_string(),
                "384".to_string(),
            ],
        ]
    }

    #[test]
    fn table_plain_contains_box_drawing_and_headers() {
        let console = Console::plain(120);
        let out = console.table(sample_headers(), &sample_rows(), &[2]);
        assert!(out.contains('┌'));
        assert!(out.contains('┬'));
        assert!(out.contains('┐'));
        assert!(out.contains("NAME"));
        assert!(out.contains("DISPLAY"));
        assert!(out.contains("DIM"));
    }

    #[test]
    fn table_plain_has_no_ansi_escapes() {
        let console = Console::plain(120);
        let out = console.table(sample_headers(), &sample_rows(), &[2]);
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn table_color_console_unstyled_cells_have_no_ansi() {
        let console = colored(120);
        let out = console.table(sample_headers(), &sample_rows(), &[2]);
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn table_right_aligned_column_is_right_aligned() {
        let console = Console::plain(120);
        let out = console.table(sample_headers(), &sample_rows(), &[2]);
        // DIM column width is 4 (max of DIM/1024/384), padded by 1 on each
        // side: "1024" fills it, "384" gets a leading space, header aligns.
        assert!(out.lines().any(|line| line.contains(" 1024 ")));
        assert!(out.lines().any(|line| line.contains("  384 ")));
        assert!(out.lines().any(|line| line.contains("  DIM ")));
    }

    #[test]
    fn kv_aligns_values_with_same_label_padding() {
        let console = Console::plain(120);
        let out = console.kv(&[
            ("Name", "alpha"),
            ("Model", "beta"),
            ("Vector Dim", "gamma"),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Name:"));
        assert!(lines[1].starts_with("Model:"));
        assert!(lines[2].starts_with("Vector Dim:"));
        let name_at = lines[0].find("alpha").expect("alpha present");
        let model_at = lines[1].find("beta").expect("beta present");
        let dim_at = lines[2].find("gamma").expect("gamma present");
        assert_eq!(name_at, model_at);
        assert_eq!(name_at, dim_at);
        // borderless: no box-drawing characters
        assert!(!out.contains('┌'));
        assert!(!out.contains('│'));
    }

    #[test]
    fn kv_wraps_long_values_to_max_width() {
        let console = Console::plain(40);
        let out = console.kv(&[("Key", &"word ".repeat(20))]);
        assert!(out.lines().all(|line| display_width(line) <= 40));
        assert_eq!(out.matches("word").count(), 20);
    }

    #[test]
    fn header_and_status_lines_plain() {
        let console = Console::plain(120);
        assert_eq!(console.header("Available Models:"), "Available Models:");
        assert_eq!(console.success("done"), "✓ done");
        assert_eq!(console.warn("slow"), "⚠ slow");
        assert_eq!(console.fail("boom"), "✗ boom");
        assert_eq!(console.info("note"), "• note");
        assert_eq!(console.line("  files: 3"), "  files: 3");
    }

    #[test]
    fn style_applies_sgr_escapes_when_color_on() {
        let console = colored(120);
        assert_eq!(console.style("s", Color::Green), "\u{1b}[32ms\u{1b}[0m");
        assert_eq!(console.style("s", Color::Red), "\u{1b}[31ms\u{1b}[0m");
        assert_eq!(console.style("s", Color::Yellow), "\u{1b}[33ms\u{1b}[0m");
        assert_eq!(console.style("s", Color::Blue), "\u{1b}[34ms\u{1b}[0m");
        assert_eq!(console.style("s", Color::Dim), "\u{1b}[2ms\u{1b}[0m");
        assert_eq!(console.style("s", Color::Bold), "\u{1b}[1ms\u{1b}[0m");
        assert!(console.success("x").contains("\u{1b}[32m"));
        assert_eq!(console.header("Title"), "\u{1b}[1mTitle\u{1b}[0m");
    }

    #[test]
    fn style_is_noop_when_color_off() {
        let console = Console::plain(120);
        assert_eq!(console.style("s", Color::Red), "s");
        assert_eq!(console.header("Title"), "Title");
    }

    #[test]
    fn table_styled_cell_keeps_column_alignment() {
        let console = colored(120);
        let styled = console.style("AAAA", Color::Red);
        assert!(styled.contains("\u{1b}[31m"));
        let rows = vec![
            vec!["AAAA".to_string(), "second-plain".to_string()],
            vec![styled, "second-styled".to_string()],
        ];
        let out = console.table(&["C1", "C2"], &rows, &[]);
        let plain_line = out
            .lines()
            .find(|line| line.contains("second-plain"))
            .expect("plain row rendered");
        let styled_line = out
            .lines()
            .find(|line| line.contains("second-styled"))
            .expect("styled row rendered");
        // The ansi feature measures through the escapes: the second column
        // starts at the same display offset in both rows (equal plain
        // length in the first cell).
        let plain_at = strip_ansi(plain_line)
            .find("second-plain")
            .expect("value present");
        let styled_at = strip_ansi(styled_line)
            .find("second-styled")
            .expect("value present");
        assert_eq!(plain_at, styled_at);
    }

    #[test]
    fn table_wraps_long_cell_without_truncation() {
        let console = Console::plain(60);
        // 300-char cell (space-free so the wrapped character sequence can
        // be compared exactly against the original).
        let cell = "abcdef".repeat(50);
        assert_eq!(cell.len(), 300);
        let out = console.table(&["TEXT"], &[vec![cell.clone()]], &[]);
        assert!(
            out.lines().all(|line| display_width(line) <= 60),
            "every line must fit 60 display columns:\n{out}"
        );
        // wrapped, not truncated: the body lines (border-stripped, padding
        // spaces removed) reproduce the cell character for character.
        let body: String = out
            .lines()
            .filter(|line| line.starts_with('│') && line.contains('a'))
            .map(|line| line.trim_start_matches('│').trim_end_matches('│'))
            .collect::<Vec<_>>()
            .join("");
        let only: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        assert_eq!(only, cell);
    }

    #[test]
    fn table_narrow_max_width_renders_without_panic() {
        // The constructor floor applies at Console::stdout() level;
        // plain(30) must still render (and stay within its bound).
        let console = Console::plain(30);
        let out = console.table(&["TEXT"], &[vec!["word ".repeat(60)]], &[]);
        assert!(out.lines().all(|line| display_width(line) <= 30));
    }
}
