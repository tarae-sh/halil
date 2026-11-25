//! One filter, written with the marks `add` uses, read the same way for
//! `halil list`, the board and its calendar: plain words are in the title,
//! every one of them, case ignored; `+tag` (all of them); `!priority`;
//! `//<date>` due that day, `//by <date>` due on or before it, late ones too,
//! or `//late`. A task with no date matches none of the three.

use jiff::civil::{Date, DateTime};

use crate::date;
use crate::task::{Priority, Status, Task};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    /// As it was written, for the line that says it (`+home milk`).
    pub text: String,
    words: Vec<String>,
    tags: Vec<String>,
    priority: Option<Priority>,
    date: Option<On>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    Day(Date),
    By(Date),
    Late,
}

/// Reads a filter out of `text`, dates as of `now`: words, `+tags`, a
/// `!priority` and one `//` date. A `!word` that isn't a priority, or a `//`
/// whose words aren't a date, `by` and a date, or `late`, is refused as
/// `add` refuses it.
pub fn read(text: &str, now: DateTime) -> Result<Filter, String> {
    reading(text, now, false)
}

/// [`read`], while it's being typed: what doesn't read yet (a half-typed
/// `//` date, a priority not finished) is left out, never refused.
pub fn read_typing(text: &str, now: DateTime) -> Filter {
    reading(text, now, true).unwrap_or_default()
}

fn reading(text: &str, now: DateTime, typing: bool) -> Result<Filter, String> {
    let mut filter = Filter {
        text: text.split_whitespace().collect::<Vec<_>>().join(" "),
        ..Filter::default()
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        i += 1;
        if let Some(first) = word.strip_prefix("//") {
            let mut marked: Vec<&str> = Vec::from_iter(Some(first).filter(|f| !f.is_empty()));
            while i < words.len() && !ends(words[i]) {
                marked.push(words[i]);
                i += 1;
            }
            match (on(&marked, now), typing) {
                (Ok(on), _) => filter.date = Some(on),
                (Err(_), true) => {}
                (Err(e), false) => return Err(e),
            }
        } else if let Some(tag) = word.strip_prefix('+').filter(|t| !t.is_empty()) {
            let tag = tag.to_lowercase();
            if !filter.tags.contains(&tag) {
                filter.tags.push(tag);
            }
        } else if let Some(p) = word
            .strip_prefix('!')
            .filter(|p| p.starts_with(char::is_alphabetic))
        {
            match (p.parse(), typing) {
                (Ok(p), _) => filter.priority = Some(p),
                (Err(_), true) => {}
                (Err(e), false) => return Err(e),
            }
        } else {
            filter.words.push(word.to_lowercase());
        }
    }
    Ok(filter)
}

/// Whether `word` ends a `//`'s words: a `+tag`, a `!priority`, another `//`.
fn ends(word: &str) -> bool {
    word.starts_with("//")
        || word.strip_prefix('+').is_some_and(|t| !t.is_empty())
        || word
            .strip_prefix('!')
            .is_some_and(|p| p.starts_with(char::is_alphabetic))
}

/// The date a `//`'s words filter on: `late`, `by` and a date, or a date.
fn on(words: &[&str], now: DateTime) -> Result<On, String> {
    let bad = || format!("`//{}` isn't a date", words.join(" "));
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let day = |words: &[&str]| match date::read(words, now) {
        Ok((Some(due), rest)) if rest.is_empty() => Ok(due.date),
        _ => Err(bad()),
    };
    match lower.first().map(String::as_str) {
        Some("late") if words.len() == 1 => Ok(On::Late),
        Some("by") if words.len() > 1 => day(&words[1..]).map(On::By),
        Some(_) => day(words).map(On::Day),
        None => Err(bad()),
    }
}

impl Filter {
    /// Whether it lets every task through: nothing written.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
            && self.tags.is_empty()
            && self.priority.is_none()
            && self.date.is_none()
    }

    /// Whether `task` itself matches, as of `today`.
    pub fn matches(&self, task: &Task, today: Date) -> bool {
        let title = task.title.to_lowercase();
        self.words.iter().all(|w| title.contains(w.as_str()))
            && self.tags.iter().all(|t| task.tags.contains(t))
            && self.priority.is_none_or(|p| task.priority == Some(p))
            && match (self.date, task.due) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(On::Day(d)), Some(due)) => due.date == d,
                (Some(On::By(d)), Some(due)) => due.date <= d,
                (Some(On::Late), Some(due)) => task.status != Status::Done && due.date < today,
            }
    }

    /// Whether the task at `i` is shown: it matches, or, a card, one of its
    /// sub items does. `parents` is [`crate::task::parents`] of `tasks`.
    pub fn shows(&self, tasks: &[Task], parents: &[Option<usize>], i: usize, today: Date) -> bool {
        self.matches(&tasks[i], today)
            || (parents[i].is_none()
                && (0..tasks.len())
                    .any(|s| parents[s] == Some(i) && self.matches(&tasks[s], today)))
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;
    use crate::task;

    fn task(id: u32, title: &str) -> Task {
        Task {
            id,
            title: title.into(),
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
        }
    }

    /// Wednesday 2026-11-11, 10:30.
    fn now() -> DateTime {
        date(2026, 11, 11).at(10, 30, 0, 0)
    }

    fn tasks() -> Vec<Task> {
        let mut milk = task(1, "buy oat Milk");
        milk.tags = vec!["home".into(), "shop".into()];
        milk.due = Some("2026-11-13".parse().unwrap());
        let mut rent = task(2, "pay rent");
        rent.tags = vec!["home".into()];
        rent.priority = Some(Priority::High);
        rent.due = Some("2026-11-09".parse().unwrap());
        let mut bike = task(3, "fix bike");
        bike.priority = Some(Priority::Low);
        bike.due = Some("2026-11-15".parse().unwrap());
        let mut milk_card = task(4, "weekly shop");
        milk_card.tags = vec!["shop".into()];
        let mut sub = task(5, "almond milk");
        sub.parent = Some(4);
        let mut done = task(6, "old rent");
        done.status = Status::Done;
        done.due = Some("2026-11-01".parse().unwrap());
        vec![milk, rent, bike, milk_card, sub, done, task(7, "read")]
    }

    /// The ids `text` lets through, cards as shown (a sub item's match shows
    /// its card), sub items by their own match.
    fn shown(text: &str) -> Vec<u32> {
        let tasks = tasks();
        let parents = task::parents(&tasks);
        let f = read(text, now()).unwrap();
        (0..tasks.len())
            .filter(|&i| f.shows(&tasks, &parents, i, now().date()))
            .map(|i| tasks[i].id)
            .collect()
    }

    #[test]
    fn each_mark_alone_and_two_together() {
        // Words: in the title, case ignored, every one.
        assert_eq!(
            shown("milk"),
            [1, 4, 5],
            "a sub item's match shows its card"
        );
        assert_eq!(shown("MILK oat"), [1]);
        assert_eq!(shown("milk rent"), Vec::<u32>::new());
        // Tags: all of them.
        assert_eq!(shown("+home"), [1, 2]);
        assert_eq!(shown("+home +shop"), [1]);
        // A priority.
        assert_eq!(shown("!high"), [2]);
        assert_eq!(shown("!l"), [3]);
        // Dates; a task with none matches none of them.
        assert_eq!(shown("//fri"), [1]);
        assert_eq!(shown("//by fri"), [1, 2, 6], "late ones too");
        assert_eq!(shown("//by sun"), [1, 2, 3, 6]);
        assert_eq!(shown("//late"), [2], "open, its day gone");
        // Two together.
        assert_eq!(shown("+home //late"), [2]);
        // One task must have both: card 4 has +shop, its sub item milk.
        assert_eq!(shown("milk +shop"), [1]);
        assert_eq!(shown("+home !high rent"), [2]);
        // Nothing written: everything.
        assert_eq!(shown("").len(), 7);
        assert!(read("  ", now()).unwrap().is_empty());
        assert_eq!(read(" +home   milk ", now()).unwrap().text, "+home milk");
    }

    #[test]
    fn what_isnt_a_filter_is_refused_and_while_typing_left_out() {
        for (text, err) in [
            ("//fridya", "`//fridya` isn't a date"),
            ("//by", "`//by` isn't a date"),
            ("//late fri", "`//late fri` isn't a date"),
            ("//", "`//` isn't a date"),
            ("!urgent", "`!urgent` isn't a priority"),
        ] {
            let got = read(text, now()).unwrap_err();
            assert!(got.contains(err), "{text}: {got}");
        }
        // Half typed: no error, and it doesn't narrow yet.
        let typing = read_typing("milk //fr", now());
        assert!(!typing.is_empty());
        assert_eq!(typing.date, None);
        assert!(read_typing("//b", now()).is_empty());
        assert_eq!(
            read_typing("+home //by fri", now()),
            read("+home //by fri", now()).unwrap()
        );
    }
}
