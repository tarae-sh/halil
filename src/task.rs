use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use jiff::Zoned;

use crate::date::{Due, Every, Local};

/// One task: a line of the tasks file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<Due>,
    /// The task this is a sub item of (see [`parents`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// Which task this is, for good: set when it's made and never changed, so
    /// a task that takes a removed task's id is never taken for it (see
    /// [`new_stamp`]). 0 is none yet: a line written before 0.3.0, until
    /// [`stamp_unstamped`] gives it one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stamp: u64,
    /// When it went to Done, as the clock read where it was (see
    /// [`set_status`]). None on a Done task: done before 0.5.0, the day not
    /// known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<Local>,
    /// When it was made, as the clock read where it was: from 0.5.0 on. None:
    /// made before, its day read from the stamp when that's a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<Local>,
    /// How it repeats (see [`roll_repeats`]): `week`, `2 weeks`, `month on
    /// the 31st`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<Every>,
    /// On a done task that repeated: the stamp of the task its repeat went on
    /// to, for the note when it's opened again. Gone once it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Todo,
    Doing,
    Done,
}

/// Ordered low to high, so the highest sorts last (`Reverse` it to list it first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    Med,
    High,
}

impl FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "high" | "h" => Ok(Priority::High),
            "med" | "medium" | "m" => Ok(Priority::Med),
            "low" | "l" => Ok(Priority::Low),
            _ => Err(format!("`!{s}` isn't a priority: use !high, !med or !low")),
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad(match self {
            Priority::High => "high",
            Priority::Med => "med",
            Priority::Low => "low",
        })
    }
}

impl fmt::Display for Task {
    /// `3: pay rent +home !high due Fri 2026-11-13 09:00`
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.id, self.title)?;
        for tag in &self.tags {
            write!(f, " +{tag}")?;
        }
        if let Some(p) = self.priority {
            write!(f, " !{p}")?;
        }
        if let Some(due) = self.due {
            write!(f, " due {} {due}", due.date.strftime("%a"))?;
            if let Some(every) = self.every {
                write!(f, " {}", every.words(due.date))?;
            }
        }
        Ok(())
    }
}

/// The id a new task gets: the smallest number from 1 up that no task has, nor
/// names as its parent — so a sub item whose parent is gone never finds itself
/// under a new task.
pub fn next_id(tasks: &[Task]) -> u32 {
    let used: HashSet<u32> = tasks
        .iter()
        .flat_map(|t| std::iter::once(t.id).chain(t.parent))
        .collect();
    (1..)
        .find(|id| !used.contains(id))
        .expect("fewer than u32::MAX tasks")
}

/// The stamp a new task gets: the time, in milliseconds since 1970, or one
/// past the largest stamp in `tasks` if that's later. No two tasks in a file
/// share one: every change is made to the file as it was read, so each file is
/// one halil's tasks, and each new stamp is past all of theirs. Milliseconds,
/// not finer, so the number stays exact in JSON readers that hold numbers as
/// doubles.
pub fn new_stamp(tasks: &[Task], now_ms: u64) -> u64 {
    let past = tasks.iter().map(|t| t.stamp).max().unwrap_or(0);
    now_ms.max(past.saturating_add(1))
}

/// Milliseconds since 1970 by the system clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Gives a stamp to each task read from a line that had none (written before
/// 0.3.0, or since by 0.2.0, which drops it): one made from its id and title,
/// so every read of the line, until a save writes it, gives the same stamp. It
/// stays below 2³², under any time [`new_stamp`] gives, and a task that took
/// the id since has another title and so, all but surely, another stamp.
pub fn stamp_unstamped(tasks: &mut [Task]) {
    for task in tasks.iter_mut().filter(|t| t.stamp == 0) {
        // FNV-1a: small, and the same in every build.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in task.id.to_le_bytes().iter().chain(task.title.as_bytes()) {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
        task.stamp = (hash & 0xffff_ffff).max(1);
    }
}

/// Sets `task`'s status, as of `now`: into Done from Todo or Doing, it's done
/// now; already Done, it keeps the time it was done; out of Done, it has none.
/// Every change of status goes through here.
pub fn set_status(task: &mut Task, to: Status, now: &Zoned) {
    match (task.status == Status::Done, to == Status::Done) {
        (false, true) => task.done = Some(Local::of(now)),
        (true, false) => {
            task.done = None;
            task.next = None;
        }
        _ => {}
    }
    task.status = to;
}

/// What [`roll_repeats`] made: the done task's id, the next one's, and its
/// date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rolled {
    pub done: u32,
    pub next: u32,
    pub due: Due,
}

/// For each done task that repeats: the next one, a new open task with the
/// same title, tags, priority and step, due the step's next date after the
/// day it was done (see [`Every::next`]); its sub items with it, all open, a
/// sub item's own date moved as many days as the card's. The done one stays
/// as any done task, with its done time, and no longer repeats. Made `now`.
/// Run after every change, under the file's lock, so a task done once makes
/// one next, however many halil are at it.
pub fn roll_repeats(tasks: &mut Vec<Task>, now: &Zoned) -> Vec<Rolled> {
    let mut rolled = Vec::new();
    let mut i = 0;
    while i < tasks.len() {
        let t = &tasks[i];
        let (Status::Done, Some(every)) = (t.status, t.every) else {
            i += 1;
            continue;
        };
        let done_day = t.done.as_ref().map_or(now.date(), |d| d.date());
        let next = t
            .due
            .and_then(|due| every.next(due, done_day).map(|n| (due, n)));
        tasks[i].every = None;
        let Some((due, next_due)) = next else {
            i += 1;
            continue;
        };
        let shift = due.date.until(next_due.date).map_or(0, |s| s.get_days());
        let card_id = tasks[i].id;
        let subs = sub_items(tasks, &parents(tasks), i);
        let made = |tasks: &[Task], from: &Task, parent: Option<u32>, due: Option<Due>| Task {
            id: next_id(tasks),
            title: from.title.clone(),
            status: Status::Todo,
            priority: from.priority,
            tags: from.tags.clone(),
            due,
            parent,
            stamp: new_stamp(tasks, now_ms()),
            done: None,
            added: Some(Local::of(now)),
            every: None,
            next: None,
        };
        let card = Task {
            every: Some(every),
            ..made(tasks, &tasks[i], None, Some(next_due))
        };
        let (id, stamp) = (card.id, card.stamp);
        tasks.push(card);
        tasks[i].next = Some(stamp);
        for s in subs {
            let from = tasks[s].clone();
            let moved = from.due.map(|d| Due {
                date: d
                    .date
                    .checked_add(jiff::Span::new().days(shift))
                    .unwrap_or(d.date),
                ..d
            });
            let sub = made(tasks, &from, Some(id), moved);
            tasks.push(sub);
        }
        rolled.push(Rolled {
            done: card_id,
            next: id,
            due: next_due,
        });
        i += 1;
    }
    rolled
}

/// `fri dec 4`, `fri dec 4 6pm`, with the year when it isn't `today`'s.
pub fn day_said(due: Due, today: jiff::civil::Date) -> String {
    let mut said = due.date.strftime("%a %b %-d").to_string().to_lowercase();
    if due.date.year() != today.year() {
        said += &format!(" {}", due.date.year());
    }
    if let Some(t) = due.time {
        let hour = match t.hour() % 12 {
            0 => 12,
            h => h,
        };
        let half = if t.hour() < 12 { "am" } else { "pm" };
        said += &match t.minute() {
            0 => format!(" {hour}{half}"),
            m => format!(" {hour}:{m:02}{half}"),
        };
    }
    said
}

/// The note for what [`roll_repeats`] made: `Done. Next: fri dec 4.`, more
/// than one listed.
pub fn next_said(rolled: &[Rolled], today: jiff::civil::Date) -> Option<String> {
    let days: Vec<String> = rolled.iter().map(|r| day_said(r.due, today)).collect();
    (!days.is_empty()).then(|| format!("Done. Next: {}.", days.join(", ")))
}

/// The note for task `i`, done and repeated, opened again: `Reopened. Its
/// repeat is on #12.` Read before it's opened, which forgets `next`.
pub fn reopened_said(tasks: &[Task], i: usize) -> Option<String> {
    let stamp = tasks[i].next.filter(|_| tasks[i].status == Status::Done)?;
    Some(match tasks.iter().find(|t| t.stamp == stamp) {
        Some(next) => format!("Reopened. Its repeat is on #{}.", next.id),
        None => "Reopened. Its repeat is gone.".into(),
    })
}

/// Drops a done time from a task that isn't done (written by hand): it's no
/// time anything was done, and the next save leaves it out.
pub fn drop_stray_done(tasks: &mut [Task]) {
    for task in tasks.iter_mut().filter(|t| t.status != Status::Done) {
        task.done = None;
    }
}

/// Whether `task` is under another task that's there: such a task takes no
/// sub items of its own, since they go one level deep.
pub fn is_sub_item(tasks: &[Task], task: &Task) -> bool {
    task.parent.is_some_and(|p| tasks.iter().any(|t| t.id == p))
}

/// For each task, the index in `tasks` of the card it's a sub item of, or
/// `None` for a card. A task is a sub item when its parent is there and is no
/// sub item itself; any other task is a card, so one whose parent is gone is
/// shown as a card, never lost.
pub fn parents(tasks: &[Task]) -> Vec<Option<usize>> {
    let index: HashMap<u32, usize> = tasks.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
    tasks
        .iter()
        .map(|t| {
            let p = *index.get(&t.parent?)?;
            let grand = tasks[p].parent.is_some_and(|g| index.contains_key(&g));
            (!grand).then_some(p)
        })
        .collect()
}

/// The sub items of the card at index `card`, in id order, as indices into the
/// tasks `parents` was read from.
pub fn sub_items(tasks: &[Task], parents: &[Option<usize>], card: usize) -> Vec<usize> {
    let mut subs: Vec<usize> = (0..parents.len())
        .filter(|&i| parents[i] == Some(card))
        .collect();
    subs.sort_by_key(|&i| tasks[i].id);
    subs
}

/// `2/4`: how many of these sub items are done, of how many; none without any.
pub fn progress(tasks: &[Task], subs: &[usize]) -> Option<String> {
    let done = subs
        .iter()
        .filter(|&&i| tasks[i].status == Status::Done)
        .count();
    (!subs.is_empty()).then(|| format!("{done}/{}", subs.len()))
}

/// `1 sub item`, `3 sub items`: `n` of `noun`.
pub fn count_of(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// `Done, with its 2 open sub items.`: the note when finishing a task (or,
/// `several`, a few) finished `n` open sub items too.
pub fn done_with(n: usize, several: bool) -> String {
    let whose = if several { "their" } else { "its" };
    format!("Done, with {whose} {}.", count_of(n, "open sub item"))
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use jiff::civil::date;
    use jiff::tz::{Offset, TimeZone};

    use super::*;
    use crate::cal::{self, Group};

    #[test]
    fn a_local_time_is_written_with_its_offset_and_zone_and_read_with_no_tz_database() {
        let seoul: Local = "2026-11-25T18:04:00+09:00[Asia/Seoul]".parse().unwrap();
        assert_eq!(seoul.at, date(2026, 11, 25).at(18, 4, 0, 0));
        assert_eq!(seoul.offset, Offset::constant(9));
        assert_eq!(seoul.zone.as_deref(), Some("Asia/Seoul"));
        assert_eq!(seoul.to_string(), "2026-11-25T18:04:00+09:00[Asia/Seoul]");
        // No name: the offset alone, both ways.
        let kathmandu: Local = "2026-11-25T18:04:00+05:45".parse().unwrap();
        assert_eq!(kathmandu.zone, None);
        assert_eq!(kathmandu.to_string(), "2026-11-25T18:04:00+05:45");
        // A zone no tz database here knows is still a name, read as written.
        let far: Local = "2026-11-25T18:04:00+09:00[Mars/Olympus_Mons]"
            .parse()
            .unwrap();
        assert_eq!(far.date(), date(2026, 11, 25));
        // What isn't a time with its offset is refused.
        for bad in ["2026-11-25", "2026-11-25T18:04:00", "friday", ""] {
            assert!(bad.parse::<Local>().is_err(), "{bad}");
        }
        // Kept to the second, with the zone's name when it has one.
        let now = date(2026, 11, 25)
            .at(18, 4, 7, 123_456_789)
            .to_zoned(TimeZone::fixed(Offset::constant(9)))
            .unwrap();
        assert_eq!(Local::of(&now).to_string(), "2026-11-25T18:04:07+09:00");
    }

    #[test]
    fn set_status_sets_the_done_time_into_done_keeps_it_there_and_clears_it_out() {
        let tz = TimeZone::fixed(Offset::constant(9));
        let at = |h| {
            date(2026, 11, 25)
                .at(h, 0, 0, 0)
                .to_zoned(tz.clone())
                .unwrap()
        };
        let mut task = with_ids(&[1]).remove(0);
        set_status(&mut task, Status::Doing, &at(9));
        assert_eq!(task.done, None);
        set_status(&mut task, Status::Done, &at(10));
        assert_eq!(task.done, Some(Local::of(&at(10))));
        set_status(&mut task, Status::Done, &at(11));
        assert_eq!(task.done, Some(Local::of(&at(10))), "done already: kept");
        set_status(&mut task, Status::Todo, &at(12));
        assert_eq!((task.status, task.done), (Status::Todo, None));
        // Done before 0.5.0: done again keeps none, as it has none.
        let mut old = with_ids(&[2]).remove(0);
        old.status = Status::Done;
        set_status(&mut old, Status::Done, &at(13));
        assert_eq!(old.done, None);
        // A done time on an open task, written by hand, goes when it's read.
        let mut stray = with_ids(&[3, 4]);
        stray[0].done = Some(Local::of(&at(9)));
        stray[1].status = Status::Done;
        stray[1].done = Some(Local::of(&at(9)));
        drop_stray_done(&mut stray);
        assert_eq!(
            (stray[0].done.is_some(), stray[1].done.is_some()),
            (false, true)
        );
    }

    /// The zones the property runs in, as POSIX rules: no tz database, and no
    /// name, as a system zone set by `TZ` has none.
    const ZONES: [&str; 7] = [
        "KST-9",                                // Seoul
        "EST5EDT,M3.2.0,M11.1.0",               // New York
        "<+1030>-10:30<+11>-11,M10.1.0,M4.1.0", // Lord Howe: a half hour
        "<+0545>-5:45",                         // Kathmandu
        "<+13>-13",                             // Apia, across the date line
        "UTC0",
        "CET-1CEST,M3.5.0,M10.5.0/3", // Berlin
    ];

    #[test]
    fn a_done_or_added_time_is_on_the_day_it_was_there_read_in_any_zone() {
        let zones: Vec<TimeZone> = ZONES.iter().map(|z| TimeZone::posix(z).unwrap()).collect();
        // Around midnight, and the clock changes of 2026 and 2027.
        let days = [
            date(2026, 3, 8),
            date(2026, 3, 29),
            date(2026, 4, 5),
            date(2026, 10, 4),
            date(2026, 10, 25),
            date(2026, 11, 1),
            date(2026, 12, 31),
            date(2027, 3, 14),
            date(2027, 4, 4),
            date(2027, 10, 3),
            date(2027, 11, 7),
        ];
        let times = [
            (0, 0),
            (0, 30),
            (1, 59),
            (2, 30),
            (3, 0),
            (12, 0),
            (23, 30),
            (23, 59),
        ];
        let mut cases = 0;
        let mut not_utc = 0;
        for day in days {
            for (h, m) in times {
                let instant: Timestamp = day
                    .at(h, m, 0, 0)
                    .to_zoned(TimeZone::UTC)
                    .unwrap()
                    .timestamp();
                for there in &zones {
                    let then = instant.to_zoned(there.clone());
                    let kept: Local = Local::of(&then).to_string().parse().unwrap();
                    let mut task = with_ids(&[1]).remove(0);
                    task.status = Status::Done;
                    task.done = Some(kept.clone());
                    task.added = Some(kept);
                    task.stamp = instant.as_millisecond() as u64;
                    let tasks = [task];
                    if then.date() != day {
                        not_utc += 1;
                    }
                    for here in &zones {
                        let counts = cal::counts(&tasks);
                        assert_eq!(counts.keys().collect::<Vec<_>>(), [&then.date()], "{then}");
                        assert_eq!(
                            cal::listed(&tasks, then.date(), here),
                            [(Group::Done, 0), (Group::Added, 0)],
                            "{then}, read in {here:?}"
                        );
                        assert_eq!(cal::added_day(&tasks[0], here), Some(then.date()));
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 11 * 8 * 7 * 7);
        assert!(
            not_utc > 100,
            "the day there isn't the day in UTC: {not_utc}"
        );
    }

    #[test]
    fn a_repeating_task_done_makes_the_next_one_with_its_sub_items_open() {
        let tz = TimeZone::fixed(Offset::constant(9));
        let now = date(2026, 11, 11).at(10, 0, 0, 0).to_zoned(tz).unwrap();
        let mut tasks = with_ids(&[1, 2, 3, 4]);
        tasks[0].title = "pay rent".into();
        tasks[0].tags = vec!["home".into()];
        tasks[0].priority = Some(Priority::High);
        tasks[0].due = Some("2026-11-13T18:00".parse().unwrap());
        tasks[0].every = Some("week".parse().unwrap());
        // Its sub items: one done, one open with a date, one open without.
        for (i, s) in [(1, Status::Done), (2, Status::Todo), (3, Status::Todo)] {
            tasks[i].parent = Some(1);
            tasks[i].status = s;
        }
        tasks[2].due = Some("2026-11-12".parse().unwrap());
        for (i, t) in tasks.iter_mut().enumerate() {
            t.stamp = 1_763_500_000_000 + i as u64;
        }
        // Nothing done: nothing made.
        assert!(roll_repeats(&mut tasks, &now).is_empty());
        set_status(&mut tasks[0], Status::Done, &now);
        let rolled = roll_repeats(&mut tasks, &now);
        assert_eq!(rolled.len(), 1);
        let next = tasks
            .iter()
            .find(|t| t.id == rolled[0].next)
            .unwrap()
            .clone();
        assert_eq!(
            next.due.unwrap().to_string(),
            "2026-11-20 18:00",
            "a week on, the time kept"
        );
        assert_eq!(
            (next.title.as_str(), &next.tags, next.priority, next.status),
            (
                "pay rent",
                &tasks[0].tags,
                Some(Priority::High),
                Status::Todo
            )
        );
        assert_eq!(
            next.every,
            tasks
                .iter()
                .find(|t| t.id == 1)
                .and(Some("week".parse().unwrap()))
        );
        assert_eq!(next.added, Some(Local::of(&now)));
        // The done one: done, with its time, no longer repeating, pointing on.
        let done = &tasks[0];
        assert_eq!(
            (done.status, done.every, done.next),
            (Status::Done, None, Some(next.stamp))
        );
        assert!(done.done.is_some());
        // Its three sub items again, all open, a date moved a week as the card's.
        let subs: Vec<&Task> = tasks.iter().filter(|t| t.parent == Some(next.id)).collect();
        assert_eq!(subs.len(), 3);
        assert!(
            subs.iter()
                .all(|s| s.status == Status::Todo && s.every.is_none())
        );
        let dated: Vec<String> = subs
            .iter()
            .filter_map(|s| s.due)
            .map(|d| d.to_string())
            .collect();
        assert_eq!(dated, ["2026-11-19"]);
        // Run again: nothing more.
        assert!(roll_repeats(&mut tasks, &now).is_empty());
        assert_eq!(
            next_said(&rolled, now.date()).as_deref(),
            Some("Done. Next: fri nov 20 6pm.")
        );
        // Opened again: where its repeat went, read before it's opened.
        assert_eq!(
            reopened_said(&tasks, 0).as_deref(),
            Some(format!("Reopened. Its repeat is on #{}.", next.id).as_str())
        );
        set_status(&mut tasks[0], Status::Todo, &now);
        assert_eq!(
            (tasks[0].next, tasks[0].every),
            (None, None),
            "a plain task"
        );
        assert_eq!(reopened_said(&tasks, 0), None);
    }

    fn with_ids(ids: &[u32]) -> Vec<Task> {
        ids.iter()
            .map(|&id| Task {
                id,
                title: "x".into(),
                status: Status::Todo,
                priority: None,
                tags: vec![],
                due: None,
                parent: None,
                stamp: 0,
                done: None,
                added: None,
                every: None,
                next: None,
            })
            .collect()
    }

    #[test]
    fn a_new_stamp_is_the_time_or_past_every_stamp_there() {
        let mut tasks = with_ids(&[1, 2]);
        assert_eq!(new_stamp(&tasks, 1_000), 1_000);
        tasks[0].stamp = 1_000;
        assert_eq!(new_stamp(&tasks, 1_000), 1_001, "made in the same ms");
        tasks[1].stamp = 5_000;
        assert_eq!(new_stamp(&tasks, 1_000), 5_001, "the clock went back");
        assert_eq!(new_stamp(&tasks, 9_000), 9_000);
    }

    #[test]
    fn an_unstamped_line_gets_the_same_stamp_on_every_read_and_a_new_title_another() {
        let mut tasks = with_ids(&[3, 4]);
        tasks[1].stamp = 1_763_500_000_000;
        stamp_unstamped(&mut tasks);
        let three = tasks[0].stamp;
        assert!(three > 0 && three < 1 << 32, "{three}");
        assert_eq!(tasks[1].stamp, 1_763_500_000_000, "a stamp stays");

        let mut again = with_ids(&[3]);
        stamp_unstamped(&mut again);
        assert_eq!(again[0].stamp, three, "the same line, the same stamp");

        // 3 removed, and a new 3 written by 0.2.0, without a stamp.
        let mut other = with_ids(&[3]);
        other[0].title = "pay rent".into();
        stamp_unstamped(&mut other);
        assert_ne!(other[0].stamp, three);
    }

    #[test]
    fn display_shows_the_due_date() {
        let mut task = with_ids(&[3]).remove(0);
        task.title = "pay rent".into();
        task.priority = Some(Priority::High);
        task.due = Some("2026-11-13 09:00".parse().unwrap());
        assert_eq!(
            task.to_string(),
            "3: pay rent !high due Fri 2026-11-13 09:00"
        );
    }

    #[test]
    fn next_id_is_the_smallest_free_number() {
        assert_eq!(next_id(&with_ids(&[])), 1);
        assert_eq!(next_id(&with_ids(&[1, 2, 3])), 4);
        assert_eq!(next_id(&with_ids(&[1, 2, 4])), 3);
        assert_eq!(next_id(&with_ids(&[5, 2, 3])), 1);
    }

    #[test]
    fn next_id_skips_a_gone_parents_id() {
        let mut tasks = with_ids(&[2]);
        tasks[0].parent = Some(1);
        assert_eq!(next_id(&tasks), 3);
    }

    #[test]
    fn sub_items_go_one_level_deep_and_an_orphan_is_a_card() {
        // 2 is under 1; 3 under 2, a sub item; 4 under 9, gone.
        let mut tasks = with_ids(&[1, 2, 3, 4]);
        tasks[1].parent = Some(1);
        tasks[2].parent = Some(2);
        tasks[3].parent = Some(9);
        assert_eq!(parents(&tasks), [None, Some(0), None, None]);
        assert!(is_sub_item(&tasks, &tasks[1]));
        assert!(!is_sub_item(&tasks, &tasks[3]));
    }
}
