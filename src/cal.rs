//! The calendar view's days: what each counts and lists, and how a day's
//! cell says it in the room it has. Drawn by the board (see `board.rs`).
//!
//! A task is due on its due date, done on the day its done time names, and
//! added on the day its added time names: each the day written, as the clock
//! read where it happened, never converted to the zone halil runs in now.
//! Only a task made before 0.5.0, with no added time, has its added day read
//! from its stamp, in the zone halil runs in now.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use unicode_width::UnicodeWidthStr;

use crate::task::{Status, Task};

/// Stamps below this are no time: the hash a line from before 0.3.0 gets
/// (see [`crate::task::stamp_unstamped`]). As milliseconds it would be in
/// January or February 1970.
pub const HASHED: u64 = 1 << 32;

/// How done is marked where there's no room for the word: the board's own
/// mark for a done sub item. In every font the board is drawn in.
pub const DONE_MARK: &str = "●";

/// The narrowest and shortest screen the calendar is drawn on: the board's
/// narrowest, and room for a week, the date-not-known row and three rows of a
/// day's list.
pub const MIN_W: usize = 46;
pub const MIN_H: usize = 10;

/// The margin left of the grid, and the space between its cells.
const MARGIN: usize = 2;
const GAP: usize = 1;

/// The day `task` was done on, where it was done: a done task with its time.
pub fn done_day(task: &Task) -> Option<Date> {
    match task.status {
        Status::Done => task.done.as_ref().map(|d| d.date()),
        _ => None,
    }
}

/// The day `task` was added on: the day its added time names, else its stamp's
/// day in `tz`, the zone halil runs in now; none for a stamp that's no time.
pub fn added_day(task: &Task, tz: &TimeZone) -> Option<Date> {
    if let Some(added) = &task.added {
        return Some(added.date());
    }
    if task.stamp < HASHED {
        return None;
    }
    let at = Timestamp::from_millisecond(i64::try_from(task.stamp).ok()?).ok()?;
    Some(at.to_zoned(tz.clone()).date())
}

/// Done before 0.5.0: done, with no time kept.
pub fn done_not_known(task: &Task) -> bool {
    task.status == Status::Done && task.done.is_none()
}

/// Added before 0.3.0: no added time, and a stamp that's no time.
pub fn added_not_known(task: &Task) -> bool {
    task.added.is_none() && task.stamp < HASHED
}

/// What a task is listed under on a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Due,
    Done,
    Added,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::Due => "Due",
            Group::Done => "Done",
            Group::Added => "Added",
        }
    }
}

/// The tasks on `day`, as indices into `tasks`, by group: due that day and
/// still open, as the board sorts them (the day alone, then by time); done that day; added that day. A task in
/// two groups is in each. Within Done and Added, as the board's Done: highest
/// priority first, then oldest.
pub fn listed(tasks: &[Task], day: Date, tz: &TimeZone) -> Vec<(Group, usize)> {
    let mut due: Vec<usize> = (0..tasks.len())
        .filter(|&i| tasks[i].status != Status::Done)
        .filter(|&i| tasks[i].due.is_some_and(|d| d.date == day))
        .collect();
    due.sort_by_key(|&i| {
        let t = &tasks[i];
        (t.due.and_then(|d| d.time), Reverse(t.priority), t.id)
    });
    let by_priority = |list: &mut Vec<usize>| {
        list.sort_by_key(|&i| (Reverse(tasks[i].priority), tasks[i].id));
    };
    let mut done: Vec<usize> = (0..tasks.len())
        .filter(|&i| done_day(&tasks[i]) == Some(day))
        .collect();
    by_priority(&mut done);
    let mut added: Vec<usize> = (0..tasks.len())
        .filter(|&i| added_day(&tasks[i], tz) == Some(day))
        .collect();
    by_priority(&mut added);
    grouped([
        (Group::Due, due),
        (Group::Done, done),
        (Group::Added, added),
    ])
}

/// The tasks on no day, as [`listed`] gives a day's: done before 0.5.0, the
/// newest first; added before 0.3.0, by id (their stamps are no time).
pub fn not_known(tasks: &[Task]) -> Vec<(Group, usize)> {
    let mut done: Vec<usize> = (0..tasks.len())
        .filter(|&i| done_not_known(&tasks[i]))
        .collect();
    done.sort_by_key(|&i| (Reverse(tasks[i].stamp), tasks[i].id));
    let mut added: Vec<usize> = (0..tasks.len())
        .filter(|&i| added_not_known(&tasks[i]))
        .collect();
    added.sort_by_key(|&i| tasks[i].id);
    grouped([(Group::Done, done), (Group::Added, added)])
}

fn grouped<const N: usize>(groups: [(Group, Vec<usize>); N]) -> Vec<(Group, usize)> {
    groups
        .into_iter()
        .flat_map(|(g, list)| list.into_iter().map(move |i| (g, i)))
        .collect()
}

/// `Date not known: done 14 · added 3`, or one of them, or none when there's
/// nothing on no day.
pub fn not_known_line(tasks: &[Task]) -> Option<String> {
    let done = tasks.iter().filter(|t| done_not_known(t)).count();
    let added = tasks.iter().filter(|t| added_not_known(t)).count();
    let parts: Vec<String> = [("done", done), ("added", added)]
        .into_iter()
        .filter(|&(_, n)| n > 0)
        .map(|(what, n)| format!("{what} {n}"))
        .collect();
    (!parts.is_empty()).then(|| format!("Date not known: {}", parts.join(" · ")))
}

/// What a day's cell counts: its tasks due and still open, and those done on
/// it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub due: usize,
    pub done: usize,
}

/// Each day's counts, for the days with any.
pub fn counts(tasks: &[Task]) -> BTreeMap<Date, Counts> {
    let mut days: BTreeMap<Date, Counts> = BTreeMap::new();
    for task in tasks {
        if task.status != Status::Done
            && let Some(due) = task.due
        {
            days.entry(due.date).or_default().due += 1;
        }
        if let Some(day) = done_day(task) {
            days.entry(day).or_default().done += 1;
        }
    }
    days
}

/// The six weeks shown for the month of `day`, Sunday first, as the `//`
/// calendar shows them: the month's days, and none for the rest.
pub fn weeks(day: Date) -> [[Option<Date>; 7]; 6] {
    let first = day.first_of_month();
    let lead = i64::from(first.weekday().to_sunday_zero_offset());
    let mut weeks = [[None; 7]; 6];
    for (w, week) in weeks.iter_mut().enumerate() {
        for (d, cell) in week.iter_mut().enumerate() {
            let n = (w * 7 + d) as i64 - lead + 1;
            if (1..=i64::from(first.days_in_month())).contains(&n) {
                *cell = first.with().day(n as i8).build().ok();
            }
        }
    }
    weeks
}

/// The week of `day`, Sunday first.
pub fn week_of(day: Date) -> [Date; 7] {
    let back = i64::from(day.weekday().to_sunday_zero_offset());
    let sunday = day.checked_sub(jiff::Span::new().days(back)).unwrap_or(day);
    std::array::from_fn(|d| {
        sunday
            .checked_add(jiff::Span::new().days(d as i64))
            .unwrap_or(sunday)
    })
}

/// How a cell says its counts, the first that fits every cell shown, so the
/// cells keep one shape: `due 3 · done 12` on a line; `due 3` and `done 12`
/// on two; `3` and `●12` on two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Words,
    Lines,
    Marks,
}

impl Form {
    /// The lines a cell's counts take, under its day.
    pub fn lines(self) -> usize {
        match self {
            Form::Words => 1,
            Form::Lines | Form::Marks => 2,
        }
    }
}

/// What a piece of a cell's line is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Due,
    Done,
    Between,
}

/// A cell's count lines in `form`, `cw` wide at most: each a few pieces, a
/// count of 0 left out. A number is never cut: one too wide for the cell is
/// said as the most 9s that fit and a `+` (`9999+`, `●999+`).
pub fn say(c: Counts, form: Form, cw: usize) -> Vec<Vec<(Part, String)>> {
    let word = |part: Part, what: &str, n: usize| match n {
        0 => Vec::new(),
        n => vec![(part, format!("{what} {n}"))],
    };
    match form {
        Form::Words => {
            let mut line = word(Part::Due, "due", c.due);
            let done = word(Part::Done, "done", c.done);
            if !line.is_empty() && !done.is_empty() {
                line.push((Part::Between, " · ".into()));
            }
            line.extend(done);
            vec![line]
        }
        Form::Lines => vec![
            word(Part::Due, "due", c.due),
            word(Part::Done, "done", c.done),
        ],
        Form::Marks => {
            let mark = |part: Part, mark: &str, n: usize| match n {
                0 => Vec::new(),
                n => vec![(part, capped(mark, n, cw))],
            };
            vec![
                mark(Part::Due, "", c.due),
                mark(Part::Done, DONE_MARK, c.done),
            ]
        }
    }
}

/// `mark` and `n`, or when that's wider than `room`, `mark`, the most 9s that
/// fit, and a `+`: never a number cut, never a wrong one said whole.
fn capped(mark: &str, n: usize, room: usize) -> String {
    let whole = format!("{mark}{n}");
    if whole.width() <= room {
        return whole;
    }
    let nines = room.saturating_sub(mark.width() + 1);
    format!("{mark}{}+", "9".repeat(nines))
}

/// A line of [`say`] as text.
pub fn plain(line: &[(Part, String)]) -> String {
    line.iter().map(|(_, s)| s.as_str()).collect()
}

/// The form for these cells, `cw` wide: the first whose every line fits.
pub fn form(cells: &[Counts], cw: usize) -> Form {
    let fits = |form: Form| {
        cells
            .iter()
            .all(|&c| say(c, form, cw).iter().all(|l| plain(l).width() <= cw))
    };
    [Form::Words, Form::Lines]
        .into_iter()
        .find(|&f| fits(f))
        .unwrap_or(Form::Marks)
}

/// How wide each of the seven cells is on a screen `w` wide.
pub fn cell_width(w: usize) -> usize {
    w.saturating_sub(2 * MARGIN + 6 * GAP) / 7
}

/// Where cell `d` (0 is Sunday) starts, the cells `cw` wide.
pub fn cell_x(d: usize, cw: usize) -> usize {
    MARGIN + d * (cw + GAP)
}

/// What the calendar shows on a screen `w` by `h`: the month when it fits,
/// else the picked day's week, else nothing (it asks for a larger window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Month(Form),
    Week(Form),
}

impl Shape {
    pub fn form(self) -> Form {
        match self {
            Shape::Month(f) | Shape::Week(f) => f,
        }
    }
}

/// The rows a month takes besides its weeks, and a week besides its day line
/// and counts: the heading, the weekday line (a month's), the date-not-known
/// row, three of the day's list, the row above the footer and the footer.
const AROUND: usize = 8;

/// The shape for a screen `w` by `h`, the month's cells and the week's: a
/// month takes 20 rows when its counts take a line, 26 when they take two.
pub fn shape(month: &[Counts], week: &[Counts], w: usize, h: usize) -> Option<Shape> {
    if w < MIN_W || h < MIN_H {
        return None;
    }
    let cw = cell_width(w);
    let form_m = form(month, cw);
    if h >= AROUND + 6 * (1 + form_m.lines()) {
        return Some(Shape::Month(form_m));
    }
    Some(Shape::Week(form(week, cw)))
}

/// The heading's pieces, each with where it starts: `  November 2026`, and
/// in the marks form, the legend at the grid's right, `due n · ● done n`,
/// when it fits after two spaces. The bool says which is the legend.
pub fn heading(day: Date, form: Form, cw: usize) -> Vec<(usize, String, bool)> {
    let title = day.strftime("%B %Y").to_string();
    let mut pieces = vec![(MARGIN, title.clone(), false)];
    if form == Form::Marks {
        let legend = format!("due n · {DONE_MARK} done n");
        let end = cell_x(6, cw) + cw;
        let x = end.saturating_sub(legend.width());
        if x >= MARGIN + title.width() + 2 {
            pieces.push((x, legend, true));
        }
    }
    pieces
}

/// A day's head in the week: its weekday and day, `We 25`, at the cell's
/// right.
pub fn strip_head(day: Date, cw: usize) -> String {
    let wd = day.strftime("%a").to_string();
    let head = format!("{} {:>2}", &wd[..2], day.day());
    format!("{head:>cw$}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::Local;
    use jiff::civil::date;
    use jiff::tz::offset;

    fn task(id: u32, status: Status) -> Task {
        Task {
            id,
            title: format!("task {id}"),
            status,
            priority: None,
            tags: vec![],
            due: None,
            parent: None,
            stamp: 0,
            done: None,
            added: None,
            every: None,
            next: None,
        }
    }

    fn at(s: &str) -> Local {
        s.parse().unwrap()
    }

    /// The line a heading's pieces make.
    fn heading_line(day: Date, form: Form, cw: usize) -> String {
        let mut line = String::new();
        for (x, text, _) in heading(day, form, cw) {
            while line.width() < x {
                line.push(' ');
            }
            line += &text;
        }
        line
    }

    #[test]
    fn the_form_ladder_by_width_and_counts() {
        let c = |due, done| Counts { due, done };
        let quiet = [c(3, 0), c(0, 2), c(1, 1)];
        // `due 12 · done 100` is 17 cells; `done 100` 8.
        let busy = [c(12, 100), c(3, 0)];
        for (w, cw, quiet_form, busy_form) in [
            (46, 5, Form::Marks, Form::Marks),
            (59, 7, Form::Lines, Form::Marks),
            (80, 10, Form::Lines, Form::Lines),
            (115, 15, Form::Words, Form::Lines),
            (120, 15, Form::Words, Form::Lines),
            (126, 16, Form::Words, Form::Lines),
            (133, 17, Form::Words, Form::Words),
            (240, 32, Form::Words, Form::Words),
        ] {
            assert_eq!(cell_width(w), cw, "{w}");
            assert_eq!(form(&quiet, cw), quiet_form, "{w}, quiet");
            assert_eq!(form(&busy, cw), busy_form, "{w}, busy");
        }
        assert_eq!(
            say(c(3, 12), Form::Words, 15)
                .iter()
                .map(|l| plain(l))
                .collect::<Vec<_>>(),
            ["due 3 · done 12"]
        );
        assert_eq!(
            say(c(3, 12), Form::Lines, 10)
                .iter()
                .map(|l| plain(l))
                .collect::<Vec<_>>(),
            ["due 3", "done 12"]
        );
        assert_eq!(
            say(c(3, 12), Form::Marks, 5)
                .iter()
                .map(|l| plain(l))
                .collect::<Vec<_>>(),
            ["3", "●12"]
        );
        // A 0 is blank, in every form.
        for form in [Form::Words, Form::Lines, Form::Marks] {
            assert!(
                say(c(0, 0), form, 5).iter().all(|l| l.is_empty()),
                "{form:?}"
            );
            assert_eq!(
                plain(&say(c(0, 4), form, 9)[form.lines() - 1]).trim(),
                match form {
                    Form::Marks => "●4",
                    _ => "done 4",
                }
            );
        }
    }

    #[test]
    fn at_46_the_heading_and_its_legend_fit_whole() {
        // The row nearest the edge, at the narrowest: every month's name.
        assert_eq!(cell_width(46), 5);
        assert_eq!(
            heading_line(date(2026, 11, 25), Form::Marks, 5),
            "  November 2026            due n · ● done n"
        );
        for m in 1..=12 {
            let line = heading_line(date(2026, m, 1), Form::Marks, 5);
            assert!(line.width() <= 46, "{line}");
            assert!(line.ends_with("due n · ● done n"), "{line}");
            let title = date(2026, m, 1).strftime("%B %Y").to_string();
            assert!(line.starts_with(&format!("  {title}  ")), "{line}");
        }
        // In the forms that say their words, no legend.
        assert_eq!(
            heading_line(date(2026, 11, 25), Form::Lines, 10),
            "  November 2026"
        );
    }

    #[test]
    fn at_46_the_weeks_heads_fit_whole() {
        // The other row nearest the edge: seven `We 25`, 5 cells each, one
        // space between, inside 46 with the margin.
        let week = week_of(date(2026, 11, 25));
        let mut line = String::new();
        for (d, day) in week.iter().enumerate() {
            while line.width() < cell_x(d, 5) {
                line.push(' ');
            }
            line += &strip_head(*day, 5);
        }
        assert_eq!(line, "  Su 22 Mo 23 Tu 24 We 25 Th 26 Fr 27 Sa 28");
        assert!(line.width() <= 46);
        // A week across two months, single-digit days right-aligned.
        let week = week_of(date(2026, 12, 1));
        let heads: Vec<String> = week.iter().map(|&d| strip_head(d, 5)).collect();
        assert_eq!(
            heads,
            [
                "Su 29", "Mo 30", "Tu  1", "We  2", "Th  3", "Fr  4", "Sa  5"
            ]
        );
        assert!(heads.iter().all(|h| h.width() == 5));
    }

    #[test]
    fn a_number_is_never_cut_at_any_width_or_count() {
        let mut counts: Vec<usize> = (0..=120).collect();
        for p in 1..=5 {
            let ten = 10usize.pow(p);
            counts.extend([ten - 1, ten, ten + 1]);
        }
        for w in 46..=240 {
            let cw = cell_width(w);
            for &n in &counts {
                for c in [
                    Counts { due: n, done: 0 },
                    Counts { due: 0, done: n },
                    Counts { due: n, done: n },
                    Counts { due: n, done: 7 },
                    Counts { due: 3, done: n },
                ] {
                    let form = form(&[c, Counts { due: 1, done: 1 }], cw);
                    for line in say(c, form, cw) {
                        let text = plain(&line);
                        assert!(text.width() <= cw, "{w}: {c:?} {text}");
                        let chars: Vec<char> = text.chars().collect();
                        let mut i = 0;
                        while i < chars.len() {
                            if !chars[i].is_ascii_digit() {
                                i += 1;
                                continue;
                            }
                            let start = i;
                            while i < chars.len() && chars[i].is_ascii_digit() {
                                i += 1;
                            }
                            let run: String = chars[start..i].iter().collect();
                            let shown: usize = run.parse().unwrap();
                            let whole = shown == c.due || shown == c.done;
                            let capped = chars.get(i) == Some(&'+')
                                && shown < c.due.max(c.done)
                                && run.chars().all(|d| d == '9');
                            assert!(whole || capped, "{w}: {c:?} says {text}");
                        }
                    }
                }
            }
        }
        assert_eq!(capped("", 123_456, 5), "9999+");
        assert_eq!(capped(DONE_MARK, 12_345, 5), "●999+");
        assert_eq!(capped(DONE_MARK, 9_999, 5), "●9999");
    }

    #[test]
    fn the_shape_is_the_month_when_it_fits_else_the_week_else_none() {
        let quiet = [Counts { due: 1, done: 0 }];
        // Forms with a line of counts: a month at 20 rows.
        assert_eq!(
            shape(&quiet, &quiet, 120, 20),
            Some(Shape::Month(Form::Words))
        );
        assert_eq!(
            shape(&quiet, &quiet, 120, 19),
            Some(Shape::Week(Form::Words))
        );
        // Two lines: 26. (`due 3 · done 2`, and `done 2`, don't fit 5.)
        let both = [Counts { due: 3, done: 2 }];
        assert_eq!(shape(&both, &both, 46, 26), Some(Shape::Month(Form::Marks)));
        assert_eq!(shape(&both, &both, 46, 25), Some(Shape::Week(Form::Marks)));
        assert_eq!(shape(&both, &both, 46, 10), Some(Shape::Week(Form::Marks)));
        assert_eq!(shape(&both, &both, 46, 9), None);
        assert_eq!(shape(&both, &both, 45, 30), None);
        // The week's own form: a quiet week in a busy month says words.
        assert_eq!(shape(&both, &quiet, 46, 12), Some(Shape::Week(Form::Words)));
    }

    #[test]
    fn due_counts_open_tasks_and_done_counts_on_the_day_it_was_done() {
        let wed = date(2026, 11, 25);
        let mut open = task(1, Status::Todo);
        open.due = Some("2026-11-25".parse().unwrap());
        let mut doing = task(2, Status::Doing);
        doing.due = Some("2026-11-25T18:00".parse().unwrap());
        // Due Wednesday, done Monday: counted on Monday, as done, only.
        let mut done = task(3, Status::Done);
        done.due = Some("2026-11-25".parse().unwrap());
        done.done = Some(at("2026-11-23T09:00:00+09:00[Asia/Seoul]"));
        // Done before 0.5.0: on no day.
        let mut unknown = task(4, Status::Done);
        unknown.due = Some("2026-11-25".parse().unwrap());
        // A sub item counts as one, as a card does.
        let mut sub = task(5, Status::Todo);
        sub.due = Some("2026-11-25".parse().unwrap());
        sub.parent = Some(1);
        let tasks = [open, doing, done, unknown, sub];
        let days = counts(&tasks);
        assert_eq!(days[&wed], Counts { due: 3, done: 0 });
        assert_eq!(days[&date(2026, 11, 23)], Counts { due: 0, done: 1 });
        assert_eq!(days.len(), 2);

        let utc = TimeZone::UTC;
        assert_eq!(
            listed(&tasks, wed, &utc),
            [(Group::Due, 0), (Group::Due, 4), (Group::Due, 1)],
            "untimed, then by time; by id"
        );
        assert_eq!(listed(&tasks, date(2026, 11, 23), &utc), [(Group::Done, 2)]);
        assert_eq!(
            not_known(&tasks),
            [
                (Group::Done, 3),
                (Group::Added, 0),
                (Group::Added, 1),
                (Group::Added, 2),
                (Group::Added, 3),
                (Group::Added, 4)
            ]
        );
    }

    #[test]
    fn added_is_its_added_day_else_its_stamps_else_not_known() {
        let mut made = task(1, Status::Todo);
        made.added = Some(at("2026-11-25T23:30:00-05:00[America/New_York]"));
        made.stamp = 1_795_667_400_000; // 2026-11-26T04:30Z: the 26th in UTC
        let mut stamped = task(2, Status::Todo);
        stamped.stamp = 1_795_667_400_000;
        let hashed = Task {
            stamp: 0x9e37_79b9,
            ..task(3, Status::Done)
        };
        let seoul = TimeZone::fixed(offset(9));
        let utc = TimeZone::UTC;
        assert_eq!(
            added_day(&made, &seoul),
            Some(date(2026, 11, 25)),
            "as written"
        );
        assert_eq!(
            added_day(&made, &utc),
            Some(date(2026, 11, 25)),
            "as written"
        );
        assert_eq!(added_day(&stamped, &utc), Some(date(2026, 11, 26)));
        assert_eq!(added_day(&stamped, &seoul), Some(date(2026, 11, 26)));
        assert_eq!(added_day(&hashed, &utc), None);
        assert!(added_not_known(&hashed) && !added_not_known(&stamped));
        let tasks = [made, stamped, hashed];
        assert_eq!(
            not_known_line(&tasks).as_deref(),
            Some("Date not known: done 1 · added 1")
        );
        assert_eq!(
            not_known(&tasks),
            [(Group::Done, 2), (Group::Added, 2)],
            "in each"
        );
        assert_eq!(not_known_line(&tasks[..2]), None);
        assert_eq!(
            listed(&tasks, date(2026, 11, 25), &utc),
            [(Group::Added, 0)]
        );
    }

    #[test]
    fn the_months_six_weeks_and_a_days_week() {
        let weeks = weeks(date(2026, 11, 25));
        // November 2026 starts on a Sunday.
        assert_eq!(weeks[0][0], Some(date(2026, 11, 1)));
        assert_eq!(weeks[4][1], Some(date(2026, 11, 30)));
        assert_eq!(weeks[4][2], None);
        assert!(weeks[5].iter().all(Option::is_none));
        let days: usize = weeks.iter().flatten().flatten().count();
        assert_eq!(days, 30);
        assert_eq!(week_of(date(2026, 11, 25))[0], date(2026, 11, 22));
        assert_eq!(week_of(date(2026, 11, 22))[0], date(2026, 11, 22));
        assert_eq!(week_of(date(2026, 11, 28))[6], date(2026, 11, 28));
    }
}
