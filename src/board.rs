//! `halil board`: Todo, Doing and Done side by side.
//!
//! The board is drawn whole on every change, as plain rows of styled text: no
//! boxes, columns set apart by space. Every change is saved at once, and made
//! to the tasks file as it is on disk then, so a `halil add` from another
//! terminal while the board is open isn't lost.

use std::cmp::Reverse;
use std::collections::BTreeSet;
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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::parse::{self, Change, Parsed};
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
    /// Set a task's status; `follow` keeps it picked in its new column. A card
    /// set to Done finishes its open sub items.
    Move { id: u32, to: Status, follow: bool },
    /// Move each of these tasks a column left (-1) or right (1) of where it
    /// is; one already at the edge stays. A card moved into Done finishes its
    /// open sub items; one moved out of it reopens none.
    Shift { ids: Vec<u32>, by: isize },
    /// Mark each of these tasks done, and the open sub items of those that
    /// are cards.
    Finish { ids: Vec<u32> },
    /// Add a task, as a sub item of `parent` when there's one.
    Add { task: Parsed, parent: Option<u32> },
    /// Change each of these tasks as `halil edit` would.
    Edit { ids: Vec<u32>, change: Change },
}

pub struct Board {
    pub tasks: Vec<Task>,
    /// The column picked, and the card picked in each column.
    col: usize,
    row: [usize; 3],
    /// The first card shown in each column, when it has more than fit.
    scroll: [usize; 3],
    /// The ids of the cards and sub items marked with space, which h, l and d
    /// act on together.
    marked: BTreeSet<u32>,
    /// The words being typed after `a`, `A` or `e`.
    input: Option<Input>,
    /// An error or a refusal to show until the next key, in red.
    pub message: Option<String>,
    /// A note of what a change did besides what was asked, shown until the
    /// next key in the default foreground, after `message` if there's one.
    pub note: Option<String>,
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
            marked: BTreeSet::new(),
            input: None,
            message: None,
            note: None,
        }
    }

    /// The rows of column `c`, as indices into `tasks`: its cards, each with
    /// its sub items under it in id order, whatever their own status — but in
    /// Done, where they're folded into their card's count.
    fn column(&self, c: usize) -> Vec<usize> {
        let parents = task::parents(&self.tasks);
        let cards = self.cards(c, &parents);
        if COLUMNS[c] == Status::Done {
            return cards;
        }
        cards
            .into_iter()
            .flat_map(|i| std::iter::once(i).chain(task::sub_items(&self.tasks, &parents, i)))
            .collect()
    }

    /// The cards of column `c`, as indices into `tasks`: in Done highest
    /// priority first, then oldest; elsewhere soonest due first.
    fn cards(&self, c: usize, parents: &[Option<usize>]) -> Vec<usize> {
        let mut ids: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| parents[i].is_none() && self.tasks[i].status == COLUMNS[c])
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

    /// The picked card or sub item, as an index into `tasks`.
    fn picked(&self) -> Option<usize> {
        self.column(self.col).get(self.row[self.col]).copied()
    }

    fn index(&self, id: u32) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    /// Picks task `id`, in whichever column it's in: a sub item in its card's
    /// column, or its card when it's folded there.
    fn pick(&mut self, id: u32) {
        let Some(i) = self.index(id) else {
            return;
        };
        let card = task::parents(&self.tasks)[i].unwrap_or(i);
        let c = COLUMNS
            .iter()
            .position(|&s| s == self.tasks[card].status)
            .unwrap_or(0);
        self.col = c;
        let rows = self.column(c);
        self.row[c] = rows
            .iter()
            .position(|&j| j == i)
            .or_else(|| rows.iter().position(|&j| j == card))
            .unwrap_or(0);
    }

    /// Picks task `id` again, if it's still in column `c`.
    fn repick(&mut self, id: u32, c: usize) {
        if self.index(id).is_some_and(|i| self.column(c).contains(&i)) {
            self.pick(id);
        }
    }

    /// Keeps every column's pick on a card.
    fn clamp(&mut self) {
        for c in 0..COLUMNS.len() {
            self.row[c] = self.row[c].min(self.column(c).len().saturating_sub(1));
        }
    }

    /// Takes the tasks as they are on disk now, keeping the picked card picked
    /// while it's still in its column, and the marks on the rows still in sight.
    pub fn replace_tasks(&mut self, tasks: Vec<Task>) {
        let picked = self.picked().map(|i| self.tasks[i].id);
        self.tasks = tasks;
        self.keep_marks_in_sight();
        if let Some(id) = picked {
            self.repick(id, self.col);
        }
        self.clamp();
    }

    /// Drops the marks on tasks that are gone, and on sub items whose card is
    /// done: folded into its count, out of sight, they're no rows for a key to
    /// act on.
    fn keep_marks_in_sight(&mut self) {
        let parents = task::parents(&self.tasks);
        let in_sight: BTreeSet<u32> = (0..self.tasks.len())
            .filter(|&i| parents[i].is_none_or(|p| self.tasks[p].status != Status::Done))
            .map(|i| self.tasks[i].id)
            .collect();
        self.marked.retain(|id| in_sight.contains(id));
    }

    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Move { id, to, follow } => {
                match self.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => task.status = to,
                    None => self.message = Some(format!("task {id} is gone")),
                }
                if to == Status::Done {
                    self.finish_sub_items(&[id]);
                }
                if follow {
                    self.pick(id);
                }
            }
            Op::Shift { ids, by } => {
                let picked = self.picked().map(|i| self.tasks[i].id);
                // A sub item moves with its card, never by itself.
                let parents = task::parents(&self.tasks);
                let mut into_done = Vec::new();
                for (task, parent) in self.tasks.iter_mut().zip(parents) {
                    if parent.is_none()
                        && ids.contains(&task.id)
                        && let Some(to) = shifted(task.status, by)
                    {
                        task.status = to;
                        if to == Status::Done {
                            into_done.push(task.id);
                        }
                    }
                }
                self.finish_sub_items(&into_done);
                if let Some(id) = picked {
                    self.pick(id);
                }
            }
            Op::Finish { ids } => {
                let picked = self.picked().map(|i| self.tasks[i].id);
                for task in self.tasks.iter_mut().filter(|t| ids.contains(&t.id)) {
                    task.status = Status::Done;
                }
                self.finish_sub_items(&ids);
                if let Some(id) = picked {
                    self.repick(id, self.col);
                }
            }
            Op::Edit { ids, change } => {
                // The picked card stays picked, wherever it sorts now.
                let picked = self.picked().map(|i| self.tasks[i].id);
                for task in self.tasks.iter_mut().filter(|t| ids.contains(&t.id)) {
                    change.apply(task);
                }
                if let Some(id) = picked {
                    self.pick(id);
                }
            }
            Op::Add {
                task: parsed,
                parent,
            } => {
                // The card it was typed under may have gone meanwhile (a `halil
                // rm` elsewhere): it's a task of its own then, never one under
                // a gone id, which `next_id` could hand to itself. The add
                // happens, another way: a note, not a refusal.
                let parent = match parent {
                    Some(p) if self.index(p).is_none() => {
                        self.note = Some(format!("{p} is gone: added as a task of its own."));
                        None
                    }
                    parent => parent,
                };
                let id = task::next_id(&self.tasks);
                self.tasks.push(Task {
                    id,
                    title: parsed.title,
                    status: Status::Todo,
                    priority: parsed.priority,
                    tags: parsed.tags,
                    due: parsed.due,
                    parent,
                });
                self.pick(id);
            }
        }
        // A card gone to Done (d, l, a marked l) folds its sub items away.
        self.keep_marks_in_sight();
        self.clamp();
    }

    /// The open tasks `Op::Finish` of `ids` marks done, as indices into
    /// `tasks`: those tasks, and the sub items of those that are cards.
    fn finishing(&self, ids: &[u32]) -> Vec<usize> {
        let parents = task::parents(&self.tasks);
        let named = |i: usize| ids.contains(&self.tasks[i].id);
        (0..self.tasks.len())
            .filter(|&i| self.tasks[i].status != Status::Done)
            .filter(|&i| named(i) || parents[i].is_some_and(named))
            .collect()
    }

    /// Marks done the open sub items of those of `ids` that are cards, since
    /// their card is now done, and says how many in a note.
    fn finish_sub_items(&mut self, ids: &[u32]) {
        let parents = task::parents(&self.tasks);
        let mut cards = BTreeSet::new();
        let mut n = 0;
        for (i, parent) in parents.into_iter().enumerate() {
            if let Some(p) = parent
                && ids.contains(&self.tasks[p].id)
                && self.tasks[i].status != Status::Done
            {
                self.tasks[i].status = Status::Done;
                cards.insert(p);
                n += 1;
            }
        }
        if n > 0 {
            self.note = Some(task::done_with(n, cards.len() > 1));
        }
    }

    /// Whether `tasks[i]` is a sub item, shown under its card.
    fn is_sub(&self, i: usize) -> bool {
        task::parents(&self.tasks)[i].is_some()
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        self.message = None;
        self.note = None;
        if self.input.is_some() {
            return self.input_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.column(self.col).len().saturating_sub(1);
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Quit,
            // No other chord is its letter: Ctrl-L, a redraw elsewhere, would
            // move a card, Ctrl-D finish one.
            KeyCode::Char(_) if ctrl => {}
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Esc => {
                if self.marked.is_empty() {
                    return Outcome::Quit;
                }
                self.marked.clear();
            }
            KeyCode::Char(' ') => {
                if let Some(i) = self.picked() {
                    let id = self.tasks[i].id;
                    if !self.marked.remove(&id) {
                        self.marked.insert(id);
                    }
                    self.row[self.col] = (self.row[self.col] + 1).min(last);
                }
            }
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
            KeyCode::Char('d') => return self.finish(),
            KeyCode::Char('a') => self.input = Some(Input::new(InputKind::Add, String::new())),
            KeyCode::Char('A') => {
                if let Some(i) = self.picked() {
                    let card = &self.tasks[task::parents(&self.tasks)[i].unwrap_or(i)];
                    if task::is_sub_item(&self.tasks, card) {
                        self.message =
                            Some(format!("{} is a sub item: they go one level deep", card.id));
                    } else {
                        let kind = InputKind::Sub(card.id);
                        self.input = Some(Input::new(kind, String::new()));
                    }
                }
            }
            KeyCode::Char('e') => {
                if !self.marked.is_empty() {
                    self.input = Some(Input::new(InputKind::EditMarked, String::new()));
                } else if let Some(i) = self.picked() {
                    let task = &self.tasks[i];
                    let kind = InputKind::Edit(task.id);
                    self.input = Some(Input::new(kind, task.title.clone()));
                }
            }
            _ => {}
        }
        Outcome::Nothing
    }

    /// Moves the marked cards, or else the picked one, a column left (-1) or
    /// right (1). A sub item moves with its card only: marked, it stays, and
    /// the message says how many did.
    fn shift(&mut self, by: isize) -> Outcome {
        const WITH_ITS_TASK: &str = "a sub item moves with its task";
        let parents = task::parents(&self.tasks);
        if !self.marked.is_empty() {
            let marked = |i: usize| self.marked.contains(&self.tasks[i].id);
            let cards: Vec<usize> = (0..self.tasks.len())
                .filter(|&i| marked(i) && parents[i].is_none())
                .collect();
            let stayed = (0..self.tasks.len())
                .filter(|&i| marked(i) && parents[i].is_some_and(|p| !marked(p)))
                .count();
            if stayed > 0 {
                let n = task::count_of(stayed, "sub item");
                self.message = Some(format!("{n} stayed: {WITH_ITS_TASK}"));
            }
            let moves = cards
                .iter()
                .any(|&i| shifted(self.tasks[i].status, by).is_some());
            return match moves {
                true => {
                    let mut ids: Vec<u32> = cards.iter().map(|&i| self.tasks[i].id).collect();
                    ids.sort_unstable();
                    Outcome::Change(Op::Shift { ids, by })
                }
                false => Outcome::Nothing,
            };
        }
        if self.picked().is_some_and(|i| parents[i].is_some()) {
            self.message = Some(WITH_ITS_TASK.into());
            return Outcome::Nothing;
        }
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

    /// Marks the marked cards and sub items done, clearing the marks, or else
    /// the picked card; a card's open sub items with it. A picked sub item is
    /// marked done, or opened again when it's done.
    fn finish(&mut self) -> Outcome {
        let ids: Vec<u32> = match self.picked() {
            _ if !self.marked.is_empty() => std::mem::take(&mut self.marked).into_iter().collect(),
            Some(i) if self.is_sub(i) => {
                let task = &self.tasks[i];
                let to = match task.status {
                    Status::Done => Status::Todo,
                    _ => Status::Done,
                };
                return Outcome::Change(Op::Move {
                    id: task.id,
                    to,
                    follow: true,
                });
            }
            Some(i) => vec![self.tasks[i].id],
            None => return Outcome::Nothing,
        };
        match self.finishing(&ids).is_empty() {
            false => Outcome::Change(Op::Finish { ids }),
            true => Outcome::Nothing,
        }
    }

    fn input_key(&mut self, key: KeyEvent) -> Outcome {
        let Some(input) = self.input.as_mut() else {
            return Outcome::Nothing;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Char('c') if ctrl => self.input = None,
            KeyCode::Char('a') if ctrl => input.cursor = 0,
            KeyCode::Char('e') if ctrl => input.cursor = input.text.len(),
            KeyCode::Char('u') if ctrl => {
                input.text.replace_range(..input.cursor, "");
                input.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let before = input.text[..input.cursor].trim_end();
                let kept = before.rfind(' ').map_or(0, |i| i + 1);
                input.text.replace_range(kept..input.cursor, "");
                input.cursor = kept;
            }
            KeyCode::Char(c) if !ctrl => {
                input.text.insert(input.cursor, c);
                input.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                let prev = input.prev();
                input.text.replace_range(prev..input.cursor, "");
                input.cursor = prev;
            }
            KeyCode::Left => input.cursor = input.prev(),
            KeyCode::Right => input.cursor = input.next(),
            KeyCode::Enter => {
                let (kind, words) = (input.kind, [input.text.clone()]);
                return self.submit(kind, &words);
            }
            _ => {}
        }
        Outcome::Nothing
    }

    /// Enter on the input: the change its words ask for, or why they can't be
    /// read, the words kept.
    fn submit(&mut self, kind: InputKind, words: &[String; 1]) -> Outcome {
        let now = Zoned::now().datetime();
        if let InputKind::Add | InputKind::Sub(_) = kind {
            if words[0].trim().is_empty() {
                self.input = None;
                return Outcome::Nothing;
            }
            let parent = match kind {
                InputKind::Sub(id) => Some(id),
                _ => None,
            };
            return match parse::parse(words, now) {
                Ok(task) => {
                    self.input = None;
                    Outcome::Change(Op::Add { task, parent })
                }
                Err(e) => {
                    self.message = Some(e);
                    Outcome::Nothing
                }
            };
        }

        let ids: Vec<u32> = match kind {
            InputKind::Edit(id) => vec![id],
            _ => self.marked.iter().copied().collect(),
        };
        let tasks: Vec<&Task> = self.tasks.iter().filter(|t| ids.contains(&t.id)).collect();
        // `e` on one fills in its title, which is read again: a `-tag` that's
        // a word of that title went in as one (the task hadn't the tag then),
        // and stays one, or an untouched enter would take the tag off. When
        // only `-words` are left for the title, though, the title is gone from
        // the input, and they're tags to take off: one the task hasn't is
        // refused, as `halil edit` refuses it.
        let in_title = |t: &Task, tag: &str| {
            t.title
                .split_whitespace()
                .any(|w| w.strip_prefix('-').is_some_and(|w| w.to_lowercase() == tag))
        };
        let read = |keep_title_words: bool| {
            parse::change(words, now, |tag| {
                tasks.iter().any(|t| {
                    t.tags.iter().any(|own| own == tag) && !(keep_title_words && in_title(t, tag))
                })
            })
        };
        let change = match kind {
            InputKind::Edit(id) => {
                read(true).and_then(|c| match parse::refuse_missing_tags(id, &c) {
                    Ok(()) => Ok(c),
                    Err(_) => {
                        read(false).and_then(|c| parse::refuse_missing_tags(id, &c).map(|()| c))
                    }
                })
            }
            _ => read(false),
        };
        let change = match change {
            Ok(change) => change,
            Err(e) => {
                self.message = Some(e);
                return Outcome::Nothing;
            }
        };
        if kind == InputKind::EditMarked && change.title.is_some() {
            self.message = Some(format!(
                "{} marked: a date, +tags or a !priority change them all",
                self.marked.len()
            ));
            return Outcome::Nothing;
        }
        let changes = tasks.iter().any(|&t| {
            let mut changed = t.clone();
            change.apply(&mut changed);
            changed != *t
        });
        self.input = None;
        match changes {
            true => Outcome::Change(Op::Edit { ids, change }),
            false => Outcome::Nothing,
        }
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
        let parents = task::parents(&self.tasks);
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
            let count = cards.iter().filter(|&&i| parents[i].is_none()).count();
            heading.push(&format!("  {count}"), Style::fg(QUIETER));
        }
        lines.push(heading.spans);
        lines.push(Line::new());

        for r in 0..body_h {
            let mut row = Row::default();
            for (c, cards) in columns.iter().enumerate() {
                row.pad_to(x(c), Style::default());
                let n = self.scroll[c] + r;
                if let Some(&i) = cards.get(n) {
                    let task = &self.tasks[i];
                    let picked = c == self.col && n == self.row[c];
                    let marked = self.marked.contains(&task.id);
                    let look = Look {
                        width: col_w,
                        picked,
                        marked,
                        today: self.today,
                    };
                    if parents[i].is_some() {
                        sub_item(&mut row, task, look);
                    } else {
                        let subs = task::sub_items(&self.tasks, &parents, i);
                        let count = task::progress(&self.tasks, &subs);
                        card(&mut row, task, count.as_deref(), look);
                    }
                } else if r == 0 && cards.is_empty() {
                    row.push(
                        "   Nothing here",
                        Style {
                            italic: true,
                            ..Style::fg(QUIETER)
                        },
                    );
                }
            }
            lines.push(row.spans);
        }

        // A refusal in red, then a note of what was done in the terminal's
        // own colour, three cells after it.
        let mut message = Row::default();
        for (said, style) in [
            (&self.message, Style::fg(RED)),
            (&self.note, Style::default()),
        ] {
            let gap = if message.width == 0 { 2 } else { 3 };
            if let Some(said) = said
                && message.width + gap < w
            {
                message.pad_to(message.width + gap, Style::default());
                message.push(&truncate(said, w - message.width), style);
            }
        }
        lines.push(message.spans);

        let mut footer = Row::default();
        let mut cursor = None;
        match &self.input {
            Some(input) => {
                let (label, hint, example) = match input.kind {
                    InputKind::Add => (
                        "New task".to_string(),
                        "enter to add · esc to cancel",
                        "title friday +tag !high",
                    ),
                    InputKind::Sub(id) => (
                        format!("Sub item of {id}"),
                        "enter to add · esc to cancel",
                        "title friday +tag !high",
                    ),
                    InputKind::Edit(id) => (
                        format!("Edit {id}"),
                        "enter to change · esc to cancel",
                        "title friday +tag -tag !high",
                    ),
                    InputKind::EditMarked => (
                        format!("Edit {} marked", self.marked.len()),
                        "enter to change · esc to cancel",
                        "friday +tag -tag !high",
                    ),
                };
                footer.push(&format!("  {label}  "), Style::fg(QUIET));
                let room = w.saturating_sub(footer.width + hint.width() + 4);
                let start = footer.width;
                let mut at = 0;
                if input.text.is_empty() {
                    footer.push(&truncate(example, room), Style::fg(QUIETER));
                } else {
                    let (shown, x) = view(&input.text, input.cursor, room);
                    footer.push(&shown, Style::default());
                    at = x;
                }
                cursor = Some(((start + at) as u16, (h - 1) as u16));
                if room > 0 {
                    footer.pad_to(w - hint.width() - 2, Style::default());
                    footer.push(hint, Style::fg(QUIETER));
                }
            }
            None if !self.marked.is_empty() => {
                footer.push(
                    &truncate(&format!("  {} marked", self.marked.len()), w),
                    Style::default(),
                );
                // `esc clear` gives way first, then `e`, then `h l`; `d` never.
                let hints = [
                    ("h l", "move", Some(3)),
                    ("e", "edit", Some(2)),
                    ("d", "done", None),
                    ("esc", "clear", Some(1)),
                ];
                for (keys, what) in giving_way(&hints, w.saturating_sub(footer.width), 4) {
                    footer.push("   ", Style::default());
                    footer.push(keys, Style::fg(QUIET));
                    footer.push(&format!(" {what}"), Style::fg(QUIETER));
                }
            }
            None => {
                // When they don't all fit, the keys give way by need: `tab`
                // first (the arrows go between columns too), then `q` (`esc`
                // quits too), then `j k`, and only then the rest. `d` never
                // goes, nor its word for what it does to the picked row.
                let sub = self.picked().is_some_and(|i| parents[i].is_some());
                footer.push(" ", Style::default());
                let hints = [
                    ("j k", "pick", Some(3)),
                    ("space", "mark", Some(5)),
                    ("h l", "move", Some(8)),
                    ("tab", "column", Some(1)),
                    ("a", "add", Some(7)),
                    ("A", "sub", Some(4)),
                    ("e", "edit", Some(6)),
                    ("d", if sub { "done/open" } else { "done" }, None),
                    ("q", "quit", Some(2)),
                ];
                for (keys, what) in giving_way(&hints, w.saturating_sub(footer.width), 3) {
                    footer.push(&format!(" {keys} "), Style::fg(QUIET));
                    footer.push(&format!("{what} "), Style::fg(QUIETER));
                }
            }
        }
        lines.push(footer.spans);
        (lines, cursor)
    }
}

/// The footer's key hints that fit in `room` columns, in their order, each
/// taking `extra` columns besides its key and its word. When they don't all
/// fit, they give way by their rank, the lowest first; one ranked `None` never
/// does. What still doesn't fit is left off the end.
fn giving_way<'a>(
    hints: &[(&'a str, &'a str, Option<u8>)],
    room: usize,
    extra: usize,
) -> Vec<(&'a str, &'a str)> {
    let width = |&(keys, what, _): &(&str, &str, Option<u8>)| extra + keys.width() + what.width();
    let mut shown = hints.to_vec();
    while shown.iter().map(width).sum::<usize>() > room {
        let Some(next) = (0..shown.len())
            .filter(|&i| shown[i].2.is_some())
            .min_by_key(|&i| shown[i].2)
        else {
            break;
        };
        shown.remove(next);
    }
    let mut used = 0;
    shown
        .iter()
        .take_while(|hint| {
            used += width(hint);
            used <= room
        })
        .map(|&(keys, what, _)| (keys, what))
        .collect()
}

/// The status a column left (-1) or right (1) of `s`, if there's one.
fn shifted(s: Status, by: isize) -> Option<Status> {
    COLUMNS
        .iter()
        .position(|&c| c == s)?
        .checked_add_signed(by)
        .and_then(|c| COLUMNS.get(c).copied())
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

/// How a card or sub item is drawn: how wide, picked or marked, and the day
/// its due date is read from.
#[derive(Clone, Copy)]
struct Look {
    width: usize,
    picked: bool,
    marked: bool,
    today: Date,
}

/// A card's or sub item's styles: plain, its title's, and quiet.
fn styles(picked: bool) -> (Style, Style, Style) {
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
    (plain, title, Style { bg, ..quiet })
}

/// A `▌` when the row is marked, else a space. The bar is in the terminal's own
/// foreground, bold, picked or not: the highlight's grey would vanish on the
/// picked row, and a priority's colour would read as a priority.
fn bar(row: &mut Row, marked: bool, plain: Style) {
    if marked {
        row.push(
            "▌",
            Style {
                fg: None,
                bold: true,
                ..plain
            },
        );
    } else {
        row.push(" ", plain);
    }
}

/// A task's due date in words, and whether it's late, when it's open and the
/// words fit `inner` beside `taken` columns and 8 of title.
fn due_fitting(task: &Task, today: Date, inner: usize, taken: usize) -> Option<(String, bool)> {
    due_in_days(task, today)
        .map(|days| (due_in_words(days), days < 0))
        .filter(|(words, _)| words.width() + 2 + taken + 8 <= inner)
}

/// The due words at the right of a row, red when late.
fn push_due(row: &mut Row, due: Option<(String, bool)>, plain: Style, quiet: Style) {
    if let Some((words, overdue)) = due {
        row.push("  ", plain);
        let style = match overdue {
            true => Style {
                fg: Some(RED),
                ..plain
            },
            false => quiet,
        };
        row.push(&words, style);
    }
}

/// One card: a `▌` when it's marked, its priority mark, title, its sub items'
/// `count` quiet after it, tags dimmed beside them and its due date at the
/// right. When room is short the tags give way first, then the due date; the
/// count stays.
fn card(row: &mut Row, task: &Task, count: Option<&str>, look: Look) {
    let Look {
        width,
        picked,
        marked,
        today,
    } = look;
    let (plain, title, quiet) = styles(picked);

    let start = row.width;
    bar(row, marked, plain);
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
    let count_w = count.map_or(0, |c| c.width() + 2);
    let due = due_fitting(task, today, inner, count_w);
    let room = inner - due.as_ref().map_or(0, |(words, _)| words.width() + 2);

    let shown = truncate(&task.title, room.saturating_sub(count_w));
    row.push(&shown, title);
    if let Some(count) = count {
        row.push("  ", plain);
        row.push(count, quiet);
    }
    let used = shown.width() + count_w;
    if !task.tags.is_empty() && room >= used + 2 + 3 {
        let tags: Vec<String> = task.tags.iter().map(|t| format!("+{t}")).collect();
        row.push("  ", plain);
        row.push(&truncate(&tags.join(" "), room - used - 2), quiet);
    }
    row.pad_to(start + 3 + room, plain);
    push_due(row, due, plain, quiet);
    row.pad_to(start + width, plain);
}

/// Where a sub item's `◦` goes: two cells further in than a card's title.
const SUB_INDENT: usize = 5;

/// One sub item, under its card: a `▌` when it's marked, `◦ title` while
/// it's open or `● title` dimmed once it's done, and its due date at the right
/// when it fits.
fn sub_item(row: &mut Row, task: &Task, look: Look) {
    let Look {
        width,
        picked,
        marked,
        today,
    } = look;
    let (plain, title, quiet) = styles(picked);
    let done = task.status == Status::Done;

    let start = row.width;
    bar(row, marked, plain);
    row.pad_to(start + SUB_INDENT, plain);
    row.push(if done { "●" } else { "◦" }, quiet);
    row.push(" ", plain);

    // Room after the glyph and its space, a space of padding at the end.
    let inner = width.saturating_sub(SUB_INDENT + 3);
    let due = due_fitting(task, today, inner, 0);
    let room = inner - due.as_ref().map_or(0, |(words, _)| words.width() + 2);
    row.push(
        &truncate(&task.title, room),
        if done { quiet } else { title },
    );
    row.pad_to(start + SUB_INDENT + 2 + room, plain);
    push_due(row, due, plain, quiet);
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

/// The words being typed after `a`, `A` or `e`, and the cursor in them: a byte
/// index, always between graphemes.
#[derive(Debug, PartialEq, Eq)]
struct Input {
    kind: InputKind,
    text: String,
    cursor: usize,
}

/// What the input is for: a new task, a new sub item of a card (by id), the
/// picked card or sub item (by id), or the marked ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Add,
    Sub(u32),
    Edit(u32),
    EditMarked,
}

impl Input {
    /// The input with `text` filled in and the cursor at its end.
    fn new(kind: InputKind, text: String) -> Input {
        let cursor = text.len();
        Input { kind, text, cursor }
    }

    /// Where the grapheme before the cursor starts.
    fn prev(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// Where the grapheme after the cursor ends.
    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }
}

/// What fits of `s` in `max` columns with the cursor, at byte `at`, in view,
/// and the cursor's column in it.
fn view(s: &str, at: usize, max: usize) -> (String, usize) {
    let before = &s[..at];
    if s.width() <= max {
        return (s.into(), before.width());
    }
    if before.width() < max {
        return (head(s, max), before.width());
    }
    let shown = tail(before, max);
    let x = shown.width();
    (shown, x)
}

/// The start of `s` that fits in `max` columns.
fn head(s: &str, max: usize) -> String {
    let mut width = 0;
    s.chars()
        .take_while(|ch| {
            width += ch.width().unwrap_or(0);
            width <= max
        })
        .collect()
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
    bold: bool,
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
    if style.bold {
        sgr += ";1";
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
            parent: None,
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

    /// The words in the input, while it's open.
    fn typed(board: &Board) -> Option<&str> {
        board.input.as_ref().map(|i| i.text.as_str())
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
        assert_eq!(typed(&board), Some("x !urgent"));
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
        assert_eq!(typed(&board), Some("q"));
        board.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(press(&mut board, "q"), [Outcome::Quit]);
    }

    #[test]
    fn a_ctrl_chord_is_never_its_letters_act_and_ctrl_c_quits() {
        // Ctrl-L, a redraw elsewhere, moved a card; Ctrl-D finished one. Each
        // chord on a board of its own, so none hides another's act.
        let chords = ['l', 'd'].into_iter().chain('a'..='z').chain(['A', ' ']);
        for c in chords.filter(|&c| c != 'c') {
            let mut board = sub_sample();
            assert_eq!(ctrl(&mut board, c), Outcome::Nothing, "ctrl-{c}");
            assert_eq!(board.tasks, sub_sample().tasks, "ctrl-{c}");
            assert_eq!(
                (board.col, picked_id(&board)),
                (0, Some(2)),
                "ctrl-{c} moved nothing"
            );
            assert!(board.marked.is_empty(), "ctrl-{c} marked nothing");
            assert_eq!(board.input, None, "ctrl-{c} opened nothing");
        }

        // In the input, only ctrl-a, ctrl-e, ctrl-u, ctrl-w and ctrl-c do
        // anything, and none types its letter.
        let mut board = sub_sample();
        press(&mut board, "abuy milk");
        for c in ('a'..='z').filter(|&c| !"aceuw".contains(c)) {
            assert_eq!(
                board.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
                Outcome::Nothing,
                "ctrl-{c}"
            );
        }
        assert_eq!(typed(&board), Some("buy milk"));
        let input = board.input.as_ref().unwrap();
        assert_eq!(input.cursor, input.text.len(), "the cursor stayed");
        ctrl(&mut board, 'c');
        assert_eq!(board.input, None);
        assert_eq!(ctrl(&mut board, 'c'), Outcome::Quit);
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

        // Its words start where a card's title does, in its own column.
        let mut board = Board::new(vec![task(1, "buy milk", Status::Todo, None, &[])], today());
        let (lines, _) = board.draw(80, 10);
        let row = text(&lines[3]);
        let col_w = (80 - 2 * MARGIN - 2 * GAP) / 3;
        let at = |s: &str| row[..row.find(s).unwrap()].width();
        let title = at("buy milk") - MARGIN;
        let nothing = at("Nothing here") - (MARGIN + col_w + GAP);
        assert_eq!(nothing, title, "{row:?}");
    }

    /// The sample with pay rent (Todo) and fix bike (Doing) marked, Doing picked.
    fn marked_sample() -> Board {
        let mut board = sample();
        press(&mut board, " ");
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, " ");
        board
    }

    fn status(board: &Board, id: u32) -> Status {
        board.tasks.iter().find(|t| t.id == id).unwrap().status
    }

    #[test]
    fn space_marks_or_unmarks_the_card_and_steps_down_but_not_off_the_end() {
        let mut board = sample();
        press(&mut board, " ");
        assert_eq!(board.marked, BTreeSet::from([2]));
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, " ");
        assert_eq!(board.marked, BTreeSet::from([1, 2]));
        assert_eq!(picked_id(&board), Some(1), "it stays on the last row");
        press(&mut board, " ");
        assert_eq!(board.marked, BTreeSet::from([2]));
        press(&mut board, "k ");
        assert!(board.marked.is_empty());
        assert_eq!(picked_id(&board), Some(1));
    }

    #[test]
    fn a_marked_card_shows_a_bold_bar_in_the_terminals_foreground_picked_or_not() {
        let mut board = sample();
        press(&mut board, "  k");
        assert_eq!(picked_id(&board), Some(2));
        let (lines, cursor) = board.draw(100, 12);
        for (line, picked) in [(&lines[3], true), (&lines[4], false)] {
            assert_eq!(text(line).chars().nth(MARGIN), Some('▌'), "its first cell");
            let bar = line.iter().find(|s| s.text == "▌").unwrap();
            // Not the highlight's grey (gone on the picked row), nor a
            // priority's colour (it would read as one).
            assert_eq!(bar.style.fg, None, "the terminal's own foreground");
            assert!(bar.style.bold);
            assert_eq!(bar.style.bg.is_some(), picked, "on the row's background");
        }
        assert_eq!(
            text(&lines[3]).matches('▌').count(),
            1,
            "fix bike isn't marked"
        );

        let mut out = Vec::new();
        paint(&mut out, &lines, cursor, true).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("\x1b[0;48;5;236;1m▌"), "picked: bold, no fg");
        assert!(out.contains("\x1b[0;1m▌"), "unpicked: bold, no fg");

        let mut out = Vec::new();
        paint(&mut out, &lines, cursor, false).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("38;5"), "no colours");
        assert_eq!(out.matches('▌').count(), 2, "the glyph alone says it");
    }

    #[test]
    fn marks_hold_across_columns_and_reloads_and_a_gone_id_drops_out() {
        let mut board = marked_sample();
        assert_eq!(board.marked, BTreeSet::from([2, 3]));
        board.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(board.marked, BTreeSet::from([2, 3]));
        // Meanwhile another `halil` deleted fix bike.
        let fresh = sample().tasks.into_iter().filter(|t| t.id != 3).collect();
        board.replace_tasks(fresh);
        assert_eq!(board.marked, BTreeSet::from([2]));
        let (lines, _) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(footer.trim_start().starts_with("1 marked"), "{footer:?}");
    }

    #[test]
    fn marks_on_sub_items_drop_when_their_card_goes_to_done() {
        // Pay rent (2), its sub item 6 and buy milk (1) marked.
        let mut board = sub_sample();
        press(&mut board, " j j ");
        assert_eq!(board.marked, BTreeSet::from([1, 2, 6]));
        run_keys(&mut board, "l");
        assert_eq!(
            board.marked,
            BTreeSet::from([1, 2, 6]),
            "in Doing, 6 is in sight under its card"
        );
        run_keys(&mut board, "l");
        assert_eq!(
            board.marked,
            BTreeSet::from([1, 2]),
            "in Done, 6 is folded into the count: no row for d to act on"
        );
        let (lines, _) = board.draw(100, 14);
        assert!(text(&lines[13]).trim_start().starts_with("2 marked"));

        // Meanwhile a `halil done 2` elsewhere: the reload drops 6's mark.
        let mut board = sub_sample();
        press(&mut board, "jj ");
        assert_eq!(board.marked, BTreeSet::from([6]));
        let mut fresh = sub_sample().tasks;
        for t in fresh
            .iter_mut()
            .filter(|t| t.id == 2 || t.parent == Some(2))
        {
            t.status = Status::Done;
        }
        board.replace_tasks(fresh);
        assert!(board.marked.is_empty());
    }

    #[test]
    fn with_marks_h_l_and_d_act_on_every_marked_card_in_one_change() {
        let mut board = marked_sample();
        let [Outcome::Change(op)] = &mut press(&mut board, "l")[..] else {
            panic!("l is one change");
        };
        assert_eq!(
            *op,
            Op::Shift {
                ids: vec![2, 3],
                by: 1
            }
        );
        // Meanwhile another `halil` moved pay rent to Doing: it goes on from there.
        let mut fresh = sample().tasks;
        fresh[1].status = Status::Doing;
        board.replace_tasks(fresh);
        board.apply(std::mem::replace(op, Op::Finish { ids: vec![] }));
        assert_eq!(status(&board, 2), Status::Done);
        assert_eq!(status(&board, 3), Status::Done);
        assert_eq!(status(&board, 1), Status::Todo, "unmarked stays");

        // One at the edge stays while the others move.
        let mut board = marked_sample();
        run_keys(&mut board, "h");
        assert_eq!(status(&board, 2), Status::Todo);
        assert_eq!(status(&board, 3), Status::Todo);
        assert_eq!(
            press(&mut board, "h"),
            [Outcome::Nothing],
            "all at the edge"
        );

        let mut board = marked_sample();
        assert_eq!(
            press(&mut board, "d"),
            [Outcome::Change(Op::Finish { ids: vec![2, 3] })]
        );
    }

    #[test]
    fn marks_stay_after_h_and_l_and_clear_after_d() {
        let mut board = marked_sample();
        run_keys(&mut board, "ll");
        assert_eq!(status(&board, 2), Status::Done);
        assert_eq!(status(&board, 3), Status::Done);
        assert_eq!(board.marked, BTreeSet::from([2, 3]));
        run_keys(&mut board, "hh");
        assert_eq!(status(&board, 2), Status::Todo);
        assert_eq!(status(&board, 3), Status::Todo);
        run_keys(&mut board, "d");
        assert_eq!(status(&board, 2), Status::Done);
        assert_eq!(status(&board, 3), Status::Done);
        assert!(board.marked.is_empty());
        assert_eq!(status(&board, 1), Status::Todo);
    }

    #[test]
    fn esc_clears_the_marks_then_quits_and_q_always_quits() {
        let mut board = marked_sample();
        assert_eq!(board.key(KeyEvent::from(KeyCode::Esc)), Outcome::Nothing);
        assert!(board.marked.is_empty());
        assert_eq!(board.key(KeyEvent::from(KeyCode::Esc)), Outcome::Quit);
        let mut board = marked_sample();
        assert_eq!(press(&mut board, "q"), [Outcome::Quit]);
    }

    #[test]
    fn while_marked_the_footer_says_how_many_and_what_acts_on_them() {
        let mut board = marked_sample();
        board.key(KeyEvent::from(KeyCode::BackTab));
        press(&mut board, " ");
        assert_eq!(board.marked, BTreeSet::from([1, 2, 3]));
        let (lines, _) = board.draw(100, 12);
        assert_eq!(
            text(&lines[11]).trim_start(),
            "3 marked   h l move   e edit   d done   esc clear"
        );
    }

    #[test]
    fn the_ordinary_footer_offers_space_to_mark() {
        let mut board = sample();
        let (lines, _) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(footer.contains(" space mark "), "{footer:?}");
    }

    #[test]
    fn the_ordinary_footer_offers_e_to_edit() {
        let mut board = sample();
        let (lines, _) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(footer.contains(" e edit "), "{footer:?}");
    }

    fn key(board: &mut Board, code: KeyCode) -> Outcome {
        board.key(KeyEvent::from(code))
    }

    fn ctrl(board: &mut Board, c: char) -> Outcome {
        board.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    /// Presses enter, making the change it asks for as the loop does; whether
    /// there was one.
    fn enter(board: &mut Board) -> bool {
        match key(board, KeyCode::Enter) {
            Outcome::Change(op) => {
                board.apply(op);
                true
            }
            _ => false,
        }
    }

    fn by_id(board: &Board, id: u32) -> &Task {
        board.tasks.iter().find(|t| t.id == id).unwrap()
    }

    #[test]
    fn e_opens_the_input_on_the_picked_cards_title_with_the_cursor_at_its_end() {
        let mut board = sample();
        press(&mut board, "e");
        assert_eq!(typed(&board), Some("pay rent"));
        let (lines, cursor) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(footer.starts_with("  Edit 2  pay rent "), "{footer:?}");
        assert!(
            footer.ends_with("enter to change · esc to cancel"),
            "{footer:?}"
        );
        assert_eq!(cursor, Some(("  Edit 2  pay rent".len() as u16, 11)));
    }

    #[test]
    fn enter_reads_the_edit_as_halil_edit_does() {
        let mut board = sample();
        press(&mut board, "e");
        ctrl(&mut board, 'u');
        press(&mut board, "pay the rent 2030-01-02 +car !low");
        assert!(enter(&mut board));
        let rent = by_id(&board, 2);
        assert_eq!(rent.title, "pay the rent");
        assert_eq!(rent.tags, ["home", "money", "car"]);
        assert_eq!(rent.priority, Some(Priority::Low));
        assert_eq!(rent.due, Some("2030-01-02".parse().unwrap()));
        assert_eq!(board.input, None);

        // No words left for a title: the title stays.
        press(&mut board, "e");
        ctrl(&mut board, 'u');
        press(&mut board, "!med");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 2).title, "pay the rent");
        assert_eq!(by_id(&board, 2).priority, Some(Priority::Med));
    }

    #[test]
    fn minus_tag_takes_off_a_tag_the_card_has_else_stays_in_the_title() {
        let mut board = sample();
        press(&mut board, "e -home");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 2).tags, ["money"]);
        assert_eq!(by_id(&board, 2).title, "pay rent");
        press(&mut board, "e -work");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 2).title, "pay rent -work");
        assert_eq!(by_id(&board, 2).tags, ["money"]);
    }

    #[test]
    fn an_untouched_enter_changes_nothing_and_says_nothing() {
        let mut board = sample();
        press(&mut board, "e");
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.input, None);
        assert_eq!(board.message, None);
        press(&mut board, "e +home");
        assert_eq!(
            key(&mut board, KeyCode::Enter),
            Outcome::Nothing,
            "it has +home"
        );
        assert_eq!(board.message, None);
        assert_eq!(board.tasks, sample().tasks);
    }

    #[test]
    fn a_bad_edit_says_why_and_keeps_the_words() {
        let mut board = sample();
        press(&mut board, "e !urgent");
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert!(board.message.as_deref().unwrap().contains("!urgent"));
        assert_eq!(typed(&board), Some("pay rent !urgent"));
        ctrl(&mut board, 'w');
        press(&mut board, "today tomorrow");
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert!(board.message.as_deref().unwrap().contains("today"));
        assert_eq!(typed(&board), Some("pay rent today tomorrow"));
        assert_eq!(board.tasks, sample().tasks);
    }

    #[test]
    fn the_edited_card_stays_picked_wherever_it_sorts_now() {
        let mut board = sample();
        press(&mut board, "j");
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, "e !high");
        assert!(enter(&mut board));
        assert_eq!(
            (picked_id(&board), board.row[0]),
            (Some(1), 0),
            "high, and older"
        );
        press(&mut board, "j");
        press(&mut board, "e 2030-01-02");
        assert!(enter(&mut board));
        assert_eq!(
            (picked_id(&board), board.row[0]),
            (Some(2), 0),
            "dated first"
        );
    }

    #[test]
    fn the_input_cursor_moves_and_edits_by_grapheme_for_a_and_e() {
        let mut board = sample();
        press(&mut board, "aac");
        key(&mut board, KeyCode::Left);
        press(&mut board, "b");
        assert_eq!(typed(&board), Some("abc"));
        ctrl(&mut board, 'a');
        press(&mut board, "x");
        ctrl(&mut board, 'e');
        press(&mut board, "y");
        assert_eq!(typed(&board), Some("xabcy"));
        key(&mut board, KeyCode::Left);
        key(&mut board, KeyCode::Left);
        key(&mut board, KeyCode::Backspace);
        assert_eq!(typed(&board), Some("xacy"));
        key(&mut board, KeyCode::Esc);

        // An accent typed as its own code point goes with its letter.
        press(&mut board, "ae\u{301}x");
        key(&mut board, KeyCode::Left);
        key(&mut board, KeyCode::Left);
        press(&mut board, "a");
        assert_eq!(typed(&board), Some("ae\u{301}x"));
        key(&mut board, KeyCode::Right);
        key(&mut board, KeyCode::Backspace);
        assert_eq!(typed(&board), Some("ax"));
        key(&mut board, KeyCode::Esc);

        press(&mut board, "e");
        ctrl(&mut board, 'a');
        for _ in 0..4 {
            key(&mut board, KeyCode::Right);
        }
        press(&mut board, "the ");
        assert_eq!(typed(&board), Some("pay the rent"));
        let (_, cursor) = board.draw(100, 12);
        assert_eq!(cursor, Some(("  Edit 2  pay the ".len() as u16, 11)));
    }

    #[test]
    fn with_marks_e_changes_them_all() {
        let mut board = marked_sample();
        press(&mut board, "e");
        assert_eq!(typed(&board), Some(""));
        let (lines, _) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(footer.starts_with("  Edit 2 marked  "), "{footer:?}");
        press(&mut board, "2030-01-02 +car -home !med");
        assert!(enter(&mut board));
        for (id, title, tags) in [
            (2, "pay rent", &["money", "car"][..]),
            (3, "fix bike", &["car"]),
        ] {
            let task = by_id(&board, id);
            assert_eq!(task.title, title);
            assert_eq!(task.tags, tags);
            assert_eq!(task.priority, Some(Priority::Med));
            assert_eq!(task.due, Some("2030-01-02".parse().unwrap()));
        }
        assert_eq!(by_id(&board, 1), &sample().tasks[0], "unmarked stays");
    }

    #[test]
    fn with_marks_title_words_are_refused_and_kept() {
        let mut board = marked_sample();
        press(&mut board, "enew title !high");
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(
            board.message.as_deref(),
            Some("2 marked: a date, +tags or a !priority change them all")
        );
        assert_eq!(typed(&board), Some("new title !high"));
        // A -tag no marked card has is a title word too.
        ctrl(&mut board, 'u');
        press(&mut board, "-work");
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert!(board.message.is_some());
        assert_eq!(board.tasks, sample().tasks);
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

    fn sub(task: Task, parent: u32) -> Task {
        Task {
            parent: Some(parent),
            ..task
        }
    }

    /// The sample with sub items under pay rent (2): 5 done, 6 open and 7
    /// started; and 8, under a task that's gone.
    fn sub_sample() -> Board {
        let mut board = sample();
        board.tasks.extend([
            sub(task(7, "find the iban", Status::Doing, None, &[]), 2),
            sub(task(5, "ask for the bill", Status::Done, None, &[]), 2),
            sub(task(6, "set up the transfer", Status::Todo, None, &[]), 2),
            sub(task(8, "orphan", Status::Todo, None, &[]), 99),
        ]);
        board
    }

    fn ids(board: &Board, c: usize) -> Vec<u32> {
        board.column(c).iter().map(|&i| board.tasks[i].id).collect()
    }

    /// Column `c` of a drawn row, `w` wide.
    fn cell(line: &Line, w: usize, c: usize) -> String {
        let col_w = (w - 2 * MARGIN - 2 * GAP) / 3;
        text(line)
            .chars()
            .skip(MARGIN + c * (col_w + GAP))
            .take(col_w)
            .collect()
    }

    #[test]
    fn sub_items_show_under_their_card_in_id_order_whatever_their_status() {
        let mut board = sub_sample();
        assert_eq!(
            ids(&board, 0),
            [2, 5, 6, 7, 1, 8],
            "8's task is gone: a card"
        );
        assert_eq!(ids(&board, 1), [3]);
        assert_eq!(ids(&board, 2), [4]);
        let (lines, _) = board.draw(100, 14);
        let heading = text(&lines[1]);
        assert!(
            heading.starts_with("   Todo  3 "),
            "cards only: {heading:?}"
        );
        assert!(heading.contains(" Doing  1"), "{heading:?}");
        assert!(cell(&lines[8], 100, 0).starts_with("   orphan"));
    }

    #[test]
    fn a_sub_item_is_a_row_two_cells_in_open_or_done_and_dimmed() {
        let mut board = sub_sample();
        let (lines, _) = board.draw(100, 14);
        let rows: Vec<String> = lines[3..7].iter().map(|l| cell(l, 100, 0)).collect();
        assert!(rows[0].starts_with(" • pay rent  1/3"), "{rows:?}");
        assert_eq!(rows[1].trim_end(), "     ● ask for the bill");
        assert_eq!(rows[2].trim_end(), "     ◦ set up the transfer");
        assert_eq!(
            rows[3].trim_end(),
            "     ◦ find the iban",
            "started is open"
        );
        let title_at = rows[0].find("pay rent").unwrap() - "•".len() + 1;
        assert_eq!(rows[2].chars().position(|c| c == '◦'), Some(title_at + 2));

        let style = |line: &Line, s: &str| line.iter().find(|span| span.text == s).unwrap().style;
        assert_eq!(style(&lines[4], "●").fg, Some(QUIET));
        assert_eq!(
            style(&lines[4], "ask for the bill").fg,
            Some(QUIET),
            "dimmed"
        );
        assert_eq!(style(&lines[5], "set up the transfer").fg, None);
        assert!(lines[5].iter().all(|s| s.style.bg.is_none()));
        press(&mut board, "jj");
        let (lines, _) = board.draw(100, 14);
        assert_eq!(
            style(&lines[5], "set up the transfer").bg,
            Some(HIGHLIGHT),
            "picked"
        );
    }

    #[test]
    fn a_sub_items_due_date_shows_at_the_right_when_it_fits() {
        let mut board = Board::new(
            vec![
                task(1, "ship", Status::Todo, None, &[]),
                sub(
                    due(task(2, "tag it", Status::Todo, None, &[]), "2026-11-12"),
                    1,
                ),
                sub(
                    due(task(3, "bump it", Status::Done, None, &[]), "2026-11-12"),
                    1,
                ),
            ],
            today(),
        );
        let (lines, _) = board.draw(100, 10);
        assert_eq!(cell(&lines[4], 100, 0), "     ◦ tag it        tomorrow ");
        assert!(!text(&lines[5]).contains("tomorrow"), "done: no date");
        let (lines, _) = board.draw(70, 10);
        assert_eq!(
            cell(&lines[4], 70, 0).trim_end(),
            "     ◦ tag it",
            "no room"
        );
    }

    #[test]
    fn a_card_counts_its_sub_items_and_the_count_outlasts_tags_and_due_date() {
        let mut board = Board::new(
            vec![
                due(
                    task(1, "ship 0.2.0", Status::Todo, None, &["work"]),
                    "2026-11-12",
                ),
                sub(task(2, "a", Status::Done, None, &[]), 1),
                sub(task(3, "b", Status::Todo, None, &[]), 1),
                sub(task(4, "c", Status::Done, None, &[]), 1),
                sub(task(5, "d", Status::Todo, None, &[]), 1),
            ],
            today(),
        );
        for (w, want) in [
            (120, "   ship 0.2.0  2/4  +work  tomorrow "),
            (100, "   ship 0.2.0  2/4   tomorrow "),
            (70, "   ship 0.2.0  2/4  "),
            (46, "   sh…  2/4 "),
        ] {
            let (lines, _) = board.draw(w, 12);
            assert_eq!(cell(&lines[3], w, 0), want, "{w} wide");
            let card: usize = lines[3]
                .iter()
                .filter(|s| s.style.bg == Some(HIGHLIGHT))
                .map(|s| s.text.width())
                .sum();
            assert_eq!(
                card,
                (w - 2 * MARGIN - 2 * GAP) / 3,
                "{w} wide: its column, no more"
            );
        }
        let (lines, _) = board.draw(100, 12);
        let count = lines[3].iter().find(|s| s.text == "2/4").unwrap();
        assert_eq!(count.style.fg, Some(QUIET_ON_HIGHLIGHT), "quiet");
    }

    #[test]
    fn in_done_a_card_shows_its_count_its_sub_items_folded() {
        let mut board = sample();
        board.tasks.extend([
            sub(task(9, "ring", Status::Done, None, &[]), 4),
            sub(task(10, "write", Status::Todo, None, &[]), 4),
        ]);
        assert_eq!(ids(&board, 2), [4]);
        let (lines, _) = board.draw(100, 12);
        assert_eq!(cell(&lines[3], 100, 2).trim_end(), "   call mum  1/2");
        let all: String = lines.iter().map(text).collect();
        assert!(!all.contains("ring") && !all.contains("write"), "{all:?}");
    }

    #[test]
    fn j_k_g_and_g_step_and_scroll_by_rows_sub_items_included() {
        let mut board = sub_sample();
        let mut seen = vec![picked_id(&board)];
        for _ in 0..6 {
            press(&mut board, "j");
            seen.push(picked_id(&board));
        }
        let want = [2, 5, 6, 7, 1, 8, 8].map(Some);
        assert_eq!(seen, want);
        press(&mut board, "k");
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, "g");
        assert_eq!(picked_id(&board), Some(2));

        let mut tasks = vec![task(1, "card", Status::Todo, None, &[])];
        tasks.extend(
            (2..=20).map(|id| sub(task(id, &format!("sub {id}"), Status::Todo, None, &[]), 1)),
        );
        let mut board = Board::new(tasks, today());
        press(&mut board, "G");
        assert_eq!(picked_id(&board), Some(20));
        let (lines, _) = board.draw(80, 10);
        let body: Vec<String> = lines[3..8].iter().map(text).collect();
        assert!(
            body[4].contains("sub 20") && body[0].contains("sub 16"),
            "{body:?}"
        );
    }

    #[test]
    fn d_marks_a_sub_item_done_or_open_and_on_a_card_finishes_its_sub_items_too() {
        let mut board = sub_sample();
        press(&mut board, "j");
        assert_eq!(
            press(&mut board, "d"),
            [Outcome::Change(Op::Move {
                id: 5,
                to: Status::Todo,
                follow: true
            })]
        );
        run_keys(&mut board, "d");
        assert_eq!(status(&board, 5), Status::Todo, "reopened");
        assert_eq!(picked_id(&board), Some(5));
        run_keys(&mut board, "d");
        assert_eq!(status(&board, 5), Status::Done);
        run_keys(&mut board, "jd");
        assert_eq!(status(&board, 6), Status::Done);
        assert_eq!(picked_id(&board), Some(6), "it stays where it is");
        run_keys(&mut board, "d");
        assert_eq!(status(&board, 6), Status::Todo, "reopened");

        // On the card: one change, so one save, for it and what's open under it.
        press(&mut board, "g");
        let [Outcome::Change(op)] = &mut press(&mut board, "d")[..] else {
            panic!("d is one change");
        };
        board.apply(std::mem::replace(op, Op::Finish { ids: vec![] }));
        for id in [2, 5, 6, 7] {
            assert_eq!(status(&board, id), Status::Done, "{id}");
        }
        assert_eq!(status(&board, 8), Status::Todo, "not its sub item");
        assert_eq!(picked_id(&board), Some(1), "the next card");
    }

    #[test]
    fn while_a_sub_item_is_picked_the_footer_says_d_done_or_open() {
        let mut board = sub_sample();
        press(&mut board, "j");
        let (lines, _) = board.draw(80, 12);
        let footer = text(&lines[11]);
        assert!(footer.contains(" d done/open "), "{footer:?}");
        assert!(footer.width() <= 80, "{footer:?}");
        press(&mut board, "k");
        let (lines, _) = board.draw(80, 12);
        let footer = text(&lines[11]);
        assert!(
            footer.contains(" d done ") && !footer.contains("open"),
            "{footer:?}"
        );
    }

    #[test]
    fn e_edits_a_sub_item_as_it_edits_a_card() {
        let mut board = sub_sample();
        press(&mut board, "jje");
        assert_eq!(typed(&board), Some("set up the transfer"));
        let (lines, _) = board.draw(100, 12);
        assert!(text(&lines[11]).starts_with("  Edit 6  "));
        press(&mut board, " +bank !low");
        assert!(enter(&mut board));
        let six = by_id(&board, 6);
        assert_eq!(six.title, "set up the transfer");
        assert_eq!(
            (six.tags.as_slice(), six.priority),
            (&["bank".to_string()][..], Some(Priority::Low))
        );
        assert_eq!(six.parent, Some(2));
        assert_eq!(picked_id(&board), Some(6));
    }

    #[test]
    fn h_and_l_on_a_sub_item_are_refused() {
        let mut board = sub_sample();
        press(&mut board, "j");
        for k in ["l", "h"] {
            assert_eq!(press(&mut board, k), [Outcome::Nothing]);
            assert_eq!(
                board.message.as_deref(),
                Some("a sub item moves with its task")
            );
        }
        assert_eq!(board.tasks, sub_sample().tasks);
    }

    #[test]
    fn marked_sub_items_take_d_and_stay_put_for_h_and_l() {
        let mut board = sub_sample();
        press(&mut board, "jj ");
        assert_eq!(board.marked, BTreeSet::from([6]));
        assert_eq!(picked_id(&board), Some(7), "space steps a row");
        press(&mut board, "j ");
        assert_eq!(board.marked, BTreeSet::from([1, 6]));
        assert_eq!(
            press(&mut board, "l"),
            [Outcome::Change(Op::Shift {
                ids: vec![1],
                by: 1
            })]
        );
        assert_eq!(
            board.message.as_deref(),
            Some("1 sub item stayed: a sub item moves with its task")
        );
        assert_eq!(
            press(&mut board, "d"),
            [Outcome::Change(Op::Finish { ids: vec![1, 6] })]
        );

        // Only sub items marked: nothing moves. Marked with its card: it goes along.
        let mut board = sub_sample();
        press(&mut board, "jjj ");
        assert_eq!(press(&mut board, "h"), [Outcome::Nothing]);
        assert!(
            board
                .message
                .as_deref()
                .unwrap()
                .starts_with("1 sub item stayed")
        );
        let mut board = sub_sample();
        press(&mut board, "  ");
        assert_eq!(board.marked, BTreeSet::from([2, 5]));
        run_keys(&mut board, "l");
        assert_eq!(board.message, None);
        assert_eq!(ids(&board, 1), [2, 5, 6, 7, 3], "high first");
    }

    #[test]
    fn a_card_whose_sub_items_are_all_done_stays_until_d() {
        let mut board = sub_sample();
        run_keys(&mut board, "jjdjd");
        assert_eq!(status(&board, 2), Status::Todo);
        let (lines, _) = board.draw(100, 14);
        assert!(cell(&lines[3], 100, 0).contains("pay rent  3/3"));
        run_keys(&mut board, "gd");
        assert_eq!(status(&board, 2), Status::Done);
    }

    #[test]
    fn a_card_moved_into_done_finishes_its_open_sub_items_and_h_reopens_none() {
        let mut board = sub_sample();
        run_keys(&mut board, "l");
        assert_eq!(status(&board, 2), Status::Doing);
        assert_eq!(status(&board, 6), Status::Todo, "not done yet");
        assert_eq!(board.message, None);

        // One change, so one save, for the card and what's open under it.
        let [Outcome::Change(op)] = &mut press(&mut board, "l")[..] else {
            panic!("l is one change");
        };
        board.apply(std::mem::replace(op, Op::Finish { ids: vec![] }));
        for id in [2, 5, 6, 7] {
            assert_eq!(status(&board, id), Status::Done, "{id}");
        }
        assert_eq!(status(&board, 8), Status::Todo, "not its sub item");
        assert_eq!(
            board.note.as_deref(),
            Some("Done, with its 2 open sub items.")
        );
        assert_eq!(board.message, None, "a note, not a refusal");

        run_keys(&mut board, "h");
        assert_eq!(status(&board, 2), Status::Doing);
        for id in [5, 6, 7] {
            assert_eq!(status(&board, id), Status::Done, "{id} stays done");
        }
        assert_eq!((&board.message, &board.note), (&None, &None));

        // d says it too.
        let mut board = sub_sample();
        run_keys(&mut board, "d");
        assert_eq!(status(&board, 6), Status::Done);
        assert_eq!(
            board.note.as_deref(),
            Some("Done, with its 2 open sub items.")
        );
    }

    #[test]
    fn marked_cards_moved_into_done_finish_their_open_sub_items() {
        // Pay rent and fix bike both in Doing, marked; 9 is open under fix bike.
        let mut board = sub_sample();
        board
            .tasks
            .push(sub(task(9, "pump the tyres", Status::Todo, None, &[]), 3));
        board.tasks[1].status = Status::Doing;
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(ids(&board, 1), [2, 5, 6, 7, 3, 9]);
        press(&mut board, " jjj ");
        assert_eq!(board.marked, BTreeSet::from([2, 3]));
        run_keys(&mut board, "l");
        for id in [2, 3, 5, 6, 7, 9] {
            assert_eq!(status(&board, id), Status::Done, "{id}");
        }
        assert_eq!(
            board.note.as_deref(),
            Some("Done, with their 3 open sub items.")
        );

        // A marked sub item of an unmarked card stays, and both are said.
        let mut board = sub_sample();
        board
            .tasks
            .push(sub(task(9, "pump the tyres", Status::Todo, None, &[]), 3));
        press(&mut board, "jj ");
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, " ");
        assert_eq!(board.marked, BTreeSet::from([3, 6]));
        run_keys(&mut board, "l");
        assert_eq!(
            (status(&board, 3), status(&board, 9), status(&board, 6)),
            (Status::Done, Status::Done, Status::Todo)
        );
        assert_eq!(
            board.message.as_deref(),
            Some("1 sub item stayed: a sub item moves with its task")
        );
        assert_eq!(
            board.note.as_deref(),
            Some("Done, with its 1 open sub item.")
        );
    }

    #[test]
    fn a_note_is_drawn_in_the_default_foreground_and_a_refusal_in_red_beside_it() {
        let style = |line: &Line, s: &str| line.iter().find(|span| span.text == s).unwrap().style;
        let mut board = sub_sample();
        run_keys(&mut board, "d");
        let (lines, _) = board.draw(100, 14);
        assert_eq!(text(&lines[12]), "  Done, with its 2 open sub items.");
        let note = style(&lines[12], "Done, with its 2 open sub items.");
        assert_eq!(note.fg, None, "not red: nothing was refused");

        // Joined to the refusal that a marked sub item stayed: each in its own
        // colour, three cells apart.
        let mut board = sub_sample();
        board
            .tasks
            .push(sub(task(9, "pump the tyres", Status::Todo, None, &[]), 3));
        press(&mut board, "jj ");
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, " ");
        run_keys(&mut board, "l");
        let (lines, _) = board.draw(100, 14);
        let stayed = "1 sub item stayed: a sub item moves with its task";
        let done = "Done, with its 1 open sub item.";
        assert_eq!(text(&lines[12]), format!("  {stayed}   {done}"));
        assert_eq!(style(&lines[12], stayed).fg, Some(RED));
        assert_eq!(style(&lines[12], done).fg, None);

        // Too narrow for both: the note is cut, the refusal stays whole.
        let (lines, _) = board.draw(60, 14);
        assert_eq!(text(&lines[12]), format!("  {stayed}   Done,…"));
    }

    #[test]
    fn a_sub_item_whose_card_went_meanwhile_is_added_as_a_task_of_its_own() {
        let mut board = sub_sample();
        press(&mut board, "A");
        assert_eq!(
            board.input.as_ref().map(|i| i.kind),
            Some(InputKind::Sub(2))
        );
        // Meanwhile a `halil rm 2` elsewhere took pay rent and its sub items.
        let fresh = sub_sample()
            .tasks
            .into_iter()
            .filter(|t| t.id != 2 && t.parent != Some(2))
            .collect();
        board.replace_tasks(fresh);
        press(&mut board, "call the bank");
        assert!(enter(&mut board));
        let added = board.tasks.last().unwrap();
        assert_eq!(
            (added.id, added.parent),
            (2, None),
            "its own task, not one under itself"
        );
        assert_eq!(
            board.note.as_deref(),
            Some("2 is gone: added as a task of its own."),
            "the add happened, another way"
        );
        assert_eq!(board.message, None, "not a refusal");
        let (lines, _) = board.draw(100, 12);
        let note = lines[10]
            .iter()
            .find(|s| s.text == "2 is gone: added as a task of its own.")
            .unwrap();
        assert_eq!(note.style.fg, None, "in the default foreground");
        // A card like any other: it takes sub items.
        assert_eq!(picked_id(&board), Some(2));
        press(&mut board, "A");
        assert_eq!(
            board.input.as_ref().map(|i| i.kind),
            Some(InputKind::Sub(2))
        );
    }

    #[test]
    fn a_minus_tag_in_the_title_stays_a_word_when_e_reads_the_title_again() {
        // `halil add x -money +money`: add takes no tag off, so -money is a
        // word of the title, beside the tag.
        let now = jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0);
        let added = parse::parse(&["x -money +money".into()], now).unwrap();
        assert_eq!(
            (added.title.as_str(), &added.tags[..]),
            ("x -money", &["money".to_string()][..])
        );
        let x = || task(1, "x -money", Status::Todo, None, &["money"]);
        let mut board = Board::new(vec![x()], today());

        press(&mut board, "e");
        assert_eq!(
            key(&mut board, KeyCode::Enter),
            Outcome::Nothing,
            "untouched"
        );
        assert_eq!(board.tasks, [x()]);

        press(&mut board, "e !high");
        assert!(enter(&mut board));
        let edited = by_id(&board, 1);
        assert_eq!(edited.title, "x -money");
        assert_eq!(edited.tags, ["money"]);
        assert_eq!(edited.priority, Some(Priority::High));

        // The title cleared, -money alone: the tag it has comes off.
        press(&mut board, "e");
        ctrl(&mut board, 'u');
        press(&mut board, "-money");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 1).title, "x -money");
        assert!(by_id(&board, 1).tags.is_empty());
    }

    #[test]
    fn e_with_only_a_tag_the_card_hasnt_is_refused_and_keeps_the_words() {
        let mut board = sample();
        press(&mut board, "e");
        ctrl(&mut board, 'u');
        press(&mut board, "-work");
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.message.as_deref(), Some("2 has no tag work"));
        assert_eq!(typed(&board), Some("-work"));
        assert_eq!(board.tasks, sample().tasks, "the title untouched");
    }

    #[test]
    fn capital_a_adds_a_sub_item_under_the_picked_card_with_the_grammar_of_a() {
        let mut board = sub_sample();
        press(&mut board, "A");
        let (lines, cursor) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(
            footer.starts_with("  Sub item of 2  title friday"),
            "{footer:?}"
        );
        assert_eq!(cursor, Some(("  Sub item of 2  ".len() as u16, 11)));
        press(&mut board, "call the bank 2030-01-02 +phone !med");
        assert!(enter(&mut board));
        let added = board.tasks.last().unwrap();
        assert_eq!(added.id, 9, "after 8, and not 99");
        assert_eq!(added.parent, Some(2));
        assert_eq!(added.title, "call the bank");
        assert_eq!(added.tags, ["phone"]);
        assert_eq!(added.priority, Some(Priority::Med));
        assert_eq!(added.due, Some("2030-01-02".parse().unwrap()));
        assert_eq!(added.status, Status::Todo);
        assert_eq!(picked_id(&board), Some(9));
        assert_eq!(ids(&board, 0), [2, 5, 6, 7, 9, 1, 8]);

        // On a sub item: under its card.
        press(&mut board, "kA");
        assert_eq!(
            board.input.as_ref().map(|i| i.kind),
            Some(InputKind::Sub(2))
        );
    }

    /// The footer of `board` drawn `w` wide.
    fn footer(board: &mut Board, w: usize) -> String {
        let (lines, _) = board.draw(w, 12);
        text(&lines[11])
    }

    #[test]
    fn at_80_columns_the_footers_fit_tab_giving_way_first() {
        let mut board = sample();
        assert_eq!(
            footer(&mut board, 100),
            "  j k pick  space mark  h l move  tab column  a add  A sub  e edit  d done  q quit ",
            "all of it, where there's room"
        );
        assert_eq!(
            footer(&mut board, 80),
            "  j k pick  space mark  h l move  a add  A sub  e edit  d done  q quit "
        );

        // A sub item picked: d's word for it stays, and so does the rest.
        let mut board = sub_sample();
        press(&mut board, "j");
        assert_eq!(
            footer(&mut board, 80),
            "  j k pick  space mark  h l move  a add  A sub  e edit  d done/open  q quit "
        );

        let mut board = marked_sample();
        assert_eq!(
            footer(&mut board, 80),
            "  2 marked   h l move   e edit   d done   esc clear"
        );
    }

    #[test]
    fn a_narrow_footer_gives_way_by_need_and_keeps_d() {
        // tab, then q, then j k; past them the least needed, d never.
        let mut board = sub_sample();
        press(&mut board, "j");
        for (w, want) in [
            (
                75,
                "  j k pick  space mark  h l move  a add  A sub  e edit  d done/open ",
            ),
            (
                67,
                "  space mark  h l move  a add  A sub  e edit  d done/open ",
            ),
            (46, "  h l move  a add  e edit  d done/open "),
        ] {
            assert_eq!(footer(&mut board, w), want, "{w} wide");
        }
        let mut board = marked_sample();
        assert_eq!(
            footer(&mut board, 46),
            "  2 marked   h l move   e edit   d done",
            "esc clear goes first"
        );
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
