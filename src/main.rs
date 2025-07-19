use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame, Terminal,
};
use ropey::Rope;
use std::{
    env,
    error::Error,
    fs,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy)]
struct VisualLine {
    start_byte: usize,
    end_byte: usize,
    is_continuation: bool,
    indent: usize,
    logical_line: usize, // Track which logical line this belongs to
}

#[derive(Clone, Debug)]
enum EditOp {
    Insert { pos: usize, text: String },
    Delete { pos: usize, text: String },
}

struct UndoGroup {
    ops: Vec<(EditOp, usize, usize)>, // (operation, caret_before, caret_after)
    timestamp: Instant,
}

struct Editor {
    rope: Rope,
    caret: usize,
    preferred_col: usize,
    viewport_offset: (usize, usize), // (row, col)
    word_wrap: bool,
    visual_lines: Vec<Option<VisualLine>>, // None for virtual lines
    visual_lines_valid: bool, // Track if visual lines need rebuilding
    logical_line_map: Vec<(usize, usize)>, // Maps logical line index to (start, count) in visual_lines
    scrolloff: usize,
    virtual_lines: usize,
    filename: Option<PathBuf>,
    modified: bool,
    undo_stack: Vec<UndoGroup>,
    redo_stack: Vec<UndoGroup>,
    current_group: Option<UndoGroup>,
    last_edit_time: Option<Instant>,
}

impl Editor {
    fn new() -> Self {
        let mut editor = Self {
            rope: Rope::new(),
            caret: 0,
            preferred_col: 0,
            viewport_offset: (0, 0),
            word_wrap: true,
            visual_lines: Vec::new(),
            visual_lines_valid: false,
            logical_line_map: Vec::new(),
            scrolloff: 3,
            virtual_lines: 2,
            filename: None,
            modified: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            current_group: None,
            last_edit_time: None,
        };
        editor.invalidate_visual_lines();
        editor
    }

    fn load_file(&mut self, path: PathBuf) -> io::Result<()> {
        let content = fs::read_to_string(&path)?;
        self.rope = Rope::from_str(&content);
        self.filename = Some(path);
        self.caret = 0;
        self.preferred_col = 0;
        self.modified = false;
        self.invalidate_visual_lines();
        self.logical_line_map.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
        Ok(())
    }

    fn push_op(&mut self, op: EditOp, caret_before: usize, caret_after: usize) {
        let now = Instant::now();
        let new_group = self.last_edit_time
            .map_or(true, |t| now.duration_since(t) > Duration::from_secs(1));

        if new_group {
            if let Some(group) = self.current_group.take() {
                self.undo_stack.push(group);
            }
            self.current_group = Some(UndoGroup {
                ops: vec![(op, caret_before, caret_after)],
                timestamp: now,
            });
        } else if let Some(ref mut group) = self.current_group {
            group.ops.push((op, caret_before, caret_after));
        }

        self.redo_stack.clear();
        self.last_edit_time = Some(now);
        self.modified = true;
    }

    fn finalize_undo_group(&mut self) {
        if let Some(group) = self.current_group.take() {
            if !group.ops.is_empty() {
                self.undo_stack.push(group);
            }
        }
    }

    fn undo(&mut self) {
        self.finalize_undo_group();
        
        if let Some(group) = self.undo_stack.pop() {
            let mut caret = self.caret;
            
            for (op, before, _) in group.ops.iter().rev() {
                match op {
                    EditOp::Insert { pos, text } => {
                        let char_pos = self.rope.byte_to_char(*pos);
                        self.rope.remove(char_pos..self.rope.byte_to_char(pos + text.len()));
                    }
                    EditOp::Delete { pos, text } => {
                        self.rope.insert(self.rope.byte_to_char(*pos), text);
                    }
                }
                caret = *before;
            }
            
            self.caret = caret;
            self.invalidate_visual_lines();
            self.logical_line_map.clear(); // Force rebuild of the map
            self.redo_stack.push(group);
            self.modified = !self.undo_stack.is_empty();
        }
    }

    fn redo(&mut self) {
        if let Some(group) = self.redo_stack.pop() {
            let mut caret = self.caret;
            
            for (op, _, after) in &group.ops {
                match op {
                    EditOp::Insert { pos, text } => {
                        self.rope.insert(self.rope.byte_to_char(*pos), text);
                    }
                    EditOp::Delete { pos, text } => {
                        let char_pos = self.rope.byte_to_char(*pos);
                        self.rope.remove(char_pos..self.rope.byte_to_char(pos + text.len()));
                    }
                }
                caret = *after;
            }
            
            self.caret = caret;
            self.invalidate_visual_lines();
            self.logical_line_map.clear(); // Force rebuild of the map
            self.undo_stack.push(group);
            self.modified = true;
        }
    }

    fn calculate_indent(line: &str) -> usize {
        let trimmed = line.trim_start();
        let base_indent = line.len() - trimmed.len();
        
        // Check for list markers
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            return base_indent + 4;
        }
        
        // Check for numbered lists
        let mut chars = trimmed.chars();
        let mut num_count = 0;
        while let Some(ch) = chars.next() {
            if ch.is_alphanumeric() {
                num_count += 1;
            } else if num_count > 0 && (ch == '.' || ch == ')') {
                if chars.next() == Some(' ') {
                    return base_indent + 4;
                }
                break;
            } else {
                break;
            }
        }
        
        base_indent
    }

    fn rebuild_visual_lines(&mut self, viewport_width: usize) {
        self.visual_lines.clear();
        self.logical_line_map.clear();
        
        // Add virtual lines at the top
        for _ in 0..self.virtual_lines {
            self.visual_lines.push(None);
        }
        
        let mut byte_pos = 0;
        
        for line_idx in 0..self.rope.len_lines() {
            let line_start_idx = self.visual_lines.len();
            let line = self.rope.line(line_idx);
            let line_str = line.to_string();
            let line_bytes = line.len_bytes();
            
            if !self.word_wrap {
                // Without word wrap, each line is a visual line
                let has_newline = line_str.ends_with('\n');
                let end = byte_pos + line_bytes.saturating_sub(if has_newline { 1 } else { 0 });
                
                self.visual_lines.push(Some(VisualLine {
                    start_byte: byte_pos,
                    end_byte: end,
                    is_continuation: false,
                    indent: 0,
                    logical_line: line_idx,
                }));
            } else {
                // With word wrap
                let has_newline = line_str.ends_with('\n');
                let content = if has_newline { &line_str[..line_str.len() - 1] } else { &line_str };
                
                if content.is_empty() {
                    self.visual_lines.push(Some(VisualLine {
                        start_byte: byte_pos,
                        end_byte: byte_pos,
                        is_continuation: false,
                        indent: 0,
                        logical_line: line_idx,
                    }));
                } else {
                    let indent = Self::calculate_indent(&line_str);
                    let segments = self.wrap_line(content, viewport_width, indent);
                    
                    for (i, (start, end)) in segments.into_iter().enumerate() {
                        self.visual_lines.push(Some(VisualLine {
                            start_byte: byte_pos + start,
                            end_byte: byte_pos + end,
                            is_continuation: i > 0,
                            indent: if i > 0 { indent } else { 0 },
                            logical_line: line_idx,
                        }));
                    }
                }
            }
            
            let line_visual_count = self.visual_lines.len() - line_start_idx;
            self.logical_line_map.push((line_start_idx, line_visual_count));
            
            byte_pos += line_bytes;
        }
        
        // Add virtual lines at the bottom
        for _ in 0..self.virtual_lines {
            self.visual_lines.push(None);
        }
        
        self.visual_lines_valid = true;
    }

    fn invalidate_visual_lines(&mut self) {
        self.visual_lines_valid = false;
    }

    fn ensure_visual_lines(&mut self, viewport_width: usize) {
        if !self.visual_lines_valid || self.visual_lines.is_empty() {
            self.rebuild_visual_lines(viewport_width);
        }
    }

    fn wrap_line(&self, content: &str, viewport_width: usize, continuation_indent: usize) -> Vec<(usize, usize)> {
        let mut segments = Vec::new();
        let mut start = 0;
        let mut is_first = true;
        
        while start < content.len() {
            let available_width = if is_first { 
                viewport_width 
            } else { 
                viewport_width.saturating_sub(continuation_indent) 
            };
            
            if available_width == 0 {
                break;
            }
            
            let mut width = 0;
            let mut end = start;
            let mut last_break = start;
            
            for (i, ch) in content[start..].chars().enumerate() {
                let ch_width = ch.to_string().width();
                if width + ch_width > available_width && i > 0 {
                    end = if last_break > start { last_break } else { start + i };
                    break;
                }
                
                width += ch_width;
                if ch == ' ' || ch == '-' || ch == '/' {
                    last_break = start + i + ch.len_utf8();
                }
                end = start + i + ch.len_utf8();
            }
            
            segments.push((start, end));
            start = end;
            is_first = false;
            
            // Skip leading spaces on continuation lines
            while start < content.len() && content.as_bytes()[start] == b' ' {
                start += 1;
            }
        }
        
        if segments.is_empty() {
            segments.push((0, content.len()));
        }
        
        segments
    }

    fn get_visual_position(&mut self, byte_pos: usize, viewport_width: usize) -> (usize, usize) {
        self.ensure_visual_lines(viewport_width);
        
        for (row, vline) in self.visual_lines.iter().enumerate() {
            if let Some(vl) = vline {
                // Check if we're at the exact end of this visual line
                if byte_pos == vl.end_byte && row + 1 < self.visual_lines.len() {
                    // Check if next line is a continuation
                    if let Some(Some(next_vl)) = self.visual_lines.get(row + 1) {
                        if next_vl.is_continuation && next_vl.start_byte == vl.end_byte {
                            // Position cursor at start of continuation line
                            return (row + 1, next_vl.indent);
                        }
                    }
                }
                
                // Normal case: cursor is within this visual line
                if byte_pos >= vl.start_byte && byte_pos <= vl.end_byte {
                    let text = &self.rope.byte_slice(vl.start_byte..byte_pos).to_string();
                    let col = vl.indent + text.width();
                    return (row, col);
                }
            }
        }
        
        // Default to end
        if let Some((row, _)) = self.visual_lines.iter().enumerate().rev().find(|(_, vl)| vl.is_some()) {
            (row, 0)
        } else {
            (self.virtual_lines, 0)
        }
    }

    fn visual_to_byte(&mut self, row: usize, col: usize, viewport_width: usize) -> usize {
        self.ensure_visual_lines(viewport_width);
        
        if let Some(Some(vline)) = self.visual_lines.get(row) {
            // For continuation lines, if col is less than indent, position at line start
            if vline.is_continuation && col < vline.indent {
                return vline.start_byte;
            }
            
            let adjusted_col = col.saturating_sub(vline.indent);
            let slice = self.rope.byte_slice(vline.start_byte..vline.end_byte);
            
            let mut width = 0;
            let mut byte_offset = 0;
            
            for ch in slice.chars() {
                if width >= adjusted_col {
                    break;
                }
                width += ch.to_string().width();
                byte_offset += ch.len_utf8();
            }
            
            vline.start_byte + byte_offset
        } else {
            self.rope.len_bytes()
        }
    }

    fn move_up(&mut self, viewport_width: usize) {
        let (row, _) = self.get_visual_position(self.caret, viewport_width);
        if row > self.virtual_lines {
            self.caret = self.visual_to_byte(row - 1, self.preferred_col, viewport_width);
        } else if row == self.virtual_lines && self.rope.len_bytes() > 0 {
            // If we're at the first content line, stay there
            self.caret = 0;
        }
    }

    fn move_down(&mut self, viewport_width: usize) {
        let (row, _) = self.get_visual_position(self.caret, viewport_width);
        let total_visual_lines = self.visual_lines.len();
        let last_content_row = total_visual_lines - self.virtual_lines - 1;
        
        // If we're in virtual lines at the top, move to first content line
        if row < self.virtual_lines && self.rope.len_bytes() > 0 {
            self.caret = 0;
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        } else if row < last_content_row {
            // Normal case: move to next visual line
            self.caret = self.visual_to_byte(row + 1, self.preferred_col, viewport_width);
        }
    }

    fn move_left(&mut self, viewport_width: usize) {
        if self.caret > 0 {
            let char_idx = self.rope.byte_to_char(self.caret);
            if char_idx > 0 {
                self.caret = self.rope.char_to_byte(char_idx - 1);
                let (_, col) = self.get_visual_position(self.caret, viewport_width);
                self.preferred_col = col;
            }
        }
    }

    fn move_right(&mut self, viewport_width: usize) {
        if self.caret < self.rope.len_bytes() {
            let char_idx = self.rope.byte_to_char(self.caret);
            if char_idx < self.rope.len_chars() {
                self.caret = self.rope.char_to_byte(char_idx + 1);
                let (_, col) = self.get_visual_position(self.caret, viewport_width);
                self.preferred_col = col;
            }
        }
    }

    fn insert_char(&mut self, ch: char, viewport_width: usize) {
        let before = self.caret;
        self.rope.insert_char(self.rope.byte_to_char(self.caret), ch);
        self.caret += ch.len_utf8();
        
        self.push_op(EditOp::Insert { pos: before, text: ch.to_string() }, before, self.caret);
        
        // For now, invalidate all visual lines to ensure correctness
        self.invalidate_visual_lines();
        
        let (_, col) = self.get_visual_position(self.caret, viewport_width);
        self.preferred_col = col;
    }

    fn delete(&mut self, viewport_width: usize) {
        if self.caret < self.rope.len_bytes() {
            let char_idx = self.rope.byte_to_char(self.caret);
            
            if let Some(ch) = self.rope.get_char(char_idx) {
                let before = self.caret;
                self.rope.remove(char_idx..char_idx + 1);
                
                self.push_op(EditOp::Delete { pos: self.caret, text: ch.to_string() }, before, self.caret);
                
                // For now, invalidate all visual lines to ensure correctness
                self.invalidate_visual_lines();
            }
        }
    }

    fn backspace(&mut self, viewport_width: usize) {
        if self.caret > 0 {
            let char_idx = self.rope.byte_to_char(self.caret);
            if char_idx > 0 {
                let ch = self.rope.char(char_idx - 1);
                let ch_bytes = ch.len_utf8();
                let before = self.caret;
                
                self.rope.remove(char_idx - 1..char_idx);
                self.caret -= ch_bytes;
                
                self.push_op(EditOp::Delete { pos: self.caret, text: ch.to_string() }, before, self.caret);
                
                // For now, invalidate all visual lines to ensure correctness
                self.invalidate_visual_lines();
            }
        }
    }

    fn indent(&mut self, viewport_width: usize) {
        let char_idx = self.rope.byte_to_char(self.caret);
        let line_idx = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_char(line_idx);
        let line_byte = self.rope.char_to_byte(line_start);
        
        let before = self.caret;
        self.rope.insert(line_start, "    ");
        if self.caret >= line_byte {
            self.caret += 4;
        }
        
        self.push_op(EditOp::Insert { pos: line_byte, text: "    ".to_string() }, before, self.caret);
        
        // For now, invalidate all visual lines to ensure correctness
        self.invalidate_visual_lines();
        
        let (_, col) = self.get_visual_position(self.caret, viewport_width);
        self.preferred_col = col;
    }

    fn dedent(&mut self, viewport_width: usize) {
        let char_idx = self.rope.byte_to_char(self.caret);
        let line_idx = self.rope.char_to_line(char_idx);
        let line = self.rope.line(line_idx);
        
        let mut spaces = 0;
        for ch in line.chars().take(4) {
            if ch == ' ' {
                spaces += 1;
            } else {
                break;
            }
        }
        
        if spaces > 0 {
            let line_start = self.rope.line_to_char(line_idx);
            let line_byte = self.rope.char_to_byte(line_start);
            let before = self.caret;
            
            self.rope.remove(line_start..line_start + spaces);
            
            if self.caret >= line_byte + spaces {
                self.caret -= spaces;
            } else if self.caret > line_byte {
                self.caret = line_byte;
            }
            
            self.push_op(EditOp::Delete { pos: line_byte, text: " ".repeat(spaces) }, before, self.caret);
            
            // For now, invalidate all visual lines to ensure correctness
            self.invalidate_visual_lines();
            
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        }
    }

    fn update_viewport(&mut self, height: usize, width: usize) {
        self.ensure_visual_lines(width);
        let (row, col) = self.get_visual_position(self.caret, width);
        
        // Vertical scrolling
        if row < self.viewport_offset.0 + self.scrolloff {
            self.viewport_offset.0 = row.saturating_sub(self.scrolloff);
        } else if row >= self.viewport_offset.0 + height - self.scrolloff {
            self.viewport_offset.0 = row + self.scrolloff + 1 - height;
        }
        
        // Horizontal scrolling (only without word wrap)
        if !self.word_wrap {
            if col < self.viewport_offset.1 + self.scrolloff {
                self.viewport_offset.1 = col.saturating_sub(self.scrolloff);
            } else if col >= self.viewport_offset.1 + width - self.scrolloff {
                self.viewport_offset.1 = col + self.scrolloff + 1 - width;
            }
        } else {
            self.viewport_offset.1 = 0;
        }
    }

    fn handle_click(&mut self, col: u16, row: u16, area: Rect, viewport_width: usize) {
        self.ensure_visual_lines(viewport_width);
        let click_row = self.viewport_offset.0 + row.saturating_sub(area.y) as usize;
        let click_col = self.viewport_offset.1 + col.saturating_sub(area.x) as usize;
        
        if click_row >= self.virtual_lines && 
           click_row < self.visual_lines.len() - self.virtual_lines {
            // Get the visual line to check for continuation constraints
            if let Some(Some(vline)) = self.visual_lines.get(click_row) {
                let actual_col = if vline.is_continuation {
                    // Ensure we don't click before the indent on continuation lines
                    click_col.max(vline.indent)
                } else {
                    click_col
                };
                self.caret = self.visual_to_byte(click_row, actual_col, viewport_width);
                self.preferred_col = actual_col;
            }
        }
    }

    fn get_display_name(&self) -> String {
        let name = self.filename.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[No Name]");
        
        if self.modified {
            format!("{}*", name)
        } else {
            name.to_string()
        }
    }

    fn get_position(&self) -> (usize, usize) {
        let char_idx = self.rope.byte_to_char(self.caret);
        let line = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_char(line);
        let col = char_idx - line_start;
        (line + 1, col + 1)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    
    let result = run_app(&mut terminal);
    
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    
    if let Err(err) = result {
        eprintln!("Error: {:?}", err);
    }
    
    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let mut editor = Editor::new();
    
    // Load file from command line
    if let Some(filename) = env::args().nth(1) {
        let path = PathBuf::from(filename);
        editor.filename = Some(path.clone());
        
        if let Ok(_) = editor.load_file(path) {
            editor.modified = false;
        }
    }
    
    execute!(io::stdout(), SetTitle(&editor.get_display_name()))?;
    
    loop {
        terminal.draw(|f| draw_ui(f, &mut editor))?;
        
        match event::read()? {
            Event::Key(key) => {
                let size = terminal.size()?;
                let viewport_width = size.width as usize;
                let viewport_height = size.height as usize - 1;
                
                match key.code {
                    KeyCode::Char('q') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Char('w') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        editor.word_wrap = !editor.word_wrap;
                        editor.invalidate_visual_lines();
                        editor.logical_line_map.clear(); // Word wrap changes all visual lines
                    }
                    KeyCode::Char('z') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        editor.undo();
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Char('y') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        editor.redo();
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Tab => {
                        if key.modifiers.contains(event::KeyModifiers::SHIFT) {
                            editor.dedent(viewport_width);
                        } else {
                            editor.indent(viewport_width);
                        }
                    }
                    KeyCode::Char(c) => {
                        editor.insert_char(c, viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Enter => {
                        editor.insert_char('\n', viewport_width);
                        editor.preferred_col = 0;
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Backspace => {
                        editor.backspace(viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Delete => {
                        editor.delete(viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Left => {
                        editor.move_left(viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Right => {
                        editor.move_right(viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Up => {
                        editor.move_up(viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    KeyCode::Down => {
                        editor.move_down(viewport_width);
                        editor.update_viewport(viewport_height, viewport_width);
                    }
                    _ => {}
                }
                
                execute!(io::stdout(), SetTitle(&editor.get_display_name()))?;
            }
            Event::Mouse(mouse) => {
                let size = terminal.size()?;
                match mouse.kind {
                    MouseEventKind::Down(_) => {
                        let chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([
                                Constraint::Min(0),
                                Constraint::Length(1),
                            ])
                            .split(size);
                        editor.handle_click(mouse.column, mouse.row, chunks[0], size.width as usize);
                    }
                    MouseEventKind::ScrollUp => {
                        editor.viewport_offset.0 = editor.viewport_offset.0.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        let max = editor.visual_lines.len().saturating_sub(size.height as usize - 1);
                        editor.viewport_offset.0 = (editor.viewport_offset.0 + 3).min(max);
                    }
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                let size = terminal.size()?;
                editor.invalidate_visual_lines();
                editor.logical_line_map.clear(); // Resize can change all line wrapping
                editor.update_viewport(size.height as usize - 1, size.width as usize);
            }
            _ => {}
        }
    }
}

fn draw_ui(f: &mut Frame, editor: &mut Editor) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.size());
    
    let viewport_height = chunks[0].height as usize;
    let viewport_width = chunks[0].width as usize;
    
    // Ensure visual lines are built before updating viewport
    editor.ensure_visual_lines(viewport_width);
    editor.update_viewport(viewport_height, viewport_width);
    
    // Render main text area
    let mut lines = Vec::new();
    let (caret_row, caret_col) = editor.get_visual_position(editor.caret, viewport_width);
    
    let start = editor.viewport_offset.0;
    let end = (start + viewport_height).min(editor.visual_lines.len());
    
    for row in start..end {
        if let Some(vline_opt) = editor.visual_lines.get(row) {
            if let Some(vline) = vline_opt {
                let text = editor.rope.byte_slice(vline.start_byte..vline.end_byte).to_string();
                
                // Apply horizontal scrolling if needed
                let display_text = if editor.word_wrap || editor.viewport_offset.1 == 0 {
                    text
                } else {
                    let mut result = String::new();
                    let mut width = 0;
                    
                    for ch in text.chars() {
                        width += ch.to_string().width();
                        if width > editor.viewport_offset.1 {
                            result.push(ch);
                        }
                    }
                    result
                };
                
                let mut spans = vec![];
                if vline.indent > 0 {
                    spans.push(Span::raw(" ".repeat(vline.indent)));
                }
                spans.push(Span::raw(display_text));
                
                lines.push(Line::from(spans));
            } else {
                // Virtual line
                lines.push(Line::from(vec![Span::styled("~", Style::default().fg(Color::DarkGray))]));
            }
        }
    }
    
    // Pad with empty lines if needed
    while lines.len() < viewport_height {
        lines.push(Line::default());
    }
    
    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, chunks[0]);
    
    // Set cursor position
    if caret_row >= start && caret_row < end {
        let screen_row = caret_row - start;
        let screen_col = if editor.word_wrap {
            caret_col
        } else {
            caret_col.saturating_sub(editor.viewport_offset.1)
        };
        
        if screen_col < viewport_width {
            f.set_cursor(
                chunks[0].x + screen_col as u16,
                chunks[0].y + screen_row as u16,
            );
        }
    }
    
    // Render status bar
    let (line, col) = editor.get_position();
    let status_text = format!(
        " {} | {} | {}:{} ",
        editor.get_display_name(),
        if editor.word_wrap { "Wrap" } else { "No-Wrap" },
        line,
        col
    );
    
    let status = Paragraph::new(Line::from(vec![Span::raw(status_text)]))
        .style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .alignment(Alignment::Left);
    
    f.render_widget(status, chunks[1]);
}