use arboard::Clipboard;
use crossterm::{
    cursor::SetCursorStyle,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind, MouseButton},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Modifier},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
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
    logical_line: usize,
}

#[derive(Clone, Debug)]
enum EditOp {
    Insert { pos: usize, text: String },
    Delete { pos: usize, text: String },
}

struct UndoGroup {
    ops: Vec<(EditOp, usize, usize)>,
    timestamp: Instant,
}

#[derive(Debug, Clone)]
enum PromptType {
    SaveAs,
    ConfirmSave,
    FindReplace,
}

struct Prompt {
    prompt_type: PromptType,
    message: String,
    input: String,
    cursor_pos: usize,
    selection_anchor: Option<usize>,
    clipboard: Clipboard,
    replace_input: String,
    replace_cursor_pos: usize,
    replace_selection_anchor: Option<usize>,
    active_field: FindReplaceField,
    find_scroll_offset: usize,
    replace_scroll_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FindReplaceField {
    Find,
    Replace,
    Buffer,
}

impl Prompt {
    fn get_active_input(&self) -> &str {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => &self.input,
                    FindReplaceField::Replace => &self.replace_input,
                    FindReplaceField::Buffer => &self.input,  // Return find input when buffer focused
                }
            }
            _ => &self.input,
        }
    }

    fn get_active_cursor_pos(&self) -> usize {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => self.cursor_pos,
                    FindReplaceField::Replace => self.replace_cursor_pos,
                    FindReplaceField::Buffer => 0,  // Cursor not relevant when buffer focused
                }
            }
            _ => self.cursor_pos,
        }
    }

    fn set_active_cursor_pos(&mut self, pos: usize) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => self.cursor_pos = pos,
                    FindReplaceField::Replace => self.replace_cursor_pos = pos,
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => self.cursor_pos = pos,
        }
    }

    fn new_save_as(default_path: String) -> Self {
        let cursor_pos = default_path.len();
        Self {
            prompt_type: PromptType::SaveAs,
            message: "Save as:".to_string(),
            input: default_path,
            cursor_pos,
            selection_anchor: None,
            clipboard: Clipboard::new().unwrap(),
            replace_input: String::new(),
            replace_cursor_pos: 0,
            replace_selection_anchor: None,
            active_field: FindReplaceField::Find,
            find_scroll_offset: 0,
            replace_scroll_offset: 0,
        }
    }

    fn new_confirm_save() -> Self {
        Self {
            prompt_type: PromptType::ConfirmSave,
            message: "Save changes before closing? (y/n/c)".to_string(),
            input: String::new(),
            cursor_pos: 0,
            selection_anchor: None,
            clipboard: Clipboard::new().unwrap(),
            replace_input: String::new(),
            replace_cursor_pos: 0,
            replace_selection_anchor: None,
            active_field: FindReplaceField::Find,
            find_scroll_offset: 0,
            replace_scroll_offset: 0,
        }
    }

    fn new_find_replace() -> Self {
        Self {
            prompt_type: PromptType::FindReplace,
            message: String::new(),
            input: String::new(),
            cursor_pos: 0,
            selection_anchor: None,
            clipboard: Clipboard::new().unwrap(),
            replace_input: String::new(),
            replace_cursor_pos: 0,
            replace_selection_anchor: None,
            active_field: FindReplaceField::Find,
            find_scroll_offset: 0,
            replace_scroll_offset: 0,
        }
    }

    fn has_selection(&self) -> bool {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => self.selection_anchor.is_some(),
                    FindReplaceField::Replace => self.replace_selection_anchor.is_some(),
                    FindReplaceField::Buffer => false,  // No selection when buffer focused
                }
            }
            _ => self.selection_anchor.is_some(),
        }
    }

    fn get_selection_range(&self) -> Option<(usize, usize)> {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        self.selection_anchor.map(|anchor| {
                            if anchor <= self.cursor_pos {
                                (anchor, self.cursor_pos)
                            } else {
                                (self.cursor_pos, anchor)
                            }
                        })
                    }
                    FindReplaceField::Replace => {
                        self.replace_selection_anchor.map(|anchor| {
                            if anchor <= self.replace_cursor_pos {
                                (anchor, self.replace_cursor_pos)
                            } else {
                                (self.replace_cursor_pos, anchor)
                            }
                        })
                    }
                    FindReplaceField::Buffer => None,  // No selection when buffer focused
                }
            }
            _ => {
                self.selection_anchor.map(|anchor| {
                    if anchor <= self.cursor_pos {
                        (anchor, self.cursor_pos)
                    } else {
                        (self.cursor_pos, anchor)
                    }
                })
            }
        }
    }

    fn clear_selection(&mut self) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => self.selection_anchor = None,
                    FindReplaceField::Replace => self.replace_selection_anchor = None,
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => self.selection_anchor = None,
        }
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((start, end)) = self.get_selection_range() {
            if start < end {
                match self.prompt_type {
                    PromptType::FindReplace => {
                        match self.active_field {
                            FindReplaceField::Find => {
                                self.input.drain(start..end);
                                self.cursor_pos = start;
                            }
                            FindReplaceField::Replace => {
                                self.replace_input.drain(start..end);
                                self.replace_cursor_pos = start;
                            }
                            FindReplaceField::Buffer => {} // No-op when buffer focused
                        }
                    }
                    _ => {
                        self.input.drain(start..end);
                        self.cursor_pos = start;
                    }
                }
                self.clear_selection();
                return true;
            }
        }
        false
    }

    fn select_all(&mut self) {
        match self.prompt_type {
            PromptType::SaveAs => {
                self.selection_anchor = Some(0);
                self.cursor_pos = self.input.len();
            }
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        self.selection_anchor = Some(0);
                        self.cursor_pos = self.input.len();
                    }
                    FindReplaceField::Replace => {
                        self.replace_selection_anchor = Some(0);
                        self.replace_cursor_pos = self.replace_input.len();
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {}
        }
    }

    fn copy(&mut self) -> bool {
        if let Some((start, end)) = self.get_selection_range() {
            if start < end {
                let text = match self.prompt_type {
                    PromptType::FindReplace => {
                        match self.active_field {
                            FindReplaceField::Find => self.input[start..end].to_string(),
                            FindReplaceField::Replace => self.replace_input[start..end].to_string(),
                            FindReplaceField::Buffer => String::new(), // No text when buffer focused
                        }
                    }
                    _ => self.input[start..end].to_string(),
                };
                if let Err(_) = self.clipboard.set_text(text) {
                    return false;
                }
                return true;
            }
        }
        false
    }

    fn cut(&mut self) -> bool {
        if self.copy() {
            self.delete_selection();
            return true;
        }
        false
    }

    fn paste(&mut self) {
        match self.prompt_type {
            PromptType::SaveAs => {
                if let Ok(text) = self.clipboard.get_text() {
                    self.delete_selection();
                    self.input.insert_str(self.cursor_pos, &text);
                    self.cursor_pos += text.len();
                }
            }
            PromptType::FindReplace => {
                if let Ok(text) = self.clipboard.get_text() {
                    self.delete_selection();
                    match self.active_field {
                        FindReplaceField::Find => {
                            self.input.insert_str(self.cursor_pos, &text);
                            self.cursor_pos += text.len();
                        }
                        FindReplaceField::Replace => {
                            self.replace_input.insert_str(self.replace_cursor_pos, &text);
                            self.replace_cursor_pos += text.len();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn insert_char(&mut self, ch: char) {
        match self.prompt_type {
            PromptType::SaveAs => {
                self.delete_selection();
                self.input.insert(self.cursor_pos, ch);
                self.cursor_pos += ch.len_utf8();
            }
            PromptType::FindReplace => {
                self.delete_selection();
                match self.active_field {
                    FindReplaceField::Find => {
                        self.input.insert(self.cursor_pos, ch);
                        self.cursor_pos += ch.len_utf8();
                    }
                    FindReplaceField::Replace => {
                        self.replace_input.insert(self.replace_cursor_pos, ch);
                        self.replace_cursor_pos += ch.len_utf8();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn backspace(&mut self) {
        match self.prompt_type {
            PromptType::SaveAs => {
                if self.delete_selection() {
                    return;
                }
                
                if self.cursor_pos > 0 {
                    let char_boundary = self.input
                        .char_indices()
                        .rev()
                        .find(|(idx, _)| *idx < self.cursor_pos)
                        .map(|(idx, ch)| (idx, ch.len_utf8()));
                    
                    if let Some((idx, _len)) = char_boundary {
                        self.input.remove(idx);
                        self.cursor_pos = idx;
                    }
                }
            }
            PromptType::FindReplace => {
                if self.delete_selection() {
                    return;
                }
                
                match self.active_field {
                    FindReplaceField::Find => {
                        if self.cursor_pos > 0 {
                            let char_boundary = self.input
                                .char_indices()
                                .rev()
                                .find(|(idx, _)| *idx < self.cursor_pos)
                                .map(|(idx, ch)| (idx, ch.len_utf8()));
                            
                            if let Some((idx, _len)) = char_boundary {
                                self.input.remove(idx);
                                self.cursor_pos = idx;
                            }
                        }
                    }
                    FindReplaceField::Replace => {
                        if self.replace_cursor_pos > 0 {
                            let char_boundary = self.replace_input
                                .char_indices()
                                .rev()
                                .find(|(idx, _)| *idx < self.replace_cursor_pos)
                                .map(|(idx, ch)| (idx, ch.len_utf8()));
                            
                            if let Some((idx, _len)) = char_boundary {
                                self.replace_input.remove(idx);
                                self.replace_cursor_pos = idx;
                            }
                        }
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {}
        }
    }

    fn delete(&mut self) {
        match self.prompt_type {
            PromptType::SaveAs => {
                if self.delete_selection() {
                    return;
                }
                
                if self.cursor_pos < self.input.len() {
                    let char_boundary = self.input
                        .char_indices()
                        .find(|(idx, _)| *idx >= self.cursor_pos)
                        .map(|(idx, ch)| (idx, ch.len_utf8()));
                    
                    if let Some((idx, len)) = char_boundary {
                        self.input.drain(idx..idx + len);
                    }
                }
            }
            PromptType::FindReplace => {
                if self.delete_selection() {
                    return;
                }
                
                match self.active_field {
                    FindReplaceField::Find => {
                        if self.cursor_pos < self.input.len() {
                            let char_boundary = self.input
                                .char_indices()
                                .find(|(idx, _)| *idx >= self.cursor_pos)
                                .map(|(idx, ch)| (idx, ch.len_utf8()));
                            
                            if let Some((idx, len)) = char_boundary {
                                self.input.drain(idx..idx + len);
                            }
                        }
                    }
                    FindReplaceField::Replace => {
                        if self.replace_cursor_pos < self.replace_input.len() {
                            let char_boundary = self.replace_input
                                .char_indices()
                                .find(|(idx, _)| *idx >= self.replace_cursor_pos)
                                .map(|(idx, ch)| (idx, ch.len_utf8()));
                            
                            if let Some((idx, len)) = char_boundary {
                                self.replace_input.drain(idx..idx + len);
                            }
                        }
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {}
        }
    }

    fn move_cursor_left(&mut self, extend_selection: bool) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        if !extend_selection && self.has_selection() {
                            if let Some((start, _)) = self.get_selection_range() {
                                self.cursor_pos = start;
                                self.clear_selection();
                                return;
                            }
                        }
                        if extend_selection && self.selection_anchor.is_none() {
                            self.selection_anchor = Some(self.cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        if self.cursor_pos > 0 {
                            let new_pos = self.input
                                .char_indices()
                                .rev()
                                .find(|(idx, _)| *idx < self.cursor_pos)
                                .map(|(idx, _)| idx)
                                .unwrap_or(0);
                            self.cursor_pos = new_pos;
                        }
                    }
                    FindReplaceField::Replace => {
                        if !extend_selection && self.has_selection() {
                            if let Some((start, _)) = self.get_selection_range() {
                                self.replace_cursor_pos = start;
                                self.clear_selection();
                                return;
                            }
                        }
                        if extend_selection && self.replace_selection_anchor.is_none() {
                            self.replace_selection_anchor = Some(self.replace_cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        if self.replace_cursor_pos > 0 {
                            let new_pos = self.replace_input
                                .char_indices()
                                .rev()
                                .find(|(idx, _)| *idx < self.replace_cursor_pos)
                                .map(|(idx, _)| idx)
                                .unwrap_or(0);
                            self.replace_cursor_pos = new_pos;
                        }
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {
                if !extend_selection && self.has_selection() {
                    if let Some((start, _)) = self.get_selection_range() {
                        self.cursor_pos = start;
                        self.clear_selection();
                        return;
                    }
                }
                if extend_selection && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor_pos);
                } else if !extend_selection {
                    self.clear_selection();
                }
                if self.cursor_pos > 0 {
                    let new_pos = self.input
                        .char_indices()
                        .rev()
                        .find(|(idx, _)| *idx < self.cursor_pos)
                        .map(|(idx, _)| idx)
                        .unwrap_or(0);
                    self.cursor_pos = new_pos;
                }
            }
        }
    }

    fn move_cursor_right(&mut self, extend_selection: bool) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        if !extend_selection && self.has_selection() {
                            if let Some((_, end)) = self.get_selection_range() {
                                self.cursor_pos = end;
                                self.clear_selection();
                                return;
                            }
                        }
                        if extend_selection && self.selection_anchor.is_none() {
                            self.selection_anchor = Some(self.cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        if self.cursor_pos < self.input.len() {
                            let new_pos = self.input
                                .char_indices()
                                .find(|(idx, _)| *idx > self.cursor_pos)
                                .map(|(idx, _)| idx)
                                .unwrap_or(self.input.len());
                            self.cursor_pos = new_pos;
                        }
                    }
                    FindReplaceField::Replace => {
                        if !extend_selection && self.has_selection() {
                            if let Some((_, end)) = self.get_selection_range() {
                                self.replace_cursor_pos = end;
                                self.clear_selection();
                                return;
                            }
                        }
                        if extend_selection && self.replace_selection_anchor.is_none() {
                            self.replace_selection_anchor = Some(self.replace_cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        if self.replace_cursor_pos < self.replace_input.len() {
                            let new_pos = self.replace_input
                                .char_indices()
                                .find(|(idx, _)| *idx > self.replace_cursor_pos)
                                .map(|(idx, _)| idx)
                                .unwrap_or(self.replace_input.len());
                            self.replace_cursor_pos = new_pos;
                        }
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {
                if !extend_selection && self.has_selection() {
                    if let Some((_, end)) = self.get_selection_range() {
                        self.cursor_pos = end;
                        self.clear_selection();
                        return;
                    }
                }
                if extend_selection && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor_pos);
                } else if !extend_selection {
                    self.clear_selection();
                }
                if self.cursor_pos < self.input.len() {
                    let new_pos = self.input
                        .char_indices()
                        .find(|(idx, _)| *idx > self.cursor_pos)
                        .map(|(idx, _)| idx)
                        .unwrap_or(self.input.len());
                    self.cursor_pos = new_pos;
                }
            }
        }
    }

    fn move_cursor_home(&mut self, extend_selection: bool) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        if extend_selection && self.selection_anchor.is_none() {
                            self.selection_anchor = Some(self.cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        self.cursor_pos = 0;
                    }
                    FindReplaceField::Replace => {
                        if extend_selection && self.replace_selection_anchor.is_none() {
                            self.replace_selection_anchor = Some(self.replace_cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        self.replace_cursor_pos = 0;
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {
                if extend_selection && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor_pos);
                } else if !extend_selection {
                    self.clear_selection();
                }
                self.cursor_pos = 0;
            }
        }
    }

    fn move_cursor_end(&mut self, extend_selection: bool) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        if extend_selection && self.selection_anchor.is_none() {
                            self.selection_anchor = Some(self.cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        self.cursor_pos = self.input.len();
                    }
                    FindReplaceField::Replace => {
                        if extend_selection && self.replace_selection_anchor.is_none() {
                            self.replace_selection_anchor = Some(self.replace_cursor_pos);
                        } else if !extend_selection {
                            self.clear_selection();
                        }
                        self.replace_cursor_pos = self.replace_input.len();
                    }
                    FindReplaceField::Buffer => {} // No-op when buffer focused
                }
            }
            _ => {
                if extend_selection && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor_pos);
                } else if !extend_selection {
                    self.clear_selection();
                }
                self.cursor_pos = self.input.len();
            }
        }
    }

    fn handle_click(&mut self, click_x: u16, area: Rect, shift_held: bool) {
        if matches!(self.prompt_type, PromptType::SaveAs) {
            let relative_x = click_x.saturating_sub(area.x) as usize;
            
            // Find the character position based on visual width
            let mut visual_pos = 0;
            let mut byte_pos = 0;
            for (idx, ch) in self.input.char_indices() {
                if visual_pos >= relative_x {
                    byte_pos = idx;
                    break;
                }
                visual_pos += ch.to_string().width();
                byte_pos = idx + ch.len_utf8();
            }
            
            if visual_pos < relative_x {
                byte_pos = self.input.len();
            }
            
            if shift_held {
                if self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor_pos);
                }
                self.cursor_pos = byte_pos;
            } else {
                self.clear_selection();
                self.cursor_pos = byte_pos;
                self.selection_anchor = Some(self.cursor_pos);
            }
        }
    }

    fn handle_drag(&mut self, drag_x: u16, area: Rect) {
        if matches!(self.prompt_type, PromptType::SaveAs) {
            let relative_x = drag_x.saturating_sub(area.x) as usize;
            
            // Find the character position based on visual width
            let mut visual_pos = 0;
            let mut byte_pos = 0;
            for (idx, ch) in self.input.char_indices() {
                if visual_pos >= relative_x {
                    byte_pos = idx;
                    break;
                }
                visual_pos += ch.to_string().width();
                byte_pos = idx + ch.len_utf8();
            }
            
            if visual_pos < relative_x {
                byte_pos = self.input.len();
            }
            
            self.cursor_pos = byte_pos;
        }
    }

    fn update_scroll_offset(&mut self, field_width: usize) {
        match self.prompt_type {
            PromptType::FindReplace => {
                match self.active_field {
                    FindReplaceField::Find => {
                        // Calculate visual cursor position
                        let mut visual_pos = 0;
                        for (idx, ch) in self.input.char_indices() {
                            if idx >= self.cursor_pos {
                                break;
                            }
                            visual_pos += ch.to_string().width();
                        }
                        
                        // Adjust scroll offset to keep cursor visible
                        if visual_pos < self.find_scroll_offset {
                            self.find_scroll_offset = visual_pos;
                        } else if visual_pos >= self.find_scroll_offset + field_width {
                            self.find_scroll_offset = visual_pos.saturating_sub(field_width - 1);
                        }
                    }
                    FindReplaceField::Replace => {
                        // Calculate visual cursor position
                        let mut visual_pos = 0;
                        for (idx, ch) in self.replace_input.char_indices() {
                            if idx >= self.replace_cursor_pos {
                                break;
                            }
                            visual_pos += ch.to_string().width();
                        }
                        
                        // Adjust scroll offset to keep cursor visible
                        if visual_pos < self.replace_scroll_offset {
                            self.replace_scroll_offset = visual_pos;
                        } else if visual_pos >= self.replace_scroll_offset + field_width {
                            self.replace_scroll_offset = visual_pos.saturating_sub(field_width - 1);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

enum AppState {
    Editing,
    Prompting(Prompt),
    Exiting,
}

struct Editor {
    rope: Rope,
    caret: usize,
    selection_anchor: Option<usize>,
    preferred_col: usize,
    viewport_offset: (usize, usize),
    word_wrap: bool,
    visual_lines: Vec<Option<VisualLine>>,
    visual_lines_valid: bool,
    logical_line_map: Vec<(usize, usize)>,
    scrolloff: usize,
    virtual_lines: usize,
    filename: Option<PathBuf>,
    modified: bool,
    undo_stack: Vec<UndoGroup>,
    redo_stack: Vec<UndoGroup>,
    current_group: Option<UndoGroup>,
    last_edit_time: Option<Instant>,
    is_dragging: bool,
    clipboard: Clipboard,
    current_dir: PathBuf,
    app_state: AppState,
    find_matches: Vec<(usize, usize)>,
    current_match_index: Option<usize>,
}

impl Editor {
    fn new() -> Self {
        let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut editor = Self {
            rope: Rope::new(),
            caret: 0,
            selection_anchor: None,
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
            is_dragging: false,
            clipboard: Clipboard::new().unwrap(),
            current_dir,
            app_state: AppState::Editing,
            find_matches: Vec::new(),
            current_match_index: None,
        };
        editor.invalidate_visual_lines();
        editor
    }

    fn save(&mut self) -> io::Result<()> {
        if let Some(ref path) = self.filename {
            let content = self.rope.to_string();
            fs::write(path, content)?;
            self.modified = false;
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "No filename"))
        }
    }

    fn save_as(&mut self, path: PathBuf) -> io::Result<()> {
        let content = self.rope.to_string();
        fs::write(&path, content)?;
        self.filename = Some(path);
        self.modified = false;
        Ok(())
    }

    fn get_save_path_suggestion(&self) -> String {
        if let Some(ref path) = self.filename {
            path.to_string_lossy().to_string()
        } else {
            let mut path = self.current_dir.clone();
            path.push("");
            path.to_string_lossy().to_string()
        }
    }

    fn load_file(&mut self, path: PathBuf) -> io::Result<()> {
        let content = fs::read_to_string(&path)?;
        self.rope = Rope::from_str(&content);
        self.filename = Some(path.clone());
        
        // Update current directory to the file's directory
        if let Some(parent) = path.parent() {
            self.current_dir = parent.to_path_buf();
        }
        
        self.caret = 0;
        self.selection_anchor = None;
        self.preferred_col = 0;
        self.modified = false;
        self.invalidate_visual_lines();
        self.logical_line_map.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
        Ok(())
    }

    fn has_selection(&self) -> bool {
        self.selection_anchor.is_some()
    }

    fn get_selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|anchor| {
            if anchor <= self.caret {
                (anchor, self.caret)
            } else {
                (self.caret, anchor)
            }
        })
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((start, end)) = self.get_selection_range() {
            if start < end {
                let text = self.rope.byte_slice(start..end).to_string();
                let before = self.caret;
                
                let start_char = self.rope.byte_to_char(start);
                let end_char = self.rope.byte_to_char(end);
                self.rope.remove(start_char..end_char);
                
                self.caret = start;
                self.push_op(EditOp::Delete { pos: start, text }, before, self.caret);
                
                self.invalidate_visual_lines();
                self.clear_selection();
                return true;
            }
        }
        false
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
                        // Ensure positions are within bounds
                        let safe_pos = (*pos).min(self.rope.len_bytes());
                        let safe_end = (pos + text.len()).min(self.rope.len_bytes());
                        if safe_pos < self.rope.len_bytes() && safe_end <= self.rope.len_bytes() {
                            let char_pos = self.rope.byte_to_char(safe_pos);
                            let char_end = self.rope.byte_to_char(safe_end);
                            self.rope.remove(char_pos..char_end);
                        }
                    }
                    EditOp::Delete { pos, text } => {
                        let safe_pos = (*pos).min(self.rope.len_bytes());
                        self.rope.insert(self.rope.byte_to_char(safe_pos), text);
                    }
                }
                caret = *before;
            }
            
            // Ensure caret is within valid bounds
            self.caret = caret.min(self.rope.len_bytes());
            self.clear_selection();
            self.invalidate_visual_lines();
            self.logical_line_map.clear();
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
                        let safe_pos = (*pos).min(self.rope.len_bytes());
                        self.rope.insert(self.rope.byte_to_char(safe_pos), text);
                    }
                    EditOp::Delete { pos, text } => {
                        // Ensure positions are within bounds
                        let safe_pos = (*pos).min(self.rope.len_bytes());
                        let safe_end = (pos + text.len()).min(self.rope.len_bytes());
                        if safe_pos < self.rope.len_bytes() && safe_end <= self.rope.len_bytes() {
                            let char_pos = self.rope.byte_to_char(safe_pos);
                            let char_end = self.rope.byte_to_char(safe_end);
                            self.rope.remove(char_pos..char_end);
                        }
                    }
                }
                caret = *after;
            }
            
            // Ensure caret is within valid bounds
            self.caret = caret.min(self.rope.len_bytes());
            self.clear_selection();
            self.invalidate_visual_lines();
            self.logical_line_map.clear();
            self.undo_stack.push(group);
            self.modified = true;
        }
    }

    fn calculate_indent(line: &str) -> usize {
        let trimmed = line.trim_start();
        let base_indent = line.len() - trimmed.len();
        
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            return base_indent + 4;
        }
        
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
            
            let slice = if start <= content.len() {
                content.chars().skip(content[..start].chars().count())
            } else {
                break;
            };
            
            let mut char_offset = 0;
            for ch in slice {
                let ch_width = ch.to_string().width();
                if width + ch_width > available_width && char_offset > 0 {
                    end = if last_break > start { 
                        last_break 
                    } else {
                        // Calculate the byte position for char_offset characters from start
                        let mut byte_pos = start;
                        for (idx, ch) in content[start..].chars().enumerate() {
                            if idx >= char_offset {
                                break;
                            }
                            byte_pos += ch.len_utf8();
                        }
                        byte_pos
                    };
                    break;
                }
                
                width += ch_width;
                if ch == ' ' || ch == '-' || ch == '/' {
                    // Calculate byte position for the break point
                    let mut byte_pos = start;
                    for (idx, c) in content[start..].chars().enumerate() {
                        if idx == char_offset {
                            byte_pos += c.len_utf8();
                            break;
                        }
                        byte_pos += c.len_utf8();
                    }
                    last_break = byte_pos;
                }
                
                // Calculate end byte position
                let mut byte_pos = start;
                for (idx, c) in content[start..].chars().enumerate() {
                    if idx == char_offset {
                        byte_pos += c.len_utf8();
                        break;
                    }
                    byte_pos += c.len_utf8();
                }
                end = byte_pos;
                
                char_offset += 1;
            }
            
            segments.push((start, end));
            start = end;
            is_first = false;
            
            // Skip spaces at the beginning of the next line, respecting UTF-8 boundaries
            while start < content.len() {
                if let Some(ch) = content[start..].chars().next() {
                    if ch == ' ' {
                        start += ch.len_utf8();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
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
                if byte_pos == vl.end_byte && row + 1 < self.visual_lines.len() {
                    if let Some(Some(next_vl)) = self.visual_lines.get(row + 1) {
                        if next_vl.is_continuation && next_vl.start_byte == vl.end_byte {
                            return (row + 1, next_vl.indent);
                        }
                    }
                }
                
                if byte_pos >= vl.start_byte && byte_pos <= vl.end_byte {
                    let text = &self.rope.byte_slice(vl.start_byte..byte_pos).to_string();
                    let col = vl.indent + text.width();
                    return (row, col);
                }
            }
        }
        
        if let Some((row, _)) = self.visual_lines.iter().enumerate().rev().find(|(_, vl)| vl.is_some()) {
            (row, 0)
        } else {
            (self.virtual_lines, 0)
        }
    }

    fn visual_to_byte(&mut self, row: usize, col: usize, viewport_width: usize) -> usize {
        self.ensure_visual_lines(viewport_width);
        
        if let Some(Some(vline)) = self.visual_lines.get(row) {
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

    fn move_up(&mut self, viewport_width: usize, extend_selection: bool) {
        if extend_selection && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.caret);
        } else if !extend_selection {
            self.clear_selection();
        }

        let (row, _) = self.get_visual_position(self.caret, viewport_width);
        if row > self.virtual_lines {
            self.caret = self.visual_to_byte(row - 1, self.preferred_col, viewport_width);
        } else if row == self.virtual_lines && self.rope.len_bytes() > 0 {
            self.caret = 0;
        }
    }

    fn move_down(&mut self, viewport_width: usize, extend_selection: bool) {
        if extend_selection && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.caret);
        } else if !extend_selection {
            self.clear_selection();
        }

        let (row, _) = self.get_visual_position(self.caret, viewport_width);
        let total_visual_lines = self.visual_lines.len();
        let last_content_row = total_visual_lines - self.virtual_lines - 1;
        
        if row < self.virtual_lines && self.rope.len_bytes() > 0 {
            self.caret = 0;
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        } else if row < last_content_row {
            self.caret = self.visual_to_byte(row + 1, self.preferred_col, viewport_width);
        }
    }

    fn move_left(&mut self, viewport_width: usize, extend_selection: bool) {
        if !extend_selection && self.has_selection() {
            if let Some((start, _)) = self.get_selection_range() {
                self.caret = start;
                self.clear_selection();
                let (_, col) = self.get_visual_position(self.caret, viewport_width);
                self.preferred_col = col;
                return;
            }
        }

        if extend_selection && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.caret);
        } else if !extend_selection {
            self.clear_selection();
        }

        if self.caret > 0 {
            let char_idx = self.rope.byte_to_char(self.caret);
            if char_idx > 0 {
                self.caret = self.rope.char_to_byte(char_idx - 1);
                let (_, col) = self.get_visual_position(self.caret, viewport_width);
                self.preferred_col = col;
            }
        }
    }

    fn move_right(&mut self, viewport_width: usize, extend_selection: bool) {
        if !extend_selection && self.has_selection() {
            if let Some((_, end)) = self.get_selection_range() {
                self.caret = end;
                self.clear_selection();
                let (_, col) = self.get_visual_position(self.caret, viewport_width);
                self.preferred_col = col;
                return;
            }
        }

        if extend_selection && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.caret);
        } else if !extend_selection {
            self.clear_selection();
        }

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
        self.delete_selection();

        let before = self.caret;
        self.rope.insert_char(self.rope.byte_to_char(self.caret), ch);
        self.caret += ch.len_utf8();
        
        self.push_op(EditOp::Insert { pos: before, text: ch.to_string() }, before, self.caret);
        
        self.invalidate_visual_lines();
        
        let (_, col) = self.get_visual_position(self.caret, viewport_width);
        self.preferred_col = col;
    }

    fn delete(&mut self, _viewport_width: usize) {
        if self.delete_selection() {
            return;
        }

        if self.caret < self.rope.len_bytes() {
            let char_idx = self.rope.byte_to_char(self.caret);
            
            if let Some(ch) = self.rope.get_char(char_idx) {
                let before = self.caret;
                self.rope.remove(char_idx..char_idx + 1);
                
                self.push_op(EditOp::Delete { pos: self.caret, text: ch.to_string() }, before, self.caret);
                
                self.invalidate_visual_lines();
            }
        }
    }

    fn backspace(&mut self, _viewport_width: usize) {
        if self.delete_selection() {
            return;
        }

        if self.caret > 0 {
            let char_idx = self.rope.byte_to_char(self.caret);
            if char_idx > 0 {
                let ch = self.rope.char(char_idx - 1);
                let ch_bytes = ch.len_utf8();
                let before = self.caret;
                
                self.rope.remove(char_idx - 1..char_idx);
                self.caret -= ch_bytes;
                
                self.push_op(EditOp::Delete { pos: self.caret, text: ch.to_string() }, before, self.caret);
                
                self.invalidate_visual_lines();
            }
        }
    }

    fn indent(&mut self, viewport_width: usize) {
        if let Some((start, end)) = self.get_selection_range() {
            // Handle selection - indent all lines in selection
            let start_char = self.rope.byte_to_char(start);
            let end_char = self.rope.byte_to_char(end);
            let start_line = self.rope.char_to_line(start_char);
            let end_line = self.rope.char_to_line(end_char);
            
            let before_caret = self.caret;
            let mut caret_adjustment = 0;
            let mut anchor_adjustment = 0;
            
            // Process lines from end to start to avoid offset issues
            for line_idx in (start_line..=end_line).rev() {
                let line_start = self.rope.line_to_char(line_idx);
                let line_byte = self.rope.char_to_byte(line_start);
                
                self.rope.insert(line_start, "    ");
                
                // Track adjustments for caret and anchor
                if self.caret >= line_byte {
                    caret_adjustment += 4;
                }
                
                if let Some(anchor) = self.selection_anchor {
                    if anchor >= line_byte {
                        anchor_adjustment += 4;
                    }
                }
                
                self.push_op(EditOp::Insert { pos: line_byte, text: "    ".to_string() }, before_caret, self.caret);
            }
            
            // Apply adjustments
            self.caret += caret_adjustment;
            if let Some(anchor) = self.selection_anchor {
                self.selection_anchor = Some(anchor + anchor_adjustment);
            }
            
            self.invalidate_visual_lines();
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        } else {
            // No selection - indent current line only
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
            
            self.invalidate_visual_lines();
            
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        }
    }

    fn dedent(&mut self, viewport_width: usize) {
        if let Some((start, end)) = self.get_selection_range() {
            // Handle selection - dedent all lines in selection
            let start_char = self.rope.byte_to_char(start);
            let end_char = self.rope.byte_to_char(end);
            let start_line = self.rope.char_to_line(start_char);
            let end_line = self.rope.char_to_line(end_char);
            
            let before_caret = self.caret;
            let mut caret_adjustment = 0;
            let mut anchor_adjustment = 0;
            
            // Process lines from end to start to avoid offset issues
            for line_idx in (start_line..=end_line).rev() {
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
                    
                    self.rope.remove(line_start..line_start + spaces);
                    
                    // Track adjustments for caret and anchor
                    if self.caret > line_byte {
                        if self.caret >= line_byte + spaces {
                            caret_adjustment += spaces;
                        } else {
                            caret_adjustment += self.caret - line_byte;
                        }
                    }
                    
                    if let Some(anchor) = self.selection_anchor {
                        if anchor > line_byte {
                            if anchor >= line_byte + spaces {
                                anchor_adjustment += spaces;
                            } else {
                                anchor_adjustment += anchor - line_byte;
                            }
                        }
                    }
                    
                    self.push_op(EditOp::Delete { pos: line_byte, text: " ".repeat(spaces) }, before_caret, self.caret);
                }
            }
            
            // Apply adjustments
            self.caret -= caret_adjustment;
            if let Some(anchor) = self.selection_anchor {
                self.selection_anchor = Some(anchor - anchor_adjustment);
            }
            
            self.invalidate_visual_lines();
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        } else {
            // No selection - dedent current line only
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
                
                self.invalidate_visual_lines();
                
                let (_, col) = self.get_visual_position(self.caret, viewport_width);
                self.preferred_col = col;
            }
        }
    }

    fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.caret = self.rope.len_bytes();
    }

    fn copy(&mut self) -> bool {
        if let Some((start, end)) = self.get_selection_range() {
            if start < end {
                let text = self.rope.byte_slice(start..end).to_string();
                if let Err(_) = self.clipboard.set_text(text) {
                    return false;
                }
                return true;
            }
        }
        false
    }

    fn cut(&mut self) -> bool {
        if self.copy() {
            self.delete_selection();
            return true;
        }
        false
    }

    fn paste(&mut self, viewport_width: usize) {
        if let Ok(text) = self.clipboard.get_text() {
            self.delete_selection();
            
            let before = self.caret;
            let char_pos = self.rope.byte_to_char(self.caret);
            let bytes_inserted = text.len();
            self.rope.insert(char_pos, &text);
            self.caret += bytes_inserted;
            
            self.push_op(EditOp::Insert { pos: before, text: text.clone() }, before, self.caret);
            
            self.invalidate_visual_lines();
            
            let (_, col) = self.get_visual_position(self.caret, viewport_width);
            self.preferred_col = col;
        }
    }

    fn update_viewport(&mut self, height: usize, width: usize) {
        self.ensure_visual_lines(width);
        let (row, col) = self.get_visual_position(self.caret, width);
        
        if row < self.viewport_offset.0 + self.scrolloff {
            self.viewport_offset.0 = row.saturating_sub(self.scrolloff);
        } else if row >= self.viewport_offset.0 + height - self.scrolloff {
            self.viewport_offset.0 = row + self.scrolloff + 1 - height;
        }
        
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

    fn handle_click(&mut self, col: u16, row: u16, area: Rect, viewport_width: usize, shift_held: bool) {
        self.ensure_visual_lines(viewport_width);
        let click_row = self.viewport_offset.0 + row.saturating_sub(area.y) as usize;
        let click_col = self.viewport_offset.1 + col.saturating_sub(area.x) as usize;
        
        if click_row >= self.virtual_lines && 
           click_row < self.visual_lines.len() - self.virtual_lines {
            if let Some(Some(vline)) = self.visual_lines.get(click_row) {
                let actual_col = if vline.is_continuation {
                    click_col.max(vline.indent)
                } else {
                    click_col
                };
                let new_pos = self.visual_to_byte(click_row, actual_col, viewport_width);
                
                if shift_held {
                    if self.selection_anchor.is_none() {
                        self.selection_anchor = Some(self.caret);
                    }
                    self.caret = new_pos;
                } else {
                    self.clear_selection();
                    self.caret = new_pos;
                }
                
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

    fn update_find_matches(&mut self, query: &str) {
        self.find_matches.clear();
        self.current_match_index = None;

        if query.is_empty() {
            return;
        }

        let text = self.rope.to_string();
        let query_bytes = query.as_bytes();
        
        for (idx, window) in text.as_bytes().windows(query_bytes.len()).enumerate() {
            if window == query_bytes {
                self.find_matches.push((idx, idx + query_bytes.len()));
            }
        }

        if !self.find_matches.is_empty() {
            // Find the first match at or after the current caret position
            let current_pos = self.caret;
            let mut found_index = None;
            
            for (i, &(match_start, _)) in self.find_matches.iter().enumerate() {
                if match_start >= current_pos {
                    found_index = Some(i);
                    break;
                }
            }
            
            // If no match after current position, wrap to the first match
            self.current_match_index = found_index.or(Some(0));
            
            // Only jump to match if buffer is not focused
            if let AppState::Prompting(ref prompt) = self.app_state {
                if !(matches!(prompt.prompt_type, PromptType::FindReplace) && prompt.active_field == FindReplaceField::Buffer) {
                    self.jump_to_current_match();
                }
            }
        }
    }

    fn find_next(&mut self) {
        if let Some(idx) = self.current_match_index {
            if !self.find_matches.is_empty() {
                self.current_match_index = Some((idx + 1) % self.find_matches.len());
                self.jump_to_current_match();
            }
        }
    }

    fn find_previous(&mut self) {
        if let Some(idx) = self.current_match_index {
            if !self.find_matches.is_empty() {
                self.current_match_index = Some(if idx == 0 {
                    self.find_matches.len() - 1
                } else {
                    idx - 1
                });
                self.jump_to_current_match();
            }
        }
    }

    fn jump_to_current_match(&mut self) {
        if let Some(idx) = self.current_match_index {
            if let Some(&(start, _)) = self.find_matches.get(idx) {
                self.caret = start;
                self.selection_anchor = None;
                self.preferred_col = 0;
            }
        }
    }

    fn replace_current(&mut self, replacement: &str, viewport_width: usize) {
        if let Some(idx) = self.current_match_index {
            if let Some(&(start, end)) = self.find_matches.get(idx) {
                // Finalize any pending undo group before starting replace
                self.finalize_undo_group();
                
                self.caret = start;
                self.selection_anchor = Some(end);
                
                self.delete_selection();
                for ch in replacement.chars() {
                    self.insert_char(ch, viewport_width);
                }
                
                // Finalize the replace operation as its own undo group
                self.finalize_undo_group();
                // Reset last edit time to prevent timing issues with immediate undo
                self.last_edit_time = None;
                
                let query = if let AppState::Prompting(ref prompt) = self.app_state {
                    prompt.input.clone()
                } else {
                    String::new()
                };
                
                if !query.is_empty() {
                    // Remember the position after replacement
                    let position_after_replace = self.caret;
                    
                    self.update_find_matches(&query);
                    
                    // After updating matches, find the next match AFTER the replacement
                    if !self.find_matches.is_empty() {
                        let mut found_next = false;
                        
                        // Look for a match that starts after our current position
                        for (i, &(match_start, _)) in self.find_matches.iter().enumerate() {
                            if match_start > position_after_replace {
                                self.current_match_index = Some(i);
                                found_next = true;
                                break;
                            }
                        }
                        
                        // If no match after current position, wrap to the first match
                        if !found_next {
                            self.current_match_index = Some(0);
                        }
                        
                        self.jump_to_current_match();
                    } else {
                        self.current_match_index = None;
                    }
                }
            }
        }
    }

    fn replace_all(&mut self, query: &str, replacement: &str, viewport_width: usize) {
        if query.is_empty() {
            return;
        }

        // Finalize any pending undo group before starting replace all
        self.finalize_undo_group();

        self.update_find_matches(query);
        
        while !self.find_matches.is_empty() {
            if let Some(&(start, end)) = self.find_matches.get(0) {
                self.caret = start;
                self.selection_anchor = Some(end);
                
                self.delete_selection();
                for ch in replacement.chars() {
                    self.insert_char(ch, viewport_width);
                }
                
                self.update_find_matches(query);
            } else {
                break;
            }
        }
        
        // Finalize the replace all operation as its own undo group
        self.finalize_undo_group();
        // Reset last edit time to prevent timing issues with immediate undo
        self.last_edit_time = None;
    }

    fn refresh_find_matches_if_active(&mut self) {
        if let AppState::Prompting(ref prompt) = self.app_state {
            if matches!(prompt.prompt_type, PromptType::FindReplace) && !prompt.input.is_empty() {
                let query = prompt.input.clone();
                self.update_find_matches(&query);
            }
        }
    }

    fn clear_find_matches(&mut self) {
        self.find_matches.clear();
        self.current_match_index = None;
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
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape
    )?;
    terminal.show_cursor()?;
    
    if let Err(err) = result {
        eprintln!("Error: {:?}", err);
    }
    
    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let mut editor = Editor::new();
    
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
        
        if let AppState::Exiting = editor.app_state {
            return Ok(());
        }
        
        match event::read()? {
            Event::Key(key) => {
                let size = terminal.size()?;
                let viewport_width = size.width as usize;
                let viewport_height = size.height as usize - 1;
                
                match &mut editor.app_state {
                    AppState::Prompting(prompt) => {
                        // Handle buffer-focused input for find/replace mode
                        if matches!(prompt.prompt_type, PromptType::FindReplace) && prompt.active_field == FindReplaceField::Buffer {
                            // Allow normal editor commands except Tab and Esc
                            match key.code {
                                KeyCode::Esc => {
                                    editor.clear_find_matches();
                                    editor.app_state = AppState::Editing;
                                }
                                KeyCode::Tab => {
                                    // Switch focus back to find field
                                    prompt.active_field = FindReplaceField::Find;
                                }
                                KeyCode::Char('f') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                    if key.modifiers.contains(event::KeyModifiers::SHIFT) {
                                        editor.find_previous();
                                    } else {
                                        editor.find_next();
                                    }
                                    editor.update_viewport(viewport_height, viewport_width);
                                }
                                KeyCode::Char('h') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                    if key.modifiers.contains(event::KeyModifiers::SHIFT) {
                                        let query = prompt.input.clone();
                                        let replacement = prompt.replace_input.clone();
                                        editor.replace_all(&query, &replacement, viewport_width);
                                        editor.update_viewport(viewport_height, viewport_width);
                                        editor.clear_find_matches();
                                        editor.app_state = AppState::Editing;
                                    } else {
                                        let replacement = prompt.replace_input.clone();
                                        editor.replace_current(&replacement, viewport_width);
                                        editor.update_viewport(viewport_height, viewport_width);
                                    }
                                }
                                _ => {
                                    // Handle normal editor commands
                                    handle_editor_key(&mut editor, key, viewport_width, viewport_height)?;
                                }
                            }
                        } else {
                            // Normal prompt handling
                            match key.code {
                                KeyCode::Esc => {
                                    if matches!(prompt.prompt_type, PromptType::FindReplace) {
                                        editor.clear_find_matches();
                                    }
                                    editor.app_state = AppState::Editing;
                                }
                            KeyCode::Enter => {
                                match prompt.prompt_type {
                                    PromptType::SaveAs => {
                                        if !prompt.input.is_empty() {
                                            let path = PathBuf::from(&prompt.input);
                                            if let Err(e) = editor.save_as(path) {
                                                // TODO: Show error message
                                                eprintln!("Save failed: {:?}", e);
                                            } else {
                                                execute!(io::stdout(), SetTitle(&editor.get_display_name()))?;
                                            }
                                            editor.clear_find_matches();
                                            editor.app_state = AppState::Editing;
                                        }
                                    }
                                    PromptType::ConfirmSave => {
                                        // Handle in the key event below
                                    }
                                    PromptType::FindReplace => {
                                        // Handle Enter for find operation
                                        let query = prompt.input.clone();
                                        editor.update_find_matches(&query);
                                    }
                                }
                            }
                            KeyCode::Char('a') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                prompt.select_all();
                            }
                            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                prompt.copy();
                            }
                            KeyCode::Char('x') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                prompt.cut();
                            }
                            KeyCode::Char('v') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                prompt.paste();
                            }
                            KeyCode::Tab if matches!(prompt.prompt_type, PromptType::FindReplace) => {
                                // Switch between find, replace, and buffer
                                match prompt.active_field {
                                    FindReplaceField::Find => prompt.active_field = FindReplaceField::Replace,
                                    FindReplaceField::Replace => prompt.active_field = FindReplaceField::Buffer,
                                    FindReplaceField::Buffer => prompt.active_field = FindReplaceField::Find,
                                }
                            }
                            KeyCode::Char('z') if key.modifiers.contains(event::KeyModifiers::CONTROL) && matches!(prompt.prompt_type, PromptType::FindReplace) => {
                                // Undo in main buffer
                                editor.undo();
                                editor.refresh_find_matches_if_active();
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('y') if key.modifiers.contains(event::KeyModifiers::CONTROL) && matches!(prompt.prompt_type, PromptType::FindReplace) => {
                                // Redo in main buffer
                                editor.redo();
                                editor.refresh_find_matches_if_active();
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('f') if key.modifiers.contains(event::KeyModifiers::CONTROL) && matches!(prompt.prompt_type, PromptType::FindReplace) => {
                                if key.modifiers.contains(event::KeyModifiers::ALT) {
                                    // Find previous (Ctrl+Alt+F)
                                    editor.find_previous();
                                } else {
                                    // Find next
                                    editor.find_next();
                                }
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('r') if key.modifiers.contains(event::KeyModifiers::CONTROL) && matches!(prompt.prompt_type, PromptType::FindReplace) => {
                                if key.modifiers.contains(event::KeyModifiers::ALT) {
                                    // Replace all (Ctrl+Alt+R)
                                    let query = prompt.input.clone();
                                    let replacement = prompt.replace_input.clone();
                                    editor.replace_all(&query, &replacement, viewport_width);
                                    editor.update_viewport(viewport_height, viewport_width);
                                    editor.clear_find_matches();
                                    editor.app_state = AppState::Editing;
                                } else {
                                    // Replace current and find next
                                    let replacement = prompt.replace_input.clone();
                                    editor.replace_current(&replacement, viewport_width);
                                    editor.update_viewport(viewport_height, viewport_width);
                                }
                            }
                            KeyCode::Char(ch) => {
                                match prompt.prompt_type {
                                    PromptType::ConfirmSave => {
                                        match ch.to_ascii_lowercase() {
                                            'y' => {
                                                if editor.filename.is_some() {
                                                    if let Err(e) = editor.save() {
                                                        eprintln!("Save failed: {:?}", e);
                                                    }
                                                    editor.app_state = AppState::Exiting;
                                                } else {
                                                    let path = editor.get_save_path_suggestion();
                                                    editor.app_state = AppState::Prompting(Prompt::new_save_as(path));
                                                }
                                            }
                                            'n' => {
                                                editor.app_state = AppState::Exiting;
                                            }
                                            'c' => {
                                                if matches!(prompt.prompt_type, PromptType::FindReplace) {
                                                    editor.clear_find_matches();
                                                }
                                                editor.app_state = AppState::Editing;
                                            }
                                            _ => {}
                                        }
                                    }
                                    _ => {
                                        prompt.insert_char(ch);
                                        if matches!(prompt.prompt_type, PromptType::FindReplace) && prompt.active_field == FindReplaceField::Find {
                                            let query = prompt.input.clone();
                                            editor.update_find_matches(&query);
                                        }
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                prompt.backspace();
                                if matches!(prompt.prompt_type, PromptType::FindReplace) && prompt.active_field == FindReplaceField::Find {
                                    let query = prompt.input.clone();
                                    editor.update_find_matches(&query);
                                }
                            }
                            KeyCode::Delete => {
                                prompt.delete();
                                if matches!(prompt.prompt_type, PromptType::FindReplace) && prompt.active_field == FindReplaceField::Find {
                                    let query = prompt.input.clone();
                                    editor.update_find_matches(&query);
                                }
                            }
                            KeyCode::Left => {
                                prompt.move_cursor_left(key.modifiers.contains(event::KeyModifiers::SHIFT));
                            }
                            KeyCode::Right => {
                                prompt.move_cursor_right(key.modifiers.contains(event::KeyModifiers::SHIFT));
                            }
                            KeyCode::Home => {
                                prompt.move_cursor_home(key.modifiers.contains(event::KeyModifiers::SHIFT));
                            }
                            KeyCode::End => {
                                prompt.move_cursor_end(key.modifiers.contains(event::KeyModifiers::SHIFT));
                            }
                            _ => {}
                        }
                        }
                    }
                    AppState::Editing => {
                        match key.code {
                            KeyCode::Char('q') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                if editor.modified {
                                    editor.app_state = AppState::Prompting(Prompt::new_confirm_save());
                                } else {
                                    return Ok(());
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                if key.modifiers.contains(event::KeyModifiers::SHIFT) || key.modifiers.contains(event::KeyModifiers::ALT) {
                                    // Save As (Ctrl+Shift+S or Ctrl+Alt+S)
                                    let path = editor.get_save_path_suggestion();
                                    editor.app_state = AppState::Prompting(Prompt::new_save_as(path));
                                } else {
                                    // Save (Ctrl+S)
                                    if editor.filename.is_some() {
                                        if let Err(e) = editor.save() {
                                            eprintln!("Save failed: {:?}", e);
                                        } else {
                                            execute!(io::stdout(), SetTitle(&editor.get_display_name()))?;
                                        }
                                    } else {
                                        let path = editor.get_save_path_suggestion();
                                        editor.app_state = AppState::Prompting(Prompt::new_save_as(path));
                                    }
                                }
                            }
                            KeyCode::F(12) => {
                                // Save As (F12) - Alternative to Ctrl+Shift+S
                                let path = editor.get_save_path_suggestion();
                                editor.app_state = AppState::Prompting(Prompt::new_save_as(path));
                            }
                            KeyCode::Char('a') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.select_all();
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.copy();
                            }
                            KeyCode::Char('x') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                if editor.cut() {
                                    editor.update_viewport(viewport_height, viewport_width);
                                }
                            }
                            KeyCode::Char('v') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.paste(viewport_width);
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('w') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.word_wrap = !editor.word_wrap;
                                editor.invalidate_visual_lines();
                                editor.logical_line_map.clear();
                            }
                            KeyCode::Char('z') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.undo();
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('y') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.redo();
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Char('f') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                editor.app_state = AppState::Prompting(Prompt::new_find_replace());
                            }
                            KeyCode::Tab => {
                                if key.modifiers.contains(event::KeyModifiers::SHIFT) {
                                    editor.dedent(viewport_width);
                                } else {
                                    editor.indent(viewport_width);
                                }
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::BackTab => {
                                editor.dedent(viewport_width);
                                editor.update_viewport(viewport_height, viewport_width);
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
                                editor.move_left(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Right => {
                                editor.move_right(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Up => {
                                editor.move_up(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            KeyCode::Down => {
                                editor.move_down(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
                                editor.update_viewport(viewport_height, viewport_width);
                            }
                            _ => {}
                        }
                        
                        execute!(io::stdout(), SetTitle(&editor.get_display_name()))?;
                    }
                    AppState::Exiting => {}
                }
            }
            Event::Mouse(mouse) => {
                match &mut editor.app_state {
                    AppState::Prompting(prompt) => {
                        // Get the prompt area coordinates
                        let area = centered_rect(60, 20, terminal.size()?);
                        let inner = Block::default()
                            .borders(Borders::ALL)
                            .inner(area);
                        
                        let input_area = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([
                                Constraint::Length(1),
                                Constraint::Length(1),
                                Constraint::Min(1),
                            ])
                            .split(inner);
                        
                        let input_y = input_area[1].y;
                        
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                if mouse.row == input_y && 
                                   mouse.column >= inner.x && 
                                   mouse.column < inner.x + inner.width {
                                    let shift_held = mouse.modifiers.contains(event::KeyModifiers::SHIFT);
                                    prompt.handle_click(mouse.column, inner, shift_held);
                                }
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                if mouse.row == input_y &&
                                   mouse.column >= inner.x && 
                                   mouse.column < inner.x + inner.width {
                                    prompt.handle_drag(mouse.column, inner);
                                }
                            }
                            _ => {}
                        }
                    }
                    AppState::Editing => {
                        let size = terminal.size()?;
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                let chunks = Layout::default()
                                    .direction(Direction::Vertical)
                                    .constraints([
                                        Constraint::Min(0),
                                        Constraint::Length(1),
                                    ])
                                    .split(size);
                                
                                let shift_held = mouse.modifiers.contains(event::KeyModifiers::SHIFT);
                                editor.handle_click(mouse.column, mouse.row, chunks[0], size.width as usize, shift_held);
                                
                                editor.is_dragging = true;
                                if !shift_held {
                                    editor.selection_anchor = Some(editor.caret);
                                }
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                if editor.is_dragging {
                                    let chunks = Layout::default()
                                        .direction(Direction::Vertical)
                                        .constraints([
                                            Constraint::Min(0),
                                            Constraint::Length(1),
                                        ])
                                        .split(size);
                                    
                                    let click_row = editor.viewport_offset.0 + mouse.row.saturating_sub(chunks[0].y) as usize;
                                    let click_col = editor.viewport_offset.1 + mouse.column.saturating_sub(chunks[0].x) as usize;
                                    
                                    if click_row >= editor.virtual_lines && 
                                       click_row < editor.visual_lines.len() - editor.virtual_lines {
                                        if let Some(Some(vline)) = editor.visual_lines.get(click_row) {
                                            let actual_col = if vline.is_continuation {
                                                click_col.max(vline.indent)
                                            } else {
                                                click_col
                                            };
                                            editor.caret = editor.visual_to_byte(click_row, actual_col, size.width as usize);
                                            editor.preferred_col = actual_col;
                                        }
                                    }
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                editor.is_dragging = false;
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
                    AppState::Exiting => {}
                }
            }
            Event::Resize(_, _) => {
                let size = terminal.size()?;
                editor.invalidate_visual_lines();
                editor.logical_line_map.clear();
                editor.update_viewport(size.height as usize - 1, size.width as usize);
            }
            _ => {}
        }
    }
}

fn handle_editor_key(editor: &mut Editor, key: event::KeyEvent, viewport_width: usize, viewport_height: usize) -> io::Result<()> {
    match key.code {
        KeyCode::Char('a') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            editor.select_all();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            editor.copy();
        }
        KeyCode::Char('x') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            if editor.cut() {
                editor.refresh_find_matches_if_active();
                editor.update_viewport(viewport_height, viewport_width);
            }
        }
        KeyCode::Char('v') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            editor.paste(viewport_width);
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Char('z') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            editor.undo();
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Char('y') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            editor.redo();
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Char(c) => {
            editor.insert_char(c, viewport_width);
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Enter => {
            editor.insert_char('\n', viewport_width);
            editor.preferred_col = 0;
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Backspace => {
            editor.backspace(viewport_width);
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Delete => {
            editor.delete(viewport_width);
            editor.refresh_find_matches_if_active();
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Left => {
            editor.move_left(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Right => {
            editor.move_right(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Up => {
            editor.move_up(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
            editor.update_viewport(viewport_height, viewport_width);
        }
        KeyCode::Down => {
            editor.move_down(viewport_width, key.modifiers.contains(event::KeyModifiers::SHIFT));
            editor.update_viewport(viewport_height, viewport_width);
        }
        _ => {}
    }
    execute!(io::stdout(), SetTitle(&editor.get_display_name()))?;
    Ok(())
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
    
    editor.ensure_visual_lines(viewport_width);
    editor.update_viewport(viewport_height, viewport_width);
    
    let selection_range = editor.get_selection_range();
    
    let mut lines = Vec::new();
    let (caret_row, caret_col) = editor.get_visual_position(editor.caret, viewport_width);
    
    let start = editor.viewport_offset.0;
    let end = (start + viewport_height).min(editor.visual_lines.len());
    
    for row in start..end {
        if let Some(vline_opt) = editor.visual_lines.get(row) {
            if let Some(vline) = vline_opt {
                let text = editor.rope.byte_slice(vline.start_byte..vline.end_byte).to_string();
                
                let (display_text, display_start_offset) = if editor.word_wrap || editor.viewport_offset.1 == 0 {
                    (text, 0)
                } else {
                    let mut result = String::new();
                    let mut width = 0;
                    let mut byte_offset = 0;
                    let mut display_start_offset = 0;
                    let mut found_start = false;
                    
                    for ch in text.chars() {
                        let ch_width = ch.to_string().width();
                        width += ch_width;
                        
                        if width > editor.viewport_offset.1 {
                            if !found_start {
                                display_start_offset = byte_offset;
                                found_start = true;
                            }
                            result.push(ch);
                        }
                        
                        byte_offset += ch.len_utf8();
                    }
                    (result, display_start_offset)
                };
                
                let mut spans = vec![];
                if vline.indent > 0 {
                    spans.push(Span::raw(" ".repeat(vline.indent)));
                }
                
                // Check for find matches in this line
                let mut char_styles = vec![Style::default(); display_text.len()];
                
                // Apply selection highlighting
                if let Some((sel_start, sel_end)) = selection_range {
                    let line_start = vline.start_byte;
                    let line_end = vline.end_byte;
                    
                    if sel_end > line_start && sel_start < line_end {
                        let mut byte_pos = display_start_offset;
                        for (i, ch) in display_text.chars().enumerate() {
                            let global_pos = line_start + byte_pos;
                            if global_pos >= sel_start && global_pos < sel_end {
                                char_styles[i] = Style::default().bg(Color::Blue).fg(Color::White);
                            }
                            byte_pos += ch.len_utf8();
                        }
                    }
                }
                
                // Apply find match highlighting
                let line_start = vline.start_byte;
                for &(match_start, match_end) in &editor.find_matches {
                    if match_end > line_start && match_start < vline.end_byte {
                        let mut byte_pos = display_start_offset;
                        for (i, ch) in display_text.chars().enumerate() {
                            let global_pos = line_start + byte_pos;
                            if global_pos >= match_start && global_pos < match_end {
                                // Current match gets a different color
                                if let Some(current_idx) = editor.current_match_index {
                                    if editor.find_matches.get(current_idx) == Some(&(match_start, match_end)) {
                                        char_styles[i] = Style::default().bg(Color::Yellow).fg(Color::Black);
                                    } else {
                                        char_styles[i] = Style::default().bg(Color::Green).fg(Color::Black);
                                    }
                                } else {
                                    char_styles[i] = Style::default().bg(Color::Green).fg(Color::Black);
                                }
                            }
                            byte_pos += ch.len_utf8();
                        }
                    }
                }
                
                // Build spans with styles
                for (i, ch) in display_text.chars().enumerate() {
                    spans.push(Span::styled(ch.to_string(), char_styles[i]));
                }
                
                lines.push(Line::from(spans));
            } else {
                lines.push(Line::from(vec![Span::styled("~", Style::default().fg(Color::DarkGray))]));
            }
        }
    }
    
    while lines.len() < viewport_height {
        lines.push(Line::default());
    }
    
    let paragraph = Paragraph::new(lines.clone());
    f.render_widget(paragraph, chunks[0]);
    
    // Draw prompt if active
    if let AppState::Prompting(prompt) = &mut editor.app_state {
        match prompt.prompt_type {
            PromptType::SaveAs => {
                let area = centered_rect(60, 20, f.size());
                f.render_widget(Clear, area);
                
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title("Save As")
                    .style(Style::default().bg(Color::Black));
                
                let inner = block.inner(area);
                f.render_widget(block, area);
                
                let input_area = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Min(1),
                    ])
                    .split(inner);
                
                let message = Paragraph::new(prompt.message.as_str());
                f.render_widget(message, input_area[0]);
                
                // Render input with selection highlighting
                let mut spans = vec![];
                if let Some((sel_start, sel_end)) = prompt.get_selection_range() {
                    for (idx, ch) in prompt.input.char_indices() {
                        let ch_str = ch.to_string();
                        if idx >= sel_start && idx < sel_end {
                            spans.push(Span::styled(ch_str, Style::default().bg(Color::Blue).fg(Color::White)));
                        } else {
                            spans.push(Span::raw(ch_str));
                        }
                    }
                } else {
                    spans.push(Span::raw(&prompt.input));
                }
                
                let input = Paragraph::new(Line::from(spans))
                    .style(Style::default().add_modifier(Modifier::UNDERLINED));
                f.render_widget(input, input_area[1]);
                
                // Set cursor position in prompt
                let mut visual_cursor_pos = 0;
                for (idx, ch) in prompt.input.char_indices() {
                    if idx >= prompt.cursor_pos {
                        break;
                    }
                    visual_cursor_pos += ch.to_string().width();
                }
                let cursor_x = inner.x + visual_cursor_pos.min(inner.width as usize - 1) as u16;
                f.set_cursor(cursor_x, input_area[1].y);
            }
            PromptType::ConfirmSave => {
                let area = centered_rect(60, 20, f.size());
                f.render_widget(Clear, area);
                
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title("Unsaved Changes")
                    .style(Style::default().bg(Color::Black));
                
                let inner = block.inner(area);
                f.render_widget(block, area);
                
                let message = Paragraph::new(prompt.message.as_str());
                f.render_widget(message, inner);
            }
            PromptType::FindReplace => {
                // Render find/replace as a bar at the bottom above the status bar
                let find_replace_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Min(0),
                        Constraint::Length(3),
                        Constraint::Length(1),
                    ])
                    .split(f.size());
                
                let find_replace_area = find_replace_chunks[1];
                f.render_widget(Clear, find_replace_area);
                
                let block_style = if prompt.active_field == FindReplaceField::Buffer {
                    Style::default().bg(Color::Black).fg(Color::DarkGray)
                } else {
                    Style::default().bg(Color::Black)
                };
                
                let block = Block::default()
                    .borders(Borders::ALL)
                    .style(block_style)
                    .title(" Find/Replace (Tab to switch focus) ");
                
                let inner = block.inner(find_replace_area);
                f.render_widget(block, find_replace_area);
                
                // Split into find and replace fields
                let fields = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(10),
                        Constraint::Min(20),
                        Constraint::Length(10),
                        Constraint::Min(20),
                    ])
                    .split(inner);
                
                // Find label and field
                let find_label = Paragraph::new("Find: ");
                f.render_widget(find_label, fields[0]);
                
                // Update scroll offset for the find field
                let field_width = fields[1].width as usize;
                prompt.update_scroll_offset(field_width);
                
                // Find input field
                let mut find_spans = vec![];
                let mut visual_pos = 0;
                let mut display_width = 0;
                
                // Build the visible text with proper scrolling
                for (idx, ch) in prompt.input.char_indices() {
                    let ch_width = ch.to_string().width();
                    
                    if visual_pos >= prompt.find_scroll_offset && display_width < field_width {
                        let ch_str = ch.to_string();
                        let style = if prompt.active_field == FindReplaceField::Find {
                            if let Some((sel_start, sel_end)) = prompt.get_selection_range() {
                                if idx >= sel_start && idx < sel_end {
                                    Style::default().bg(Color::Blue).fg(Color::White)
                                } else {
                                    Style::default()
                                }
                            } else {
                                Style::default()
                            }
                        } else {
                            Style::default()
                        };
                        find_spans.push(Span::styled(ch_str, style));
                        display_width += ch_width;
                    }
                    visual_pos += ch_width;
                }
                
                let find_style = if prompt.active_field == FindReplaceField::Find {
                    Style::default().add_modifier(Modifier::UNDERLINED).fg(Color::Yellow)
                } else {
                    Style::default().add_modifier(Modifier::UNDERLINED)
                };
                
                let find_input = Paragraph::new(Line::from(find_spans))
                    .style(find_style);
                f.render_widget(find_input, fields[1]);
                
                // Replace label and field
                let replace_label = Paragraph::new("Replace: ");
                f.render_widget(replace_label, fields[2]);
                
                // Replace input field
                let mut replace_spans = vec![];
                let mut visual_pos = 0;
                let mut display_width = 0;
                let replace_field_width = fields[3].width as usize;
                
                // Build the visible text with proper scrolling
                for (idx, ch) in prompt.replace_input.char_indices() {
                    let ch_width = ch.to_string().width();
                    
                    if visual_pos >= prompt.replace_scroll_offset && display_width < replace_field_width {
                        let ch_str = ch.to_string();
                        let style = if prompt.active_field == FindReplaceField::Replace {
                            if let Some((sel_start, sel_end)) = prompt.get_selection_range() {
                                if idx >= sel_start && idx < sel_end {
                                    Style::default().bg(Color::Blue).fg(Color::White)
                                } else {
                                    Style::default()
                                }
                            } else {
                                Style::default()
                            }
                        } else {
                            Style::default()
                        };
                        replace_spans.push(Span::styled(ch_str, style));
                        display_width += ch_width;
                    }
                    visual_pos += ch_width;
                }
                
                let replace_style = if prompt.active_field == FindReplaceField::Replace {
                    Style::default().add_modifier(Modifier::UNDERLINED).fg(Color::Yellow)
                } else {
                    Style::default().add_modifier(Modifier::UNDERLINED)
                };
                
                let replace_input = Paragraph::new(Line::from(replace_spans))
                    .style(replace_style);
                f.render_widget(replace_input, fields[3]);
                
                // Set cursor position based on active field
                if prompt.active_field != FindReplaceField::Buffer {
                    let cursor_field = match prompt.active_field {
                        FindReplaceField::Find => {
                            let mut visual_cursor_pos = 0;
                            for (idx, ch) in prompt.input.char_indices() {
                                if idx >= prompt.cursor_pos {
                                    break;
                                }
                                visual_cursor_pos += ch.to_string().width();
                            }
                            let screen_pos = visual_cursor_pos.saturating_sub(prompt.find_scroll_offset);
                            (fields[1].x + screen_pos.min(fields[1].width as usize - 1) as u16, fields[1].y)
                        }
                        FindReplaceField::Replace => {
                            let mut visual_cursor_pos = 0;
                            for (idx, ch) in prompt.replace_input.char_indices() {
                                if idx >= prompt.replace_cursor_pos {
                                    break;
                                }
                                visual_cursor_pos += ch.to_string().width();
                            }
                            let screen_pos = visual_cursor_pos.saturating_sub(prompt.replace_scroll_offset);
                            (fields[3].x + screen_pos.min(fields[3].width as usize - 1) as u16, fields[3].y)
                        }
                        _ => unreachable!(),
                    };
                    f.set_cursor(cursor_field.0, cursor_field.1);
                } else {
                    // When buffer has focus, set cursor in the editor area
                    let (caret_row, caret_col) = editor.get_visual_position(editor.caret, viewport_width);
                    if caret_row >= editor.viewport_offset.0 && caret_row < editor.viewport_offset.0 + viewport_height {
                        let screen_row = caret_row - editor.viewport_offset.0;
                        let screen_col = if editor.word_wrap {
                            caret_col
                        } else {
                            caret_col.saturating_sub(editor.viewport_offset.1)
                        };
                        
                        if screen_col < viewport_width {
                            f.set_cursor(
                                find_replace_chunks[0].x + screen_col as u16,
                                find_replace_chunks[0].y + screen_row as u16,
                            );
                        }
                    }
                }
                
                // Still render the main editor area above the find/replace bar
                let editor_paragraph = Paragraph::new(lines.clone());
                f.render_widget(editor_paragraph, find_replace_chunks[0]);
                
                // Render status bar below find/replace
                let (line, col) = editor.get_position();
                let selection_info = if editor.has_selection() {
                    if let Some((start, end)) = selection_range {
                        format!(" | {} chars selected", end - start)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                
                let total_lines = editor.rope.len_lines();
                let match_info = if editor.find_matches.is_empty() {
                    "0 matches".to_string()
                } else if let Some(current_idx) = editor.current_match_index {
                    format!("{}/{} matches", current_idx + 1, editor.find_matches.len())
                } else {
                    format!("{} matches", editor.find_matches.len())
                };
                let status_text_fr = format!(
                    " {} | {} | {}/{}:{}{} | {} ",
                    editor.get_display_name(),
                    if editor.word_wrap { "Wrap" } else { "No-Wrap" },
                    line,
                    total_lines,
                    col,
                    selection_info,
                    match_info
                );
                
                let status_fr = Paragraph::new(Line::from(vec![Span::raw(status_text_fr)]))
                    .style(Style::default().bg(Color::DarkGray).fg(Color::White))
                    .alignment(Alignment::Left);
                
                f.render_widget(status_fr, find_replace_chunks[2]);
                
                // Early return to avoid rendering the normal editor UI
                return;
            }
        }
    } else {
        // Set cursor position in editor
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
    }
    
    let cursor_style = if editor.has_selection() {
        SetCursorStyle::SteadyUnderScore
    } else {
        SetCursorStyle::SteadyBlock
    };
    execute!(io::stdout(), cursor_style).unwrap();
    
    // Render status bar
    let (line, col) = editor.get_position();
    let selection_info = if editor.has_selection() {
        if let Some((start, end)) = selection_range {
            format!(" | {} chars selected", end - start)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    
    let total_lines = editor.rope.len_lines();
    let status_text = format!(
        " {} | {} | {}/{}:{}{} ",
        editor.get_display_name(),
        if editor.word_wrap { "Wrap" } else { "No-Wrap" },
        line,
        total_lines,
        col,
        selection_info
    );
    
    let status = Paragraph::new(Line::from(vec![Span::raw(status_text)]))
        .style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .alignment(Alignment::Left);
    
    f.render_widget(status, chunks[1]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}