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
    change.apply(task);
    Ok(())
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

    for word in words.iter().flat_map(|w| w.split_whitespace()) {
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

    let (due, title) = date::read(&title, now)?;
    let parsed = Parsed {
        title: title.join(" "),
        tags,
        priority,
        due,
    };
    Ok((parsed, untags))
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
    fn edit_with_nothing_to_change_is_an_error_and_changes_nothing() {
        let mut task = rent();
        assert!(edit(&mut task, " ").is_err());
        assert!(edit(&mut task, "x today tomorrow").is_err());
        assert_eq!(task, rent());
    }
}
