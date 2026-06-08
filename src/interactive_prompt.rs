//! Interactive terminal stream selection for the CLI.

use std::collections::HashSet;
use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use crossterm::queue;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use crate::config::DownloadOptions;
use crate::error::{Error, Result};
use crate::manifest::{MediaType, Stream};
use crate::stream_label::stream_full_label;

const PAGE_SIZE: usize = 10;
const STREAM_VIDEO_COLOR: Color = Color::AnsiValue(14);
const STREAM_AUDIO_COLOR: Color = Color::AnsiValue(32);
const STREAM_SUBTITLE_COLOR: Color = Color::AnsiValue(45);
const GROUP_COLOR: Color = Color::Rgb {
    r: 135,
    g: 255,
    b: 255,
};
const ACTIVE_ROW_COLOR: Color = Color::Blue;
const SELECTED_MARK_COLOR: Color = Color::Blue;
const ENCRYPTION_METHOD_COLOR: Color = Color::AnsiValue(9);
const CHILD_INDENT: &str = "  ";

#[derive(Clone, Debug)]
enum PromptEntry {
    Group { title: &'static str },
    Stream { stream_index: usize, label: String },
}

#[derive(Debug)]
struct PromptState {
    entries: Vec<PromptEntry>,
    selected: HashSet<usize>,
    cursor: usize,
    offset: usize,
}

#[derive(Debug)]
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut output = io::stdout();
        let _ = queue!(output, Show, ResetColor);
        let _ = output.flush();
    }
}

pub(crate) fn select_streams(
    streams: &[Stream],
    default_indexes: &[usize],
    options: &DownloadOptions,
) -> Result<Vec<usize>> {
    let entries = prompt_entries(streams);
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut state = PromptState {
        entries,
        selected: default_indexes
            .iter()
            .copied()
            .filter(|index| *index < streams.len())
            .collect(),
        cursor: 0,
        offset: 0,
    };
    if state.selected.is_empty() && !streams.is_empty() {
        state.selected.insert(0);
    }

    let _raw = RawModeGuard::enter()?;
    let mut output = io::stdout();
    queue!(output, Hide)?;
    let ansi = !options.no_ansi_color;
    let mut previous_lines = 0_usize;
    loop {
        draw_prompt(&mut output, &state, previous_lines, ansi)?;
        previous_lines = prompt_line_count(&state);
        match read()? {
            Event::Key(event) if event.kind == KeyEventKind::Press => match event.code {
                KeyCode::Up => state.move_up(),
                KeyCode::Down => state.move_down(),
                KeyCode::Home => state.move_first(),
                KeyCode::End => state.move_last(),
                KeyCode::PageUp => state.move_page_up(),
                KeyCode::PageDown => state.move_page_down(),
                KeyCode::Char(' ') => state.toggle_current(),
                KeyCode::Enter => {
                    if !state.selected.is_empty() {
                        draw_prompt(&mut output, &state, previous_lines, ansi)?;
                        writeln!(output)?;
                        output.flush()?;
                        return Ok(selected_indexes(&state));
                    }
                }
                KeyCode::Esc => return Err(Error::config("interactive selection cancelled")),
                KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(Error::config("interactive selection cancelled"));
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl PromptState {
    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.keep_cursor_visible();
    }

    fn move_down(&mut self) {
        if self.cursor + 1 < self.entries.len() {
            self.cursor += 1;
        }
        self.keep_cursor_visible();
    }

    fn move_first(&mut self) {
        self.cursor = 0;
        self.keep_cursor_visible();
    }

    fn move_last(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
        self.keep_cursor_visible();
    }

    fn move_page_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(PAGE_SIZE);
        self.keep_cursor_visible();
    }

    fn move_page_down(&mut self) {
        self.cursor = (self.cursor + PAGE_SIZE).min(self.entries.len().saturating_sub(1));
        self.keep_cursor_visible();
    }

    fn toggle_current(&mut self) {
        if let Some(PromptEntry::Stream { stream_index, .. }) = self.entries.get(self.cursor) {
            toggle_index(&mut self.selected, *stream_index);
        }
    }

    fn keep_cursor_visible(&mut self) {
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + PAGE_SIZE {
            self.offset = self.cursor.saturating_sub(PAGE_SIZE - 1);
        }
    }
}

fn toggle_index(selected: &mut HashSet<usize>, index: usize) {
    if !selected.remove(&index) {
        selected.insert(index);
    }
}

fn prompt_entries(streams: &[Stream]) -> Vec<PromptEntry> {
    let groups = [
        ("Basic", MediaGroup::Basic),
        ("Audio", MediaGroup::Audio),
        ("Subtitle", MediaGroup::Subtitle),
    ];
    let mut entries = Vec::new();
    for (title, group) in groups {
        let indexes = streams
            .iter()
            .enumerate()
            .filter_map(|(index, stream)| group.matches(stream).then_some(index))
            .collect::<Vec<_>>();
        if indexes.is_empty() {
            continue;
        }
        entries.push(PromptEntry::Group { title });
        entries.extend(indexes.into_iter().map(|stream_index| PromptEntry::Stream {
            stream_index,
            label: stream_full_label(&streams[stream_index]),
        }));
    }
    entries
}

#[derive(Clone, Copy, Debug)]
enum MediaGroup {
    Basic,
    Audio,
    Subtitle,
}

impl MediaGroup {
    fn matches(self, stream: &Stream) -> bool {
        match self {
            Self::Basic => {
                stream.media_type.is_none() || stream.media_type == Some(MediaType::Video)
            }
            Self::Audio => stream.media_type == Some(MediaType::Audio),
            Self::Subtitle => {
                stream.media_type == Some(MediaType::Subtitles)
                    || stream.media_type == Some(MediaType::ClosedCaptions)
            }
        }
    }
}

fn selected_indexes(state: &PromptState) -> Vec<usize> {
    let mut output = state.selected.iter().copied().collect::<Vec<_>>();
    output.sort_unstable();
    output
}

fn draw_prompt<W: Write>(
    output: &mut W,
    state: &PromptState,
    previous_lines: usize,
    ansi: bool,
) -> io::Result<()> {
    if previous_lines > 0 {
        queue!(
            output,
            MoveUp(lines_to_u16(previous_lines)),
            MoveToColumn(0)
        )?;
    }
    clear_current_line(output)?;
    write!(output, "Please select ")?;
    write_colored(output, ansi, Color::Green, "what you want to download")?;
    writeln!(output, ":")?;

    let end = (state.offset + PAGE_SIZE).min(state.entries.len());
    for (absolute_index, entry) in state.entries[state.offset..end].iter().enumerate() {
        let entry_index = state.offset + absolute_index;
        clear_current_line(output)?;
        draw_entry(
            output,
            entry,
            &state.selected,
            ansi,
            entry_index == state.cursor,
        )?;
        writeln!(output)?;
    }
    clear_current_line(output)?;
    writeln!(output)?;
    if state.entries.len() > PAGE_SIZE {
        clear_current_line(output)?;
        write_colored(
            output,
            ansi,
            Color::DarkGrey,
            "(Move up and down to reveal more streams)",
        )?;
        writeln!(output)?;
    }
    clear_current_line(output)?;
    write!(output, "(Press ")?;
    write_colored(output, ansi, Color::Blue, "<space>")?;
    write!(output, " to toggle a stream, ")?;
    write_colored(output, ansi, Color::Green, "<enter>")?;
    writeln!(output, " to accept)")?;
    output.flush()
}

fn draw_entry<W: Write>(
    output: &mut W,
    entry: &PromptEntry,
    selected: &HashSet<usize>,
    ansi: bool,
    active: bool,
) -> io::Result<()> {
    match entry {
        PromptEntry::Group { title } => {
            write_cursor(output, ansi, active)?;
            write_checkbox(output, ansi, false)?;
            let color = if active {
                ACTIVE_ROW_COLOR
            } else {
                GROUP_COLOR
            };
            write_colored(output, ansi, color, title)
        }
        PromptEntry::Stream {
            stream_index,
            label,
        } => {
            write!(output, "{CHILD_INDENT}")?;
            write_cursor(output, ansi, active)?;
            write_checkbox(output, ansi, selected.contains(stream_index))?;
            write_stream_label(output, label, ansi, active.then_some(ACTIVE_ROW_COLOR))
        }
    }
}

fn clear_current_line<W: Write>(output: &mut W) -> io::Result<()> {
    queue!(output, Clear(ClearType::CurrentLine), MoveToColumn(0))
}

fn write_checkbox<W: Write>(output: &mut W, ansi: bool, selected: bool) -> io::Result<()> {
    write!(output, "[")?;
    if selected {
        write_colored(output, ansi, SELECTED_MARK_COLOR, "X")?;
    } else {
        write!(output, " ")?;
    }
    write!(output, "] ")
}

fn write_cursor<W: Write>(output: &mut W, ansi: bool, active: bool) -> io::Result<()> {
    if active {
        write_colored(output, ansi, ACTIVE_ROW_COLOR, ">")?;
        write!(output, " ")
    } else {
        write!(output, "  ")
    }
}

fn write_stream_label<W: Write>(
    output: &mut W,
    label: &str,
    ansi: bool,
    base_color: Option<Color>,
) -> io::Result<()> {
    for (prefix, color) in [
        ("Vid", STREAM_VIDEO_COLOR),
        ("Aud", STREAM_AUDIO_COLOR),
        ("Sub", STREAM_SUBTITLE_COLOR),
    ] {
        if label == prefix {
            return write_colored(output, ansi, color, prefix);
        }
        if let Some(rest) = label.strip_prefix(prefix)
            && rest.starts_with(' ')
        {
            write_colored(output, ansi, color, prefix)?;
            return write_stream_label_tail(output, rest, ansi, base_color);
        }
    }
    write_stream_label_tail(output, label, ansi, base_color)
}

fn write_stream_label_tail<W: Write>(
    output: &mut W,
    value: &str,
    ansi: bool,
    base_color: Option<Color>,
) -> io::Result<()> {
    let mut rest = value;
    while let Some(index) = rest.find('*') {
        write_base_colored(output, ansi, base_color, &rest[..index])?;
        rest = &rest[index..];
        let token_len = encryption_token_len(rest);
        if token_len == 0 {
            write_base_colored(output, ansi, base_color, "*")?;
            rest = &rest[1..];
            continue;
        }
        write_colored(output, ansi, ENCRYPTION_METHOD_COLOR, &rest[..token_len])?;
        rest = &rest[token_len..];
    }
    write_base_colored(output, ansi, base_color, rest)
}

fn write_base_colored<W: Write>(
    output: &mut W,
    ansi: bool,
    color: Option<Color>,
    text: &str,
) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    if let Some(color) = color {
        write_colored(output, ansi, color, text)
    } else {
        write!(output, "{text}")
    }
}

fn encryption_token_len(value: &str) -> usize {
    let mut chars = value.char_indices();
    match chars.next() {
        Some((_, '*')) => {}
        _ => return 0,
    }
    let mut end = 1;
    let mut has_method_char = false;
    for (index, ch) in chars {
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, '_' | ',' | '-') {
            has_method_char = true;
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if has_method_char { end } else { 0 }
}

fn write_colored<W: Write>(output: &mut W, ansi: bool, color: Color, text: &str) -> io::Result<()> {
    if ansi {
        queue!(output, SetForegroundColor(color))?;
        write!(output, "{text}")?;
        queue!(output, ResetColor)
    } else {
        write!(output, "{text}")
    }
}

fn prompt_line_count(state: &PromptState) -> usize {
    let visible = PAGE_SIZE.min(state.entries.len());
    let more = usize::from(state.entries.len() > PAGE_SIZE);
    1 + visible + 1 + more + 1
}

fn lines_to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
