//! The due date in a task's words: `friday 9am`, `in 3 days`, `nov 20` ...

use std::fmt;
use std::str::FromStr;

use jiff::Span;
use jiff::civil::{Date, DateTime, Time, Weekday};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// When a task is due: a day, and maybe a time on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Due {
    pub date: Date,
    pub time: Option<Time>,
}

impl fmt::Display for Due {
    /// `2026-11-20`, or `2026-11-20 09:00`.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.date)?;
        if let Some(t) = self.time {
            write!(f, " {}", t.strftime("%H:%M"))?;
        }
        Ok(())
    }
}

impl FromStr for Due {
    type Err = String;

    /// What `Display` writes, with a space or a `T` between the day and the time.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = |e| format!("`{s}` isn't a due date: {e}");
        match s.split_once([' ', 'T']) {
            None => Ok(Due {
                date: s.parse().map_err(bad)?,
                time: None,
            }),
            Some((date, time)) => Ok(Due {
                date: date.parse().map_err(bad)?,
                time: Some(time.parse().map_err(bad)?),
            }),
        }
    }
}

/// Stored as `2026-11-20` or `2026-11-20T09:00`.
impl Serialize for Due {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.time {
            None => s.collect_str(&self.date),
            Some(t) => s.collect_str(&format_args!("{}T{}", self.date, t.strftime("%H:%M"))),
        }
    }
}

impl<'de> Deserialize<'de> for Due {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

/// Reads a due date out of `words`, as of `now`; gives it and the words left.
///
/// - a day: `today`, `tomorrow`, a weekday (`friday`, `fri`: the soonest one,
///   today included), `next friday` (the friday of next week, weeks starting on
///   monday), `in 3 days`, `in a week`, `nov 20`, `20 nov`, `november 20th`
///   (the soonest one, today included), `2026-11-20`
/// - a time: `9am`, `9 pm`, `9:30am`, `14:30`, `noon`, `eod` (17:00)
/// - both: `in 2 hours`, `2026-11-20T09:00`
///
/// A day and a time may come apart, in either order. A time alone is the next
/// time it comes: today, or tomorrow once it's past. `at`, `by`, `on` or `due`
/// right before a date goes with it. Case doesn't matter. Two days or two
/// times is an error, and so is a date that doesn't exist (`feb 30`, `25:00`).
pub fn read<'a>(words: &[&'a str], now: DateTime) -> Result<(Option<Due>, Vec<&'a str>), String> {
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    let mut day: Option<(Date, String)> = None;
    let mut time: Option<(Time, String)> = None;
    let mut rest = Vec::new();

    let mut i = 0;
    while i < words.len() {
        let lead = usize::from(matches!(lower[i], "at" | "by" | "on" | "due"));
        let start = i + lead;
        let Some((piece, len)) = piece(&lower[start..], now)? else {
            rest.push(words[i]);
            i += 1;
            continue;
        };
        let phrase = lower[start..start + len].join(" ");
        match piece {
            Piece::Day(d) => set(&mut day, d, &phrase, "day")?,
            Piece::Time(t) => set(&mut time, t, &phrase, "time")?,
            Piece::Both(dt) => {
                set(&mut day, dt.date(), &phrase, "day")?;
                set(&mut time, hm(dt.hour(), dt.minute())?, &phrase, "time")?;
            }
        }
        i = start + len;
    }

    let due = match (day, time) {
        (None, None) => None,
        (Some((date, _)), time) => Some(Due {
            date,
            time: time.map(|(t, _)| t),
        }),
        (None, Some((t, phrase))) => {
            let date = if t > now.time() { Ok(now.date()) } else { now.date().tomorrow() };
            Some(Due {
                date: date.map_err(|e| format!("`{phrase}`: {e}"))?,
                time: Some(t),
            })
        }
    };
    Ok((due, rest))
}

/// One date phrase: a day, a time, or both.
enum Piece {
    Day(Date),
    Time(Time),
    Both(DateTime),
}

/// Fills `slot` with what `phrase` read, unless an earlier phrase did.
fn set<T>(slot: &mut Option<(T, String)>, value: T, phrase: &str, what: &str) -> Result<(), String> {
    if let Some((_, first)) = slot {
        return Err(format!("`{first}` and `{phrase}` both give the {what}: give one"));
    }
    *slot = Some((value, phrase.to_string()));
    Ok(())
}

/// The date phrase the lowercased words `w` start with, and how many words it
/// takes; none if they don't start with one, an error if it isn't a real date.
fn piece(w: &[&str], now: DateTime) -> Result<Option<(Piece, usize)>, String> {
    let today = now.date();
    let Some(&first) = w.first() else {
        return Ok(None);
    };
    let second = w.get(1).copied();
    let too_far = |n: usize| {
        let phrase = w[..n].join(" ");
        move |e: jiff::Error| format!("`{phrase}` is too far off: {e}")
    };

    match first {
        "today" => return Ok(Some((Piece::Day(today), 1))),
        "tomorrow" => return Ok(Some((Piece::Day(today.tomorrow().map_err(too_far(1))?), 1))),
        "noon" => return Ok(Some((Piece::Time(hm(12, 0)?), 1))),
        "eod" => return Ok(Some((Piece::Time(hm(17, 0)?), 1))),
        _ => {}
    }

    // friday: the soonest, today included
    if let Some(wd) = weekday(first) {
        let ahead = (wd.to_monday_zero_offset() - today.weekday().to_monday_zero_offset()).rem_euclid(7);
        let date = today.checked_add(Span::new().days(ahead)).map_err(too_far(1))?;
        return Ok(Some((Piece::Day(date), 1)));
    }

    // next friday: the one in next week
    if first == "next"
        && let Some(wd) = second.and_then(weekday)
    {
        let ahead = 7 - today.weekday().to_monday_zero_offset() + wd.to_monday_zero_offset();
        let date = today.checked_add(Span::new().days(ahead)).map_err(too_far(2))?;
        return Ok(Some((Piece::Day(date), 2)));
    }

    // in 3 days, in a week, in 2 hours
    if first == "in"
        && let Some(n) = second.and_then(count)
        && let Some(&unit) = w.get(2)
    {
        let piece = match unit {
            "day" | "days" => Span::new().try_days(n).and_then(|s| today.checked_add(s)).map(Piece::Day),
            "week" | "weeks" => Span::new().try_weeks(n).and_then(|s| today.checked_add(s)).map(Piece::Day),
            "hour" | "hours" | "hr" | "hrs" => {
                Span::new().try_hours(n).and_then(|s| now.checked_add(s)).map(Piece::Both)
            }
            "minute" | "minutes" | "min" | "mins" => {
                Span::new().try_minutes(n).and_then(|s| now.checked_add(s)).map(Piece::Both)
            }
            _ => return Ok(None),
        };
        return Ok(Some((piece.map_err(too_far(3))?, 3)));
    }

    // 9am, 9 am, 9:30pm, 14:30
    if let Some((t, len)) = clock(first, second)? {
        return Ok(Some((Piece::Time(t), len)));
    }

    // nov 20, 20 nov: the soonest, today included
    let month_day = match (month(first), second.and_then(day_of_month)) {
        (Some(m), Some(d)) => Some((m, d)),
        _ => day_of_month(first).zip(second.and_then(month)).map(|(d, m)| (m, d)),
    };
    if let Some((m, d)) = month_day {
        let date = (today.year()..today.year().saturating_add(8))
            .filter_map(|y| Date::new(y, m, d).ok())
            .find(|&date| date >= today)
            .ok_or_else(|| format!("`{} {}` isn't a date", first, second.unwrap_or_default()))?;
        return Ok(Some((Piece::Day(date), 2)));
    }

    // 2026-11-20, 2026-11-20t09:00
    let b = first.as_bytes();
    let iso_date = b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    if iso_date {
        let bad = |e| format!("`{first}` isn't a date: {e}");
        if b.len() == 10 {
            return Ok(Some((Piece::Day(first.parse().map_err(bad)?), 1)));
        }
        if b[10] == b't' {
            let dt: DateTime = first.to_uppercase().parse().map_err(bad)?;
            return Ok(Some((Piece::Both(dt), 1)));
        }
    }
    Ok(None)
}

/// A time on the clock, minutes and no finer.
fn hm(hour: i8, minute: i8) -> Result<Time, String> {
    Time::new(hour, minute, 0, 0).map_err(|e| e.to_string())
}

/// `9am`, `9:30pm`, `9` then `am` (two words), `14:30`; none if it isn't one
/// of those shapes, an error if it is but isn't a time (`13pm`, `25:00`).
fn clock(first: &str, second: Option<&str>) -> Result<Option<(Time, usize)>, String> {
    let (digits, pm, len) = if let Some(d) = first.strip_suffix("am") {
        (d, Some(false), 1)
    } else if let Some(d) = first.strip_suffix("pm") {
        (d, Some(true), 1)
    } else if let Some(half @ ("am" | "pm")) = second {
        (first, Some(half == "pm"), 2)
    } else if first.contains(':') {
        (first, None, 1)
    } else {
        return Ok(None);
    };

    let (h, m) = digits.split_once(':').unwrap_or((digits, "00"));
    let all_digits = |s: &str| s.bytes().all(|c| c.is_ascii_digit());
    if !(1..=2).contains(&h.len()) || m.len() != 2 || !all_digits(h) || !all_digits(m) {
        return Ok(None);
    }
    let (h, m): (i8, i8) = (h.parse().unwrap(), m.parse().unwrap());

    let hour = match pm {
        None if h <= 23 => Some(h),
        Some(pm) if (1..=12).contains(&h) => Some(h % 12 + if pm { 12 } else { 0 }),
        _ => None,
    };
    match hour {
        Some(hour) if m <= 59 => Ok(Some((hm(hour, m)?, len))),
        _ => Err(format!("`{}` isn't a time", if len == 2 { format!("{first} {}", second.unwrap()) } else { first.to_string() })),
    }
}

/// `3`, or `a` / `an` for one.
fn count(s: &str) -> Option<i64> {
    match s {
        "a" | "an" => Some(1),
        _ if !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) => s.parse().ok(),
        _ => None,
    }
}

/// `20`, `20th`, `1st`.
fn day_of_month(s: &str) -> Option<i8> {
    let d = ["st", "nd", "rd", "th"].iter().find_map(|end| s.strip_suffix(end)).unwrap_or(s);
    if (1..=2).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit()) {
        d.parse().ok()
    } else {
        None
    }
}

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "monday" | "mon" => Weekday::Monday,
        "tuesday" | "tue" | "tues" => Weekday::Tuesday,
        "wednesday" | "wed" => Weekday::Wednesday,
        "thursday" | "thu" | "thur" | "thurs" => Weekday::Thursday,
        "friday" | "fri" => Weekday::Friday,
        "saturday" | "sat" => Weekday::Saturday,
        "sunday" | "sun" => Weekday::Sunday,
        _ => return None,
    })
}

/// The month's number, 1 to 12.
fn month(s: &str) -> Option<i8> {
    Some(match s {
        "january" | "jan" => 1,
        "february" | "feb" => 2,
        "march" | "mar" => 3,
        "april" | "apr" => 4,
        "may" => 5,
        "june" | "jun" => 6,
        "july" | "jul" => 7,
        "august" | "aug" => 8,
        "september" | "sep" | "sept" => 9,
        "october" | "oct" => 10,
        "november" | "nov" => 11,
        "december" | "dec" => 12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    /// Wednesday 2026-11-11, 10:30.
    fn now() -> DateTime {
        date(2026, 11, 11).at(10, 30, 0, 0)
    }

    fn read_str(s: &str) -> Result<(Option<Due>, String), String> {
        let words: Vec<&str> = s.split_whitespace().collect();
        read(&words, now()).map(|(due, rest)| (due, rest.join(" ")))
    }

    #[test]
    fn reads_dates() {
        // words, the due date read ("": none), the words left
        let table = [
            // days
            ("today", "2026-11-11", ""),
            ("tomorrow", "2026-11-12", ""),
            ("Tomorrow", "2026-11-12", ""),
            ("friday", "2026-11-13", ""),
            ("FRIDAY", "2026-11-13", ""),
            ("fri", "2026-11-13", ""),
            ("sunday", "2026-11-15", ""),
            ("monday", "2026-11-16", ""),
            ("tuesday", "2026-11-17", ""),
            ("wednesday", "2026-11-11", ""),
            ("next monday", "2026-11-16", ""),
            ("next wednesday", "2026-11-18", ""),
            ("next friday", "2026-11-20", ""),
            ("next sunday", "2026-11-22", ""),
            ("in 1 day", "2026-11-12", ""),
            ("in 3 days", "2026-11-14", ""),
            ("in a week", "2026-11-18", ""),
            ("in 2 weeks", "2026-11-25", ""),
            ("nov 20", "2026-11-20", ""),
            ("20 nov", "2026-11-20", ""),
            ("November 20th", "2026-11-20", ""),
            ("nov 11", "2026-11-11", ""),
            ("nov 1", "2027-11-01", ""),
            ("jan 3rd", "2027-01-03", ""),
            ("feb 29", "2028-02-29", ""),
            ("2026-12-01", "2026-12-01", ""),
            ("2025-01-01", "2025-01-01", ""),
            // times
            ("11am", "2026-11-11 11:00", ""),
            ("9am", "2026-11-12 09:00", ""),
            ("9 am", "2026-11-12 09:00", ""),
            ("9:15pm", "2026-11-11 21:15", ""),
            ("9:15PM", "2026-11-11 21:15", ""),
            ("12pm", "2026-11-11 12:00", ""),
            ("12am", "2026-11-12 00:00", ""),
            ("14:30", "2026-11-11 14:30", ""),
            ("9:05", "2026-11-12 09:05", ""),
            ("10:30", "2026-11-12 10:30", ""),
            ("noon", "2026-11-11 12:00", ""),
            ("eod", "2026-11-11 17:00", ""),
            // both
            ("in 2 hours", "2026-11-11 12:30", ""),
            ("in 1 hour", "2026-11-11 11:30", ""),
            ("in 90 minutes", "2026-11-11 12:00", ""),
            ("in 14 hours", "2026-11-12 00:30", ""),
            ("2026-12-01T09:00", "2026-12-01 09:00", ""),
            ("today 9am", "2026-11-11 09:00", ""),
            ("tomorrow eod", "2026-11-12 17:00", ""),
            ("friday 9am", "2026-11-13 09:00", ""),
            ("9am friday", "2026-11-13 09:00", ""),
            ("next monday 14:30", "2026-11-16 14:30", ""),
            ("dec 25 6pm", "2026-12-25 18:00", ""),
            ("2027-01-15 14:30", "2027-01-15 14:30", ""),
            // with a title
            ("pay rent friday 9am", "2026-11-13 09:00", "pay rent"),
            ("9am call the bank tomorrow", "2026-11-12 09:00", "call the bank"),
            ("call mum by tomorrow", "2026-11-12", "call mum"),
            ("standup at 9am on friday", "2026-11-13 09:00", "standup"),
            ("Report due nov 20", "2026-11-20", "Report"),
            ("water plants in 3 days", "2026-11-14", "water plants"),
            // no date
            ("buy milk", "", "buy milk"),
            ("read chapter 3", "", "read chapter 3"),
            ("may the force", "", "may the force"),
            ("in the garden", "", "in the garden"),
            ("in 3 parts", "", "in 3 parts"),
            ("meet at the cafe", "", "meet at the cafe"),
            ("next steps", "", "next steps"),
            ("score 3:2", "", "score 3:2"),
            ("buy 2 eggs", "", "buy 2 eggs"),
            ("", "", ""),
        ];
        for (words, due, rest) in table {
            let (got_due, got_rest) = read_str(words).unwrap_or_else(|e| panic!("{words:?}: {e}"));
            let got_due = got_due.map(|d| d.to_string()).unwrap_or_default();
            assert_eq!((got_due.as_str(), got_rest.as_str()), (due, rest), "{words:?}");
        }
    }

    #[test]
    fn rejects_bad_dates() {
        // words, a piece of the error
        let table = [
            ("today tomorrow", "`today`"),
            ("friday in 3 days", "`in 3 days`"),
            ("9am 5pm", "`5pm`"),
            ("eod at noon", "`noon`"),
            ("in 2 hours friday", "`friday`"),
            ("feb 30", "feb 30"),
            ("2026-02-30", "2026-02-30"),
            ("2026-13-01", "2026-13-01"),
            ("25:00", "25:00"),
            ("9:60", "9:60"),
            ("13pm", "13pm"),
            ("0am", "0am"),
        ];
        for (words, want) in table {
            match read_str(words) {
                Ok(got) => panic!("{words:?}: read as {got:?}"),
                Err(e) => assert!(e.contains(want), "{words:?}: {e}"),
            }
        }
    }

    #[test]
    fn due_round_trips_through_the_file() {
        for s in ["2026-11-20", "2026-11-20T09:00"] {
            let json = format!("\"{s}\"");
            let due: Due = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&due).unwrap(), json);
        }
        assert_eq!("2026-11-20 09:00".parse::<Due>().unwrap().to_string(), "2026-11-20 09:00");
        assert!("2026-11-31".parse::<Due>().is_err());
    }
}
