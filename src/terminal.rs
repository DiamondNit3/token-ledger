//! Shared terminal presentation primitives.
//!
//! Human rendering is deliberately separated from accounting and machine
//! schemas. ANSI styling, responsive layout, and compact display precision
//! must never change persisted values or JSON/CSV output.

use std::borrow::Cow;
use std::env;
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;
use std::time::Duration;

use anstream::{AutoStream, ColorChoice as StreamColorChoice};
use anstyle::{AnsiColor, Style};
use comfy_table::{CellAlignment, ContentArrangement, Table, presets};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rust_decimal::Decimal;
use terminal_size::{Width, terminal_size};

static CURRENT: OnceLock<TerminalOptions> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnicodeChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Wide,
    Compact,
    Narrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Accent,
    Success,
    Warning,
    Error,
    Muted,
    Strong,
}

#[derive(Debug, Clone)]
pub struct TerminalOptions {
    pub width: usize,
    pub color: bool,
    pub unicode: bool,
    pub plain: bool,
    pub details: bool,
    pub interactive: bool,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            width: 100,
            color: false,
            unicode: true,
            plain: false,
            details: false,
            interactive: false,
        }
    }
}

impl TerminalOptions {
    pub fn plain(width: usize) -> Self {
        Self {
            width: width.max(40),
            color: false,
            unicode: false,
            plain: true,
            details: false,
            interactive: false,
        }
    }

    pub fn styled(width: usize) -> Self {
        Self {
            width: width.max(40),
            color: true,
            unicode: true,
            plain: false,
            details: false,
            interactive: true,
        }
    }

    pub fn detect(
        color_choice: ColorChoice,
        unicode_choice: UnicodeChoice,
        plain: bool,
        details: bool,
        machine_output: bool,
    ) -> Self {
        let stdout_tty = io::stdout().is_terminal();
        let stderr_tty = io::stderr().is_terminal();
        let terminal_is_dumb = env::var("TERM").is_ok_and(|value| value == "dumb");
        let no_color = env::var("NO_COLOR").is_ok_and(|value| !value.is_empty());
        let width = env::var("TOKEN_LEDGER_WIDTH")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .or_else(|| terminal_size().map(|(Width(width), _)| usize::from(width)))
            .unwrap_or(100)
            .max(40);
        let color = !plain
            && !machine_output
            && match color_choice {
                ColorChoice::Always => true,
                ColorChoice::Never => false,
                ColorChoice::Auto => stdout_tty && !terminal_is_dumb && !no_color,
            };
        let unicode = !plain
            && !machine_output
            && match unicode_choice {
                UnicodeChoice::Always => true,
                UnicodeChoice::Never => false,
                UnicodeChoice::Auto => stdout_tty && !terminal_is_dumb,
            };
        Self {
            width,
            color,
            unicode,
            plain,
            details,
            interactive: !plain && !machine_output && stdout_tty && stderr_tty,
        }
    }

    pub fn layout(&self) -> Layout {
        if self.plain || self.width < 72 {
            Layout::Narrow
        } else if self.width < 110 {
            Layout::Compact
        } else {
            Layout::Wide
        }
    }

    pub fn separator(&self) -> &'static str {
        if self.unicode { " · " } else { " | " }
    }

    pub fn range_separator(&self) -> &'static str {
        if self.unicode { "–" } else { ".." }
    }

    pub fn rule(&self, requested: usize) -> String {
        let width = requested.min(self.width.saturating_sub(1)).max(8);
        let character = if self.unicode { '─' } else { '-' };
        character.to_string().repeat(width)
    }

    pub fn paint(&self, tone: Tone, value: impl Display) -> String {
        let value = value.to_string();
        if !self.color {
            return value;
        }
        let style = match tone {
            Tone::Accent => Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold(),
            Tone::Success => Style::new().fg_color(Some(AnsiColor::Green.into())).bold(),
            Tone::Warning => Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold(),
            Tone::Error => Style::new().fg_color(Some(AnsiColor::Red.into())).bold(),
            Tone::Muted => Style::new().dimmed(),
            Tone::Strong => Style::new().bold(),
        };
        format!("{}{value}{}", style.render(), style.render_reset())
    }

    pub fn badge(&self, label: &str, tone: Tone) -> String {
        self.paint(tone, format!("[{label}]"))
    }

    pub fn status_symbol(&self, tone: Tone) -> &'static str {
        match (self.unicode, tone) {
            (true, Tone::Success) => "✓",
            (true, Tone::Warning) => "!",
            (true, Tone::Error) => "×",
            (true, _) => "•",
            (false, Tone::Success) => "OK",
            (false, Tone::Warning) => "WARN",
            (false, Tone::Error) => "ERROR",
            (false, _) => "INFO",
        }
    }

    pub fn emit_stdout(&self, text: &str) -> io::Result<()> {
        let choice = if self.color {
            StreamColorChoice::Always
        } else {
            StreamColorChoice::Never
        };
        let mut stream = AutoStream::new(io::stdout(), choice);
        let text = if self.unicode {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(ascii_fallback(text))
        };
        stream.write_all(text.as_bytes())?;
        stream.flush()
    }

    pub fn emit_stderr(&self, text: &str) -> io::Result<()> {
        let choice = if self.color {
            StreamColorChoice::Always
        } else {
            StreamColorChoice::Never
        };
        let mut stream = AutoStream::new(io::stderr(), choice);
        let text = if self.unicode {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(ascii_fallback(text))
        };
        stream.write_all(text.as_bytes())?;
        stream.flush()
    }

    pub fn spinner(&self, message: impl Into<String>) -> Option<ProgressBar> {
        if !self.interactive {
            return None;
        }
        let bar = ProgressBar::new_spinner();
        bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(12));
        let template = if self.color {
            "{spinner:.cyan} {msg}"
        } else {
            "{spinner} {msg}"
        };
        let style = ProgressStyle::with_template(template)
            .expect("static progress template is valid")
            .tick_strings(if self.unicode {
                &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            } else {
                &["-", "\\", "|", "/"]
            });
        bar.set_style(style);
        bar.set_message(message.into());
        bar.enable_steady_tick(Duration::from_millis(90));
        Some(bar)
    }

    pub fn table(
        &self,
        headers: &[&str],
        rows: impl IntoIterator<Item = Vec<String>>,
        right_aligned_columns: &[usize],
    ) -> String {
        let mut table = Table::new();
        table.load_preset(if self.unicode {
            "││  │─│││           "
        } else {
            presets::ASCII_MARKDOWN
        });
        table.set_content_arrangement(ContentArrangement::Dynamic);
        table.set_width(self.width.saturating_sub(1).min(u16::MAX as usize) as u16);
        table.set_header(headers.iter().copied());
        for row in rows {
            table.add_row(row);
        }
        for index in right_aligned_columns {
            if let Some(column) = table.column_mut(*index) {
                column.set_cell_alignment(CellAlignment::Right);
            }
        }
        if !self.interactive {
            table.force_no_tty();
        }
        table.to_string()
    }
}

pub fn set_current(options: TerminalOptions) {
    let _ = CURRENT.set(options);
}

pub fn current() -> &'static TerminalOptions {
    CURRENT.get_or_init(TerminalOptions::default)
}

pub fn format_count(value: u64) -> String {
    group_digits(&value.to_string())
}

pub fn format_count_compact(value: u64) -> String {
    let (scale, suffix) = if value >= 1_000_000_000 {
        (1_000_000_000_f64, "B")
    } else if value >= 1_000_000 {
        (1_000_000_f64, "M")
    } else if value >= 10_000 {
        (1_000_f64, "K")
    } else {
        return format_count(value);
    };
    let scaled = value as f64 / scale;
    let rendered = format!("{scaled:.2}");
    let rendered = rendered.trim_end_matches('0').trim_end_matches('.');
    format!("{rendered}{suffix}")
}

pub fn format_decimal(value: Decimal, currency: bool) -> String {
    if currency && value == Decimal::ZERO {
        return "$0.00".to_string();
    }
    let absolute = value.abs();
    let decimals = if currency && absolute >= Decimal::ONE {
        2
    } else if currency && absolute >= Decimal::new(1, 2) {
        4
    } else if currency {
        6
    } else if absolute >= Decimal::ONE {
        2
    } else {
        4
    };
    let rounded = value.round_dp(decimals);
    let precision = decimals as usize;
    let mut raw = if currency {
        format!("{rounded:.precision$}")
    } else {
        rounded.normalize().to_string()
    };
    if currency && rounded == Decimal::ZERO && value != Decimal::ZERO {
        return if value.is_sign_negative() {
            "-$<0.000001".to_string()
        } else {
            "$<0.000001".to_string()
        };
    }
    let negative = raw.starts_with('-');
    if negative {
        raw.remove(0);
    }
    let (integer, fraction) = raw
        .split_once('.')
        .map_or((raw.as_str(), None), |(left, right)| (left, Some(right)));
    let mut display = group_digits(integer);
    if let Some(fraction) = fraction
        && !fraction.is_empty()
    {
        display.push('.');
        display.push_str(fraction);
    }
    if currency {
        display.insert(0, '$');
    }
    if negative {
        display.insert(0, '-');
    }
    display
}

pub fn format_percent(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "n/a".to_string();
    }
    format!("{:.1}%", numerator as f64 * 100.0 / denominator as f64)
}

pub fn display_model_name(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "claude-fable-5" => "Claude Fable 5".to_string(),
        "gpt-5.6-sol" => "GPT-5.6 Sol".to_string(),
        _ => value.to_string(),
    }
}

pub fn display_client_name(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "claude" | "claude_code" => "Claude Code".to_string(),
        "codex" | "openai_codex" => "Codex".to_string(),
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        _ => value.to_string(),
    }
}

fn group_digits(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + value.len() / 3);
    for (index, character) in value.chars().enumerate() {
        if index > 0 && (value.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn ascii_fallback(value: &str) -> String {
    value
        .replace('·', "|")
        .replace('–', "..")
        .replace('—', "-")
        .replace('≥', ">=")
        .replace('…', "...")
        .replace('→', "->")
        .replace('✓', "OK")
        .replace('×', "X")
        .replace('•', "*")
        .replace('│', "|")
        .replace('─', "-")
        .replace('═', "=")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_breakpoints_and_plain_mode_are_stable() {
        assert_eq!(TerminalOptions::styled(120).layout(), Layout::Wide);
        assert_eq!(TerminalOptions::styled(90).layout(), Layout::Compact);
        assert_eq!(TerminalOptions::styled(60).layout(), Layout::Narrow);
        assert_eq!(TerminalOptions::plain(140).layout(), Layout::Narrow);
    }

    #[test]
    fn human_numbers_are_grouped_without_changing_values() {
        assert_eq!(format_count(782_107_732), "782,107,732");
        assert_eq!(format_count_compact(782_107_732), "782.11M");
        assert_eq!(
            format_decimal(Decimal::new(11_134_970_295, 7), true),
            "$1,113.50"
        );
        assert_eq!(format_percent(755_913_809, 782_107_732), "96.7%");
    }

    #[test]
    fn color_is_semantic_and_removable() {
        let plain = TerminalOptions::plain(100);
        assert_eq!(plain.badge("RANGE", Tone::Warning), "[RANGE]");
        let styled = TerminalOptions::styled(100);
        assert!(styled.badge("RANGE", Tone::Warning).contains("\u{1b}["));
        assert_eq!(ascii_fallback("✓ 10–20 · ≥5"), "OK 10..20 | >=5");
    }
}
