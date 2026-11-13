use jiff::civil::DateTime;

use crate::date::{self, Due};
use crate::task::Priority;

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
    let mut title = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    let mut priority = None;

    for word in words.iter().flat_map(|w| w.split_whitespace()) {
        if let Some(tag) = word.strip_prefix('+').filter(|t| !t.is_empty()) {
            let tag = tag.to_lowercase();
            if !tags.contains(&tag) {
                tags.push(tag);
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
    if title.is_empty() {
        return Err("a task needs a title, not just a date, tags and a priority".into());
    }
    Ok(Parsed {
        title: title.join(" "),
        tags,
        priority,
        due,
    })
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
}
