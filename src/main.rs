mod board;
mod cal;
mod clip;
mod date;
mod filter;
mod hangul;
mod list;
mod parse;
mod store;
mod task;

use std::env;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use jiff::Zoned;

use date::Local;
use task::{Status, Task};

/// A small task manager for the terminal.
///
/// Run with no command to list what's due first.
#[derive(Parser)]
#[command(name = "halil", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a task; its date, +tags and !priority are read from the words
    #[command(after_help = "Example: halil add \"pay rent friday 9am +home !high\"")]
    Add {
        /// The task in plain English: a date (or //fri 6pm), +tags, !high, !med or !low
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// List open tasks, what's due first (the default); words, +tags, !priority
    /// and //fri, //by fri or //late narrow it
    #[command(after_help = "Example: halil list +home //by fri")]
    List {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        filter: Vec<String>,
    },
    /// Mark a task as started
    Start { id: u32 },
    /// Mark a task as done, and its open sub items
    Done { id: u32 },
    /// Change a task: new words replace its title; a date, +tags, -tags and !priority update it
    Edit {
        id: u32,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Remove a task, and its sub items
    Rm { id: u32 },
    /// Add a sub item under a task, with the words of `add`
    #[command(after_help = "Example: halil sub 3 write the tests friday")]
    Sub {
        id: u32,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Open the board: Todo, Doing, Done; a filter as list's, on
    Board {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        filter: Vec<String>,
    },
    /// Open the board's calendar: each day's due and done, and what's on it
    Cal {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        filter: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Command::List { filter: vec![] })) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("halil: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), String> {
    run_at(&store::default_path()?, command)
}

/// Runs `command` on the tasks file at `path`. A change is read, made and
/// written under the file's lock (see [`store::update`]), so commands run at
/// once each see the other's change.
fn run_at(path: &Path, command: Command) -> Result<(), String> {
    let now = jiff::Zoned::now();
    let read = |words: &[String]| filter::read(&words.join(" "), now.datetime());
    match command {
        Command::List { filter } => {
            let color = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
            let filter = read(&filter)?;
            print!(
                "{}",
                list::render(&store::load(path)?, now.datetime(), color, &filter)
            );
        }
        Command::Board { filter } => board::run(path, store::load(path)?, false, read(&filter)?)?,
        Command::Cal { filter } => board::run(path, store::load(path)?, true, read(&filter)?)?,
        command => {
            let said = store::update(path, |tasks| {
                let said = change(command, tasks, &now)?;
                let changed = said.changed;
                Ok((said, changed))
            })?;
            print!("{}", said.text);
        }
    }
    Ok(())
}

/// What a command said, and whether it changed the tasks (to be saved).
#[derive(Debug, PartialEq, Eq)]
struct Said {
    text: String,
    changed: bool,
}

/// Makes the change a command asks for to `tasks`, read as of `zoned`: every
/// command but `list`, `board` and `cal`.
fn change(command: Command, tasks: &mut Vec<Task>, zoned: &Zoned) -> Result<Said, String> {
    let now = zoned.datetime();
    let said = |text: String, changed| Ok(Said { text, changed });
    match command {
        Command::Add { words } => {
            let task = new_task(tasks, parse::parse(&words, now)?, None, zoned);
            let added = format!("Added {task}\n");
            tasks.push(task);
            said(added, true)
        }
        Command::Sub { id, words } => {
            let parent = &tasks[position(tasks, id)?];
            if task::is_sub_item(tasks, parent) {
                return Err(format!("{id} is a sub item: they go one level deep"));
            }
            let under = parent.to_string();
            let parsed = parse::parse(&words, now)?;
            if parsed.every.is_some() {
                return Err(REPEATS_WITH_ITS_CARD.into());
            }
            let task = new_task(tasks, parsed, Some(id), zoned);
            let added = format!("Added {task}\n  under {under}\n");
            tasks.push(task);
            said(added, true)
        }
        Command::Done { id } => {
            let i = position(tasks, id)?;
            let open: Vec<usize> = task::sub_items(tasks, &task::parents(tasks), i)
                .into_iter()
                .filter(|&s| tasks[s].status != Status::Done)
                .collect();
            let was_done = tasks[i].status == Status::Done;
            for &s in open.iter().chain([&i]) {
                task::set_status(&mut tasks[s], Status::Done, zoned);
            }
            let mut text = match was_done {
                true => format!("Already done {}\n", tasks[i]),
                false => format!("Done {}\n", tasks[i]),
            };
            if !open.is_empty() {
                text += &match (was_done, open.len()) {
                    // Done already: only its sub items are done now.
                    (true, 1) => "Its 1 open sub item is done now.".into(),
                    (true, n) => format!("Its {n} open sub items are done now."),
                    (false, n) => task::done_with(n, false),
                };
                text.push('\n');
            }
            let rolled = task::roll_repeats(tasks, zoned);
            if let Some(next) = task::next_said(&rolled, now.date()) {
                text += &next;
                text.push('\n');
            }
            said(text, !was_done || !open.is_empty())
        }
        Command::Rm { id } => {
            let i = position(tasks, id)?;
            let subs = task::sub_items(tasks, &task::parents(tasks), i);
            let mut text = format!("Removed {}\n", tasks[i]);
            if !subs.is_empty() {
                let n = task::count_of(subs.len(), "sub item");
                text += &format!("Removed its {n} too\n");
            }
            let mut gone = subs;
            gone.push(i);
            gone.sort_unstable();
            for &g in gone.iter().rev() {
                tasks.remove(g);
            }
            said(text, true)
        }
        Command::Start { id } => {
            let i = position(tasks, id)?;
            if tasks[i].status == Status::Doing {
                return said(format!("Already started {}\n", tasks[i]), false);
            }
            let reopened = task::reopened_said(tasks, i);
            task::set_status(&mut tasks[i], Status::Doing, zoned);
            let mut text = format!("Started {}\n", tasks[i]);
            if let Some(reopened) = reopened {
                text += &reopened;
                text.push('\n');
            }
            said(text, true)
        }
        Command::Edit { id, words } => {
            let task = find(tasks, id)?;
            let change = parse::edit(task, &words, now)?;
            let mut text = format!("Changed {task}\n");
            if let Some(still) = still_said(&change, task) {
                text += &still;
                text.push('\n');
            }
            said(text, true)
        }
        Command::List { .. } | Command::Board { .. } | Command::Cal { .. } => {
            unreachable!("run shows these")
        }
    }
}

/// Refused for `sub … every`, and `A` with `every` on the board.
pub const REPEATS_WITH_ITS_CARD: &str =
    "a sub item repeats with its card: give `every` to the card";

/// `Still every month — -every stops it.`: when the words of an edit gave the
/// date and no `every …`, which leaves a repeating task's repeat on (the
/// date's rule: taking its words out of `e`'s line leaves it).
pub fn still_said(change: &parse::Change, task: &Task) -> Option<String> {
    let (Some(Some(due)), None, Some(every)) = (change.due, change.every, task.every) else {
        return None;
    };
    Some(format!(
        "Still {} — -every stops it.",
        every.words(due.date)
    ))
}

/// A new open task of `parsed`'s words, under `parent` if one's given, made
/// `now`.
fn new_task(tasks: &[Task], parsed: parse::Parsed, parent: Option<u32>, now: &Zoned) -> Task {
    Task {
        id: task::next_id(tasks),
        title: parsed.title,
        status: Status::Todo,
        priority: parsed.priority,
        tags: parsed.tags,
        due: parsed.due,
        parent,
        stamp: task::new_stamp(tasks, task::now_ms()),
        done: None,
        added: Some(Local::of(now)),
        every: parsed.every,
        next: None,
    }
}

fn position(tasks: &[Task], id: u32) -> Result<usize, String> {
    tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("there's no task {id}"))
}

fn find(tasks: &mut [Task], id: u32) -> Result<&mut Task, String> {
    let i = position(tasks, id)?;
    Ok(&mut tasks[i])
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    fn task(id: u32, title: &str, status: Status, parent: Option<u32>) -> Task {
        Task {
            id,
            title: title.into(),
            status,
            priority: None,
            tags: vec![],
            due: None,
            parent,
            stamp: 0,
            done: None,
            added: None,
            every: None,
            next: None,
        }
    }

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    /// `change` as of Wednesday 2026-11-11, 10:30: what it said, or its error.
    fn run(command: Command, tasks: &mut Vec<Task>) -> Result<Said, String> {
        change(command, tasks, &wednesday())
    }

    /// Wednesday 2026-11-11, 10:30, in Seoul's offset (a zone with no name,
    /// so no tz database is needed).
    fn wednesday() -> Zoned {
        let seoul = jiff::tz::TimeZone::fixed(jiff::tz::offset(9));
        jiff::civil::date(2026, 11, 11)
            .at(10, 30, 0, 0)
            .to_zoned(seoul)
            .unwrap()
    }

    /// Task 3, ship 0.2.0, with sub items 5 (done) and 6 and 7 (open), and an
    /// unrelated task 4.
    fn ship() -> Vec<Task> {
        vec![
            task(3, "ship 0.2.0", Status::Doing, None),
            task(4, "buy milk", Status::Todo, None),
            task(5, "bump the version", Status::Done, Some(3)),
            task(6, "write the tests", Status::Todo, Some(3)),
            task(7, "tag the release", Status::Doing, Some(3)),
        ]
    }

    /// The words clap gives a command, from the command line.
    fn words_of(args: &[&str]) -> Vec<String> {
        let cli = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        match cli.command {
            Some(Command::Add { words })
            | Some(Command::Edit { words, .. })
            | Some(Command::Sub { words, .. }) => words,
            _ => panic!("{args:?}"),
        }
    }

    #[test]
    fn a_minus_tag_first_on_the_command_line_is_a_word_not_an_option() {
        // 0.4.2 read `-home` as `-h`, its help, and changed nothing.
        assert_eq!(words_of(&["halil", "edit", "3", "-home"]), ["-home"]);
        assert_eq!(
            words_of(&["halil", "edit", "3", "-home", "+work", "!low"]),
            ["-home", "+work", "!low"]
        );
        assert_eq!(
            words_of(&["halil", "add", "-urgent", "call"]),
            ["-urgent", "call"]
        );
        assert_eq!(
            words_of(&["halil", "sub", "1", "-x", "first"]),
            ["-x", "first"]
        );
        // A tag named h: after `--`, as `-h` alone is help.
        assert_eq!(words_of(&["halil", "edit", "3", "--", "-h"]), ["-h"]);
        let help = Cli::try_parse_from(["halil", "edit", "3", "-h"])
            .map(|_| ())
            .unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);

        // From the command line to the task: tags off and on.
        let mut tasks = ship();
        tasks[1].tags = vec!["home".into(), "money".into()];
        for (args, tags) in [
            (&["halil", "edit", "4", "-home"][..], &["money"][..]),
            (&["halil", "edit", "4", "+work", "-money"], &["work"]),
            (
                &["halil", "edit", "4", "-work", "+car", "+home"],
                &["car", "home"],
            ),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            run(cli.command.unwrap(), &mut tasks).unwrap();
            assert_eq!(tasks[1].tags, tags, "{args:?}");
            assert_eq!(tasks[1].title, "buy milk", "{args:?}");
        }
    }

    #[test]
    fn done_on_a_repeat_says_the_next_start_says_where_it_went_edit_keeps_the_step() {
        let mut tasks = ship();
        tasks[1].due = Some("2026-11-13".parse().unwrap());
        tasks[1].every = Some("week".parse().unwrap());
        let said = run(Command::Done { id: 4 }, &mut tasks).unwrap();
        assert_eq!(
            said.text,
            "Done 4: buy milk due Fri 2026-11-13 every week\nDone. Next: fri nov 20.\n"
        );
        let next = tasks.last().unwrap().clone();
        assert_eq!(
            (next.title.as_str(), next.due.unwrap().to_string()),
            ("buy milk", "2026-11-20".into())
        );
        let said = run(Command::Start { id: 4 }, &mut tasks).unwrap();
        assert_eq!(
            said.text,
            format!(
                "Started 4: buy milk due Fri 2026-11-13\nReopened. Its repeat is on #{}.\n",
                next.id
            )
        );
        let said = run(
            Command::Edit {
                id: next.id,
                words: words("//sat"),
            },
            &mut tasks,
        )
        .unwrap();
        assert!(
            said.text
                .ends_with("\nStill every week — -every stops it.\n"),
            "{}",
            said.text
        );
        // A sub item repeats with its card.
        assert_eq!(
            run(
                Command::Sub {
                    id: 3,
                    words: words("x //every week"),
                },
                &mut tasks,
            )
            .unwrap_err(),
            REPEATS_WITH_ITS_CARD
        );
    }

    #[test]
    fn done_under_the_lock_racing_an_add_makes_one_next_and_keeps_the_add() {
        let dir =
            std::env::temp_dir().join(format!("halil-test-{}-repeat-race", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.jsonl");
        let mut rent = task(1, "pay rent", Status::Todo, None);
        rent.stamp = 1_763_500_000_001;
        rent.due = Some("2026-11-13".parse().unwrap());
        rent.every = Some("week".parse().unwrap());
        store::save(&path, &[rent]).unwrap();
        // Eight `halil done 1` and an add, all at once.
        let start = std::sync::Arc::new(std::sync::Barrier::new(9));
        let mut runs: Vec<_> = (0..8)
            .map(|_| {
                let (path, start) = (path.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    run_at(&path, Command::Done { id: 1 })
                })
            })
            .collect();
        runs.push({
            let (path, start) = (path.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                run_at(
                    &path,
                    Command::Add {
                        words: words("buy milk"),
                    },
                )
            })
        });
        for r in runs {
            r.join().unwrap().unwrap();
        }
        let after = store::load(&path).unwrap();
        let rents: Vec<&Task> = after.iter().filter(|t| t.title == "pay rent").collect();
        assert_eq!(
            rents.len(),
            2,
            "the done one and one next, not two: {after:?}"
        );
        assert_eq!(rents.iter().filter(|t| t.every.is_some()).count(), 1);
        assert!(after.iter().any(|t| t.title == "buy milk"), "the add kept");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_command_waits_for_the_lock_and_then_sees_the_file_as_it_is() {
        let dir = std::env::temp_dir().join(format!("halil-test-{}-main-lock", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.jsonl");
        let mut tasks = vec![task(1, "pay rent", Status::Todo, None)];
        tasks[0].stamp = 1_763_500_000_001;
        store::save(&path, &tasks).unwrap();
        // Another halil holds the lock and writes a task meanwhile.
        let held = store::lock(&path).unwrap();
        let add = {
            let path = path.clone();
            std::thread::spawn(move || {
                run_at(
                    &path,
                    Command::Add {
                        words: words("buy milk"),
                    },
                )
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!add.is_finished(), "the command waits for the lock");
        let mut other = task(2, "call mum", Status::Todo, None);
        other.stamp = 1_763_500_000_002;
        tasks.push(other);
        store::save(&path, &tasks).unwrap();
        drop(held);
        add.join().unwrap().unwrap();
        let after = store::load(&path).unwrap();
        let titles: Vec<&str> = after.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["pay rent", "call mum", "buy milk"]);
        assert_eq!(after[2].id, 3, "after the other's task");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn done_keeps_when_it_was_done_start_clears_it_and_add_and_sub_keep_when_made() {
        let made = Some(Local::of(&wednesday()));
        let mut tasks = ship();
        run(Command::Done { id: 3 }, &mut tasks).unwrap();
        let by_id = |tasks: &[Task], id: u32| tasks.iter().find(|t| t.id == id).unwrap().clone();
        for id in [3, 6, 7] {
            assert_eq!(by_id(&tasks, id).done, made, "{id}, and its open sub items");
        }
        assert_eq!(by_id(&tasks, 5).done, None, "done before: not known");
        run(Command::Start { id: 3 }, &mut tasks).unwrap();
        assert_eq!(by_id(&tasks, 3).done, None);
        assert_eq!(by_id(&tasks, 6).done, made, "its sub items stay done");
        run(
            Command::Add {
                words: words("buy milk"),
            },
            &mut tasks,
        )
        .unwrap();
        run(
            Command::Sub {
                id: 4,
                words: words("oat milk"),
            },
            &mut tasks,
        )
        .unwrap();
        let n = tasks.len();
        for t in &tasks[n - 2..] {
            assert_eq!(t.added, made, "{}", t.title);
            assert_eq!(t.done, None);
        }
    }

    #[test]
    fn sub_adds_under_the_task_with_the_words_of_add_from_the_same_ids() {
        let mut tasks = ship();
        let said = run(
            Command::Sub {
                id: 4,
                words: words("go to the shop +errand !low"),
            },
            &mut tasks,
        )
        .unwrap();
        assert_eq!(
            said.text,
            "Added 1: go to the shop +errand !low\n  under 4: buy milk\n"
        );
        assert!(said.changed);
        let added = tasks.last().unwrap();
        assert_eq!((added.id, added.parent), (1, Some(4)));
        assert_eq!(added.tags, ["errand"]);
        assert!(added.stamp >= 1_763_500_000_000, "stamped when made");

        // done and edit take a sub item's id as they take a task's.
        run(
            Command::Edit {
                id: 1,
                words: words("go to the market"),
            },
            &mut tasks,
        )
        .unwrap();
        let said = run(Command::Done { id: 1 }, &mut tasks).unwrap();
        assert_eq!(said.text, "Done 1: go to the market +errand !low\n");
        assert_eq!(tasks.last().unwrap().status, Status::Done);
        assert_eq!(tasks.last().unwrap().parent, Some(4));
    }

    #[test]
    fn sub_under_a_sub_item_is_refused() {
        let mut tasks = ship();
        let err = run(
            Command::Sub {
                id: 6,
                words: words("x"),
            },
            &mut tasks,
        )
        .unwrap_err();
        assert_eq!(err, "6 is a sub item: they go one level deep");
        let err = run(
            Command::Sub {
                id: 9,
                words: words("x"),
            },
            &mut tasks,
        )
        .unwrap_err();
        assert_eq!(err, "there's no task 9");
        assert_eq!(tasks, ship());
    }

    #[test]
    fn done_finishes_the_open_sub_items_and_says_so() {
        let mut tasks = ship();
        let said = run(Command::Done { id: 3 }, &mut tasks).unwrap();
        assert_eq!(
            said.text,
            "Done 3: ship 0.2.0\nDone, with its 2 open sub items.\n"
        );
        assert!(tasks.iter().all(|t| t.id == 4 || t.status == Status::Done));
        assert_eq!(tasks[1].status, Status::Todo);

        // Done already, with nothing open under it: nothing to save.
        let said = run(Command::Done { id: 3 }, &mut tasks).unwrap();
        assert_eq!(said.text, "Already done 3: ship 0.2.0\n");
        assert!(!said.changed);
        let said = run(Command::Done { id: 4 }, &mut tasks).unwrap();
        assert_eq!(said.text, "Done 4: buy milk\n");
    }

    #[test]
    fn done_on_a_done_task_says_its_open_sub_items_are_done_now() {
        // Done already, with sub items open under it (`halil sub` after it
        // was done, say): only they change.
        let mut tasks = ship();
        tasks[0].status = Status::Done;
        let said = run(Command::Done { id: 3 }, &mut tasks).unwrap();
        assert_eq!(
            said.text,
            "Already done 3: ship 0.2.0\nIts 2 open sub items are done now.\n"
        );
        assert!(said.changed);
        assert!(tasks.iter().all(|t| t.id == 4 || t.status == Status::Done));

        let mut tasks = ship();
        tasks[0].status = Status::Done;
        tasks[4].status = Status::Done;
        let said = run(Command::Done { id: 3 }, &mut tasks).unwrap();
        assert_eq!(
            said.text,
            "Already done 3: ship 0.2.0\nIts 1 open sub item is done now.\n"
        );
    }

    #[test]
    fn edit_with_only_a_tag_the_task_hasnt_is_refused() {
        let mut tasks = ship();
        let err = run(
            Command::Edit {
                id: 3,
                words: words("-money"),
            },
            &mut tasks,
        )
        .unwrap_err();
        assert_eq!(err, "3 has no tag money");
        assert_eq!(tasks, ship(), "the title untouched");
    }

    #[test]
    fn add_edit_and_sub_take_a_marked_date_and_refuse_one_that_isnt() {
        let mut tasks = ship();
        let said = run(
            Command::Add {
                words: words("pay rent //fri 9am +home"),
            },
            &mut tasks,
        )
        .unwrap();
        assert_eq!(
            said.text,
            "Added 1: pay rent +home due Fri 2026-11-13 09:00\n"
        );
        let said = run(
            Command::Sub {
                id: 1,
                words: words("transfer it //nov 12"),
            },
            &mut tasks,
        )
        .unwrap();
        assert!(
            said.text
                .starts_with("Added 2: transfer it due Thu 2026-11-12\n"),
            "{}",
            said.text
        );
        run(
            Command::Edit {
                id: 1,
                words: words("//2027-01-03"),
            },
            &mut tasks,
        )
        .unwrap();
        assert_eq!(tasks[5].due.unwrap().to_string(), "2027-01-03");

        let before = tasks.clone();
        for (command, err) in [
            (
                Command::Add {
                    words: words("pay rent //fridya"),
                },
                "`//fridya` isn't a date",
            ),
            (
                Command::Sub {
                    id: 1,
                    words: words("x //"),
                },
                "`//` needs a date",
            ),
        ] {
            assert_eq!(run(command, &mut tasks), Err(err.into()));
        }
        assert_eq!(tasks, before, "nothing changed");
    }

    fn dated(task: Task, due: &str) -> Task {
        Task {
            due: Some(due.parse().unwrap()),
            ..task
        }
    }

    /// The friday party (due Wednesday), a date with a time, no date, no date
    /// with a date word in the title, a sub item, and Hangul.
    fn dates() -> Vec<Task> {
        vec![
            dated(
                task(1, "plan the friday party", Status::Todo, None),
                "2026-11-11",
            ),
            dated(task(2, "pay rent", Status::Todo, None), "2026-11-13 18:00"),
            task(3, "buy milk", Status::Doing, None),
            task(4, "plan the friday party", Status::Todo, None),
            dated(
                task(5, "set up the transfer", Status::Todo, Some(2)),
                "2026-11-20 09:00",
            ),
            dated(task(6, "월세 내기", Status::Todo, None), "2026-11-27"),
        ]
    }

    #[test]
    fn edit_reads_the_line_the_boards_e_fills_in_back_as_the_task() {
        let now = jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0);
        let mut tasks = dates();
        let mut lines = Vec::new();
        for id in 1..=6 {
            let (line, _) = parse::edit_line(&tasks[id as usize - 1], now);
            lines.push(line.clone());
            // As one quoted word, and as the shell splits it.
            for words in [vec![line.clone()], words(&line)] {
                run(Command::Edit { id, words }, &mut tasks).unwrap();
                assert_eq!(tasks, dates(), "{line}");
            }
        }
        assert_eq!(
            lines,
            [
                "plan the friday party //2026-11-11",
                "pay rent //2026-11-13 6pm",
                "buy milk",
                "plan the friday party //",
                "set up the transfer //2026-11-20 9am",
                "월세 내기 //2026-11-27",
            ]
        );

        // 0.4.0's line, the title alone, lost the date word to the date.
        run(
            Command::Edit {
                id: 1,
                words: words("plan the friday party"),
            },
            &mut tasks,
        )
        .unwrap();
        assert_eq!(tasks[0].title, "plan the party");

        // Changed in place: the title words stay title.
        let mut tasks = dates();
        let said = run(
            Command::Edit {
                id: 1,
                words: words("plan the friday party //fri 6pm"),
            },
            &mut tasks,
        )
        .unwrap();
        assert_eq!(
            said.text,
            "Changed 1: plan the friday party due Fri 2026-11-13 18:00\n"
        );
        run(
            Command::Edit {
                id: 6,
                words: words("월세 꼭 내기 //nov 30"),
            },
            &mut tasks,
        )
        .unwrap();
        assert_eq!(
            (tasks[5].title.as_str(), tasks[5].due.unwrap().to_string()),
            ("월세 꼭 내기", "2026-11-30".into())
        );
    }

    #[test]
    fn edit_takes_the_date_off_with_slashes_alone_add_and_sub_refuse_them() {
        let mut tasks = dates();
        for (id, line, said) in [
            (2, "//", "Changed 2: pay rent\n"),
            (
                1,
                "plan the friday party //",
                "Changed 1: plan the friday party\n",
            ),
            (5, "// +bank", "Changed 5: set up the transfer +bank\n"),
            (6, "월세 내기 //", "Changed 6: 월세 내기\n"),
            // No date to take off: nothing else changes.
            (3, "//", "Changed 3: buy milk\n"),
        ] {
            let got = run(
                Command::Edit {
                    id,
                    words: words(line),
                },
                &mut tasks,
            );
            assert_eq!(got.unwrap().text, said, "{line}");
        }
        assert!(tasks.iter().all(|t| t.due.is_none()));
        assert_eq!(tasks[4].parent, Some(2), "a sub item stays one");
        let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
        let before: Vec<String> = dates().into_iter().map(|t| t.title).collect();
        assert_eq!(titles, before);

        // Not on add or sub: a new task has no date to take off.
        let before = tasks.clone();
        for command in [
            Command::Add {
                words: words("x //"),
            },
            Command::Sub {
                id: 2,
                words: words("x // +home"),
            },
        ] {
            assert_eq!(run(command, &mut tasks), Err("`//` needs a date".into()));
        }
        assert_eq!(tasks, before);
    }

    #[test]
    fn rm_removes_the_sub_items_and_says_how_many() {
        let mut tasks = ship();
        let said = run(Command::Rm { id: 3 }, &mut tasks).unwrap();
        assert_eq!(
            said.text,
            "Removed 3: ship 0.2.0\nRemoved its 3 sub items too\n"
        );
        assert_eq!(tasks, [task(4, "buy milk", Status::Todo, None)]);

        // A sub item goes alone.
        let mut tasks = ship();
        let said = run(Command::Rm { id: 6 }, &mut tasks).unwrap();
        assert_eq!(said.text, "Removed 6: write the tests\n");
        assert_eq!(tasks.len(), 4);
    }
}
