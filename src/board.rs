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
use std::ops::Range;
use std::path::{Path, PathBuf};
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
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cal::{self, Group};
use crate::clip::{self, Clipboard};
use crate::date::{self, Due, Local};
use crate::filter::{self, Filter};
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

/// Opens the board on the tasks file at `path`, or its calendar when `cal`
/// (`halil cal`), `filter` on (none when it's empty), and runs it until `q`.
pub fn run(path: &Path, tasks: Vec<Task>, cal: bool, filter: Filter) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("the board needs a terminal".into());
    }
    let color = env::var_os("NO_COLOR").is_none();
    let mut board = Board::new(tasks, Zoned::now().date());
    board.folded = load_folds(&folds_path(path));
    board.keep_folds_in_sight();
    if cal {
        board.open_cal(false);
    }
    if !filter.is_empty() {
        board.filter = Some(filter);
        board.refilter(None);
    }
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
                    board.save_change(path, op);
                    seen = modified();
                }
            }
            match board.next_queued() {
                Some(next) => outcome = next,
                None => break,
            }
        }
        board.keep_folds(path);
    }
}

/// Where the board keeps which cards are folded: a file of its own beside
/// the tasks file, which `halil` itself never reads.
pub fn folds_path(tasks: &Path) -> PathBuf {
    tasks.with_file_name("board.json")
}

/// The board's own file: the folded cards, by id and stamp.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    folded: Vec<Ref>,
}

/// The folded cards kept at `path`: none when it's missing or can't be read,
/// never an error.
fn load_folds(path: &Path) -> BTreeSet<Ref> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Kept>(&text).ok())
        .map(|kept| kept.folded.into_iter().collect())
        .unwrap_or_default()
}

/// Keeps the folded cards at `path`, written beside it, then renamed over it.
fn save_folds(path: &Path, folded: &BTreeSet<Ref>) -> Result<(), String> {
    let kept = Kept {
        folded: folded.iter().copied().collect(),
    };
    let fail = |e: io::Error| format!("can't keep the folds in {}: {e}", path.display());
    let text = serde_json::to_string(&kept).map_err(|e| e.to_string())? + "\n";
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, text).map_err(fail)?;
    fs::rename(&tmp, path).map_err(fail)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// The column picked, and the row picked in it. Another column holds no
    /// pick of its own: going there lands on the row on the same line (see
    /// [`Board::pick_column`]).
    col: usize,
    row: usize,
    /// The first row drawn in each column, when it has more than fit.
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
    /// The cards whose sub items are folded away, and whether that changed
    /// since it was last kept (see [`Board::keep_folds`]).
    folded: BTreeSet<Ref>,
    folds_changed: bool,
    /// Whether the screen last drawn had room for the calendar: without it,
    /// the arrows and esc are the input's.
    cal_room: bool,
    /// The calendar view, while it's up (`c`, `halil cal`).
    cal: Option<CalView>,
    /// The filter on (`/`, `halil board <filter>`): kept, or as it's typed.
    filter: Option<Filter>,
    /// While one is typed, the filter it replaces, back on `esc`.
    filter_was: Option<Option<Filter>>,
    /// What a change under the filter took out of it: shown, faint, until
    /// the filter changes.
    filter_faint: BTreeSet<Ref>,
    /// The zone halil runs in: a stamp's day is read in it.
    tz: TimeZone,
}

/// Where the calendar's keys go: the grid of days, the picked day's list, or
/// the list of what has no date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Grid,
    Day,
    Unknown,
}

/// The calendar view: the day picked, where the keys go, the row picked in
/// the list under the grid, and the list as it was before the last change,
/// while the pick hasn't moved since: what left it stays, faint.
#[derive(Debug, Clone)]
struct CalView {
    day: Date,
    focus: Focus,
    row: usize,
    scroll: usize,
    kept: Option<Vec<(Group, Ref)>>,
    /// Opened from the board, where `esc` goes back to; else from `halil cal`,
    /// and `esc` quits.
    from_board: bool,
}

/// A row of the calendar's list: a task, what it's listed under, and whether
/// it's left the list since the pick last moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    group: Group,
    at: Ref,
    faint: bool,
}

/// A day as the calendar's notes say it: `Fri 27 Nov`, with its year when
/// it's not `today`'s.
fn day_words(day: Date, today: Date) -> String {
    match day.year() == today.year() {
        true => day.strftime("%a %-d %b").to_string(),
        false => day.strftime("%a %-d %b %Y").to_string(),
    }
}

impl Board {
    pub fn new(tasks: Vec<Task>, today: Date) -> Board {
        Board {
            tasks,
            today,
            col: 0,
            row: 0,
            scroll: [0; 3],
            marked: BTreeMap::new(),
            input: None,
            asking: None,
            queued: VecDeque::new(),
            hangul_said: false,
            message: None,
            note: None,
            hint: None,
            folded: BTreeSet::new(),
            folds_changed: false,
            cal_room: true,
            cal: None,
            tz: TimeZone::system(),
            filter: None,
            filter_was: None,
            filter_faint: BTreeSet::new(),
        }
    }

    /// Whether the task at `i` is shown under the filter: it or (a card) one
    /// of its sub items matches, or a change took it out of it meanwhile.
    fn shown(&self, i: usize, parents: &[Option<usize>]) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|f| f.shows(&self.tasks, parents, i, self.today))
            || self.filter_faint.contains(&self.held(i))
    }

    /// Whether the task at `i` stays only because a change took it out of the
    /// filter: drawn faint.
    fn faint_under_filter(&self, i: usize) -> bool {
        self.filter_faint.contains(&self.held(i))
    }

    /// The tasks the filter shows (all of them with none), for the
    /// calendar's counts.
    fn visible(&self) -> Vec<Task> {
        let parents = task::parents(&self.tasks);
        (0..self.tasks.len())
            .filter(|&i| self.shown(i, &parents))
            .map(|i| self.tasks[i].clone())
            .collect()
    }

    /// `3 of 12`: the cards the filter shows, of all.
    fn filter_count(&self) -> (usize, usize) {
        let parents = task::parents(&self.tasks);
        let cards: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| parents[i].is_none())
            .collect();
        let shown = cards
            .iter()
            .filter(|&&i| {
                self.filter
                    .as_ref()
                    .is_none_or(|f| f.shows(&self.tasks, &parents, i, self.today))
            })
            .count();
        (shown, cards.len())
    }

    /// After the filter changed, the pick: on its card while that's shown,
    /// else the first shown in its column, else the first shown.
    fn refilter(&mut self, before: Option<Ref>) {
        self.filter_faint.clear();
        if let Some(at) = before
            && let Some(i) = self.find(at)
            && (0..COLUMNS.len()).any(|c| self.column(c).contains(&i))
        {
            self.pick(at.id);
            return;
        }
        self.scroll = [0; 3];
        if !self.column(self.col).is_empty() {
            self.row = 0;
        } else if let Some(c) = (0..COLUMNS.len()).find(|&c| !self.column(c).is_empty()) {
            self.col = c;
            self.row = 0;
        }
    }

    /// `/`: the typing line as the filter, the kept one's words in it.
    fn open_filter(&mut self) {
        let text = self
            .filter
            .as_ref()
            .map(|f| f.text.clone())
            .unwrap_or_default();
        self.filter_was = Some(self.filter.clone());
        self.input = Some(Input::new(InputKind::Filter, text));
    }

    /// The filter as the words typed read so far, the columns narrowing.
    fn filter_typed(&mut self) {
        let before = self.board_pick();
        let text = self
            .input
            .as_ref()
            .map(|i| i.text.clone())
            .unwrap_or_default();
        let typed = filter::read_typing(&text, self.now());
        self.filter = (!typed.is_empty()).then_some(typed);
        self.refilter(before);
    }

    /// The board's own pick, the calendar up or not.
    fn board_pick(&self) -> Option<Ref> {
        self.column(self.col).get(self.row).map(|&i| self.held(i))
    }

    /// `c` on the board, or `halil cal`: the calendar, on today.
    fn open_cal(&mut self, from_board: bool) {
        self.cal = Some(CalView {
            day: self.today,
            focus: Focus::Grid,
            row: 0,
            scroll: 0,
            kept: None,
            from_board,
        });
    }

    /// Back to the board: the marks it has no row for go.
    fn close_cal(&mut self) {
        self.cal = None;
        self.keep_marks_in_sight();
        self.clamp();
    }

    /// The list under the calendar's grid as it's to be shown, without what
    /// left it: the picked day's, or what has no date.
    fn fresh_entries(&self) -> Vec<(Group, Ref)> {
        let Some(view) = &self.cal else {
            return Vec::new();
        };
        let listed = match view.focus {
            Focus::Unknown => cal::not_known(&self.tasks),
            _ => cal::listed(&self.tasks, view.day, &self.tz),
        };
        let parents = task::parents(&self.tasks);
        listed
            .into_iter()
            .filter(|&(_, i)| self.shown(i, &parents))
            .map(|(g, i)| (g, self.held(i)))
            .collect()
    }

    /// The list under the calendar's grid as it's shown: with what left it
    /// since the last change, faint, in its place, until the pick moves.
    fn entries(&self) -> Vec<Entry> {
        let fresh = self.fresh_entries();
        let entry = |(group, at): (Group, Ref), faint| Entry { group, at, faint };
        match self.cal.as_ref().and_then(|v| v.kept.as_ref()) {
            None => fresh.into_iter().map(|e| entry(e, false)).collect(),
            Some(kept) => kept
                .iter()
                .filter(|(_, at)| self.find(*at).is_some())
                .map(|&e| entry(e, !fresh.contains(&e)))
                .chain(
                    fresh
                        .iter()
                        .filter(|e| !kept.contains(e))
                        .map(|&e| entry(e, false)),
                )
                .collect(),
        }
    }

    /// The task picked in the calendar's list, as an index into `tasks`: none
    /// while the keys are the grid's.
    fn cal_picked(&self) -> Option<usize> {
        let view = self.cal.as_ref()?;
        if view.focus == Focus::Grid {
            return None;
        }
        self.entries().get(view.row).and_then(|e| self.find(e.at))
    }

    /// A key in the calendar: the day's moves, the list's picks, and the keys
    /// that act otherwise here. `None` for the board's own (`h`, `e`, `x`,
    /// `y` ...), which act on the task picked in the list as on the board's.
    fn cal_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        let shown = self.entries();
        let rows = shown.len();
        let visible = self.visible();
        let day_rows = {
            let view = self.cal.as_ref()?;
            cal::listed(&visible, view.day, &self.tz).len()
        };
        let has_unknown = cal::not_known_line(&visible).is_some();
        let picked = self.cal_picked();
        let marked = !self.marked.is_empty();
        let today = self.today;
        let mut view = self.cal.clone()?;
        let day = |view: &mut CalView, to: Option<Date>| {
            if let Some(to) = to {
                view.day = to;
            }
            view.focus = Focus::Grid;
            view.row = 0;
            view.scroll = 0;
        };
        let at = view.day;
        let by = |span: jiff::Span| at.checked_add(span).ok();
        let grid = view.focus == Focus::Grid;
        match key.code {
            KeyCode::Char('c') => {
                self.close_cal();
                return Some(Outcome::Nothing);
            }
            KeyCode::Esc if marked => return None,
            KeyCode::Esc if self.filter.is_some() => {
                self.clear_filter();
                return Some(Outcome::Nothing);
            }
            KeyCode::Esc if view.from_board => {
                self.close_cal();
                return Some(Outcome::Nothing);
            }
            KeyCode::Esc => return Some(Outcome::Quit),
            KeyCode::Left => day(&mut view, by(jiff::Span::new().days(-1))),
            KeyCode::Right => day(&mut view, by(jiff::Span::new().days(1))),
            KeyCode::PageUp => day(&mut view, by(jiff::Span::new().months(-1))),
            KeyCode::PageDown => day(&mut view, by(jiff::Span::new().months(1))),
            KeyCode::Char('T') => day(&mut view, Some(today)),
            KeyCode::Up if grid => day(&mut view, by(jiff::Span::new().weeks(-1))),
            KeyCode::Down if grid => day(&mut view, by(jiff::Span::new().weeks(1))),
            KeyCode::Char('g') | KeyCode::Home if grid => day(&mut view, Some(at.first_of_month())),
            KeyCode::Char('G') | KeyCode::End if grid => day(&mut view, Some(at.last_of_month())),
            KeyCode::Char('j') if grid => {
                if day_rows > 0 {
                    view.focus = Focus::Day;
                    view.row = 0;
                }
            }
            KeyCode::Char('k') if grid => {}
            KeyCode::Char('j') | KeyCode::Down => {
                view.row = (view.row + 1).min(rows.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => match view.row {
                0 => view.focus = Focus::Grid,
                n => view.row = n - 1,
            },
            KeyCode::Char('g') | KeyCode::Home => view.row = 0,
            KeyCode::Char('G') | KeyCode::End => view.row = rows.saturating_sub(1),
            KeyCode::Tab | KeyCode::BackTab => {
                // The grid, the day's list, what has no date, round; a list
                // with nothing in it is passed by.
                let order = [Focus::Grid, Focus::Day, Focus::Unknown];
                let there = |f: Focus| match f {
                    Focus::Grid => true,
                    Focus::Day => day_rows > 0,
                    Focus::Unknown => has_unknown,
                };
                let at = order.iter().position(|&f| f == view.focus).unwrap_or(0);
                let step = if key.code == KeyCode::Tab { 1 } else { 2 };
                let mut next = at;
                loop {
                    next = (next + step) % order.len();
                    if there(order[next]) {
                        break;
                    }
                }
                view.focus = order[next];
                view.row = 0;
                view.scroll = 0;
            }
            KeyCode::Char(' ') => {
                let Some(i) = picked else {
                    return Some(Outcome::Nothing);
                };
                let Ref { id, stamp } = self.held(i);
                if self.marked.remove(&id) != Some(stamp) {
                    self.marked.insert(id, stamp);
                }
                view.row = (view.row + 1).min(rows.saturating_sub(1));
            }
            // `d` is done: on a task done already, card or sub item, it keeps
            // it, and its done time. Opening one again is a move (`h`, `s`,
            // `t`), as on the board.
            KeyCode::Char('d') if !marked => {
                let i = picked?;
                if self.tasks[i].status != Status::Done {
                    return None;
                }
                self.note = Some(format!("{} is done already.", self.tasks[i].id));
                return Some(Outcome::Nothing);
            }
            // A new task on the picked day: its date filled in, the cursor
            // before it.
            KeyCode::Char('a') => {
                let mut input = match view.focus {
                    Focus::Unknown => Input::new(InputKind::Add, String::new()),
                    _ => Input::new(InputKind::Add, format!(" //{}", view.day)),
                };
                input.cursor = 0;
                self.input = Some(input);
                return Some(Outcome::Nothing);
            }
            KeyCode::Char('z') => {
                self.message =
                    Some("the calendar lists each task on its own: z folds on the board".into());
                return Some(Outcome::Nothing);
            }
            _ => return None,
        }
        // The pick moved: what left the list goes, and the pick stays on the
        // task it moved to, wherever the list without it puts it.
        let target = (view.focus != Focus::Grid && view.kept.is_some())
            .then(|| shown.get(view.row).map(|e| (e.group, e.at)))
            .flatten();
        view.kept = None;
        self.cal = Some(view);
        if let Some(target) = target {
            let fresh = self.fresh_entries();
            let row = fresh
                .iter()
                .position(|&e| e == target)
                .or_else(|| fresh.iter().position(|e| e.1 == target.1));
            if let (Some(row), Some(view)) = (row, self.cal.as_mut()) {
                view.row = row;
            }
        }
        Some(Outcome::Nothing)
    }

    /// Where task `i` is now, for a note: `due Fri 27 Nov`, `done on Wed 25
    /// Nov`, `open, with no date`.
    fn whereabouts(&self, i: usize) -> String {
        let t = &self.tasks[i];
        match (cal::done_day(t), t.status, t.due) {
            (Some(day), _, _) => format!("done on {}", day_words(day, self.today)),
            (None, Status::Done, _) => "done".into(),
            (None, _, Some(due)) => format!("due {}", day_words(due.date, self.today)),
            (None, _, None) => "open, with no date".into(),
        }
    }

    /// After a change made in the calendar, `before` the list as it was: what
    /// left it stays, faint, and the note says where it went; a task added
    /// on the day is picked, and one added elsewhere is said to be.
    fn after_cal_change(&mut self, before: Vec<(Group, Ref)>, adding: bool) {
        let mut said = Vec::new();
        if adding {
            let at = self.held(self.tasks.len() - 1);
            let shown = self.entries().iter().position(|e| e.at == at);
            match (shown, self.cal.as_mut()) {
                (Some(n), Some(view)) => {
                    if view.focus == Focus::Grid {
                        view.focus = Focus::Day;
                    }
                    view.row = n;
                }
                _ => {
                    let i = self.tasks.len() - 1;
                    let id = self.tasks[i].id;
                    said.push(match self.tasks[i].due {
                        Some(due) => {
                            format!("Added {id}, due {}.", day_words(due.date, self.today))
                        }
                        None => format!("Added {id}, with no date."),
                    });
                }
            }
        } else {
            let fresh: Vec<Ref> = self.fresh_entries().iter().map(|&(_, r)| r).collect();
            let mut left: Vec<Ref> = before
                .iter()
                .map(|&(_, r)| r)
                .filter(|r| !fresh.contains(r))
                .collect();
            // Once each, a task listed in two groups too.
            left.sort_unstable();
            left.dedup();
            for at in left {
                if let Some(i) = self.find(at) {
                    said.push(format!("{} is {} now.", at.id, self.whereabouts(i)));
                }
            }
            if let Some(view) = self.cal.as_mut() {
                view.kept = Some(before);
            }
        }
        if !said.is_empty() {
            let said = said.join(" ");
            self.note = Some(match self.note.take() {
                Some(note) => format!("{note} {said}"),
                None => said,
            });
        }
    }

    /// Makes `op` to the tasks file at `path` as it is on disk now, and saves
    /// it, under the file's lock (see [`store::update`]): a `halil add` from
    /// another terminal meanwhile is read first, never written over.
    pub fn save_change(&mut self, path: &Path, op: Op) {
        let made = store::update(path, |fresh| {
            self.replace_tasks(std::mem::take(fresh));
            self.apply(op);
            fresh.clone_from(&self.tasks);
            Ok(((), true))
        });
        if let Err(e) = made {
            self.message = Some(e);
        }
    }

    /// Whether the card at `tasks[i]` is folded.
    fn is_folded(&self, i: usize) -> bool {
        self.folded.contains(&self.held(i))
    }

    /// Forgets the folds of cards that are gone.
    fn keep_folds_in_sight(&mut self) {
        let there: BTreeSet<Ref> = (0..self.tasks.len()).map(|i| self.held(i)).collect();
        self.folded.retain(|r| there.contains(r));
    }

    /// Keeps the folds in their file beside the tasks file at `path`, when
    /// they changed.
    pub fn keep_folds(&mut self, path: &Path) {
        // Under the tasks file's lock: two boards keeping folds at once each
        // write theirs whole, one after the other.
        if std::mem::take(&mut self.folds_changed)
            && let Err(e) =
                store::lock(path).and_then(|_held| save_folds(&folds_path(path), &self.folded))
        {
            self.message = Some(e);
        }
    }

    /// A card's due date as the board shows it: its own, or, folded outside
    /// Done, a hidden open sub item's when that's sooner or the card has none.
    fn card_due(&self, i: usize, parents: &[Option<usize>]) -> Option<Due> {
        let own = self.tasks[i].due;
        if !self.is_folded(i) || self.tasks[i].status == Status::Done {
            return own;
        }
        let hidden = task::sub_items(&self.tasks, parents, i)
            .into_iter()
            .filter(|&s| self.tasks[s].status != Status::Done)
            .filter_map(|s| self.tasks[s].due)
            .min();
        match (own, hidden) {
            (Some(own), Some(hidden)) => Some(own.min(hidden)),
            (own, hidden) => own.or(hidden),
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
    /// Done, where they're folded into their card's count, and under a card
    /// folded with `z`.
    fn column(&self, c: usize) -> Vec<usize> {
        let parents = task::parents(&self.tasks);
        let cards = self.cards(c, &parents);
        if COLUMNS[c] == Status::Done {
            return cards;
        }
        cards
            .into_iter()
            .flat_map(|i| {
                let subs = match self.is_folded(i) {
                    true => Vec::new(),
                    false => task::sub_items(&self.tasks, &parents, i),
                };
                std::iter::once(i).chain(subs)
            })
            .collect()
    }

    /// The cards of column `c`, as indices into `tasks`: in Done highest
    /// priority first, then oldest; elsewhere soonest due first.
    fn cards(&self, c: usize, parents: &[Option<usize>]) -> Vec<usize> {
        let mut ids: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| parents[i].is_none() && self.tasks[i].status == COLUMNS[c])
            .filter(|&i| self.shown(i, parents))
            .collect();
        if COLUMNS[c] == Status::Done {
            ids.sort_by_key(|&i| (Reverse(self.tasks[i].priority), self.tasks[i].id));
        } else {
            // As the list does: soonest due first (so late ones lead), undated
            // after, then highest priority, then oldest.
            // A folded card by the date it shows (see `card_due`).
            ids.sort_by_key(|&i| {
                let (t, due) = (&self.tasks[i], self.card_due(i, parents));
                (due.is_none(), due, Reverse(t.priority), t.id)
            });
        }
        ids
    }

    /// The picked card or sub item, as an index into `tasks`.
    fn picked(&self) -> Option<usize> {
        if self.cal.is_some() {
            return self.cal_picked();
        }
        self.column(self.col).get(self.row).copied()
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
        self.row = rows
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

    /// Keeps the pick on a row of its column.
    fn clamp(&mut self) {
        self.row = self.row.min(self.column(self.col).len().saturating_sub(1));
    }

    /// Takes the pick to column `c` (tab, the arrows): to the row drawn on
    /// the same line as the picked row's first, or the column's last row when
    /// none is on that line.
    fn pick_column(&mut self, c: usize) {
        if c == self.col {
            return;
        }
        let line = self.row.saturating_sub(self.scroll[self.col]);
        let rows = self.column(c).len();
        self.col = c;
        self.row = (self.scroll[c] + line).min(rows.saturating_sub(1));
    }

    /// Takes the tasks as they are on disk now, keeping the picked card picked
    /// while it's still in its column, and the marks on the rows still in sight.
    pub fn replace_tasks(&mut self, tasks: Vec<Task>) {
        let picked = self.picked_ref();
        self.tasks = tasks;
        self.keep_folds_in_sight();
        self.keep_marks_in_sight();
        if let Some(at) = picked {
            self.repick(at, self.col);
        }
        self.clamp();
    }

    /// Drops the marks on tasks that are gone (a task that took one's id
    /// meanwhile has another stamp), and on sub items whose card is done or
    /// folded: folded into its count, out of sight, they're no rows for a key
    /// to act on.
    fn keep_marks_in_sight(&mut self) {
        // In the calendar, every task is a row of its own.
        if self.cal.is_some() {
            let there: BTreeSet<Ref> = (0..self.tasks.len()).map(|i| self.held(i)).collect();
            self.marked
                .retain(|&id, &mut stamp| there.contains(&Ref { id, stamp }));
            return;
        }
        let parents = task::parents(&self.tasks);
        let in_sight: BTreeSet<Ref> = (0..self.tasks.len())
            .filter(|&i| {
                parents[i]
                    .is_none_or(|p| self.tasks[p].status != Status::Done && !self.is_folded(p))
            })
            .map(|i| self.held(i))
            .collect();
        self.marked
            .retain(|&id, &mut stamp| in_sight.contains(&Ref { id, stamp }));
    }

    pub fn apply(&mut self, op: Op) {
        let now = self.when();
        // In the calendar, the list as it was: what the change takes off it
        // stays, faint, until the pick moves.
        let before: Option<Vec<(Group, Ref)>> = self
            .cal
            .as_ref()
            .map(|_| self.entries().iter().map(|e| (e.group, e.at)).collect());
        let adding = matches!(op, Op::Add { .. });
        // What the notes say after: a done repeat opened again, and a moved
        // date that kept its step.
        let reopening: Vec<(Ref, String)> = (0..self.tasks.len())
            .filter_map(|i| task::reopened_said(&self.tasks, i).map(|s| (self.held(i), s)))
            .collect();
        // Under a filter, what's shown: what the change takes out of it stays,
        // faint, until the filter changes.
        let filter_shown: Vec<Ref> = match &self.filter {
            Some(_) => {
                let parents = task::parents(&self.tasks);
                (0..self.tasks.len())
                    .filter(|&i| self.shown(i, &parents))
                    .map(|i| self.held(i))
                    .collect()
            }
            None => Vec::new(),
        };
        match op {
            Op::Move { at, to, follow } => {
                if let Some(i) = self.find(at) {
                    task::set_status(&mut self.tasks[i], to, &now);
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
                        task::set_status(task, to, &now);
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
                        task::set_status(task, to, &now);
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
                    task::set_status(task, Status::Done, &now);
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
                    done: None,
                    added: Some(Local::of(&now)),
                    every: parsed.every,
                    next: None,
                });
                self.pick(id);
            }
        }
        // A repeating task done: its next one, said.
        let rolled = task::roll_repeats(&mut self.tasks, &now);
        let mut said = Vec::new();
        if let Some(next) = task::next_said(&rolled, self.today) {
            said.push(match self.note.take() {
                // `Done, with its 2 open sub items.`, then when it's next.
                Some(note) => format!("{note} {}", next.trim_start_matches("Done. ")),
                None => next,
            });
        }
        for (at, reopened) in reopening {
            if self
                .find(at)
                .is_some_and(|i| self.tasks[i].status != Status::Done)
            {
                said.push(reopened);
            }
        }
        if let Some(kept) = self.filter.clone() {
            let parents = task::parents(&self.tasks);
            let added = adding.then(|| self.held(self.tasks.len() - 1));
            let mut left = false;
            for at in filter_shown.into_iter().chain(added) {
                if let Some(i) = self.find(at)
                    && !kept.shows(&self.tasks, &parents, i, self.today)
                    && self.filter_faint.insert(at)
                {
                    left = true;
                }
            }
            // The faint card is what it's about: no number to read.
            if left {
                said.push(format!("No longer matches {}.", kept.text));
            }
        }
        if !said.is_empty() {
            let said = said.join(" ");
            self.note = Some(match self.note.take() {
                Some(note) => format!("{note} {said}"),
                None => said,
            });
        }
        // A card gone to Done (d, l, a marked l) folds its sub items away.
        self.keep_folds_in_sight();
        self.keep_marks_in_sight();
        self.clamp();
        if let Some(before) = before {
            self.after_cal_change(before, adding);
        }
    }

    /// `z`: folds the picked card's sub items away, or unfolds them; on a sub
    /// item, its card's, the pick going to the card. Folding clears their
    /// marks, and says how many.
    fn fold(&mut self) {
        let Some(i) = self.picked() else {
            return;
        };
        let parents = task::parents(&self.tasks);
        let card = parents[i].unwrap_or(i);
        let id = self.tasks[card].id;
        if COLUMNS[self.col] == Status::Done {
            self.message = Some("in Done, sub items are folded into the count".into());
            return;
        }
        if task::sub_items(&self.tasks, &parents, card).is_empty() {
            self.message = Some(format!("{id} has no sub items to fold"));
            return;
        }
        let at = self.held(card);
        let place = |board: &Board| {
            let parents = task::parents(&board.tasks);
            board
                .cards(board.col, &parents)
                .iter()
                .position(|&c| c == card)
        };
        let was = place(self);
        let mut said = Vec::new();
        let folding = !self.folded.remove(&at);
        if folding {
            self.folded.insert(at);
            let marked = self.marked.len();
            self.keep_marks_in_sight();
            match marked - self.marked.len() {
                0 => {}
                1 => said.push("Cleared the mark on its sub item.".to_string()),
                n => said.push(format!("Cleared the marks on its {n} sub items.")),
            }
        }
        // A folded card sorts by the date it shows: when that moved it, why.
        if place(self) != was {
            said.push(match folding {
                true => {
                    let shown = Task {
                        due: self.card_due(card, &parents),
                        ..self.tasks[card].clone()
                    };
                    let days = due_in_days(&shown, self.today).unwrap_or_default();
                    format!("Sorted by its sub item's date now: {}.", due_in_words(days))
                }
                false => "Sorted by its own date again.".into(),
            });
        }
        self.note = (!said.is_empty()).then(|| said.join(" "));
        self.folds_changed = true;
        self.pick(id);
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
        let now = self.when();
        let mut cards = BTreeSet::new();
        let mut n = 0;
        for (i, parent) in parents.into_iter().enumerate() {
            if let Some(p) = parent
                && ids.contains(&self.tasks[p].id)
                && self.tasks[i].status != Status::Done
            {
                task::set_status(&mut self.tasks[i], Status::Done, &now);
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
                input.edited(input.cursor..input.cursor, jamo.len_utf8());
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
        if self.cal.is_some()
            && let Some(outcome) = self.cal_key(key)
        {
            return outcome;
        }
        let last = self.column(self.col).len().saturating_sub(1);
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Quit,
            // No other chord is its letter: Ctrl-L, a redraw elsewhere, would
            // move a card, Ctrl-D finish one.
            KeyCode::Char(_) if ctrl => {}
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Esc => {
                if !self.marked.is_empty() {
                    self.marked.clear();
                } else if self.filter.is_some() {
                    self.clear_filter();
                } else {
                    return Outcome::Quit;
                }
            }
            KeyCode::Char('/') => self.open_filter(),
            KeyCode::Char(' ') => {
                if let Some(i) = self.picked() {
                    let Ref { id, stamp } = self.held(i);
                    if self.marked.remove(&id) != Some(stamp) {
                        self.marked.insert(id, stamp);
                    }
                    self.row = (self.row + 1).min(last);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.row = (self.row + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.row = self.row.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.row = 0,
            KeyCode::Char('G') | KeyCode::End => self.row = last,
            KeyCode::Right => self.pick_column((self.col + 1).min(COLUMNS.len() - 1)),
            KeyCode::Left => self.pick_column(self.col.saturating_sub(1)),
            KeyCode::Tab => self.pick_column((self.col + 1) % COLUMNS.len()),
            KeyCode::BackTab => self.pick_column((self.col + COLUMNS.len() - 1) % COLUMNS.len()),
            KeyCode::Char('h') => return self.shift(-1),
            KeyCode::Char('l') => return self.shift(1),
            KeyCode::Char('s') => return self.send(Status::Doing),
            KeyCode::Char('t') => return self.send(Status::Todo),
            KeyCode::Char('d') => return self.finish(),
            KeyCode::Char('x') | KeyCode::Delete => self.ask_removal(),
            KeyCode::Char('y') => return self.copy(),
            KeyCode::Char('z') => self.fold(),
            KeyCode::Char('c') => self.open_cal(true),
            KeyCode::Char('a') => self.input = Some(Input::new(InputKind::Add, String::new())),
            KeyCode::Char('A') => {
                if let Some(i) = self.picked() {
                    let card = task::parents(&self.tasks)[i].unwrap_or(i);
                    if task::is_sub_item(&self.tasks, &self.tasks[card]) {
                        let id = self.tasks[card].id;
                        self.message = Some(format!("{id} is a sub item: they go one level deep"));
                    } else {
                        // Folded, it opens: the new one shows when it's added.
                        if self.folded.remove(&self.held(card)) {
                            self.folds_changed = true;
                        }
                        let kind = InputKind::Sub(self.held(card));
                        self.input = Some(Input::new(kind, String::new()));
                    }
                }
            }
            KeyCode::Char('e') => {
                if !self.marked.is_empty() {
                    self.input = Some(Input::new(InputKind::EditMarked, String::new()));
                } else if let Some(i) = self.picked() {
                    // Its title and its date, `//2026-11-27`, the cursor after the
                    // title: typing goes on with it, as ever.
                    let (line, title_end) = parse::edit_line(&self.tasks[i], self.now());
                    let mut input = Input::new(InputKind::Edit(self.held(i)), line);
                    input.cursor = title_end;
                    self.input = Some(input);
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
            false => {
                // The board's order; then, marked in the calendar, a sub
                // item the board folds away, by id.
                let mut order: Vec<usize> =
                    (0..COLUMNS.len()).flat_map(|c| self.column(c)).collect();
                let mut rest: Vec<usize> = (0..self.tasks.len())
                    .filter(|i| !order.contains(i))
                    .collect();
                rest.sort_by_key(|&i| self.tasks[i].id);
                order.extend(rest);
                order
                    .into_iter()
                    .filter(|&i| self.is_marked(i))
                    .map(|i| self.tasks[i].title.as_str())
                    .collect()
            }
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
        // By its own column: in the calendar, the board's isn't where it is.
        let to = self
            .picked()
            .and_then(|i| shifted(self.tasks[i].status, by));
        match (self.picked(), to) {
            (Some(i), Some(to)) => Outcome::Change(Op::Move {
                at: self.held(i),
                to,
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

    /// The time dates are read as of: now, or the start of the board's day
    /// when that isn't today (a test's day).
    fn now(&self) -> DateTime {
        let now = Zoned::now().datetime();
        match now.date() == self.today {
            true => now,
            false => self.today.to_datetime(jiff::civil::Time::midnight()),
        }
    }

    /// The time a change is made at, with the zone halil runs in: now, or
    /// noon on the board's day when that isn't today (a test's day).
    fn when(&self) -> Zoned {
        let now = Zoned::now();
        match now.date() == self.today {
            true => now,
            false => self
                .today
                .at(12, 0, 0, 0)
                .to_zoned(now.time_zone().clone())
                .unwrap_or(now),
        }
    }

    /// The date of the task an `e` is typing for, if it has one.
    fn editing_due(&self) -> Option<Due> {
        match self.input.as_ref()?.kind {
            InputKind::Edit(at) => self.find(at).and_then(|i| self.tasks[i].due),
            _ => None,
        }
    }

    fn input_key(&mut self, key: KeyEvent) -> Outcome {
        let (now, today, own) = (self.now(), self.today, self.editing_due());
        let cal_room = self.cal_room;
        let Some(input) = self.input.as_mut() else {
            return Outcome::Nothing;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let filtering = input.kind == InputKind::Filter;
        // The filter line: enter keeps it, esc drops it (the one kept before
        // comes back).
        if filtering {
            match key.code {
                KeyCode::Enter => return self.keep_filter(),
                KeyCode::Esc => return self.drop_typed_filter(),
                KeyCode::Char('c') if ctrl => return self.drop_typed_filter(),
                _ => {}
            }
        }
        // While the calendar is up, the arrows and the page keys are its, and
        // esc closes it, the words left as they are.
        if input.cal.is_some() && cal_room && !ctrl {
            let by = match key.code {
                KeyCode::Left => Some(jiff::Span::new().days(-1)),
                KeyCode::Right => Some(jiff::Span::new().days(1)),
                KeyCode::Up => Some(jiff::Span::new().weeks(-1)),
                KeyCode::Down => Some(jiff::Span::new().weeks(1)),
                KeyCode::PageUp => Some(jiff::Span::new().months(-1)),
                KeyCode::PageDown => Some(jiff::Span::new().months(1)),
                _ => None,
            };
            if let Some(by) = by {
                input.move_day(by, own, now, today);
                return Outcome::Nothing;
            }
            if key.code == KeyCode::Esc {
                input.cal = None;
                return Outcome::Nothing;
            }
        }
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Char('c') if ctrl => self.input = None,
            KeyCode::Char('a') if ctrl => input.move_to(0),
            KeyCode::Char('e') if ctrl => input.move_to(input.text.len()),
            KeyCode::Char('u') if ctrl => {
                input.text.replace_range(..input.cursor, "");
                input.edited(0..input.cursor, 0);
                input.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let before = input.text[..input.cursor].trim_end();
                let kept = before.rfind(' ').map_or(0, |i| i + 1);
                input.text.replace_range(kept..input.cursor, "");
                input.edited(kept..input.cursor, 0);
                input.cursor = kept;
            }
            KeyCode::Char(c) if !ctrl => {
                input.text.insert(input.cursor, c);
                input.edited(input.cursor..input.cursor, c.len_utf8());
                input.cursor += c.len_utf8();
                // The second `/` of a `//` opens the calendar under it; not
                // in the filter line, where `by` and `late` aren't days.
                if c == '/' && !filtering && input.text[..input.cursor].ends_with("//") {
                    input.cal = Some(Cal {
                        at: input.cursor - 2,
                    });
                }
            }
            KeyCode::Backspace => {
                let prev = input.prev();
                input.text.replace_range(prev..input.cursor, "");
                input.edited(prev..input.cursor, 0);
                input.cursor = prev;
            }
            KeyCode::Left => input.move_to(input.prev()),
            KeyCode::Right => input.move_to(input.next()),
            KeyCode::Enter => {
                input.cal = None;
                let (kind, words) = (input.kind, [input.text.clone()]);
                return self.submit(kind, &words);
            }
            _ => {}
        }
        if filtering {
            self.filter_typed();
        }
        Outcome::Nothing
    }

    /// Enter on the filter line: the filter kept, the keys the board's again;
    /// a `//` or `!` that doesn't read, refused, the words kept.
    fn keep_filter(&mut self) -> Outcome {
        let text = self
            .input
            .as_ref()
            .map(|i| i.text.clone())
            .unwrap_or_default();
        match filter::read(&text, self.now()) {
            Ok(kept) => {
                let before = self.board_pick();
                self.input = None;
                self.filter_was = None;
                self.filter = (!kept.is_empty()).then_some(kept);
                self.refilter(before);
            }
            Err(e) => self.message = Some(e),
        }
        Outcome::Nothing
    }

    /// Esc on the filter line: what was typed dropped, the filter kept
    /// before back.
    fn drop_typed_filter(&mut self) -> Outcome {
        let before = self.board_pick();
        self.input = None;
        self.filter = self.filter_was.take().flatten();
        self.refilter(before);
        Outcome::Nothing
    }

    /// Esc on the board with a filter kept: cleared, the pick kept.
    fn clear_filter(&mut self) {
        let before = self.board_pick();
        self.filter = None;
        self.refilter(before);
    }

    /// Enter on the input: the change its words ask for, or why they can't be
    /// read, the words kept.
    fn submit(&mut self, kind: InputKind, words: &[String; 1]) -> Outcome {
        let now = self.now();
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
                Ok(task) if parent.is_some() && task.every.is_some() => {
                    self.message = Some(crate::REPEATS_WITH_ITS_CARD.into());
                    Outcome::Nothing
                }
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
        let repeats = tasks.iter().any(|t| t.every.is_some());
        let read = |keep_title_words: bool| {
            parse::change(
                words,
                now,
                |tag| {
                    tasks.iter().any(|t| {
                        t.tags.iter().any(|own| own == tag)
                            && !(keep_title_words && in_title(t, tag))
                    })
                },
                repeats,
            )
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
        // A repeat the words left on is said, the line changing anything else
        // or not.
        self.note = tasks.iter().find_map(|&t| crate::still_said(&change, t));
        self.input = None;
        match changes {
            true => Outcome::Change(Op::Edit { refs, change }),
            false => Outcome::Nothing,
        }
    }

    /// The screen, `w` by `h`, as one line a row, and where the cursor goes
    /// (while typing a task).
    fn draw(&mut self, w: usize, h: usize) -> (Vec<Line>, Option<(u16, u16)>) {
        let body = match self.cal.is_some() {
            true => self.draw_cal(w, h),
            false => self.draw_board(w, h),
        };
        let Some(mut lines) = body else {
            let what = if self.cal.is_some() {
                "calendar"
            } else {
                "board"
            };
            let mut row = Row::default();
            row.push(
                &truncate(&format!("  Make the window larger for the {what}"), w),
                Style::fg(QUIET),
            );
            return (vec![row.spans], None);
        };
        let parents = task::parents(&self.tasks);
        self.draw_bottom(&mut lines, &parents, w, h)
    }

    /// The board's rows above the row above the footer: none when the screen
    /// is too small for it.
    fn draw_board(&mut self, w: usize, h: usize) -> Option<Vec<Line>> {
        let col_w = w.saturating_sub(2 * MARGIN + (COLUMNS.len() - 1) * GAP) / COLUMNS.len();
        if col_w < 12 || h < 7 {
            return None;
        }
        let body_h = h - 5;
        let columns: Vec<Vec<usize>> = (0..COLUMNS.len()).map(|c| self.column(c)).collect();
        let parents = task::parents(&self.tasks);
        let x = |c: usize| MARGIN + c * (col_w + GAP);
        let edge = x(COLUMNS.len() - 1) + col_w;
        // Each column's rows, drawn, one line each. The picked one, shown
        // whole, may run over the columns to its right, or take more than a
        // line: it's drawn over the rest (`over`: its column, its line in the
        // body, its rows), and the column scrolls to show it all.
        let mut cells: Vec<Vec<Row>> = Vec::new();
        let mut over: Option<(usize, usize, Vec<Row>)> = None;
        for (c, rows) in columns.iter().enumerate() {
            let drawn_whole = |n: usize| {
                let i = rows[n];
                let look = Look {
                    width: col_w,
                    room: edge - x(c),
                    picked: c == self.col && n == self.row,
                    marked: self.is_marked(i),
                    today: self.today,
                    late_count: false,
                };
                if parents[i].is_some() {
                    sub_item(&self.tasks[i], look)
                } else {
                    let subs = task::sub_items(&self.tasks, &parents, i);
                    let count = task::progress(&self.tasks, &subs);
                    if COLUMNS[c] == Status::Done || !self.is_folded(i) {
                        return card(&self.tasks[i], count.as_deref(), look);
                    }
                    // Folded, it never hides what's due: a late open sub
                    // item reddens the count, and the soonest gives the card
                    // its date when that's sooner.
                    let late = subs
                        .iter()
                        .any(|&s| due_in_days(&self.tasks[s], self.today).is_some_and(|d| d < 0));
                    let shown = Task {
                        due: self.card_due(i, &parents),
                        ..self.tasks[i].clone()
                    };
                    let count = count.map(|n| format!("▸ {n}"));
                    let look = Look {
                        late_count: late,
                        ..look
                    };
                    card(&shown, count.as_deref(), look)
                }
            };
            // A change took it out of the filter: faint, until the filter
            // changes.
            let drawn = |n: usize| {
                let mut rows_drawn = drawn_whole(n);
                if self.faint_under_filter(rows[n]) {
                    faint(&mut rows_drawn);
                }
                rows_drawn
            };
            // The picked column scrolls to keep the pick in view; another
            // stays where it was, as far as its rows go.
            let s = if c == self.col {
                let pick = self.row;
                let tall = if pick < rows.len() {
                    drawn(pick).len()
                } else {
                    1
                };
                let mut s = self.scroll[c].min(pick);
                s = s.max((pick + tall).saturating_sub(body_h)).min(pick);
                s.min((rows.len() + tall - 1).saturating_sub(body_h))
            } else {
                self.scroll[c].min(rows.len().saturating_sub(body_h))
            };
            let mut lines = Vec::new();
            for n in s..rows.len().min(s + body_h) {
                let mut row = drawn(n);
                if row.len() > 1 || row[0].width > col_w {
                    over = Some((c, n - s, row));
                    lines.push(Row::default());
                } else {
                    lines.push(row.remove(0));
                }
            }
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
                } else if r == 0 && cards.is_empty() && self.filter.is_none() {
                    cell.push(
                        "   Nothing here",
                        Style {
                            italic: true,
                            ..Style::fg(QUIETER)
                        },
                    );
                }
                row.pad_to(x(c), Style::default());
                row.append(&cell);
            }
            lines.push(row.spans);
        }
        // The picked row whole, over its neighbours: they're cut where it
        // ends, a double-width letter there covered whole.
        if let Some((c, at, rows)) = over {
            for (j, top) in rows.iter().enumerate() {
                if let Some(line) = lines.get_mut(3 + at + j).filter(|_| at + j < body_h) {
                    *line = overlay(line, x(c), top, styles(true).0);
                }
            }
        }
        // A filter that shows nothing anywhere says so, mid-board.
        if self.filter.is_some() && columns.iter().all(Vec::is_empty) {
            let said = "Nothing matches";
            let mut row = Row::default();
            row.pad_to(w.saturating_sub(said.width()) / 2, Style::default());
            row.push(
                said,
                Style {
                    italic: true,
                    ..Style::fg(QUIETER)
                },
            );
            lines[3 + body_h / 2] = row.spans;
        }
        Some(lines)
    }

    /// The calendar's rows above the row above the footer: the heading, the
    /// month or the picked day's week, the date-not-known row, and the list
    /// under them; none when the screen is too small for it.
    fn draw_cal(&mut self, w: usize, h: usize) -> Option<Vec<Line>> {
        let view = self.cal.clone()?;
        // Under a filter, every count is of what it shows.
        let visible = self.visible();
        let counts = cal::counts(&visible);
        let on = |d: Date| counts.get(&d).copied().unwrap_or_default();
        let weeks = cal::weeks(view.day);
        let month: Vec<cal::Counts> = weeks.iter().flatten().flatten().map(|&d| on(d)).collect();
        let week = cal::week_of(view.day);
        let week_counts: Vec<cal::Counts> = week.iter().map(|&d| on(d)).collect();
        let shape = cal::shape(&month, &week_counts, w, h)?;
        let form = shape.form();
        let cw = cal::cell_width(w);
        let today = self.today;
        let mut lines: Vec<Line> = Vec::new();

        let mut heading = Row::default();
        for (x, text, legend) in cal::heading(view.day, form, cw) {
            heading.pad_to(x, Style::default());
            let style = if legend {
                Style::fg(QUIET)
            } else {
                Style::default()
            };
            heading.push(&text, style);
        }
        lines.push(heading.spans);

        // A day's cell: its head at the right (the day, or `We 25`), then its
        // counts; picked, on the picked row's ground; today's head in blue;
        // due in amber, red once the day is gone; done quiet.
        let cell = |date: Date, head: &str| -> Vec<Row> {
            let picked = date == view.day;
            let ground = Style {
                bg: picked.then_some(HIGHLIGHT),
                ..Style::default()
            };
            let head_fg = match (picked, date == today) {
                (true, _) => Some(ON_HIGHLIGHT),
                (false, true) => Some(BLUE),
                (false, false) => None,
            };
            let mut top = Row::default();
            top.pad_to(cw.saturating_sub(head.width()), ground);
            top.push(
                head,
                Style {
                    fg: head_fg,
                    bold: picked,
                    ..ground
                },
            );
            let mut rows = vec![top];
            for line in cal::say(on(date), form, cw) {
                let mut row = Row::default();
                row.pad_to(cw.saturating_sub(cal::plain(&line).width()), ground);
                for (part, text) in &line {
                    let fg = match part {
                        cal::Part::Due if date < today => RED,
                        cal::Part::Due => AMBER,
                        cal::Part::Done | cal::Part::Between => QUIET,
                    };
                    row.push(
                        text,
                        Style {
                            fg: Some(fg),
                            ..ground
                        },
                    );
                }
                rows.push(row);
            }
            for row in &mut rows {
                row.pad_to(cw, ground);
            }
            rows
        };
        let push_week = |lines: &mut Vec<Line>, days: [Option<(Date, String)>; 7]| {
            let mut rows = vec![Row::default(); 1 + form.lines()];
            for (d, day) in days.iter().enumerate() {
                let Some((date, head)) = day else {
                    continue;
                };
                for (row, part) in rows.iter_mut().zip(cell(*date, head)) {
                    row.pad_to(cal::cell_x(d, cw), Style::default());
                    row.append(&part);
                }
            }
            lines.extend(rows.into_iter().map(|r| r.spans));
        };
        match shape {
            cal::Shape::Month(_) => {
                let mut names = Row::default();
                for (d, name) in ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"]
                    .iter()
                    .enumerate()
                {
                    names.pad_to(cal::cell_x(d, cw), Style::default());
                    names.push(&format!("{name:>cw$}"), Style::fg(QUIETER));
                }
                lines.push(names.spans);
                for days in weeks {
                    push_week(
                        &mut lines,
                        days.map(|d| d.map(|d| (d, format!("{:>2}", d.day())))),
                    );
                }
            }
            cal::Shape::Week(_) => {
                push_week(&mut lines, week.map(|d| Some((d, cal::strip_head(d, cw)))));
            }
        }

        // What has no date, in one place, always in sight.
        if let Some(said) = cal::not_known_line(&visible) {
            let style = match view.focus {
                Focus::Unknown => Style {
                    fg: Some(ON_HIGHLIGHT),
                    bg: Some(HIGHLIGHT),
                    ..Style::default()
                },
                _ => Style::fg(QUIET),
            };
            let mut row = Row::default();
            row.pad_to(MARGIN - 1, Style::default());
            row.push(&truncate(&format!(" {said} "), w - MARGIN), style);
            lines.push(row.spans);
        }

        // The list: its group's name on the first of each, then the task as
        // a card is drawn, a sub item with its card's title after it.
        let room = (h - 2).saturating_sub(lines.len());
        let entries = self.entries();
        let label_w = 6;
        let list_w = w.saturating_sub(MARGIN + label_w + MARGIN);
        let parents = task::parents(&self.tasks);
        let row = view.row.min(entries.len().saturating_sub(1));
        let listing = view.focus != Focus::Grid;
        let drawn: Vec<Vec<Row>> = entries
            .iter()
            .enumerate()
            .map(|(n, e)| {
                let Some(i) = self.find(e.at) else {
                    return vec![Row::default()];
                };
                let look = Look {
                    width: list_w,
                    room: list_w,
                    picked: listing && n == row,
                    marked: self.is_marked(i),
                    today,
                    late_count: false,
                };
                // Its card's title, where its own stays whole beside it and a
                // date's words.
                let under = parents[i]
                    .map(|p| format!("under {}", truncate(&self.tasks[p].title, 24)))
                    .filter(|u| {
                        self.tasks[i].title.width() + 2 + u.width() + 12 <= list_w.saturating_sub(4)
                    });
                let mut rows = card(&self.tasks[i], under.as_deref(), look);
                if e.faint || self.faint_under_filter(i) {
                    faint(&mut rows);
                }
                rows
            })
            .collect();
        let tall = |n: usize| drawn[n].len();
        let mut scroll = view.scroll.min(row);
        while scroll < row && (scroll..=row).map(tall).sum::<usize>() > room {
            scroll += 1;
        }
        let mut listed = Vec::new();
        'entries: for n in scroll..entries.len() {
            for (j, part) in drawn[n].iter().enumerate() {
                if listed.len() == room {
                    break 'entries;
                }
                let first = j == 0 && (n == scroll || entries[n - 1].group != entries[n].group);
                let label = if first { entries[n].group.name() } else { "" };
                let mut line = Row::default();
                line.pad_to(MARGIN, Style::default());
                line.push(&format!("{label:<label_w$}"), Style::fg(QUIET));
                line.append(part);
                listed.push(line.spans);
            }
        }
        if entries.is_empty() && room > 0 {
            let mut line = Row::default();
            line.pad_to(MARGIN, Style::default());
            line.push(
                &truncate(
                    &format!(
                        "Nothing due, done or added on {}",
                        day_words(view.day, today)
                    ),
                    w - MARGIN,
                ),
                Style {
                    italic: true,
                    ..Style::fg(QUIETER)
                },
            );
            listed.push(line.spans);
        }
        lines.extend(listed);
        lines.resize(h - 2, Line::new());
        if let Some(view) = self.cal.as_mut() {
            view.scroll = scroll;
            view.row = row;
        }
        Some(lines)
    }

    /// The row above the footer, the footer, and the `//` calendar over them
    /// while it's up, after `lines`, the rows above them: the whole screen,
    /// and where the cursor goes.
    fn draw_bottom(
        &mut self,
        lines: &mut Vec<Line>,
        parents: &[Option<usize>],
        w: usize,
        h: usize,
    ) -> (Vec<Line>, Option<(u16, u16)>) {
        let mut lines = std::mem::take(lines);

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
        // The calendar's `//`, where it's drawn in the footer.
        let mut cal_x = None;
        // A kept filter, always in sight where the typing line is: the keys
        // give way before it does.
        let filter_said = self.filter.as_ref().map(|f| {
            let (n, m) = self.filter_count();
            format!("  / {} · {n} of {m} ", f.text)
        });
        let filtered = filter_said.is_some();
        if let (Some(said), None, None) = (&filter_said, &self.input, &self.asking) {
            footer.push(&truncate(said, w), Style::default());
        }
        match &self.input {
            Some(input) => {
                let (label, hint, example) = match input.kind {
                    InputKind::Add => (
                        "New task".to_string(),
                        "enter to add · esc to cancel",
                        "title //fri 6pm +tag !high",
                    ),
                    InputKind::Sub(at) => (
                        format!("Sub item of {}", at.id),
                        "enter to add · esc to cancel",
                        "title //fri 6pm +tag !high",
                    ),
                    InputKind::Edit(at) => (
                        format!("Edit {}", at.id),
                        "enter to change · esc to cancel",
                        "title //fri 6pm +tag -tag !high",
                    ),
                    InputKind::EditMarked => (
                        format!("Edit {} marked", self.marked.len()),
                        "enter to change · esc to cancel",
                        "//fri 6pm +tag -tag !high",
                    ),
                    InputKind::Filter => (
                        "/".to_string(),
                        "//fri  //by fri  //late",
                        "words +tag !high",
                    ),
                };
                footer.push(&format!("  {label}  "), Style::fg(QUIET));
                // The hint gives way to what's typed, so the line's end stays
                // in sight: its last part first (`esc to cancel`), then the rest.
                let typed_w = match input.text.is_empty() {
                    true => 0,
                    false => input.text.width() + 1,
                };
                let parts: Vec<&str> = hint.split(" · ").collect();
                let hint = (0..=parts.len())
                    .rev()
                    .map(|n| parts[..n].join(" · "))
                    .find(|h| {
                        let gap = if h.is_empty() { 0 } else { 4 };
                        footer.width + typed_w + h.width() + gap <= w
                    })
                    .unwrap_or_default();
                let hint = hint.as_str();
                let room = match hint.is_empty() {
                    true => w.saturating_sub(footer.width + 1),
                    false => w.saturating_sub(footer.width + hint.width() + 4),
                };
                let start = footer.width;
                let mut at = 0;
                if input.text.is_empty() {
                    footer.push(&truncate(example, room), Style::fg(QUIETER));
                } else {
                    let (shown, x, from) = view(&input.text, input.cursor, room);
                    footer.push(&shown, Style::default());
                    at = x;
                    cal_x = input.cal.map(|cal| match cal.at.checked_sub(from) {
                        Some(_) => start + input.text[from..cal.at].width(),
                        None => start,
                    });
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
            None if self.cal.is_some() => {
                // The calendar's keys: the moves while no task is picked, else
                // the keys that act on it, as the board's footer gives them.
                if !filtered {
                    footer.push(" ", Style::default());
                }
                let mut hints: Vec<(&str, &str, Option<u8>)> = match self.picked() {
                    None => vec![
                        // The way back, right after the moves and tab, at
                        // every width: a first look always sees it.
                        ("←→", "day", None),
                        ("↑↓", "week", Some(10)),
                        ("pgup pgdn", "month", Some(3)),
                        ("tab", "list", Some(9)),
                        ("c", "board", Some(8)),
                        ("/", "filter", Some(7)),
                        ("T", "today", Some(6)),
                        ("a", "add", Some(5)),
                        ("q", "quit", Some(2)),
                    ],
                    Some(i) => {
                        let sub = parents[i].is_some();
                        let status = self.tasks[i].status;
                        // As the board's, but the way back, which gives way
                        // last; and on a done task no `d`: it's done, and the
                        // moves (`h l`, `t`, `s`) open it again.
                        // The board's order (see `board_hints`), `c` the way
                        // back to it.
                        let mut hints = vec![
                            ("j k", "pick", Some(RANK_J_K)),
                            ("space", "mark", Some(RANK_SPACE)),
                            ("h l", "move", None),
                            ("t", "todo", Some(RANK_S_T)),
                            ("s", "doing", Some(RANK_S_T)),
                            ("d", "done", None),
                            ("tab", "next", Some(RANK_TAB)),
                            ("c", "board", Some(RANK_C)),
                            ("/", "filter", Some(RANK_FILTER)),
                            ("a", "add", None),
                            ("A", "sub", Some(RANK_A)),
                            ("e", "edit", None),
                            ("y", "copy", Some(RANK_Y)),
                            ("x", "remove", Some(RANK_X)),
                            ("q", "quit", Some(RANK_Q)),
                        ];
                        hints.retain(|&(keys, _, _)| match keys {
                            "h l" | "t" | "s" if sub => false,
                            "t" => status != Status::Todo,
                            "s" => status != Status::Doing,
                            "d" => status != Status::Done,
                            _ => true,
                        });
                        hints
                    }
                };
                if filtered {
                    hints.retain(|&(keys, _, _)| keys != "/");
                }
                for (keys, what) in
                    hints_with_clear(&hints, w.saturating_sub(footer.width), filtered)
                {
                    footer.push(&format!(" {keys} "), Style::fg(QUIET));
                    footer.push(&format!("{what} "), Style::fg(QUIETER));
                }
            }
            None => {
                // When they don't all fit, the keys give way by need: `tab`
                // first (the arrows go between columns too), then `q` (`esc`
                // quits too), then `j k`, then `t` and `x`, then `c`, then `s`,
                // and only then the rest. `d` goes last, and its word for what it does to
                // the picked row with it.
                let sub = self.picked().is_some_and(|i| parents[i].is_some());
                // `z` where it acts: on a card with sub items, or one of
                // them, outside Done.
                let folds = self
                    .picked()
                    .map(|i| parents[i].unwrap_or(i))
                    .filter(|&card| {
                        COLUMNS[self.col] != Status::Done
                            && !task::sub_items(&self.tasks, parents, card).is_empty()
                    });
                let fold = match folds.map(|card| self.is_folded(card)) {
                    Some(true) => "unfold",
                    _ => "fold",
                };
                if !filtered {
                    footer.push(" ", Style::default());
                }
                let mut hints = board_hints(sub, fold);
                // With a filter kept, its line says `/`.
                if filtered {
                    hints.retain(|&(keys, _, _)| keys != "/");
                }
                // Only the keys that act on what's picked: not the key to the
                // column it's in (in Done, `d` moves it back to Todo, as `t`
                // says), and none that moves it on a sub item, which moves
                // with its card only.
                hints.retain(|&(keys, _, _)| match keys {
                    "h l" | "t" | "s" if sub => false,
                    "t" => COLUMNS[self.col] != Status::Todo,
                    "s" => COLUMNS[self.col] != Status::Doing,
                    "d" => COLUMNS[self.col] != Status::Done,
                    "z" => folds.is_some(),
                    _ => true,
                });
                for (keys, what) in
                    hints_with_clear(&hints, w.saturating_sub(footer.width), filtered)
                {
                    footer.push(&format!(" {keys} "), Style::fg(QUIET));
                    footer.push(&format!("{what} "), Style::fg(QUIETER));
                }
            }
        }
        lines.push(footer.spans);

        // The calendar, above the typing line, its left edge at the `//`,
        // inside the screen; without room, the row above says so.
        self.cal_room = h > CAL_ROWS && w >= CAL_GRID + 2;
        if let (Some(x), Some(input)) = (cal_x, &self.input)
            && let Some(cal) = input.cal
        {
            if self.cal_room {
                let day = parse::marked_due(&input.text, cal.at, self.now())
                    .or(self.editing_due())
                    .map(|d| d.date);
                let due: BTreeSet<Date> = self
                    .tasks
                    .iter()
                    .filter(|t| t.status != Status::Done)
                    .filter_map(|t| t.due.map(|d| d.date))
                    .collect();
                let width = w.min(CAL_KEYS_W + 4);
                let card = calendar(day, self.today, &due, width);
                let (left, top) = (x.min(w - width), h - 1 - CAL_ROWS);
                for (j, row) in card.iter().enumerate() {
                    lines[top + j] = overlay(&lines[top + j], left, row, Style::default());
                }
            } else if message.width == 0 {
                let mut said = Row::default();
                said.push("  ", Style::default());
                said.push(
                    &truncate(NO_ROOM_FOR_CAL, w.saturating_sub(2)),
                    Style::default(),
                );
                lines[h - 2] = said.spans;
            }
        }
        (lines, cursor)
    }
}

/// The width of the calendar's keys, all of them, with two cells between.
const CAL_KEYS_W: usize = 43;

/// The calendar's card, `width` wide (22 at least), as its rows: the month of
/// `day` (or of `today`) in its top line, the weekdays faint, Sunday first,
/// then six weeks, `day` in the picked row's colours, `today` in the accent,
/// a day with something `due` in bold; its keys on the bottom line, whole
/// pairs, those that don't fit giving way from the right.
fn calendar(day: Option<Date>, today: Date, due: &BTreeSet<Date>, width: usize) -> Vec<Row> {
    let frame = Style::fg(QUIET);
    let pad = (width - 2 - CAL_GRID) / 2;
    let first = day.unwrap_or(today).first_of_month();
    let mut rows = Vec::new();

    let mut top = Row::default();
    top.push("╭ ", frame);
    top.push(&first.strftime("%B %Y").to_string(), Style::default());
    top.push(" ", frame);
    top.push(&"─".repeat(width - 1 - top.width), frame);
    top.push("╮", frame);
    rows.push(top);

    let side = |row: &mut Row, inside: Row| {
        row.push("│", frame);
        row.pad_to(1 + pad, Style::default());
        row.append(&inside);
        row.pad_to(width - 1, Style::default());
        row.push("│", frame);
    };
    let mut days = Row::default();
    days.push("Su Mo Tu We Th Fr Sa", Style::fg(QUIETER));
    let mut row = Row::default();
    side(&mut row, days);
    rows.push(row);

    for days in cal::weeks(first) {
        let mut inside = Row::default();
        for (d, date) in days.into_iter().enumerate() {
            if d > 0 {
                inside.push(" ", Style::default());
            }
            let Some(date) = date else {
                inside.push("  ", Style::default());
                continue;
            };
            let style = Style {
                bold: due.contains(&date),
                ..match (Some(date) == day, date == today) {
                    (true, _) => Style {
                        fg: Some(ON_HIGHLIGHT),
                        bg: Some(HIGHLIGHT),
                        ..Style::default()
                    },
                    (false, true) => Style::fg(BLUE),
                    (false, false) => Style::default(),
                }
            };
            inside.push(&format!("{:>2}", date.day()), style);
        }
        let mut row = Row::default();
        side(&mut row, inside);
        rows.push(row);
    }

    let mut bottom = Row::default();
    bottom.push("╰", frame);
    let mut used = 1;
    for (n, (keys, what)) in CAL_KEYS.iter().enumerate() {
        let pair = keys.width() + 1 + what.width();
        let gap = if n == 0 { 1 } else { 2 };
        // Room for it, a space after, and the corner.
        if used + gap + pair + 2 > width {
            break;
        }
        bottom.push(&" ".repeat(gap), frame);
        bottom.push(keys, Style::fg(QUIET));
        bottom.push(&format!(" {what}"), Style::fg(QUIETER));
        used += gap + pair;
    }
    bottom.push(" ", frame);
    bottom.push(&"─".repeat(width - 1 - bottom.width), frame);
    bottom.push("╯", frame);
    rows.push(bottom);
    rows
}

// The footer's order of giving way, first to go first: `tab`, `y`, `q`, `j k`
// (as since 0.3.0), `space`, the picked row's other column (`s`, `t`: `h l`
// says it too), `z`, `A`, `x`, `/`, `c`. Never: `h l`, `d`, `a`, `e`.
const RANK_TAB: u8 = 1;
const RANK_Y: u8 = 2;
const RANK_Q: u8 = 3;
const RANK_J_K: u8 = 4;
const RANK_SPACE: u8 = 5;
const RANK_S_T: u8 = 6;
const RANK_Z: u8 = 7;
const RANK_A: u8 = 8;
const RANK_X: u8 = 9;
const RANK_FILTER: u8 = 10;
const RANK_C: u8 = 11;

/// The hints that fit `room`, as [`giving_way`] gives them, and with a
/// filter kept, `esc clear` after them, its room taken first: it never goes.
fn hints_with_clear<'a>(
    hints: &[(&'a str, &'a str, Option<u8>)],
    room: usize,
    filtered: bool,
) -> Vec<(&'a str, &'a str)> {
    const CLEAR: (&str, &str) = ("esc", "clear");
    match filtered {
        false => giving_way(hints, room, 3),
        true => {
            let clear_w = 3 + CLEAR.0.width() + CLEAR.1.width();
            let mut shown = giving_way(hints, room.saturating_sub(clear_w), 3);
            shown.push(CLEAR);
            shown
        }
    }
}

/// The board's key hints, in their order on the footer, each with its rank
/// in giving way: a sub item picked (`sub`) or not, and `fold`'s word.
fn board_hints(sub: bool, fold: &'static str) -> Vec<(&'static str, &'static str, Option<u8>)> {
    vec![
        ("j k", "pick", Some(RANK_J_K)),
        ("space", "mark", Some(RANK_SPACE)),
        ("h l", "move", None),
        ("t", "todo", Some(RANK_S_T)),
        ("s", "doing", Some(RANK_S_T)),
        ("d", if sub { "done/open" } else { "done" }, None),
        ("tab", "column", Some(RANK_TAB)),
        ("a", "add", None),
        ("A", "sub", Some(RANK_A)),
        ("e", "edit", None),
        ("z", fold, Some(RANK_Z)),
        ("y", "copy", Some(RANK_Y)),
        ("x", "remove", Some(RANK_X)),
        ("q", "quit", Some(RANK_Q)),
        ("c", "calendar", Some(RANK_C)),
        ("/", "filter", Some(RANK_FILTER)),
    ]
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

/// `line` with `top` drawn over it from column `x`. What's under it is cut
/// where it starts and where it ends, never a letter in half: a double-width
/// one the start would split leaves a space in its own style, and one the end
/// would split is covered whole, in `ground`.
fn overlay(line: &Line, x: usize, top: &Row, ground: Style) -> Line {
    let mut end = x + top.width;
    let (mut before, mut after) = (Row::default(), Row::default());
    let mut col = 0;
    for span in line {
        for ch in span.text.chars() {
            let (a, b) = (col, col + ch.width().unwrap_or(0));
            col = b;
            if b <= x {
                before.push_char(ch, span.style);
            } else if a >= end {
                after.push_char(ch, span.style);
            } else if a < x {
                before.pad_to(x, span.style);
            } else if b > end {
                end = b;
            }
        }
    }
    before.pad_to(x, Style::default());
    before.append(top);
    before.pad_to(end, ground);
    before.append(&after);
    before.spans
}

/// `rows` drawn faint: what's left a list or a filter since, until the pick
/// or the filter moves.
fn faint(rows: &mut [Row]) {
    for row in rows {
        for span in &mut row.spans {
            span.style.fg = Some(QUIETER);
        }
    }
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
    /// A card's count in red: a sub item folded away under it is late.
    late_count: bool,
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

/// A task's due date as a row says it: its words (`fri`, `2 days ago`),
/// whether it's late, and how it repeats.
#[derive(Debug, Clone)]
struct DueWords {
    words: String,
    late: bool,
    step: Step,
}

/// A repeat as a row says it after the date: its words, faint (`weekly`),
/// or `*` where the title is cut and there's no room for them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    None,
    Words(String),
    Star,
}

impl DueWords {
    fn width(&self) -> usize {
        self.words.width()
            + match &self.step {
                Step::None => 0,
                Step::Words(s) => 1 + s.width(),
                Step::Star => 1,
            }
    }

    /// The same, its step's words left to `*`.
    fn starred(self) -> DueWords {
        let step = match self.step {
            Step::Words(_) => Step::Star,
            step => step,
        };
        DueWords { step, ..self }
    }
}

/// A task's due date in words, and whether it's late, when it's open and the
/// words fit `inner` beside `taken` columns and 8 of title.
fn due_fitting(task: &Task, today: Date, inner: usize, taken: usize) -> Option<DueWords> {
    due_words(task, today).filter(|due| due.width() + 2 + taken + 8 <= inner)
}

/// The due words at the right of a row, red when late; its step's words
/// after them, faint, or `*` in the date's own style.
fn push_due(row: &mut Row, due: Option<DueWords>, plain: Style, quiet: Style) {
    if let Some(due) = due {
        row.push("  ", plain);
        let style = match due.late {
            true => Style {
                fg: Some(RED),
                ..plain
            },
            false => quiet,
        };
        row.push(&due.words, style);
        match due.step {
            Step::None => {}
            Step::Words(words) => {
                let faint = if plain.bg.is_some() { QUIET } else { QUIETER };
                row.push(" ", plain);
                row.push(
                    &words,
                    Style {
                        fg: Some(faint),
                        ..plain
                    },
                );
            }
            Step::Star => row.push("*", style),
        }
    }
}

/// The width of a due date's words with the two spaces before them, or 0.
fn due_width(due: &Option<DueWords>) -> usize {
    due.as_ref().map_or(0, |due| due.width() + 2)
}

/// A due date that didn't fit at the right of the first row: at the right of
/// the last, two cells after what's there, when it fits; else on a row of its
/// own, at the right, under the rest of the picked row.
fn place_late_due(rows: &mut Vec<Row>, due: DueWords, width: usize, plain: Style, quiet: Style) {
    let last = rows.last_mut().expect("a row grown has its first line");
    let words_w = due.width();
    if last.width + 2 + words_w < width {
        last.pad_to(width - 1 - words_w - 2, plain);
        push_due(last, Some(due), plain, quiet);
        return;
    }
    // A row of its own; cut to fit when even that's short, its step first.
    let due = match due.width() + 3 <= width {
        true => due,
        false => DueWords {
            words: truncate(&due.words, width.saturating_sub(3)),
            step: Step::None,
            ..due
        },
    };
    let mut row = Row::default();
    row.pad_to(width.saturating_sub(due.width() + 3), plain);
    push_due(&mut row, Some(due), plain, quiet);
    rows.push(row);
}

/// A task's due date in words, and whether it's late, while it's open; and
/// how it repeats.
fn due_words(task: &Task, today: Date) -> Option<DueWords> {
    due_in_days(task, today).map(|days| DueWords {
        words: due_in_words(days),
        late: days < 0,
        step: match task.every {
            Some(every) => Step::Words(every.short()),
            None => Step::None,
        },
    })
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
    let count_style = match look.late_count {
        true => Style {
            fg: Some(RED),
            ..plain
        },
        false => quiet,
    };

    // Room inside the card: a space of padding at each end, the mark and its space.
    let inner = width.saturating_sub(4);
    let count_w = count.map_or(0, |c| c.width() + 2);
    // A repeat's words after the date where the title still fits whole
    // beside them; else `*` (`fri*`), the picked row's overlay spelling them.
    let fits = |due: &DueWords| due.width() + 2 + count_w + 8 <= inner;
    let title_whole = |due: &DueWords| task.title.width() + count_w + due.width() + 2 <= inner;
    let due = match due_words(task, today) {
        Some(due) if due.step == Step::None => Some(due).filter(fits),
        Some(due) if title_whole(&due) => Some(due),
        Some(due) => Some(due.starred()).filter(fits),
        None => None,
    };
    let room = inner - due_width(&due);
    let tags = (!task.tags.is_empty()).then(|| {
        let tags: Vec<String> = task.tags.iter().map(|t| format!("+{t}")).collect();
        tags.join(" ")
    });

    let shown = truncate(&task.title, room.saturating_sub(count_w));
    let used = shown.width() + count_w;
    let whole = shown == task.title
        && tags.as_ref().is_none_or(|t| used + 2 + t.width() <= room)
        && (due.is_some() || due_in_days(task, today).is_none())
        && due.as_ref().is_none_or(|due| due.step != Step::Star);
    if picked && !whole {
        let mut head = Row::default();
        card_head(&mut head, task, marked, plain);
        let whole = Whole {
            title: &task.title,
            title_style: title,
            count,
            count_style,
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
        row.push(count, count_style);
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
    count_style: Style,
    tags: Option<String>,
    due: Option<DueWords>,
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
        for (said, style) in [
            (whole.count.map(String::from), whole.count_style),
            (whole.tags, quiet),
        ] {
            if let Some(said) = said {
                row.push("  ", plain);
                row.push(&said, style);
            }
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
        Some(due) if due.width() + 2 + count_w + 8 <= inner => (Some(due), None),
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
            row.push(count, whole.count_style);
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
            count_style: quiet,
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
    /// The calendar, while it's up.
    cal: Option<Cal>,
}

/// The calendar a `//` typed in the input opens, or the cursor moved into its
/// words: where that `//` is, a byte index into the words. Its day is what
/// the words after the `//` read as (see [`parse::marked_due`]), else the
/// task's own on `e`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cal {
    at: usize,
}

/// The calendar's card: 7 days of 2 cells and a space between, in a frame.
const CAL_GRID: usize = 20;
const CAL_ROWS: usize = 9;
/// Its keys, on its bottom line.
const CAL_KEYS: [(&str, &str); 4] = [
    ("←→", "day"),
    ("↑↓", "week"),
    ("pgup pgdn", "month"),
    ("esc", "close"),
];
const NO_ROOM_FOR_CAL: &str = "no room for the calendar — type the date";

/// What the input is for: a new task, a new sub item of a card, the picked
/// card or sub item, or the marked ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Add,
    Sub(Ref),
    Edit(Ref),
    EditMarked,
    /// `/`: the filter, the columns narrowing as it's typed.
    Filter,
}

impl Input {
    /// The input with `text` filled in and the cursor at its end.
    fn new(kind: InputKind, text: String) -> Input {
        let cursor = text.len();
        Input {
            kind,
            text,
            cursor,
            cal: None,
        }
    }

    /// Keeps the calendar's `//` in step with an edit that replaced `range`
    /// of the words with `inserted` bytes, and closes it when the edit took
    /// either `/`.
    fn edited(&mut self, range: Range<usize>, inserted: usize) {
        if let Some(cal) = &mut self.cal {
            if range.end <= cal.at {
                cal.at = cal.at - range.len() + inserted;
            } else if range.start < cal.at + 2 {
                self.cal = None;
            }
        }
    }

    /// Moves the calendar's day `by` and writes it after the `//`, keeping
    /// the time, in words that name the day (`2026-11-09 6pm`), as `e` fills
    /// it in: they read back as it on any day, a day gone or a save past
    /// midnight too. From the day the words read as, else `own` (the task's on
    /// `e`), else `today`.
    fn move_day(&mut self, by: jiff::Span, own: Option<Due>, now: DateTime, today: Date) {
        let Some(cal) = self.cal else {
            return;
        };
        let from = parse::marked_due(&self.text, cal.at, now).or(own);
        let Ok(day) = from.map_or(today, |d| d.date).checked_add(by) else {
            return;
        };
        let words = date::say_exact(Due {
            date: day,
            time: from.and_then(|d| d.time),
        });
        // The date's words only: a typed `every …` after them stays, as a
        // typed time does.
        let span = parse::marked_date(&self.text, cal.at);
        let step_after = span.is_empty() && !parse::marked(&self.text, cal.at).is_empty();
        let put = match step_after {
            true => format!("{words} "),
            false => words.clone(),
        };
        self.text.replace_range(span.clone(), &put);
        self.cursor = span.start + words.len();
        // A step on a weekday (`every fri`) follows the picked day, as the
        // date's words do: the calendar never shows a day it won't be due.
        if let Some(weekday) = parse::marked_step_weekday(&self.text, cal.at) {
            let named = day.strftime("%a").to_string().to_lowercase();
            self.text.replace_range(weekday, &named);
        }
    }

    /// Moves the cursor to `to`. Moved into a `//`'s words, or right after it,
    /// from outside them, it opens the calendar there, on the day they read
    /// as: the date `e` filled in is reached as one typed.
    fn move_to(&mut self, to: usize) {
        let from = parse::mark_holding(&self.text, self.cursor);
        self.cursor = to;
        // Not in the filter line: `by` and `late` aren't days to pick.
        if self.kind != InputKind::Filter
            && let Some(at) = parse::mark_holding(&self.text, to)
            && from != Some(at)
        {
            self.cal = Some(Cal { at });
        }
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
/// the cursor's column in it, and the byte of `s` it starts at: its end when
/// the cursor is in it, so the words after the cursor show (the date `e`
/// fills in after the title), else its start, else what ends at the cursor.
fn view(s: &str, at: usize, max: usize) -> (String, usize, usize) {
    let before = &s[..at];
    if s.width() <= max {
        return (s.into(), before.width(), 0);
    }
    let end = tail(s, max);
    let from = s.len() - end.len();
    if from <= at {
        return (end, s[from..at].width(), from);
    }
    if before.width() < max {
        return (head(s, max), before.width(), 0);
    }
    let shown = tail(before, max);
    let x = shown.width();
    let from = at - shown.len();
    (shown, x, from)
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

    /// One letter more, in the last span when it has the same style.
    fn push_char(&mut self, ch: char, style: Style) {
        match self.spans.last_mut() {
            Some(last) if last.style == style => {
                last.text.push(ch);
                self.width += ch.width().unwrap_or(0);
            }
            _ => self.push(ch.encode_utf8(&mut [0; 4]), style),
        }
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
            done: None,
            added: None,
            every: None,
            next: None,
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

    /// `task`, made at `at` (`2026-11-02T10:00:00+09:00`).
    fn added(task: Task, at: &str) -> Task {
        Task {
            added: Some(at.parse().unwrap()),
            ..task
        }
    }

    /// `task`, done at `at`.
    fn done_at(task: Task, at: &str) -> Task {
        Task {
            status: Status::Done,
            done: Some(at.parse().unwrap()),
            ..task
        }
    }

    /// November 2026 on the calendar, the tests' Wednesday the 11th today: …
    ///
    /// - 1 pay rent !high, due Fri 13, added Mon 2; 6 its sub item, due today
    /// - 2 call mum, due today, done today; 3 fix bike, Doing, due today,
    ///   added today; 4 buy milk, due Mon 9 (late)
    /// - 5 water plants, done before 0.5.0; 7 an old note, from before 0.3
    /// - 8 file taxes, done on the 3rd; 9 book the flights, due the 30th
    fn cal_sample() -> Board {
        let mut board = Board::new(
            vec![
                added(
                    due(
                        task(1, "pay rent", Status::Todo, Some(Priority::High), &["home"]),
                        "2026-11-13",
                    ),
                    "2026-11-02T10:00:00+09:00",
                ),
                added(
                    done_at(
                        due(task(2, "call mum", Status::Todo, None, &[]), "2026-11-11"),
                        "2026-11-11T09:30:00+09:00[Asia/Seoul]",
                    ),
                    "2026-11-01T10:00:00+09:00",
                ),
                added(
                    due(
                        task(3, "fix bike", Status::Doing, Some(Priority::Low), &[]),
                        "2026-11-11T18:00",
                    ),
                    "2026-11-11T08:00:00+09:00",
                ),
                added(
                    due(task(4, "buy milk", Status::Todo, None, &[]), "2026-11-09"),
                    "2026-11-01T10:00:00+09:00",
                ),
                Task {
                    stamp: 1_763_500_000_000,
                    ..task(5, "water plants", Status::Done, None, &[])
                },
                added(
                    sub(
                        due(
                            task(6, "set up the transfer", Status::Todo, None, &[]),
                            "2026-11-11",
                        ),
                        1,
                    ),
                    "2026-11-10T21:00:00+09:00",
                ),
                task(7, "an old note", Status::Todo, None, &[]),
                added(
                    done_at(
                        task(8, "file taxes", Status::Todo, None, &[]),
                        "2026-11-03T20:00:00+09:00",
                    ),
                    "2026-10-20T10:00:00+09:00",
                ),
                added(
                    due(
                        task(9, "book the flights", Status::Todo, None, &[]),
                        "2026-11-30",
                    ),
                    "2026-11-04T10:00:00+09:00",
                ),
            ],
            today(),
        );
        board.tz = TimeZone::fixed(jiff::tz::offset(9));
        board
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

    /// Picks column `c`'s first row, as a test sets it up (no key does).
    fn go_to(board: &mut Board, c: usize) {
        board.col = c;
        board.row = 0;
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
        press(&mut board, "j ");
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
    fn e_on_one_card_or_sub_item_takes_tags_on_and_off() {
        let mut board = sample();
        press(&mut board, "e +work -home");
        assert_eq!(typed(&board), Some("pay rent +work -home"));
        assert!(enter(&mut board));
        let rent = by_id(&board, 2);
        assert_eq!(rent.title, "pay rent");
        assert_eq!(rent.tags, ["money", "work"]);
        // Tags alone, the title cleared: the title stays.
        press(&mut board, "e");
        ctrl(&mut board, 'u');
        press(&mut board, "-money -work +car");
        assert!(enter(&mut board));
        let rent = by_id(&board, 2);
        assert_eq!(rent.title, "pay rent");
        assert_eq!(rent.tags, ["car"]);
        // A sub item, as a card.
        let mut board = sub_sample();
        press(&mut board, "j");
        let id = picked_id(&board).unwrap();
        press(&mut board, "e +bank");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, id).tags, ["bank"]);
        press(&mut board, "e -bank");
        assert!(enter(&mut board));
        assert!(by_id(&board, id).tags.is_empty());
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
            (picked_id(&board), board.row),
            (Some(1), 0),
            "high, and older"
        );
        press(&mut board, "j");
        press(&mut board, "e 2030-01-02");
        assert!(enter(&mut board));
        assert_eq!((picked_id(&board), board.row), (Some(2), 0), "dated first");
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
            go_to(&mut board, 1);
            let (lines, _) = board.draw(w, 12);
            assert_eq!(cell(&lines[3], w, 0), want, "{w} wide");
            // Picked: its column wide when it fits there, else whole on
            // the line, `ship 0.2.0  2/4  +work  tomorrow` (35) and a cell.
            go_to(&mut board, 0);
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
        press(&mut board, "g ");
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
        press(&mut board, "g ");
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
            footer.starts_with("  Sub item of 2  title //fri 6pm"),
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
            footer(&mut board, 140),
            "  j k pick  space mark  h l move  s doing  d done  tab column  a add  A sub  e edit  y copy  x remove  q quit  c calendar  / filter ",
            "all of it, where there's room"
        );
        assert_eq!(
            footer(&mut board, 120),
            "  j k pick  space mark  h l move  s doing  d done  a add  A sub  e edit  y copy  x remove  q quit  c calendar  / filter ",
            "tab first"
        );
        assert_eq!(
            footer(&mut board, 100),
            "  space mark  h l move  s doing  d done  a add  A sub  e edit  x remove  c calendar  / filter ",
            "then y, q and j k"
        );
        let (lines, _) = board.draw(90, 12);
        assert_eq!(
            text(&lines[11]),
            "  h l move  s doing  d done  a add  A sub  e edit  x remove  q quit  c calendar  / filter ",
            "then space; q back in the room left"
        );
        // At 80, the design session's line: space and the other column gone,
        // the one key that removes kept.
        assert_eq!(
            footer(&mut board, 80),
            "  h l move  d done  a add  A sub  e edit  x remove  c calendar  / filter "
        );
        // In Doing: no s.
        go_to(&mut board, 1);
        assert_eq!(
            footer(&mut board, 80),
            "  h l move  d done  a add  A sub  e edit  x remove  c calendar  / filter "
        );
        // In Done: no d.
        go_to(&mut board, 2);
        assert_eq!(
            footer(&mut board, 120),
            "  j k pick  space mark  h l move  t todo  s doing  a add  A sub  e edit  y copy  x remove  q quit  c calendar  / filter "
        );
        assert_eq!(
            footer(&mut board, 80),
            "  h l move  s doing  a add  A sub  e edit  x remove  c calendar  / filter "
        );

        // A sub item picked: none of h l, t and s, and z for its card.
        let mut board = sub_sample();
        press(&mut board, "j");
        assert_eq!(
            footer(&mut board, 80),
            "  d done/open  a add  A sub  e edit  z fold  x remove  c calendar  / filter "
        );

        // A filter kept: its line, then the keys; `/ filter` not shown (the
        // line says it), `esc clear` never gone. The design session's line
        // at 80, and at 46 and 100.
        let mut board = sub_sample();
        press(&mut board, "/");
        type_in(&mut board, "+home");
        hit(&mut board, KeyCode::Enter);
        for (w, want) in [
            (46, "  / +home · 1 of 5  h l move  esc clear "),
            (
                80,
                "  / +home · 1 of 5  h l move  d done  a add  e edit  c calendar  esc clear ",
            ),
            (
                100,
                "  / +home · 1 of 5  h l move  d done  a add  A sub  e edit  z fold  x remove  c calendar  esc clear ",
            ),
        ] {
            assert_eq!(footer(&mut board, w), want, "{w}, +home kept");
        }

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
        // tab, y, q, j k, space, the other column, z, A, x, /, c; never h l,
        // d, a, e. The room left goes back to those gone, the last to go
        // first, one that doesn't fit skipped.
        let mut board = sample();
        go_to(&mut board, 2);
        for (w, want) in [
            (
                85,
                "  h l move  t todo  s doing  a add  A sub  e edit  x remove  c calendar  / filter ",
            ),
            (
                76,
                "  h l move  s doing  a add  A sub  e edit  x remove  c calendar  / filter ",
            ),
            (
                66,
                "  h l move  a add  A sub  e edit  x remove  c calendar  / filter ",
            ),
            (
                56,
                "  h l move  a add  A sub  e edit  c calendar  / filter ",
            ),
            (50, "  h l move  a add  e edit  c calendar  / filter "),
        ] {
            assert_eq!(footer(&mut board, w), want, "{w} wide, in Done");
        }
        let mut board = sub_sample();
        press(&mut board, "j");
        for (w, want) in [
            (
                76,
                "  d done/open  a add  A sub  e edit  z fold  x remove  c calendar  / filter ",
            ),
            (
                66,
                "  d done/open  a add  e edit  x remove  c calendar  / filter ",
            ),
            (56, "  d done/open  a add  e edit  c calendar  / filter "),
            (46, "  d done/open  a add  e edit  c calendar "),
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

        // At 100 it ends inside Done: call mum, under it, is cut where it
        // ends, with no `…`.
        let rows = body(&mut board, 100, 14);
        assert_eq!(
            rows[0],
            "   • write the quarterly report for the whole team  1/1  +work +q4  tomorrow um"
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
        go_to(&mut board, 1);
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
        go_to(&mut board, 1);
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
        go_to(&mut board, 2);
        let (lines, _) = board.draw(80, 12);
        assert_eq!(cell(&lines[3], 80, 2).trim_end(), "   read the whole");
        assert_eq!(cell(&lines[4], 80, 2).trim_end(), "   lease again");
        assert_eq!(lit(&lines[3]), 23);

        // Its wrapped lines are drawn over the rows under it, which stay
        // where they are: the third row isn't pushed down.
        board.tasks.extend([
            task(2, "call mum", Status::Done, None, &[]),
            task(3, "water plants", Status::Done, None, &[]),
        ]);
        let (lines, _) = board.draw(80, 12);
        assert_eq!(cell(&lines[4], 80, 2).trim_end(), "   lease again");
        assert_eq!(lit(&lines[4]), 23, "call mum under it");
        assert_eq!(cell(&lines[5], 80, 2).trim_end(), "   water plants");
        press(&mut board, "j");
        let (lines, _) = board.draw(80, 12);
        assert_eq!(cell(&lines[4], 80, 2).trim_end(), "   call mum");
    }

    #[test]
    fn an_overlay_never_cuts_a_double_width_letter() {
        let line = |s: &str| -> Line {
            vec![Span {
                text: s.into(),
                style: Style::fg(QUIET),
            }]
        };
        let mut top = Row::default();
        top.push("XY", Style::fg(RED));
        let ground = Style::fg(BLUE);
        // Its end on 보's second cell: 보 covered whole, in the ground.
        let over = overlay(&line("ab보고"), 1, &top, ground);
        assert_eq!(text(&over), "aXY 고");
        assert_eq!(over[2].style, ground);
        // Its start on 보's second cell: a space where 보's first was.
        let over = overlay(&line("a보cdef"), 2, &top, ground);
        assert_eq!(text(&over), "a XYdef");
        assert_eq!(
            over[1],
            Span {
                text: " ".into(),
                style: Style::fg(QUIET)
            }
        );
        // Past the line's end, and on a short line.
        assert_eq!(text(&overlay(&line("ab"), 4, &top, ground)), "ab  XY");
        assert_eq!(text(&overlay(&line("abcdef"), 0, &top, ground)), "XYcdef");
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
        // Into Doing, whose "Nothing here" is cut where it ends; Done's whole.
        assert_eq!(
            body(&mut board, 60, 10)[..2],
            [
                "     tax form  in 7 weeks thing here       Nothing here",
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
        go_to(&mut board, 2);
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
                count_style: quiet,
                tags: tags.map(String::from),
                due: Some(DueWords {
                    words: "in 7 weeks".into(),
                    late: false,
                    step: Step::None,
                }),
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
    fn a_neighbours_row_is_cut_where_the_picked_row_ends() {
        // pay rent with its count and tags reaches into Doing: fix bike is
        // drawn as ever, cut where pay rent's ground ends; Done it doesn't
        // reach.
        let mut board = sub_sample();
        board.tasks[3] = task(4, "call mum", Status::Done, None, &[]);
        let (lines, _) = board.draw(80, 14);
        let rows: Vec<String> = lines[3..6]
            .iter()
            .map(|l| text(l).trim_end().to_string())
            .collect();
        assert_eq!(
            rows[0], "   • pay rent  1/3  +home +money x bike                  call mum",
            "Doing's row cut, no `…`; Done's whole"
        );
        assert_eq!(
            cell(&lines[3], 80, 2).trim_end(),
            "   call mum",
            "in its place"
        );
        assert_eq!(lit(&lines[3]), 31, "the ground one cell past the text");
        // fix bike's priority dot and the rest in their own colours.
        let bike = lines[3].iter().find(|s| s.text.contains("x bike")).unwrap();
        assert_eq!(bike.style, Style::default());
        assert!(
            rows[1].starts_with("       ● ask for the bi…"),
            "{:?}",
            rows[1]
        );

        // A Korean title in Doing under a Todo row shown whole: 고 under the
        // cell after +work is covered whole, never cut in half; 서 on, and
        // the column it doesn't reach, drawn as ever.
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
            "     write the quarterly report  +work  서 초안          Nothing here"
        );
        let (lines, _) = board.draw(80, 10);
        assert_eq!(lit(&lines[3]), 38, "the text, a cell, and 고's two");
        // Not picked, it's there, whole.
        go_to(&mut board, 1);
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
        go_to(&mut board, 0);
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
            footer(&mut board, 80).contains(" e edit "),
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
            done: None,
            added: None,
            every: None,
            next: None,
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
        go_to(&mut board, 2);
        board.row = ids(&board, 2).iter().position(|&id| id == 2).unwrap();
        run_keys(&mut board, "s");
        assert_eq!(status(&board, 2), Status::Doing);
        for id in [5, 6, 7] {
            assert_eq!(status(&board, id), Status::Done, "{id} stays done");
        }
        go_to(&mut board, 1);
        run_keys(&mut board, "t");
        assert_eq!(status(&board, 2), Status::Todo);
        assert_eq!(status(&board, 6), Status::Done);
    }

    // Going to another column: the row on the same line.

    /// Todo 1 to 4, Doing 5 to 8, Done 9 and 10, each titled by its id.
    fn lines_sample() -> Board {
        let at = |id: u32, status| task(id, &format!("task {id}"), status, None, &[]);
        let mut tasks: Vec<Task> = (1..=4).map(|id| at(id, Status::Todo)).collect();
        tasks.extend((5..=8).map(|id| at(id, Status::Doing)));
        tasks.extend((9..=10).map(|id| at(id, Status::Done)));
        Board::new(tasks, today())
    }

    #[test]
    fn tab_lands_on_the_row_on_the_same_line() {
        let mut board = lines_sample();
        press(&mut board, "jj");
        assert_eq!(picked_id(&board), Some(3), "row three in Todo");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            (board.col, picked_id(&board)),
            (1, Some(7)),
            "row three in Doing"
        );
        // The arrows the same.
        key(&mut board, KeyCode::Left);
        assert_eq!(picked_id(&board), Some(3));
        key(&mut board, KeyCode::Right);
        assert_eq!(picked_id(&board), Some(7));
    }

    #[test]
    fn a_shorter_column_lands_on_its_last_row() {
        let mut board = lines_sample();
        press(&mut board, "jjj");
        board.key(KeyEvent::from(KeyCode::Tab));
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!((board.col, picked_id(&board)), (2, Some(10)), "Done's last");
        key(&mut board, KeyCode::Right);
        assert_eq!(
            (board.col, picked_id(&board)),
            (2, Some(10)),
            "no column past it"
        );
    }

    #[test]
    fn an_empty_column_takes_the_pick_as_ever() {
        let mut board = lines_sample();
        board.tasks.retain(|t| t.status != Status::Doing);
        press(&mut board, "jj");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!((board.col, board.row, picked_id(&board)), (1, 0, None));
        assert!(body(&mut board, 80, 12)[0].contains("Nothing here"));
        // On from it: its first line, Done's first row.
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!((board.col, picked_id(&board)), (2, Some(9)));
    }

    #[test]
    fn there_and_back_lands_on_the_same_line_each_way() {
        let mut board = lines_sample();
        press(&mut board, "jjj");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(8));
        press(&mut board, "k");
        // Back on row three, not on row four, where Todo's pick was.
        board.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!((board.col, picked_id(&board)), (0, Some(3)));
        press(&mut board, "g");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(5));
    }

    #[test]
    fn from_a_sub_items_line_and_onto_one() {
        // Todo: pay rent, its sub items 5, 6 and 7, buy milk, the orphan.
        let mut board = sub_sample();
        board.tasks.extend([
            task(9, "oil the chain", Status::Doing, None, &[]),
            task(10, "wash the bike", Status::Doing, None, &[]),
        ]);
        press(&mut board, "jj");
        assert_eq!(picked_id(&board), Some(6), "a sub item, line three");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(10), "Doing's row three");
        press(&mut board, "k");
        board.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(picked_id(&board), Some(5), "onto the sub item on its line");
    }

    #[test]
    fn the_same_line_is_by_what_is_drawn_and_a_wrapped_rows_first() {
        // Todo of 20, scrolled to its end in five lines of body; Doing of 10.
        let mut tasks: Vec<Task> = (1..=20)
            .map(|id| task(id, &format!("todo {id}"), Status::Todo, None, &[]))
            .collect();
        tasks
            .extend((21..=30).map(|id| task(id, &format!("doing {id}"), Status::Doing, None, &[])));
        let mut board = Board::new(tasks, today());
        press(&mut board, "G");
        let (lines, _) = board.draw(80, 10);
        assert_eq!(cell(&lines[7], 80, 0).trim_end(), "   todo 20", "line five");
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(25), "Doing's line five");
        board.draw(80, 10);
        board.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(
            picked_id(&board),
            Some(20),
            "Todo's line five, as it's drawn"
        );

        // A picked row wrapped over the lines under it: its first line.
        let mut board = lines_sample();
        board.tasks[1].title =
            "a title long enough to wrap past the edge of the whole board at eighty".repeat(2);
        press(&mut board, "j");
        assert!(
            board.draw(80, 12).0[5]
                .iter()
                .any(|s| s.style.bg == Some(HIGHLIGHT)),
            "wrapped"
        );
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(6));
    }

    // The calendar.

    /// The `n` cells of a drawn line from cell `x`.
    fn cells(line: &Line, x: usize, n: usize) -> String {
        text(line).chars().skip(x).take(n).collect()
    }

    /// The day the calendar shows picked, by the picked row's colours.
    fn cal_picked(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .flatten()
            .filter(|s| s.style.bg == Some(HIGHLIGHT) && s.style.fg == Some(ON_HIGHLIGHT))
            .filter(|s| s.text.trim().parse::<u8>().is_ok())
            .map(|s| s.text.trim().to_string())
            .collect()
    }

    #[test]
    fn two_slashes_open_the_calendar_above_the_typing_line_at_them() {
        let mut board = sample();
        press(&mut board, "apay rent /");
        assert_eq!(board.input.as_ref().unwrap().cal, None, "one is a letter");
        press(&mut board, "/");
        assert_eq!(board.input.as_ref().unwrap().cal, Some(Cal { at: 9 }));
        let (lines, _) = board.draw(80, 20);
        // Its left edge at the `//` (after `  New task  `), its foot right
        // above the typing line.
        assert_eq!(cells(&lines[10], 21, 17), "╭ November 2026 ─");
        assert_eq!(cells(&lines[11], 21, 1), "│");
        assert!(cells(&lines[11], 22, 45).contains("Su Mo Tu We Th Fr Sa"));
        assert!(
            cells(&lines[12], 22, 45).contains(" 1  2  3  4  5  6  7"),
            "Sunday first"
        );
        assert_eq!(
            cells(&lines[18], 21, 47),
            "╰ ←→ day  ↑↓ week  pgup pgdn month  esc close ╯"
        );
        assert!(
            text(&lines[19]).contains("pay rent //"),
            "the typing line free"
        );
        // The weekdays faint, today (the 11th) in the accent, none picked.
        assert_eq!(
            span(&lines[11], "Su Mo Tu We Th Fr Sa").style.fg,
            Some(QUIETER)
        );
        assert_eq!(span(&lines[13], "11").style.fg, Some(BLUE));
        assert!(cal_picked(&lines).is_empty());

        // Typing stays typing; the calendar follows what the words read as.
        press(&mut board, "fri");
        assert_eq!(typed(&board), Some("pay rent //fri"));
        assert_eq!(cal_picked(&board.draw(80, 20).0), ["13"]);
        press(&mut board, "dya");
        let (lines, _) = board.draw(80, 20);
        assert!(cal_picked(&lines).is_empty());
        assert_eq!(
            (board.message.as_deref(), text(&lines[18]).contains("isn't")),
            (None, false),
            "no error while typing"
        );
        // The month of the day it reads.
        key(&mut board, KeyCode::Backspace);
        key(&mut board, KeyCode::Backspace);
        key(&mut board, KeyCode::Backspace);
        press(&mut board, " 6pm +home");
        assert_eq!(cal_picked(&board.draw(80, 20).0), ["13"]);
        press(&mut board, "x");
        for _ in 0..14 {
            key(&mut board, KeyCode::Backspace);
        }
        press(&mut board, "jan 3");
        assert_eq!(typed(&board), Some("pay rent //jan 3"));
        assert!(text(&board.draw(80, 20).0[10]).contains("January 2027"));
    }

    #[test]
    fn the_calendars_keys_move_the_day_and_write_it_after_the_slashes() {
        let mut board = sample();
        press(&mut board, "apay //");
        let now = today().to_datetime(jiff::civil::Time::midnight());
        // The first move from today, the 11th; each written as halil reads it.
        for (code, want, day) in [
            (KeyCode::Right, "pay //2026-11-12", "2026-11-12"),
            (KeyCode::Left, "pay //2026-11-11", "2026-11-11"),
            (KeyCode::Left, "pay //2026-11-10", "2026-11-10"),
            (KeyCode::Down, "pay //2026-11-17", "2026-11-17"),
            (KeyCode::Up, "pay //2026-11-10", "2026-11-10"),
            (KeyCode::PageDown, "pay //2026-12-10", "2026-12-10"),
            (KeyCode::PageDown, "pay //2027-01-10", "2027-01-10"),
            (KeyCode::PageUp, "pay //2026-12-10", "2026-12-10"),
        ] {
            key(&mut board, code);
            assert_eq!(typed(&board), Some(want), "{code:?}");
            assert_eq!(
                parse::marked_due(want, 4, now).unwrap().to_string(),
                day,
                "{want} reads back as {day}"
            );
            assert_eq!(
                cal_picked(&board.draw(80, 20).0),
                [day[8..].trim_start_matches('0')]
            );
        }
        let input = board.input.as_ref().unwrap();
        assert_eq!(input.cursor, input.text.len(), "the cursor after the day");

        // From the day typed, a typed time kept, the words after it too.
        let mut board = sample();
        press(&mut board, "apay //fri 18:00 +home");
        key(&mut board, KeyCode::Right);
        assert_eq!(typed(&board), Some("pay //2026-11-14 6pm +home"));
        // enter adds it, once.
        match key(&mut board, KeyCode::Enter) {
            Outcome::Change(Op::Add { task, .. }) => {
                assert_eq!(task.title, "pay");
                assert_eq!(task.due.unwrap().to_string(), "2026-11-14 18:00");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(board.input, None);
    }

    /// The calendar names the day it writes, so what's saved is the day it
    /// showed: a day gone, one ahead this year, one next year, and today saved
    /// a midnight later — on `a` from today, on `e` from the task's own day.
    #[test]
    fn a_calendar_move_saves_as_the_day_the_calendar_showed() {
        let next_day = jiff::civil::date(2026, 11, 12);
        use KeyCode::{Left, PageDown, Right};
        let moves: [(&[KeyCode], &str); 4] = [
            (&[Left, Left], "2026-11-09"),
            (&[Right, Right, Right], "2026-11-14"),
            (&[PageDown, PageDown], "2027-01-11"),
            (&[Left, Right], "2026-11-11"),
        ];
        for (keys, day) in moves {
            for enter_on in [today(), next_day] {
                let mut board = sample();
                press(&mut board, "apay //");
                for &code in keys {
                    key(&mut board, code);
                }
                let shown = cal_picked(&board.draw(80, 20).0);
                assert_eq!(shown, [day[8..].trim_start_matches('0')], "{day}");
                assert_eq!(typed(&board).unwrap(), format!("pay //{day}"));
                board.today = enter_on;
                assert!(enter(&mut board));
                let added = board.tasks.last().unwrap();
                assert_eq!(added.due.unwrap().to_string(), day, "saved on {enter_on}");
            }
        }

        // On `e`, from the task's own day, Friday the 13th at 6pm.
        let moves: [(&[KeyCode], &str, &str); 4] = [
            (
                &[Left, Left, Left, Left],
                "2026-11-09 6pm",
                "2026-11-09 18:00",
            ),
            (&[Right], "2026-11-14 6pm", "2026-11-14 18:00"),
            (&[PageDown, PageDown], "2027-01-13 6pm", "2027-01-13 18:00"),
            (&[Left, Left], "2026-11-11 6pm", "2026-11-11 18:00"),
        ];
        for (keys, words, due) in moves {
            for enter_on in [today(), next_day] {
                let mut board = one("pay rent", "2026-11-13 18:00");
                press(&mut board, "e");
                ctrl(&mut board, 'e');
                for &code in keys {
                    key(&mut board, code);
                }
                let shown = cal_picked(&board.draw(80, 20).0);
                assert_eq!(shown, [due[8..10].trim_start_matches('0')], "{due}");
                assert_eq!(typed(&board).unwrap(), format!("pay rent //{words}"));
                board.today = enter_on;
                assert!(enter(&mut board));
                let rent = by_id(&board, 1);
                assert_eq!(rent.due.unwrap().to_string(), due, "saved on {enter_on}");
                assert_eq!(rent.title, "pay rent");
            }
        }
    }

    #[test]
    fn esc_closes_the_calendar_then_the_typing_and_only_slashes_open_it_again() {
        let mut board = sample();
        press(&mut board, "ax //fri");
        key(&mut board, KeyCode::Esc);
        assert_eq!(board.input.as_ref().unwrap().cal, None);
        assert_eq!(typed(&board), Some("x //fri"), "the words left");
        assert!(!text(&board.draw(80, 20).0[10]).contains("╭"));
        // The arrows are the cursor's again.
        key(&mut board, KeyCode::Left);
        assert_eq!(board.input.as_ref().unwrap().cursor, 6);
        key(&mut board, KeyCode::Right);
        press(&mut board, " 6pm");
        assert_eq!(
            board.input.as_ref().unwrap().cal,
            None,
            "typing doesn't open it"
        );
        press(&mut board, " //");
        assert_eq!(board.input.as_ref().unwrap().cal, Some(Cal { at: 12 }));
        // Backspace over a `/` closes it; a / typed back opens it.
        key(&mut board, KeyCode::Backspace);
        assert_eq!(board.input.as_ref().unwrap().cal, None);
        press(&mut board, "/");
        assert!(board.input.as_ref().unwrap().cal.is_some());
        // A letter typed before it keeps it where its `//` went.
        ctrl(&mut board, 'a');
        press(&mut board, "y");
        assert_eq!(board.input.as_ref().unwrap().cal, Some(Cal { at: 13 }));
        key(&mut board, KeyCode::Esc);
        key(&mut board, KeyCode::Esc);
        assert_eq!(board.input, None, "a second esc leaves typing");
    }

    #[test]
    fn on_e_the_calendar_opens_on_the_tasks_own_date() {
        let mut board = sample();
        board.tasks[1].due = Some("2026-12-04 09:00".parse().unwrap());
        press(&mut board, "e");
        assert_eq!(typed(&board), Some("pay rent //2026-12-04 9am"));
        let cal = |board: &Board| board.input.as_ref().unwrap().cal;
        assert_eq!(cal(&board), None, "the cursor after the title");
        assert!(!text(&board.draw(80, 20).0[10]).contains("December"));
        // The cursor moved into the date's words opens it there: right
        // after the `//`, not before it.
        key(&mut board, KeyCode::Right);
        key(&mut board, KeyCode::Right);
        assert_eq!(cal(&board), None);
        key(&mut board, KeyCode::Right);
        assert_eq!(cal(&board), Some(Cal { at: 9 }));
        let (lines, _) = board.draw(80, 20);
        assert!(text(&lines[10]).contains("December 2026"));
        assert_eq!(cal_picked(&lines), ["4"]);
        // The first move from it, its time with it, in the line to see.
        key(&mut board, KeyCode::Right);
        assert_eq!(typed(&board), Some("pay rent //2026-12-05 9am"));
        // Closed, moving inside the words leaves it closed; from outside
        // them, ctrl-e opens it again.
        key(&mut board, KeyCode::Esc);
        key(&mut board, KeyCode::Left);
        key(&mut board, KeyCode::Right);
        assert_eq!(cal(&board), None);
        ctrl(&mut board, 'a');
        assert_eq!(cal(&board), None);
        ctrl(&mut board, 'e');
        assert_eq!(cal(&board), Some(Cal { at: 9 }));
        // Its words cleared: on the task's own day again, a move from it.
        for _ in 0.."2026-12-05 9am".len() {
            key(&mut board, KeyCode::Backspace);
        }
        assert_eq!(cal_picked(&board.draw(80, 20).0), ["4"]);
        key(&mut board, KeyCode::Right);
        assert_eq!(typed(&board), Some("pay rent //2026-12-05 9am"));
        assert!(enter(&mut board));
        assert_eq!(
            by_id(&board, 2).due.unwrap().to_string(),
            "2026-12-05 09:00"
        );
        assert_eq!(by_id(&board, 2).title, "pay rent");
    }

    // 0.4.1: `e` fills in the date.

    /// A board of one task, 1, `title` +home, due `when` ("": no date).
    fn one(title: &str, when: &str) -> Board {
        let mut t = task(1, title, Status::Todo, None, &["home"]);
        if !when.is_empty() {
            t = due(t, when);
        }
        Board::new(vec![t], today())
    }

    /// The input's cursor, as cells after the footer's `  Edit 1  `.
    fn cursor_cells(board: &mut Board) -> usize {
        let (_, cursor) = board.draw(100, 12);
        cursor.unwrap().0 as usize - "  Edit 1  ".len()
    }

    #[test]
    fn e_fills_in_the_title_then_the_date_and_an_untouched_enter_changes_nothing() {
        for (title, when, line) in [
            (
                "plan the friday party",
                "2026-11-11",
                "plan the friday party //2026-11-11",
            ),
            ("pay rent", "2026-11-13 18:00", "pay rent //2026-11-13 6pm"),
            (
                "pay rent",
                "2026-11-09 09:30",
                "pay rent //2026-11-09 9:30am",
            ),
            (
                "renew the passport",
                "2027-01-03",
                "renew the passport //2027-01-03",
            ),
            ("buy milk", "", "buy milk"),
            ("plan the friday party", "", "plan the friday party //"),
            ("월세 내기", "2026-11-27", "월세 내기 //2026-11-27"),
        ] {
            let mut board = one(title, when);
            press(&mut board, "e");
            assert_eq!(typed(&board), Some(line));
            assert_eq!(cursor_cells(&mut board), title.width(), "after {title:?}");
            assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing, "{line}");
            assert_eq!((&board.input, &board.message), (&None, &None));
            assert_eq!(board.tasks, one(title, when).tasks, "{line}");
        }

        // A sub item, the same.
        let mut board = sub_sample();
        board.tasks[6].due = Some("2026-11-20 09:00".parse().unwrap());
        let before = board.tasks.clone();
        press(&mut board, "jje");
        assert_eq!(typed(&board), Some("set up the transfer //2026-11-20 9am"));
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.tasks, before);
        // Its date changed with the calendar, from the line.
        press(&mut board, "e");
        ctrl(&mut board, 'e');
        key(&mut board, KeyCode::Down);
        assert_eq!(typed(&board), Some("set up the transfer //2026-11-27 9am"));
        assert!(enter(&mut board));
        let six = by_id(&board, 6);
        assert_eq!(six.due.unwrap().to_string(), "2026-11-27 09:00");
        assert_eq!(
            (six.title.as_str(), six.parent),
            ("set up the transfer", Some(2))
        );
        assert_eq!(picked_id(&board), Some(6));
    }

    /// The property `e` is held to: for every task — with or without a date or
    /// a time, late, due today or next year, a sub item, Hangul or date words
    /// in its title — `e` then enter with nothing changed leaves it exactly as
    /// it was, whatever day `e` is pressed on and whatever day enter is: the
    /// same, a midnight later, or a week.
    #[test]
    fn e_then_an_untouched_enter_leaves_every_task_as_it_was_on_any_day() {
        let titles = [
            "pay rent",
            "plan the friday party",
            "call mum tomorrow 9am",
            "월세 내기 금요일",
            "x -money",
        ];
        let times = [
            None,
            Some((0, 0)),
            Some((0, 5)),
            Some((9, 30)),
            Some((12, 0)),
            Some((23, 59)),
        ];
        let offsets = [-400, -366, -31, -7, -1, 0, 1, 6, 7, 45, 364, 365, 400];
        let after = |d: Date, n: i64| d.checked_add(jiff::Span::new().days(n)).unwrap();
        let first = jiff::civil::date(2025, 12, 20);
        let mut days: Vec<Date> = (0..=800).step_by(13).map(|n| after(first, n)).collect();
        days.extend([
            jiff::civil::date(2026, 12, 31),
            jiff::civil::date(2027, 1, 1),
            jiff::civil::date(2028, 2, 28),
            jiff::civil::date(2028, 2, 29),
            today(),
        ]);
        let card = task(1, "the card", Status::Todo, None, &[]);
        let mut checked = 0;
        for &on in &days {
            let mut dues = vec![None];
            for n in offsets {
                for time in times {
                    dues.push(Some(Due {
                        date: after(on, n),
                        time: time.map(|(h, m)| jiff::civil::Time::new(h, m, 0, 0).unwrap()),
                    }));
                }
            }
            for (n, (title, due)) in titles
                .iter()
                .flat_map(|t| dues.iter().map(move |d| (*t, *d)))
                .enumerate()
            {
                let task = Task {
                    due,
                    parent: (n % 3 == 0).then_some(1),
                    ..task(2, title, Status::Todo, Some(Priority::Med), &["money"])
                };
                for enter_on in [on, after(on, 1), after(on, 7)] {
                    let tasks = vec![card.clone(), task.clone()];
                    let mut board = Board::new(tasks.clone(), on);
                    board.pick(2);
                    assert_eq!(picked_id(&board), Some(2));
                    press(&mut board, "e");
                    let line = typed(&board).unwrap().to_string();
                    board.today = enter_on;
                    let outcome = key(&mut board, KeyCode::Enter);
                    let why = format!("{task}: `{line}` filled in on {on}, enter on {enter_on}");
                    assert_eq!(outcome, Outcome::Nothing, "{why}");
                    assert_eq!((&board.message, &board.input), (&None, &None), "{why}");
                    assert_eq!(board.tasks, tasks, "{why}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 70_000, "{checked}");
    }

    #[test]
    fn a_long_titles_date_shows_in_the_typing_line_with_the_cursor_before_it() {
        let title = "read the lease and mark what to ask the landlord";
        for (w, when, date) in [
            (80, "2026-11-16", " //2026-11-16"),
            (60, "2026-11-16", " //2026-11-16"),
            (80, "2027-01-03 09:30", " //2027-01-03 9:30am"),
        ] {
            let mut board = one(title, when);
            press(&mut board, "e");
            let (lines, cursor) = board.draw(w, 12);
            let footer = text(&lines[11]);
            let typed = footer.split("  ").nth(2).unwrap();
            assert!(typed.ends_with(&format!("d{date}")), "{w}: {footer:?}");
            let x = cursor.unwrap().0 as usize;
            assert_eq!(
                footer[x..].split("  ").next(),
                Some(date),
                "{w}: the cursor before it"
            );
            // Moved to the start, the start shows, as ever.
            ctrl(&mut board, 'a');
            let footer = text(&board.draw(w, 12).0[11]);
            assert!(footer.starts_with("  Edit 1  read the lease"), "{footer:?}");
            // Where the whole line can't fit, its start, not its date (at 80
            // it fits whole now: the hint gives way to it).
            let whole = "  Edit 1  ".width() + title.width() + date.width() + 1;
            assert_eq!(footer.contains("//"), whole <= w, "{w}: {footer:?}");
        }
    }

    #[test]
    fn the_typing_lines_hint_gives_way_to_what_is_typed() {
        // At 46, e on a long title: `esc to cancel` goes first, then the rest,
        // and the line's end is in sight while typing.
        let mut board = one("pay the rent for november", "2026-12-11 18:00");
        press(&mut board, "e");
        let footer = text(&board.draw(46, 12).0[11]);
        assert!(!footer.contains("enter to change"), "{footer:?}");
        assert!(
            footer.trim_end().ends_with("//2026-12-11 6pm"),
            "{footer:?}"
        );
        // Room for part of it: `enter to change` stays, `esc to cancel` goes.
        let mut board = one("rent", "2026-12-11");
        press(&mut board, "e");
        let footer = text(&board.draw(60, 12).0[11]);
        assert!(footer.ends_with("enter to change"), "{footer:?}");
        assert!(footer.contains("rent //2026-12-11"), "{footer:?}");
        // Room for all of it: all of it, as before.
        let footer = text(&board.draw(80, 12).0[11]);
        assert!(
            footer.ends_with("enter to change · esc to cancel"),
            "{footer:?}"
        );
        // Nothing typed yet: the hint and the example, as before.
        let mut board = one("rent", "2026-12-11");
        press(&mut board, "a");
        let footer = text(&board.draw(80, 12).0[11]);
        assert!(
            footer.ends_with("enter to add · esc to cancel"),
            "{footer:?}"
        );
    }

    #[test]
    fn e_changes_the_date_in_place_and_typing_goes_on_with_the_title() {
        // Typed after `e`: the title's, as in 0.4.0.
        let mut board = one("pay rent", "2026-11-13 18:00");
        press(&mut board, "e now");
        assert_eq!(typed(&board), Some("pay rent now //2026-11-13 6pm"));
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 1).title, "pay rent now");
        assert_eq!(
            by_id(&board, 1).due.unwrap().to_string(),
            "2026-11-13 18:00"
        );

        // The time, typed over.
        press(&mut board, "e");
        ctrl(&mut board, 'e');
        key(&mut board, KeyCode::Esc);
        for _ in 0..3 {
            key(&mut board, KeyCode::Backspace);
        }
        press(&mut board, "9am");
        assert_eq!(typed(&board), Some("pay rent now //2026-11-13 9am"));
        assert!(enter(&mut board));
        let rent = by_id(&board, 1);
        assert_eq!(rent.due.unwrap().to_string(), "2026-11-13 09:00");
        assert_eq!(
            (rent.title.as_str(), &rent.tags[..]),
            ("pay rent now", &["home".to_string()][..])
        );

        // The friday party: a day typed in place, friday still title.
        let mut board = one("plan the friday party", "2026-11-11");
        press(&mut board, "e");
        ctrl(&mut board, 'e');
        for _ in 0.."2026-11-11".len() {
            key(&mut board, KeyCode::Backspace);
        }
        press(&mut board, "fri");
        assert!(enter(&mut board));
        let party = by_id(&board, 1);
        assert_eq!(party.title, "plan the friday party");
        assert_eq!(party.due.unwrap().to_string(), "2026-11-13");

        // No date: one typed after the title, as ever, or after the `//`
        // that keeps a date word title.
        let mut board = one("buy milk", "");
        press(&mut board, "e //fri");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 1).due.unwrap().to_string(), "2026-11-13");
        let mut board = one("plan the friday party", "");
        press(&mut board, "e");
        ctrl(&mut board, 'e');
        press(&mut board, "sat");
        assert!(enter(&mut board));
        let party = by_id(&board, 1);
        assert_eq!(party.title, "plan the friday party");
        assert_eq!(party.due.unwrap().to_string(), "2026-11-14");

        // Hangul: typed into the title, the date kept.
        let mut board = one("월세 내기", "2026-11-27");
        press(&mut board, "e");
        ctrl(&mut board, 'a');
        key(&mut board, KeyCode::Right);
        key(&mut board, KeyCode::Right);
        press(&mut board, " 꼭");
        assert_eq!(typed(&board), Some("월세 꼭 내기 //2026-11-27"));
        assert_eq!(cursor_cells(&mut board), "월세 꼭".width());
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 1).title, "월세 꼭 내기");
        assert_eq!(by_id(&board, 1).due.unwrap().to_string(), "2026-11-27");
    }

    #[test]
    fn e_takes_the_date_off_when_the_words_after_the_slashes_are_cleared() {
        for (title, when, next) in [
            ("pay rent", "2026-11-13 18:00", "pay rent"),
            (
                "plan the friday party",
                "2026-11-11",
                "plan the friday party //",
            ),
            ("월세 내기", "2026-11-27", "월세 내기"),
        ] {
            let mut board = one(title, when);
            press(&mut board, "e");
            ctrl(&mut board, 'e');
            let words = typed(&board).unwrap().split("//").nth(1).unwrap().len();
            for _ in 0..words {
                key(&mut board, KeyCode::Backspace);
            }
            assert_eq!(typed(&board).unwrap(), format!("{title} //"));
            assert!(enter(&mut board), "{title}");
            assert_eq!(board.message, None);
            let task = by_id(&board, 1);
            assert_eq!(task.due, None, "{title}");
            assert_eq!(
                (task.title.as_str(), &task.tags[..]),
                (title, &["home".to_string()][..])
            );
            // And the next `e` has no date to fill in.
            press(&mut board, "e");
            assert_eq!(typed(&board), Some(next));
            assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        }

        // A sub item, the same.
        let mut board = sub_sample();
        board.tasks[6].due = Some("2026-11-20".parse().unwrap());
        press(&mut board, "jje");
        ctrl(&mut board, 'e');
        ctrl(&mut board, 'u');
        press(&mut board, "//");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 6).due, None);
        assert_eq!(by_id(&board, 6).title, "set up the transfer");

        // The whole `//2026-11-13 6pm` taken out: the date stays, as `halil
        // edit` with no date leaves it.
        let mut board = one("pay rent", "2026-11-13 18:00");
        press(&mut board, "e");
        ctrl(&mut board, 'e');
        key(&mut board, KeyCode::Esc);
        for _ in 0..2 {
            ctrl(&mut board, 'w');
        }
        assert_eq!(typed(&board), Some("pay rent "));
        assert_eq!(key(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.tasks, one("pay rent", "2026-11-13 18:00").tasks);

        // With marks, `//` takes them all off; `a` refuses it, as `add` does.
        let mut board = marked_sample();
        for i in [1, 2] {
            board.tasks[i].due = Some("2026-11-20".parse().unwrap());
        }
        press(&mut board, "e//");
        assert!(enter(&mut board));
        assert_eq!((by_id(&board, 2).due, by_id(&board, 3).due), (None, None));
        assert_eq!(by_id(&board, 2).title, "pay rent");
        press(&mut board, "ax //");
        assert!(!enter(&mut board));
        assert_eq!(board.message.as_deref(), Some("`//` needs a date"));
    }

    #[test]
    fn without_room_there_is_no_calendar_and_the_row_above_says_so() {
        let mut board = sample();
        press(&mut board, "ax //");
        let (lines, _) = board.draw(80, 9);
        assert_eq!(
            text(&lines[7]).trim_end(),
            "  no room for the calendar — type the date"
        );
        assert!(!lines.iter().any(|l| text(l).contains('╭')));
        // The arrows are the cursor's, and esc leaves typing.
        key(&mut board, KeyCode::Left);
        assert_eq!(typed(&board), Some("x //"));
        assert_eq!(board.input.as_ref().unwrap().cursor, 3);
        // Typed, the date reads as ever.
        press(&mut board, "/fri");
        key(&mut board, KeyCode::End);
        let (lines, _) = board.draw(80, 10);
        assert!(
            lines[0..9].iter().any(|l| text(l).contains('╭')),
            "10 rows: room"
        );
        let (_, _) = board.draw(80, 9);
        key(&mut board, KeyCode::Esc);
        assert_eq!(board.input, None);
    }

    #[test]
    fn the_calendars_keys_give_way_from_the_right_and_due_days_are_bold() {
        let due = BTreeSet::from([jiff::civil::date(2026, 11, 20)]);
        let card = calendar(None, today(), &due, 22);
        assert_eq!(card.len(), CAL_ROWS);
        let rows: Vec<String> = card.iter().map(|r| text(&r.spans)).collect();
        assert_eq!(rows[0], "╭ November 2026 ─────╮");
        assert_eq!(rows[1], "│Su Mo Tu We Th Fr Sa│");
        assert_eq!(rows[2], "│ 1  2  3  4  5  6  7│");
        assert_eq!(rows[6], "│29 30               │");
        assert_eq!(rows[7], "│                    │");
        assert_eq!(rows[8], "╰ ←→ day  ↑↓ week ───╯");
        assert!(card.iter().all(|r| r.width == 22));
        assert!(span(&card[4].spans, "20").style.bold);
        assert!(!span(&card[4].spans, "19").style.bold);
        // Whole pairs from the left: esc close would fit after week, but
        // it gives way with pgup pgdn month.
        let card = calendar(None, today(), &due, 32);
        assert_eq!(
            text(&card[8].spans),
            format!("╰ ←→ day  ↑↓ week {}╯", "─".repeat(13))
        );
        let card = calendar(None, today(), &due, 38);
        assert!(text(&card[8].spans).starts_with("╰ ←→ day  ↑↓ week  pgup pgdn month ─"));
        let card = calendar(None, today(), &due, 47);
        assert_eq!(
            text(&card[8].spans),
            "╰ ←→ day  ↑↓ week  pgup pgdn month  esc close ╯"
        );
    }

    #[test]
    fn the_board_reads_slashes_as_halil_does_and_refuses_as_it_does() {
        let mut board = sample();
        for (words, err) in [
            ("ax //fridya", "`//fridya` isn't a date"),
            ("ax //", "`//` needs a date"),
        ] {
            press(&mut board, words);
            assert!(!enter(&mut board));
            assert_eq!(board.message.as_deref(), Some(err));
            assert_eq!(typed(&board), Some(&words[1..]), "the words kept");
            key(&mut board, KeyCode::Esc);
        }
        // On e, after the title filled in.
        press(&mut board, "e //fri");
        assert!(enter(&mut board));
        assert_eq!(by_id(&board, 2).title, "pay rent");
        assert_eq!(by_id(&board, 2).due.unwrap().to_string(), "2026-11-13");
        press(&mut board, "aplan the friday party //tomorrow +home");
        assert!(enter(&mut board));
        let added = board.tasks.last().unwrap();
        assert_eq!(added.title, "plan the friday party");
        assert_eq!(added.due.unwrap().to_string(), "2026-11-12");
    }

    #[test]
    fn the_calendar_stays_inside_the_screen() {
        let mut board = sample();
        press(&mut board, "a");
        press(&mut board, &"x".repeat(40));
        press(&mut board, " //");
        let (lines, _) = board.draw(60, 20);
        // The `//` at 53: the card's 47 cells end at the screen's.
        assert_eq!(cells(&lines[10], 13, 1), "╭");
        assert_eq!(text(&lines[10]).chars().count(), 60);
    }

    // Folding.

    /// The span of a drawn line whose text is `s`.
    fn span<'a>(line: &'a Line, s: &str) -> &'a Span {
        line.iter()
            .find(|span| span.text == s)
            .unwrap_or_else(|| panic!("no {s:?} in {:?}", text(line)))
    }

    #[test]
    fn z_folds_the_picked_card_and_z_again_unfolds_it() {
        let mut board = sub_sample();
        assert_eq!(ids(&board, 0), [2, 5, 6, 7, 1, 8]);
        press(&mut board, "z");
        assert_eq!(ids(&board, 0), [2, 1, 8], "its sub items not drawn");
        assert_eq!(picked_id(&board), Some(2));
        let (lines, _) = board.draw(100, 12);
        assert!(
            text(&lines[3]).starts_with("   • pay rent  ▸ 1/3  +home"),
            "{:?}",
            text(&lines[3])
        );
        assert_eq!(span(&lines[3], "▸ 1/3").style.fg, Some(QUIET_ON_HIGHLIGHT));
        assert_eq!(cell(&lines[4], 100, 0).trim_end(), "   buy milk");
        // j and k step past them.
        press(&mut board, "j");
        assert_eq!(picked_id(&board), Some(1));
        press(&mut board, "k");
        assert_eq!(picked_id(&board), Some(2));
        // Again: open, as it was.
        press(&mut board, "z");
        assert_eq!(ids(&board, 0), [2, 5, 6, 7, 1, 8]);
        let (lines, _) = board.draw(100, 12);
        assert!(text(&lines[3]).starts_with("   • pay rent  1/3  +home"));

        // On a sub item: its card folds, and the pick goes to it.
        press(&mut board, "jj");
        assert_eq!(picked_id(&board), Some(6));
        press(&mut board, "z");
        assert_eq!(
            (ids(&board, 0), picked_id(&board)),
            (vec![2, 1, 8], Some(2))
        );
        // ㅋ is z.
        press(&mut board, "ㅋ");
        assert_eq!(ids(&board, 0), [2, 5, 6, 7, 1, 8]);

        // Nothing to fold: a card without sub items, and in Done.
        press(&mut board, "G");
        press(&mut board, "z");
        assert_eq!(board.message.as_deref(), Some("8 has no sub items to fold"));
        board
            .tasks
            .push(sub(task(9, "ring them", Status::Done, None, &[]), 4));
        go_to(&mut board, 2);
        press(&mut board, "z");
        assert_eq!(
            board.message.as_deref(),
            Some("in Done, sub items are folded into the count")
        );
        assert!(board.folded.is_empty());
    }

    #[test]
    fn the_footer_offers_z_only_where_it_acts() {
        let mut board = sub_sample();
        assert!(footer(&mut board, 120).contains(" z fold "));
        press(&mut board, "z");
        assert!(footer(&mut board, 120).contains(" z unfold "));
        // Not on a card without sub items.
        press(&mut board, "j");
        assert!(!footer(&mut board, 120).contains(" z "));
        // On a sub item, for its card.
        press(&mut board, "kz");
        press(&mut board, "j");
        assert_eq!(picked_id(&board), Some(5));
        assert!(footer(&mut board, 120).contains(" z fold "));
        // Not in Done, a card with sub items or not.
        board
            .tasks
            .push(sub(task(9, "ring them", Status::Done, None, &[]), 4));
        go_to(&mut board, 2);
        assert!(!footer(&mut board, 120).contains(" z "));
    }

    #[test]
    fn a_folded_card_never_hides_whats_due() {
        // A card due in 7 weeks, with an open sub item due tomorrow, one
        // late, and a done one due long ago; a card due friday beside it.
        let mut board = Board::new(
            vec![
                due(task(1, "move house", Status::Todo, None, &[]), "2026-12-30"),
                due(task(2, "pay rent", Status::Todo, None, &[]), "2026-11-13"),
                sub(
                    due(
                        task(3, "book the van", Status::Todo, None, &[]),
                        "2026-11-12",
                    ),
                    1,
                ),
                sub(
                    due(task(4, "old boxes", Status::Done, None, &[]), "2026-10-01"),
                    1,
                ),
            ],
            today(),
        );
        assert_eq!(ids(&board, 0), [2, 1, 3, 4]);
        board.pick(1);
        press(&mut board, "z");
        // Folded, it shows tomorrow, its open sub item's, and sorts by it.
        assert_eq!(ids(&board, 0), [1, 2]);
        assert_eq!(picked_id(&board), Some(1), "the pick goes with it");
        let (lines, _) = board.draw(100, 12);
        assert!(text(&lines[3]).contains("move house  ▸ 1/2"));
        assert!(cell(&lines[3], 100, 0).trim_end().ends_with("tomorrow"));
        assert_eq!(
            span(&lines[3], "▸ 1/2").style.fg,
            Some(QUIET_ON_HIGHLIGHT),
            "none late"
        );
        // Unfolded, its own date again; nothing written to the task.
        press(&mut board, "z");
        assert_eq!(by_id(&board, 1).due.unwrap().to_string(), "2026-12-30");
        let (lines, _) = board.draw(100, 12);
        assert!(cell(&lines[4], 100, 0).trim_end().ends_with("in 7 weeks"));

        // A late one inside: the count red, and the card's date its.
        board.tasks[2].due = Some("2026-11-09".parse().unwrap());
        board.pick(1);
        press(&mut board, "z");
        board.pick(2);
        let (lines, _) = board.draw(120, 12);
        assert_eq!(span(&lines[3], "▸ 1/2").style.fg, Some(RED));
        assert!(cell(&lines[3], 120, 0).trim_end().ends_with("2 days ago"));
        assert_eq!(span(&lines[3], "2 days ago").style.fg, Some(RED));
        // And on a card with no date of its own.
        board.tasks[0].due = None;
        let (lines, _) = board.draw(120, 12);
        assert!(cell(&lines[3], 120, 0).trim_end().ends_with("2 days ago"));
        // A hidden one due after the card's own date leaves the card's.
        board.tasks.push(sub(
            due(task(5, "pay it", Status::Todo, None, &[]), "2026-12-01"),
            2,
        ));
        board.pick(2);
        press(&mut board, "z");
        let (lines, _) = board.draw(120, 12);
        assert!(
            text(&lines[4]).contains("pay rent  ▸ 0/1"),
            "{:?}",
            text(&lines[4])
        );
        assert!(
            cell(&lines[4], 120, 0).trim_end().ends_with("in 2 days"),
            "{:?}",
            text(&lines[4])
        );
        press(&mut board, "z");
        // Not when it's done.
        board.tasks[2].status = Status::Done;
        let (lines, _) = board.draw(120, 12);
        let house = lines
            .iter()
            .find(|l| text(l).contains("move house"))
            .unwrap();
        assert_ne!(span(house, "▸ 2/2").style.fg, Some(RED));
        assert!(!text(house).contains("ago"));
    }

    #[test]
    fn a_fold_that_moves_the_card_says_why_and_its_unfold_too() {
        // move house, due in 7 weeks, under pay rent, due friday; its open
        // sub item is due tomorrow.
        let mut board = Board::new(
            vec![
                due(task(1, "move house", Status::Todo, None, &[]), "2026-12-30"),
                due(task(2, "pay rent", Status::Todo, None, &[]), "2026-11-13"),
                sub(
                    due(
                        task(3, "book the van", Status::Todo, None, &[]),
                        "2026-11-12",
                    ),
                    1,
                ),
            ],
            today(),
        );
        board.pick(1);
        press(&mut board, "z");
        assert_eq!(ids(&board, 0), [1, 2]);
        assert_eq!(
            board.note.as_deref(),
            Some("Sorted by its sub item's date now: tomorrow.")
        );
        let (lines, _) = board.draw(100, 12);
        assert!(text(&lines[10]).contains("Sorted by its sub item's date now: tomorrow."));
        press(&mut board, "z");
        assert_eq!(ids(&board, 0), [2, 1, 3]);
        assert_eq!(board.note.as_deref(), Some("Sorted by its own date again."));

        // Its place the same, folded or not: nothing said.
        board.tasks[2].parent = Some(2);
        board.pick(2);
        press(&mut board, "z");
        assert_eq!(ids(&board, 0), [2, 1]);
        assert_eq!(board.note, None);
        press(&mut board, "z");
        assert_eq!(board.note, None);
    }

    #[test]
    fn capital_a_on_a_folded_card_unfolds_it_and_another_halils_sub_leaves_it_folded() {
        let mut board = sub_sample();
        press(&mut board, "z");
        // `halil sub 2 …` in another terminal: the count, still folded.
        let mut tasks = board.tasks.clone();
        tasks.push(sub(
            task(9, "print the receipt", Status::Todo, None, &[]),
            2,
        ));
        board.replace_tasks(tasks);
        assert_eq!(ids(&board, 0), [2, 1, 8]);
        let (lines, _) = board.draw(100, 12);
        assert!(text(&lines[3]).contains("▸ 1/4"));
        // A opens it, and its words.
        press(&mut board, "A");
        assert_eq!(ids(&board, 0), [2, 5, 6, 7, 9, 1, 8]);
        assert_eq!(
            board.input.as_ref().map(|i| i.kind),
            Some(InputKind::Sub(r(2)))
        );
        assert!(board.folds_changed);
    }

    #[test]
    fn folding_clears_its_sub_items_marks_and_says_how_many() {
        let mut board = sub_sample();
        press(&mut board, "  ");
        press(&mut board, "j ");
        assert_eq!(marks(&board), BTreeSet::from([2, 5, 7]));
        press(&mut board, "gz");
        assert_eq!(marks(&board), BTreeSet::from([2]), "the card's own stays");
        assert_eq!(
            board.note.as_deref(),
            Some("Cleared the marks on its 2 sub items.")
        );
        press(&mut board, "zj ");
        press(&mut board, "k");
        press(&mut board, "z");
        assert_eq!(
            board.note.as_deref(),
            Some("Cleared the mark on its sub item.")
        );
        press(&mut board, "z");
        press(&mut board, "z");
        assert_eq!(board.note, None, "none to clear: nothing said");
    }

    #[test]
    fn keys_on_a_folded_card_act_on_the_card() {
        let mut board = sub_sample();
        press(&mut board, "z");
        // x still names its sub items.
        press(&mut board, "x");
        let (lines, _) = board.draw(100, 12);
        assert!(
            text(&lines[11]).contains("Remove 2 \"pay rent\" and its 3 sub items?"),
            "{:?}",
            text(&lines[11])
        );
        press(&mut board, "n");
        assert_eq!(
            press(&mut board, "y"),
            [Outcome::Copy {
                text: "pay rent".into(),
                what: "\"pay rent\"".into()
            }]
        );
        // s moves it, folded still; d finishes it and its open sub items.
        run_keys(&mut board, "s");
        go_to(&mut board, 1);
        assert_eq!(ids(&board, 1), [2, 3]);
        run_keys(&mut board, "t");
        board.pick(2);
        assert_eq!(ids(&board, 0), [2, 1, 8]);
        run_keys(&mut board, "d");
        assert!(
            board
                .tasks
                .iter()
                .filter(|t| t.parent == Some(2))
                .all(|t| t.status == Status::Done)
        );
        assert_eq!(
            board.note.as_deref(),
            Some("Done, with its 2 open sub items.")
        );
    }

    /// A fresh, empty folder for one test, under the system's temp folder.
    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("halil-board-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn folds_are_kept_beside_the_tasks_file_across_a_restart() {
        let dir = scratch("folds");
        let path = dir.join("tasks.jsonl");
        let mut board = sub_sample();
        board.tasks[1].stamp = 1_763_500_000_002;
        store::save(&path, &board.tasks).unwrap();
        let tasks_before = fs::read_to_string(&path).unwrap();
        press(&mut board, "z");
        board.keep_folds(&path);
        assert!(!board.folds_changed);
        let kept = fs::read_to_string(dir.join("board.json")).unwrap();
        assert_eq!(kept, "{\"folded\":[{\"id\":2,\"stamp\":1763500000002}]}\n");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            tasks_before,
            "the tasks untouched"
        );
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["board.json", "tasks.jsonl", "tasks.jsonl.lock"],
            "no temp file left; the folds kept under the tasks file's lock"
        );

        // The next run: folded.
        let mut next = Board::new(store::load(&path).unwrap(), today());
        next.folded = load_folds(&folds_path(&path));
        next.keep_folds_in_sight();
        assert_eq!(ids(&next, 0), [2, 1, 8]);
        // A task that took id 2 since, with a stamp of its own: not folded.
        let mut other = sub_sample();
        other.folded = load_folds(&folds_path(&path));
        other.keep_folds_in_sight();
        assert!(other.folded.is_empty());
        assert_eq!(ids(&other, 0), [2, 5, 6, 7, 1, 8]);

        // Unchanged, nothing written.
        fs::remove_file(dir.join("board.json")).unwrap();
        next.keep_folds(&path);
        assert!(!dir.join("board.json").exists());

        // Missing or unreadable: nothing folded, and no error.
        for text in [None, Some("not json"), Some("{\"folded\":3}")] {
            if let Some(text) = text {
                fs::write(dir.join("board.json"), text).unwrap();
            }
            assert!(load_folds(&folds_path(&path)).is_empty(), "{text:?}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    // The calendar.

    fn day_picked(board: &Board) -> Date {
        board.cal.as_ref().expect("the calendar").day
    }

    fn focus(board: &Board) -> Focus {
        board.cal.as_ref().expect("the calendar").focus
    }

    /// The calendar's list: each row's group, id and whether it's faint.
    fn listed(board: &Board) -> Vec<(Group, u32, bool)> {
        board
            .entries()
            .iter()
            .map(|e| (e.group, e.at.id, e.faint))
            .collect()
    }

    fn hit(board: &mut Board, code: KeyCode) -> Outcome {
        board.key(KeyEvent::from(code))
    }

    /// Enter on the input with `text` typed in it, the change made.
    fn enter_with(board: &mut Board, text: &str) {
        let input = board.input.as_mut().expect("the input");
        input.text = text.into();
        input.cursor = text.len();
        if let Outcome::Change(op) = hit(board, KeyCode::Enter) {
            board.apply(op);
        }
    }

    #[test]
    fn c_opens_the_calendar_on_today_and_c_or_esc_go_back_and_from_halil_cal_esc_quits() {
        let mut board = cal_sample();
        press(&mut board, "c");
        assert_eq!(day_picked(&board), today());
        assert_eq!(focus(&board), Focus::Grid);
        assert_eq!(picked_id(&board), None, "on the grid, no task is picked");
        press(&mut board, "c");
        assert!(board.cal.is_none(), "c: back");
        press(&mut board, "ㅊ");
        assert!(board.cal.is_some(), "ㅊ is c");
        assert_eq!(hit(&mut board, KeyCode::Esc), Outcome::Nothing);
        assert!(board.cal.is_none(), "esc: back to the board");

        // `halil cal`: esc quits, after clearing the marks.
        let mut board = cal_sample();
        board.open_cal(false);
        press(&mut board, "j ");
        assert_eq!(marks(&board), BTreeSet::from([6]));
        assert_eq!(hit(&mut board, KeyCode::Esc), Outcome::Nothing);
        assert!(marks(&board).is_empty());
        assert_eq!(hit(&mut board, KeyCode::Esc), Outcome::Quit);
        let mut board = cal_sample();
        board.open_cal(false);
        assert_eq!(press(&mut board, "q"), [Outcome::Quit]);

        // Back on the board, a mark on a sub item it folds away goes.
        let mut board = cal_sample();
        board.folded.insert(r(1));
        press(&mut board, "cj ");
        assert_eq!(marks(&board), BTreeSet::from([6]));
        press(&mut board, "c");
        assert!(marks(&board).is_empty());
    }

    #[test]
    fn the_days_move_as_the_slash_calendars_and_capital_t_goes_back_to_today() {
        let d = jiff::civil::date;
        let mut board = cal_sample();
        press(&mut board, "c");
        let mut to = |code: KeyCode| {
            hit(&mut board, code);
            day_picked(&board)
        };
        assert_eq!(to(KeyCode::Right), d(2026, 11, 12));
        assert_eq!(to(KeyCode::Left), d(2026, 11, 11));
        assert_eq!(to(KeyCode::Down), d(2026, 11, 18));
        assert_eq!(to(KeyCode::Up), d(2026, 11, 11));
        assert_eq!(to(KeyCode::PageDown), d(2026, 12, 11));
        assert_eq!(to(KeyCode::PageUp), d(2026, 11, 11));
        assert_eq!(to(KeyCode::End), d(2026, 11, 30));
        assert_eq!(to(KeyCode::Home), d(2026, 11, 1));
        assert_eq!(to(KeyCode::Char('G')), d(2026, 11, 30));
        assert_eq!(to(KeyCode::Char('g')), d(2026, 11, 1));
        assert_eq!(to(KeyCode::Char('T')), today());
        board.cal.as_mut().unwrap().day = d(2027, 1, 31);
        assert_eq!(hit(&mut board, KeyCode::PageDown), Outcome::Nothing);
        assert_eq!(
            day_picked(&board),
            d(2027, 2, 28),
            "a month on, the month's last"
        );

        // In the list, ↑ ↓ pick; ← → still move the day, back to the grid.
        let mut board = cal_sample();
        press(&mut board, "cj");
        assert_eq!((focus(&board), picked_id(&board)), (Focus::Day, Some(6)));
        hit(&mut board, KeyCode::Down);
        assert_eq!(day_picked(&board), today());
        assert_eq!(picked_id(&board), Some(3));
        hit(&mut board, KeyCode::Right);
        assert_eq!(day_picked(&board), d(2026, 11, 12));
        assert_eq!(focus(&board), Focus::Grid);
        // k on the first row goes back to the grid.
        press(&mut board, "T");
        press(&mut board, "jk");
        assert_eq!(focus(&board), Focus::Grid);
    }

    #[test]
    fn a_days_list_is_due_done_and_added_and_tab_goes_round_to_what_has_no_date() {
        let mut board = cal_sample();
        press(&mut board, "c");
        assert_eq!(
            listed(&board),
            [
                (Group::Due, 6, false),
                (Group::Due, 3, false),
                (Group::Done, 2, false),
                (Group::Added, 3, false),
            ],
            "fix bike in two groups, listed in each"
        );
        hit(&mut board, KeyCode::Tab);
        assert_eq!((focus(&board), picked_id(&board)), (Focus::Day, Some(6)));
        hit(&mut board, KeyCode::Tab);
        assert_eq!(focus(&board), Focus::Unknown);
        assert_eq!(
            listed(&board),
            [(Group::Done, 5, false), (Group::Added, 7, false)],
            "done before 0.5.0; added before 0.3.0"
        );
        assert_eq!(picked_id(&board), Some(5));
        press(&mut board, "j");
        assert_eq!(picked_id(&board), Some(7));
        hit(&mut board, KeyCode::Tab);
        assert_eq!(focus(&board), Focus::Grid);
        hit(&mut board, KeyCode::BackTab);
        assert_eq!(focus(&board), Focus::Unknown);
        // The row says how many, and is drawn on the picked row's ground
        // while it's picked.
        let (lines, _) = board.draw(80, 26);
        let row = lines
            .iter()
            .find(|l| text(l).contains("Date not known"))
            .expect("the row");
        assert_eq!(text(row).trim(), "Date not known: done 1 · added 1");
        assert!(row.iter().any(|s| s.style.bg == Some(HIGHLIGHT)));

        // A day with nothing on it: tab passes its list by.
        press(&mut board, "T");
        hit(&mut board, KeyCode::Right);
        assert!(listed(&board).is_empty());
        let (lines, _) = board.draw(80, 26);
        assert!(
            lines
                .iter()
                .any(|l| text(l).trim() == "Nothing due, done or added on Thu 12 Nov")
        );
        hit(&mut board, KeyCode::Tab);
        assert_eq!(focus(&board), Focus::Unknown);

        // Nothing with no date: no row, and tab passes it by.
        let mut board = cal_sample();
        board.tasks.retain(|t| ![5, 7].contains(&t.id));
        press(&mut board, "c");
        hit(&mut board, KeyCode::Tab);
        hit(&mut board, KeyCode::Tab);
        assert_eq!(focus(&board), Focus::Grid);
        let (lines, _) = board.draw(80, 26);
        assert!(!lines.iter().any(|l| text(l).contains("Date not known")));
    }

    #[test]
    fn the_boards_keys_act_on_the_task_picked_in_the_list() {
        let mut board = cal_sample();
        press(&mut board, "cjj");
        assert_eq!(picked_id(&board), Some(3), "fix bike, in Doing");
        // h and l by its own column, not the board's pick's (in Todo).
        assert_eq!(board.col, 0);
        run_keys(&mut board, "h");
        let status = |board: &Board| board.tasks[board.index(3).unwrap()].status;
        assert_eq!(status(&board), Status::Todo);
        run_keys(&mut board, "l");
        assert_eq!(status(&board), Status::Doing);
        run_keys(&mut board, "d");
        let bike = |board: &Board| board.tasks[board.index(3).unwrap()].clone();
        assert_eq!(bike(&board).status, Status::Done);
        assert_eq!(
            bike(&board).done.map(|d| d.date()),
            Some(today()),
            "done now"
        );
        assert_eq!(picked_id(&board), Some(3), "the pick stays on it");
        // d on a done card keeps it, and its done time; t opens it again,
        // its done time gone.
        let done = bike(&board).done;
        run_keys(&mut board, "d");
        assert_eq!(
            (bike(&board).status, bike(&board).done),
            (Status::Done, done)
        );
        assert_eq!(board.note.as_deref(), Some("3 is done already."));
        run_keys(&mut board, "t");
        assert_eq!(bike(&board).status, Status::Todo);
        assert_eq!(bike(&board).done, None);
        // h l s t by its own column.
        run_keys(&mut board, "l");
        assert_eq!(bike(&board).status, Status::Doing);
        run_keys(&mut board, "t");
        assert_eq!(bike(&board).status, Status::Todo);
        run_keys(&mut board, "s");
        assert_eq!(bike(&board).status, Status::Doing);
        run_keys(&mut board, "l");
        assert_eq!(bike(&board).status, Status::Done);
        assert!(bike(&board).done.is_some());
        run_keys(&mut board, "h");
        assert_eq!(bike(&board).status, Status::Doing);
        assert_eq!(bike(&board).done, None);
        // y, e and x as on the board.
        assert_eq!(
            press(&mut board, "y"),
            [Outcome::Copy {
                text: "fix bike".into(),
                what: "\"fix bike\"".into()
            }]
        );
        press(&mut board, "e");
        assert_eq!(typed(&board), Some("fix bike //2026-11-11 6pm"));
        hit(&mut board, KeyCode::Esc);
        run_keys(&mut board, "xy");
        assert_eq!(board.index(3), None, "removed");
        // On a sub item, h l s and t refuse, as on the board.
        let mut board = cal_sample();
        press(&mut board, "cj");
        assert_eq!(picked_id(&board), Some(6));
        assert_eq!(press(&mut board, "s"), [Outcome::Nothing]);
        assert_eq!(
            board.message.as_deref(),
            Some("a sub item moves with its task")
        );
        press(&mut board, "z");
        assert!(
            board
                .message
                .as_deref()
                .unwrap()
                .contains("z folds on the board")
        );
        // Marked in the list, all at once, a sub item the board folds away
        // too: its mark stays through a change, and y gives it after the
        // board's rows.
        let mut board = cal_sample();
        board.folded.insert(r(1));
        press(&mut board, "cj  ");
        assert_eq!(marks(&board), BTreeSet::from([3, 6]));
        assert_eq!(
            press(&mut board, "y"),
            [Outcome::Copy {
                text: "fix bike\nset up the transfer".into(),
                what: "2 titles".into()
            }]
        );
        run_keys(&mut board, "l");
        assert_eq!(
            marks(&board),
            BTreeSet::from([3, 6]),
            "kept through a change"
        );
        assert_eq!(
            board.message.as_deref(),
            Some("1 sub item stayed: a sub item moves with its task")
        );
        run_keys(&mut board, "d");
        assert!([3, 6].iter().all(|&id| {
            let t = &board.tasks[board.index(id).unwrap()];
            t.status == Status::Done && t.done.is_some()
        }));
    }

    #[test]
    fn a_task_that_leaves_the_day_stays_faint_until_the_pick_moves_and_the_note_says_where() {
        let mut board = cal_sample();
        press(&mut board, "cj");
        press(&mut board, "e");
        enter_with(&mut board, "set up the transfer //2026-11-13");
        assert_eq!(board.note.as_deref(), Some("6 is due Fri 13 Nov now."));
        assert_eq!(listed(&board)[0], (Group::Due, 6, true));
        assert_eq!(picked_id(&board), Some(6), "the pick stays under the key");
        let (lines, _) = board.draw(80, 26);
        let row = lines
            .iter()
            .find(|l| text(l).contains("set up the transfer"))
            .unwrap();
        assert!(
            row.iter()
                .filter(|s| s.text.contains("set up"))
                .all(|s| s.style.fg == Some(QUIETER)),
            "faint"
        );
        // The pick moves: it's gone, and the pick is on the next task.
        press(&mut board, "j");
        assert!(!listed(&board).iter().any(|e| e.1 == 6));
        assert_eq!(picked_id(&board), Some(3));

        // Opened again from a day it was done on, with no date.
        let mut board = cal_sample();
        press(&mut board, "c");
        board.cal.as_mut().unwrap().day = jiff::civil::date(2026, 11, 3);
        press(&mut board, "j");
        assert_eq!(picked_id(&board), Some(8));
        run_keys(&mut board, "t");
        assert_eq!(board.note.as_deref(), Some("8 is open, with no date now."));
        assert_eq!(listed(&board), [(Group::Done, 8, true)]);

        // Done today, from a day it was due on: done on today, said so.
        let mut board = cal_sample();
        press(&mut board, "c");
        board.cal.as_mut().unwrap().day = jiff::civil::date(2026, 11, 9);
        press(&mut board, "j");
        run_keys(&mut board, "d");
        assert_eq!(board.note.as_deref(), Some("4 is done on Wed 11 Nov now."));
    }

    #[test]
    fn a_on_a_day_fills_in_its_date_and_the_new_task_is_picked_there() {
        let mut board = cal_sample();
        press(&mut board, "c");
        hit(&mut board, KeyCode::Right);
        hit(&mut board, KeyCode::Right);
        press(&mut board, "a");
        assert_eq!(typed(&board), Some(" //2026-11-13"));
        assert_eq!(board.input.as_ref().unwrap().cursor, 0, "before the date");
        run_keys(&mut board, "water the fern");
        assert_eq!(typed(&board), Some("water the fern //2026-11-13"));
        if let Outcome::Change(op) = hit(&mut board, KeyCode::Enter) {
            board.apply(op);
        }
        let new = board.tasks.last().unwrap().clone();
        assert_eq!(new.title, "water the fern");
        assert_eq!(
            new.due.map(|d| d.date),
            Some(jiff::civil::date(2026, 11, 13))
        );
        assert_eq!(new.added.map(|a| a.date()), Some(today()), "added now");
        assert_eq!(
            (focus(&board), picked_id(&board)),
            (Focus::Day, Some(new.id))
        );
        // Typed to another day: said where it went.
        press(&mut board, "a");
        enter_with(&mut board, "buy stamps //2026-11-20");
        let id = board.tasks.last().unwrap().id;
        assert_eq!(
            board.note.as_deref(),
            Some(format!("Added {id}, due Fri 20 Nov.").as_str())
        );
        // Where nothing has a date, a starts empty.
        hit(&mut board, KeyCode::Tab);
        assert_eq!(focus(&board), Focus::Unknown);
        press(&mut board, "a");
        assert_eq!(typed(&board), Some(""));
    }

    #[test]
    fn at_46_by_10_the_week_shows_whole() {
        let mut board = cal_sample();
        press(&mut board, "c");
        let (lines, _) = board.draw(46, 10);
        let screen: Vec<String> = lines
            .iter()
            .map(|l| text(l).trim_end().to_string())
            .collect();
        assert_eq!(
            screen,
            [
                "  November 2026            due n · ● done n",
                "  Su  8 Mo  9 Tu 10 We 11 Th 12 Fr 13 Sa 14",
                "            1           2           1",
                "                       ●1",
                "  Date not known: done 1 · added 1",
                "  Due      set up the transfer        today",
                "         • fix bike                   today",
                "  Done     call mum",
                "",
                "  ←→ day  ↑↓ week  tab list  c board  T today",
            ]
        );
        assert!(lines.iter().all(|l| text(l).width() <= 46));
        // Under 10 rows, or 46 columns: it asks for room.
        for (w, h) in [(46, 9), (45, 10)] {
            let (lines, _) = board.draw(w, h);
            assert_eq!(
                text(&lines[0]),
                truncate("  Make the window larger for the calendar", w)
            );
        }
    }

    #[test]
    fn a_late_days_due_count_is_red_and_todays_day_blue() {
        let mut board = cal_sample();
        press(&mut board, "c");
        hit(&mut board, KeyCode::Right);
        let (lines, _) = board.draw(80, 26);
        let span = |line: &Line, at: usize, s: &str| {
            let mut x = 0;
            for span in line {
                if x <= at && at < x + span.text.width() && span.text.contains(s) {
                    return Some(span.style);
                }
                x += span.text.width();
            }
            None
        };
        // The week of the 8th: its day line, then due.
        let week = lines
            .iter()
            .position(|l| text(l).trim_start().starts_with("8 "))
            .unwrap();
        let due = &lines[week + 1];
        let monday = cal::cell_x(1, 10) + 9;
        let friday = cal::cell_x(5, 10) + 9;
        assert_eq!(
            span(due, monday, "due 1").unwrap().fg,
            Some(RED),
            "the 9th, gone"
        );
        assert_eq!(
            span(due, friday, "due 1").unwrap().fg,
            Some(AMBER),
            "the 13th"
        );
        let wednesday = cal::cell_x(3, 10) + 9;
        assert_eq!(span(&lines[week], wednesday, "11").unwrap().fg, Some(BLUE));
        let thursday = cal::cell_x(4, 10) + 9;
        let picked = span(&lines[week], thursday, "12").unwrap();
        assert_eq!(
            (picked.fg, picked.bg),
            (Some(ON_HIGHLIGHT), Some(HIGHLIGHT))
        );
    }

    /// Types `s` into the input, a key at a time, as typed.
    fn type_in(board: &mut Board, s: &str) {
        for c in s.chars() {
            board.key(KeyEvent::from(KeyCode::Char(c)));
        }
    }

    /// The ids in column `c`, as shown.
    fn shown_ids(board: &Board, c: usize) -> Vec<u32> {
        board.column(c).iter().map(|&i| board.tasks[i].id).collect()
    }

    #[test]
    fn slash_narrows_as_its_typed_enter_keeps_it_and_esc_drops_or_clears_it() {
        let mut board = sub_sample();
        press(&mut board, "/");
        assert_eq!(
            board.input.as_ref().map(|i| i.kind),
            Some(InputKind::Filter)
        );
        type_in(&mut board, "rent");
        assert_eq!(
            shown_ids(&board, 0),
            [2, 5, 6, 7],
            "the card, drawn as it is"
        );
        assert!(shown_ids(&board, 1).is_empty() && shown_ids(&board, 2).is_empty());
        // A half-typed `//`: no error while typing, and it doesn't narrow yet.
        type_in(&mut board, " //fr");
        assert_eq!(board.message, None);
        assert_eq!(shown_ids(&board, 0), [2, 5, 6, 7]);
        assert_eq!(
            board.input.as_ref().unwrap().cal,
            None,
            "no calendar in the filter line"
        );
        // Enter on it: refused in words, the words kept.
        assert_eq!(hit(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.message.as_deref(), Some("`//fr` isn't a date"));
        assert_eq!(typed(&board), Some("rent //fr"));
        for _ in 0..5 {
            hit(&mut board, KeyCode::Backspace);
        }
        hit(&mut board, KeyCode::Enter);
        assert_eq!(board.input, None, "kept, the keys the board's");
        assert_eq!(board.filter.as_ref().map(|f| f.text.as_str()), Some("rent"));
        let f = footer(&mut board, 80);
        assert!(f.starts_with("  / rent · 1 of 5 "), "{f}");
        assert!(f.contains(" esc clear "), "{f}");
        // `/` again edits it; esc while typing drops what's typed, the kept
        // one back.
        press(&mut board, "/");
        assert_eq!(typed(&board), Some("rent"));
        type_in(&mut board, " +zzz");
        assert!(shown_ids(&board, 0).is_empty());
        hit(&mut board, KeyCode::Esc);
        assert_eq!(board.filter.as_ref().map(|f| f.text.as_str()), Some("rent"));
        // Esc on the board clears it; the next quits.
        assert_eq!(hit(&mut board, KeyCode::Esc), Outcome::Nothing);
        assert_eq!(board.filter, None);
        assert_eq!(hit(&mut board, KeyCode::Esc), Outcome::Quit);
        // Hangul in the filter line is text, not keys.
        let mut board = sub_sample();
        press(&mut board, "/");
        board.key(KeyEvent::from(KeyCode::Char('월')));
        assert_eq!(typed(&board), Some("월"));
    }

    #[test]
    fn the_pick_stays_on_its_card_else_goes_to_the_first_shown() {
        let filtered = |board: &mut Board, text: &str| {
            press(board, "/");
            type_in(board, text);
            hit(board, KeyCode::Enter);
        };
        // It matches: it stays.
        let mut board = sub_sample();
        assert_eq!(picked_id(&board), Some(2));
        filtered(&mut board, "+home");
        assert_eq!(picked_id(&board), Some(2));
        // It doesn't: the first shown in its column.
        let mut board = sub_sample();
        press(&mut board, "jjjj");
        assert_eq!(picked_id(&board), Some(1));
        filtered(&mut board, "rent");
        assert_eq!((board.col, picked_id(&board)), (0, Some(2)));
        // Its column shows nothing: the first shown.
        let mut board = sub_sample();
        go_to(&mut board, 1);
        assert_eq!(picked_id(&board), Some(3));
        filtered(&mut board, "milk");
        assert_eq!((board.col, picked_id(&board)), (0, Some(1)));
        // Clearing keeps the pick.
        hit(&mut board, KeyCode::Esc);
        assert_eq!(picked_id(&board), Some(1));
    }

    #[test]
    fn what_a_change_takes_out_of_the_filter_stays_faint_until_it_changes() {
        let mut board = sub_sample();
        press(&mut board, "/");
        type_in(&mut board, "+home");
        hit(&mut board, KeyCode::Enter);
        assert_eq!(picked_id(&board), Some(2));
        press(&mut board, "e");
        enter_with(&mut board, "pay rent -home");
        assert_eq!(board.note.as_deref(), Some("No longer matches +home."));
        assert_eq!(shown_ids(&board, 0)[0], 2, "still shown");
        let (lines, _) = board.draw(100, 12);
        let row = &lines[3];
        assert!(
            row.iter()
                .filter(|s| s.text.contains("pay rent"))
                .all(|s| s.style.fg == Some(QUIETER)),
            "faint"
        );
        // Added under it, not matching: there too, faint.
        press(&mut board, "a");
        enter_with(&mut board, "water plants");
        let id = board.tasks.last().unwrap().id;
        assert!(shown_ids(&board, 0).contains(&id));
        assert_eq!(board.note.as_deref(), Some("No longer matches +home."));
        // The filter changes: they go.
        press(&mut board, "/");
        hit(&mut board, KeyCode::Enter);
        press(&mut board, "/");
        type_in(&mut board, "+home");
        hit(&mut board, KeyCode::Enter);
        assert!(shown_ids(&board, 0).is_empty());
        let (lines, _) = board.draw(100, 12);
        assert!(lines.iter().any(|l| text(l).trim() == "Nothing matches"));
        // Counts are of what's shown.
        assert!(text(&lines[1]).contains("Todo  0"), "{}", text(&lines[1]));
    }

    #[test]
    fn the_kept_filter_line_at_46_never_gives_way() {
        let mut board = sub_sample();
        press(&mut board, "/");
        type_in(&mut board, "+home rent");
        hit(&mut board, KeyCode::Enter);
        let f = footer(&mut board, 46);
        assert!(f.starts_with("  / +home rent · 1 of 5 "), "{f}");
        assert!(f.width() <= 46, "{f}");
        // The hints gave way before it: at 46 one is left at most.
        let hints = f.trim_start_matches("  / +home rent · 1 of 5 ");
        assert!(hints.width() <= 46 - 24, "{f}");
        // While it's typed: its hint for the dates.
        press(&mut board, "/");
        let f = footer(&mut board, 80);
        assert!(f.starts_with("  /  +home rent"), "{f}");
        assert!(f.ends_with("//fri  //by fri  //late"), "{f}");
    }

    #[test]
    fn the_calendar_counts_only_what_the_filter_shows_and_c_keeps_it() {
        let mut board = cal_sample();
        press(&mut board, "/");
        type_in(&mut board, "+home");
        hit(&mut board, KeyCode::Enter);
        let on_board = footer(&mut board, 80);
        press(&mut board, "c");
        assert!(board.filter.is_some(), "c keeps it");
        // One count, cards, in both views: 1 of the 8 cards (9 tasks, one a
        // sub item).
        let on_calendar = footer(&mut board, 80);
        assert!(on_board.starts_with("  / +home · 1 of 8 "), "{on_board}");
        assert!(
            on_calendar.starts_with("  / +home · 1 of 8 "),
            "{on_calendar}"
        );
        let counts = cal::counts(&board.visible());
        let on = |d| {
            counts
                .get(&jiff::civil::date(2026, 11, d))
                .copied()
                .unwrap_or_default()
        };
        assert_eq!(on(13), cal::Counts { due: 1, done: 0 }, "pay rent");
        assert_eq!(
            on(11),
            cal::Counts::default(),
            "nothing on today with +home"
        );
        assert_eq!(on(3), cal::Counts::default(), "file taxes hasn't +home");
        board.cal.as_mut().unwrap().day = jiff::civil::date(2026, 11, 13);
        assert_eq!(listed(&board), [(Group::Due, 1, false)]);
        // The not-known row counts what's shown too: neither has +home.
        let (lines, _) = board.draw(80, 26);
        assert!(!lines.iter().any(|l| text(l).contains("Date not known")));
        press(&mut board, "c");
        assert!(board.filter.is_some(), "and back");
    }

    /// The sample, pay rent due Friday and repeating weekly.
    fn weekly_sample() -> Board {
        let mut board = sample();
        let rent = board.index(2).unwrap();
        board.tasks[rent].due = Some("2026-11-13".parse().unwrap());
        board.tasks[rent].every = Some("week".parse().unwrap());
        board
    }

    #[test]
    fn d_on_a_repeating_card_says_its_next_and_t_on_the_done_one_where_it_went() {
        let mut board = weekly_sample();
        assert_eq!(picked_id(&board), Some(2));
        run_keys(&mut board, "d");
        assert_eq!(board.note.as_deref(), Some("Done. Next: fri nov 20."));
        let next = board.tasks.last().unwrap().clone();
        assert_eq!(
            (
                next.title.as_str(),
                next.due.unwrap().to_string(),
                next.status
            ),
            ("pay rent", "2026-11-20".into(), Status::Todo)
        );
        assert_eq!(next.every, Some("week".parse().unwrap()));
        assert_eq!(
            by_id(&board, 2).every,
            None,
            "the done one no longer repeats"
        );
        // The done one opened again: a plain task, and where its repeat is.
        go_to(&mut board, 2);
        while picked_id(&board) != Some(2) {
            press(&mut board, "j");
        }
        run_keys(&mut board, "t");
        assert_eq!(
            board.note.as_deref(),
            Some(format!("Reopened. Its repeat is on #{}.", next.id).as_str())
        );
        assert_eq!(
            (by_id(&board, 2).every, by_id(&board, 2).next),
            (None, None)
        );
        // x on a repeating one removes it and makes nothing.
        let mut board = weekly_sample();
        let n = board.tasks.len();
        run_keys(&mut board, "xy");
        assert_eq!(board.tasks.len(), n - 1);
    }

    #[test]
    fn e_holds_the_repeat_and_the_date_rule_says_what_deleting_it_does() {
        let mut board = weekly_sample();
        press(&mut board, "e");
        assert_eq!(typed(&board), Some("pay rent //2026-11-13 every week"));
        // The step's words taken out: the step stays, as the date stays when
        // its words are taken out (0.4.1's rule for the date).
        enter_with(&mut board, "pay rent //2026-11-14");
        assert_eq!(by_id(&board, 2).every, Some("week".parse().unwrap()));
        assert_eq!(by_id(&board, 2).due.unwrap().to_string(), "2026-11-14");
        assert_eq!(
            board.note.as_deref(),
            Some("Still every week — -every stops it.")
        );
        // The same line again, nothing else to change: said all the same.
        press(&mut board, "e");
        enter_with(&mut board, "pay rent //2026-11-14");
        assert_eq!(
            board.note.as_deref(),
            Some("Still every week — -every stops it.")
        );
        // The step's words left in the line: nothing to say.
        press(&mut board, "e");
        assert_eq!(typed(&board), Some("pay rent //2026-11-14 every week"));
        hit(&mut board, KeyCode::Enter);
        assert_eq!(board.note, None);
        // -every stops it.
        press(&mut board, "e");
        enter_with(&mut board, "pay rent //2026-11-14 every week -every");
        assert_eq!(
            by_id(&board, 2).every,
            Some("week".parse().unwrap()),
            "a new step wins"
        );
        press(&mut board, "e");
        enter_with(&mut board, "pay rent -every");
        assert_eq!(by_id(&board, 2).every, None);
        // A on a card: a sub item repeats with its card.
        let mut board = weekly_sample();
        press(&mut board, "A");
        let input = board.input.as_mut().unwrap();
        input.text = "x //every week".into();
        input.cursor = input.text.len();
        assert_eq!(hit(&mut board, KeyCode::Enter), Outcome::Nothing);
        assert_eq!(board.message.as_deref(), Some(crate::REPEATS_WITH_ITS_CARD));
    }

    #[test]
    fn a_cut_repeating_card_shows_a_star_and_its_overlay_the_step() {
        let mut board = weekly_sample();
        let rent = board.index(2).unwrap();
        board.tasks[rent].title = "pay the rent for november and keep the receipt".into();
        board.tasks[rent].tags.clear();
        // Not picked, cut: `*` right after the date (at 100: at 80 a column
        // has no room for the date at all, as ever).
        press(&mut board, "j");
        let (lines, _) = board.draw(100, 12);
        let row = text(&lines[3]);
        assert!(row.contains("in 2 days*"), "{row}");
        assert!(!row.contains("weekly"), "{row}");
        // Picked: its overlay spells the step, at 80 too.
        press(&mut board, "k");
        for w in [80, 100] {
            let (lines, _) = board.draw(w, 12);
            let row = text(&lines[3]);
            assert!(row.contains("in 2 days weekly"), "{w}: {row}");
        }
        // Room for the title whole: the step's words after the date, faint.
        board.tasks[rent].title = "pay rent".into();
        press(&mut board, "j");
        let (lines, _) = board.draw(120, 12);
        let row = &lines[3];
        assert!(text(row).contains("in 2 days weekly"), "{}", text(row));
        let step = row.iter().find(|s| s.text == "weekly").unwrap();
        assert_eq!(step.style.fg, Some(QUIETER));
        // A whole title takes the step's words, though they leave it less
        // than the 8 cells a cut title keeps: `*` is for a cut one.
        board.tasks[rent].title = "water".into();
        board.tasks[rent].due = Some("2026-11-11".parse().unwrap());
        let (lines, _) = board.draw(80, 12);
        let row = text(&lines[3]);
        assert!(row.contains("water  today weekly"), "{row}");
    }

    #[test]
    fn the_calendar_draws_only_real_tasks_and_keeps_typed_steps() {
        let mut board = weekly_sample();
        press(&mut board, "c");
        let counts = cal::counts(&board.tasks);
        let fridays =
            [13, 20, 27].map(|d| counts.get(&jiff::civil::date(2026, 11, d)).map(|c| c.due));
        assert_eq!(fridays, [Some(1), None, None], "no coming repeat drawn");
        // Its day's list: the step after the date.
        board.cal.as_mut().unwrap().day = jiff::civil::date(2026, 11, 13);
        let (lines, _) = board.draw(100, 26);
        assert!(
            lines
                .iter()
                .any(|l| text(l).contains("pay rent") && text(l).contains("weekly"))
        );
        // The `//` calendar moves the date and leaves the typed step.
        let mut board = sample();
        press(&mut board, "a");
        let input = board.input.as_mut().unwrap();
        input.text = "x //fri every week".into();
        input.cursor = input.text.len();
        input.cal = Some(Cal { at: 2 });
        hit(&mut board, KeyCode::Down);
        assert_eq!(typed(&board), Some("x //2026-11-20 every week"));
    }

    #[test]
    fn the_popup_on_a_weekday_step_shows_and_writes_the_day_its_due() {
        let open = |board: &mut Board, text: &str| {
            press(board, "a");
            let input = board.input.as_mut().unwrap();
            input.text = text.into();
            input.cursor = input.text.len();
            input.cal = Some(Cal { at: 2 });
        };
        // It shows what the line reads as: nov 28, a saturday, every friday
        // is due friday dec 4, and that's the day picked.
        let mut board = sample();
        open(&mut board, "x //nov 28 every fri");
        let (lines, _) = board.draw(80, 20);
        let top = lines
            .iter()
            .position(|l| text(l).contains("December 2026"))
            .expect("its month");
        let picked: Vec<String> = lines[top..top + 9]
            .iter()
            .flat_map(|l| l.iter())
            .filter(|s| s.style.bg == Some(HIGHLIGHT))
            .map(|s| s.text.trim().to_string())
            .collect();
        assert_eq!(picked, ["4"], "friday the 4th picked");
        // A move rewrites the weekday to the picked day's, as the date's words.
        hit(&mut board, KeyCode::Right);
        assert_eq!(typed(&board), Some("x //2026-12-05 every sat"));
        let mut board = sample();
        open(&mut board, "x //every fri");
        hit(&mut board, KeyCode::Right);
        assert_eq!(typed(&board), Some("x //2026-11-14 every sat"));
        hit(&mut board, KeyCode::Down);
        assert_eq!(typed(&board), Some("x //2026-11-21 every sat"));
        // Other steps stay as typed.
        let mut board = sample();
        open(&mut board, "x //fri every 2 weeks");
        hit(&mut board, KeyCode::Right);
        assert_eq!(typed(&board), Some("x //2026-11-14 every 2 weeks"));
        // Typed by hand, `//sat every fri` keeps its reading: the first friday
        // on or after saturday.
        let p = parse::parse(&["x //sat every fri".to_string()], board.now()).unwrap();
        assert_eq!(p.due.unwrap().to_string(), "2026-11-20");
    }

    #[test]
    fn the_folds_are_kept_under_the_tasks_files_lock() {
        let dir = env::temp_dir().join(format!("halil-test-{}-folds-lock", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.jsonl");
        let mut board = sub_sample();
        board.folded.insert(r(2));
        board.folds_changed = true;
        // Another halil holds the file for 300 ms: the folds wait for it.
        let held = store::lock(&path).unwrap();
        let other = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let start = std::time::Instant::now();
        board.keep_folds(&path);
        let waited = start.elapsed();
        other.join().unwrap();
        assert!(waited >= Duration::from_millis(250), "{waited:?}");
        assert_eq!(board.message, None);
        assert_eq!(load_folds(&folds_path(&path)), BTreeSet::from([r(2)]));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_board_save_racing_an_add_keeps_both() {
        let dir = env::temp_dir().join(format!("halil-test-{}-board-race", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.jsonl");
        let mut board = sample();
        for t in &mut board.tasks {
            t.stamp = 1_763_500_000_000 + u64::from(t.id);
        }
        store::save(&path, &board.tasks).unwrap();
        let one = board.held(board.index(1).unwrap());
        // `halil add` from another terminal, holding the file between its
        // read and its write.
        let (holding, held) = mpsc::channel();
        let add = {
            let path = path.clone();
            std::thread::spawn(move || {
                store::update(&path, |tasks| {
                    holding.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    let id = task::next_id(tasks);
                    tasks.push(task(id, "added elsewhere", Status::Todo, None, &[]));
                    Ok(((), true))
                })
                .unwrap();
            })
        };
        held.recv().unwrap();
        // Meanwhile the board finishes 1: it waits, then makes its change to
        // the file as the add left it.
        board.save_change(&path, Op::Finish { refs: vec![one] });
        add.join().unwrap();
        let tasks = store::load(&path).unwrap();
        assert!(
            tasks.iter().any(|t| t.title == "added elsewhere"),
            "the add kept"
        );
        let one = tasks.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(one.status, Status::Done, "the board's change kept");
        assert!(
            board.tasks.iter().any(|t| t.title == "added elsewhere"),
            "and the board shows the add"
        );
        assert_eq!(board.message, None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_boards_footer_shows_c_calendar_at_80_in_every_case() {
        let mut board = sample();
        for c in 0..COLUMNS.len() {
            go_to(&mut board, c);
            let f = footer(&mut board, 80);
            assert!(f.contains("  c calendar "), "column {c}: {f}");
        }
        let mut board = sub_sample();
        press(&mut board, "j");
        let f = footer(&mut board, 80);
        assert!(f.contains("  c calendar "), "a sub item: {f}");
        // Narrower, x gives way before / and c, and / before c.
        let mut board = sample();
        go_to(&mut board, 2);
        let f = footer(&mut board, 56);
        assert!(
            f.contains(" c calendar ") && f.contains(" / filter ") && !f.contains(" x remove "),
            "{f}"
        );
        let f = footer(&mut board, 46);
        assert!(
            f.contains(" c calendar ") && !f.contains(" / filter "),
            "{f}"
        );
    }

    #[test]
    fn the_calendars_footer_shows_the_way_back_at_every_width() {
        let mut board = cal_sample();
        press(&mut board, "c");
        for (state, keys) in [
            ("the grid", ""),
            ("a sub item", "j"),
            ("a card in Doing", "j"),
            ("a done card", "j"),
        ] {
            press(&mut board, keys);
            for w in 46..=240 {
                let f = footer(&mut board, w);
                assert!(f.contains("  c board "), "{state}, {w}: {f}");
            }
        }
        board.key(KeyEvent::from(KeyCode::Tab));
        for w in 46..=240 {
            let f = footer(&mut board, w);
            assert!(f.contains("  c board "), "what has no date, {w}: {f}");
        }
        // On the grid, right after the moves and tab, ahead of T and a.
        let mut board = cal_sample();
        press(&mut board, "c");
        assert_eq!(
            footer(&mut board, 46),
            "  ←→ day  ↑↓ week  tab list  c board  T today "
        );
        assert_eq!(
            footer(&mut board, 80),
            "  ←→ day  ↑↓ week  pgup pgdn month  tab list  c board  / filter  T today  a add "
        );
    }

    #[test]
    fn d_in_the_calendar_keeps_a_done_task_done_and_the_moves_open_it() {
        let mut board = cal_sample();
        // 6, a sub item, done today too.
        let six = board.index(6).unwrap();
        board.tasks[six] = done_at(
            board.tasks[six].clone(),
            "2026-11-11T08:00:00+09:00[Asia/Seoul]",
        );
        press(&mut board, "c");
        assert_eq!(
            listed(&board),
            [
                (Group::Due, 3, false),
                (Group::Done, 2, false),
                (Group::Done, 6, false),
                (Group::Added, 3, false),
            ]
        );
        let at = |board: &Board, id: u32| board.tasks[board.index(id).unwrap()].clone();
        for (id, keys) in [(2, "jj"), (6, "j")] {
            press(&mut board, keys);
            assert_eq!(picked_id(&board), Some(id));
            let before = at(&board, id);
            assert_eq!(press(&mut board, "d"), [Outcome::Nothing]);
            assert_eq!(at(&board, id), before, "{id}: done, its time kept");
            assert_eq!(
                board.note.as_deref(),
                Some(format!("{id} is done already.").as_str())
            );
            // No d where a done task is picked.
            let f = footer(&mut board, 120);
            assert!(!f.contains(" d "), "{f}");
        }
        // A done card's footer has the moves that open it, and they do.
        press(&mut board, "k");
        assert_eq!(picked_id(&board), Some(2));
        let f = footer(&mut board, 80);
        assert!(f.contains(" h l move ") && f.contains(" s doing "), "{f}");
        run_keys(&mut board, "s");
        assert_eq!(
            (at(&board, 2).status, at(&board, 2).done),
            (Status::Doing, None)
        );
        // What has no date: the moves, no d.
        let mut board = cal_sample();
        press(&mut board, "c");
        board.key(KeyEvent::from(KeyCode::Tab));
        board.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(picked_id(&board), Some(5));
        let f = footer(&mut board, 46);
        assert_eq!(f, "  h l move  c board  / filter  a add  e edit ");
    }

    #[test]
    fn every_change_of_status_on_the_board_sets_keeps_or_clears_the_done_time() {
        let mut board = sub_sample();
        let at = |board: &Board, id: u32| board.tasks[board.index(id).unwrap()].clone();
        // d on a card: it and its open sub items done now; 5, done before,
        // keeps no time.
        board.apply(Op::Finish { refs: refs(&[2]) });
        for id in [2, 6, 7] {
            let t = at(&board, id);
            assert_eq!(
                (t.status, t.done.map(|d| d.date())),
                (Status::Done, Some(today()))
            );
        }
        assert_eq!(at(&board, 5).done, None);
        // Done already: its time kept.
        let kept = at(&board, 2).done;
        board.apply(Op::Finish { refs: refs(&[2]) });
        assert_eq!(at(&board, 2).done, kept);
        // Out of Done, by s, t or h: none.
        board.apply(Op::Send {
            refs: refs(&[2]),
            to: Status::Todo,
        });
        assert_eq!(at(&board, 2).done, None);
        assert!(at(&board, 6).done.is_some(), "its sub items stay done");
        board.apply(Op::Shift {
            refs: refs(&[2]),
            by: 1,
        });
        board.apply(Op::Shift {
            refs: refs(&[2]),
            by: 1,
        });
        assert!(at(&board, 2).done.is_some(), "l into Done");
        board.apply(Op::Move {
            at: r(2),
            to: Status::Doing,
            follow: false,
        });
        assert_eq!(at(&board, 2).done, None);
        board.apply(Op::Move {
            at: r(6),
            to: Status::Todo,
            follow: false,
        });
        assert_eq!(at(&board, 6).done, None, "a sub item opened again");
        // A task added on the board: made now.
        board.apply(Op::Add {
            task: parse::parse(&["x".to_string()], board.now()).unwrap(),
            parent: None,
        });
        let new = board.tasks.last().unwrap();
        assert_eq!(new.added.as_ref().map(|a| a.date()), Some(today()));
        assert_eq!(new.done, None);
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

        // 0.4.0: tab lands on the row on the same line.
        let mut board = mixed();
        press(&mut board, "jjj");
        shoot("lines-80", "Todo, line four picked", &mut board, 80, 18);
        board.key(KeyEvent::from(KeyCode::Tab));
        shoot(
            "lines-80",
            "tab: Doing is shorter, its last row",
            &mut board,
            80,
            18,
        );
        board.key(KeyEvent::from(KeyCode::BackTab));
        press(&mut board, "k");
        shoot(
            "lines-80",
            "back, then k: Todo's line one",
            &mut board,
            80,
            18,
        );
        board.key(KeyEvent::from(KeyCode::Tab));
        shoot("lines-80", "tab: Doing's line one", &mut board, 80, 18);
        board.key(KeyEvent::from(KeyCode::BackTab));
        press(&mut board, "jjj");
        board.key(KeyEvent::from(KeyCode::Tab));
        board.key(KeyEvent::from(KeyCode::Tab));
        shoot(
            "lines-80",
            "from line four, tab tab: Done's last row",
            &mut board,
            80,
            18,
        );

        // 0.4.0: the calendar.
        for w in [80, 60] {
            let sheet = format!("calendar-{w}");
            let mut board = mixed();
            press(&mut board, "apay rent /");
            shoot(&sheet, "a, `pay rent /` typed", &mut board, w, 20);
            press(&mut board, "/");
            shoot(
                &sheet,
                "the second /: open, today in the accent",
                &mut board,
                w,
                20,
            );
            press(&mut board, "fri");
            shoot(&sheet, "fri typed: the calendar follows", &mut board, w, 20);
            press(&mut board, " 6pm");
            board.key(KeyEvent::from(KeyCode::Right));
            shoot(
                &sheet,
                "6pm typed, then →: the day written, the time kept",
                &mut board,
                w,
                20,
            );
            board.key(KeyEvent::from(KeyCode::PageDown));
            board.key(KeyEvent::from(KeyCode::PageDown));
            shoot(
                &sheet,
                "pgdn twice: another year, with it",
                &mut board,
                w,
                20,
            );
            board.key(KeyEvent::from(KeyCode::Esc));
            shoot(&sheet, "esc: closed, the words left", &mut board, w, 20);

            // 0.4.1: `e` fills in the date; the cursor moved into it opens
            // the calendar there.
            let mut board = mixed();
            board.pick(8);
            press(&mut board, "e");
            shoot(
                &sheet,
                "e on 8 (due nov 16): its title, then //2026-11-16",
                &mut board,
                w,
                20,
            );
            ctrl(&mut board, 'e');
            shoot(
                &sheet,
                "ctrl-e: the calendar on its date",
                &mut board,
                w,
                20,
            );
            board.key(KeyEvent::from(KeyCode::Down));
            shoot(&sheet, "↓: a week on from its date", &mut board, w, 20);

            let mut board = mixed();
            press(&mut board, "apay rent //");
            shoot(&sheet, "9 rows: no room", &mut board, w, 9);
        }

        // 0.4.1: the line `e` fills in, before and after.
        let mut board = mixed();
        board.tasks.push(due(
            task(14, "plan the friday party", Status::Todo, None, &["home"]),
            "2026-11-11 18:00",
        ));
        board.pick(14);
        let i = board.picked().unwrap();
        let kind = InputKind::Edit(board.held(i));
        board.input = Some(Input::new(kind, board.tasks[i].title.clone()));
        shoot(
            "edit-80",
            "before, 0.4.0: e on 14 (due today 6pm) fills in the title alone; enter makes it `plan the party`, due fri",
            &mut board,
            80,
            14,
        );
        board.input = None;
        press(&mut board, "e");
        shoot(
            "edit-80",
            "after, 0.4.1: the title, then //2026-11-11 6pm; enter untouched changes nothing",
            &mut board,
            80,
            14,
        );

        // 0.4.0: the picked row over its neighbours.
        for w in [80, 60] {
            let sheet = format!("overlay-{w}");
            let mut board = sub_sample();
            board.tasks[3] = task(4, "call mum", Status::Done, None, &[]);
            shoot(
                &sheet,
                "pay rent over fix bike, cut where it ends",
                &mut board,
                w,
                12,
            );
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
                    task(4, "call mum", Status::Done, None, &[]),
                ],
                today(),
            );
            shoot(&sheet, "Hangul under the overlay's end", &mut board, w, 12);
            let mut board = Board::new(
                vec![
                    task(1, "buy milk", Status::Todo, None, &[]),
                    due(
                        task(
                            2,
                            "fix the bike before the weekend ride with the club and the long climb",
                            Status::Doing,
                            None,
                            &["bike"],
                        ),
                        "2026-11-12",
                    ),
                    task(3, "call mum", Status::Done, None, &[]),
                    task(5, "oil the chain", Status::Doing, None, &[]),
                ],
                today(),
            );
            go_to(&mut board, 1);
            shoot(
                &sheet,
                "a wrapped row in Doing, over oil the chain",
                &mut board,
                w,
                12,
            );
            let mut board = Board::new(
                vec![
                    task(1, "read the whole lease again", Status::Done, None, &[]),
                    task(2, "call mum", Status::Done, None, &[]),
                    task(3, "water plants", Status::Done, None, &[]),
                    task(4, "buy milk", Status::Todo, None, &[]),
                ],
                today(),
            );
            go_to(&mut board, 2);
            shoot(
                &sheet,
                "wrapped in Done: under itself, nothing pushed down",
                &mut board,
                w,
                12,
            );
        }

        // 0.4.0: folding.
        for w in [80, 60] {
            let sheet = format!("fold-{w}");
            let mut board = mixed();
            shoot(
                &sheet,
                "open, as ever: z fold in the footer",
                &mut board,
                w,
                18,
            );
            press(&mut board, "z");
            shoot(
                &sheet,
                "z: 1 folded, its date its open sub item's (nov 12)",
                &mut board,
                w,
                18,
            );
            press(&mut board, "j");
            shoot(&sheet, "j: past its sub items", &mut board, w, 18);
            press(&mut board, "k");
            press(&mut board, "z");
            press(&mut board, "jj ");
            press(&mut board, "k");
            shoot(
                &sheet,
                "z again, open; 11 marked, picked: z fold on a sub item",
                &mut board,
                w,
                18,
            );
            press(&mut board, "z");
            shoot(
                &sheet,
                "z on a sub item: its card folds, its mark cleared",
                &mut board,
                w,
                18,
            );

            let mut board = mixed();
            board.tasks[11].due = Some("2026-11-09".parse().unwrap());
            press(&mut board, "z");
            shoot(
                &sheet,
                "a late sub item inside: the count red, the card its date",
                &mut board,
                w,
                18,
            );
            press(&mut board, "j");
            shoot(&sheet, "the same, not picked", &mut board, w, 18);
            press(&mut board, "k");
            board.tasks.push(sub(
                task(14, "print the charts", Status::Todo, None, &[]),
                1,
            ));
            shoot(
                &sheet,
                "another terminal's halil sub 1: the count, folded still",
                &mut board,
                w,
                18,
            );
            go_to(&mut board, 2);
            shoot(&sheet, "in Done: no z", &mut board, w, 18);

            let mut board = mixed();
            board.tasks.push(sub(
                due(
                    task(14, "book the photo", Status::Todo, None, &[]),
                    "2026-11-12",
                ),
                5,
            ));
            board.pick(5);
            shoot(
                &sheet,
                "5 (in 7 weeks) picked, its sub item due tomorrow",
                &mut board,
                w,
                18,
            );
            press(&mut board, "z");
            shoot(
                &sheet,
                "z: it moves up, and the row above says why",
                &mut board,
                w,
                18,
            );
            press(&mut board, "z");
            shoot(&sheet, "z again: back, and why", &mut board, w, 18);
        }

        // The calendar: 46×10 first, the narrowest, a week; then months.
        let mut board = cal_sample();
        press(&mut board, "c");
        shoot(
            "cal-46",
            "c: the week of today, nothing picked",
            &mut board,
            46,
            10,
        );
        press(&mut board, "j");
        shoot("cal-46", "j: the day's first task", &mut board, 46, 10);
        board.key(KeyEvent::from(KeyCode::Tab));
        shoot("cal-46", "tab: what has no date", &mut board, 46, 10);
        let mut board = cal_sample();
        press(&mut board, "c");
        shoot("cal-46", "the month", &mut board, 46, 26);
        for (w, h) in [(80, 26), (120, 20), (120, 26)] {
            let sheet = format!("cal-{w}");
            let mut board = cal_sample();
            press(&mut board, "c");
            shoot(&sheet, "c: today, late days red", &mut board, w, h);
            press(&mut board, "j");
            shoot(&sheet, "j: the day's first task", &mut board, w, h);
            run_keys(&mut board, "d");
            shoot(
                &sheet,
                "d: done, still listed under Due, faint",
                &mut board,
                w,
                h,
            );
            press(&mut board, "j");
            shoot(
                &sheet,
                "j: the pick moved, the faint row gone",
                &mut board,
                w,
                h,
            );
            board.key(KeyEvent::from(KeyCode::Tab));
            shoot(
                &sheet,
                "tab: what has no date, both groups",
                &mut board,
                w,
                h,
            );
            board.key(KeyEvent::from(KeyCode::Tab));
            board.key(KeyEvent::from(KeyCode::Right));
            board.key(KeyEvent::from(KeyCode::Right));
            shoot(&sheet, "tab, → →: Friday", &mut board, w, h);
            board.key(KeyEvent::from(KeyCode::PageDown));
            shoot(&sheet, "pgdn: December", &mut board, w, h);
            press(&mut board, "a");
            shoot(&sheet, "a: the day filled in", &mut board, w, h);
        }

        // Repeating: a card whole with its step, one cut with `*` and its
        // overlay; done, and the note; the calendar's day.
        let repeating = || {
            let mut board = Board::new(
                vec![
                    task(1, "water", Status::Todo, None, &[]),
                    task(
                        2,
                        "pay the rent for november and keep the receipt",
                        Status::Todo,
                        Some(Priority::High),
                        &["home"],
                    ),
                    task(3, "fix bike", Status::Doing, Some(Priority::Low), &[]),
                    task(4, "call mum", Status::Done, None, &[]),
                ],
                today(),
            );
            board.tasks[0].due = Some("2026-11-11".parse().unwrap());
            board.tasks[0].every = Some("week".parse().unwrap());
            board.tasks[1].due = Some("2026-11-11T18:00".parse().unwrap());
            board.tasks[1].every = Some("month on the 11th".parse().unwrap());
            board.tz = TimeZone::fixed(jiff::tz::offset(9));
            board
        };
        for w in [46, 80, 100] {
            let sheet = format!("repeat-{w}");
            let mut board = repeating();
            board.key(KeyEvent::from(KeyCode::Tab));
            shoot(
                &sheet,
                "a repeating card whole, one cut: today*",
                &mut board,
                w,
                12,
            );
            board.key(KeyEvent::from(KeyCode::BackTab));
            while picked_id(&board) != Some(2) {
                press(&mut board, "j");
            }
            shoot(
                &sheet,
                "the cut one picked: its overlay spells the step",
                &mut board,
                w,
                12,
            );
            run_keys(&mut board, "d");
            shoot(&sheet, "d: done, and its next", &mut board, w, 12);
            press(&mut board, "e");
            shoot(
                &sheet,
                "e on the next: the repeat in the line",
                &mut board,
                w,
                12,
            );
            board.key(KeyEvent::from(KeyCode::Esc));
            press(&mut board, "c");
            shoot(
                &sheet,
                "c: today's list, the steps after the dates",
                &mut board,
                w,
                14,
            );
        }

        // The filter: typing, kept, a change it takes out, nothing matching,
        // the calendar under it.
        for w in [46, 80] {
            let sheet = format!("filter-{w}");
            let mut board = sub_sample();
            press(&mut board, "/");
            for c in "+home".chars() {
                board.key(KeyEvent::from(KeyCode::Char(c)));
            }
            shoot(
                &sheet,
                "/ +home: narrowing as it's typed",
                &mut board,
                w,
                12,
            );
            board.key(KeyEvent::from(KeyCode::Enter));
            shoot(&sheet, "enter: kept, always in sight", &mut board, w, 12);
            press(&mut board, "e");
            let input = board.input.as_mut().unwrap();
            input.text = "pay rent -home".into();
            if let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Enter)) {
                board.apply(op);
            }
            shoot(
                &sheet,
                "e: -home, faint until the filter changes",
                &mut board,
                w,
                12,
            );
            press(&mut board, "/");
            for _ in 0..5 {
                board.key(KeyEvent::from(KeyCode::Backspace));
            }
            for c in "zzz".chars() {
                board.key(KeyEvent::from(KeyCode::Char(c)));
            }
            board.key(KeyEvent::from(KeyCode::Enter));
            shoot(&sheet, "/ zzz: nothing matches", &mut board, w, 12);
            let mut board = cal_sample();
            press(&mut board, "/");
            for c in "+home".chars() {
                board.key(KeyEvent::from(KeyCode::Char(c)));
            }
            board.key(KeyEvent::from(KeyCode::Enter));
            press(&mut board, "c");
            shoot(
                &sheet,
                "c: the calendar under +home, its counts of it",
                &mut board,
                w,
                26,
            );
        }

        // The design session's five changes after 0.5.0's sheets.
        for w in [46, 80, 100] {
            let sheet = format!("changes-{w}");
            let mut board = sub_sample();
            shoot(&sheet, "the footer, no filter", &mut board, w, 12);
            press(&mut board, "/");
            for c in "+home".chars() {
                board.key(KeyEvent::from(KeyCode::Char(c)));
            }
            board.key(KeyEvent::from(KeyCode::Enter));
            shoot(&sheet, "the footer, +home kept", &mut board, w, 12);
            press(&mut board, "e");
            let input = board.input.as_mut().unwrap();
            input.text = "pay rent -home".into();
            if let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Enter)) {
                board.apply(op);
            }
            shoot(&sheet, "e: -home, the faint note", &mut board, w, 12);
            let mut board = weekly_sample();
            press(&mut board, "e");
            let input = board.input.as_mut().unwrap();
            input.text = "pay rent //2026-11-14".into();
            if let Outcome::Change(op) = board.key(KeyEvent::from(KeyCode::Enter)) {
                board.apply(op);
            }
            shoot(
                &sheet,
                "e with every week taken out: still said",
                &mut board,
                w,
                12,
            );
            let mut board = one("pay the rent for november", "2026-12-11 18:00");
            press(&mut board, "e");
            shoot(
                &sheet,
                "e on a long title: the hint gives way",
                &mut board,
                w,
                12,
            );
            let mut board = sample();
            press(&mut board, "a");
            let input = board.input.as_mut().unwrap();
            input.text = "x //nov 28 every fri".into();
            input.cursor = input.text.len();
            input.cal = Some(Cal { at: 2 });
            shoot(
                &sheet,
                "the popup on //nov 28 every fri: friday the 4th",
                &mut board,
                w,
                14,
            );
            board.key(KeyEvent::from(KeyCode::Right));
            shoot(&sheet, "→: the 5th, every sat", &mut board, w, 14);
        }

        for (name, (p, a)) in sheets {
            fs::write(dir.join(format!("{name}.txt")), p).unwrap();
            fs::write(dir.join(format!("{name}.ansi")), a).unwrap();
        }
    }
}
