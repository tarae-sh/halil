use std::ops::Range;

use jiff::civil::DateTime;

use crate::date::{self, Due};
use crate::task::{Priority, Task};

/// What `add` reads out of its words.
#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub title: String,
    pub tags: Vec<String>,
    pub priority: Option<Priority>,
    pub due: Option<Due>,
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
/// are read as ever.
pub fn parse(words: &[String], now: DateTime) -> Result<Parsed, String> {
    let (parsed, _) = read(words, now, |_| false)?;
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
    /// A priority or a date that replaces the task's own.
    pub priority: Option<Priority>,
    pub due: Option<Due>,
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
        task.due = self.due.or(task.due);
    }
}

/// Reads the words of `edit` as `parse` reads them, and `-tag` too, which takes
/// a tag off: it's read only when `has(tag)` says the task has it, else it
/// stays in the title.
pub fn change(
    words: &[String],
    now: DateTime,
    has: impl Fn(&str) -> bool,
) -> Result<Change, String> {
    let (p, remove) = read(words, now, has)?;
    Ok(Change {
        title: Some(p.title).filter(|t| !t.is_empty()),
        add: p.tags,
        remove,
        priority: p.priority,
        due: p.due,
    })
}

/// Changes `task` by the words of `edit` (see [`change`]): words left after the
/// date, tags and priority replace its title, `+tags` are added to its own and
/// `-tags` taken off, and a priority or a date replaces its own.
pub fn edit(task: &mut Task, words: &[String], now: DateTime) -> Result<(), String> {
    let change = change(words, now, |tag| task.tags.iter().any(|t| t == tag))?;
    if change == Change::default() {
        return Err(
            "nothing to change: give a new title, a date, +tags, -tags or a !priority".into(),
        );
    }
    refuse_missing_tags(task.id, &change)?;
    change.apply(task);
    Ok(())
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
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => return Ok(()),
    };
    Err(format!("{id} has no tag {named}"))
}

/// `parse`'s reading, the title allowed empty, and the `-tags` that `has`.
fn read(
    words: &[String],
    now: DateTime,
    has: impl Fn(&str) -> bool,
) -> Result<(Parsed, Vec<String>), String> {
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

    let (due, title) = match &marks[..] {
        [] => date::read(&title, now)?,
        [words] => (Some(mark_due(words, now)?), title),
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
    };
    Ok((parsed, untags))
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

/// The date the words after a `//` give, read as of `now`: all of them must
/// read as one date (`fri 6pm`, `nov 27`), else they're refused, quoted.
pub fn mark_due(words: &[&str], now: DateTime) -> Result<Due, String> {
    if words.is_empty() {
        return Err("`//` needs a date".into());
    }
    match date::read(words, now) {
        Ok((Some(due), rest)) if rest.is_empty() => Ok(due),
        _ => Err(format!("`//{}` isn't a date", words.join(" "))),
    }
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

/// The date the `//` at byte `at` of `text` marks, if its words read as one.
pub fn marked_due(text: &str, at: usize, now: DateTime) -> Option<Due> {
    let words: Vec<&str> = text[marked(text, at)].split_whitespace().collect();
    mark_due(&words, now).ok()
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
        }
    }

    /// `edit` as of Wednesday 2026-11-11, 10:30.
    fn edit(task: &mut Task, s: &str) -> Result<(), String> {
        super::edit(task, &words(s), date(2026, 11, 11).at(10, 30, 0, 0))
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
    fn edit_with_nothing_to_change_is_an_error_and_changes_nothing() {
        let mut task = rent();
        assert!(edit(&mut task, " ").is_err());
        assert!(edit(&mut task, "x today tomorrow").is_err());
        assert_eq!(task, rent());
    }
}
