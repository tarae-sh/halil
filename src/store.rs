//! The tasks file: one JSON object a line.

use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    Ok(tasks)
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
