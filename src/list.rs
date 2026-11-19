use std::cmp::Reverse;

use jiff::civil::{DateTime, Time};

use crate::date::Due;
use crate::task::{self, Priority, Status, Task};

/// The open tasks as a table: soonest due first (so late ones lead), undated
/// after, then highest priority, then oldest. A task's open sub items come
/// under it, indented, in id order, and how many of them are done after it.
/// An open sub item whose task is done (as a hand-edited file may have it) is
/// listed as a task of its own: no open task is left out.
pub fn render(tasks: &[Task], now: DateTime, color: bool) -> String {
    let parents = task::parents(tasks);
    let under_open = |i: usize| parents[i].is_some_and(|p| tasks[p].status != Status::Done);
    let mut cards: Vec<usize> = (0..tasks.len())
        .filter(|&i| !under_open(i) && tasks[i].status != Status::Done)
        .collect();
    if cards.is_empty() {
        return "Nothing to do.\n".into();
    }
    cards.sort_by_key(|&i| {
        let t = &tasks[i];
        (t.due.is_none(), t.due, Reverse(t.priority), t.id)
    });
    // Each task, whether it's a sub item, and its sub items' count.
    let mut open: Vec<(&Task, bool, Option<String>)> = Vec::new();
    for i in cards {
        let subs = task::sub_items(tasks, &parents, i);
        open.push((&tasks[i], false, task::progress(tasks, &subs)));
        for s in subs
            .into_iter()
            .filter(|&s| tasks[s].status != Status::Done)
        {
            open.push((&tasks[s], true, None));
        }
    }

    let paint = |code: &str, s: String| -> String {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s
        }
    };
    let id_width = open
        .iter()
        .map(|(t, ..)| t.id.to_string().len())
        .max()
        .unwrap_or(0)
        .max(2);
    // The DUE column shows only when some task has a date.
    let due_width = open
        .iter()
        .filter_map(|(t, ..)| t.due.map(|d| due_words(d, now).0.len()))
        .max()
        .map(|w| w.max(3));

    let mut header = format!("{:>id_width$}  {:<4}  ", "ID", "PRI");
    if let Some(w) = due_width {
        header += &format!("{:<w$}  ", "DUE");
    }
    header += "TASK";
    let mut out = paint("2", header) + "\n";
    for (t, sub, progress) in open {
        let pri = match t.priority {
            Some(p) => paint(priority_color(p), format!("{p:<4}")),
            None => " ".repeat(4),
        };
        let mut line = format!("{}  {pri}  ", paint("2", format!("{:>id_width$}", t.id)));
        if let Some(w) = due_width {
            line += &match t.due {
                Some(d) => {
                    let (words, code) = due_words(d, now);
                    paint(code, format!("{words:<w$}"))
                }
                None => " ".repeat(w),
            };
            line += "  ";
        }
        if sub {
            line += "  ";
        }
        line += &t.title;
        if let Some(progress) = progress {
            line += "  ";
            line += &paint("2", progress);
        }
        if !t.tags.is_empty() {
            let tags: Vec<String> = t.tags.iter().map(|tag| format!("+{tag}")).collect();
            line += "  ";
            line += &paint("36", tags.join(" "));
        }
        out += &line;
        out.push('\n');
    }
    out
}

/// When `due` is, in words from `now` — `today 6pm`, `tomorrow`, `fri`,
/// `in 3w`, `2d late` — and the color to paint them.
fn due_words(due: Due, now: DateTime) -> (String, &'static str) {
    let days = (due.date - now.date()).get_days();
    let at = |s: String| match due.time {
        Some(t) => format!("{s} {}", clock(t)),
        None => s,
    };
    match days {
        ..0 => (format!("{}d late", -days), "31"),
        0 => {
            let late = due.time.is_some_and(|t| t < now.time());
            (at("today".into()), if late { "31" } else { "33" })
        }
        1 => (at("tomorrow".into()), "0"),
        2..7 => (at(due.date.strftime("%a").to_string().to_lowercase()), "0"),
        7..14 => (format!("in {days}d"), "2"),
        14..60 => (format!("in {}w", days / 7), "2"),
        _ => (format!("in {}mo", days / 30), "2"),
    }
}

/// `6pm`, `9:30am`.
fn clock(t: Time) -> String {
    let ampm = if t.hour() < 12 { "am" } else { "pm" };
    let h = match t.hour() % 12 {
        0 => 12,
        h => h,
    };
    match t.minute() {
        0 => format!("{h}{ampm}"),
        m => format!("{h}:{m:02}{ampm}"),
    }
}

fn priority_color(p: Priority) -> &'static str {
    match p {
        Priority::High => "31",
        Priority::Med => "33",
        Priority::Low => "2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: u32, title: &str, priority: Option<Priority>, status: Status) -> Task {
        Task {
            id,
            title: title.into(),
            status,
            priority,
            tags: vec![],
            due: None,
            parent: None,
            stamp: 0,
        }
    }

    fn sub(id: u32, title: &str, status: Status, parent: u32) -> Task {
        Task {
            parent: Some(parent),
            ..task(id, title, None, status)
        }
    }

    #[test]
    fn sub_items_come_under_their_task_indented_with_the_count_after_it() {
        let mut ship = task(3, "ship 0.2.0", Some(Priority::High), Status::Doing);
        ship.tags = vec!["work".into()];
        let mut tests = sub(6, "write the tests", Status::Todo, 3);
        tests.priority = Some(Priority::Low);
        let tasks = [
            task(1, "buy milk", None, Status::Todo),
            sub(7, "tag the release", Status::Doing, 3),
            ship,
            sub(5, "bump the version", Status::Done, 3),
            tests,
            // Its task is gone: a task of its own.
            sub(8, "orphan", Status::Todo, 99),
        ];
        let now = jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0);
        assert_eq!(
            render(&tasks, now, false),
            concat!(
                "ID  PRI   TASK\n",
                " 3  high  ship 0.2.0  1/3  +work\n",
                " 6  low     write the tests\n",
                " 7          tag the release\n",
                " 1        buy milk\n",
                " 8        orphan\n",
            )
        );
        assert!(render(&tasks, now, true).contains("ship 0.2.0  \x1b[2m1/3\x1b[0m"));
    }

    #[test]
    fn an_open_sub_item_whose_task_is_done_is_listed_as_a_task_of_its_own() {
        // A hand-edited file: ship is done, write the tests isn't.
        let tasks = [
            task(1, "buy milk", None, Status::Todo),
            task(3, "ship 0.2.0", Some(Priority::High), Status::Done),
            sub(5, "bump the version", Status::Done, 3),
            sub(6, "write the tests", Status::Doing, 3),
        ];
        let now = jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0);
        assert_eq!(
            render(&tasks, now, false),
            concat!(
                "ID  PRI   TASK\n",
                " 1        buy milk\n",
                " 6        write the tests\n",
            )
        );
    }

    #[test]
    fn lists_open_tasks_by_priority_then_id() {
        let mut rent = task(4, "pay rent", Some(Priority::High), Status::Todo);
        rent.tags = vec!["home".into()];
        let tasks = [
            task(1, "buy milk", None, Status::Todo),
            task(2, "call mum", Some(Priority::Low), Status::Doing),
            task(3, "old thing", Some(Priority::High), Status::Done),
            rent,
            task(5, "fix bike", Some(Priority::Med), Status::Todo),
            task(6, "read", None, Status::Todo),
        ];
        let now = jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0);
        assert_eq!(
            render(&tasks, now, false),
            concat!(
                "ID  PRI   TASK\n",
                " 4  high  pay rent  +home\n",
                " 5  med   fix bike\n",
                " 2  low   call mum\n",
                " 1        buy milk\n",
                " 6        read\n",
            )
        );
    }

    #[test]
    fn lists_late_then_soonest_due_then_undated() {
        use jiff::ToSpan;
        use jiff::civil::date;

        use crate::date::Due;

        // Wednesday 2026-11-11, 10:30.
        let now = date(2026, 11, 11).at(10, 30, 0, 0);
        let due = |days: i64| {
            Some(Due {
                date: now.date().checked_add(days.days()).unwrap(),
                time: None,
            })
        };
        let undated = task(1, "read", None, Status::Todo);
        let mut later = task(2, "renew passport", Some(Priority::High), Status::Todo);
        later.due = due(21);
        let mut today = task(3, "call mum", None, Status::Todo);
        today.due = due(0);
        let mut late = task(4, "pay rent", None, Status::Todo);
        late.due = due(-2);
        let tasks = [undated, later, today, late];
        assert_eq!(
            render(&tasks, now, false),
            concat!(
                "ID  PRI   DUE      TASK\n",
                " 4        2d late  pay rent\n",
                " 3        today    call mum\n",
                " 2  high  in 3w    renew passport\n",
                " 1                 read\n",
            )
        );
    }

    #[test]
    fn due_dates_in_words() {
        use jiff::civil::{date, time};

        // Wednesday 2026-11-11, 10:30.
        let now = date(2026, 11, 11).at(10, 30, 0, 0);
        let words = |d, t| due_words(Due { date: d, time: t }, now).0;
        assert_eq!(
            words(date(2026, 11, 11), Some(time(18, 0, 0, 0))),
            "today 6pm"
        );
        assert_eq!(words(date(2026, 11, 12), None), "tomorrow");
        assert_eq!(
            words(date(2026, 11, 13), Some(time(9, 30, 0, 0))),
            "fri 9:30am"
        );
        assert_eq!(words(date(2026, 11, 20), None), "in 9d");
        assert_eq!(words(date(2026, 12, 2), None), "in 3w");
        assert_eq!(words(date(2026, 11, 9), None), "2d late");
    }

    #[test]
    fn nothing_open_says_so() {
        let tasks = [task(1, "done", None, Status::Done)];
        let now = jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0);
        assert_eq!(render(&tasks, now, false), "Nothing to do.\n");
        assert_eq!(render(&[], now, false), "Nothing to do.\n");
    }
}
