use console::{Emoji, Style};

mod table;
pub use table::{cell, cell_sem, header, table};

mod spinner;
pub use spinner::spinner;

mod logpane;
pub use logpane::LogPane;

mod pipeline;
pub use pipeline::Pipeline;

mod banner;
pub use banner::{brand_banner, deploy_stamp, stderr_is_tty, stdout_is_tty, StampOutcome};
mod theme;

fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// Call once at process start. `console` implements the clicolors spec
/// (`CLICOLOR`/`CLICOLOR_FORCE`) and TTY detection on its own, but does not
/// check `NO_COLOR` — this stays consistent with clap's own `--help`
/// coloring, which is `NO_COLOR`-aware transitively via `anstream`.
pub fn init_colors() {
    if no_color_requested() {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }
}

/// Marker glyph for message lines. The glyph pair comes from the active
/// theme; `console::Emoji` degrades to the ASCII form on terminals without
/// unicode support.
fn marker() -> Emoji<'static, 'static> {
    let t = theme::theme();
    Emoji(t.marker.0, t.marker.1)
}

/// Semantic role — the single source of truth for what a colour *means*.
/// Rendered to `console::Style` here and to `comfy_table::Color` in `table.rs`,
/// so one role stays consistent across messages, the pane, and tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sem {
    Success,
    Error,
    Warn,
    Muted,
    Accent,
    Neutral,
    Frame,
}

/// Role -> terminal style for `console`-rendered output (messages, pane).
pub(crate) fn console_style(sem: Sem) -> Style {
    let t = theme::theme();
    let s = Style::new();
    match sem {
        Sem::Success => t.success.console(),
        Sem::Error => t.error.console(),
        Sem::Warn => t.warn.console(),
        Sem::Accent => t.accent.console(),
        Sem::Muted => s.dim(),
        Sem::Neutral => s,
        Sem::Frame => s.black().bright(),
    }
}

/// One stderr status line: a bold marker + colour-tinted text. Under a
/// `marker_accent` theme the marker is always accent (the brand mark);
/// otherwise it follows the line's own semantic colour.
fn stderr_line(sem: Sem, msg: &str) -> String {
    let marker_sem = if theme::theme().marker_accent {
        Sem::Accent
    } else {
        sem
    };
    format!(
        "{} {}",
        console_style(marker_sem)
            .for_stderr()
            .bold()
            .apply_to(marker()),
        console_style(sem).for_stderr().apply_to(msg)
    )
}

pub fn success(msg: impl std::fmt::Display) {
    eprintln!("{}", stderr_line(Sem::Success, &msg.to_string()));
}

pub fn error(msg: impl std::fmt::Display) {
    eprintln!("{}", stderr_line(Sem::Error, &msg.to_string()));
}

pub fn warn(msg: impl std::fmt::Display) {
    eprintln!("{}", stderr_line(Sem::Warn, &msg.to_string()));
}

pub fn note(msg: impl std::fmt::Display) {
    eprintln!(
        "{} {}",
        console_style(Sem::Accent)
            .for_stderr()
            .bold()
            .apply_to(marker()),
        console_style(Sem::Muted)
            .for_stderr()
            .apply_to(format!("note: {msg}"))
    );
}

pub fn heading(msg: impl std::fmt::Display) {
    let s = console_style(Sem::Accent).bold();
    println!("{} {}", s.clone().apply_to(marker()), s.apply_to(msg));
}

/// Informational stdout line (answer content, not a status verdict):
/// accent bold marker + untinted text.
fn info_line(msg: &str) -> String {
    format!(
        "{} {}",
        console_style(Sem::Accent).bold().apply_to(marker()),
        msg
    )
}

pub fn info(msg: impl std::fmt::Display) {
    println!("{}", info_line(&msg.to_string()));
}

/// Progress/status stderr line: accent bold marker + untinted text.
fn status_line(msg: &str) -> String {
    format!(
        "{} {}",
        console_style(Sem::Accent)
            .for_stderr()
            .bold()
            .apply_to(marker()),
        msg
    )
}

pub fn status(msg: impl std::fmt::Display) {
    eprintln!("{}", status_line(&msg.to_string()));
}

/// One line of a streamed remote payload, printed to stdout verbatim as it
/// arrives: no frame, no scroll window, no truncation to the terminal width.
/// Used by `rpi command`, where the remote output *is* the data the operator
/// came to read, so it must survive redirection and long lines intact.
///
/// Only layout-breaking control sequences are dropped (cursor movement, erase,
/// scroll — see `sanitize_line`); colour is passed through untouched.
pub fn log_line(line: &str) {
    let stdout = std::io::stdout();
    write_log_line(line, &mut stdout.lock());
}

fn write_log_line(line: &str, out: &mut impl std::io::Write) {
    // A closed/broken pipe downstream is not this layer's problem to report:
    // the exit code still carries the remote command's verdict.
    let _ = writeln!(out, "{}", pi_infrastructure::process::sanitize_line(line));
}

/// Print the deploy logo banner to stderr, but only on an interactive
/// terminal — under a pipe, file, or CI it is skipped so logs stay clean.
pub fn show_deploy_banner(project: &str) {
    if banner::stderr_is_tty() {
        eprintln!("{}", banner::deploy_banner(project));
    }
}

/// Pure, string-returning variants for callers that build up a `String`
/// instead of printing directly (e.g. `render_doctor`).
pub fn styled_ok(text: &str) -> String {
    console_style(Sem::Success).apply_to(text).to_string()
}

pub fn styled_err(text: &str) -> String {
    console_style(Sem::Error).bold().apply_to(text).to_string()
}

/// Accent-tinted copy of `text` for `console`-rendered stdout (e.g. the stats
/// sparkline); plain text when colours are disabled (tests/pipes), same as
/// `styled_ok`/`styled_err`.
pub fn styled_accent(text: &str) -> String {
    console_style(Sem::Accent).apply_to(text).to_string()
}

/// Accent foreground for `ratatui` widgets (the stats chart line), from the
/// active theme; `None` when the accent is a no-op paint. Truecolor vs
/// xterm-256 follows the same detection as tables, so the chart matches the
/// rest of the branded output.
pub fn accent_ratatui_color() -> Option<ratatui::style::Color> {
    theme::theme()
        .accent
        .ratatui_color(theme::truecolor_enabled())
}

/// True when 24-bit color should be emitted (colors on + truecolor terminal).
/// Public wrapper so `ratatui` callers can build a matching grey palette.
pub fn truecolor_enabled() -> bool {
    theme::truecolor_enabled()
}

/// Memory-series green (`#75A928`) for `ratatui`; exact under truecolor,
/// nearest xterm-256 otherwise.
pub fn mem_ratatui_color() -> Option<ratatui::style::Color> {
    theme::Paint::Rgb(117, 169, 40).ratatui_color(theme::truecolor_enabled())
}

/// Temperature-series magenta (`#D96AE0`) for `ratatui`.
pub fn temp_ratatui_color() -> Option<ratatui::style::Color> {
    theme::Paint::Rgb(217, 106, 224).ratatui_color(theme::truecolor_enabled())
}

/// Semantic role -> `ratatui` color, following the active theme; `None` for
/// non-colored roles (Muted/Neutral/Frame). Mirrors `table.rs::sem_colour`.
pub fn sem_ratatui_color(sem: Sem) -> Option<ratatui::style::Color> {
    let t = theme::theme();
    let tc = theme::truecolor_enabled();
    match sem {
        Sem::Success => t.success.ratatui_color(tc),
        Sem::Warn => t.warn.ratatui_color(tc),
        Sem::Error => t.error.ratatui_color(tc),
        Sem::Accent => t.accent.ratatui_color(tc),
        Sem::Muted | Sem::Neutral | Sem::Frame => None,
    }
}

/// Container state (+ optional health) -> semantic role.
/// running/healthy = success; restarting/paused/created or a non-healthy
/// healthcheck = warn; everything else (exited, dead, unknown) = error.
pub fn status_sem(state: &str, health: Option<&str>) -> Sem {
    if matches!(health, Some("unhealthy") | Some("starting")) {
        return Sem::Warn;
    }
    match state {
        "running" => Sem::Success,
        "restarting" | "created" | "paused" => Sem::Warn,
        _ => Sem::Error,
    }
}

/// Resource-usage percentage -> semantic role: >=90 error, >=70 warn, else none.
pub fn usage_sem(percent: f64) -> Sem {
    if percent >= 90.0 {
        Sem::Error
    } else if percent >= 70.0 {
        Sem::Warn
    } else {
        Sem::Neutral
    }
}

/// Aggregate of a project's service states -> the worst role among them
/// (Error worse than Warn worse than Success); empty = Neutral.
pub fn services_sem(states: &[&str]) -> Sem {
    fn rank(sem: Sem) -> u8 {
        match sem {
            Sem::Error => 3,
            Sem::Warn => 2,
            Sem::Success => 1,
            _ => 0,
        }
    }
    states
        .iter()
        .map(|s| status_sem(s, None))
        .max_by_key(|s| rank(*s))
        .unwrap_or(Sem::Neutral)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_env_var_is_detected() {
        std::env::set_var("NO_COLOR", "1");
        assert!(no_color_requested());
        std::env::remove_var("NO_COLOR");
        assert!(!no_color_requested());
    }

    #[test]
    fn styled_ok_and_err_are_plain_text_when_colors_disabled() {
        // Test stdout isn't a TTY, so `console` auto-disables styling: the
        // returned string must be exactly the input, no ANSI codes.
        assert_eq!(styled_ok("PASS"), "PASS");
        assert_eq!(styled_err("FAIL"), "FAIL");
        // The sparkline accent tint is plain text off a TTY, same contract.
        assert_eq!(styled_accent("▁▂▃"), "▁▂▃");
    }

    #[test]
    fn log_line_keeps_long_lines_whole_and_drops_layout_control() {
        // The `rpi command` contract: whatever the container printed reaches
        // stdout unabridged. A line far wider than any terminal must not be
        // truncated, while a cursor-erase sequence (which would corrupt the
        // scrollback) is stripped and colour survives.
        let long = "x".repeat(400);
        let mut out = Vec::new();
        write_log_line(&format!("\x1b[2K\x1b[32m{long}\x1b[0m"), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("\x1b[32m{long}\x1b[0m\n")
        );
    }

    #[test]
    fn stderr_line_is_plain_text_when_colours_disabled() {
        // Captured test output is not a TTY, so console styling is disabled:
        // the composed line must carry the text and no ANSI escape bytes.
        let line = stderr_line(Sem::Error, "boom");
        assert!(line.contains("boom"), "{line:?}");
        assert!(!line.contains('\u{1b}'), "no ANSI when disabled: {line:?}");
    }

    #[test]
    fn console_style_neutral_is_a_no_op() {
        assert_eq!(console_style(Sem::Neutral).apply_to("x").to_string(), "x");
    }

    #[test]
    fn usage_sem_thresholds() {
        assert_eq!(usage_sem(10.0), Sem::Neutral);
        assert_eq!(usage_sem(70.0), Sem::Warn);
        assert_eq!(usage_sem(89.9), Sem::Warn);
        assert_eq!(usage_sem(90.0), Sem::Error);
    }

    #[test]
    fn status_sem_maps_states_and_health() {
        assert_eq!(status_sem("running", None), Sem::Success);
        assert_eq!(status_sem("running", Some("unhealthy")), Sem::Warn);
        assert_eq!(status_sem("restarting", None), Sem::Warn);
        assert_eq!(status_sem("exited", None), Sem::Error);
    }

    #[test]
    fn services_sem_takes_the_worst_state() {
        assert_eq!(services_sem(&["running", "running"]), Sem::Success);
        assert_eq!(services_sem(&["running", "restarting"]), Sem::Warn);
        assert_eq!(services_sem(&["running", "exited"]), Sem::Error);
        assert_eq!(services_sem(&[]), Sem::Neutral);
    }

    #[test]
    fn info_and_status_lines_carry_text_and_no_ansi_when_disabled() {
        // Captured test output is not a TTY, so console styling is off.
        let line = info_line("hello world");
        assert!(line.contains("hello world"), "{line:?}");
        assert!(!line.contains('\u{1b}'), "{line:?}");
        let line = status_line("working...");
        assert!(line.contains("working..."), "{line:?}");
        assert!(!line.contains('\u{1b}'), "{line:?}");
    }

    #[test]
    fn marker_renders_one_of_the_active_theme_glyphs() {
        let t = theme::theme();
        let s = marker().to_string();
        assert!(s == t.marker.0 || s == t.marker.1, "{s:?}");
    }

    #[test]
    fn metric_colors_are_exact_rgb_under_truecolor() {
        use ratatui::style::Color;
        assert_eq!(
            theme::Paint::Rgb(117, 169, 40).ratatui_color(true),
            Some(Color::Rgb(117, 169, 40))
        );
        assert_eq!(
            theme::Paint::Rgb(217, 106, 224).ratatui_color(true),
            Some(Color::Rgb(217, 106, 224))
        );
    }

    #[test]
    fn sem_ratatui_color_maps_status_roles() {
        // running -> Success -> some color; unknown neutral -> None
        assert!(sem_ratatui_color(Sem::Success).is_some());
        assert_eq!(sem_ratatui_color(Sem::Neutral), None);
        assert_eq!(sem_ratatui_color(Sem::Muted), None);
    }
}
