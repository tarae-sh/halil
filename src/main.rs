mod board;
mod clip;
mod date;
mod hangul;
mod list;
mod parse;
mod store;
mod task;

use std::env;
use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use jiff::civil::DateTime;

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
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// List open tasks, what's due first (the default)
    List,
    /// Mark a task as started
    Start { id: u32 },
    /// Mark a task as done, and its open sub items
    Done { id: u32 },
    /// Change a task: new words replace its title; a date, +tags, -tags and !priority update it
    Edit {
        id: u32,
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// Remove a task, and its sub items
    Rm { id: u32 },
    /// Add a sub item under a task, with the words of `add`
    #[command(after_help = "Example: halil sub 3 write the tests friday")]
    Sub {
        id: u32,
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// Open the board: Todo, Doing, Done
    Board,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Command::List)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("halil: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), String> {
    let path = store::default_path()?;
    let mut tasks = store::load(&path)?;
    let now = jiff::Zoned::now().datetime();

    match command {
        Command::List => {
            let color = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
            print!("{}", list::render(&tasks, now, color));
        }
        Command::Board => board::run(&path, tasks)?,
        command => {
            let said = change(command, &mut tasks, now)?;
            if said.changed {
                store::save(&path, &tasks)?;
            }
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

/// Makes the change a command asks for to `tasks`, read as of `now`: every
/// command but `list` and `board`.
fn change(command: Command, tasks: &mut Vec<Task>, now: DateTime) -> Result<Said, String> {
    let said = |text: String, changed| Ok(Said { text, changed });
    match command {
        Command::Add { words } => {
            let task = new_task(tasks, parse::parse(&words, now)?, None);
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
            let task = new_task(tasks, parse::parse(&words, now)?, Some(id));
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
                tasks[s].status = Status::Done;
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
            let task = find(tasks, id)?;
            if task.status == Status::Doing {
                return said(format!("Already started {task}\n"), false);
            }
            task.status = Status::Doing;
            said(format!("Started {task}\n"), true)
        }
        Command::Edit { id, words } => {
            let task = find(tasks, id)?;
            parse::edit(task, &words, now)?;
            said(format!("Changed {task}\n"), true)
        }
        Command::List | Command::Board => unreachable!("run shows these"),
    }
}

/// A new open task of `parsed`'s words, under `parent` if one's given.
fn new_task(tasks: &[Task], parsed: parse::Parsed, parent: Option<u32>) -> Task {
    Task {
        id: task::next_id(tasks),
        title: parsed.title,
        status: Status::Todo,
        priority: parsed.priority,
        tags: parsed.tags,
        due: parsed.due,
        parent,
        stamp: task::new_stamp(tasks, task::now_ms()),
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
        }
    }

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    /// `change` as of Wednesday 2026-11-11, 10:30: what it said, or its error.
    fn run(command: Command, tasks: &mut Vec<Task>) -> Result<Said, String> {
        change(
            command,
            tasks,
            jiff::civil::date(2026, 11, 11).at(10, 30, 0, 0),
        )
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
