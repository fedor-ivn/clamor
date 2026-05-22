use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// 8 visually distinct colors for agent identification on dark terminals.
pub const AGENT_COLORS: &[Color] = &[
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Blue,
    Color::Green,
    Color::Red,
    Color::LightCyan,
    Color::LightMagenta,
];

/// Get the color for an agent by its color_index (wraps around).
pub fn agent_color(color_index: u8) -> Color {
    AGENT_COLORS[color_index as usize % AGENT_COLORS.len()]
}

/// Mouse text selection state (pane-relative coordinates).
#[derive(Clone)]
pub struct Selection {
    pub start: (u16, u16), // (col, row)
    pub end: (u16, u16),   // (col, row)
    pub active: bool,      // true while mouse button is held
}

/// Keyboard-driven copy mode state (tmux-style).
pub struct CopyMode {
    pub cursor_col: u16,
    pub cursor_row: u16,            // screen-relative (0 = top of visible area)
    pub anchor: Option<(u16, u16)>, // selection anchor, set on `v`
    pub line_select: bool,          // true = line-wise selection (V mode)
    pub pending_g: bool,            // true after first `g`, waiting for second `g`
}

/// Client-side view of a single PTY pane.
///
/// Does NOT own a PTY -- the daemon does. This struct maintains a vt100 parser
/// that processes output bytes received from the daemon, and tracks scroll state.
///
/// Uses tmux-style freeze semantics: when scrolled up, new output is buffered
/// without touching the parser, so the display stays stable. Output is flushed
/// through the parser when returning to live view.
pub struct PaneView {
    pub parser: vt100::Parser,
    pub scroll_offset: usize,
    pub selection: Option<Selection>,
    pub copy_mode: Option<CopyMode>,
    pending_output: Vec<u8>,
}

impl PaneView {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 50000),
            scroll_offset: 0,
            selection: None,
            copy_mode: None,
            pending_output: Vec::new(),
        }
    }

    /// Create with catch-up data already processed.
    pub fn from_catch_up(rows: u16, cols: u16, catch_up: &[u8]) -> Self {
        let mut parser = vt100::Parser::new(rows, cols, 50000);
        parser.process(catch_up);
        Self {
            parser,
            scroll_offset: 0,
            selection: None,
            copy_mode: None,
            pending_output: Vec::new(),
        }
    }

    /// Feed output bytes (received from daemon) into the vt100 parser.
    ///
    /// When scrolled up (frozen), output is buffered without touching the parser
    /// so the display stays completely stable. Buffered data is flushed when
    /// returning to live view via `snap_to_bottom()`.
    pub fn process_output(&mut self, data: &[u8]) {
        if self.scroll_offset > 0 || self.copy_mode.is_some() {
            self.pending_output.extend_from_slice(data);
        } else {
            self.parser.process(data);
        }
    }

    /// Whether there is buffered output waiting to be flushed.
    pub fn has_pending_output(&self) -> bool {
        !self.pending_output.is_empty()
    }

    /// Resize the virtual terminal.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Apply scrollback offset and return the screen for rendering.
    /// Clamps offset to actual scrollback size.
    pub fn scrolled_screen(&mut self) -> &vt100::Screen {
        self.parser.screen_mut().set_scrollback(self.scroll_offset);
        self.scroll_offset = self.parser.screen().scrollback();
        self.parser.screen()
    }

    /// Whether the app inside the PTY has mouse mode enabled.
    pub fn mouse_mode_active(&self) -> bool {
        self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
    }

    /// Whether the app is using the alternate screen buffer.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Total scrollback lines available (set to MAX, read clamped value).
    pub fn scrollback_len(&mut self) -> usize {
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let len = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(0);
        len
    }

    /// Clear any active text selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Scroll up by `n` lines, clamped to actual scrollback size.
    pub fn scroll_up(&mut self, n: usize) {
        let before = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_add(n);
        let max = self.scrollback_len();
        self.scroll_offset = self.scroll_offset.min(max);
        let actual_delta = (self.scroll_offset - before) as u16;
        // Shift copy mode anchor down to compensate for scroll
        if actual_delta > 0 {
            if let Some(ref mut cm) = self.copy_mode {
                if let Some(ref mut anchor) = cm.anchor {
                    anchor.1 = anchor.1.saturating_add(actual_delta);
                }
            }
        }
    }

    /// Scroll down by `n` lines (toward live view).
    /// If this reaches offset 0, flushes pending output to return to live mode.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if self.scroll_offset == 0 && !self.pending_output.is_empty() {
            let data = std::mem::take(&mut self.pending_output);
            self.parser.process(&data);
        }
    }

    /// Snap back to live view — flush any pending output through the parser.
    pub fn snap_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.copy_mode = None;
        self.clear_selection();
        if !self.pending_output.is_empty() {
            let data = std::mem::take(&mut self.pending_output);
            self.parser.process(&data);
        }
    }

    /// Enter copy mode. Cursor starts at bottom-center of visible area.
    pub fn enter_copy_mode(&mut self, visible_rows: u16, visible_cols: u16) {
        if self.copy_mode.is_some() {
            return;
        }
        self.copy_mode = Some(CopyMode {
            cursor_col: 0,
            cursor_row: visible_rows.saturating_sub(1),
            anchor: None,
            line_select: false,
            pending_g: false,
        });
        // Freeze at current position if not already scrolled
        if self.scroll_offset == 0 {
            self.scroll_offset = 1;
            // Immediately clamp to actual scrollback
            let max = self.scrollback_len();
            self.scroll_offset = self.scroll_offset.min(max).max(1);
        }
        let _ = visible_cols; // used for future word nav
    }

    /// Exit copy mode — flush pending output and return to live view.
    pub fn exit_copy_mode(&mut self) {
        self.copy_mode = None;
        self.snap_to_bottom();
    }

    /// Move the copy mode cursor, scrolling when hitting screen edges.
    pub fn copy_move(&mut self, dx: i32, dy: i32, visible_rows: u16, visible_cols: u16) {
        if self.copy_mode.is_none() {
            return;
        }

        // Horizontal movement
        {
            let cm = self.copy_mode.as_mut().unwrap();
            let new_col = cm.cursor_col as i32 + dx;
            cm.cursor_col = new_col.clamp(0, visible_cols.saturating_sub(1) as i32) as u16;
        }

        // Vertical movement — may need to scroll, which borrows self
        let new_row = self.copy_mode.as_ref().unwrap().cursor_row as i32 + dy;
        if new_row < 0 {
            let scroll_amount = (-new_row) as usize;
            self.scroll_up(scroll_amount);
            self.copy_mode.as_mut().unwrap().cursor_row = 0;
        } else if new_row >= visible_rows as i32 {
            let scroll_amount = (new_row - visible_rows as i32 + 1) as usize;
            self.scroll_down_no_flush(scroll_amount);
            self.copy_mode.as_mut().unwrap().cursor_row = visible_rows.saturating_sub(1);
        } else {
            self.copy_mode.as_mut().unwrap().cursor_row = new_row as u16;
        }

        self.update_copy_selection();
    }

    /// Toggle character-wise selection anchor at current cursor position.
    pub fn copy_toggle_selection(&mut self, visible_cols: u16) {
        let cm = match self.copy_mode.as_mut() {
            Some(cm) => cm,
            None => return,
        };

        if cm.anchor.is_some() && !cm.line_select {
            // Already in char select — toggle off
            cm.anchor = None;
            cm.line_select = false;
            self.selection = None;
        } else {
            // Enter char select (or switch from line select)
            cm.line_select = false;
            cm.anchor = Some((cm.cursor_col, cm.cursor_row));
            self.selection = Some(Selection {
                start: (cm.cursor_col, cm.cursor_row),
                end: (cm.cursor_col, cm.cursor_row),
                active: true,
            });
        }
        let _ = visible_cols;
    }

    /// Toggle line-wise selection (V mode). Selects full lines.
    pub fn copy_toggle_line_selection(&mut self, visible_cols: u16) {
        let cm = match self.copy_mode.as_mut() {
            Some(cm) => cm,
            None => return,
        };

        if cm.anchor.is_some() && cm.line_select {
            // Already in line select — toggle off
            cm.anchor = None;
            cm.line_select = false;
            self.selection = None;
        } else {
            // Enter line select (or switch from char select)
            cm.line_select = true;
            cm.anchor = Some((0, cm.cursor_row));
            self.selection = Some(Selection {
                start: (0, cm.cursor_row),
                end: (visible_cols.saturating_sub(1), cm.cursor_row),
                active: true,
            });
        }
    }

    /// Yank the current selection to clipboard. Returns true if text was copied.
    pub fn copy_yank(&mut self, visible_cols: u16) -> bool {
        let sel = match self.selection.clone() {
            Some(s) => s,
            None => return false,
        };
        let screen = self.scrolled_screen();
        let text = extract_selected_text(screen, &sel, visible_cols);
        if !text.is_empty() {
            copy_to_clipboard(&text);
            return true;
        }
        false
    }

    /// Jump cursor to start/end of line.
    pub fn copy_line_jump(&mut self, to_end: bool, visible_cols: u16) {
        if let Some(cm) = self.copy_mode.as_mut() {
            cm.cursor_col = if to_end {
                visible_cols.saturating_sub(1)
            } else {
                0
            };
            self.update_copy_selection();
        }
    }

    /// Page up/down in copy mode (half screen).
    pub fn copy_page(&mut self, up: bool, visible_rows: u16) {
        let half = (visible_rows / 2) as usize;
        if up {
            self.scroll_up(half);
        } else {
            self.scroll_down_no_flush(half);
        }
        self.update_copy_selection();
    }

    /// Jump to top or bottom of scrollback in copy mode.
    pub fn copy_jump_edge(&mut self, to_top: bool, visible_rows: u16) {
        if to_top {
            let max = self.scrollback_len();
            self.scroll_offset = max;
            if let Some(cm) = self.copy_mode.as_mut() {
                cm.cursor_row = 0;
            }
        } else {
            self.scroll_offset = 1;
            let max = self.scrollback_len();
            self.scroll_offset = self.scroll_offset.min(max).max(1);
            if let Some(cm) = self.copy_mode.as_mut() {
                cm.cursor_row = visible_rows.saturating_sub(1);
            }
        }
        self.update_copy_selection();
    }

    /// Scroll down without flushing pending output (stay in frozen/copy mode).
    fn scroll_down_no_flush(&mut self, n: usize) {
        let before = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        // In copy mode, keep at least offset 1 to stay frozen
        if self.copy_mode.is_some() && self.scroll_offset == 0 {
            self.scroll_offset = 1;
        }
        let actual_delta = (before - self.scroll_offset) as u16;
        // Shift copy mode anchor up to compensate for scroll
        if actual_delta > 0 {
            if let Some(ref mut cm) = self.copy_mode {
                if let Some(ref mut anchor) = cm.anchor {
                    anchor.1 = anchor.1.saturating_sub(actual_delta);
                }
            }
        }
    }

    /// Sync the selection to match copy mode cursor + anchor positions.
    fn update_copy_selection(&mut self) {
        let cm = match self.copy_mode.as_ref() {
            Some(cm) => cm,
            None => return,
        };
        if let Some(anchor) = cm.anchor {
            if cm.line_select {
                // Line-wise: always span full width, anchor col is 0
                let (start_row, end_row) = if anchor.1 <= cm.cursor_row {
                    (anchor.1, cm.cursor_row)
                } else {
                    (cm.cursor_row, anchor.1)
                };
                self.selection = Some(Selection {
                    start: (0, start_row),
                    end: (u16::MAX, end_row), // u16::MAX = full width, clamped at render
                    active: true,
                });
            } else {
                self.selection = Some(Selection {
                    start: anchor,
                    end: (cm.cursor_col, cm.cursor_row),
                    active: true,
                });
            }
        }
    }
}

/// Encode a crossterm KeyEvent to raw bytes suitable for PTY input.
///
/// Returns None for keys that have no PTY representation.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char(c) if c.is_ascii_lowercase() => {
                return Some(vec![c as u8 - b'a' + 1]);
            }
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                return Some(vec![c.to_ascii_lowercase() as u8 - b'a' + 1]);
            }
            KeyCode::Char('\\') => return Some(vec![0x1c]),
            KeyCode::Char(']') => return Some(vec![0x1d]),
            _ => {}
        }
    }

    if key.modifiers.contains(KeyModifiers::SUPER) && key.code == KeyCode::Backspace {
        return Some(vec![0x15]);
    }

    if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Enter {
        return Some(vec![0x0a]);
    }

    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            KeyCode::Backspace => return Some(vec![0x1b, 0x7f]),
            KeyCode::Char(c) => {
                let mut bytes = vec![0x1b];
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                bytes.extend_from_slice(s.as_bytes());
                return Some(bytes);
            }
            KeyCode::Left => return Some(b"\x1b[1;3D".to_vec()),
            KeyCode::Right => return Some(b"\x1b[1;3C".to_vec()),
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            Some(s.as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![0x0d]),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![0x09]),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::F(1) => Some(b"\x1bOP".to_vec()),
        KeyCode::F(2) => Some(b"\x1bOQ".to_vec()),
        KeyCode::F(3) => Some(b"\x1bOR".to_vec()),
        KeyCode::F(4) => Some(b"\x1bOS".to_vec()),
        KeyCode::F(5) => Some(b"\x1b[15~".to_vec()),
        KeyCode::F(6) => Some(b"\x1b[17~".to_vec()),
        KeyCode::F(7) => Some(b"\x1b[18~".to_vec()),
        KeyCode::F(8) => Some(b"\x1b[19~".to_vec()),
        KeyCode::F(9) => Some(b"\x1b[20~".to_vec()),
        KeyCode::F(10) => Some(b"\x1b[21~".to_vec()),
        KeyCode::F(11) => Some(b"\x1b[23~".to_vec()),
        KeyCode::F(12) => Some(b"\x1b[24~".to_vec()),
        _ => None,
    }
}

/// Encode a mouse event relative to a pane area using SGR mouse protocol.
///
/// Translates absolute terminal coordinates to pane-relative coordinates
/// and produces the appropriate SGR escape sequence.
pub fn encode_mouse_for_pane(mouse: MouseEvent, pane_area: Rect) -> Option<Vec<u8>> {
    let col = mouse.column.checked_sub(pane_area.x)?;
    let row = mouse.row.checked_sub(pane_area.y)?;

    if col >= pane_area.width || row >= pane_area.height {
        return None;
    }

    // SGR encoding is 1-indexed
    let c = col as u32 + 1;
    let r = row as u32 + 1;

    let seq = match mouse.kind {
        MouseEventKind::ScrollUp => format!("\x1b[<64;{c};{r}M"),
        MouseEventKind::ScrollDown => format!("\x1b[<65;{c};{r}M"),
        MouseEventKind::Down(MouseButton::Left) => format!("\x1b[<0;{c};{r}M"),
        MouseEventKind::Up(MouseButton::Left) => format!("\x1b[<0;{c};{r}m"),
        MouseEventKind::Down(MouseButton::Right) => format!("\x1b[<2;{c};{r}M"),
        MouseEventKind::Up(MouseButton::Right) => format!("\x1b[<2;{c};{r}m"),
        MouseEventKind::Down(MouseButton::Middle) => format!("\x1b[<1;{c};{r}M"),
        MouseEventKind::Up(MouseButton::Middle) => format!("\x1b[<1;{c};{r}m"),
        MouseEventKind::Moved => format!("\x1b[<35;{c};{r}M"),
        _ => return None,
    };
    Some(seq.into_bytes())
}

/// Parameters for rendering an agent title bar.
pub struct TitleBarParams<'a> {
    pub folder: &'a str,
    pub description: &'a str,
    pub state: &'a str,
    pub duration: &'a str,
    pub color: Color,
    pub focused: bool,
    pub hint: Option<&'a str>,
}

/// Render an agent title bar.
///
/// Layout: ` folder | description ... state duration`
///
/// Background color is determined by agent state (via `color` param),
/// tinted by whether the pane is focused.
pub fn render_title_bar(frame: &mut Frame, area: Rect, params: &TitleBarParams) {
    let TitleBarParams {
        folder,
        description,
        state,
        duration,
        color,
        focused,
        hint,
    } = params;
    let bg = if *focused { *color } else { dim_color(*color) };
    let fg = if *focused {
        Color::Black
    } else {
        Color::Rgb(80, 80, 80)
    };
    let style = Style::default().bg(bg).fg(fg);

    let left = format!(" {} | {}", folder, description);
    let right = match hint {
        Some(h) => format!(" {} {}  {} ", state, duration, h),
        None => format!(" {} {} ", state, duration),
    };
    let padding_len = (area.width as usize).saturating_sub(left.len() + right.len());
    let padding = " ".repeat(padding_len);

    let line = Line::from(vec![Span::styled(
        format!("{}{}{}", left, padding, right),
        style,
    )]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Extract the text covered by a selection from the vt100 screen.
///
/// Normalizes start/end so the earlier position comes first,
/// reads cells row-by-row, trims trailing whitespace per line,
/// and strips empty trailing lines.
pub fn extract_selected_text(screen: &vt100::Screen, sel: &Selection, cols: u16) -> String {
    let (start, end) =
        if sel.start.1 < sel.end.1 || (sel.start.1 == sel.end.1 && sel.start.0 <= sel.end.0) {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };

    let (start_col, start_row) = start;
    let (end_col, end_row) = end;

    let mut lines: Vec<String> = Vec::new();

    for row in start_row..=end_row {
        let from = if row == start_row { start_col } else { 0 };
        let to = if row == end_row { end_col } else { cols - 1 };

        let mut line = String::new();
        for col in from..=to {
            if let Some(cell) = screen.cell(row, col) {
                let contents = cell.contents();
                if contents.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(contents);
                }
            } else {
                line.push(' ');
            }
        }

        lines.push(line.trim_end().to_string());
    }

    // Strip empty trailing lines
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Copy text to the macOS clipboard via pbcopy.
pub fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn shift_enter_encodes_as_lf() {
        let event = key(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(encode_key(event), Some(vec![0x0a]));
    }

    #[test]
    fn plain_enter_encodes_as_cr() {
        let event = key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(encode_key(event), Some(vec![0x0d]));
    }
}

/// Dim a color for unfocused pane title bars.
fn dim_color(color: Color) -> Color {
    match color {
        Color::Cyan => Color::Rgb(0, 80, 80),
        Color::Magenta => Color::Rgb(80, 0, 80),
        Color::Yellow => Color::Rgb(80, 80, 0),
        Color::Blue => Color::Rgb(0, 0, 100),
        Color::Green => Color::Rgb(0, 80, 0),
        Color::Red => Color::Rgb(100, 0, 0),
        Color::LightCyan => Color::Rgb(0, 100, 100),
        Color::LightMagenta => Color::Rgb(100, 0, 100),
        Color::Rgb(r, g, b) => Color::Rgb(r / 2, g / 2, b / 2),
        other => other,
    }
}
