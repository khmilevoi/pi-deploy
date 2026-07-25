use console::Term;

use console::Style;

use super::{console_style, Sem};

/// Colours for one rendered frame: the box glyphs and the label can differ
/// (grey box + cyan label while streaming; all-red on failure).
struct FrameStyle {
    border: Style,
    label: Style,
}

/// Neutral streaming frame: bright-black (grey) border, cyan label.
fn neutral_frame() -> FrameStyle {
    FrameStyle {
        border: console_style(Sem::Frame),
        label: console_style(Sem::Accent),
    }
}

/// Failure frame: border and label both red.
fn err_frame() -> FrameStyle {
    let red = console_style(Sem::Error);
    FrameStyle {
        border: red.clone(),
        label: red,
    }
}

fn top_border(label: &str, width: usize, style: &FrameStyle) -> String {
    // "╭─ " + label + " " + fill + "╮" — visible width == `width`.
    let max_label_width = width.saturating_sub(5);
    let label: String = label.chars().take(max_label_width).collect();
    let prefix_len = 3 + label.chars().count() + 1; // "╭─ " + label + " "
    let fill = width.saturating_sub(prefix_len + 1); // +1 for the closing ╮
    format!(
        "{}{}{}",
        style.border.apply_to("╭─ "),
        style.label.apply_to(&label),
        style.border.apply_to(format!(" {}╮", "─".repeat(fill))),
    )
}

fn side_line(content: &str, width: usize, style: &FrameStyle) -> String {
    // Truncate by *visible* columns: ANSI CSI escape sequences (colours) carry
    // no width, so they pass through without spending the budget. This keeps
    // streamed colour intact while still fitting the box.
    let inner_width = width.saturating_sub(4); // "│ " + " │"
    let mut truncated = String::new();
    let mut visible = 0;
    let mut had_escape = false;
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            had_escape = true;
            truncated.push(c);
            truncated.push(chars.next().unwrap()); // '['
            for c in chars.by_ref() {
                truncated.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if visible == inner_width {
                break;
            }
            truncated.push(c);
            visible += 1;
        }
    }
    // If colour was used and truncation dropped its reset, close it ourselves
    // so it can't bleed onto the padding, the border, or the next line.
    if had_escape && !truncated.ends_with("\x1b[0m") {
        truncated.push_str("\x1b[0m");
    }
    let pad = " ".repeat(inner_width.saturating_sub(visible));
    format!(
        "{}{truncated}{pad}{}",
        style.border.apply_to("│ "),
        style.border.apply_to(" │"),
    )
}

fn bottom_border(width: usize, style: &FrameStyle) -> String {
    style
        .border
        .apply_to(format!("╰{}╯", "─".repeat(width.saturating_sub(2))))
        .to_string()
}

/// Builds the whole pane as a single string ready for one atomic write.
///
/// The anti-flicker trick: instead of clearing the previous block and then
/// redrawing it (which leaves a visible blank frame between the two steps and
/// spreads the update across many small writes), we move the cursor back up to
/// the top of the previous block and rewrite every row *in place*, erasing each
/// to end-of-line first. Because it is one buffer flushed in one write, the
/// terminal applies the update in a single repaint — no tearing, no flicker.
fn render_frame(
    label: &str,
    visible: &std::collections::VecDeque<String>,
    width: usize,
    prev_rendered: usize,
    style: &FrameStyle,
) -> String {
    let mut rows = Vec::with_capacity(visible.len() + 2);
    rows.push(top_border(label, width, style));
    for l in visible {
        rows.push(side_line(l, width, style));
    }
    rows.push(bottom_border(width, style));

    let mut buf = String::new();
    // Return to the top of the previously drawn block (nothing to return to on
    // the first paint).
    if prev_rendered > 0 {
        buf.push_str(&format!("\x1b[{prev_rendered}A"));
    }
    for row in &rows {
        buf.push_str("\r\x1b[2K"); // column 0 + erase the old row in place
        buf.push_str(row);
        buf.push_str("\r\n");
    }
    // If the block shrank, wipe the now-stale rows left below it, then move the
    // cursor back up so it ends right beneath the (smaller) block — keeping the
    // "cursor sits just below the block" invariant the next redraw relies on.
    let shrink = prev_rendered.saturating_sub(rows.len());
    for _ in 0..shrink {
        buf.push_str("\r\x1b[2K\r\n");
    }
    if shrink > 0 {
        buf.push_str(&format!("\x1b[{shrink}A"));
    }
    buf
}

use pi_infrastructure::process::sanitize_line;

pub struct LogPane {
    term: Term,
    interactive: bool,
    label: String,
    max_visible: usize,
    visible: std::collections::VecDeque<String>,
    full: Vec<String>,
    rendered: usize,
    print_line: Box<dyn Fn(&str)>,
}

impl LogPane {
    pub fn new(label: impl Into<String>, max_visible: usize) -> Self {
        let term = Term::stdout();
        let interactive = term.features().is_attended();
        Self {
            term,
            interactive,
            label: label.into(),
            max_visible,
            visible: std::collections::VecDeque::new(),
            full: Vec::new(),
            rendered: 0,
            print_line: Box::new(|l: &str| println!("{l}")),
        }
    }

    #[cfg(test)]
    fn new_non_interactive(label: impl Into<String>, max_visible: usize) -> Self {
        let mut pane = Self::new(label, max_visible);
        pane.interactive = false;
        pane
    }

    #[cfg(test)]
    pub(crate) fn new_recording(
        label: impl Into<String>,
        max_visible: usize,
        interactive: bool,
        printed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        let mut pane = Self::new(label, max_visible);
        pane.interactive = interactive;
        pane.print_line = Box::new(move |l: &str| printed.lock().unwrap().push(l.to_string()));
        pane
    }

    pub fn push_line(&mut self, line: &str) {
        let clean = sanitize_line(line);
        self.full.push(clean.clone());
        if !self.interactive {
            (self.print_line)(&clean);
            return;
        }
        self.visible.push_back(clean);
        if self.visible.len() > self.max_visible {
            self.visible.pop_front();
        }
        self.redraw();
    }

    fn redraw(&mut self) {
        let (_, cols) = self.term.size();
        let width = (cols as usize).max(20);
        // One buffer, one write, one repaint — see `render_frame` for why this
        // is what kills the flicker.
        let frame = render_frame(
            &self.label,
            &self.visible,
            width,
            self.rendered,
            &neutral_frame(),
        );
        let _ = self.term.write_str(&frame);
        let _ = self.term.flush();
        self.rendered = self.visible.len() + 2; // + top and bottom border
    }

    pub fn finish_ok(self, summary: &str) {
        if self.interactive {
            let _ = self.term.clear_last_lines(self.rendered);
        }
        crate::output::success(summary);
    }

    /// Neutral outcome (e.g. deploy superseded) — same as success: clear, no dump.
    pub fn finish_neutral(self, summary: &str) {
        if self.interactive {
            let _ = self.term.clear_last_lines(self.rendered);
        }
        crate::output::note(summary);
    }

    /// Erase the live frame without printing anything — used by the deploy
    /// pipeline to collapse a stage pane before printing its own summary line.
    #[allow(dead_code)]
    pub fn clear(self) {
        if self.interactive {
            let _ = self.term.clear_last_lines(self.rendered);
        }
    }

    /// Failure treatment without a summary line: recolour the frame red in
    /// place and dump the full captured log under a `— {dump_label} —`
    /// separator. Non-interactive runs already streamed every line.
    pub fn abort(self, dump_label: &str) {
        if self.interactive {
            let (_, cols) = self.term.size();
            let width = (cols as usize).max(20);
            let frame = render_frame(
                &self.label,
                &self.visible,
                width,
                self.rendered,
                &err_frame(),
            );
            let _ = self.term.write_str(&frame);
            let _ = self.term.flush();
            (self.print_line)(
                &console_style(Sem::Muted)
                    .apply_to(format!("— {dump_label} —"))
                    .to_string(),
            );
            for l in &self.full {
                (self.print_line)(l);
            }
        }
    }

    pub fn finish_err(self, summary: &str) {
        self.abort("full log");
        crate::output::error(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_border_wraps_label_and_fills_width() {
        let line = top_border("build", 20, &neutral_frame());
        assert!(line.starts_with("╭─ build "), "{line}");
        assert!(line.ends_with('╮'), "{line}");
        assert_eq!(line.chars().count(), 20, "{line}");
    }

    #[test]
    fn side_line_pads_short_content_to_full_width() {
        let line = side_line("hi", 10, &neutral_frame());
        assert_eq!(line.chars().count(), 10, "{line}");
        assert!(line.starts_with("│ hi"), "{line}");
        assert!(line.ends_with(" │"), "{line}");
    }

    #[test]
    fn side_line_truncates_content_wider_than_the_box() {
        let line = side_line("this is way too long for the box", 10, &neutral_frame());
        assert_eq!(line.chars().count(), 10, "{line}");
        assert!(line.starts_with('│'), "{line}");
        assert!(line.ends_with('│'), "{line}");
    }

    #[test]
    fn side_line_ignores_color_codes_when_measuring_width() {
        // "hello" is 5 visible columns; the SGR codes must not count toward the
        // inner width (10 - 4 = 6), so all of "hello" survives and the line is
        // padded to a visible width of 10. A reset already terminates the
        // colour, so no extra one is appended.
        let line = side_line("\x1b[31mhello\x1b[0m", 10, &neutral_frame());
        assert_eq!(line, "│ \x1b[31mhello\x1b[0m  │", "{line:?}");
    }

    #[test]
    fn side_line_resets_color_when_truncation_drops_the_reset() {
        // Truncation cuts the line before its own reset; side_line must append
        // one so colour cannot bleed onto the border or following lines.
        let line = side_line("\x1b[32mthis is way too long\x1b[0m", 10, &neutral_frame());
        assert_eq!(line, "│ \x1b[32mthis i\x1b[0m │", "{line:?}");
    }

    #[test]
    fn top_border_truncates_a_label_wider_than_the_box() {
        let line = top_border("command 'run-full-integration-suite'", 40, &neutral_frame());
        assert_eq!(line.chars().count(), 40, "{line}");
        assert!(line.starts_with("╭─ "), "{line}");
        assert!(line.ends_with('╮'), "{line}");
    }

    #[test]
    fn render_frame_first_paint_does_not_move_the_cursor() {
        let visible = std::collections::VecDeque::new();
        let frame = render_frame("build", &visible, 20, 0, &neutral_frame());
        // Nothing was drawn before, so there is no previous block to return to.
        assert!(
            !frame.starts_with("\x1b["),
            "no cursor-up on first paint: {frame:?}"
        );
        assert!(frame.contains("╭─ build"), "{frame:?}");
    }

    #[test]
    fn render_frame_overwrites_previous_block_in_place() {
        let mut visible = std::collections::VecDeque::new();
        visible.push_back("hello".to_string());
        let frame = render_frame("build", &visible, 20, 3, &neutral_frame());
        // Returns to the top of the 3-line previous block instead of clearing
        // it first — the whole update lands as one write, so no blank frame.
        assert!(frame.starts_with("\x1b[3A"), "{frame:?}");
        // Each row erases to end of line as it is rewritten in place.
        assert!(frame.contains("\x1b[2K"), "{frame:?}");
        assert!(frame.contains("hello"), "{frame:?}");
    }

    #[test]
    fn bottom_border_matches_width() {
        let line = bottom_border(12, &neutral_frame());
        assert_eq!(line.chars().count(), 12, "{line}");
        assert!(line.starts_with('╰'), "{line}");
        assert!(line.ends_with('╯'), "{line}");
    }

    #[test]
    fn non_interactive_pane_streams_immediately_and_keeps_full_history() {
        let mut pane = LogPane::new_non_interactive("test", 3);
        pane.push_line("one");
        pane.push_line("two");
        pane.push_line("three");
        pane.push_line("four");
        assert_eq!(pane.full, vec!["one", "two", "three", "four"]);
        assert!(pane.visible.is_empty(), "interactive-only buffer untouched");
        assert_eq!(pane.rendered, 0, "no live redraw happened");
    }

    #[test]
    fn push_line_sanitizes_before_storing() {
        let mut pane = LogPane::new_non_interactive("test", 3);
        pane.push_line("\x1b[2K\rstep 4/9");
        assert_eq!(pane.full, vec!["step 4/9"]);
    }

    #[test]
    fn finish_err_does_not_reprint_lines_already_streamed_live() {
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pane = LogPane::new_recording("test", 3, false, printed.clone());
        pane.push_line("one");
        pane.push_line("two");
        pane.finish_err("boom");
        assert_eq!(*printed.lock().unwrap(), vec!["one", "two"]);
    }

    #[test]
    fn finish_err_dumps_full_history_when_interactive() {
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pane = LogPane::new_recording("test", 3, true, printed.clone());
        pane.push_line("one");
        pane.push_line("two");
        pane.finish_err("boom");
        assert_eq!(*printed.lock().unwrap(), vec!["— full log —", "one", "two"]);
    }

    #[test]
    fn borders_are_plain_when_colours_disabled() {
        // Non-TTY test env => console styling disabled => the styled frame
        // pieces must be byte-identical to the unstyled box (no ANSI).
        let fs = neutral_frame();
        assert!(
            !top_border("build", 20, &fs).contains('\u{1b}'),
            "no ANSI in top"
        );
        assert!(
            !bottom_border(12, &fs).contains('\u{1b}'),
            "no ANSI in border"
        );
        assert!(
            !side_line("hi", 10, &fs).contains('\u{1b}'),
            "no ANSI in side"
        );
    }

    #[test]
    fn clear_prints_nothing_through_print_line() {
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pane = LogPane::new_recording("test", 3, true, printed.clone());
        pane.push_line("one");
        pane.clear();
        assert!(printed.lock().unwrap().is_empty());
    }

    #[test]
    fn abort_dumps_history_under_a_custom_label() {
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pane = LogPane::new_recording("build", 3, true, printed.clone());
        pane.push_line("step 1");
        pane.push_line("boom");
        pane.abort("build log");
        assert_eq!(
            *printed.lock().unwrap(),
            vec!["— build log —", "step 1", "boom"]
        );
    }

    #[test]
    fn abort_non_interactive_adds_nothing_lines_already_streamed() {
        let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pane = LogPane::new_recording("build", 3, false, printed.clone());
        pane.push_line("one");
        pane.abort("build log");
        assert_eq!(*printed.lock().unwrap(), vec!["one"]);
    }
}
