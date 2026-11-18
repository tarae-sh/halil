mod board;
mod date;
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
        /// The task in plain English: a date, +tags, !high, !med or !low
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
                text += &task::done_with(open.len(), false);
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
