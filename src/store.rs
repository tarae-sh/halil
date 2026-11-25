//! The tasks file: one JSON object a line.

use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::task::{self, Task};

/// `$XDG_DATA_HOME/halil/tasks.jsonl`, else `~/.local/share/halil/tasks.jsonl`.
pub fn default_path() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = env::var_os("HOME").ok_or("can't find the tasks file: HOME isn't set")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("halil").join("tasks.jsonl"))
}

/// Reads every task. A missing file is no tasks; a line that doesn't parse is
/// an error naming it, so a save never drops what it couldn't read. A line
/// without a stamp gets one (see [`task::stamp_unstamped`]), written by the
/// next save.
pub fn load(path: &Path) -> Result<Vec<Task>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let mut tasks = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str(line)
                .map_err(|e| format!("{}:{}: not a task: {e}", path.display(), i + 1))
        })
        .collect::<Result<Vec<Task>, String>>()?;
    task::stamp_unstamped(&mut tasks);
    task::drop_stray_done(&mut tasks);
    Ok(tasks)
}

/// Where the lock that orders changes to the tasks file at `path` is taken: a
/// file of its own beside it (`tasks.jsonl.lock`), never written to.
pub fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("jsonl.lock")
}

/// How long a change waits for another halil to let go of the tasks file
/// before it's said, not hung.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The lock on the tasks file at `path`, held until the file it returns is
/// dropped: an advisory lock on [`lock_path`] (`flock` on Unix, `LockFileEx`
/// on Windows), which every halil takes before it reads the tasks to change
/// them, and the system lets go of when the process ends, however it ends.
/// Waited for at most [`LOCK_WAIT`].
pub fn lock(path: &Path) -> Result<File, String> {
    lock_within(path, LOCK_WAIT)
}

/// [`lock`], waiting at most `wait`; past that, `tasks.jsonl is in use by
/// another halil — nothing changed`.
pub fn lock_within(path: &Path, wait: Duration) -> Result<File, String> {
    let at = lock_path(path);
    let fail = |e: io::Error| format!("can't lock {}: {e}", at.display());
    if let Some(dir) = at.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&at)
        .map_err(fail)?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if start.elapsed() < wait => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                let name = path.file_name().unwrap_or(path.as_os_str());
                return Err(format!(
                    "{} is in use by another halil — nothing changed",
                    name.to_string_lossy()
                ));
            }
            Err(TryLockError::Error(e)) => return Err(fail(e)),
        }
    }
}

/// Makes a change to the tasks file at `path` as one step: under its
/// [`lock`], held from the read to the write, so a change another halil made
/// meanwhile (a command, or the board) is read first and never written over.
/// `change` gets the tasks as they are on disk, and gives back what it says
/// and whether to save them.
pub fn update<T>(
    path: &Path,
    change: impl FnOnce(&mut Vec<Task>) -> Result<(T, bool), String>,
) -> Result<T, String> {
    update_within(path, LOCK_WAIT, change)
}

/// [`update`], waiting at most `wait` for the lock.
pub fn update_within<T>(
    path: &Path,
    wait: Duration,
    change: impl FnOnce(&mut Vec<Task>) -> Result<(T, bool), String>,
) -> Result<T, String> {
    let _held = lock_within(path, wait)?;
    let mut tasks = load(path)?;
    let (said, changed) = change(&mut tasks)?;
    if changed {
        save(path, &tasks)?;
    }
    Ok(said)
}

/// Writes every task, replacing the file whole: written beside it first, then
/// renamed over it, so a crash leaves the old file or the new one.
pub fn save(path: &Path, tasks: &[Task]) -> Result<(), String> {
    let mut text = String::new();
    for task in tasks {
        text += &serde_json::to_string(task).map_err(|e| e.to_string())?;
        text.push('\n');
    }

    let fail = |e: io::Error| format!("can't write {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = temp_beside(path, std::process::id());
    let mut file = File::create(&tmp).map_err(fail)?;
    file.write_all(text.as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    fs::rename(&tmp, path).map_err(fail)
}

/// Where process `pid` writes a save before renaming it over `path`: a name of
/// its own, so two halil saving at once never write into one file.
fn temp_beside(path: &Path, pid: u32) -> PathBuf {
    path.with_extension(format!("jsonl.{pid}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Priority, Status};

    /// A fresh, empty folder for one test, under the system's temp folder.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("halil-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Vec<Task> {
        vec![
            Task {
                id: 1,
                title: "buy milk".into(),
                status: Status::Todo,
                priority: None,
                tags: vec![],
                due: None,
                parent: None,
                stamp: 1_763_500_000_001,
                done: None,
                added: None,
                every: None,
                next: None,
            },
            Task {
                id: 2,
                title: "pay rent".into(),
                status: Status::Done,
                priority: Some(Priority::High),
                tags: vec!["home".into(), "money".into()],
                due: Some("2026-11-20T09:00".parse().unwrap()),
                parent: None,
                stamp: 1_763_500_000_002,
                done: None,
                added: None,
                every: None,
                next: None,
            },
        ]
    }

    /// The sample as 0.1.0 wrote it: no sub items, no stamps.
    const FROM_0_1: &str = concat!(
        r#"{"id":1,"title":"buy milk","status":"todo"}"#,
        "\n",
        r#"{"id":2,"title":"pay rent","status":"done","priority":"high","tags":["home","money"],"due":"2026-11-20T09:00"}"#,
        "\n",
    );

    #[test]
    fn saves_and_loads_the_same_tasks() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("tasks.jsonl");
        save(&path, &sample()).unwrap();
        assert_eq!(load(&path).unwrap(), sample());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_one_json_object_a_line() {
        let dir = temp_dir("lines");
        let path = dir.join("tasks.jsonl");
        save(&path, &sample()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"id":1,"title":"buy milk","status":"todo","stamp":1763500000001}"#,
                "\n",
                r#"{"id":2,"title":"pay rent","status":"done","priority":"high","tags":["home","money"],"due":"2026-11-20T09:00","stamp":1763500000002}"#,
                "\n",
            )
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn done_and_added_are_kept_as_written_a_bad_one_refuses_its_line_a_stray_done_goes() {
        let dir = temp_dir("times");
        let path = dir.join("tasks.jsonl");
        let lines = concat!(
            r#"{"id":1,"title":"pay rent","status":"done","stamp":1763500000001,"done":"2026-11-25T18:04:00+09:00[Asia/Seoul]","added":"2026-11-20T09:00:00-05:00"}"#,
            "\n",
        );
        fs::write(&path, lines).unwrap();
        let tasks = load(&path).unwrap();
        assert_eq!(
            tasks[0].done.as_ref().map(|d| d.to_string()).as_deref(),
            Some("2026-11-25T18:04:00+09:00[Asia/Seoul]")
        );
        save(&path, &tasks).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            lines,
            "written back as read"
        );

        for (line, said) in [
            (
                r#"{"id":1,"title":"x","status":"done","done":"friday"}"#,
                "`friday`",
            ),
            (
                r#"{"id":1,"title":"x","added":"2026-11-25"}"#,
                "`2026-11-25`",
            ),
        ] {
            fs::write(&path, format!("{line}\n")).unwrap();
            let err = load(&path).unwrap_err();
            assert!(
                err.contains("tasks.jsonl:1: not a task") && err.contains(said),
                "{err}"
            );
        }

        fs::write(
            &path,
            r#"{"id":1,"title":"x","status":"todo","done":"2026-11-25T18:04:00+09:00"}"#,
        )
        .unwrap();
        assert_eq!(load(&path).unwrap()[0].done, None, "not done: no done time");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn changes_made_at_once_each_see_the_others_under_the_lock() {
        use std::sync::{Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let dir = temp_dir("at-once");
        let path = dir.join("tasks.jsonl");
        // Threads standing in for halil processes, each adding a task at
        // once, with time between its read and its write.
        let n = 12;
        let start = Arc::new(Barrier::new(n));
        let adds: Vec<_> = (0..n)
            .map(|k| {
                let (path, start) = (path.clone(), Arc::clone(&start));
                thread::spawn(move || {
                    start.wait();
                    update(&path, |tasks| {
                        let id = task::next_id(tasks);
                        thread::sleep(Duration::from_millis(5));
                        tasks.push(Task {
                            id,
                            title: format!("added by {k}"),
                            status: Status::Todo,
                            priority: None,
                            tags: vec![],
                            due: None,
                            parent: None,
                            stamp: 1_763_500_000_000 + k as u64,
                            done: None,
                            added: None,
                            every: None,
                            next: None,
                        });
                        Ok(((), true))
                    })
                    .unwrap();
                })
            })
            .collect();
        for add in adds {
            add.join().unwrap();
        }
        let tasks = load(&path).unwrap();
        let mut ids: Vec<u32> = tasks.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=n as u32).collect::<Vec<_>>(), "every one kept");
        for k in 0..n {
            assert!(
                tasks.iter().any(|t| t.title == format!("added by {k}")),
                "{k}"
            );
        }
        // The lock is a file of its own beside the tasks; nothing else is.
        assert!(lock_path(&path).is_file());
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["tasks.jsonl", "tasks.jsonl.lock"]);
        // A change that saves nothing writes nothing.
        let before = fs::read_to_string(&path).unwrap();
        update(&path, |tasks| Ok((tasks.len(), false))).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_wait_that_doesnt_end_is_said_and_changes_nothing() {
        let dir = temp_dir("held");
        let path = dir.join("tasks.jsonl");
        save(&path, &sample()).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        // A stand-in for another halil, holding the lock and never letting go.
        let held = lock(&path).unwrap();
        let start = Instant::now();
        // In a thread of its own, so a wait that never ends fails the row
        // instead of hanging it.
        let (said, got) = std::sync::mpsc::channel();
        {
            let path = path.clone();
            thread::spawn(move || {
                let r = update_within(&path, Duration::from_millis(300), |tasks| {
                    tasks.clear();
                    Ok(((), true))
                });
                let _ = said.send(r);
            });
        }
        let err = got
            .recv_timeout(Duration::from_secs(3))
            .expect("said within the wait, not hung")
            .unwrap_err();
        assert_eq!(
            err,
            "tasks.jsonl is in use by another halil — nothing changed"
        );
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            before,
            "nothing changed"
        );
        // Let go, and the change goes through; the system lets go of a lock
        // when its file is closed, as when a process ends.
        drop(held);
        update_within(&path, Duration::from_millis(300), |tasks| {
            Ok((tasks.len(), false))
        })
        .unwrap();
        assert_eq!(LOCK_WAIT, Duration::from_secs(5));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_no_tasks() {
        let dir = temp_dir("missing");
        assert_eq!(load(&dir.join("tasks.jsonl")).unwrap(), vec![]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_makes_the_folder_and_leaves_no_temp_file() {
        let dir = temp_dir("folder");
        let path = dir.join("halil").join("tasks.jsonl");
        save(&path, &sample()).unwrap();
        let names: Vec<_> = fs::read_dir(dir.join("halil"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tasks.jsonl"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_replaces_what_was_there() {
        let dir = temp_dir("replace");
        let path = dir.join("tasks.jsonl");
        save(&path, &sample()).unwrap();
        save(&path, &sample()[..1]).unwrap();
        assert_eq!(load(&path).unwrap(), &sample()[..1]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn blank_lines_and_missing_optional_fields_are_fine() {
        let dir = temp_dir("lenient");
        let path = dir.join("tasks.jsonl");
        fs::write(&path, "\n{\"id\":7,\"title\":\"x\"}\n\n").unwrap();
        let tasks = load(&path).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 7);
        assert_eq!(tasks[0].status, Status::Todo);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_sub_item_names_its_parent() {
        let dir = temp_dir("parent");
        let path = dir.join("tasks.jsonl");
        let mut tasks = sample();
        tasks[1].parent = Some(1);
        save(&path, &tasks).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.lines().next().unwrap().contains("parent"));
        assert!(
            text.lines()
                .nth(1)
                .unwrap()
                .ends_with(r#","parent":1,"stamp":1763500000002}"#),
            "{text}"
        );
        assert_eq!(load(&path).unwrap(), tasks);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_from_0_1_and_0_2_load_as_they_are_and_get_stamps_when_next_saved() {
        let dir = temp_dir("older");
        let path = dir.join("tasks.jsonl");
        // 0.2.0's adds a sub item; neither has stamps.
        let from_0_2 = format!(
            "{FROM_0_1}{}\n",
            r#"{"id":3,"title":"transfer it","status":"todo","parent":2}"#
        );
        for (version, text) in [("0.1", FROM_0_1), ("0.2", from_0_2.as_str())] {
            fs::write(&path, text).unwrap();
            let tasks = load(&path).unwrap();
            let unstamped: Vec<Task> = tasks
                .iter()
                .map(|t| Task {
                    stamp: 0,
                    done: None,
                    added: None,
                    every: None,
                    next: None,
                    ..t.clone()
                })
                .collect();
            let mut want = sample();
            for t in &mut want {
                t.stamp = 0;
            }
            assert_eq!(unstamped[..2], want[..], "{version}: as they are");
            assert!(
                tasks.iter().all(|t| t.stamp > 0 && t.stamp < 1 << 32),
                "{version}: stamps of their own: {tasks:?}"
            );
            assert_eq!(
                load(&path).unwrap(),
                tasks,
                "{version}: the same on every read"
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                text,
                "{version}: reading writes nothing"
            );

            // The next save writes them, and they hold from then on.
            save(&path, &tasks).unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            assert_eq!(saved.matches("\"stamp\":").count(), tasks.len(), "{saved}");
            assert_eq!(load(&path).unwrap(), tasks, "{version}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn each_halil_saves_through_a_temp_file_of_its_own() {
        let dir = temp_dir("own-temp");
        let path = dir.join("tasks.jsonl");
        let mine = std::process::id();
        // Another halil is halfway through its save: ours leaves its file be.
        let theirs = temp_beside(&path, mine.wrapping_add(1));
        fs::write(&theirs, "half a save\n").unwrap();
        save(&path, &sample()).unwrap();
        assert_eq!(fs::read_to_string(&theirs).unwrap(), "half a save\n");
        assert_eq!(load(&path).unwrap(), sample());
        let ours = temp_beside(&path, mine);
        assert_eq!(ours, dir.join(format!("tasks.jsonl.{mine}.tmp")));
        assert!(!ours.exists(), "renamed into place");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_bad_line_is_an_error_naming_it() {
        let dir = temp_dir("bad");
        let path = dir.join("tasks.jsonl");
        fs::write(&path, "{\"id\":1,\"title\":\"ok\"}\nnot json\n").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("tasks.jsonl:2:"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }
}
