use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame, Terminal,
};
use ropey::{Rope, RopeSlice};
use std::{
    env,
    error::Error,
    fs,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug)]
struct VisualLine {
    start_byte: usize,
    end_byte: usize,
    is_continuation: bool,
    virtual_indent: usize,
    is_virtual: bool,
}

#[derive(Clone)]
struct TextBuffer {
    rope: Rope,
}

impl TextBuffer {
    fn new() -> Self {
        Self {
            rope: Rope::new(),
        }
    }

    fn from_string(s: String) -> Self {
        Self {
            rope: Rope::from_str(&s),
        }
    }

    fn insert_char(&mut self, byte_pos: usize, ch: char) {
        let char_idx = self.rope.byte_to_char(byte_pos);
        self.rope.insert_char(char_idx, ch);
    }

    fn insert_str(&mut self, byte_pos: usize, text: &str) {
        let char_idx = self.rope.byte_to_char(byte_pos);
        self.rope.insert(char_idx, text);
    }

    fn delete_range(&mut self, start_byte: usize, end_byte: usize) -> String {
        let start_char = self.rope.byte_to_char(start_byte);
        let end_char = self.rope.byte_to_char(end_byte);
        let deleted_text = self.rope.slice(start_char..end_char).to_string();
        self.rope.remove(start_char..end_char);
        deleted_text
    }

    fn delete_char(&mut self, byte_pos: usize) -> Option<char> {
        if byte_pos < self.rope.len_bytes() {
            let char_idx = self.rope.byte_to_char(byte_pos);
            if char_idx < self.rope.len_chars() {
                let ch = self.rope.char(char_idx);
                let next_char_idx = char_idx + 1;
                self.rope.remove(char_idx..next_char_idx);
                return Some(ch);
            }
        }
        None
    }

    fn backspace(&mut self, byte_pos: usize) -> Option<(usize, char)> {
        if byte_pos > 0 {
            let char_idx = self.rope.byte_to_char(byte_pos);
            if char_idx > 0 {
                let prev_char_idx = char_idx - 1;
                let ch = self.rope.char(prev_char_idx);
                let prev_byte = self.rope.char_to_byte(prev_char_idx);
                self.rope.remove(prev_char_idx..char_idx);
                return Some((byte_pos - prev_byte, ch));
            }
        }
        None
    }

    fn get_line(&self, index: usize) -> Option<RopeSlice> {
        if index < self.rope.len_lines() {
            Some(self.rope.line(index))
        } else {
            None
        }
    }

    fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    fn byte_to_line_col(&self, byte_pos: usize) -> (usize, usize, usize) {
        let char_idx = self.rope.byte_to_char(byte_pos.min(self.rope.len_bytes()));
        let line_idx = self.rope.char_to_line(char_idx);
        let line_start_char = self.rope.line_to_char(line_idx);
        let line_char_offset = char_idx - line_start_char;
        
        let line = self.rope.line(line_idx);
        let line_byte_offset = if line_char_offset == 0 {
            0
        } else {
            let mut byte_offset = 0;
            for (i, ch) in line.chars().enumerate() {
                if i >= line_char_offset {
                    break;
                }
                byte_offset += ch.len_utf8();
            }
            byte_offset
        };
        
        let col = line.slice(..line.len_chars().min(line_char_offset))
            .as_str()
            .map(|s| s.width())
            .unwrap_or(0);
        
        (line_idx, col, line_byte_offset)
    }

    fn line_col_to_byte(&self, line: usize, target_col: usize) -> usize {
        if line >= self.rope.len_lines() {
            return self.rope.len_bytes();
        }
        
        let line_start_char = self.rope.line_to_char(line);
        let line_slice = self.rope.line(line);
        
        let mut current_col = 0;
        let mut char_offset = 0;
        
        for ch in line_slice.chars() {
            let ch_width = ch.to_string().width();
            if current_col >= target_col {
                break;
            }
            current_col += ch_width;
            char_offset += 1;
        }
        
        self.rope.char_to_byte(line_start_char + char_offset)
    }

    fn to_string(&self) -> String {
        self.rope.to_string()
    }
}

#[derive(Clone, Debug)]
enum EditOperation {
    Insert {
        position: usize,
        text: String,
        caret_before: usize,
        caret_after: usize,
    },
    Delete {
        position: usize,
        text: String,
        caret_before: usize,
        caret_after: usize,
    },
}

impl EditOperation {
    fn undo(&self, buffer: &mut TextBuffer) -> usize {
        match self {
            EditOperation::Insert { position, text, caret_before, .. } => {
                buffer.delete_range(*position, position + text.len());
                *caret_before
            }
            EditOperation::Delete { position, text, caret_before, .. } => {
                buffer.insert_str(*position, text);
                *caret_before
            }
        }
    }

    fn redo(&self, buffer: &mut TextBuffer) -> usize {
        match self {
            EditOperation::Insert { position, text, caret_after, .. } => {
                buffer.insert_str(*position, text);
                *caret_after
            }
            EditOperation::Delete { position, text, caret_after, .. } => {
                buffer.delete_range(*position, position + text.len());
                *caret_after
            }
        }
    }
}

struct UndoGroup {
    operations: Vec<EditOperation>,
    timestamp: Instant,
}

struct Editor {
    buffer: TextBuffer,
    caret_byte: usize,
    preferred_col: usize,
    viewport_offset_row: usize,
    viewport_offset_col: usize,
    word_wrap: bool,
    visual_lines: Vec<VisualLine>,
    scrolloff: usize,
    virtual_lines_count: usize,
    filename: Option<PathBuf>,
    modified: bool,
    
    // Undo/redo state
    undo_stack: Vec<UndoGroup>,
    redo_stack: Vec<UndoGroup>,
    current_undo_group: Option<UndoGroup>,
    last_edit_time: Option<Instant>,
    undo_group_timeout: Duration,
}

impl Editor {
    fn new() -> Self {
        let buffer = TextBuffer::new();
        let mut editor = Self {
            buffer,
            caret_byte: 0,
            preferred_col: 0,
            viewport_offset_row: 0,
            viewport_offset_col: 0,
            word_wrap: true,
            visual_lines: Vec::new(),
            scrolloff: 3,
            virtual_lines_count: 2,
            filename: None,
            modified: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            current_undo_group: None,
            last_edit_time: None,
            undo_group_timeout: Duration::from_secs(1),
        };
        editor.rebuild_visual_lines(80);
        editor
    }

    fn set_content(&mut self, content: String, viewport_width: usize) {
        self.buffer = TextBuffer::from_string(content);
        self.caret_byte = 0;
        self.preferred_col = 0;
        self.rebuild_visual_lines(viewport_width);
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.current_undo_group = None;
        self.last_edit_time = None;
    }

    fn push_edit_operation(&mut self, operation: EditOperation) {
        let now = Instant::now();
        
        // Check if we should create a new undo group
        let should_create_new_group = match self.last_edit_time {
            Some(last_time) => now.duration_since(last_time) > self.undo_group_timeout,
            None => true,
        };
        
        if should_create_new_group {
            // Push the current group to the undo stack if it exists
            if let Some(group) = self.current_undo_group.take() {
                if !group.operations.is_empty() {
                    self.undo_stack.push(group);
                }
            }
            
            // Create a new group
            self.current_undo_group = Some(UndoGroup {
                operations: vec![operation],
                timestamp: now,
            });
        } else {
            // Add to the current group
            if let Some(ref mut group) = self.current_undo_group {
                group.operations.push(operation);
            } else {
                self.current_undo_group = Some(UndoGroup {
                    operations: vec![operation],
                    timestamp: now,
                });
            }
        }
        
        // Clear redo stack when new edit is made
        self.redo_stack.clear();
        self.last_edit_time = Some(now);
    }

    fn finalize_current_undo_group(&mut self) {
        if let Some(group) = self.current_undo_group.take() {
            if !group.operations.is_empty() {
                self.undo_stack.push(group);
            }
        }
    }

    fn undo(&mut self, viewport_width: usize) {
        // First, finalize any pending operations
        self.finalize_current_undo_group();
        
        if let Some(mut group) = self.undo_stack.pop() {
            let mut caret = self.caret_byte;
            
            // Apply all operations in reverse order
            for operation in group.operations.iter().rev() {
                caret = operation.undo(&mut self.buffer);
            }
            
            self.caret_byte = caret;
            self.rebuild_visual_lines(viewport_width);
            
            // Update preferred column
            let (_, col) = self.get_caret_visual_position();
            self.preferred_col = col;
            
            // Move the group to redo stack
            self.redo_stack.push(group);
            
            self.modified = !self.undo_stack.is_empty() || self.current_undo_group.is_some();
        }
    }

    fn redo(&mut self, viewport_width: usize) {
        if let Some(mut group) = self.redo_stack.pop() {
            let mut caret = self.caret_byte;
            
            // Apply all operations in forward order
            for operation in group.operations.iter() {
                caret = operation.redo(&mut self.buffer);
            }
            
            self.caret_byte = caret;
            self.rebuild_visual_lines(viewport_width);
            
            // Update preferred column
            let (_, col) = self.get_caret_visual_position();
            self.preferred_col = col;
            
            // Move the group to undo stack
            self.undo_stack.push(group);
            
            self.modified = true;
        }
    }

    fn calculate_indentation(line: &str) -> usize {
        let mut indent = 0;
        
        for ch in line.chars() {
            if ch == ' ' {
                indent += 1;
            } else if ch == '\t' {
                indent += 4;
            } else {
                break;
            }
        }
        
        let trimmed = line.trim_start();
        
        // Check for bullet patterns
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            return indent + 4;
        }
        
        // Check for numbered lists (handles multi-digit numbers)
        let chars: Vec<char> = trimmed.chars().collect();
        if !chars.is_empty() {
            let mut i = 0;
            
            // Skip all numeric or alphabetic characters
            while i < chars.len() && (chars[i].is_numeric() || chars[i].is_alphabetic()) {
                i += 1;
            }
            
            // Check if followed by delimiter and space
            if i > 0 && i + 1 < chars.len() {
                if (chars[i] == '.' || chars[i] == ')') && chars[i + 1] == ' ' {
                    return indent + 4;
                }
            }
        }
        
        indent
    }

    fn rebuild_visual_lines(&mut self, viewport_width: usize) {
        self.visual_lines.clear();
        
        // Add virtual lines at the top
        for _ in 0..self.virtual_lines_count {
            self.visual_lines.push(VisualLine {
                start_byte: 0,
                end_byte: 0,
                is_continuation: false,
                virtual_indent: 0,
                is_virtual: true,
            });
        }
        
        if !self.word_wrap {
            // Without word wrap, each logical line is a visual line
            for line_idx in 0..self.buffer.len_lines() {
                let line_start_char = self.buffer.rope.line_to_char(line_idx);
                let line_end_char = if line_idx + 1 < self.buffer.len_lines() {
                    self.buffer.rope.line_to_char(line_idx + 1)
                } else {
                    self.buffer.rope.len_chars()
                };
                
                let line_start_byte = self.buffer.rope.char_to_byte(line_start_char);
                let line_end_byte = self.buffer.rope.char_to_byte(line_end_char);
                
                // Check if line has a trailing newline
                let line_slice = self.buffer.rope.slice(line_start_char..line_end_char);
                let has_newline = line_slice.len_chars() > 0 && 
                    line_slice.char(line_slice.len_chars() - 1) == '\n';
                
                let visual_end_byte = if has_newline {
                    line_end_byte - 1
                } else {
                    line_end_byte
                };
                
                self.visual_lines.push(VisualLine {
                    start_byte: line_start_byte,
                    end_byte: visual_end_byte,
                    is_continuation: false,
                    virtual_indent: 0,
                    is_virtual: false,
                });
            }
        } else {
            // With word wrap - work with character indices, convert to bytes only when needed
            for line_idx in 0..self.buffer.len_lines() {
                let line_start_char = self.buffer.rope.line_to_char(line_idx);
                let line_end_char = if line_idx + 1 < self.buffer.len_lines() {
                    self.buffer.rope.line_to_char(line_idx + 1)
                } else {
                    self.buffer.rope.len_chars()
                };
                
                // Get the line as a slice
                let line_slice = self.buffer.rope.slice(line_start_char..line_end_char);
                
                // Handle empty lines
                if line_slice.len_chars() == 0 {
                    let byte_pos = self.buffer.rope.char_to_byte(line_start_char);
                    self.visual_lines.push(VisualLine {
                        start_byte: byte_pos,
                        end_byte: byte_pos,
                        is_continuation: false,
                        virtual_indent: 0,
                        is_virtual: false,
                    });
                    continue;
                }
                
                // Get line as string for indentation calculation
                let line_str = line_slice.to_string();
                let continuation_indent = Self::calculate_indentation(&line_str);
                
                // Determine content length (excluding trailing newline)
                let has_newline = line_slice.len_chars() > 0 && 
                    line_slice.char(line_slice.len_chars() - 1) == '\n';
                let content_len_chars = if has_newline {
                    line_slice.len_chars() - 1
                } else {
                    line_slice.len_chars()
                };
                
                // Handle lines with only a newline
                if content_len_chars == 0 {
                    let byte_pos = self.buffer.rope.char_to_byte(line_start_char);
                    self.visual_lines.push(VisualLine {
                        start_byte: byte_pos,
                        end_byte: byte_pos,
                        is_continuation: false,
                        virtual_indent: 0,
                        is_virtual: false,
                    });
                    continue;
                }
                
                // Process line wrapping
                let mut segment_start_char = 0;  // Relative to line start
                let mut is_first_segment = true;
                
                while segment_start_char < content_len_chars {
                    let effective_width = if is_first_segment {
                        viewport_width
                    } else {
                        viewport_width.saturating_sub(continuation_indent)
                    };
                    
                    if effective_width == 0 {
                        break;
                    }
                    
                    // Find break point by iterating through characters
                    let mut current_width = 0;
                    let mut segment_end_char = segment_start_char;
                    let mut last_break_char = segment_start_char;
                    
                    // Create a slice from the current position to the end of content
                    let search_slice = line_slice.slice(segment_start_char..content_len_chars);
                    
                    for (idx, ch) in search_slice.chars().enumerate() {
                        let ch_width = ch.to_string().width();
                        
                        if current_width + ch_width > effective_width && idx > 0 {
                            // Use last break point if available, otherwise break here
                            segment_end_char = if last_break_char > segment_start_char {
                                last_break_char
                            } else {
                                segment_start_char + idx
                            };
                            break;
                        }
                        
                        current_width += ch_width;
                        
                        // Track potential break points
                        if ch == ' ' || ch == '-' || ch == '/' {
                            last_break_char = segment_start_char + idx + 1;
                        }
                        
                        segment_end_char = segment_start_char + idx + 1;
                    }
                    
                    // Convert character positions to byte positions
                    let start_byte = self.buffer.rope.char_to_byte(line_start_char + segment_start_char);
                    let end_byte = self.buffer.rope.char_to_byte(line_start_char + segment_end_char);
                    
                    self.visual_lines.push(VisualLine {
                        start_byte,
                        end_byte,
                        is_continuation: !is_first_segment,
                        virtual_indent: if is_first_segment { 0 } else { continuation_indent },
                        is_virtual: false,
                    });
                    
                    is_first_segment = false;
                    segment_start_char = segment_end_char;
                    
                    // Skip leading spaces on continuation lines
                    while segment_start_char < content_len_chars {
                        let ch = line_slice.char(segment_start_char);
                        if ch != ' ' {
                            break;
                        }
                        segment_start_char += 1;
                    }
                }
            }
        }
        
        // Add virtual lines at the bottom
        for _ in 0..self.virtual_lines_count {
            self.visual_lines.push(VisualLine {
                start_byte: self.buffer.len_bytes(),
                end_byte: self.buffer.len_bytes(),
                is_continuation: false,
                virtual_indent: 0,
                is_virtual: true,
            });
        }
    }

    fn get_caret_visual_position(&self) -> (usize, usize) {
        // Handle empty buffer case
        if self.buffer.len_bytes() == 0 {
            return (self.virtual_lines_count, 0);
        }
        
        for (visual_row, vline) in self.visual_lines.iter().enumerate() {
            if vline.is_virtual {
                continue;
            }
            
            // Check if caret is within this visual line (including at the end)
            if self.caret_byte >= vline.start_byte && self.caret_byte <= vline.end_byte {
                // For continuation lines, check if we should be on the next line
                if self.caret_byte == vline.end_byte && visual_row + 1 < self.visual_lines.len() {
                    if let Some(next_line) = self.visual_lines.get(visual_row + 1) {
                        if !next_line.is_virtual && next_line.is_continuation {
                            // Caret should appear at the start of the continuation line
                            continue;
                        }
                    }
                }
                
                // Calculate column position using Rope's character API
                let start_char = self.buffer.rope.byte_to_char(vline.start_byte);
                let caret_char = self.buffer.rope.byte_to_char(self.caret_byte);
                let chars_from_start = caret_char - start_char;
                
                // Calculate visual width
                let line_slice = self.buffer.rope.slice(start_char..caret_char);
                let mut col = vline.virtual_indent;
                for ch in line_slice.chars() {
                    col += ch.to_string().width();
                }
                
                return (visual_row, col);
            }
        }
        
        // If caret is at the very end of the buffer
        if let Some((idx, last_line)) = self.visual_lines.iter()
            .enumerate()
            .rev()
            .find(|(_, vl)| !vl.is_virtual) {
            
            let start_char = self.buffer.rope.byte_to_char(last_line.start_byte);
            let end_char = self.buffer.rope.byte_to_char(last_line.end_byte);
            let line_slice = self.buffer.rope.slice(start_char..end_char);
            
            let mut col = last_line.virtual_indent;
            for ch in line_slice.chars() {
                col += ch.to_string().width();
            }
            
            return (idx, col);
        }
        
        // Fallback
        (self.virtual_lines_count, 0)
    }

    fn visual_row_col_to_byte(&self, visual_row: usize, target_col: usize) -> usize {
        if let Some(vline) = self.visual_lines.get(visual_row) {
            if vline.is_virtual {
                return if visual_row < self.virtual_lines_count { 0 } else { self.buffer.len_bytes() };
            }
            
            if target_col < vline.virtual_indent {
                return vline.start_byte;
            }
            
            let adjusted_target = target_col - vline.virtual_indent;
            let start_char = self.buffer.rope.byte_to_char(vline.start_byte);
            let end_char = self.buffer.rope.byte_to_char(vline.end_byte);
            
            // Check if line has a trailing newline we should ignore
            let line_slice = self.buffer.rope.slice(start_char..end_char);
            let content_end_char = if line_slice.len_chars() > 0 && 
                line_slice.char(line_slice.len_chars() - 1) == '\n' {
                end_char - 1
            } else {
                end_char
            };
            
            // Find the character position at the target column
            let mut current_col = 0;
            let mut target_char = start_char;
            
            for ch in self.buffer.rope.slice(start_char..content_end_char).chars() {
                if current_col >= adjusted_target {
                    break;
                }
                let ch_width = ch.to_string().width();
                current_col += ch_width;
                target_char += 1;
            }
            
            self.buffer.rope.char_to_byte(target_char)
        } else {
            self.buffer.len_bytes()
        }
    }

    fn move_caret_up(&mut self) {
        let (visual_row, _) = self.get_caret_visual_position();
        if visual_row > self.virtual_lines_count {
            self.caret_byte = self.visual_row_col_to_byte(visual_row - 1, self.preferred_col);
        }
    }

    fn move_caret_down(&mut self) {
        let (visual_row, _) = self.get_caret_visual_position();
        let last_content_row = self.visual_lines.len() - self.virtual_lines_count - 1;
        if visual_row < last_content_row {
            self.caret_byte = self.visual_row_col_to_byte(visual_row + 1, self.preferred_col);
        }
    }

    fn move_caret_left(&mut self) {
        if self.caret_byte > 0 {
            let char_idx = self.buffer.rope.byte_to_char(self.caret_byte);
            if char_idx > 0 {
                self.caret_byte = self.buffer.rope.char_to_byte(char_idx - 1);
                let (_, col) = self.get_caret_visual_position();
                self.preferred_col = col;
            }
        }
    }

    fn move_caret_right(&mut self) {
        if self.caret_byte < self.buffer.len_bytes() {
            let char_idx = self.buffer.rope.byte_to_char(self.caret_byte);
            if char_idx < self.buffer.rope.len_chars() {
                self.caret_byte = self.buffer.rope.char_to_byte(char_idx + 1);
                let (_, col) = self.get_caret_visual_position();
                self.preferred_col = col;
            }
        }
    }

    fn insert_char(&mut self, ch: char, viewport_width: usize) {
        let caret_before = self.caret_byte;
        self.buffer.insert_char(self.caret_byte, ch);
        self.caret_byte += ch.len_utf8();
        let caret_after = self.caret_byte;
        
        self.push_edit_operation(EditOperation::Insert {
            position: caret_before,
            text: ch.to_string(),
            caret_before,
            caret_after,
        });
        
        self.rebuild_visual_lines(viewport_width);
        let (_, col) = self.get_caret_visual_position();
        self.preferred_col = col;
        self.modified = true;
    }

    fn delete_char(&mut self, viewport_width: usize) {
        if let Some(ch) = self.buffer.delete_char(self.caret_byte) {
            let caret_before = self.caret_byte;
            let caret_after = self.caret_byte;
            
            self.push_edit_operation(EditOperation::Delete {
                position: self.caret_byte,
                text: ch.to_string(),
                caret_before,
                caret_after,
            });
            
            self.rebuild_visual_lines(viewport_width);
            self.modified = true;
        }
    }

    fn backspace(&mut self, viewport_width: usize) {
        if self.caret_byte > 0 {
            let char_idx = self.buffer.rope.byte_to_char(self.caret_byte);
            if char_idx > 0 {
                let prev_char_idx = char_idx - 1;
                let ch = self.buffer.rope.char(prev_char_idx);
                let ch_text = ch.to_string();
                let delete_pos = self.buffer.rope.char_to_byte(prev_char_idx);
                
                // Store caret positions before the operation
                let caret_before = self.caret_byte;
                
                // Perform the deletion
                if let Some((bytes_removed, _)) = self.buffer.backspace(self.caret_byte) {
                    self.caret_byte -= bytes_removed;
                    let caret_after = self.caret_byte;
                    
                    // Create delete operation with correct position
                    self.push_edit_operation(EditOperation::Delete {
                        position: delete_pos,
                        text: ch_text,
                        caret_before,
                        caret_after,
                    });
                    
                    self.rebuild_visual_lines(viewport_width);
                    self.modified = true;
                }
            }
        }
    }

    fn insert_newline(&mut self, viewport_width: usize) {
        self.insert_char('\n', viewport_width);
        self.preferred_col = 0;
    }

    fn indent_line(&mut self, viewport_width: usize) {
        let (line_idx, _, _) = self.buffer.byte_to_line_col(self.caret_byte);
        let line_start_byte = self.buffer.line_col_to_byte(line_idx, 0);
        
        let caret_before = self.caret_byte;
        let indent_text = "    ";
        self.buffer.insert_str(line_start_byte, indent_text);
        
        if self.caret_byte >= line_start_byte {
            self.caret_byte += 4;
        }
        let caret_after = self.caret_byte;
        
        self.push_edit_operation(EditOperation::Insert {
            position: line_start_byte,
            text: indent_text.to_string(),
            caret_before,
            caret_after,
        });
        
        self.rebuild_visual_lines(viewport_width);
        let (_, col) = self.get_caret_visual_position();
        self.preferred_col = col;
        self.modified = true;
    }

    fn dedent_line(&mut self, viewport_width: usize) {
        let (line_idx, _, _) = self.buffer.byte_to_line_col(self.caret_byte);
        if let Some(line) = self.buffer.get_line(line_idx) {
            if let Some(line_str) = line.as_str() {
                let line_start_byte = self.buffer.line_col_to_byte(line_idx, 0);
                
                let mut spaces_to_remove = 0;
                for ch in line_str.chars().take(4) {
                    if ch == ' ' {
                        spaces_to_remove += 1;
                    } else {
                        break;
                    }
                }
                
                if spaces_to_remove > 0 {
                    let caret_before = self.caret_byte;
                    let deleted_text = self.buffer.delete_range(line_start_byte, line_start_byte + spaces_to_remove);
                    
                    if self.caret_byte >= line_start_byte + spaces_to_remove {
                        self.caret_byte -= spaces_to_remove;
                    } else if self.caret_byte > line_start_byte {
                        self.caret_byte = line_start_byte;
                    }
                    let caret_after = self.caret_byte;
                    
                    self.push_edit_operation(EditOperation::Delete {
                        position: line_start_byte,
                        text: deleted_text,
                        caret_before,
                        caret_after,
                    });
                    
                    self.rebuild_visual_lines(viewport_width);
                    let (_, col) = self.get_caret_visual_position();
                    self.preferred_col = col;
                    self.modified = true;
                }
            }
        }
    }

    fn toggle_word_wrap(&mut self, viewport_width: usize) {
        self.word_wrap = !self.word_wrap;
        self.rebuild_visual_lines(viewport_width);
    }

    fn update_viewport(&mut self, viewport_height: usize, viewport_width: usize) {
        let (visual_row, visual_col) = self.get_caret_visual_position();
        
        // Vertical scrolling
        if visual_row < self.viewport_offset_row + self.scrolloff {
            self.viewport_offset_row = visual_row.saturating_sub(self.scrolloff);
        }
        
        let bottom_threshold = self.viewport_offset_row + viewport_height.saturating_sub(self.scrolloff);
        if visual_row >= bottom_threshold && viewport_height > self.scrolloff {
            self.viewport_offset_row = visual_row + self.scrolloff + 1 - viewport_height;
        }
        
        let max_offset = self.visual_lines.len().saturating_sub(viewport_height);
        self.viewport_offset_row = self.viewport_offset_row.min(max_offset);
        
        // Horizontal scrolling (only when word wrap is disabled)
        if !self.word_wrap {
            // Ensure caret is visible with scrolloff margins
            let left_margin = self.viewport_offset_col + self.scrolloff;
            let right_margin = self.viewport_offset_col + viewport_width.saturating_sub(self.scrolloff + 1);
            
            if visual_col < left_margin {
                // Scroll left to maintain scrolloff distance from left edge
                self.viewport_offset_col = visual_col.saturating_sub(self.scrolloff);
            } else if visual_col > right_margin && viewport_width > self.scrolloff * 2 {
                // Scroll right to maintain scrolloff distance from right edge  
                self.viewport_offset_col = visual_col + self.scrolloff + 1 - viewport_width;
            }
        } else {
            self.viewport_offset_col = 0;
        }
    }

    fn scroll_viewport(&mut self, delta: i32, viewport_height: usize) {
        if delta < 0 {
            self.viewport_offset_row = self.viewport_offset_row.saturating_sub((-delta) as usize);
        } else {
            let max_offset = self.visual_lines.len().saturating_sub(viewport_height);
            self.viewport_offset_row = (self.viewport_offset_row + delta as usize).min(max_offset);
        }
    }

    fn handle_mouse_click(&mut self, col: u16, row: u16, viewport_rect: Rect) {
        let click_row = row.saturating_sub(viewport_rect.y) as usize;
        let click_col = col.saturating_sub(viewport_rect.x) as usize;
        
        let visual_row = self.viewport_offset_row + click_row;
        let visual_col = self.viewport_offset_col + click_col;
        
        if visual_row >= self.virtual_lines_count && 
           visual_row < self.visual_lines.len() - self.virtual_lines_count {
            self.caret_byte = self.visual_row_col_to_byte(visual_row, visual_col);
            self.preferred_col = visual_col;
        }
    }

    fn get_display_filename(&self) -> String {
        let base_name = match &self.filename {
            Some(path) => path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("[No Name]")
                .to_string(),
            None => "[No Name]".to_string(),
        };
        
        if self.modified {
            format!("{}*", base_name)
        } else {
            base_name
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err)
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let mut editor = Editor::new();
    
    // Handle command line arguments
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        let filename = PathBuf::from(&args[1]);
        editor.filename = Some(filename.clone());
        
        // Try to read the file
        match fs::read_to_string(&filename) {
            Ok(content) => {
                let size = terminal.size()?;
                let viewport_width = size.width as usize;
                editor.set_content(content, viewport_width);
                editor.modified = false; // Reset modified flag after loading
            }
            Err(_) => {
                // File doesn't exist, just keep empty buffer and filename
            }
        }
    }
    
    // Set terminal title
    let title = editor.get_display_filename();
    execute!(io::stdout(), SetTitle(&title))?;

    loop {
        terminal.draw(|f| ui(f, &mut editor))?;

        if let Event::Key(key) = event::read()? {
            let size = terminal.size()?;
            let viewport_width = size.width as usize;
            let viewport_height = size.height as usize - 1;
            
            match key.code {
                KeyCode::Char('q') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char('w') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    editor.toggle_word_wrap(viewport_width);
                }
                KeyCode::Char('z') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    editor.undo(viewport_width);
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Char('y') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    editor.redo(viewport_width);
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Tab => {
                    if key.modifiers.contains(event::KeyModifiers::SHIFT) {
                        editor.dedent_line(viewport_width);
                    } else {
                        editor.indent_line(viewport_width);
                    }
                }
                KeyCode::BackTab => {
                    editor.dedent_line(viewport_width);
                }
                KeyCode::Char(c) => {
                    editor.insert_char(c, viewport_width);
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Enter => {
                    editor.insert_newline(viewport_width);
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Backspace => {
                    editor.backspace(viewport_width);
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Delete => {
                    editor.delete_char(viewport_width);
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Left => {
                    editor.move_caret_left();
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Right => {
                    editor.move_caret_right();
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Up => {
                    editor.move_caret_up();
                    editor.update_viewport(viewport_height, viewport_width);
                }
                KeyCode::Down => {
                    editor.move_caret_down();
                    editor.update_viewport(viewport_height, viewport_width);
                }
                _ => {}
            }
            
            // Update terminal title if modified status changed
            let title = editor.get_display_filename();
            execute!(io::stdout(), SetTitle(&title))?;
        } else if let Event::Mouse(mouse) = event::read()? {
            match mouse.kind {
                MouseEventKind::Down(_) => {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Min(0),
                            Constraint::Length(1),
                        ].as_ref())
                        .split(terminal.size()?);
                    editor.handle_mouse_click(mouse.column, mouse.row, chunks[0]);
                }
                MouseEventKind::ScrollUp => {
                    let size = terminal.size()?;
                    editor.scroll_viewport(-3, size.height as usize - 1);
                }
                MouseEventKind::ScrollDown => {
                    let size = terminal.size()?;
                    editor.scroll_viewport(3, size.height as usize - 1);
                }
                _ => {}
            }
        } else if let Event::Resize(_, _) = event::read()? {
            let size = terminal.size()?;
            editor.rebuild_visual_lines(size.width as usize);
            editor.update_viewport(size.height as usize - 1, size.width as usize);
            terminal.clear()?;
        }
    }
}

fn ui(f: &mut Frame, editor: &mut Editor) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
        ].as_ref())
        .split(f.size());

    let viewport_height = chunks[0].height as usize;
    let viewport_width = chunks[0].width as usize;
    
    editor.update_viewport(viewport_height, viewport_width);

    let mut lines = Vec::new();
    let (caret_visual_row, caret_visual_col) = editor.get_caret_visual_position();
    
    let start_row = editor.viewport_offset_row;
    let end_row = (start_row + viewport_height).min(editor.visual_lines.len());
    
    for visual_row in start_row..end_row {
        if let Some(vline) = editor.visual_lines.get(visual_row) {
            if vline.is_virtual {
                lines.push(Line::from(vec![Span::styled("~", Style::default().fg(Color::DarkGray))]));
            } else {
                // Use Rope's slice API to get the content
                let start_char = editor.buffer.rope.byte_to_char(vline.start_byte);
                let end_char = editor.buffer.rope.byte_to_char(vline.end_byte);
                let line_slice = editor.buffer.rope.slice(start_char..end_char);
                
                // Convert slice to string - use to_string() which always works
                let line_content = line_slice.to_string();
                
                // Strip trailing newline for display
                let display_content = if line_content.ends_with('\n') {
                    &line_content[..line_content.len() - 1]
                } else {
                    &line_content
                };
                
                // Calculate the display content based on viewport offset
                let (final_display_content, display_offset) = if editor.word_wrap {
                    // With word wrap, always show from the beginning of the visual line
                    (display_content.to_string(), vline.virtual_indent)
                } else {
                    // Without word wrap, we need to handle horizontal scrolling
                    let mut display = String::new();
                    let mut current_col = 0;
                    let mut display_start_col = 0;
                    let mut found_start = false;
                    
                    // First pass: find where to start displaying
                    for ch in display_content.chars() {
                        let ch_width = ch.to_string().width();
                        
                        if !found_start && current_col + vline.virtual_indent >= editor.viewport_offset_col {
                            found_start = true;
                            display_start_col = current_col;
                        }
                        
                        if found_start {
                            let effective_col = current_col - display_start_col;
                            if effective_col < viewport_width {
                                display.push(ch);
                            } else {
                                break;
                            }
                        }
                        
                        current_col += ch_width;
                    }
                    
                    // Calculate how much virtual indent to show
                    let display_indent = if editor.viewport_offset_col < vline.virtual_indent {
                        vline.virtual_indent - editor.viewport_offset_col
                    } else {
                        0
                    };
                    
                    (display, display_indent)
                };
                
                let mut spans = Vec::new();
                
                // Add virtual indent if needed
                if display_offset > 0 {
                    spans.push(Span::raw(" ".repeat(display_offset)));
                }
                
                spans.push(Span::raw(final_display_content));
                lines.push(Line::from(spans));
            }
        }
    }
    
    while lines.len() < viewport_height {
        lines.push(Line::from(vec![]));
    }
    
    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, chunks[0]);
    
    // Calculate cursor position on screen
    if caret_visual_row >= start_row && caret_visual_row < end_row {
        let cursor_screen_row = caret_visual_row - start_row;
        let cursor_screen_col = if editor.word_wrap {
            caret_visual_col
        } else {
            if caret_visual_col >= editor.viewport_offset_col {
                caret_visual_col - editor.viewport_offset_col
            } else {
                0 // Cursor is off-screen to the left
            }
        };
        
        // Only set cursor if it's within the viewport
        if cursor_screen_col < viewport_width {
            f.set_cursor(
                chunks[0].x + cursor_screen_col as u16,
                chunks[0].y + cursor_screen_row as u16,
            );
        }
    }
    
    // Get logical line and column position
    let (logical_line, logical_col, _) = editor.buffer.byte_to_line_col(editor.caret_byte);
    let total_lines = editor.buffer.len_lines();
    
    // Create left side of status bar
    let left_status = format!(
        " {} | {}",
        editor.get_display_filename(),
        if editor.word_wrap { "Wrap" } else { "No-Wrap" }
    );
    
    // Create right side of status bar
    let right_status = format!("{}/{}:{} ", logical_line + 1, total_lines, logical_col + 1);
    
    // Calculate padding needed to right-align
    let status_width = chunks[1].width as usize;
    let left_len = left_status.len();
    let right_len = right_status.len();
    let padding_len = status_width.saturating_sub(left_len + right_len);
    
    // Build the complete status bar
    let status_spans = vec![
        Span::raw(left_status),
        Span::raw(" ".repeat(padding_len)),
        Span::raw(right_status),
    ];
    
    let status_bar = Paragraph::new(Line::from(status_spans))
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    
    f.render_widget(status_bar, chunks[1]);
}