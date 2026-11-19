use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::date::Due;

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
    use super::*;

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
