use std::ops::Range;

use jiff::civil::DateTime;

use crate::date::{self, Due, Every};
use crate::task::{Priority, Task};

/// What `add` reads out of its words.
#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub title: String,
    pub tags: Vec<String>,
    pub priority: Option<Priority>,
    pub due: Option<Due>,
    /// How it repeats: `every …` inside the `//` mark.
    pub every: Option<Every>,
}

/// Splits the words of `add` into the title, `+tags`, a `!priority` and a due
/// date, read as of `now` (see [`date::read`]).
///
/// The words may come as separate arguments or as one quoted string. Tags are
/// lowercased and kept once each; the last priority wins. A lone `+` or `!`, or
/// `!` not followed by a letter (`!!`), stays in the title.
///
/// `//` marks the date: the words after it, up to the next `+tag`, `!priority`,
/// `-tag` or the end, must read as one (see [`mark_due`]), and then the other
/// words are all title, date words or not. Without `//`, date words anywhere
/// are read as ever. A `//` with no words is refused here: only `edit` takes
/// it, as the date taken off.
pub fn parse(words: &[String], now: DateTime) -> Result<Parsed, String> {
    let Read {
        parsed, undated, ..
    } = read(words, now, |_| false, false)?;
    if undated {
        return Err(NEEDS_A_DATE.into());
    }
    if parsed.title.is_empty() {
        return Err("a task needs a title, not just a date, tags and a priority".into());
    }
    Ok(parsed)
}

/// What the words of `edit` change in a task.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Change {
    /// A new title, when words are left after the rest is read out.
    pub title: Option<String>,
    /// Tags to add, and tags to take off.
    pub add: Vec<String>,
    pub remove: Vec<String>,
    /// A priority that replaces the task's own.
    pub priority: Option<Priority>,
    /// A date that replaces the task's own: `Some(None)` takes it off.
    pub due: Option<Option<Due>>,
    /// A repeat that replaces the task's own: `Some(None)` stops it (`-every`,
    /// or `//` alone, the date taken off).
    pub every: Option<Option<Every>>,
}

impl Change {
    pub fn apply(&self, task: &mut Task) {
        if let Some(title) = &self.title {
            task.title.clone_from(title);
        }
        for tag in &self.add {
            if !task.tags.contains(tag) {
                task.tags.push(tag.clone());
            }
        }
        task.tags.retain(|t| !self.remove.contains(t));
        task.priority = self.priority.or(task.priority);
        if let Some(due) = self.due {
            // A date moved alone keeps the step, a month or year step aimed
            // at its day now.
            if self.every.is_none()
                && let (Some(due), Some(every)) = (due, task.every.as_mut())
            {
                *every = every.aimed_at(due.date);
            }
            task.due = due;
        }
        if let Some(every) = self.every {
            task.every = every;
        }
    }
}

/// Reads the words of `edit` as `parse` reads them, and `-tag` too, which takes
/// a tag off: it's read only when `has(tag)` says the task has it, else it
/// stays in the title. A `//` with no words after it, up to the next mark or
/// the end, takes the date off, and the other words are all title, as with
/// any `//`. When the task `repeats`, `-every` stops it; a `//` mark with
/// `every …` in it gives a new one, and one without leaves the task's as it
/// is; `//` alone takes the step off with the date.
pub fn change(
    words: &[String],
    now: DateTime,
    has: impl Fn(&str) -> bool,
    repeats: bool,
) -> Result<Change, String> {
    let Read {
        parsed: p,
        untags,
        undated,
        stop_every,
    } = read(words, now, has, repeats)?;
    Ok(Change {
        title: Some(p.title).filter(|t| !t.is_empty()),
        add: p.tags,
        remove: untags,
        priority: p.priority,
        due: match undated {
            true => Some(None),
            false => p.due.map(Some),
        },
        every: match (p.every, undated || stop_every) {
            (Some(every), _) => Some(Some(every)),
            (None, true) => Some(None),
            (None, false) => None,
        },
    })
}

/// The words `e` fills in on the board for `task`, which [`change`] reads
/// back as it on any day, and the byte its title ends at: its title, then its
/// date after `//` in words that name the day ([`date::say_exact`]:
/// `2026-11-27 6pm`), never one relative to today, which a save after
/// midnight would move. With no date, the title alone, or with `//` alone
/// after it when its words would read as a date (`plan the friday party //`),
/// so they stay title.
pub fn edit_line(task: &Task, now: DateTime) -> (String, usize) {
    let mark = match task.due {
        Some(due) => match task.every {
            Some(every) => format!(" //{} {}", date::say_exact(due), every.words(due.date)),
            None => format!(" //{}", date::say_exact(due)),
        },
        None => {
            let words: Vec<&str> = task.title.split_whitespace().collect();
            match date::read(&words, now) {
                Ok((None, _)) => String::new(),
                _ => " //".into(),
            }
        }
    };
    (format!("{}{mark}", task.title), task.title.len())
}

/// Changes `task` by the words of `edit` (see [`change`]): words left after the
/// date, tags and priority replace its title, `+tags` are added to its own and
/// `-tags` taken off, and a priority or a date replaces its own; `//` alone
/// takes the date off.
pub fn edit(task: &mut Task, words: &[String], now: DateTime) -> Result<Change, String> {
    let change = change(
        words,
        now,
        |tag| task.tags.iter().any(|t| t == tag),
        task.every.is_some(),
    )?;
    if change == Change::default() {
        return Err(
            "nothing to change: give a new title, a date, +tags, -tags or a !priority".into(),
        );
    }
    refuse_missing_tags(task.id, &change)?;
    change.apply(task);
    Ok(change)
}

/// Refuses `change` to task `id` when every word left for the title is a
/// `-word`: tags the task hasn't, meant to come off, which as its title would
/// replace its own (`3 has no tag money`). Beside other words of a title, a
/// `-word` is one of them.
pub fn refuse_missing_tags(id: u32, change: &Change) -> Result<(), String> {
    let Some(title) = &change.title else {
        return Ok(());
    };
    let mut tags: Vec<String> = Vec::new();
    for word in title.split_whitespace() {
        match word.strip_prefix('-').filter(|t| !t.is_empty()) {
            Some(tag) if !tags.contains(&tag.to_lowercase()) => tags.push(tag.to_lowercase()),
            Some(_) => {}
            None => return Ok(()),
        }
    }
    let named = match tags.split_last() {
        Some((last, [])) if last == "every" => return Err(format!("{id} doesn't repeat")),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => return Ok(()),
    };
    Err(format!("{id} has no tag {named}"))
}

const NEEDS_A_DATE: &str = "`//` needs a date";

/// What [`read`] reads.
struct Read {
    /// `parse`'s reading, the title allowed empty.
    parsed: Parsed,
    /// The `-tags` that `has`.
    untags: Vec<String>,
    /// A `//` with no words: the date taken off, on `edit`.
    undated: bool,
    /// `-every`, on a task that repeats.
    stop_every: bool,
}

fn read(
    words: &[String],
    now: DateTime,
    has: impl Fn(&str) -> bool,
    repeats: bool,
) -> Result<Read, String> {
    let mut stop_every = false;
    let mut title = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    let mut untags: Vec<String> = Vec::new();
    let mut priority = None;
    // Each `//`'s words, and whether the word read last was one of them.
    let mut marks: Vec<Vec<&str>> = Vec::new();
    let mut in_mark = false;

    for word in words.iter().flat_map(|w| w.split_whitespace()) {
        if let Some(rest) = word.strip_prefix("//") {
            marks.push(Vec::from_iter(Some(rest).filter(|r| !r.is_empty())));
            in_mark = true;
            continue;
        }
        if in_mark && !ends_mark(word) {
            marks.last_mut().expect("a mark").push(word);
            continue;
        }
        in_mark = false;
        if let Some(tag) = word.strip_prefix('+').filter(|t| !t.is_empty()) {
            let tag = tag.to_lowercase();
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        } else if repeats && word.eq_ignore_ascii_case("-every") {
            stop_every = true;
        } else if let Some(tag) = word
            .strip_prefix('-')
            .map(str::to_lowercase)
            .filter(|t| !t.is_empty() && has(t))
        {
            if !untags.contains(&tag) {
                untags.push(tag);
            }
        } else if let Some(p) = word
            .strip_prefix('!')
            .filter(|p| p.starts_with(|c: char| c.is_alphabetic()))
        {
            priority = Some(p.parse()?);
        } else {
            title.push(word);
        }
    }

    let (due, title, every) = match &marks[..] {
        [] => {
            let (due, title) = date::read(&title, now)?;
            (due, title, None)
        }
        [words] if words.is_empty() => (None, title, None),
        [words] => {
            let (due, every) = mark_due(words, now)?;
            (Some(due), title, every)
        }
        [first, second, ..] => {
            return Err(format!(
                "`//{}` and `//{}` both give the date: give one",
                first.join(" "),
                second.join(" ")
            ));
        }
    };
    let parsed = Parsed {
        title: title.join(" "),
        tags,
        priority,
        due,
        every,
    };
    Ok(Read {
        parsed,
        untags,
        undated: matches!(&marks[..], [words] if words.is_empty()),
        stop_every,
    })
}

/// Whether `word` ends the words a `//` marks: a `+tag`, a `!priority`, a
/// `-tag`, or another `//`. No date word starts as these do.
fn ends_mark(word: &str) -> bool {
    word.starts_with("//")
        || word.strip_prefix('+').is_some_and(|t| !t.is_empty())
        || word.strip_prefix('-').is_some_and(|t| !t.is_empty())
        || word
            .strip_prefix('!')
            .is_some_and(|p| p.starts_with(char::is_alphabetic))
}

/// Whether `word` starts the step in a `//` mark: `every`, `daily`, `weekly`,
/// `monthly`, `yearly`.
fn starts_step(word: &str) -> bool {
    matches!(
        word.to_lowercase().as_str(),
        "every" | "daily" | "weekly" | "monthly" | "yearly"
    )
}

/// The date the words after a `//` give, read as of `now`, and how it
/// repeats: `fri 6pm`, `nov 27`, `dec 1 every 2 weeks`, `every fri`. The words
/// before a step must read as one date, and the step's as one step (see
/// [`date::read_step`]), else they're refused, quoted, never taken into the
/// title. With a step and no date, the first is the step's next match from
/// today, today included. No words are none ([`read`] takes a `//` alone as
/// the date taken off).
pub fn mark_due(words: &[&str], now: DateTime) -> Result<(Due, Option<Every>), String> {
    let bad = || format!("`//{}` isn't a date", words.join(" "));
    let at = words.iter().position(|w| starts_step(w));
    let (date_words, step) = match at {
        Some(i) => {
            let lower: Vec<String> = words[i..].iter().map(|w| w.to_lowercase()).collect();
            let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
            (&words[..i], Some(date::read_step(&lower).ok_or_else(bad)?))
        }
        None => (words, None),
    };
    let due = match (date_words, &step) {
        ([], Some(step)) => step.first(now.date()),
        _ => match date::read(date_words, now) {
            Ok((Some(due), rest)) if rest.is_empty() => due,
            _ => return Err(bad()),
        },
    };
    Ok(match step {
        Some(step) => {
            let (due, every) = step.with_first(due);
            (due, Some(every))
        }
        None => (due, None),
    })
}

/// The bytes of `text` that the `//` at byte `at` marks: its words, from
/// right after it to the next mark or the end, without the spaces around
/// them; an empty range right after it when there are none. A word glued to
/// the `//` is always its own.
pub fn marked(text: &str, at: usize) -> Range<usize> {
    let from = at + 2;
    let mut span = from..from;
    let mut i = from;
    loop {
        let rest = &text[i..];
        let start = i + (rest.len() - rest.trim_start().len());
        let rest = &text[start..];
        if rest.is_empty() {
            break;
        }
        let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if start != from && ends_mark(&rest[..len]) {
            break;
        }
        if span.is_empty() {
            span.start = start;
        }
        span.end = start + len;
        i = span.end;
    }
    span
}

/// The `//` whose words hold byte `cursor` of `text` — right after it, or in
/// or at the end of its words — as the byte it's at.
pub fn mark_holding(text: &str, cursor: usize) -> Option<usize> {
    text.match_indices("//")
        .map(|(at, _)| at)
        .filter(|&at| {
            text[..at]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace)
        })
        .filter(|&at| (at + 2..=marked(text, at).end).contains(&cursor))
        .last()
}

/// The date the `//` at byte `at` of `text` marks, if its words read as one.
pub fn marked_due(text: &str, at: usize, now: DateTime) -> Option<Due> {
    let words: Vec<&str> = text[marked(text, at)].split_whitespace().collect();
    mark_due(&words, now).ok().map(|(due, _)| due)
}

/// The bytes of the weekday in the step the `//` at byte `at` marks, when
/// the step is `every <weekday>` (`every fri`): a move in the calendar
/// rewrites it to the picked day's.
pub fn marked_step_weekday(text: &str, at: usize) -> Option<Range<usize>> {
    let (date, span) = (marked_date(text, at), marked(text, at));
    let step = &text[date.end..span.end];
    let words: Vec<(usize, &str)> = step
        .split_whitespace()
        .map(|w| (w.as_ptr() as usize - step.as_ptr() as usize, w))
        .collect();
    match words[..] {
        [(_, every), (i, weekday)]
            if every.eq_ignore_ascii_case("every") && date::is_weekday(weekday) =>
        {
            Some(date.end + i..date.end + i + weekday.len())
        }
        _ => None,
    }
}

/// The bytes of the date's own words that the `//` at byte `at` marks: its
/// words up to a step (`every …`), which the calendar leaves as typed; an
/// empty range where the step starts when there's no date before it.
pub fn marked_date(text: &str, at: usize) -> Range<usize> {
    let span = marked(text, at);
    let mut end = span.start;
    let mut i = span.start;
    for word in text[span.clone()].split_whitespace() {
        let start = i + text[i..].find(word).unwrap_or(0);
        if starts_step(word) {
            return span.start..end;
        }
        end = start + word.len();
        i = end;
    }
    span
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    /// `parse` as of Wednesday 2026-11-11, 10:30.
    fn parse(words: &[String]) -> Result<Parsed, String> {
        super::parse(words, date(2026, 11, 11).at(10, 30, 0, 0))
    }

    #[test]
    fn reads_title_tags_and_priority() {
        let p = parse(&words("pay rent +home !high")).unwrap();
        assert_eq!(
            p,
            Parsed {
                title: "pay rent".into(),
                tags: vec!["home".into()],
                priority: Some(Priority::High),
                due: None,
                every: None,
            }
        );
    }

    #[test]
    fn reads_a_due_date_out_of_the_title() {
        let p = parse(&words("pay rent friday +home 9am !high")).unwrap();
        assert_eq!(p.title, "pay rent");
        assert_eq!(p.tags, ["home"]);
        assert_eq!(p.priority, Some(Priority::High));
        assert_eq!(p.due.unwrap().to_string(), "2026-11-13 09:00");
    }

    #[test]
    fn a_bad_date_is_an_error() {
        let err = parse(&words("x today tomorrow")).unwrap_err();
        assert!(err.contains("`today`"), "{err}");
    }

    #[test]
    fn one_quoted_string_reads_like_separate_words() {
        let quoted = parse(&["pay  rent +home !high".to_string()]).unwrap();
        assert_eq!(quoted, parse(&words("pay rent +home !high")).unwrap());
    }

    #[test]
    fn tags_and_priority_can_go_anywhere() {
        let p = parse(&words("+work call !low the bank +phone")).unwrap();
        assert_eq!(p.title, "call the bank");
        assert_eq!(p.tags, ["work", "phone"]);
        assert_eq!(p.priority, Some(Priority::Low));
    }

    #[test]
    fn plain_words_have_no_tags_or_priority() {
        let p = parse(&words("buy milk")).unwrap();
        assert_eq!(p.title, "buy milk");
        assert!(p.tags.is_empty());
        assert_eq!(p.priority, None);
    }

    #[test]
    fn tags_are_lowercased_and_kept_once() {
        let p = parse(&words("x +Home +home +HOME +car")).unwrap();
        assert_eq!(p.tags, ["home", "car"]);
    }

    #[test]
    fn priority_names_and_short_forms() {
        for (word, want) in [
            ("!high", Priority::High),
            ("!H", Priority::High),
            ("!med", Priority::Med),
            ("!medium", Priority::Med),
            ("!m", Priority::Med),
            ("!Low", Priority::Low),
            ("!l", Priority::Low),
        ] {
            let p = parse(&words(&format!("x {word}"))).unwrap();
            assert_eq!(p.priority, Some(want), "{word}");
        }
    }

    #[test]
    fn last_priority_wins() {
        let p = parse(&words("x !low !high")).unwrap();
        assert_eq!(p.priority, Some(Priority::High));
    }

    #[test]
    fn unknown_priority_is_an_error() {
        let err = parse(&words("x !urgent")).unwrap_err();
        assert!(err.contains("!urgent"), "{err}");
    }

    #[test]
    fn lone_marks_stay_in_the_title() {
        let p = parse(&words("1 + 1 ! !! wow!")).unwrap();
        assert_eq!(p.title, "1 + 1 ! !! wow!");
        assert!(p.tags.is_empty());
        assert_eq!(p.priority, None);
    }

    #[test]
    fn a_title_is_required() {
        assert!(parse(&words("+home !high")).is_err());
        assert!(parse(&["  ".to_string()]).is_err());
        assert!(parse(&words("tomorrow 9am +home")).is_err());
    }

    fn rent() -> Task {
        Task {
            id: 1,
            title: "pay rent".into(),
            status: crate::task::Status::Todo,
            priority: Some(Priority::Low),
            tags: vec!["home".into()],
            due: Some("2026-11-20".parse().unwrap()),
            parent: None,
            stamp: 0,
            done: None,
            added: None,
            every: None,
            next: None,
        }
    }

    /// `edit` as of Wednesday 2026-11-11, 10:30.
    fn edit(task: &mut Task, s: &str) -> Result<(), String> {
        super::edit(task, &words(s), date(2026, 11, 11).at(10, 30, 0, 0)).map(|_| ())
    }

    #[test]
    fn edit_without_a_title_keeps_the_title() {
        let mut task = rent();
        edit(&mut task, "friday 9am !high +money").unwrap();
        assert_eq!(task.title, "pay rent");
        assert_eq!(task.priority, Some(Priority::High));
        assert_eq!(task.tags, ["home", "money"]);
        assert_eq!(task.due.unwrap().to_string(), "2026-11-13 09:00");
    }

    #[test]
    fn edit_with_words_replaces_the_title_and_keeps_the_rest() {
        let mut task = rent();
        edit(&mut task, "pay the rent +Home").unwrap();
        assert_eq!(task.title, "pay the rent");
        assert_eq!(task.tags, ["home"]);
        assert_eq!(task.priority, Some(Priority::Low));
        assert_eq!(task.due, rent().due);
    }

    #[test]
    fn minus_tag_takes_off_a_tag_the_task_has_and_is_a_title_word_otherwise() {
        let mut task = rent();
        task.tags.push("money".into());
        edit(&mut task, "-Home").unwrap();
        assert_eq!(task.tags, ["money"]);
        assert_eq!(task.title, "pay rent");
        edit(&mut task, "pay rent -work").unwrap();
        assert_eq!(task.title, "pay rent -work");
        assert_eq!(task.tags, ["money"]);
        assert_eq!(
            parse(&words("x -money")).unwrap().title,
            "x -money",
            "add has no tags to take off"
        );
    }

    #[test]
    fn minus_tags_the_task_hasnt_alone_for_a_title_are_refused() {
        let mut task = rent();
        assert_eq!(edit(&mut task, "-work"), Err("1 has no tag work".into()));
        assert_eq!(
            edit(&mut task, "-Work -car -work"),
            Err("1 has no tag work or car".into())
        );
        // -home comes off, but -work would be the title: all of it refused.
        assert_eq!(
            edit(&mut task, "-home -work !high"),
            Err("1 has no tag work".into())
        );
        assert_eq!(task, rent(), "the title untouched, and the rest");
        // Beside other words, a -word is one of the title's, as before.
        edit(&mut task, "rent -work").unwrap();
        assert_eq!(task.title, "rent -work");
    }

    #[test]
    fn slashes_mark_the_date_up_to_the_next_mark() {
        let p = parse(&words("pay rent //fri 6pm +home !high")).unwrap();
        assert_eq!(p.title, "pay rent");
        assert_eq!(p.tags, ["home"]);
        assert_eq!(p.priority, Some(Priority::High));
        assert_eq!(p.due.unwrap().to_string(), "2026-11-13 18:00");
        // A space after it, at the end, or before the title.
        let p = parse(&words("pay rent // nov 27")).unwrap();
        assert_eq!(p.due.unwrap().to_string(), "2026-11-27");
        let p = parse(&words("//2027-01-03 +home pay rent")).unwrap();
        assert_eq!(p.title, "pay rent");
        assert_eq!(p.due.unwrap().to_string(), "2027-01-03");
        // The words of a quoted string as of separate ones.
        let p = parse(&["call mom //tomorrow 9am".to_string()]).unwrap();
        assert_eq!(p.due.unwrap().to_string(), "2026-11-12 09:00");
        // A day with its year, a comma in it too.
        let p = parse(&words("file taxes //december 31, 2026 +money")).unwrap();
        assert_eq!(
            (p.title.as_str(), p.due.unwrap().to_string()),
            ("file taxes", "2026-12-31".into())
        );
    }

    #[test]
    fn with_slashes_the_other_words_are_all_title() {
        let p = parse(&words("plan the friday party //wed")).unwrap();
        assert_eq!(p.title, "plan the friday party");
        assert_eq!(p.due.unwrap().to_string(), "2026-11-11");
        // Without them, date words read as ever.
        let p = parse(&words("plan the party friday")).unwrap();
        assert_eq!(p.title, "plan the party");
        assert_eq!(p.due.unwrap().to_string(), "2026-11-13");
        // A -word ends the date, and stays a word of the title in add.
        let p = parse(&words("pay //fri -work")).unwrap();
        assert_eq!(
            (p.title.as_str(), p.due.unwrap().to_string()),
            ("pay -work", "2026-11-13".into())
        );
    }

    #[test]
    fn slashes_that_dont_read_as_one_date_are_refused_quoted() {
        for (s, err) in [
            ("pay rent //fridya", "`//fridya` isn't a date"),
            (
                "pay rent //fri call him +home",
                "`//fri call him` isn't a date",
            ),
            ("pay rent //feb 30", "`//feb 30` isn't a date"),
            ("pay rent //feb 30 2027", "`//feb 30 2027` isn't a date"),
            ("pay rent //dec 2026", "`//dec 2026` isn't a date"),
            ("pay rent //fri sat", "`//fri sat` isn't a date"),
            ("pay rent //", "`//` needs a date"),
            ("pay rent // +home", "`//` needs a date"),
            (
                "pay //fri //sat",
                "`//fri` and `//sat` both give the date: give one",
            ),
        ] {
            assert_eq!(parse(&words(s)), Err(err.into()), "{s}");
        }
        let mut task = rent();
        assert_eq!(
            edit(&mut task, "//fridya"),
            Err("`//fridya` isn't a date".into())
        );
        assert_eq!(task, rent());
    }

    #[test]
    fn edit_takes_a_marked_date() {
        let mut task = rent();
        edit(&mut task, "//nov 27 6pm -home").unwrap();
        assert_eq!(task.title, "pay rent");
        assert!(task.tags.is_empty());
        assert_eq!(task.due.unwrap().to_string(), "2026-11-27 18:00");
    }

    #[test]
    fn the_marked_words_in_a_line_and_their_date() {
        let now = date(2026, 11, 11).at(10, 30, 0, 0);
        let line = "pay rent //fri 6pm +home";
        let at = line.find("//").unwrap();
        assert_eq!(&line[marked(line, at)], "fri 6pm");
        assert_eq!(
            marked_due(line, at, now).unwrap().to_string(),
            "2026-11-13 18:00"
        );
        let line = "pay rent // nov 2";
        assert_eq!(&line[marked(line, 9)], "nov 2");
        for line in ["pay //", "pay // ", "pay // +home"] {
            assert_eq!(marked(line, 4), 6..6, "{line:?}");
            assert_eq!(marked_due(line, 4, now), None);
        }
        // Typing, it reads nothing until it reads.
        assert_eq!(marked_due("x //fr", 2, now), None);
    }

    #[test]
    fn the_line_e_fills_in_is_the_title_then_the_date_and_reads_back_as_the_task() {
        let now = date(2026, 11, 11).at(10, 30, 0, 0);
        for (title, due, line) in [
            (
                "plan the friday party",
                "2026-11-11",
                "plan the friday party //2026-11-11",
            ),
            ("pay rent", "2026-11-13 18:00", "pay rent //2026-11-13 6pm"),
            (
                "pay rent",
                "2026-11-13 18:30",
                "pay rent //2026-11-13 6:30pm",
            ),
            (
                "pay rent",
                "2026-11-13 00:05",
                "pay rent //2026-11-13 12:05am",
            ),
            // Late, or another year: the day named, as any other.
            ("pay rent", "2026-11-09", "pay rent //2026-11-09"),
            ("pay rent", "2027-01-03 09:00", "pay rent //2027-01-03 9am"),
            ("buy milk", "", "buy milk"),
            // No date, a date word in the title: `//` alone keeps it title.
            ("plan the friday party", "", "plan the friday party //"),
            ("fix the 9:60 clock", "", "fix the 9:60 clock //"),
            ("월세 내기", "2026-11-27", "월세 내기 //2026-11-27"),
            ("x -money", "2026-11-27", "x -money //2026-11-27"),
        ] {
            let task = Task {
                title: title.into(),
                due: (!due.is_empty()).then(|| due.parse().unwrap()),
                ..rent()
            };
            assert_eq!(edit_line(&task, now), (line.into(), title.len()), "{title}");
            let mut read = task.clone();
            super::edit(&mut read, &[line.into()], now).unwrap();
            assert_eq!(read, task, "{line}");
        }
    }

    #[test]
    fn slashes_alone_take_the_date_off_on_edit_only() {
        let dated = || Task {
            due: Some("2026-11-13 18:00".parse().unwrap()),
            ..rent()
        };
        for (words, title, tags) in [
            ("//", "pay rent", &["home"][..]),
            ("// +car -home", "pay rent", &["car"]),
            ("pay the rent //", "pay the rent", &["home"]),
            ("pay rent friday // !low", "pay rent friday", &["home"]),
        ] {
            let mut task = dated();
            edit(&mut task, words).unwrap();
            assert_eq!(task.due, None, "{words}");
            assert_eq!(task.title, title, "{words}");
            assert_eq!(task.tags, tags, "{words}");
        }
        // In the change: taken off, which isn't left alone.
        let now = date(2026, 11, 11).at(10, 30, 0, 0);
        let off = change(&["//".into()], now, |_| false, false).unwrap();
        assert_eq!(off.due, Some(None));
        assert_eq!(
            change(&["x".into()], now, |_| false, false).unwrap().due,
            None
        );
        // Two are refused still, empty or not.
        let mut task = dated();
        assert_eq!(
            edit(&mut task, "//fri //"),
            Err("`//fri` and `//` both give the date: give one".into())
        );
        assert_eq!(task, dated());
        // add refuses them, quoted as ever.
        assert_eq!(parse(&words("x //")), Err("`//` needs a date".into()));
        assert_eq!(parse(&words("//")), Err("`//` needs a date".into()));
    }

    #[test]
    fn a_repeat_is_part_of_the_date_inside_the_slashes() {
        let read = |s: &str| {
            let p = parse(&words(s)).unwrap();
            (
                p.title,
                p.due.unwrap().to_string(),
                p.every.map(|e| e.to_string()),
            )
        };
        let r = |title: &str, due: &str, every: &str| {
            (title.to_string(), due.to_string(), Some(every.to_string()))
        };
        // With a date: the first one; the time kept.
        assert_eq!(
            read("pay rent //dec 1 every 2 weeks +home"),
            r("pay rent", "2026-12-01", "2 weeks")
        );
        assert_eq!(
            read("standup //fri 6pm every week"),
            r("standup", "2026-11-13 18:00", "week")
        );
        assert_eq!(
            read("rent //jan 31 every month"),
            r("rent", "2027-01-31", "month on the 31st")
        );
        assert_eq!(
            read("rent //dec 1 monthly"),
            r("rent", "2026-12-01", "month on the 1st")
        );
        // Without one: the step's next match from today, today included.
        assert_eq!(read("water //every fri"), r("water", "2026-11-13", "week"));
        assert_eq!(read("water //every wed"), r("water", "2026-11-11", "week"));
        assert_eq!(read("water //every day"), r("water", "2026-11-11", "day"));
        assert_eq!(read("water //daily"), r("water", "2026-11-11", "day"));
        assert_eq!(
            read("rent //every month"),
            r("rent", "2026-11-11", "month on the 11th")
        );
        assert_eq!(
            read("tax //every year"),
            r("tax", "2026-11-11", "year on the 11th")
        );
        // A weekday step with a date: that weekday, on or after it.
        assert_eq!(read("x //dec 1 every fri"), r("x", "2026-12-04", "week"));
        // `every` outside the slashes is a word of the title.
        let p = parse(&words("read every page //fri")).unwrap();
        assert_eq!((p.title.as_str(), p.every), ("read every page", None));
        // Anything else after `every`: refused in the date's words.
        for (s, err) in [
            ("x //every fortnite", "`//every fortnite` isn't a date"),
            ("x //every", "`//every` isn't a date"),
            ("x //fri every 0 days", "`//fri every 0 days` isn't a date"),
            (
                "x //fridya every week",
                "`//fridya every week` isn't a date",
            ),
        ] {
            assert_eq!(parse(&words(s)), Err(err.into()), "{s}");
        }
    }

    #[test]
    fn edit_moves_the_date_and_keeps_the_step_changes_both_or_stops_it() {
        let weekly = || {
            let mut task = rent();
            task.due = Some("2026-11-13".parse().unwrap());
            task.every = Some("week".parse().unwrap());
            task
        };
        let mut task = weekly();
        edit(&mut task, "//sat").unwrap();
        assert_eq!(task.due.unwrap().to_string(), "2026-11-14");
        assert_eq!(task.every.unwrap().to_string(), "week", "kept");
        let mut task = weekly();
        edit(&mut task, "//sat every 2 weeks").unwrap();
        assert_eq!(task.every.unwrap().to_string(), "2 weeks");
        let mut task = weekly();
        edit(&mut task, "-every").unwrap();
        assert_eq!(
            (task.every, task.due.is_some()),
            (None, true),
            "stopped, the date kept"
        );
        let mut task = weekly();
        edit(&mut task, "//").unwrap();
        assert_eq!((task.every, task.due), (None, None), "no date, no repeat");
        // A moved month step aims at its new day.
        let mut task = rent();
        task.due = Some("2027-01-31".parse().unwrap());
        task.every = Some("month on the 31st".parse().unwrap());
        edit(&mut task, "//2027-02-05").unwrap();
        assert_eq!(task.every.unwrap().to_string(), "month on the 5th");
        // With no step: `-every` alone is refused, as a -tag it hasn't is;
        // among other words it's one of them.
        let mut task = rent();
        assert_eq!(edit(&mut task, "-every"), Err("1 doesn't repeat".into()));
        edit(&mut task, "pay -every").unwrap();
        assert_eq!(task.title, "pay -every");
    }

    #[test]
    fn the_line_e_fills_in_holds_the_repeat_and_reads_back_as_the_task() {
        let now = date(2026, 11, 11).at(10, 30, 0, 0);
        for (due, every, line) in [
            ("2026-11-13", "week", "pay rent //2026-11-13 every week"),
            (
                "2026-11-13T18:00",
                "2 weeks",
                "pay rent //2026-11-13 6pm every 2 weeks",
            ),
            (
                "2027-01-31",
                "month on the 31st",
                "pay rent //2027-01-31 every month",
            ),
            (
                "2027-02-28",
                "month on the 31st",
                "pay rent //2027-02-28 every month on the 31st",
            ),
        ] {
            let mut task = rent();
            task.due = Some(due.parse().unwrap());
            task.every = Some(every.parse().unwrap());
            assert_eq!(edit_line(&task, now).0, line);
            let mut back = task.clone();
            super::edit(&mut back, &[line.to_string()], now).unwrap();
            assert_eq!(back, task, "{line}");
        }
    }

    #[test]
    fn the_calendar_writes_only_the_dates_words() {
        for (text, date_words) in [
            ("x //fri every week +home", "fri"),
            ("x //fri 6pm daily", "fri 6pm"),
            ("x //every fri", ""),
            ("x //fri +home", "fri"),
        ] {
            let at = text.find("//").unwrap();
            assert_eq!(&text[marked_date(text, at)], date_words, "{text}");
        }
    }

    #[test]
    fn the_slashes_whose_words_hold_the_cursor() {
        let line = "pay rent //nov 13 6pm +home";
        // `p`..` `: none; between the slashes: none; right after them, in
        // their words and at their end: them; past the space after: none.
        for (cursor, holding) in [
            (0, None),
            (8, None),
            (9, None),
            (10, None),
            (11, Some(9)),
            (14, Some(9)),
            (21, Some(9)),
            (22, None),
            (line.len(), None),
        ] {
            assert_eq!(mark_holding(line, cursor), holding, "{cursor}");
        }
        assert_eq!(mark_holding("pay //", 6), Some(4));
        assert_eq!(mark_holding("//fri", 5), Some(0));
        // Slashes inside a word mark nothing, as `read` reads them.
        assert_eq!(mark_holding("see a//b", 8), None);
        assert_eq!(mark_holding("월세 //nov 2", "월세 //".len()), Some(7));
    }

    #[test]
    fn edit_with_nothing_to_change_is_an_error_and_changes_nothing() {
        let mut task = rent();
        assert!(edit(&mut task, " ").is_err());
        assert!(edit(&mut task, "x today tomorrow").is_err());
        assert_eq!(task, rent());
    }
}
