//! The due date in a task's words: `friday 9am`, `in 3 days`, `nov 20` ...

use std::fmt;
use std::str::FromStr;

use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::fmt::temporal::{Pieces, TimeZoneAnnotationKind};
use jiff::tz::Offset;
use jiff::{Span, Zoned};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// When a task was done or made, as the clock read where it was: the day and
/// time there, the offset from UTC, and the zone's name when the system had
/// one. It's placed by the day written, never converted to the zone halil
/// runs in later, so travel or a clock change never moves it to another day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub at: DateTime,
    pub offset: Offset,
    pub zone: Option<String>,
}

impl Local {
    /// `now` as it's kept: to the second, with its zone's name if it has one.
    pub fn of(now: &Zoned) -> Local {
        let at = now.datetime();
        Local {
            at: at.with().subsec_nanosecond(0).build().unwrap_or(at),
            offset: now.offset(),
            zone: now.time_zone().iana_name().map(String::from),
        }
    }

    /// The day it was, where it was.
    pub fn date(&self) -> Date {
        self.at.date()
    }
}

impl fmt::Display for Local {
    /// `2026-11-25T18:04:00+09:00[Asia/Seoul]`, or without the name.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let pieces = Pieces::from(self.at).with_offset(self.offset);
        match &self.zone {
            Some(name) => write!(f, "{}", pieces.with_time_zone_name(name)),
            None => write!(f, "{pieces}"),
        }
    }
}

impl FromStr for Local {
    type Err = String;

    /// What `Display` writes. The day and time are taken as written, and the
    /// zone's name as a name: a zone this machine doesn't know is no error.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("`{s}` isn't a time with its offset, as 2026-11-25T18:04:00+09:00");
        let pieces = Pieces::parse(s).map_err(|_| bad())?;
        let (Some(time), Some(offset)) = (pieces.time(), pieces.to_numeric_offset()) else {
            return Err(bad());
        };
        let zone = match pieces.time_zone_annotation().map(|a| a.kind()) {
            Some(TimeZoneAnnotationKind::Named(name)) => Some(name.as_str().to_string()),
            _ => None,
        };
        Ok(Local {
            at: pieces.date().to_datetime(time),
            offset,
            zone,
        })
    }
}

impl Serialize for Local {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Local {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// How a task repeats: every `n` days, weeks, months or years. A month or
/// year step aims at a day of the month, its first date's, which it keeps:
/// `jan 31 every month` is feb 28, then mar 31, never drifting to the 28th.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Every {
    pub n: u16,
    pub unit: Unit,
    /// The day of the month a month or year step aims at; none for days and
    /// weeks.
    pub aim: Option<i8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    fn name(self) -> &'static str {
        match self {
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
            Unit::Year => "year",
        }
    }

    fn of(word: &str) -> Option<Unit> {
        Some(match word.strip_suffix('s').unwrap_or(word) {
            "day" => Unit::Day,
            "week" => Unit::Week,
            "month" => Unit::Month,
            "year" => Unit::Year,
            _ => return None,
        })
    }

    fn aims(self) -> bool {
        matches!(self, Unit::Month | Unit::Year)
    }
}

/// `1st`, `2nd`, `31st`.
fn ordinal(d: i8) -> String {
    let end = match (d % 10, d % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{d}{end}")
}

impl Every {
    /// `week`, `2 weeks`.
    fn step(&self) -> String {
        match self.n {
            1 => self.unit.name().to_string(),
            n => format!("{n} {}s", self.unit.name()),
        }
    }

    /// The words that read back in after a date `day`: `every week`, `every 2
    /// weeks`, with `on the 31st` when the step aims at another day than
    /// `day`'s.
    pub fn words(&self, day: Date) -> String {
        match self.aim.filter(|&aim| aim != day.day()) {
            Some(aim) => format!("every {} on the {}", self.step(), ordinal(aim)),
            None => format!("every {}", self.step()),
        }
    }

    /// A row's words for it: `daily`, `weekly`, `monthly`, `yearly`, else
    /// `every 2 weeks`.
    pub fn short(&self) -> String {
        match (self.n, self.unit) {
            (1, Unit::Day) => "daily".into(),
            (1, Unit::Week) => "weekly".into(),
            (1, Unit::Month) => "monthly".into(),
            (1, Unit::Year) => "yearly".into(),
            _ => format!("every {}", self.step()),
        }
    }

    /// The step for a task whose date moved to `day`: a month or year step
    /// aims at its day now.
    pub fn aimed_at(self, day: Date) -> Every {
        match self.unit.aims() {
            true => Every {
                aim: Some(day.day()),
                ..self
            },
            false => self,
        }
    }

    /// The day `k` steps on from `from`, a month or year step on its aimed
    /// day, or the month's last when it's shorter.
    fn nth(&self, from: Date, k: i64) -> Option<Date> {
        let n = i64::from(self.n) * k;
        match self.unit {
            Unit::Day => from.checked_add(Span::new().try_days(n).ok()?).ok(),
            Unit::Week => from.checked_add(Span::new().try_weeks(n).ok()?).ok(),
            Unit::Month | Unit::Year => {
                let months = match self.unit {
                    Unit::Month => n,
                    _ => n.checked_mul(12)?,
                };
                let first = from
                    .first_of_month()
                    .checked_add(Span::new().try_months(months).ok()?)
                    .ok()?;
                let aim = self.aim.unwrap_or(from.day());
                first
                    .with()
                    .day(aim.min(first.days_in_month()))
                    .build()
                    .ok()
            }
        }
    }

    /// The next date for a task due `due` and done on `done`: the first of
    /// due + a step, due + two steps, ... that's later than `done`. Done early,
    /// the one after its due date; done late, the coming one, never the ones
    /// missed. The time stays.
    pub fn next(&self, due: Due, done: Date) -> Option<Due> {
        let mut k = 1;
        loop {
            let date = self.nth(due.date, k)?;
            if date > done {
                return Some(Due { date, ..due });
            }
            k += 1;
        }
    }
}

impl fmt::Display for Every {
    /// As the tasks file keeps it: `week`, `2 weeks`, `month on the 31st`.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.step())?;
        if let Some(aim) = self.aim {
            write!(f, " on the {}", ordinal(aim))?;
        }
        Ok(())
    }
}

impl FromStr for Every {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("`{s}` isn't a repeat, as `week`, `2 weeks` or `month on the 31st`");
        let words: Vec<&str> = s.split_whitespace().collect();
        let (step, aim) = match words[..] {
            [.., "on", "the", d] => (
                &words[..words.len() - 3],
                Some(day_of_month(d).ok_or_else(bad)?),
            ),
            _ => (&words[..], None),
        };
        let (n, unit) = match step {
            [unit] => (1, Unit::of(unit).filter(|_| !unit.ends_with('s'))),
            [n, unit] => (n.parse::<u16>().map_err(|_| bad())?, Unit::of(unit)),
            _ => return Err(bad()),
        };
        let unit = unit.ok_or_else(bad)?;
        if n == 0 || unit.aims() != aim.is_some() || aim.is_some_and(|d| !(1..=31).contains(&d)) {
            return Err(bad());
        }
        Ok(Every { n, unit, aim })
    }
}

impl Serialize for Every {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Every {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A step read out of the words after `every` in a `//` mark, before its
/// first date is known.
pub struct StepRead {
    every: Every,
    /// `every fri`: weekly, on that day.
    weekday: Option<Weekday>,
}

/// The step the lowercased words `w` say: `every day`, `every week`, `every
/// month`, `every year`, `every 3 days` (weeks, months, years), `every fri`
/// (weekly, on fridays), `daily`, `weekly`, `monthly`, `yearly`; a month or
/// year step may end `on the 31st`. None for anything else.
pub fn read_step(w: &[&str]) -> Option<StepRead> {
    let (w, aim) = match w {
        [rest @ .., "on", "the", d] => (rest, Some(day_of_month(d)?)),
        _ => (w, None),
    };
    let plain = |n: u16, unit: Unit| {
        Some(StepRead {
            every: Every { n, unit, aim: None },
            weekday: None,
        })
    };
    let read = match w {
        ["daily"] => plain(1, Unit::Day),
        ["weekly"] => plain(1, Unit::Week),
        ["monthly"] => plain(1, Unit::Month),
        ["yearly"] => plain(1, Unit::Year),
        ["every", word] => match (
            Unit::of(word).filter(|_| !word.ends_with('s')),
            weekday(word),
        ) {
            (Some(unit), _) => plain(1, unit),
            (None, Some(wd)) => Some(StepRead {
                every: Every {
                    n: 1,
                    unit: Unit::Week,
                    aim: None,
                },
                weekday: Some(wd),
            }),
            _ => None,
        },
        ["every", n, unit] => {
            let n: u16 = n.parse().ok().filter(|&n| n > 0)?;
            plain(n, Unit::of(unit)?)
        }
        _ => None,
    }?;
    match aim {
        Some(d) if read.every.unit.aims() && (1..=31).contains(&d) => Some(StepRead {
            every: Every {
                aim: Some(d),
                ..read.every
            },
            ..read
        }),
        Some(_) => None,
        None => Some(read),
    }
}

impl StepRead {
    /// The first date when none is given: the step's next match counted from
    /// `today`, today included (`every fri`: the coming friday; else today).
    pub fn first(&self, today: Date) -> Due {
        Due {
            date: self.on_or_after(today),
            time: None,
        }
    }

    /// `day`, or for `every fri` the first friday on or after it.
    fn on_or_after(&self, day: Date) -> Date {
        match self.weekday {
            Some(wd) => {
                let ahead = (wd.to_monday_zero_offset() - day.weekday().to_monday_zero_offset())
                    .rem_euclid(7);
                day.checked_add(Span::new().days(ahead)).unwrap_or(day)
            }
            None => day,
        }
    }

    /// The step for a first date `due`, moved to the step's weekday when it
    /// has one; a month or year step aims at that date's day, unless it said
    /// its own.
    pub fn with_first(&self, due: Due) -> (Due, Every) {
        let due = Due {
            date: self.on_or_after(due.date),
            ..due
        };
        let aim = match self.every.unit.aims() {
            true => Some(self.every.aim.unwrap_or(due.date.day())),
            false => None,
        };
        (due, Every { aim, ..self.every })
    }
}

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
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
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
            let date = if t > now.time() {
                Ok(now.date())
            } else {
                now.date().tomorrow()
            };
            Some(Due {
                date: date.map_err(|e| format!("`{phrase}`: {e}"))?,
                time: Some(t),
            })
        }
    };
    Ok((due, rest))
}

/// `due` in words that name its day, which [`read`] reads back as it on any
/// day: `2026-11-27`, then the time as it's typed, `2026-11-27 6pm`. Never a
/// month and a day (`nov 27`): read after that day, they're next year's.
pub fn say_exact(due: Due) -> String {
    match due.time {
        None => due.date.to_string(),
        Some(t) => format!("{} {}", due.date, say_time(t)),
    }
}

/// A time as it's typed: `6pm`, `6:30pm`, `12am`.
fn say_time(t: Time) -> String {
    let hour = match t.hour() % 12 {
        0 => 12,
        h => h,
    };
    let half = if t.hour() < 12 { "am" } else { "pm" };
    match t.minute() {
        0 => format!("{hour}{half}"),
        m => format!("{hour}:{m:02}{half}"),
    }
}

/// One date phrase: a day, a time, or both.
enum Piece {
    Day(Date),
    Time(Time),
    Both(DateTime),
}

/// Fills `slot` with what `phrase` read, unless an earlier phrase did.
fn set<T>(
    slot: &mut Option<(T, String)>,
    value: T,
    phrase: &str,
    what: &str,
) -> Result<(), String> {
    if let Some((_, first)) = slot {
        return Err(format!(
            "`{first}` and `{phrase}` both give the {what}: give one"
        ));
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
        let ahead =
            (wd.to_monday_zero_offset() - today.weekday().to_monday_zero_offset()).rem_euclid(7);
        let date = today
            .checked_add(Span::new().days(ahead))
            .map_err(too_far(1))?;
        return Ok(Some((Piece::Day(date), 1)));
    }

    // next friday: the one in next week
    if first == "next"
        && let Some(wd) = second.and_then(weekday)
    {
        let ahead = 7 - today.weekday().to_monday_zero_offset() + wd.to_monday_zero_offset();
        let date = today
            .checked_add(Span::new().days(ahead))
            .map_err(too_far(2))?;
        return Ok(Some((Piece::Day(date), 2)));
    }

    // in 3 days, in a week, in 2 hours
    if first == "in"
        && let Some(n) = second.and_then(count)
        && let Some(&unit) = w.get(2)
    {
        let piece = match unit {
            "day" | "days" => Span::new()
                .try_days(n)
                .and_then(|s| today.checked_add(s))
                .map(Piece::Day),
            "week" | "weeks" => Span::new()
                .try_weeks(n)
                .and_then(|s| today.checked_add(s))
                .map(Piece::Day),
            "hour" | "hours" | "hr" | "hrs" => Span::new()
                .try_hours(n)
                .and_then(|s| now.checked_add(s))
                .map(Piece::Both),
            "minute" | "minutes" | "min" | "mins" => Span::new()
                .try_minutes(n)
                .and_then(|s| now.checked_add(s))
                .map(Piece::Both),
            _ => return Ok(None),
        };
        return Ok(Some((piece.map_err(too_far(3))?, 3)));
    }

    // 9am, 9 am, 9:30pm, 14:30
    if let Some((t, len)) = clock(first, second)? {
        return Ok(Some((Piece::Time(t), len)));
    }

    // dec 31 2026, december 31st, 2026, 31 dec 2026, 31st of december 2026,
    // 2026 dec 31: that day, in that year
    if let Some((y, m, d, len)) = with_year(w) {
        let date =
            Date::new(y, m, d).map_err(|_| format!("`{}` isn't a date", w[..len].join(" ")))?;
        return Ok(Some((Piece::Day(date), len)));
    }

    // nov 20, 20 nov, 20th of nov, dec 31,: the soonest, today included
    let bare = |i: usize| w.get(i).map(|s| s.strip_suffix(',').unwrap_or(s));
    let month_day = match (month(first), bare(1).and_then(day_of_month)) {
        (Some(m), Some(d)) => Some((m, d, 2)),
        _ => match (day_of_month(bare(0).unwrap_or(first)), bare(1), bare(2)) {
            (Some(d), Some("of"), Some(m)) if month(m).is_some() => month(m).map(|m| (m, d, 3)),
            (Some(d), Some(m), _) => month(m).map(|m| (m, d, 2)),
            _ => None,
        },
    };
    if let Some((m, d, len)) = month_day {
        let date = (today.year()..today.year().saturating_add(8))
            .filter_map(|y| Date::new(y, m, d).ok())
            .find(|&date| date >= today)
            .ok_or_else(|| format!("`{}` isn't a date", w[..len].join(" ")))?;
        return Ok(Some((Piece::Day(date), len)));
    }

    // 2026/12/31, 2026.12.31: year first is one way only; 12/31/2026 and
    // 31.12.2026 could be either, and are refused, in words
    if let Some(parts) = numeric_date(first) {
        return match parts {
            [y, m, d] if y.len() == 4 => {
                let n = |s: &str| s.parse::<i8>().ok();
                let date = y
                    .parse()
                    .ok()
                    .zip(n(m).zip(n(d)))
                    .and_then(|(y, (m, d))| Date::new(y, m, d).ok())
                    .ok_or_else(|| format!("`{first}` isn't a date"))?;
                Ok(Some((Piece::Day(date), 1)))
            }
            _ => Err(format!(
                "`{first}` could be the day or the month first: give the year first \
                 (2026-12-31) or the month's name (dec 31 2026)"
            )),
        };
    }

    // 2026-11-20, 2026-11-20t09:00
    let b = first.as_bytes();
    let iso_date = b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        });
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
        _ => Err(format!(
            "`{}` isn't a time",
            if len == 2 {
                format!("{first} {}", second.unwrap())
            } else {
                first.to_string()
            }
        )),
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

/// A day named with its year, as the lowercased words `w` start with: its
/// year, month and day, and how many words it takes. `dec 31 2026`, `dec 31,
/// 2026`, `31 dec 2026`, `31st of december 2026`, `2026 dec 31`; a comma after
/// any word but the last is let be.
fn with_year(w: &[&str]) -> Option<(i16, i8, i8, usize)> {
    let at = |i: usize| w.get(i).map(|s| s.strip_suffix(',').unwrap_or(s));
    let last = |i: usize| w.get(i).copied().and_then(year);
    let (a, b, c) = (at(0)?, at(1), at(2));
    if let (Some(m), Some(d), Some(y)) = (month(a), b.and_then(day_of_month), last(2)) {
        return Some((y, m, d, 3));
    }
    if let (Some(d), Some(m), Some(y)) = (day_of_month(a), b.and_then(month), last(2)) {
        return Some((y, m, d, 3));
    }
    if let (Some(d), Some("of"), Some(m), Some(y)) =
        (day_of_month(a), b, c.and_then(month), last(3))
    {
        return Some((y, m, d, 4));
    }
    if let (Some(y), Some(m), Some(d)) = (
        year(a),
        b.and_then(month),
        w.get(2).copied().and_then(day_of_month),
    ) {
        return Some((y, m, d, 3));
    }
    None
}

/// `2026`: a year, four digits.
fn year(s: &str) -> Option<i16> {
    (s.len() == 4 && s.bytes().all(|c| c.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// The three numbers of `12/31/2026`, `31.12.2026` or `2026/12/31`: a word of
/// digits in three parts split by `/` or `.`, one of them a year.
fn numeric_date(s: &str) -> Option<[&str; 3]> {
    let sep = if s.contains('/') { '/' } else { '.' };
    let parts: Vec<&str> = s.split(sep).collect();
    let [a, b, c] = parts[..] else {
        return None;
    };
    let digits = |p: &str, n: std::ops::RangeInclusive<usize>| {
        n.contains(&p.len()) && p.bytes().all(|c| c.is_ascii_digit())
    };
    let year_first = digits(a, 4..=4) && digits(b, 1..=2) && digits(c, 1..=2);
    let year_last = digits(a, 1..=2) && digits(b, 1..=2) && digits(c, 4..=4);
    (year_first || year_last).then_some([a, b, c])
}

/// `20`, `20th`, `1st`.
fn day_of_month(s: &str) -> Option<i8> {
    let d = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|end| s.strip_suffix(end))
        .unwrap_or(s);
    if (1..=2).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit()) {
        d.parse().ok()
    } else {
        None
    }
}

/// Whether `word` names a weekday (`fri`, `Friday`).
pub fn is_weekday(word: &str) -> bool {
    weekday(&word.to_lowercase()).is_some()
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
            // a day with its year: that day, gone or not
            ("dec 31 2026", "2026-12-31", ""),
            ("Dec 31, 2026", "2026-12-31", ""),
            ("december 31st, 2026", "2026-12-31", ""),
            ("31 dec 2026", "2026-12-31", ""),
            ("31st december, 2026", "2026-12-31", ""),
            ("31st of december 2026", "2026-12-31", ""),
            ("2026 dec 31", "2026-12-31", ""),
            ("2026 december 31st", "2026-12-31", ""),
            ("nov 1 2026", "2026-11-01", ""),
            ("jan 3 2030", "2030-01-03", ""),
            ("feb 29 2028", "2028-02-29", ""),
            ("2026/12/31", "2026-12-31", ""),
            ("2026.12.31", "2026-12-31", ""),
            ("dec 31 2026 6pm", "2026-12-31 18:00", ""),
            ("pay rent dec 31 2026", "2026-12-31", "pay rent"),
            (
                "pay rent on 31 dec 2026 by noon",
                "2026-12-31 12:00",
                "pay rent",
            ),
            // the same family, the year left out: the soonest
            ("20th of nov", "2026-11-20", ""),
            ("dec 31, 6pm", "2026-12-31 18:00", ""),
            ("nov 20 9am", "2026-11-20 09:00", ""),
            ("dec 31 26", "2026-12-31", "26"),
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
            (
                "9am call the bank tomorrow",
                "2026-11-12 09:00",
                "call the bank",
            ),
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
            assert_eq!(
                (got_due.as_str(), got_rest.as_str()),
                (due, rest),
                "{words:?}"
            );
        }
    }

    #[test]
    fn every_day_written_with_its_year_reads_as_that_day() {
        // Each day of four years, a leap year among them, in each form, its
        // month named long and short, its day plain and with its ending.
        let mut day = date(2025, 1, 1);
        let mut cases = 0;
        while day.year() <= 2028 {
            let (y, d) = (day.year(), day.day());
            let long = day.strftime("%B").to_string();
            let short = day.strftime("%b").to_string();
            let nth = match d {
                1 | 21 | 31 => "st",
                2 | 22 => "nd",
                3 | 23 => "rd",
                _ => "th",
            };
            for m in [&long, &short] {
                for form in [
                    format!("{m} {d} {y}"),
                    format!("{m} {d}, {y}"),
                    format!("{m} {d}{nth}, {y}"),
                    format!("{d} {m} {y}"),
                    format!("{d}{nth} {m}, {y}"),
                    format!("{d}{nth} of {m} {y}"),
                    format!("{y} {m} {d}"),
                ] {
                    let (due, rest) = read_str(&form).unwrap_or_else(|e| panic!("{form}: {e}"));
                    assert_eq!(due.map(|d| d.date), Some(day), "{form}");
                    assert_eq!(rest, "", "{form}");
                    cases += 1;
                }
            }
            let slashes = format!("{y}/{}/{d}", day.month());
            assert_eq!(read_str(&slashes).unwrap().0.map(|d| d.date), Some(day));
            day = day.tomorrow().unwrap();
        }
        assert_eq!(cases, 1461 * 14);
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
            ("feb 30 2027", "`feb 30 2027` isn't a date"),
            ("feb 29 2027", "`feb 29 2027` isn't a date"),
            ("31st of june 2027", "`31st of june 2027` isn't a date"),
            ("2027 feb 30", "`2027 feb 30` isn't a date"),
            ("2027/02/30", "`2027/02/30` isn't a date"),
            (
                "12/31/2026",
                "`12/31/2026` could be the day or the month first",
            ),
            (
                "31.12.2026",
                "`31.12.2026` could be the day or the month first",
            ),
            (
                "03/04/2026",
                "the year first (2026-12-31) or the month's name",
            ),
            ("dec 31 2026 jan 1 2027", "both give the day"),
        ];
        for (words, want) in table {
            match read_str(words) {
                Ok(got) => panic!("{words:?}: read as {got:?}"),
                Err(e) => assert!(e.contains(want), "{words:?}: {e}"),
            }
        }
    }

    #[test]
    fn exact_words_read_back_as_the_same_day_on_any_day() {
        let t = |h, m| Some(Time::new(h, m, 0, 0).unwrap());
        for (d, time, want) in [
            (date(2026, 11, 25), None, "2026-11-25"),
            (date(2026, 11, 25), t(18, 0), "2026-11-25 6pm"),
            (date(2026, 11, 25), t(0, 5), "2026-11-25 12:05am"),
            (date(2026, 11, 13), t(12, 0), "2026-11-13 12pm"),
            (date(2028, 2, 29), t(12, 30), "2028-02-29 12:30pm"),
            (date(2027, 1, 3), t(9, 30), "2027-01-03 9:30am"),
            (date(2025, 12, 1), None, "2025-12-01"),
        ] {
            let due = Due { date: d, time };
            let said = say_exact(due);
            assert_eq!(said, want);
            let words: Vec<&str> = said.split(' ').collect();
            // Read the day before, on it, after it, and a year on.
            for on in [
                date(2026, 11, 24),
                d,
                date(2026, 11, 26),
                date(2027, 11, 25),
            ] {
                let (read, rest) = read(&words, on.at(23, 59, 0, 0)).unwrap();
                assert_eq!((read, rest.len()), (Some(due), 0), "{said} on {on}");
            }
        }
    }

    fn step(s: &str) -> Option<StepRead> {
        let w: Vec<&str> = s.split_whitespace().collect();
        read_step(&w)
    }

    #[test]
    fn each_step_form_reads_and_anything_else_doesnt() {
        let e = |n, unit, aim| Every { n, unit, aim };
        for (words, want, weekday) in [
            ("every day", e(1, Unit::Day, None), None),
            ("every week", e(1, Unit::Week, None), None),
            ("every month", e(1, Unit::Month, None), None),
            ("every year", e(1, Unit::Year, None), None),
            ("every 3 days", e(3, Unit::Day, None), None),
            ("every 2 weeks", e(2, Unit::Week, None), None),
            ("every 2 months", e(2, Unit::Month, None), None),
            ("every 10 years", e(10, Unit::Year, None), None),
            ("every 1 week", e(1, Unit::Week, None), None),
            ("every fri", e(1, Unit::Week, None), Some(Weekday::Friday)),
            (
                "every monday",
                e(1, Unit::Week, None),
                Some(Weekday::Monday),
            ),
            ("daily", e(1, Unit::Day, None), None),
            ("weekly", e(1, Unit::Week, None), None),
            ("monthly", e(1, Unit::Month, None), None),
            ("yearly", e(1, Unit::Year, None), None),
            ("every month on the 31st", e(1, Unit::Month, Some(31)), None),
            ("monthly on the 1st", e(1, Unit::Month, Some(1)), None),
        ] {
            let read = step(words).unwrap_or_else(|| panic!("{words}"));
            assert_eq!((read.every, read.weekday), (want, weekday), "{words}");
        }
        for words in [
            "every",
            "every fortnite",
            "every 0 days",
            "every days",
            "every 2 week fri",
            "every week on the 3rd",
            "every month on the 32nd",
            "weekly every",
            "every other week",
        ] {
            assert!(step(words).is_none(), "{words}");
        }
    }

    #[test]
    fn a_step_in_the_file_is_written_read_and_written_the_same() {
        let mut seen = 0;
        for n in [1, 2, 3, 12] {
            for unit in [Unit::Day, Unit::Week, Unit::Month, Unit::Year] {
                let aims: Vec<Option<i8>> = match unit.aims() {
                    true => (1..=31).map(Some).collect(),
                    false => vec![None],
                };
                for aim in aims {
                    let every = Every { n, unit, aim };
                    let written = every.to_string();
                    let read: Every = written.parse().unwrap_or_else(|e| panic!("{written}: {e}"));
                    assert_eq!(read, every, "{written}");
                    assert_eq!(read.to_string(), written);
                    let json = serde_json::to_string(&every).unwrap();
                    assert_eq!(serde_json::from_str::<Every>(&json).unwrap(), every);
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 4 * (2 + 2 * 31));
        assert_eq!(
            Every {
                n: 2,
                unit: Unit::Week,
                aim: None
            }
            .to_string(),
            "2 weeks"
        );
        assert_eq!(
            Every {
                n: 1,
                unit: Unit::Month,
                aim: Some(31)
            }
            .to_string(),
            "month on the 31st"
        );
        for bad in [
            "fortnight",
            "0 weeks",
            "weeks",
            "month",
            "week on the 3rd",
            "2",
        ] {
            assert!(bad.parse::<Every>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_month_or_year_step_keeps_its_aimed_day_through_short_months() {
        let monthly = Every {
            n: 1,
            unit: Unit::Month,
            aim: Some(31),
        };
        let from = date(2027, 1, 31);
        let months: Vec<Date> = (1..=4).map(|k| monthly.nth(from, k).unwrap()).collect();
        assert_eq!(
            months,
            [
                date(2027, 2, 28),
                date(2027, 3, 31),
                date(2027, 4, 30),
                date(2027, 5, 31)
            ]
        );
        // From the clamped day, still the 31st after.
        assert_eq!(monthly.nth(date(2027, 2, 28), 1), Some(date(2027, 3, 31)));
        assert_eq!(
            monthly.nth(date(2028, 1, 31), 1),
            Some(date(2028, 2, 29)),
            "a leap year"
        );
        let yearly = Every {
            n: 1,
            unit: Unit::Year,
            aim: Some(29),
        };
        let years: Vec<Date> = (1..=4)
            .map(|k| yearly.nth(date(2028, 2, 29), k).unwrap())
            .collect();
        assert_eq!(
            years,
            [
                date(2029, 2, 28),
                date(2030, 2, 28),
                date(2031, 2, 28),
                date(2032, 2, 29)
            ]
        );
        let fortnightly = Every {
            n: 2,
            unit: Unit::Week,
            aim: None,
        };
        assert_eq!(
            fortnightly.nth(date(2026, 12, 1), 1),
            Some(date(2026, 12, 15))
        );
        // A moved date aims at its day now.
        assert_eq!(monthly.aimed_at(date(2027, 3, 5)).aim, Some(5));
        assert_eq!(fortnightly.aimed_at(date(2027, 3, 5)), fortnightly);
    }

    #[test]
    fn the_next_date_is_the_first_step_after_the_day_it_was_done() {
        let weekly = Every {
            n: 1,
            unit: Unit::Week,
            aim: None,
        };
        let due = Due {
            date: date(2026, 11, 13),
            time: Some(Time::new(18, 0, 0, 0).unwrap()),
        };
        let next = |done| weekly.next(due, done).unwrap();
        // Done early: the one after its due date.
        assert_eq!(next(date(2026, 11, 10)).date, date(2026, 11, 20));
        // On the day.
        assert_eq!(next(date(2026, 11, 13)).date, date(2026, 11, 20));
        // Three weeks late: the coming one, not the three missed.
        assert_eq!(next(date(2026, 12, 3)).date, date(2026, 12, 4));
        assert_eq!(next(date(2026, 12, 4)).date, date(2026, 12, 11));
        // The time is kept on each one.
        assert_eq!(next(date(2026, 12, 3)).time, due.time);
        let monthly = Every {
            n: 1,
            unit: Unit::Month,
            aim: Some(31),
        };
        let jan = Due {
            date: date(2027, 1, 31),
            time: None,
        };
        assert_eq!(
            monthly.next(jan, date(2027, 1, 31)).unwrap().date,
            date(2027, 2, 28)
        );
        assert_eq!(
            monthly.next(jan, date(2027, 3, 1)).unwrap().date,
            date(2027, 3, 31)
        );
    }

    #[test]
    fn a_step_says_its_words() {
        let e = |n, unit, aim| Every { n, unit, aim };
        assert_eq!(
            e(1, Unit::Week, None).words(date(2026, 11, 13)),
            "every week"
        );
        assert_eq!(e(2, Unit::Week, None).short(), "every 2 weeks");
        assert_eq!(e(1, Unit::Day, None).short(), "daily");
        assert_eq!(e(1, Unit::Year, Some(29)).short(), "yearly");
        // The aim said only where the date's day isn't it.
        let monthly = e(1, Unit::Month, Some(31));
        assert_eq!(monthly.words(date(2027, 1, 31)), "every month");
        assert_eq!(monthly.words(date(2027, 2, 28)), "every month on the 31st");
    }

    #[test]
    fn due_round_trips_through_the_file() {
        for s in ["2026-11-20", "2026-11-20T09:00"] {
            let json = format!("\"{s}\"");
            let due: Due = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&due).unwrap(), json);
        }
        assert_eq!(
            "2026-11-20 09:00".parse::<Due>().unwrap().to_string(),
            "2026-11-20 09:00"
        );
        assert!("2026-11-31".parse::<Due>().is_err());
    }
}
