//! `halil board`: Todo, Doing and Done side by side.
//!
//! The board is drawn whole on every change, as plain rows of styled text: no
//! boxes, columns set apart by space. Every change is saved at once, and made
//! to the tasks file as it is on disk then, so a `halil add` from another
//! terminal while the board is open isn't lost.

use std::cmp::Reverse;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use jiff::Zoned;
use jiff::civil::Date;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::parse::{self, Parsed};
use crate::store;
use crate::task::{self, Priority, Status, Task};

const COLUMNS: [Status; 3] = [Status::Todo, Status::Doing, Status::Done];

/// Spaces left of the first column, and between columns.
const MARGIN: usize = 2;
const GAP: usize = 3;

// A quiet palette from the 256 colors every current terminal has.
type Color = u8;
const HIGHLIGHT: Color = 236;
const ON_HIGHLIGHT: Color = 255;
const QUIET: Color = 244;
const QUIET_ON_HIGHLIGHT: Color = 248;
const QUIETER: Color = 240;
const RED: Color = 203;
const AMBER: Color = 215;
const BLUE: Color = 74;

/// Opens the board on the tasks file at `path` and runs it until `q`.
pub fn run(path: &Path, tasks: Vec<Task>) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("the board needs a terminal".into());
    }
    let color = env::var_os("NO_COLOR").is_none();
    let mut board = Board::new(tasks, Zoned::now().date());
    let mut out = io::stdout();
    let _screen = Screen::enter(&mut out).map_err(term_err)?;
    event_loop(&mut board, path, &mut out, color)
}

/// Raw mode on the alternate screen for as long as it lives, even through a panic.
struct Screen;

impl Screen {
    fn enter(out: &mut impl Write) -> io::Result<Screen> {
        terminal::enable_raw_mode()?;
        let screen = Screen;
        execute!(out, EnterAlternateScreen, Hide)?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn term_err(e: io::Error) -> String {
    format!("terminal: {e}")
}

fn event_loop(
    board: &mut Board,
    path: &Path,
    out: &mut impl Write,
    color: bool,
) -> Result<(), String> {
    let modified = || fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut seen: Option<SystemTime> = modified();
    let mut redraw = true;
    loop {
        if redraw {
            let (w, h) = terminal::size().map_err(term_err)?;
            let (lines, cursor) = board.draw(w as usize, h as usize);
            paint(out, &lines, cursor, color).map_err(term_err)?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(500)).map_err(term_err)? {
            // Idle: past midnight, due dates read anew.
            let today = Zoned::now().date();
            if today != board.today {
                board.today = today;
                redraw = true;
            }
            // And pick up what another `halil` changed meanwhile.
            let now = modified();
            if now != seen {
                seen = now;
                match store::load(path) {
                    Ok(tasks) => board.replace_tasks(tasks),
                    Err(e) => board.message = Some(e),
                }
                redraw = true;
            }
            continue;
        }

        redraw = true;
        let Event::Key(key) = event::read().map_err(term_err)? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match board.key(key) {
            Outcome::Quit => return Ok(()),
            Outcome::Nothing => {}
            Outcome::Change(op) => {
                match store::load(path) {
                    Ok(fresh) => {
                        board.replace_tasks(fresh);
                        board.apply(op);
                        if let Err(e) = store::save(path, &board.tasks) {
                            board.message = Some(e);
                        }
                    }
                    Err(e) => board.message = Some(e),
                }
                seen = modified();
            }
        }
    }
}

/// What a key asks of the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    /// A change to the tasks, to make and save.
    Change(Op),
    Quit,
}

/// A change to the tasks, by id, so it can be made to the file as it is now.
#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    /// Set a task's status; `follow` keeps it picked in its new column.
    Move {
        id: u32,
        to: Status,
        follow: bool,
    },
    Add(Parsed),
}

pub struct Board {
    pub tasks: Vec<Task>,
    /// The column picked, and the card picked in each column.
    col: usize,
    row: [usize; 3],
    /// The first card shown in each column, when it has more than fit.
    scroll: [usize; 3],
    /// The words being typed after `a`.
    input: Option<String>,
    /// An error to show until the next key.
    pub message: Option<String>,
    /// The day due dates are read from.
    today: Date,
}

impl Board {
    pub fn new(tasks: Vec<Task>, today: Date) -> Board {
        Board {
            tasks,
            today,
            col: 0,
            row: [0; 3],
            scroll: [0; 3],
            input: None,
            message: None,
        }
    }

    /// The tasks of column `c`, as indices into `tasks`: highest priority
    /// first, then oldest first.
    fn column(&self, c: usize) -> Vec<usize> {
        let mut ids: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| self.tasks[i].status == COLUMNS[c])
            .collect();
        if COLUMNS[c] == Status::Done {
            ids.sort_by_key(|&i| (Reverse(self.tasks[i].priority), self.tasks[i].id));
        } else {
            // As the list does: soonest due first (so late ones lead), undated
            // after, then highest priority, then oldest.
            ids.sort_by_key(|&i| {
                let t = &self.tasks[i];
                (t.due.is_none(), t.due, Reverse(t.priority), t.id)
            });
        }
        ids
    }

    /// The picked card, as an index into `tasks`.
    fn picked(&self) -> Option<usize> {
        self.column(self.col).get(self.row[self.col]).copied()
    }

    /// Picks task `id`, in whichever column it's in.
    fn pick(&mut self, id: u32) {
        let Some(i) = self.tasks.iter().position(|t| t.id == id) else {
            return;
        };
        let c = COLUMNS
            .iter()
            .position(|&s| s == self.tasks[i].status)
            .unwrap_or(0);
        self.col = c;
        self.row[c] = self.column(c).iter().position(|&j| j == i).unwrap_or(0);
    }

    /// Keeps every column's pick on a card.
    fn clamp(&mut self) {
        for c in 0..COLUMNS.len() {
            self.row[c] = self.row[c].min(self.column(c).len().saturating_sub(1));
        }
    }

    /// Takes the tasks as they are on disk now, keeping the picked card picked
    /// while it's still in its column.
    pub fn replace_tasks(&mut self, tasks: Vec<Task>) {
        let picked = self
            .picked()
            .map(|i| (self.tasks[i].id, self.tasks[i].status));
        self.tasks = tasks;
        if let Some((id, status)) = picked
            && self.tasks.iter().any(|t| t.id == id && t.status == status)
        {
            self.pick(id);
        }
        self.clamp();
    }

    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Move { id, to, follow } => {
                match self.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => task.status = to,
                    None => self.message = Some(format!("task {id} is gone")),
                }
                if follow {
                    self.pick(id);
                }
            }
            Op::Add(parsed) => {
                let id = task::next_id(&self.tasks);
                self.tasks.push(Task {
                    id,
                    title: parsed.title,
                    status: Status::Todo,
                    priority: parsed.priority,
                    tags: parsed.tags,
                    due: parsed.due,
                });
                self.pick(id);
            }
        }
        self.clamp();
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        self.message = None;
        if self.input.is_some() {
            return self.input_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.column(self.col).len().saturating_sub(1);
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Quit,
            KeyCode::Char('q') | KeyCode::Esc => return Outcome::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.row[self.col] = (self.row[self.col] + 1).min(last)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.row[self.col] = self.row[self.col].saturating_sub(1)
            }
            KeyCode::Char('g') | KeyCode::Home => self.row[self.col] = 0,
            KeyCode::Char('G') | KeyCode::End => self.row[self.col] = last,
            KeyCode::Right => self.col = (self.col + 1).min(COLUMNS.len() - 1),
            KeyCode::Left => self.col = self.col.saturating_sub(1),
            KeyCode::Tab => self.col = (self.col + 1) % COLUMNS.len(),
            KeyCode::BackTab => self.col = (self.col + COLUMNS.len() - 1) % COLUMNS.len(),
            KeyCode::Char('h') => return self.shift(-1),
            KeyCode::Char('l') => return self.shift(1),
            KeyCode::Char('d') => {
                if let Some(i) = self.picked()
                    && self.tasks[i].status != Status::Done
                {
                    let id = self.tasks[i].id;
                    return Outcome::Change(Op::Move {
                        id,
                        to: Status::Done,
                        follow: false,
                    });
                }
            }
            KeyCode::Char('a') => self.input = Some(String::new()),
            _ => {}
        }
        Outcome::Nothing
    }

    /// Moves the picked card a column left (-1) or right (1).
    fn shift(&self, by: isize) -> Outcome {
        let to = self
            .col
            .checked_add_signed(by)
            .filter(|&c| c < COLUMNS.len());
        match (self.picked(), to) {
            (Some(i), Some(to)) => Outcome::Change(Op::Move {
                id: self.tasks[i].id,
                to: COLUMNS[to],
                follow: true,
            }),
            _ => Outcome::Nothing,
        }
    }

    fn input_key(&mut self, key: KeyEvent) -> Outcome {
        let Some(text) = self.input.as_mut() else {
            return Outcome::Nothing;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Char('c') if ctrl => self.input = None,
            KeyCode::Char('u') if ctrl => text.clear(),
            KeyCode::Char('w') if ctrl => {
                let kept = text.trim_end().rfind(' ').map_or(0, |i| i + 1);
                text.truncate(kept);
            }
            KeyCode::Char(c) if !ctrl => text.push(c),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Enter => {
                if text.trim().is_empty() {
                    self.input = None;
                    return Outcome::Nothing;
                }
                match parse::parse(std::slice::from_ref(text), Zoned::now().datetime()) {
                    Ok(parsed) => {
                        self.input = None;
                        return Outcome::Change(Op::Add(parsed));
                    }
                    Err(e) => self.message = Some(e),
                }
            }
            _ => {}
        }
        Outcome::Nothing
    }

    /// The screen, `w` by `h`, as one line a row, and where the cursor goes
    /// (while typing a task).
    fn draw(&mut self, w: usize, h: usize) -> (Vec<Line>, Option<(u16, u16)>) {
        let col_w = w.saturating_sub(2 * MARGIN + (COLUMNS.len() - 1) * GAP) / COLUMNS.len();
        if col_w < 12 || h < 7 {
            let mut row = Row::default();
            row.push(
                &truncate("  Make the window larger for the board", w),
                Style::fg(QUIET),
            );
            return (vec![row.spans], None);
        }
        let body_h = h - 5;
        let columns: Vec<Vec<usize>> = (0..COLUMNS.len()).map(|c| self.column(c)).collect();
        for (c, cards) in columns.iter().enumerate() {
            let s = &mut self.scroll[c];
            *s = (*s)
                .min(self.row[c])
                .max((self.row[c] + 1).saturating_sub(body_h));
            *s = (*s).min(cards.len().saturating_sub(body_h));
        }
        let x = |c: usize| MARGIN + c * (col_w + GAP);

        let mut lines = vec![Line::new()];

        let mut heading = Row::default();
        for (c, cards) in columns.iter().enumerate() {
            heading.pad_to(x(c), Style::default());
            let name = if c == self.col {
                Style::default()
            } else {
                Style::fg(QUIET)
            };
            heading.push(&format!(" {}", status_name(COLUMNS[c])), name);
            heading.push(&format!("  {}", cards.len()), Style::fg(QUIETER));
        }
        lines.push(heading.spans);
        lines.push(Line::new());

        for r in 0..body_h {
            let mut row = Row::default();
            for (c, cards) in columns.iter().enumerate() {
                row.pad_to(x(c), Style::default());
                let n = self.scroll[c] + r;
                if let Some(&i) = cards.get(n) {
                    let picked = c == self.col && n == self.row[c];
                    card(&mut row, &self.tasks[i], col_w, picked, self.today);
                } else if r == 0 && cards.is_empty() {
                    row.push(
                        " Nothing here",
                        Style {
                            italic: true,
                            ..Style::fg(QUIETER)
                        },
                    );
                }
            }
            lines.push(row.spans);
        }

        let mut message = Row::default();
        if let Some(m) = &self.message {
            message.push(&truncate(&format!("  {m}"), w), Style::fg(RED));
        }
        lines.push(message.spans);

        let mut footer = Row::default();
        let mut cursor = None;
        match &self.input {
            Some(text) => {
                footer.push("  New task  ", Style::fg(QUIET));
                let hint = "enter to add · esc to cancel";
                let room = w.saturating_sub(footer.width + hint.width() + 4);
                let start = footer.width;
                if text.is_empty() {
                    footer.push(
                        &truncate("title friday +tag !high", room),
                        Style::fg(QUIETER),
                    );
                } else {
                    footer.push(&tail(text, room), Style::default());
                }
                let typed = if text.is_empty() {
                    0
                } else {
                    footer.width - start
                };
                cursor = Some(((start + typed) as u16, (h - 1) as u16));
                if room > 0 {
                    footer.pad_to(w - hint.width() - 2, Style::default());
                    footer.push(hint, Style::fg(QUIETER));
                }
            }
            None => {
                footer.push(" ", Style::default());
                for (keys, what) in [
                    ("j k", "pick"),
                    ("h l", "move"),
                    ("tab", "column"),
                    ("a", "add"),
                    ("d", "done"),
                    ("q", "quit"),
                ] {
                    if footer.width + 3 + keys.len() + what.len() > w {
                        break;
                    }
                    footer.push(&format!(" {keys} "), Style::fg(QUIET));
                    footer.push(&format!("{what} "), Style::fg(QUIETER));
                }
            }
        }
        lines.push(footer.spans);
        (lines, cursor)
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Todo => "Todo",
        Status::Doing => "Doing",
        Status::Done => "Done",
    }
}

fn priority_color(p: Priority) -> Color {
    match p {
        Priority::High => RED,
        Priority::Med => AMBER,
        Priority::Low => BLUE,
    }
}

/// Days from `today` to the task's due date, while it's open: a done task's
/// date no longer matters.
fn due_in_days(task: &Task, today: Date) -> Option<i64> {
    let due = task.due.filter(|_| task.status != Status::Done)?;
    Some(today.until(due.date).ok()?.get_days().into())
}

/// A due date as a person says it: `today`, `tomorrow`, `in 3 days`, `next
/// week`, `2 days ago`.
fn due_in_words(days: i64) -> String {
    match days {
        0 => "today".into(),
        1 => "tomorrow".into(),
        -1 => "yesterday".into(),
        2..=6 => format!("in {days} days"),
        7..=13 => "next week".into(),
        14.. => format!("in {} weeks", days / 7),
        -13..=-2 => format!("{} days ago", -days),
        _ => format!("{} weeks ago", -days / 7),
    }
}

/// One card, `width` wide: its priority mark, title, tags dimmed beside it and
/// its due date at the right.
fn card(row: &mut Row, task: &Task, width: usize, picked: bool, today: Date) {
    let bg = picked.then_some(HIGHLIGHT);
    let plain = Style {
        bg,
        ..Style::default()
    };
    let title = Style {
        fg: picked.then_some(ON_HIGHLIGHT),
        ..plain
    };
    let quiet = Style::fg(if picked { QUIET_ON_HIGHLIGHT } else { QUIET });
    let quiet = Style { bg, ..quiet };

    let start = row.width;
    row.push(" ", plain);
    match task.priority {
        Some(p) => row.push(
            "•",
            Style {
                fg: Some(priority_color(p)),
                ..plain
            },
        ),
        None => row.push(" ", plain),
    }
    row.push(" ", plain);

    // Room inside the card: a space of padding at each end, the mark and its space.
    let inner = width.saturating_sub(4);
    let due = due_in_days(task, today)
        .map(|days| (due_in_words(days), days < 0))
        .filter(|(words, _)| words.width() + 2 + 8 <= inner);
    let room = inner - due.as_ref().map_or(0, |(words, _)| words.width() + 2);

    let shown = truncate(&task.title, room);
    let used = shown.width();
    row.push(&shown, title);
    if !task.tags.is_empty() && room >= used + 2 + 3 {
        let tags: Vec<String> = task.tags.iter().map(|t| format!("+{t}")).collect();
        row.push("  ", plain);
        row.push(&truncate(&tags.join(" "), room - used - 2), quiet);
    }
    row.pad_to(start + 3 + room, plain);
    if let Some((words, overdue)) = due {
        row.push("  ", plain);
        row.push(
            &words,
            if overdue {
                Style {
                    fg: Some(RED),
                    ..plain
                }
            } else {
                quiet
            },
        );
    }
    row.pad_to(start + width, plain);
}

/// `s` cut to `max` columns, an ellipsis marking the cut.
fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.into();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w + 1 > max {
            break;
        }
        out.push(ch);
        width += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// The end of `s` that fits in `max` columns: what's being typed stays in view.
fn tail(s: &str, max: usize) -> String {
    let mut width = 0;
    let mut start = s.len();
    for (i, ch) in s.char_indices().rev() {
        width += ch.width().unwrap_or(0);
        if width > max {
            break;
        }
        start = i;
    }
    s[start..].into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    italic: bool,
}

impl Style {
    fn fg(color: Color) -> Style {
        Style {
            fg: Some(color),
            ..Style::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    text: String,
    style: Style,
}

type Line = Vec<Span>;

/// A line being built, and how many columns wide it is so far.
#[derive(Default)]
struct Row {
    spans: Line,
    width: usize,
}

impl Row {
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.width += text.width();
        self.spans.push(Span {
            text: text.into(),
            style,
        });
    }

    fn pad_to(&mut self, width: usize, style: Style) {
        if width > self.width {
            self.push(&" ".repeat(width - self.width), style);
        }
    }
}

/// The escape that sets a span's style, from a reset. Without `color` the
/// picked card is shown in reverse, since its soft background is a color.
fn sgr(style: Style, color: bool) -> String {
    let mut sgr = String::from("\x1b[0");
    if let (Some(fg), true) = (style.fg, color) {
        sgr += &format!(";38;5;{fg}");
    }
    match (style.bg, color) {
        (Some(bg), true) => sgr += &format!(";48;5;{bg}"),
        (Some(_), false) => sgr += ";7",
        (None, _) => {}
    }
    if style.italic {
        sgr += ";3";
    }
    sgr + "m"
}

/// Draws the lines over the screen, each row cleared past its end.
fn paint(
    out: &mut impl Write,
    lines: &[Line],
    cursor: Option<(u16, u16)>,
    color: bool,
) -> io::Result<()> {
    queue!(out, BeginSynchronizedUpdate, Hide)?;
    for (y, line) in lines.iter().enumerate() {
        queue!(out, MoveTo(0, y as u16))?;
        for span in line {
            queue!(out, Print(sgr(span.style, color)), Print(&span.text))?;
        }
        queue!(
            out,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::UntilNewLine)
        )?;
    }
    if lines.len() < terminal::size().map_or(0, |(_, h)| h as usize) {
        queue!(
            out,
            MoveTo(0, lines.len() as u16),
            Clear(ClearType::FromCursorDown)
        )?;
    }
    if let Some((x, y)) = cursor {
        queue!(out, MoveTo(x, y), Show)?;
    }
    queue!(out, EndSynchronizedUpdate)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(
        id: u32,
        title: &str,
        status: Status,
        priority: Option<Priority>,
        tags: &[&str],
    ) -> Task {
        Task {
            id,
            title: title.into(),
            status,
            priority,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            due: None,
        }
    }

    /// The day the tests' boards read due dates from: Wednesday 2026-11-11.
    fn today() -> Date {
        jiff::civil::date(2026, 11, 11)
    }

    fn due(task: Task, due: &str) -> Task {
        Task {
            due: Some(due.parse().unwrap()),
            ..task
        }
    }

    fn sample() -> Board {
        Board::new(
            vec![
                task(1, "buy milk", Status::Todo, None, &[]),
                task(
                    2,
                    "pay rent",
                    Status::Todo,
                    Some(Priority::High),
                    &["home", "money"],
                ),
                task(3, "fix bike", Status::Doing, Some(Priority::Low), &[]),
                task(4, "call mum", Status::Done, None, &[]),
            ],
            today(),
        )
    }

    fn press(board: &mut Board, keys: &str) -> Vec<Outcome> {
        keys.chars()
            .map(|c| board.key(KeyEvent::from(KeyCode::Char(c))))
            .collect()
    }

    /// Presses the keys, making each change as the loop does.
    fn run_keys(board: &mut Board, keys: &str) {
        for c in keys.chars() {
            if let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Char(c))) {
                board.apply(op);
            }
        }
    }

    fn picked_id(board: &Board) -> Option<u32> {
        board.picked().map(|i| board.tasks[i].id)
    }

    fn text(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn columns_list_highest_priority_first() {
        let board = sample();
        let ids = |c| {
            board
                .column(c)
                .iter()
                .map(|&i| board.tasks[i].id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(0), [2, 1]);
        assert_eq!(ids(1), [3]);
        assert_eq!(ids(2), [4]);
    }

    #[test]
    fn j_and_k_pick_within_the_column_and_stop_at_its_ends() {
        let mut board = sample();
        assert_eq!(picked_id(&board), Some(2));
        press(&mut board, "j");
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, "jj");
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, "kkk");
        assert_eq!(picked_id(&board), Some(2));
    }

    #[test]
    fn h_and_l_move_the_card_and_it_stays_picked() {
        let mut board = sample();
        run_keys(&mut board, "jl");
        assert_eq!(board.tasks[0].status, Status::Doing);
        assert_eq!(picked_id(&board), Some(1));
        run_keys(&mut board, "l");
        assert_eq!(board.tasks[0].status, Status::Done);
        assert_eq!(picked_id(&board), Some(1));
        assert_eq!(press(&mut board, "l"), [Outcome::Nothing]);
        run_keys(&mut board, "hh");
        assert_eq!(board.tasks[0].status, Status::Todo);
        assert_eq!(press(&mut board, "h"), [Outcome::Nothing]);
    }

    #[test]
    fn d_marks_the_card_done_and_picks_the_next_in_the_column() {
        let mut board = sample();
        run_keys(&mut board, "d");
        assert_eq!(board.tasks[1].status, Status::Done);
        assert_eq!(picked_id(&board), Some(1));
        run_keys(&mut board, "d");
        assert_eq!(picked_id(&board), None);
        assert_eq!(press(&mut board, "d"), [Outcome::Nothing]);
    }

    #[test]
    fn tab_and_arrows_go_between_columns() {
        let mut board = sample();
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(3));
        board.key(KeyEvent::from(KeyCode::Right));
        board.key(KeyEvent::from(KeyCode::Right));
        assert_eq!(picked_id(&board), Some(4));
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(2));
    }

    #[test]
    fn a_adds_with_the_grammar_of_add_and_picks_the_new_card() {
        let mut board = sample();
        board.key(KeyEvent::from(KeyCode::Tab));
        run_keys(&mut board, "awater plants +home !med");
        assert_eq!(board.tasks.len(), 4, "nothing added before enter");
        let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Enter)) else {
            panic!("enter adds");
        };
        board.apply(op);
        let added = board.tasks.last().unwrap();
        assert_eq!(added.id, 5);
        assert_eq!(added.title, "water plants");
        assert_eq!(added.tags, ["home"]);
        assert_eq!(added.priority, Some(Priority::Med));
        assert_eq!(added.status, Status::Todo);
        assert_eq!(picked_id(&board), Some(5));
        assert_eq!(board.input, None);
    }

    #[test]
    fn a_bad_task_says_why_and_keeps_the_words() {
        let mut board = sample();
        run_keys(&mut board, "ax !urgent");
        assert_eq!(board.key(KeyEvent::from(KeyCode::Enter)), Outcome::Nothing);
        assert!(board.message.as_deref().unwrap().contains("!urgent"));
        assert_eq!(board.input.as_deref(), Some("x !urgent"));
        board.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(board.input, None);
        assert_eq!(board.tasks.len(), 4);
    }

    #[test]
    fn q_quits_but_is_a_letter_while_typing() {
        let mut board = sample();
        assert_eq!(
            press(&mut board, "aq"),
            [Outcome::Nothing, Outcome::Nothing]
        );
        assert_eq!(board.input.as_deref(), Some("q"));
        board.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(press(&mut board, "q"), [Outcome::Quit]);
    }

    #[test]
    fn changes_are_made_to_the_tasks_as_they_are_on_disk() {
        let mut board = sample();
        let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Char('l'))) else {
            panic!("l moves");
        };
        // Meanwhile another `halil add` wrote task 5.
        let mut fresh = sample().tasks;
        fresh.push(task(5, "new elsewhere", Status::Todo, None, &[]));
        board.replace_tasks(fresh);
        board.apply(op);
        assert_eq!(board.tasks.len(), 5);
        assert_eq!(board.tasks[1].status, Status::Doing);
        assert_eq!(picked_id(&board), Some(2));
    }

    #[test]
    fn draws_quiet_headings_with_counts_and_one_row_a_card() {
        let mut board = sample();
        let (lines, cursor) = board.draw(100, 12);
        assert_eq!(lines.len(), 12);
        assert_eq!(cursor, None);
        assert_eq!(text(&lines[0]), "");
        let heading = text(&lines[1]);
        assert!(heading.starts_with("   Todo  2"), "{heading:?}");
        assert!(
            heading.contains(" Doing  1") && heading.contains(" Done  1"),
            "{heading:?}"
        );
        assert!(!heading.contains(['│', '─', '┌', '|']));
        assert_eq!(text(&lines[2]), "");
        let first = text(&lines[3]);
        let second = text(&lines[4]);
        assert!(
            first.starts_with("   • pay rent  +home +money"),
            "{first:?}"
        );
        assert!(
            first.contains("• fix bike") && first.contains("call mum"),
            "{first:?}"
        );
        assert!(second.starts_with("     buy milk"), "{second:?}");
        assert!(text(&lines[5]).trim().is_empty());
    }

    #[test]
    fn the_picked_card_is_highlighted_and_its_mark_colored() {
        let mut board = sample();
        let (lines, _) = board.draw(80, 12);
        let highlighted: Vec<&Span> = lines[3]
            .iter()
            .filter(|s| s.style.bg == Some(HIGHLIGHT))
            .collect();
        let width: usize = highlighted.iter().map(|s| s.text.width()).sum();
        assert_eq!(
            width,
            (80 - 2 * MARGIN - 2 * GAP) / 3,
            "the whole card, no more"
        );
        let mark = lines[3].iter().find(|s| s.text == "•").unwrap();
        assert_eq!(mark.style.fg, Some(RED));
        assert!(lines[4].iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn long_titles_are_cut_and_tags_dropped_to_fit() {
        let mut board = Board::new(
            vec![task(
                1,
                "write the quarterly report for the whole team",
                Status::Todo,
                None,
                &["work"],
            )],
            today(),
        );
        let (lines, _) = board.draw(60, 10);
        let row = text(&lines[3]);
        let col_w = (60 - 2 * MARGIN - 2 * GAP) / 3;
        let card: String = row.chars().skip(MARGIN).take(col_w).collect();
        assert_eq!(card.trim_end(), "   write the q…");
        assert!(!row.contains("+work"));
        assert!(lines.iter().all(|l| text(l).width() <= 60));
    }

    #[test]
    fn an_empty_column_says_so() {
        let mut board = Board::new(vec![], today());
        let (lines, _) = board.draw(80, 10);
        assert_eq!(text(&lines[3]).matches("Nothing here").count(), 3);
    }

    #[test]
    fn a_long_column_scrolls_to_keep_the_pick_in_view() {
        let tasks = (1..=20)
            .map(|id| task(id, &format!("task {id}"), Status::Todo, None, &[]))
            .collect();
        let mut board = Board::new(tasks, today());
        press(&mut board, "G");
        let (lines, _) = board.draw(80, 10);
        let body: Vec<String> = lines[3..8].iter().map(text).collect();
        assert!(body[4].contains("task 20"), "{body:?}");
        assert!(body[0].contains("task 16"), "{body:?}");
    }

    #[test]
    fn typing_a_task_shows_the_words_and_the_cursor_after_them() {
        let mut board = sample();
        press(&mut board, "abuy eggs");
        let (lines, cursor) = board.draw(80, 12);
        let footer = text(&lines[11]);
        assert!(footer.starts_with("  New task  buy eggs"), "{footer:?}");
        assert_eq!(cursor, Some((20, 11)));
    }

    #[test]
    fn paints_rows_in_place_with_a_soft_background_and_no_box_lines() {
        let mut board = sample();
        let (lines, cursor) = board.draw(100, 12);
        let mut out = Vec::new();
        paint(&mut out, &lines, cursor, true).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("\x1b[4;1H"), "rows are placed, not scrolled");
        assert!(
            out.contains("\x1b[0;48;5;236m "),
            "the picked card's background"
        );
        assert!(out.contains("\x1b[0;38;5;203;48;5;236m•"), "its mark");
        assert!(
            out.contains("\x1b[0;38;5;255;48;5;236mpay rent"),
            "its title"
        );
        assert!(out.contains("\x1b[0;38;5;248;48;5;236m+home"), "its tags");
        assert!(out.contains("\x1b[K"), "each row cleared past its end");
        assert!(!out.contains(['│', '─', '┌', '└']));
    }

    #[test]
    fn without_color_the_picked_card_is_reversed() {
        let mut board = sample();
        let (lines, cursor) = board.draw(100, 12);
        let mut out = Vec::new();
        paint(&mut out, &lines, cursor, false).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("38;5") && !out.contains("48;5"), "no colors");
        assert!(out.contains("\x1b[0;7mpay rent"));
    }

    #[test]
    fn open_cards_show_their_due_date_in_words_at_the_right() {
        let mut board = Board::new(
            vec![
                due(
                    task(1, "pay rent", Status::Todo, None, &["home"]),
                    "2026-11-12",
                ),
                due(task(2, "tax form", Status::Doing, None, &[]), "2026-11-09"),
                due(task(3, "old thing", Status::Done, None, &[]), "2026-11-01"),
            ],
            today(),
        );
        let (lines, _) = board.draw(100, 10);
        let row = text(&lines[3]);
        let col_w = (100 - 2 * MARGIN - 2 * GAP) / 3;
        let todo: String = row.chars().skip(MARGIN).take(col_w).collect();
        assert!(todo.starts_with("   pay rent  +home"), "{todo:?}");
        assert!(todo.ends_with("tomorrow "), "{todo:?}");
        assert!(row.contains("2 days ago"), "{row:?}");
        let overdue = lines[3].iter().find(|s| s.text == "2 days ago").unwrap();
        assert_eq!(overdue.style.fg, Some(RED));
        assert!(row.contains("old thing"), "{row:?}");
        assert!(!row.contains("10 days ago"), "a done card shows no date");
    }

    #[test]
    fn a_reads_the_due_date_too() {
        let mut board = sample();
        run_keys(&mut board, "awater plants 2030-01-02 +home");
        let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Enter)) else {
            panic!("enter adds");
        };
        board.apply(op);
        let added = board.tasks.last().unwrap();
        assert_eq!(added.title, "water plants");
        assert_eq!(added.due, Some("2030-01-02".parse().unwrap()));
    }

    #[test]
    fn due_dates_read_as_words() {
        assert_eq!(due_in_words(0), "today");
        assert_eq!(due_in_words(1), "tomorrow");
        assert_eq!(due_in_words(-1), "yesterday");
        assert_eq!(due_in_words(4), "in 4 days");
        assert_eq!(due_in_words(9), "next week");
        assert_eq!(due_in_words(21), "in 3 weeks");
        assert_eq!(due_in_words(-3), "3 days ago");
        assert_eq!(due_in_words(-15), "2 weeks ago");
    }

    #[test]
    fn truncate_and_tail_count_columns_not_bytes() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(tail("hello world", 5), "world");
        assert_eq!(tail("日本語", 4), "本語");
    }
}
