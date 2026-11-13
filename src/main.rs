mod date;
mod list;
mod parse;
mod store;
mod task;

use std::env;
use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    /// Mark a task as done
    Done { id: u32 },
    /// Change a task: new words replace its title; a date, +tags and !priority update it
    Edit {
        id: u32,
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// Remove a task
    Rm { id: u32 },
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

    match command {
        Command::Add { words } => {
            let parsed = parse::parse(&words, jiff::Zoned::now().datetime())?;
            let task = Task {
                id: task::next_id(&tasks),
                title: parsed.title,
                status: Status::Todo,
                priority: parsed.priority,
                tags: parsed.tags,
                due: parsed.due,
            };
            let added = task.to_string();
            tasks.push(task);
            store::save(&path, &tasks)?;
            println!("Added {added}");
        }
        Command::List => {
            let color = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
            print!("{}", list::render(&tasks, color));
        }
        Command::Done { id } => {
            let task = find(&mut tasks, id)?;
            if task.status == Status::Done {
                println!("Already done {task}");
                return Ok(());
            }
            task.status = Status::Done;
            let done = task.to_string();
            store::save(&path, &tasks)?;
            println!("Done {done}");
        }
        Command::Rm { id } => {
            let i = position(&tasks, id)?;
            let removed = tasks.remove(i);
            store::save(&path, &tasks)?;
            println!("Removed {removed}");
        }
        Command::Start { .. } => return Err("`start` isn't written yet".into()),
        Command::Edit { .. } => return Err("`edit` isn't written yet".into()),
        Command::Board => return Err("`board` isn't written yet".into()),
    }
    Ok(())
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

    use super::Cli;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}
