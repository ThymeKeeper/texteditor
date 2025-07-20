# Text Editor

A modern, efficient terminal-based text editor written in Rust. Because sometimes you just need to edit text without your IDE asking if you've considered upgrading to the premium subscription.

## Features

- **Full Unicode Support** - Edit in any language, emoji included 🚀
- **Efficient Text Handling** - Built on rope data structures for blazing-fast performance with large files
- **Find and Replace** - With visual highlighting and replace-all functionality
- **Undo/Redo** - Full history with intelligent operation grouping
- **Word Wrapping** - Toggle visual line wrapping without modifying your files
- **Line Movement** - Shuffle lines up and down like a deck of cards
- **Mouse Support** - Click and drag text selection for when keyboard shortcuts feel like too much work
- **Cross-Platform Clipboard** - Copy, cut, and paste with system clipboard integration

## Installation

### Prerequisites
- Rust toolchain (1.70.0 or later)
- A terminal emulator with UTF-8 support

### Building from Source
```bash
# Clone the repository
git clone <repository-url>
cd texteditor

# Build the project
cargo build --release

# The binary will be available at ./target/release/texteditor
```

## Usage

```bash
# Open the editor with a new file
texteditor

# Open an existing file
texteditor filename.txt

# Or use cargo run during development
cargo run -- myfile.rs
```

## Key Bindings

### File Operations
- `Ctrl+Q` - Quit (with save prompt for unsaved changes)
- `Ctrl+S` - Save
- `Ctrl+Shift+S` / `Ctrl+Alt+S` - Save As

### Editing
- `Ctrl+Z` - Undo
- `Ctrl+Y` - Redo
- `Ctrl+C` - Copy
- `Ctrl+X` - Cut
- `Ctrl+V` - Paste
- `Ctrl+A` - Select All
- `Tab` - Indent
- `Shift+Tab` - Dedent
- `Ctrl+Shift+Up` - Move line up
- `Ctrl+Shift+Down` - Move line down

### Search and Replace
- `Ctrl+F` - Find next
- `Ctrl+Shift+F` - Find previous
- `Ctrl+H` - Replace current match
- `Ctrl+Alt+R` - Replace all matches

### View
- `Ctrl+W` - Toggle word wrap

### Navigation
- Arrow keys for cursor movement
- `Home`/`End` - Beginning/end of line
- `Page Up`/`Page Down` - Scroll by page
- Mouse click to position cursor
- Mouse drag to select text

## Technical Details

Built with:
- **ratatui** - Terminal UI framework
- **crossterm** - Cross-platform terminal manipulation
- **ropey** - Efficient rope data structure for text storage
- **arboard** - System clipboard integration
- **unicode-segmentation** - Proper Unicode text handling

The editor implements a rope-based text buffer for efficient insertion and deletion operations, making it suitable for editing large files. The visual line mapping system ensures smooth word wrapping without performance degradation.

## Development

```bash
# Run in development mode
cargo run

# Run tests (when available)
cargo test

# Check code formatting
cargo fmt -- --check

# Run clippy for lints
cargo clippy
```

## Contributing

Contributions are welcome! Please feel free to submit pull requests or open issues for bugs and feature requests.

## Acknowledgments

This editor stands on the shoulders of giants - namely, the excellent Rust crates that make terminal UI development a joy rather than a journey through the seven circles of ANSI escape sequences.