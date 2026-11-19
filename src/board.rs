//! `halil board`: Todo, Doing and Done side by side.
//!
//! The board is drawn whole on every change, as plain rows of styled text: no
//! boxes, columns set apart by space. Every change is saved at once, and made
//! to the tasks file as it is on disk then, so a `halil add` from another
//! terminal while the board is open isn't lost.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::mpsc;
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

use crate::clip::{self, Clipboard};
use crate::hangul;
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
    event_loop(&mut board, path, &mut out, color, clip::detect())
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
    clipboard: Clipboard,
) -> Result<(), String> {
    let modified = || fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut seen: Option<SystemTime> = modified();
    let mut redraw = true;
    // Copies under way on threads of their own (see `clip`), and how many.
    let (copied_tx, copied) = mpsc::channel::<(String, Result<(), String>)>();
    let mut copying = 0;
    loop {
        while let Ok((what, result)) = copied.try_recv() {
            copying -= 1;
            board.copied(&what, result);
            redraw = true;
        }
        if redraw {
            let (w, h) = terminal::size().map_err(term_err)?;
            let (lines, cursor) = board.draw(w as usize, h as usize);
            paint(out, &lines, cursor, color).map_err(term_err)?;
            redraw = false;
        }

        // While a copy is under way, look for its result more often.
        let wait = if copying > 0 { 50 } else { 500 };
        if !event::poll(Duration::from_millis(wait)).map_err(term_err)? {
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
        // A Hangul syllable is several keys: each is done, its change made,
        // before the next.
        let mut outcome = board.key(key);
        loop {
            match outcome {
                Outcome::Quit => return Ok(()),
                Outcome::Nothing => {}
                Outcome::Copy { text, what } => match &clipboard {
                    Clipboard::Command { program, args } => {
                        copying += 1;
                        let (program, args) = (program.clone(), args.clone());
                        let done = copied_tx.clone();
                        clip::spawn(program, args, text, what, clip::LIMIT, done);
                    }
                    Clipboard::Terminal => {
                        let sent = write!(out, "{}", clip::osc52(&text)).and_then(|()| out.flush());
                        board.sent(&what, sent.map_err(|e| e.to_string()));
                    }
                },
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
            match board.next_queued() {
                Some(next) => outcome = next,
                None => break,
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
    /// `text` to copy to the clipboard: `what` it is, for the note
    /// (`"pay rent"`, `3 titles`).
    Copy {
        text: String,
        what: String,
    },
    Quit,
}

/// Said after how a Hangul letter was read, the first time one is: the long
/// form where it fits, else the short.
const HANGUL_LATE: [&str; 2] = [
    "A Korean input may hold a letter until the next key.",
    "It may run a key late.",
];

/// A task as the board holds it while it waits on a key: its id, and its
/// stamp, so a task that took the id meanwhile is never taken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ref {
    pub id: u32,
    pub stamp: u64,
}

/// A change to the tasks, by [`Ref`], so it can be made to the file as it is
/// now. A task whose stamp no longer matches its id is gone, and said to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    /// Set a task's status; `follow` keeps it picked in its new column. A card
    /// set to Done finishes its open sub items.
    Move { at: Ref, to: Status, follow: bool },
    /// Move each of these tasks a column left (-1) or right (1) of where it
    /// is; one already at the edge stays. A card moved into Done finishes its
    /// open sub items; one moved out of it reopens none.
    Shift { refs: Vec<Ref>, by: isize },
    /// Move each of these cards to Todo or Doing (`s`, `t`): out of Done, it
    /// reopens none of its sub items.
    Send { refs: Vec<Ref>, to: Status },
    /// Mark each of these tasks done, and the open sub items of those that
    /// are cards.
    Finish { refs: Vec<Ref> },
    /// Add a task, as a sub item of `parent` when there's one.
    Add { task: Parsed, parent: Option<Ref> },
    /// Change each of these tasks as `halil edit` would.
    Edit { refs: Vec<Ref>, change: Change },
    /// Remove the `asked` (the picked, or the `marked`), and `with` them
    /// their cards' sub items, as `halil rm` does.
    Remove {
        asked: Vec<Ref>,
        with: Vec<Ref>,
        marked: bool,
    },
}

/// A removal `x` asked about, in the footer, until the next key answers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Removal {
    asked: Vec<Ref>,
    with: Vec<Ref>,
    marked: bool,
    /// The title, when one task is asked about.
    title: Option<String>,
}

impl Removal {
    /// `Remove 3 "pay rent" and its 2 sub items?`, with the title as given,
    /// or none.
    fn question(&self, title: Option<&str>) -> String {
        let what = match (self.marked, title) {
            (true, _) => format!("{} marked", self.asked.len()),
            (false, Some(title)) => format!("{} \"{title}\"", self.asked[0].id),
            (false, None) => self.asked[0].id.to_string(),
        };
        format!(
            "Remove {what}{}?",
            with_sub_items(self.with.len(), self.asked.len())
        )
    }
}

/// ` and its 2 sub items`, ` and their 4 sub items`: the sub items going with
/// `of` tasks, or nothing when there are none.
fn with_sub_items(n: usize, of: usize) -> String {
    let whose = if of == 1 { "its" } else { "their" };
    match n {
        0 => String::new(),
        n => format!(" and {whose} {}", task::count_of(n, "sub item")),
    }
}

/// `task 3 is gone`, `tasks 3 and 5 are gone`.
fn gone(ids: &[u32]) -> String {
    match ids {
        [id] => format!("task {id} is gone"),
        [rest @ .., last] => {
            let rest: Vec<String> = rest.iter().map(u32::to_string).collect();
            format!("tasks {} and {last} are gone", rest.join(", "))
        }
        [] => String::new(),
    }
}

pub struct Board {
    pub tasks: Vec<Task>,
    /// The column picked, and the card picked in each column.
    col: usize,
    row: [usize; 3],
    /// The first card shown in each column, when it has more than fit.
    scroll: [usize; 3],
    /// The cards and sub items marked with space, which h, l, s, t, d, e and
    /// x act on together: each id, with its stamp.
    marked: BTreeMap<u32, u64>,
    /// The words being typed after `a`, `A` or `e`.
    input: Option<Input>,
    /// The removal `x` asked about, waiting on `y`.
    asking: Option<Removal>,
    /// The keys a Hangul letter was read as that are still to do, each with
    /// the jamo it stands for.
    queued: VecDeque<(KeyEvent, char)>,
    /// Whether a Hangul letter was read as keys yet, and said so.
    hangul_said: bool,
    /// An error or a refusal to show until the next key, in red.
    pub message: Option<String>,
    /// A note of what a change did besides what was asked, shown until the
    /// next key in the default foreground, after `message` if there's one.
    pub note: Option<String>,
    /// How a Hangul letter was read, the first time one is (`ㅓ read as j.`):
    /// after the note, with what the input method may do (see [`HANGUL_LATE`]).
    hint: Option<String>,
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
            marked: BTreeMap::new(),
            input: None,
            asking: None,
            queued: VecDeque::new(),
            hangul_said: false,
            message: None,
            note: None,
            hint: None,
        }
    }

    /// `tasks[i]` as the board holds it.
    fn held(&self, i: usize) -> Ref {
        Ref {
            id: self.tasks[i].id,
            stamp: self.tasks[i].stamp,
        }
    }

    /// The task `at` names, if it's still there: its id, with its stamp.
    fn find(&self, at: Ref) -> Option<usize> {
        self.tasks
            .iter()
            .position(|t| t.id == at.id && t.stamp == at.stamp)
    }

    fn is_marked(&self, i: usize) -> bool {
        self.marked.get(&self.tasks[i].id) == Some(&self.tasks[i].stamp)
    }

    fn marked_refs(&self) -> Vec<Ref> {
        self.marked
            .iter()
            .map(|(&id, &stamp)| Ref { id, stamp })
            .collect()
    }

    /// The ids of the tasks of `refs` still there; the rest are said to be
    /// gone, a refusal.
    fn still_there(&mut self, refs: &[Ref]) -> Vec<u32> {
        let (there, gone_ones): (Vec<Ref>, Vec<Ref>) =
            refs.iter().partition(|&&r| self.find(r).is_some());
        if !gone_ones.is_empty() {
            let ids: Vec<u32> = gone_ones.iter().map(|r| r.id).collect();
            self.message = Some(gone(&ids));
        }
        there.iter().map(|r| r.id).collect()
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

    /// Picks the task `at` names again, if it's still in column `c`.
    fn repick(&mut self, at: Ref, c: usize) {
        if self.find(at).is_some_and(|i| self.column(c).contains(&i)) {
            self.pick(at.id);
        }
    }

    /// The picked card or sub item, as the board holds it.
    fn picked_ref(&self) -> Option<Ref> {
        self.picked().map(|i| self.held(i))
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
        let picked = self.picked_ref();
        self.tasks = tasks;
        self.keep_marks_in_sight();
        if let Some(at) = picked {
            self.repick(at, self.col);
        }
        self.clamp();
    }

    /// Drops the marks on tasks that are gone (a task that took one's id
    /// meanwhile has another stamp), and on sub items whose card is done:
    /// folded into its count, out of sight, they're no rows for a key to act
    /// on.
    fn keep_marks_in_sight(&mut self) {
        let parents = task::parents(&self.tasks);
        let in_sight: BTreeSet<Ref> = (0..self.tasks.len())
            .filter(|&i| parents[i].is_none_or(|p| self.tasks[p].status != Status::Done))
            .map(|i| self.held(i))
            .collect();
        self.marked
            .retain(|&id, &mut stamp| in_sight.contains(&Ref { id, stamp }));
    }

    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Move { at, to, follow } => {
                if let Some(i) = self.find(at) {
                    self.tasks[i].status = to;
                    if to == Status::Done {
                        self.finish_sub_items(&[at.id]);
                    }
                    if follow {
                        self.pick(at.id);
                    }
                } else {
                    self.message = Some(gone(&[at.id]));
                }
            }
            Op::Send { refs, to } => {
                let picked = self.picked_ref();
                let ids = self.still_there(&refs);
                let parents = task::parents(&self.tasks);
                for (task, parent) in self.tasks.iter_mut().zip(parents) {
                    if parent.is_none() && ids.contains(&task.id) {
                        task.status = to;
                    }
                }
                // As `d`: the pick stays on its row, the next card under it.
                if let Some(at) = picked {
                    self.repick(at, self.col);
                }
            }
            Op::Remove {
                asked,
                with,
                marked,
            } => {
                let ids = self.still_there(&asked);
                // Its sub items go with a card that's still there; one gone
                // meanwhile goes unsaid, as asked.
                let with: Vec<u32> = with
                    .iter()
                    .filter_map(|&r| self.find(r))
                    .filter(|&i| self.tasks[i].parent.is_some_and(|p| ids.contains(&p)))
                    .map(|i| self.tasks[i].id)
                    .collect();
                self.tasks
                    .retain(|t| !ids.contains(&t.id) && !with.contains(&t.id));
                if !ids.is_empty() {
                    let what = match marked {
                        true => format!("{} marked", ids.len()),
                        false => ids[0].to_string(),
                    };
                    let subs = with_sub_items(with.len(), ids.len());
                    self.note = Some(format!("Removed {what}{subs}."));
                }
                // The pick stays on its row, clamped below.
            }
            Op::Shift { refs, by } => {
                let ids = self.still_there(&refs);
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
            Op::Finish { refs } => {
                let ids = self.still_there(&refs);
                let picked = self.picked_ref();
                for task in self.tasks.iter_mut().filter(|t| ids.contains(&t.id)) {
                    task.status = Status::Done;
                }
                self.finish_sub_items(&ids);
                if let Some(at) = picked {
                    self.repick(at, self.col);
                }
            }
            Op::Edit { refs, change } => {
                let ids = self.still_there(&refs);
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
                // A card that took its id meanwhile is another card: it's gone
                // just the same.
                let parent = match parent {
                    Some(p) if self.find(p).is_none() => {
                        let p = p.id;
                        self.note = Some(format!("{p} is gone: added as a task of its own."));
                        None
                    }
                    parent => parent.map(|p| p.id),
                };
                let id = task::next_id(&self.tasks);
                let stamp = task::new_stamp(&self.tasks, task::now_ms());
                self.tasks.push(Task {
                    id,
                    title: parsed.title,
                    status: Status::Todo,
                    priority: parsed.priority,
                    tags: parsed.tags,
                    due: parsed.due,
                    parent,
                    stamp,
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

    /// What a key asks. A Hangul letter is read as the keys it sits on: the
    /// first is done here, and the rest wait for [`Board::next_queued`].
    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        self.message = None;
        self.note = None;
        self.hint = None;
        self.queued.clear();
        self.step(key, None)
    }

    /// The next key a Hangul letter was read as, once the change of the one
    /// before it is made; `None` when there's none left.
    pub fn next_queued(&mut self) -> Option<Outcome> {
        let (key, jamo) = self.queued.pop_front()?;
        Some(self.step(key, Some(jamo)))
    }

    /// One key, `jamo` the Hangul letter it was read from, if it was.
    fn step(&mut self, key: KeyEvent, jamo: Option<char>) -> Outcome {
        if let Some(input) = self.input.as_mut() {
            // Where words are typed, Hangul is words: what's left of a letter
            // whose first key opened the input goes in as it was typed.
            if let Some(jamo) = jamo {
                input.text.insert(input.cursor, jamo);
                input.cursor += jamo.len_utf8();
                return Outcome::Nothing;
            }
            return self.input_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let KeyCode::Char(c) = key.code
            && !ctrl
            && let Some(keys) = hangul::keys(c)
        {
            if !self.hangul_said {
                self.hangul_said = true;
                let read: Vec<String> = keys.iter().map(|(k, _)| k.to_string()).collect();
                self.hint = Some(format!("{c} read as {}.", read.join(" ")));
            }
            let mut keys = keys
                .into_iter()
                .map(|(k, j)| (KeyEvent::from(KeyCode::Char(k)), j));
            let (first, jamo) = keys.next().expect("a letter is a key at least");
            self.queued.extend(keys);
            return self.step(first, Some(jamo));
        }
        if let Some(removal) = self.asking.take() {
            return self.answer(removal, key);
        }
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
                    let Ref { id, stamp } = self.held(i);
                    if self.marked.remove(&id) != Some(stamp) {
                        self.marked.insert(id, stamp);
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
            KeyCode::Char('s') => return self.send(Status::Doing),
            KeyCode::Char('t') => return self.send(Status::Todo),
            KeyCode::Char('d') => return self.finish(),
            KeyCode::Char('x') | KeyCode::Delete => self.ask_removal(),
            KeyCode::Char('y') => return self.copy(),
            KeyCode::Char('a') => self.input = Some(Input::new(InputKind::Add, String::new())),
            KeyCode::Char('A') => {
                if let Some(i) = self.picked() {
                    let card = task::parents(&self.tasks)[i].unwrap_or(i);
                    if task::is_sub_item(&self.tasks, &self.tasks[card]) {
                        let id = self.tasks[card].id;
                        self.message = Some(format!("{id} is a sub item: they go one level deep"));
                    } else {
                        let kind = InputKind::Sub(self.held(card));
                        self.input = Some(Input::new(kind, String::new()));
                    }
                }
            }
            KeyCode::Char('e') => {
                if !self.marked.is_empty() {
                    self.input = Some(Input::new(InputKind::EditMarked, String::new()));
                } else if let Some(i) = self.picked() {
                    let kind = InputKind::Edit(self.held(i));
                    self.input = Some(Input::new(kind, self.tasks[i].title.clone()));
                }
            }
            _ => {}
        }
        Outcome::Nothing
    }

    /// `y`: the picked row's title to copy, or each marked row's, one a line,
    /// in the board's order (by column, then row).
    fn copy(&mut self) -> Outcome {
        let titles: Vec<&str> = match self.marked.is_empty() {
            true => self
                .picked()
                .map(|i| self.tasks[i].title.as_str())
                .into_iter()
                .collect(),
            false => (0..COLUMNS.len())
                .flat_map(|c| self.column(c))
                .filter(|&i| self.is_marked(i))
                .map(|i| self.tasks[i].title.as_str())
                .collect(),
        };
        let what = match titles[..] {
            [] => return Outcome::Nothing,
            [title] => format!("\"{title}\""),
            _ => format!("{} titles", titles.len()),
        };
        Outcome::Copy {
            text: titles.join("\n"),
            what,
        }
    }

    /// A copy command's result for `what` it copied: a note when it ended
    /// well, a refusal when it didn't.
    pub fn copied(&mut self, what: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self.note = Some(format!("Copied {what}.")),
            Err(e) => self.message = Some(format!("couldn't copy {what}: {e}")),
        }
    }

    /// The clipboard sequence sent for `what`: whether the terminal took it,
    /// halil can't know.
    pub fn sent(&mut self, what: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self.note = Some(format!("Sent {what} to the terminal's clipboard.")),
            Err(e) => self.message = Some(format!("couldn't copy {what}: {e}")),
        }
    }

    /// `x`: asks, in the footer, to remove the marked, or else the picked
    /// card with its sub items, or the picked sub item alone.
    fn ask_removal(&mut self) {
        let (asked, marked): (Vec<usize>, bool) = match self.picked() {
            _ if !self.marked.is_empty() => (
                (0..self.tasks.len())
                    .filter(|&i| self.is_marked(i))
                    .collect(),
                true,
            ),
            Some(i) => (vec![i], false),
            None => return,
        };
        // A sub item marked under a marked card is counted once, as marked.
        let parents = task::parents(&self.tasks);
        let with: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| !asked.contains(&i) && parents[i].is_some_and(|p| asked.contains(&p)))
            .collect();
        self.asking = Some(Removal {
            asked: asked.iter().map(|&i| self.held(i)).collect(),
            with: with.iter().map(|&i| self.held(i)).collect(),
            marked,
            title: (!marked).then(|| self.tasks[asked[0]].title.clone()),
        });
    }

    /// The key after `x`: `y` or enter removes, any other keeps.
    fn answer(&mut self, removal: Removal, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter if !ctrl => {
                if removal.marked {
                    self.marked.clear();
                }
                Outcome::Change(Op::Remove {
                    asked: removal.asked,
                    with: removal.with,
                    marked: removal.marked,
                })
            }
            KeyCode::Char('c') if ctrl => Outcome::Quit,
            _ => {
                self.note = Some("Nothing was removed.".into());
                Outcome::Nothing
            }
        }
    }

    /// `s` and `t`: sends the marked cards, or else the picked one, to Doing
    /// or Todo. A sub item moves with its card only, as for `h` and `l`.
    fn send(&mut self, to: Status) -> Outcome {
        const WITH_ITS_TASK: &str = "a sub item moves with its task";
        let parents = task::parents(&self.tasks);
        let column = status_name(to);
        if !self.marked.is_empty() {
            let marked = |i: usize| self.is_marked(i);
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
            let going: Vec<Ref> = cards
                .iter()
                .filter(|&&i| self.tasks[i].status != to)
                .map(|&i| self.held(i))
                .collect();
            if going.is_empty() {
                // Short, to fit beside a sub item's staying: `2 are in Todo
                // already.`, or one card by its id, as when it's picked.
                self.note = match cards[..] {
                    [] => None,
                    [i] => Some(format!("{} is in {column} already.", self.tasks[i].id)),
                    _ => Some(format!("{} are in {column} already.", cards.len())),
                };
                return Outcome::Nothing;
            }
            return Outcome::Change(Op::Send { refs: going, to });
        }
        match self.picked() {
            Some(i) if parents[i].is_some() => {
                self.message = Some(WITH_ITS_TASK.into());
                Outcome::Nothing
            }
            Some(i) if self.tasks[i].status == to => {
                let id = self.tasks[i].id;
                self.note = Some(format!("{id} is in {column} already."));
                Outcome::Nothing
            }
            Some(i) => Outcome::Change(Op::Send {
                refs: vec![self.held(i)],
                to,
            }),
            None => Outcome::Nothing,
        }
    }

    /// Moves the marked cards, or else the picked one, a column left (-1) or
    /// right (1). A sub item moves with its card only: marked, it stays, and
    /// the message says how many did.
    fn shift(&mut self, by: isize) -> Outcome {
        const WITH_ITS_TASK: &str = "a sub item moves with its task";
        let parents = task::parents(&self.tasks);
        if !self.marked.is_empty() {
            let marked = |i: usize| self.is_marked(i);
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
                    let mut refs: Vec<Ref> = cards.iter().map(|&i| self.held(i)).collect();
                    refs.sort_unstable();
                    Outcome::Change(Op::Shift { refs, by })
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
                at: self.held(i),
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
        let refs: Vec<Ref> = match self.picked() {
            _ if !self.marked.is_empty() => {
                let refs = self.marked_refs();
                self.marked.clear();
                refs
            }
            Some(i) if self.is_sub(i) => {
                let to = match self.tasks[i].status {
                    Status::Done => Status::Todo,
                    _ => Status::Done,
                };
                return Outcome::Change(Op::Move {
                    at: self.held(i),
                    to,
                    follow: true,
                });
            }
            Some(i) => vec![self.held(i)],
            None => return Outcome::Nothing,
        };
        let ids: Vec<u32> = refs.iter().map(|r| r.id).collect();
        match self.finishing(&ids).is_empty() {
            false => Outcome::Change(Op::Finish { refs }),
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

        let refs: Vec<Ref> = match kind {
            InputKind::Edit(at) => vec![at],
            _ => self.marked_refs(),
        };
        // The task went while its words were typed (a `halil rm` elsewhere,
        // maybe with another task in its id since): nothing to change.
        if let InputKind::Edit(at) = kind
            && self.find(at).is_none()
        {
            self.input = None;
            self.message = Some(gone(&[at.id]));
            return Outcome::Nothing;
        }
        let tasks: Vec<&Task> = refs
            .iter()
            .filter_map(|&r| self.find(r))
            .map(|i| &self.tasks[i])
            .collect();
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
            InputKind::Edit(Ref { id, .. }) => {
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
            true => Outcome::Change(Op::Edit { refs, change }),
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
        let x = |c: usize| MARGIN + c * (col_w + GAP);
        let edge = x(COLUMNS.len() - 1) + col_w;
        // Each column's rows, drawn: the picked one, shown whole, may run
        // over the columns to its right, or take more than a line, and the
        // column scrolls to show it; the rest are one line each.
        let mut cells: Vec<Vec<Row>> = Vec::new();
        for (c, rows) in columns.iter().enumerate() {
            let drawn = |n: usize| {
                let i = rows[n];
                let look = Look {
                    width: col_w,
                    room: edge - x(c),
                    picked: c == self.col && n == self.row[c],
                    marked: self.is_marked(i),
                    today: self.today,
                };
                if parents[i].is_some() {
                    sub_item(&self.tasks[i], look)
                } else {
                    let subs = task::sub_items(&self.tasks, &parents, i);
                    let count = task::progress(&self.tasks, &subs);
                    card(&self.tasks[i], count.as_deref(), look)
                }
            };
            let pick = self.row[c];
            let tall = if pick < rows.len() {
                drawn(pick).len()
            } else {
                1
            };
            let mut s = self.scroll[c].min(pick);
            s = s.max((pick + tall).saturating_sub(body_h)).min(pick);
            s = s.min((rows.len() + tall - 1).saturating_sub(body_h));
            let mut lines = Vec::new();
            for n in s..rows.len() {
                if lines.len() >= body_h {
                    break;
                }
                lines.extend(drawn(n));
            }
            lines.truncate(body_h);
            self.scroll[c] = s;
            cells.push(lines);
        }

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
                let mut cell = Row::default();
                if let Some(line) = cells[c].get(r) {
                    cell = line.clone();
                } else if r == 0 && cards.is_empty() {
                    cell.push(
                        "   Nothing here",
                        Style {
                            italic: true,
                            ..Style::fg(QUIETER)
                        },
                    );
                }
                // A neighbour's row is whole or not there: where a picked row
                // shown whole reaches into this column, its row on this line
                // isn't drawn, never cut to what's past the ground.
                if row.width <= x(c) {
                    row.pad_to(x(c), Style::default());
                    row.append(&cell);
                }
            }
            lines.push(row.spans);
        }

        // A refusal in red, then a note of what was done in the terminal's
        // own colour, three cells after it, and how a Hangul letter was read.
        let mut message = Row::default();
        let gap = |message: &Row| if message.width == 0 { 2 } else { 3 };
        let hint = self.hint.as_ref().map(|read| {
            let mut taken = Row::default();
            for said in [&self.message, &self.note].into_iter().flatten() {
                taken.pad_to(taken.width + gap(&taken), Style::default());
                taken.push(said, Style::default());
            }
            let room = w.saturating_sub(taken.width + gap(&taken));
            let long = format!("{read} {}", HANGUL_LATE[0]);
            match long.width() <= room {
                true => long,
                false => format!("{read} {}", HANGUL_LATE[1]),
            }
        });
        for (said, style) in [
            (&self.message, Style::fg(RED)),
            (&self.note, Style::default()),
            (&hint, Style::default()),
        ] {
            let gap = gap(&message);
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
                    InputKind::Sub(at) => (
                        format!("Sub item of {}", at.id),
                        "enter to add · esc to cancel",
                        "title friday +tag !high",
                    ),
                    InputKind::Edit(at) => (
                        format!("Edit {}", at.id),
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
            None if self.asking.is_some() => {
                let removal = self.asking.as_ref().expect("asking");
                footer.push("  ", Style::default());
                // The title is cut to fit, or left out when little of it
                // would; then the answers' keys give way; then it's cut.
                let answers = "   y remove · esc keep".width();
                let room = w.saturating_sub(footer.width + answers);
                let question = match &removal.title {
                    Some(title) => {
                        let bare = removal.question(Some("")).width();
                        match room.checked_sub(bare) {
                            Some(left) if left >= title.width().min(6) => {
                                removal.question(Some(&truncate(title, left)))
                            }
                            _ => removal.question(None),
                        }
                    }
                    None => removal.question(None),
                };
                footer.push(
                    &truncate(&question, w.saturating_sub(footer.width)),
                    Style::default(),
                );
                if footer.width + answers <= w {
                    footer.push("   ", Style::default());
                    footer.push("y", Style::fg(QUIET));
                    footer.push(" remove · ", Style::fg(QUIETER));
                    footer.push("esc", Style::fg(QUIET));
                    footer.push(" keep", Style::fg(QUIETER));
                }
            }
            None if !self.marked.is_empty() => {
                footer.push(
                    &truncate(&format!("  {} marked", self.marked.len()), w),
                    Style::default(),
                );
                // `esc clear` gives way first, then `t`, `x` and `s`, then
                // `e`, then `h l`; `d` last.
                let hints = [
                    ("h l", "move", Some(6)),
                    ("t", "todo", Some(2)),
                    ("s", "doing", Some(4)),
                    ("d", "done", None),
                    ("e", "edit", Some(5)),
                    ("x", "remove", Some(3)),
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
                // quits too), then `j k`, then `t`, `x` and `s`, and only then
                // the rest. `d` goes last, and its word for what it does to
                // the picked row with it.
                let sub = self.picked().is_some_and(|i| parents[i].is_some());
                footer.push(" ", Style::default());
                let mut hints = vec![
                    ("j k", "pick", Some(4)),
                    ("space", "mark", Some(9)),
                    ("h l", "move", Some(12)),
                    ("t", "todo", Some(5)),
                    ("s", "doing", Some(7)),
                    ("d", if sub { "done/open" } else { "done" }, None),
                    ("tab", "column", Some(1)),
                    ("a", "add", Some(11)),
                    ("A", "sub", Some(8)),
                    ("e", "edit", Some(10)),
                    ("y", "copy", Some(2)),
                    ("x", "remove", Some(6)),
                    ("q", "quit", Some(3)),
                ];
                // Only the keys that act on what's picked: not the key to the
                // column it's in (in Done, `d` moves it back to Todo, as `t`
                // says), and none that moves it on a sub item, which moves
                // with its card only.
                hints.retain(|&(keys, _, _)| match keys {
                    "h l" | "t" | "s" if sub => false,
                    "t" => COLUMNS[self.col] != Status::Todo,
                    "s" => COLUMNS[self.col] != Status::Doing,
                    "d" => COLUMNS[self.col] != Status::Done,
                    _ => true,
                });
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
    let width = |i: usize| {
        let (keys, what, _) = hints[i];
        extra + keys.width() + what.width()
    };
    let mut shown = vec![true; hints.len()];
    let used = |shown: &[bool]| {
        (0..hints.len())
            .filter(|&i| shown[i])
            .map(width)
            .sum::<usize>()
    };
    let mut gone = Vec::new();
    while used(&shown) > room {
        let Some(next) = (0..hints.len())
            .filter(|&i| shown[i] && hints[i].2.is_some())
            .min_by_key(|&i| hints[i].2)
        else {
            break;
        };
        shown[next] = false;
        gone.push(next);
    }
    // The room left goes back to those gone, the last to go first: one that
    // doesn't fit is skipped, and the next tried.
    for &i in gone.iter().rev() {
        if used(&shown) + width(i) <= room {
            shown[i] = true;
        }
    }
    // Those that never give way, when even they don't all fit: each that does.
    let mut taken = 0;
    (0..hints.len())
        .filter(|&i| shown[i])
        .filter(|&i| {
            let fits = taken + width(i) <= room;
            if fits {
                taken += width(i);
            }
            fits
        })
        .map(|i| (hints[i].0, hints[i].1))
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

/// How a card or sub item is drawn: how wide its column, and how wide the
/// room from it to the board's edge (a picked row shown whole may take it),
/// picked or marked, and the day its due date is read from.
#[derive(Clone, Copy)]
struct Look {
    width: usize,
    room: usize,
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

/// The width of a due date's words with the two spaces before them, or 0.
fn due_width(due: &Option<(String, bool)>) -> usize {
    due.as_ref().map_or(0, |(words, _)| words.width() + 2)
}

/// A due date that didn't fit at the right of the first row: at the right of
/// the last, two cells after what's there, when it fits; else on a row of its
/// own, at the right, under the rest of the picked row.
fn place_late_due(
    rows: &mut Vec<Row>,
    due: (String, bool),
    width: usize,
    plain: Style,
    quiet: Style,
) {
    let last = rows.last_mut().expect("a row grown has its first line");
    let words_w = due.0.width();
    if last.width + 2 + words_w < width {
        last.pad_to(width - 1 - words_w - 2, plain);
        push_due(last, Some(due), plain, quiet);
        return;
    }
    let words = truncate(&due.0, width.saturating_sub(3));
    let mut row = Row::default();
    row.pad_to(width.saturating_sub(words.width() + 3), plain);
    push_due(&mut row, Some((words, due.1)), plain, quiet);
    rows.push(row);
}

/// A task's due date in words, and whether it's late, while it's open.
fn due_words(task: &Task, today: Date) -> Option<(String, bool)> {
    due_in_days(task, today).map(|days| (due_in_words(days), days < 0))
}

/// A card's first cells: a `▌` when it's marked, its priority mark, a space.
fn card_head(row: &mut Row, task: &Task, marked: bool, plain: Style) {
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
}

/// One card, as its lines: a `▌` when it's marked, its priority mark, title,
/// its sub items' `count` quiet after it, tags dimmed beside them and its due
/// date at the right. When room is short the tags give way first, then the
/// due date; the count stays. Picked, when any of it was cut or left out, it's
/// shown whole instead (see [`whole_row`]).
fn card(task: &Task, count: Option<&str>, look: Look) -> Vec<Row> {
    let Look {
        width,
        picked,
        marked,
        today,
        ..
    } = look;
    let (plain, title, quiet) = styles(picked);

    // Room inside the card: a space of padding at each end, the mark and its space.
    let inner = width.saturating_sub(4);
    let count_w = count.map_or(0, |c| c.width() + 2);
    let due = due_fitting(task, today, inner, count_w);
    let room = inner - due_width(&due);
    let tags = (!task.tags.is_empty()).then(|| {
        let tags: Vec<String> = task.tags.iter().map(|t| format!("+{t}")).collect();
        tags.join(" ")
    });

    let shown = truncate(&task.title, room.saturating_sub(count_w));
    let used = shown.width() + count_w;
    let whole = shown == task.title
        && tags.as_ref().is_none_or(|t| used + 2 + t.width() <= room)
        && (due.is_some() || due_in_days(task, today).is_none());
    if picked && !whole {
        let mut head = Row::default();
        card_head(&mut head, task, marked, plain);
        let whole = Whole {
            title: &task.title,
            title_style: title,
            count,
            tags,
            due: due_words(task, today),
        };
        return whole_row(head, whole, look, 4, quiet);
    }

    let mut row = Row::default();
    card_head(&mut row, task, marked, plain);
    row.push(&shown, title);
    if let Some(count) = count {
        row.push("  ", plain);
        row.push(count, quiet);
    }
    if let Some(tags) = tags
        && room >= used + 2 + 3
    {
        row.push("  ", plain);
        row.push(&truncate(&tags, room - used - 2), quiet);
    }
    row.pad_to(3 + room, plain);
    push_due(&mut row, due, plain, quiet);
    row.pad_to(width, plain);
    vec![row]
}

/// What a picked row shows whole, besides its head.
struct Whole<'a> {
    title: &'a str,
    title_style: Style,
    count: Option<&'a str>,
    tags: Option<String>,
    due: Option<(String, bool)>,
}

/// The picked row whole. On one line when it fits in the look's `room`, the
/// width from where the row starts to the board's edge: `head` (the mark and
/// priority, or the glyph), the title, the count, the tags and the due date,
/// two cells between each, and the picked ground one cell past them, over the
/// columns to its right (or to its column's end, when that's further). Else
/// wrapped in that room (see [`grow`]), `pad` of it being the head and the
/// cell at the end.
fn whole_row(head: Row, whole: Whole, look: Look, pad: usize, quiet: Style) -> Vec<Row> {
    let room = look.room;
    let plain = styles(true).0;
    let beside = |s: &Option<String>| s.as_ref().map_or(0, |s| s.width() + 2);
    let line = head.width
        + whole.title.width()
        + beside(&whole.count.map(String::from))
        + beside(&whole.tags)
        + due_width(&whole.due);
    if line < room {
        let mut row = head;
        row.push(whole.title, whole.title_style);
        for said in [whole.count.map(String::from), whole.tags]
            .into_iter()
            .flatten()
        {
            row.push("  ", plain);
            row.push(&said, quiet);
        }
        push_due(&mut row, whole.due, plain, quiet);
        row.push(" ", plain);
        // Short enough for its column (its date was left out there only to
        // keep room for the title): the ground to the column's end, as ever.
        row.pad_to(look.width, plain);
        return vec![row];
    }
    grow(head, room.saturating_sub(pad), room, whole, plain, quiet)
}

/// A picked card or sub item whole, over as many lines as it takes, `width`
/// wide with `inner` of it for words. The first line is `head` (the mark and
/// priority, or the glyph), the title and the count, and keeps a due date
/// that fits there beside 8 cells of title, as on one line; the title wraps by
/// words under itself, a word longer than the room broken; the tags follow
/// the title's last line when they fit there, else take lines of their own; a
/// due date with no room on the first line goes at the right of the last, or
/// else on a line of its own.
fn grow(
    head: Row,
    inner: usize,
    width: usize,
    whole: Whole,
    plain: Style,
    quiet: Style,
) -> Vec<Row> {
    let indent = head.width;
    let count_w = whole.count.map_or(0, |c| c.width() + 2);
    let (first_due, last_due) = match whole.due {
        Some((words, late)) if words.width() + 2 + count_w + 8 <= inner => {
            (Some((words, late)), None)
        }
        due => (None, due),
    };
    let room = inner - due_width(&first_due);
    let lines = wrap(whole.title, room.saturating_sub(count_w), inner);

    let last = lines.len() - 1;
    let tags_after = whole.tags.as_ref().is_some_and(|t| {
        let (used, room) = match last {
            0 => (lines[0].width() + count_w, room),
            _ => (lines[last].width(), inner),
        };
        used + 2 + t.width() <= room
    });
    let mut rows = Vec::new();
    for (k, line) in lines.iter().enumerate() {
        let mut row = match k {
            0 => head.clone(),
            _ => Row::default(),
        };
        row.pad_to(indent, plain);
        row.push(line, whole.title_style);
        if k == 0
            && let Some(count) = whole.count
        {
            row.push("  ", plain);
            row.push(count, quiet);
        }
        if k == last
            && tags_after
            && let Some(tags) = &whole.tags
        {
            row.push("  ", plain);
            row.push(tags, quiet);
        }
        if k == 0 && first_due.is_some() {
            row.pad_to(indent + room, plain);
            push_due(&mut row, first_due.clone(), plain, quiet);
        }
        rows.push(row);
    }
    if let Some(tags) = whole.tags.filter(|_| !tags_after) {
        for line in wrap(&tags, inner, inner) {
            let mut row = Row::default();
            row.pad_to(indent, plain);
            row.push(&line, quiet);
            rows.push(row);
        }
    }
    if let Some(due) = last_due {
        place_late_due(&mut rows, due, width, plain, quiet);
    }
    for row in &mut rows {
        row.pad_to(width, plain);
    }
    rows
}

/// Where a sub item's `◦` goes: two cells further in than a card's title.
const SUB_INDENT: usize = 5;

/// One sub item, under its card, as its lines: a `▌` when it's marked, `◦
/// title` while it's open or `● title` dimmed once it's done, and its due date
/// at the right when it fits. Picked, it shows its tags too, after the title;
/// when they don't fit there, or its title was cut or its date left out, it's
/// shown whole, as a card is, from its own indent.
fn sub_item(task: &Task, look: Look) -> Vec<Row> {
    let Look {
        width,
        picked,
        marked,
        today,
        ..
    } = look;
    let (plain, title, quiet) = styles(picked);
    let done = task.status == Status::Done;
    let title = if done { quiet } else { title };

    let mut head = Row::default();
    bar(&mut head, marked, plain);
    head.pad_to(SUB_INDENT, plain);
    head.push(if done { "●" } else { "◦" }, quiet);
    head.push(" ", plain);

    // Room after the glyph and its space, a space of padding at the end.
    let inner = width.saturating_sub(SUB_INDENT + 3);
    let due = due_fitting(task, today, inner, 0);
    let room = inner - due_width(&due);
    let shown = truncate(&task.title, room);
    let tags = (picked && !task.tags.is_empty()).then(|| {
        let tags: Vec<String> = task.tags.iter().map(|t| format!("+{t}")).collect();
        tags.join(" ")
    });
    let whole = shown == task.title
        && tags
            .as_ref()
            .is_none_or(|t| shown.width() + 2 + t.width() <= room)
        && (due.is_some() || due_in_days(task, today).is_none());
    if picked && !whole {
        let whole = Whole {
            title: &task.title,
            title_style: title,
            count: None,
            tags,
            due: due_words(task, today),
        };
        return whole_row(head, whole, look, SUB_INDENT + 3, quiet);
    }

    let mut row = head;
    row.push(&shown, title);
    if let Some(tags) = tags {
        row.push("  ", plain);
        row.push(&tags, quiet);
    }
    row.pad_to(SUB_INDENT + 2 + room, plain);
    push_due(&mut row, due, plain, quiet);
    row.pad_to(width, plain);
    vec![row]
}

/// `s` in lines by words, the first `first` columns wide and the rest `rest`
/// wide. A word goes to the next line when it doesn't fit on this one; one
/// longer than its line is broken, as it is when it doesn't fit the first.
fn wrap(s: &str, first: usize, rest: usize) -> Vec<String> {
    let room = |line: usize| if line == 0 { first } else { rest };
    let mut lines = vec![String::new()];
    let mut width = 0;
    for word in s.split_whitespace() {
        let word_w = word.width();
        let at = lines.len() - 1;
        if width > 0 && width + 1 + word_w <= room(at) {
            lines[at].push(' ');
            lines[at] += word;
            width += 1 + word_w;
            continue;
        }
        if width > 0 {
            lines.push(String::new());
            width = 0;
        }
        for ch in word.chars() {
            let w = ch.width().unwrap_or(0);
            if width + w > room(lines.len() - 1) && (width > 0 || lines.len() == 1) {
                lines.push(String::new());
                width = 0;
            }
            lines.last_mut().expect("a line").push(ch);
            width += w;
        }
    }
    lines
}

/// `s` cut to `max` columns, an ellipsis marking the cut, right after the
/// last letter kept.
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
    out.truncate(out.trim_end().len());
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

/// What the input is for: a new task, a new sub item of a card, the picked
/// card or sub item, or the marked ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Add,
    Sub(Ref),
    Edit(Ref),
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
#[derive(Default, Clone)]
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

    fn append(&mut self, other: &Row) {
        self.spans.extend(other.spans.iter().cloned());
        self.width += other.width;
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
            stamp: 0,
        }
    }

    /// Task `id` as the board holds it: the tests' tasks have no stamps.
    fn r(id: u32) -> Ref {
        Ref { id, stamp: 0 }
    }

    fn refs(ids: &[u32]) -> Vec<Ref> {
        ids.iter().map(|&id| r(id)).collect()
    }

    /// The ids marked.
    fn marks(board: &Board) -> BTreeSet<u32> {
        board.marked.keys().copied().collect()
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

    /// Presses the keys, a Hangul letter's keys each in turn: what each asked.
    fn press(board: &mut Board, keys: &str) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        for c in keys.chars() {
            outcomes.push(board.key(KeyEvent::from(KeyCode::Char(c))));
            while let Some(next) = board.next_queued() {
                outcomes.push(next);
            }
        }
        outcomes
    }

    /// Presses the keys, making each change as the loop does, before the next
    /// key, a Hangul letter's too.
    fn run_keys(board: &mut Board, keys: &str) {
        for c in keys.chars() {
            let mut outcome = board.key(KeyEvent::from(KeyCode::Char(c)));
            loop {
                if let Outcome::Change(op) = outcome {
                    board.apply(op);
                }
                match board.next_queued() {
                    Some(next) => outcome = next,
                    None => break,
                }
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
        let (lines, _) = board.draw(100, 12);
        let highlighted: Vec<&Span> = lines[3]
            .iter()
            .filter(|s| s.style.bg == Some(HIGHLIGHT))
            .collect();
        let width: usize = highlighted.iter().map(|s| s.text.width()).sum();
        assert_eq!(
            width,
            (100 - 2 * MARGIN - 2 * GAP) / 3,
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
        // Not picked: the picked row shows whole.
        board.key(KeyEvent::from(KeyCode::Tab));
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
        assert_eq!(marks(&board), BTreeSet::from([2]));
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, " ");
        assert_eq!(marks(&board), BTreeSet::from([1, 2]));
        assert_eq!(picked_id(&board), Some(1), "it stays on the last row");
        press(&mut board, " ");
        assert_eq!(marks(&board), BTreeSet::from([2]));
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
        assert_eq!(marks(&board), BTreeSet::from([2, 3]));
        board.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(marks(&board), BTreeSet::from([2, 3]));
        // Meanwhile another `halil` deleted fix bike.
        let fresh = sample().tasks.into_iter().filter(|t| t.id != 3).collect();
        board.replace_tasks(fresh);
        assert_eq!(marks(&board), BTreeSet::from([2]));
        let (lines, _) = board.draw(100, 12);
        let footer = text(&lines[11]);
        assert!(footer.trim_start().starts_with("1 marked"), "{footer:?}");
    }

    #[test]
    fn marks_on_sub_items_drop_when_their_card_goes_to_done() {
        // Pay rent (2), its sub item 6 and buy milk (1) marked.
        let mut board = sub_sample();
        press(&mut board, " j j ");
        assert_eq!(marks(&board), BTreeSet::from([1, 2, 6]));
        run_keys(&mut board, "l");
        assert_eq!(
            marks(&board),
            BTreeSet::from([1, 2, 6]),
            "in Doing, 6 is in sight under its card"
        );
        run_keys(&mut board, "l");
        assert_eq!(
            marks(&board),
            BTreeSet::from([1, 2]),
            "in Done, 6 is folded into the count: no row for d to act on"
        );
        let (lines, _) = board.draw(100, 14);
        assert!(text(&lines[13]).trim_start().starts_with("2 marked"));

        // Meanwhile a `halil done 2` elsewhere: the reload drops 6's mark.
        let mut board = sub_sample();
        press(&mut board, "jj ");
        assert_eq!(marks(&board), BTreeSet::from([6]));
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
                refs: refs(&[2, 3]),
                by: 1
            }
        );
        // Meanwhile another `halil` moved pay rent to Doing: it goes on from there.
        let mut fresh = sample().tasks;
        fresh[1].status = Status::Doing;
        board.replace_tasks(fresh);
        board.apply(std::mem::replace(op, Op::Finish { refs: vec![] }));
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
            [Outcome::Change(Op::Finish {
                refs: refs(&[2, 3])
            })]
        );
    }

    #[test]
    fn marks_stay_after_h_and_l_and_clear_after_d() {
        let mut board = marked_sample();
        run_keys(&mut board, "ll");
        assert_eq!(status(&board, 2), Status::Done);
        assert_eq!(status(&board, 3), Status::Done);
        assert_eq!(marks(&board), BTreeSet::from([2, 3]));
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
        assert_eq!(marks(&board), BTreeSet::from([1, 2, 3]));
        let (lines, _) = board.draw(100, 12);
        assert_eq!(
            text(&lines[11]).trim_start(),
            "3 marked   h l move   t todo   s doing   d done   e edit   x remove   esc clear"
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
        board.key(KeyEvent::from(KeyCode::Tab));
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
        // Pay rent not picked: picked, its tags would take a line of their own.
        board.key(KeyEvent::from(KeyCode::Tab));
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
        board.key(KeyEvent::from(KeyCode::BackTab));
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
            // Not picked: one line, cut to fit.
            board.col = 1;
            let (lines, _) = board.draw(w, 12);
            assert_eq!(cell(&lines[3], w, 0), want, "{w} wide");
            // Picked: its column wide when it fits there, else whole on
            // the line, `ship 0.2.0  2/4  +work  tomorrow` (35) and a cell.
            board.col = 0;
            let (lines, _) = board.draw(w, 12);
            let col_w = (w - 2 * MARGIN - 2 * GAP) / 3;
            assert_eq!(lit(&lines[3]), col_w.max(36), "{w} wide");
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
                at: r(5),
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
        board.apply(std::mem::replace(op, Op::Finish { refs: vec![] }));
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
        assert_eq!(marks(&board), BTreeSet::from([6]));
        assert_eq!(picked_id(&board), Some(7), "space steps a row");
        press(&mut board, "j ");
        assert_eq!(marks(&board), BTreeSet::from([1, 6]));
        assert_eq!(
            press(&mut board, "l"),
            [Outcome::Change(Op::Shift {
                refs: refs(&[1]),
                by: 1
            })]
        );
        assert_eq!(
            board.message.as_deref(),
            Some("1 sub item stayed: a sub item moves with its task")
        );
        assert_eq!(
            press(&mut board, "d"),
            [Outcome::Change(Op::Finish {
                refs: refs(&[1, 6])
            })]
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
        assert_eq!(marks(&board), BTreeSet::from([2, 5]));
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
        board.apply(std::mem::replace(op, Op::Finish { refs: vec![] }));
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
        assert_eq!(marks(&board), BTreeSet::from([2, 3]));
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
        assert_eq!(marks(&board), BTreeSet::from([3, 6]));
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
            Some(InputKind::Sub(r(2)))
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
        let new_two = board.held(board.index(2).unwrap());
        assert!(new_two.stamp > 0, "stamped when made");
        assert_eq!(
            board.input.as_ref().map(|i| i.kind),
            Some(InputKind::Sub(new_two))
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
            Some(InputKind::Sub(r(2)))
        );
    }

    /// The footer of `board` drawn `w` wide.
    fn footer(board: &mut Board, w: usize) -> String {
        let (lines, _) = board.draw(w, 12);
        text(&lines[11])
    }

    #[test]
    fn the_footers_at_120_100_and_80_columns() {
        // A card in Todo: no t.
        let mut board = sample();
        assert_eq!(
            footer(&mut board, 120),
            "  j k pick  space mark  h l move  s doing  d done  tab column  a add  A sub  e edit  y copy  x remove  q quit ",
            "all of it, where there's room"
        );
        assert_eq!(
            footer(&mut board, 100),
            "  j k pick  space mark  h l move  s doing  d done  a add  A sub  e edit  y copy  x remove  q quit ",
            "tab first"
        );
        // tab, y, q and j k give way; then j k, needing 10 of the 8 cells
        // left, is skipped, and q comes back; y, last, doesn't fit.
        let (lines, _) = board.draw(90, 12);
        assert_eq!(
            text(&lines[11]),
            "  j k pick  space mark  h l move  s doing  d done  a add  A sub  e edit  x remove  q quit ",
            "y right after tab"
        );
        assert_eq!(
            footer(&mut board, 80),
            "  space mark  h l move  s doing  d done  a add  A sub  e edit  x remove  q quit "
        );
        // In Doing: no s.
        board.col = 1;
        assert_eq!(
            footer(&mut board, 80),
            "  space mark  h l move  t todo  d done  a add  A sub  e edit  x remove  q quit "
        );
        // In Done: no d.
        board.col = 2;
        assert_eq!(
            footer(&mut board, 120),
            "  j k pick  space mark  h l move  t todo  s doing  tab column  a add  A sub  e edit  y copy  x remove  q quit "
        );
        assert_eq!(
            footer(&mut board, 80),
            "  space mark  h l move  t todo  s doing  a add  A sub  e edit  x remove  q quit "
        );

        // A sub item picked: none of h l, t and s; the room brings j k and q
        // back.
        let mut board = sub_sample();
        press(&mut board, "j");
        assert_eq!(
            footer(&mut board, 80),
            "  j k pick  space mark  d done/open  a add  A sub  e edit  x remove  q quit "
        );

        let mut board = marked_sample();
        assert_eq!(
            footer(&mut board, 100),
            "  2 marked   h l move   t todo   s doing   d done   e edit   x remove   esc clear"
        );
        assert_eq!(
            footer(&mut board, 80),
            "  2 marked   h l move   t todo   s doing   d done   e edit   x remove",
            "esc clear first"
        );
    }

    #[test]
    fn a_narrow_footer_gives_way_by_need_and_d_goes_last() {
        // tab, q, j k, then t, x and s, then the least needed; d last. The
        // room left goes back to those gone, the last to go first, one that
        // doesn't fit skipped.
        let mut board = sample();
        board.col = 2;
        for (w, want) in [
            (
                85,
                "  j k pick  space mark  h l move  t todo  s doing  a add  A sub  e edit  x remove ",
            ),
            (
                76,
                "  space mark  h l move  t todo  s doing  a add  A sub  e edit  x remove ",
            ),
            (
                66,
                "  space mark  h l move  s doing  a add  A sub  e edit  x remove ",
            ),
            (56, "  space mark  h l move  s doing  a add  A sub  e edit "),
            (50, "  space mark  h l move  a add  A sub  e edit "),
        ] {
            assert_eq!(footer(&mut board, w), want, "{w} wide, in Done");
        }
        let mut board = sub_sample();
        press(&mut board, "j");
        for (w, want) in [
            (
                76,
                "  j k pick  space mark  d done/open  a add  A sub  e edit  x remove  q quit ",
            ),
            (
                66,
                "  space mark  d done/open  a add  A sub  e edit  x remove  q quit ",
            ),
            (
                56,
                "  space mark  d done/open  a add  A sub  e edit  q quit ",
            ),
            (46, "  space mark  d done/open  a add  e edit "),
        ] {
            assert_eq!(footer(&mut board, w), want, "{w} wide, a sub item");
        }

        // Skipped, not the end: j k doesn't fit back, q after it does.
        let hints = [
            ("j k", "pick", Some(3)),
            ("d", "done", None),
            ("q", "quit", Some(2)),
        ];
        assert_eq!(giving_way(&hints, 17, 3), [("d", "done"), ("q", "quit")]);
        assert_eq!(giving_way(&hints, 26, 3).len(), 3);
        // Those that never give way, too: one that doesn't fit is skipped.
        let never = [("space", "mark", None), ("d", "done", None)];
        assert_eq!(giving_way(&never, 10, 3), [("d", "done")]);
        let mut board = marked_sample();
        assert_eq!(
            footer(&mut board, 46),
            "  2 marked   h l move   d done   e edit",
            "esc clear, then t, x and s"
        );

        // Where there's room for one, it's d.
        let hints = [
            ("j k", "pick", Some(3)),
            ("d", "done/open", None),
            ("a", "add", Some(10)),
        ];
        assert_eq!(giving_way(&hints, 13, 3), [("d", "done/open")]);
    }

    #[test]
    fn truncate_and_tail_count_columns_not_bytes() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(
            truncate("분기 보고서 초안", 13),
            "분기 보고서…",
            "no space before the cut"
        );
        assert_eq!(tail("hello world", 5), "world");
        assert_eq!(tail("日本語", 4), "本語");
    }

    // The picked row, whole.

    /// A long card with tags, a sub item and a due date, picked; buy milk
    /// under it; fix bike in Doing.
    fn long_sample() -> Board {
        Board::new(
            vec![
                due(
                    task(
                        1,
                        "write the quarterly report for the whole team",
                        Status::Todo,
                        Some(Priority::High),
                        &["work", "q4"],
                    ),
                    "2026-11-12",
                ),
                task(2, "buy milk", Status::Todo, None, &[]),
                task(3, "fix bike", Status::Doing, None, &[]),
                sub(task(4, "draft it", Status::Done, None, &[]), 1),
            ],
            today(),
        )
    }

    /// Column `c` of the body's rows, drawn `w` by `h`, its ends trimmed.
    fn column_rows(board: &mut Board, w: usize, h: usize, c: usize) -> Vec<String> {
        let (lines, _) = board.draw(w, h);
        lines[3..h - 2]
            .iter()
            .map(|l| cell(l, w, c).trim_end().to_string())
            .collect()
    }

    /// The body's rows, drawn `w` by `h`, whole and trimmed at the end.
    fn body(board: &mut Board, w: usize, h: usize) -> Vec<String> {
        let (lines, _) = board.draw(w, h);
        lines[3..h - 2]
            .iter()
            .map(|l| text(l).trim_end().to_string())
            .collect()
    }

    /// The cells of a drawn line on the picked ground.
    fn lit(line: &Line) -> usize {
        line.iter()
            .filter(|s| s.style.bg == Some(HIGHLIGHT))
            .map(|s| s.text.width())
            .sum()
    }

    #[test]
    fn the_picked_row_shows_whole_on_one_line_over_the_columns_to_its_right() {
        let mut board = long_sample();
        board
            .tasks
            .push(task(5, "call mum", Status::Done, None, &[]));
        // In order, two cells between: the title whole, count, tags, due
        // date, not pushed to the far edge; it starts where it always does.
        // At 80 it runs over all of Doing and Done, fix bike and call mum
        // under it.
        let rows = body(&mut board, 80, 14);
        assert_eq!(
            rows[..3],
            [
                "   • write the quarterly report for the whole team  1/1  +work +q4  tomorrow",
                "       ● draft it",
                "     buy milk",
            ]
        );
        let (lines, _) = board.draw(80, 14);
        assert_eq!(lit(&lines[3]), 75, "the text and one cell past it");
        assert!(lines[4].iter().all(|s| s.style.bg.is_none()));
        assert!(!rows[0].contains("fix bike") && !rows[0].contains("call mum"));

        // At 100 it ends inside Done: call mum, in a column it reaches into,
        // isn't drawn on that line, never cut.
        let rows = body(&mut board, 100, 14);
        assert_eq!(
            rows[0],
            "   • write the quarterly report for the whole team  1/1  +work +q4  tomorrow"
        );
        let (lines, _) = board.draw(100, 14);
        assert_eq!(
            lit(&lines[3]),
            75,
            "the ground still one cell past the text"
        );
        // And the next row of each column is where it was.
        assert_eq!(cell(&lines[4], 100, 0).trim_end(), "     ● draft it");

        // Not picked: every row in its column, as ever.
        board.col = 1;
        let rows = body(&mut board, 100, 14);
        assert!(rows[0].starts_with("   • write the…  1/1"), "{:?}", rows[0]);
        assert!(rows[0].contains("fix bike") && rows[0].contains("call mum"));
    }

    #[test]
    fn a_picked_row_longer_than_its_room_wraps_in_that_room() {
        // In Doing the room is two columns: 49 at 80. Longer, it wraps there,
        // its lines that room's, the due date on the first as on one line.
        let title = "fix the bike before the weekend ride with the club";
        let mut board = Board::new(
            vec![
                task(1, "buy milk", Status::Todo, None, &[]),
                due(task(2, title, Status::Doing, None, &["bike"]), "2026-11-12"),
                task(3, "call mum", Status::Done, None, &[]),
            ],
            today(),
        );
        board.col = 1;
        let (lines, _) = board.draw(80, 12);
        let rows: Vec<String> = lines[3..6]
            .iter()
            .map(|l| text(l).trim_end().to_string())
            .collect();
        assert_eq!(
            rows,
            [
                "     buy milk                  fix the bike before the weekend      tomorrow",
                "                               ride with the club  +bike",
                "",
            ]
        );
        assert_eq!(lit(&lines[3]), 49);
        assert_eq!(lit(&lines[4]), 49);
        assert_eq!(
            cell(&lines[3], 80, 0).trim_end(),
            "   buy milk",
            "Todo untouched"
        );

        // In Done the room is its own column: under itself, as before.
        let mut board = Board::new(
            vec![task(
                1,
                "read the whole lease again",
                Status::Done,
                None,
                &[],
            )],
            today(),
        );
        board.col = 2;
        let (lines, _) = board.draw(80, 12);
        assert_eq!(cell(&lines[3], 80, 2).trim_end(), "   read the whole");
        assert_eq!(cell(&lines[4], 80, 2).trim_end(), "   lease again");
        assert_eq!(lit(&lines[3]), 23);
    }

    #[test]
    fn only_the_picked_row_grows_and_only_when_some_of_it_was_cut() {
        // Not picked, or picked in a column that isn't, or marked: one line.
        let mut board = long_sample();
        press(&mut board, " ");
        assert_eq!(marks(&board), BTreeSet::from([1]));
        let rows = column_rows(&mut board, 80, 14, 0);
        assert_eq!(
            rows[..3],
            ["▌• write the qua…  1/1", "     ● draft it", "   buy milk"]
        );
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, "k");
        let rows = column_rows(&mut board, 80, 14, 0);
        assert_eq!(rows[0], "▌• write the qua…  1/1", "its column isn't picked");

        // Picked, with all of it shown: in its column, the ground its width.
        let mut board = sample();
        let rows = body(&mut board, 100, 12);
        assert!(
            rows[0].starts_with("   • pay rent  +home +money     "),
            "{:?}",
            rows[0]
        );
        let (lines, _) = board.draw(100, 12);
        assert_eq!(lit(&lines[3]), 30);
        // Its tags cut: whole, over the gap, up to fix bike.
        let (lines, _) = board.draw(80, 12);
        assert_eq!(
            text(&lines[3]).trim_end(),
            "   • pay rent  +home +money  • fix bike                  call mum"
        );
        assert_eq!(lit(&lines[3]), 26);

        // A due date left out for want of room, and nothing else: on the
        // line, two cells after the title.
        let mut board = Board::new(
            vec![due(
                task(1, "tax form", Status::Todo, None, &[]),
                "2026-12-30",
            )],
            today(),
        );
        // Into Doing, whose "Nothing here" isn't drawn; Done's is.
        assert_eq!(
            body(&mut board, 60, 10)[..2],
            [
                "     tax form  in 7 weeks                  Nothing here",
                ""
            ]
        );
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(column_rows(&mut board, 60, 10, 0)[..2], ["   tax form", ""]);
        // Short enough for its column, it keeps the column's ground.
        let mut board = Board::new(
            vec![due(task(1, "x", Status::Todo, None, &[]), "2026-12-30")],
            today(),
        );
        let (lines, _) = board.draw(80, 10);
        assert_eq!(cell(&lines[3], 80, 0), "   x  in 7 weeks       ");
        assert_eq!(lit(&lines[3]), 23);
    }

    #[test]
    fn a_word_longer_than_the_room_breaks_and_the_rest_wrap_by_words() {
        assert_eq!(wrap("pay the rent", 8, 8), ["pay the", "rent"]);
        assert_eq!(
            wrap("call supercalifragilistic now", 10, 10),
            ["call", "supercalif", "ragilistic", "now"]
        );
        // The first line narrower: a first word too long for it breaks there.
        assert_eq!(wrap("shipping it", 3, 8), ["shi", "pping it"]);
        assert_eq!(wrap("x", 0, 8), ["", "x"]);
        // Columns, not bytes: a wide letter that doesn't fit goes on.
        assert_eq!(
            wrap("보고서 작성하기 금요일까지", 9, 9),
            ["보고서", "작성하기", "금요일까", "지"]
        );
        assert_eq!(wrap("", 5, 5), [""]);
    }

    #[test]
    fn a_picked_sub_item_shows_whole_at_its_own_indent() {
        let mut board = sub_sample();
        board.tasks.push(due(
            sub(
                task(
                    9,
                    "ring the landlord about the boiler",
                    Status::Todo,
                    None,
                    &[],
                ),
                2,
            ),
            "2026-11-13",
        ));
        // Not picked: cut, its date left out.
        board.key(KeyEvent::from(KeyCode::Tab));
        let rows = column_rows(&mut board, 80, 16, 0);
        assert_eq!(rows[4], "     ◦ ring the landl…");
        board.key(KeyEvent::from(KeyCode::BackTab));
        press(&mut board, "jjjj");
        assert_eq!(picked_id(&board), Some(9));
        // From its own indent, on one line.
        let rows = body(&mut board, 80, 16);
        assert_eq!(
            rows[3..6],
            [
                "       ◦ find the iban",
                "       ◦ ring the landlord about the boiler  in 2 days",
                "     buy milk",
            ]
        );
        let (lines, _) = board.draw(80, 16);
        assert_eq!(
            lit(&lines[7]),
            53,
            "7 of head, 34 of title, 11 of date, a cell"
        );
    }

    #[test]
    fn the_column_scrolls_so_the_grown_card_is_in_view_and_no_other_moves() {
        // In Done, where a long row wraps under itself.
        let mut tasks: Vec<Task> = (1..=7)
            .map(|id| task(id, &format!("task {id}"), Status::Done, None, &[]))
            .collect();
        tasks.push(task(
            8,
            "the last one has a title that takes three lines",
            Status::Done,
            None,
            &[],
        ));
        tasks.extend((9..=16).map(|id| task(id, &format!("todo {id}"), Status::Todo, None, &[])));
        let mut board = Board::new(tasks, today());
        board.col = 2;
        press(&mut board, "G");
        // Five rows of body: two cards above, then the last one whole.
        assert_eq!(
            column_rows(&mut board, 80, 10, 2),
            [
                "   task 6",
                "   task 7",
                "   the last one has a",
                "   title that takes",
                "   three lines",
            ]
        );
        assert_eq!(
            column_rows(&mut board, 80, 10, 0)[0],
            "   todo 9",
            "Todo stays put"
        );
        // The pick a line again, the column fills down to its end.
        press(&mut board, "k");
        assert_eq!(
            column_rows(&mut board, 80, 10, 2)[3..],
            ["   task 7", "   the last one has a…"]
        );
        press(&mut board, "g");
        assert_eq!(column_rows(&mut board, 80, 10, 2)[0], "   task 1");
    }

    #[test]
    fn a_left_out_due_date_goes_right_of_the_last_line_else_on_its_own() {
        // A row wrapped 23 wide, as in a narrow Done, or a room too narrow
        // for the date beside 8 cells of title.
        let grown = |title: &str, tags: Option<&str>| -> Vec<String> {
            let (plain, title_style, quiet) = styles(true);
            let mut head = Row::default();
            head.push("   ", plain);
            let whole = Whole {
                title,
                title_style,
                count: None,
                tags: tags.map(String::from),
                due: Some(("in 7 weeks".into(), false)),
            };
            grow(head, 19, 23, whole, plain, quiet)
                .iter()
                .map(|r| text(&r.spans).trim_end().to_string())
                .collect()
        };
        // Beside the title's last line, two cells or more between.
        assert_eq!(
            grown("renew the passport soon", None),
            ["   renew the passport", "   soon     in 7 weeks"]
        );
        // Beside the tags, exactly two between; one cell short of that, a
        // line of its own.
        assert_eq!(
            grown("renew the passport", Some("+travel")),
            ["   renew the passport", "   +travel  in 7 weeks"]
        );
        assert_eq!(
            grown("renew the passport", Some("+travels")),
            [
                "   renew the passport",
                "   +travels",
                "            in 7 weeks"
            ]
        );
    }

    #[test]
    fn a_picked_sub_item_shows_its_tags_and_grows_for_them() {
        let mut board = sub_sample();
        board.tasks[6].tags = vec!["bank".into()];
        assert_eq!(board.tasks[6].id, 6);
        // Not picked: no tags, as ever.
        assert_eq!(
            column_rows(&mut board, 120, 14, 0)[2],
            "     ◦ set up the transfer"
        );
        press(&mut board, "jj");
        // Picked, where they fit beside the title: one line.
        assert_eq!(
            column_rows(&mut board, 120, 14, 0)[2..4],
            ["     ◦ set up the transfer  +bank", "     ◦ find the iban"]
        );
        // Where they don't: on the line still, over the gap to its right.
        assert_eq!(
            body(&mut board, 100, 14)[2..4],
            [
                "       ◦ set up the transfer  +bank",
                "       ◦ find the iban"
            ]
        );
        let (lines, _) = board.draw(100, 14);
        assert_eq!(lit(&lines[5]), 34);
        // Marked and not picked: none.
        press(&mut board, " ");
        assert_eq!(
            column_rows(&mut board, 120, 14, 0)[2],
            "▌    ◦ set up the transfer"
        );
    }

    #[test]
    fn the_footer_shows_only_the_keys_that_act_on_whats_picked() {
        let shown = |board: &mut Board| -> Vec<String> {
            let f = footer(board, 200);
            ["h l move", "t todo", "s doing", "d done", "x remove"]
                .into_iter()
                .filter(|k| f.contains(&format!(" {k} ")))
                .map(String::from)
                .collect()
        };
        let mut board = sub_sample();
        assert_eq!(
            shown(&mut board),
            ["h l move", "s doing", "d done", "x remove"],
            "Todo: no t"
        );
        press(&mut board, "j");
        assert_eq!(
            shown(&mut board),
            ["x remove"],
            "a sub item: none that moves it"
        );
        assert!(footer(&mut board, 200).contains(" d done/open "));
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            shown(&mut board),
            ["h l move", "t todo", "d done", "x remove"],
            "Doing: no s"
        );
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            shown(&mut board),
            ["h l move", "t todo", "s doing", "x remove"],
            "Done: t and s, no d"
        );
        // The key still does what it did in 0.2.0 there: nothing on a done
        // card, and finishes the open sub items of one that has them.
        assert_eq!(picked_id(&board), Some(4));
        assert_eq!(press(&mut board, "d"), [Outcome::Nothing]);
        board
            .tasks
            .push(sub(task(9, "ring", Status::Todo, None, &[]), 4));
        run_keys(&mut board, "d");
        assert_eq!(status(&board, 9), Status::Done);
        assert_eq!(status(&board, 4), Status::Done);
        // Marked: all of them, whatever's picked.
        let mut board = sub_sample();
        press(&mut board, " ");
        let f = footer(&mut board, 200);
        for k in ["h l move", "t todo", "s doing", "d done", "x remove"] {
            assert!(f.contains(k), "{k}: {f:?}");
        }
    }

    #[test]
    fn a_neighbours_row_is_whole_or_not_there_never_cut() {
        // The footer-80 frame that showed `x bike`: pay rent with its count
        // and tags reaches into Doing; Done it doesn't reach.
        let mut board = sub_sample();
        board.tasks[3] = task(4, "call mum", Status::Done, None, &[]);
        let (lines, _) = board.draw(80, 14);
        let rows: Vec<String> = lines[3..6]
            .iter()
            .map(|l| text(l).trim_end().to_string())
            .collect();
        assert_eq!(
            rows[0], "   • pay rent  1/3  +home +money                         call mum",
            "Doing's row not there, Done's whole"
        );
        assert_eq!(
            cell(&lines[3], 80, 2).trim_end(),
            "   call mum",
            "in its place"
        );
        assert_eq!(lit(&lines[3]), 31, "the ground one cell past the text");
        assert!(
            rows[1].starts_with("       ● ask for the bi…"),
            "{:?}",
            rows[1]
        );
        assert!(!rows.iter().any(|r| r.contains("x bike")));

        // A Korean title in Doing under a Todo row shown whole: not there,
        // and no letter cut in half; the column it doesn't reach is drawn.
        let mut board = Board::new(
            vec![
                task(
                    1,
                    "write the quarterly report",
                    Status::Todo,
                    None,
                    &["work"],
                ),
                task(2, "분기 보고서 초안", Status::Doing, None, &[]),
            ],
            today(),
        );
        let rows = body(&mut board, 80, 10);
        assert_eq!(
            rows[0],
            "     write the quarterly report  +work                   Nothing here"
        );
        let (lines, _) = board.draw(80, 10);
        assert_eq!(cell(&lines[3], 80, 2).trim_end(), "   Nothing here");
        // Not picked, it's there, whole.
        board.col = 1;
        assert!(body(&mut board, 80, 10)[0].contains("분기 보고서 초안"));
    }

    // Copying.

    #[test]
    fn y_copies_the_picked_rows_title_and_with_marks_each_marked_one_a_line() {
        let mut board = sub_sample();
        assert_eq!(
            press(&mut board, "y"),
            [Outcome::Copy {
                text: "pay rent".into(),
                what: "\"pay rent\"".into()
            }]
        );
        // A sub item's own title; ㅛ is y.
        press(&mut board, "jj");
        assert_eq!(
            press(&mut board, "ㅛ"),
            [Outcome::Copy {
                text: "set up the transfer".into(),
                what: "\"set up the transfer\"".into()
            }]
        );
        // Marked: in the board's order, by column then row, whatever the
        // order they were marked in; the marks stay.
        let mut board = sub_sample();
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, " ");
        board.key(KeyEvent::from(KeyCode::BackTab));
        press(&mut board, "jjjj ");
        board.key(KeyEvent::from(KeyCode::BackTab));
        board.col = 0;
        board.row[0] = 0;
        press(&mut board, "jj ");
        assert_eq!(marks(&board), BTreeSet::from([1, 3, 6]));
        assert_eq!(
            press(&mut board, "y"),
            [Outcome::Copy {
                text: "set up the transfer\nbuy milk\nfix bike".into(),
                what: "3 titles".into()
            }]
        );
        assert_eq!(marks(&board), BTreeSet::from([1, 3, 6]));
        // Nothing picked, nothing marked: nothing.
        let mut board = Board::new(vec![], today());
        assert_eq!(press(&mut board, "y"), [Outcome::Nothing]);
        // In the remove question, y still answers it.
        let mut board = sample();
        let outcomes = press(&mut board, "xy");
        assert!(matches!(outcomes[1], Outcome::Change(Op::Remove { .. })));
    }

    #[test]
    fn the_note_says_only_what_halil_knows_of_the_copy() {
        let mut board = sample();
        board.copied("\"pay rent\"", Ok(()));
        assert_eq!(board.note.as_deref(), Some("Copied \"pay rent\"."));
        assert_eq!(board.message, None);
        let mut board = sample();
        board.copied("3 titles", Ok(()));
        assert_eq!(board.note.as_deref(), Some("Copied 3 titles."));
        let mut board = sample();
        board.sent("\"pay rent\"", Ok(()));
        assert_eq!(
            board.note.as_deref(),
            Some("Sent \"pay rent\" to the terminal's clipboard.")
        );
        // A command that failed: in the refusal's colour.
        let mut board = sample();
        board.copied("\"pay rent\"", Err("xclip failed (exit status: 1)".into()));
        assert_eq!(board.note, None);
        let said = "couldn't copy \"pay rent\": xclip failed (exit status: 1)";
        assert_eq!(board.message.as_deref(), Some(said));
        let (lines, _) = board.draw(100, 12);
        let span = lines[10].iter().find(|s| s.text == said).unwrap();
        assert_eq!(span.style.fg, Some(RED));
    }

    #[test]
    fn ys_text_goes_to_the_copy_command_off_the_keys_path() {
        // A stand-in for the platform's command, writing into this test's
        // own scratch folder: the real clipboard is never touched.
        let dir = env::temp_dir().join(format!("halil-test-{}-board-copy", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut board = marked_sample();
        let [Outcome::Copy { text, what }] = &press(&mut board, "y")[..] else {
            panic!("y copies");
        };
        let (tx, rx) = mpsc::channel();
        let args = vec![
            "-c".into(),
            "cat > \"$1/clipboard\"".into(),
            "sh".into(),
            dir.display().to_string(),
        ];
        clip::spawn(
            "sh".into(),
            args,
            text.clone(),
            what.clone(),
            clip::LIMIT,
            tx,
        );
        let (what, result) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        board.copied(&what, result);
        assert_eq!(board.note.as_deref(), Some("Copied 2 titles."));
        assert_eq!(
            fs::read_to_string(dir.join("clipboard")).unwrap(),
            "pay rent\nfix bike"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    // Hangul.

    #[test]
    fn hangul_is_read_as_the_keys_it_sits_on_and_said_once_a_run() {
        let mut board = sub_sample();
        press(&mut board, "ㅓ");
        assert_eq!(picked_id(&board), Some(5), "ㅓ is j");
        assert_eq!(board.hint.as_deref(), Some("ㅓ read as j."));
        let long = "ㅓ read as j. A Korean input may hold a letter until the next key.";
        let (lines, _) = board.draw(80, 14);
        assert_eq!(text(&lines[12]), format!("  {long}"), "whole at 80");
        let said = lines[12].iter().find(|s| s.text == long).unwrap();
        assert_eq!(said.style.fg, None, "in the default foreground");
        // Where that doesn't fit, the short form.
        let (lines, _) = board.draw(60, 14);
        assert_eq!(text(&lines[12]), "  ㅓ read as j. It may run a key late.");
        press(&mut board, "ㅓ");
        assert_eq!(board.hint, None, "once a run");
        assert_eq!(picked_id(&board), Some(6));

        // A syllable is its jamo's keys, in order, each change made before
        // the next key: 어 is d (6 done) then j.
        run_keys(&mut board, "어");
        assert_eq!(status(&board, 6), Status::Done);
        assert_eq!(picked_id(&board), Some(7));
        // ㅘ is h then k; ㅗ alone is h, refused on a sub item, and said.
        let mut board = sample();
        assert_eq!(
            press(&mut board, "ㅘ"),
            [Outcome::Nothing, Outcome::Nothing],
            "h at the edge, k at the top"
        );
        assert!(
            board
                .hint
                .as_deref()
                .unwrap()
                .starts_with("ㅘ read as h k.")
        );
        // Beside a refusal: what room is left decides the form.
        let mut board = sub_sample();
        press(&mut board, "jㅗ");
        assert_eq!(
            board.message.as_deref(),
            Some("a sub item moves with its task")
        );
        let (lines, _) = board.draw(110, 14);
        assert_eq!(
            text(&lines[12]),
            "  a sub item moves with its task   ㅗ read as h. A Korean input may hold a letter until the next key."
        );
        let (lines, _) = board.draw(80, 14);
        assert_eq!(
            text(&lines[12]),
            "  a sub item moves with its task   ㅗ read as h. It may run a key late."
        );
        let mut board = sub_sample();
        press(&mut board, "jㅗ");
        assert_eq!(
            board.message.as_deref(),
            Some("a sub item moves with its task")
        );
        // ㅂ is q.
        assert_eq!(press(&mut board, "ㅂ"), [Outcome::Quit]);
    }

    #[test]
    fn hangul_is_words_where_words_are_typed() {
        let mut board = sample();
        press(&mut board, "ㅁ");
        assert_eq!(typed(&board), Some(""), "ㅁ is a: the input opens");
        press(&mut board, "장보기 ㅓ");
        assert_eq!(typed(&board), Some("장보기 ㅓ"));
        key(&mut board, KeyCode::Esc);
        // 마 is a then k: what's left after a opened the input is typed.
        press(&mut board, "마");
        assert_eq!(typed(&board), Some("ㅏ"));
        key(&mut board, KeyCode::Esc);
        press(&mut board, "ㄷ");
        assert_eq!(typed(&board), Some("pay rent"), "ㄷ is e");
        press(&mut board, "ㅓ");
        assert_eq!(typed(&board), Some("pay rentㅓ"));
    }

    // Removing.

    fn delete(board: &mut Board) -> Outcome {
        key(board, KeyCode::Delete)
    }

    #[test]
    fn x_asks_in_the_footer_and_y_removes_the_card_with_its_sub_items() {
        let mut board = sub_sample();
        assert_eq!(press(&mut board, "x"), [Outcome::Nothing]);
        assert_eq!(
            footer(&mut board, 80),
            "  Remove 2 \"pay rent\" and its 3 sub items?   y remove · esc keep"
        );
        let [Outcome::Change(op)] = &mut press(&mut board, "y")[..] else {
            panic!("y removes");
        };
        assert_eq!(
            *op,
            Op::Remove {
                asked: refs(&[2]),
                with: refs(&[7, 5, 6]),
                marked: false
            }
        );
        board.apply(std::mem::replace(op, Op::Finish { refs: vec![] }));
        assert_eq!(ids(&board, 0), [1, 8], "its sub items with it");
        assert_eq!(
            board.note.as_deref(),
            Some("Removed 2 and its 3 sub items.")
        );
        assert_eq!(picked_id(&board), Some(1), "the pick on its row");
        assert!(
            footer(&mut board, 80).contains(" x remove "),
            "the keys again"
        );

        // Enter removes too, and Delete asks as x does; the last row's
        // removal leaves the pick on the row above.
        let mut board = sample();
        press(&mut board, "j");
        assert_eq!(delete(&mut board), Outcome::Nothing);
        assert!(footer(&mut board, 80).starts_with("  Remove 1 \"buy milk\"?"));
        assert!(enter(&mut board));
        assert_eq!(board.note.as_deref(), Some("Removed 1."));
        assert_eq!(ids(&board, 0), [2]);
        assert_eq!(picked_id(&board), Some(2), "clamped");
        // ㅌ is x, ㅛ is y.
        run_keys(&mut board, "ㅌㅛ");
        assert!(ids(&board, 0).is_empty());
        assert_eq!(press(&mut board, "x"), [Outcome::Nothing]);
        assert_eq!(board.asking, None, "nothing to ask about");
    }

    #[test]
    fn any_other_key_keeps_it_and_says_nothing_was_removed() {
        for keep in [
            KeyCode::Char('j'),
            KeyCode::Char('n'),
            KeyCode::Esc,
            KeyCode::Char('x'),
        ] {
            let mut board = sub_sample();
            press(&mut board, "x");
            assert_eq!(key(&mut board, keep), Outcome::Nothing, "{keep:?}");
            assert_eq!(board.tasks, sub_sample().tasks, "{keep:?}");
            assert_eq!(board.note.as_deref(), Some("Nothing was removed."));
            assert_eq!(picked_id(&board), Some(2), "{keep:?} did nothing else");
            assert_eq!(board.asking, None);
        }
        // Ctrl-Y isn't y.
        let mut board = sub_sample();
        press(&mut board, "x");
        assert_eq!(ctrl(&mut board, 'y'), Outcome::Nothing);
        assert_eq!(board.note.as_deref(), Some("Nothing was removed."));
    }

    #[test]
    fn a_picked_sub_item_goes_alone() {
        let mut board = sub_sample();
        press(&mut board, "jjx");
        assert_eq!(
            footer(&mut board, 80),
            "  Remove 6 \"set up the transfer\"?   y remove · esc keep"
        );
        run_keys(&mut board, "y");
        assert_eq!(ids(&board, 0), [2, 5, 7, 1, 8]);
        assert_eq!(board.note.as_deref(), Some("Removed 6."));
        assert_eq!(picked_id(&board), Some(7));
    }

    #[test]
    fn with_marks_x_removes_all_the_marked_a_sub_item_counted_once() {
        // Pay rent and its sub item 6 marked, and fix bike.
        let mut board = sub_sample();
        press(&mut board, " j ");
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, " ");
        assert_eq!(marks(&board), BTreeSet::from([2, 3, 6]));
        press(&mut board, "x");
        assert_eq!(
            footer(&mut board, 80),
            "  Remove 3 marked and their 2 sub items?   y remove · esc keep"
        );
        run_keys(&mut board, "y");
        assert!(board.marked.is_empty(), "cleared");
        assert_eq!(
            board.tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [1, 4, 8]
        );
        assert_eq!(
            board.note.as_deref(),
            Some("Removed 3 marked and their 2 sub items.")
        );

        // One marked, no sub items.
        let mut board = sample();
        press(&mut board, "j x");
        assert_eq!(
            footer(&mut board, 80),
            "  Remove 1 marked?   y remove · esc keep"
        );
        run_keys(&mut board, "y");
        assert_eq!(board.note.as_deref(), Some("Removed 1 marked."));
        // Kept: the marks stay.
        let mut board = marked_sample();
        press(&mut board, "xk");
        assert_eq!(marks(&board), BTreeSet::from([2, 3]));
    }

    #[test]
    fn a_long_question_cuts_the_title_then_leaves_it_out() {
        let mut board = long_sample();
        press(&mut board, "x");
        assert_eq!(
            footer(&mut board, 80),
            "  Remove 1 \"write the quarterly repo…\" and its 1 sub item?   y remove · esc keep"
        );
        assert_eq!(
            footer(&mut board, 60),
            "  Remove 1 and its 1 sub item?   y remove · esc keep"
        );
        assert_eq!(footer(&mut board, 46), "  Remove 1 and its 1 sub item?");
    }

    // Stamps: a task that took a gone task's id isn't it.

    /// Sub sample with fix bike (3) gone and a new 3 in its id, with a stamp.
    fn three_taken(board: &Board) -> Vec<Task> {
        let mut fresh: Vec<Task> = board.tasks.iter().filter(|t| t.id != 3).cloned().collect();
        fresh.push(Task {
            stamp: 1_763_500_000_000,
            ..task(3, "walk the dog", Status::Doing, None, &[])
        });
        fresh
    }

    #[test]
    fn a_mark_on_a_task_whose_id_was_taken_meanwhile_drops() {
        let mut board = marked_sample();
        assert_eq!(marks(&board), BTreeSet::from([2, 3]));
        let fresh = three_taken(&board);
        board.replace_tasks(fresh);
        assert_eq!(marks(&board), BTreeSet::from([2]));
        run_keys(&mut board, "s");
        assert_eq!(status(&board, 2), Status::Doing);
        assert_eq!(by_id(&board, 3).title, "walk the dog");
        assert_eq!(status(&board, 3), Status::Doing, "untouched");
    }

    #[test]
    fn a_change_to_a_task_whose_id_was_taken_meanwhile_is_refused_as_gone() {
        // Each key's change, and the file changed before it's made.
        for keys in ["d", "l", "h", "t", "xy"] {
            let mut board = sample();
            board.key(KeyEvent::from(KeyCode::Tab));
            let outcomes = press(&mut board, keys);
            let Some(Outcome::Change(op)) = outcomes.into_iter().last() else {
                panic!("{keys} is a change");
            };
            let fresh = three_taken(&board);
            board.replace_tasks(fresh.clone());
            board.apply(op);
            assert_eq!(board.message.as_deref(), Some("task 3 is gone"), "{keys}");
            assert_eq!(board.tasks, fresh, "{keys}: the new 3 untouched");
        }
        // With marks: the others are changed.
        let mut board = marked_sample();
        let [Outcome::Change(op)] = &mut press(&mut board, "d")[..] else {
            panic!("d is a change");
        };
        let fresh = three_taken(&board);
        board.replace_tasks(fresh);
        board.apply(std::mem::replace(op, Op::Finish { refs: vec![] }));
        assert_eq!(board.message.as_deref(), Some("task 3 is gone"));
        assert_eq!(
            (status(&board, 2), status(&board, 3)),
            (Status::Done, Status::Doing)
        );

        assert_eq!(gone(&[3, 5, 8]), "tasks 3, 5 and 8 are gone");
    }

    #[test]
    fn an_open_e_or_a_whose_task_was_taken_meanwhile_is_refused_or_its_own() {
        let mut board = sample();
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, "e and oil the chain");
        let fresh = three_taken(&board);
        board.replace_tasks(fresh.clone());
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.message.as_deref(), Some("task 3 is gone"));
        assert_eq!(board.input, None);
        assert_eq!(board.tasks, fresh);

        let mut board = sample();
        board.key(KeyEvent::from(KeyCode::Tab));
        press(&mut board, "Apump the tyres");
        let fresh = three_taken(&board);
        board.replace_tasks(fresh);
        assert!(enter(&mut board));
        let added = board.tasks.last().unwrap();
        assert_eq!(
            (added.title.as_str(), added.parent),
            ("pump the tyres", None)
        );
        assert_eq!(
            board.note.as_deref(),
            Some("3 is gone: added as a task of its own.")
        );
    }

    // s and t.

    #[test]
    fn s_sends_to_doing_and_t_to_todo_the_pick_staying_on_its_row() {
        let mut board = sample();
        assert_eq!(
            press(&mut board, "s"),
            [Outcome::Change(Op::Send {
                refs: refs(&[2]),
                to: Status::Doing
            })]
        );
        run_keys(&mut board, "s");
        assert_eq!(status(&board, 2), Status::Doing);
        assert_eq!(
            (board.col, picked_id(&board)),
            (0, Some(1)),
            "as d: the next card"
        );
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(ids(&board, 1), [2, 3]);
        run_keys(&mut board, "jt");
        assert_eq!(status(&board, 3), Status::Todo);
        assert_eq!(picked_id(&board), Some(2));

        // From Done too.
        board.key(KeyEvent::from(KeyCode::Tab));
        run_keys(&mut board, "t");
        assert_eq!(status(&board, 4), Status::Todo);
        // Digits are no keys, on any column.
        for c in 0..3 {
            let mut board = sample();
            board.col = c;
            assert!(picked_id(&board).is_some());
            let outcomes = press(&mut board, "0123456789");
            assert!(outcomes.iter().all(|o| *o == Outcome::Nothing), "{c}");
            assert_eq!(board.tasks, sample().tasks);
        }
    }

    #[test]
    fn s_or_t_on_a_card_already_there_says_so() {
        let mut board = sample();
        assert_eq!(press(&mut board, "t"), [Outcome::Nothing]);
        assert_eq!(board.note.as_deref(), Some("2 is in Todo already."));
        assert_eq!(board.message, None, "not a refusal");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(press(&mut board, "s"), [Outcome::Nothing]);
        assert_eq!(board.note.as_deref(), Some("3 is in Doing already."));
        let (lines, _) = board.draw(100, 12);
        assert_eq!(text(&lines[10]), "  3 is in Doing already.");

        let mut board = marked_sample();
        board.key(KeyEvent::from(KeyCode::BackTab));
        press(&mut board, "j ");
        assert_eq!(marks(&board), BTreeSet::from([1, 2, 3]));
        run_keys(&mut board, "t");
        assert_eq!(status(&board, 3), Status::Todo);
        assert_eq!(press(&mut board, "t"), [Outcome::Nothing]);
        assert_eq!(board.note.as_deref(), Some("3 are in Todo already."));

        // Beside a marked sub item's staying, whole at 80: buy milk, orphan
        // and sub item 6 marked.
        let mut board = sub_sample();
        press(&mut board, "jj j  ");
        assert_eq!(marks(&board), BTreeSet::from([1, 6, 8]));
        assert_eq!(press(&mut board, "t"), [Outcome::Nothing]);
        let (lines, _) = board.draw(80, 14);
        assert_eq!(
            text(&lines[12]),
            "  1 sub item stayed: a sub item moves with its task   2 are in Todo already."
        );
        // One marked card: by its id, as when it's picked.
        let mut board = sample();
        press(&mut board, " ");
        assert_eq!(marks(&board), BTreeSet::from([2]));
        press(&mut board, "t");
        assert_eq!(board.note.as_deref(), Some("2 is in Todo already."));
    }

    #[test]
    fn s_and_t_on_a_sub_item_are_refused_and_with_marks_marked_sub_items_stay() {
        let mut board = sub_sample();
        press(&mut board, "j");
        for k in ["s", "t"] {
            assert_eq!(press(&mut board, k), [Outcome::Nothing]);
            assert_eq!(
                board.message.as_deref(),
                Some("a sub item moves with its task")
            );
        }

        // Marked: buy milk and sub item 6 (pay rent unmarked).
        let mut board = sub_sample();
        press(&mut board, "jj j ");
        assert_eq!(marks(&board), BTreeSet::from([1, 6]));
        run_keys(&mut board, "s");
        assert_eq!(
            (status(&board, 1), status(&board, 6), status(&board, 2)),
            (Status::Doing, Status::Todo, Status::Todo)
        );
        assert_eq!(
            board.message.as_deref(),
            Some("1 sub item stayed: a sub item moves with its task")
        );
        assert_eq!(
            marks(&board),
            BTreeSet::from([1, 6]),
            "marks stay, as for h and l"
        );
    }

    #[test]
    fn out_of_done_a_card_reopens_none_of_its_sub_items() {
        let mut board = sub_sample();
        run_keys(&mut board, "d");
        for id in [2, 5, 6, 7] {
            assert_eq!(status(&board, id), Status::Done);
        }
        board.col = 2;
        board.row[2] = ids(&board, 2).iter().position(|&id| id == 2).unwrap();
        run_keys(&mut board, "s");
        assert_eq!(status(&board, 2), Status::Doing);
        for id in [5, 6, 7] {
            assert_eq!(status(&board, id), Status::Done, "{id} stays done");
        }
        board.col = 1;
        board.row[1] = 0;
        run_keys(&mut board, "t");
        assert_eq!(status(&board, 2), Status::Todo);
        assert_eq!(status(&board, 6), Status::Done);
    }

    // Shots for the design session: `HALIL_SHOTS=<dir> cargo test shots --
    // --ignored` writes them, plain and with colours as ANSI.
    #[test]
    #[ignore = "writes shots to $HALIL_SHOTS"]
    fn shots() {
        let Some(dir) = env::var_os("HALIL_SHOTS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        fs::create_dir_all(&dir).unwrap();
        let plain = |lines: &[Line]| -> String {
            lines
                .iter()
                .map(|l| text(l).trim_end().to_string() + "\n")
                .collect()
        };
        let ansi = |lines: &[Line]| -> String {
            lines
                .iter()
                .map(|l| {
                    let mut s: String = l
                        .iter()
                        .map(|span| sgr(span.style, true) + &span.text)
                        .collect();
                    s += "\x1b[0m\n";
                    s
                })
                .collect()
        };
        // One sheet: each frame titled, then the screen.
        let mut sheets: BTreeMap<String, (String, String)> = BTreeMap::new();
        let mut shoot = |sheet: &str, title: &str, board: &mut Board, w: usize, h: usize| {
            let (lines, _) = board.draw(w, h);
            let (p, a) = sheets.entry(sheet.to_string()).or_default();
            let rule = format!("── {title} ({w}×{h}) ");
            let rule = format!("{rule}{}\n", "─".repeat(w.saturating_sub(rule.width())));
            *p += &rule;
            *p += &plain(&lines);
            *a += &rule;
            *a += &ansi(&lines);
        };

        let mixed = || {
            let mut tasks = vec![
                due(
                    task(
                        1,
                        "write the quarterly report for the whole team",
                        Status::Todo,
                        Some(Priority::High),
                        &["work", "q4"],
                    ),
                    "2026-11-13",
                ),
                task(2, "buy milk", Status::Todo, None, &[]),
                task(
                    3,
                    "분기 보고서 초안 작성하고 팀장님께 검토 요청하기",
                    Status::Todo,
                    Some(Priority::Med),
                    &["회사"],
                ),
                task(4, "call mum", Status::Todo, None, &["family"]),
                due(
                    task(
                        5,
                        "renew the passport before the trip in march",
                        Status::Todo,
                        Some(Priority::Low),
                        &[],
                    ),
                    "2026-12-30",
                ),
                task(6, "장보기", Status::Todo, None, &[]),
                task(7, "fix bike", Status::Doing, Some(Priority::Low), &[]),
                due(
                    task(
                        8,
                        "read the lease and mark what to ask the landlord",
                        Status::Doing,
                        None,
                        &["home"],
                    ),
                    "2026-11-16",
                ),
                task(
                    9,
                    "pay rent for november and keep the receipt",
                    Status::Done,
                    Some(Priority::High),
                    &["home"],
                ),
                task(13, "월세 이체", Status::Done, None, &[]),
            ];
            tasks.extend([
                sub(
                    task(
                        10,
                        "pull the numbers from last quarter's sheets",
                        Status::Done,
                        None,
                        &[],
                    ),
                    1,
                ),
                sub(
                    due(
                        task(
                            11,
                            "ask design for the charts",
                            Status::Todo,
                            None,
                            &["design"],
                        ),
                        "2026-11-12",
                    ),
                    1,
                ),
                sub(task(12, "표와 그래프 정리하기", Status::Todo, None, &[]), 3),
            ]);
            Board::new(tasks, today())
        };
        for w in [80, 60] {
            let sheet = format!("jjj-{w}");
            let mut board = mixed();
            for (c, &status) in COLUMNS.iter().enumerate() {
                let name = status_name(status);
                let id = picked_id(&board).unwrap();
                shoot(&sheet, &format!("{name}: {id} picked"), &mut board, w, 18);
                for n in 1..board.column(c).len() {
                    press(&mut board, "j");
                    let id = picked_id(&board).unwrap();
                    shoot(
                        &sheet,
                        &format!("{name}, j ×{n}: {id} picked"),
                        &mut board,
                        w,
                        18,
                    );
                }
                board.key(KeyEvent::from(KeyCode::Tab));
            }
        }

        for w in [80, 60] {
            let sheet = format!("footer-{w}");
            let mut board = sub_sample();
            shoot(&sheet, "a card in Todo", &mut board, w, 14);
            press(&mut board, "j");
            shoot(&sheet, "a sub item picked", &mut board, w, 14);
            board.key(KeyEvent::from(KeyCode::Tab));
            shoot(&sheet, "a card in Doing", &mut board, w, 14);
            board.key(KeyEvent::from(KeyCode::Tab));
            shoot(&sheet, "a card in Done", &mut board, w, 14);
            let mut board = marked_sample();
            shoot(&sheet, "marked", &mut board, w, 14);
            let mut board = sub_sample();
            press(&mut board, "x");
            shoot(&sheet, "x on a card", &mut board, w, 14);
            let mut board = sub_sample();
            press(&mut board, "jjx");
            shoot(&sheet, "x on a sub item", &mut board, w, 14);
            let mut board = marked_sample();
            press(&mut board, "x");
            shoot(&sheet, "x with marks", &mut board, w, 14);

            // What y ends with: a command that ended well, the sequence
            // sent, a command that failed.
            let mut board = sub_sample();
            board.copied("\"pay rent\"", Ok(()));
            shoot(&sheet, "y, a command that ended well", &mut board, w, 14);
            let mut board = marked_sample();
            board.copied("2 titles", Ok(()));
            shoot(
                &sheet,
                "y with marks, a command that ended well",
                &mut board,
                w,
                14,
            );
            let mut board = sub_sample();
            board.sent("\"pay rent\"", Ok(()));
            shoot(&sheet, "y, the terminal's sequence sent", &mut board, w, 14);
            let mut board = sub_sample();
            board.copied("\"pay rent\"", Err("xclip failed (exit status: 1)".into()));
            shoot(&sheet, "y, a command that failed", &mut board, w, 14);
        }

        for w in [80, 60] {
            let sheet = format!("s-t-x-{w}");
            let mut board = sub_sample();
            shoot(&sheet, "before", &mut board, w, 14);
            run_keys(&mut board, "s");
            shoot(&sheet, "s on pay rent", &mut board, w, 14);
            board.key(KeyEvent::from(KeyCode::Tab));
            run_keys(&mut board, "s");
            shoot(&sheet, "tab, s on pay rent again", &mut board, w, 14);
            run_keys(&mut board, "t");
            shoot(&sheet, "t on pay rent", &mut board, w, 14);
            board.key(KeyEvent::from(KeyCode::BackTab));
            run_keys(&mut board, "j");
            run_keys(&mut board, "s");
            shoot(&sheet, "j, s on a sub item", &mut board, w, 14);
            run_keys(&mut board, "x");
            shoot(&sheet, "x on the sub item", &mut board, w, 14);
            run_keys(&mut board, "y");
            shoot(&sheet, "y", &mut board, w, 14);
            run_keys(&mut board, "x");
            run_keys(&mut board, "n");
            shoot(&sheet, "x, n", &mut board, w, 14);

            let mut board = sub_sample();
            board
                .tasks
                .push(task(9, "water plants", Status::Done, None, &[]));
            press(&mut board, "jj j ");
            board.key(KeyEvent::from(KeyCode::Tab));
            board.key(KeyEvent::from(KeyCode::Tab));
            press(&mut board, "  ");
            assert_eq!(marks(&board), BTreeSet::from([1, 4, 6, 9]));
            shoot(
                &sheet,
                "marked: set up the transfer, buy milk, call mum, water plants",
                &mut board,
                w,
                14,
            );
            run_keys(&mut board, "s");
            shoot(&sheet, "s with marks", &mut board, w, 14);
            run_keys(&mut board, "t");
            shoot(&sheet, "t with marks", &mut board, w, 14);
            run_keys(&mut board, "t");
            shoot(&sheet, "t again", &mut board, w, 14);
            run_keys(&mut board, "x");
            shoot(&sheet, "x with marks", &mut board, w, 14);
            run_keys(&mut board, "y");
            shoot(&sheet, "y", &mut board, w, 14);
        }

        let mut board = sub_sample();
        press(&mut board, "ㅓ");
        shoot(
            "hangul-80",
            "ㅓ pressed: read as j, said once",
            &mut board,
            80,
            14,
        );
        press(&mut board, "ㅓ");
        shoot("hangul-80", "ㅓ again: not said", &mut board, 80, 14);
        press(&mut board, "ㅁ");
        press(&mut board, "장보기");
        shoot("hangul-80", "ㅁ (a), then 장보기 typed", &mut board, 80, 14);

        for (name, (p, a)) in sheets {
            fs::write(dir.join(format!("{name}.txt")), p).unwrap();
            fs::write(dir.join(format!("{name}.ansi")), a).unwrap();
        }
    }
}
