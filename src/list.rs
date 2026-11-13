use std::cmp::Reverse;

use crate::task::{Priority, Status, Task};

/// The open tasks as a table, highest priority first, then oldest first.
pub fn render(tasks: &[Task], color: bool) -> String {
    let mut open: Vec<&Task> = tasks.iter().filter(|t| t.status != Status::Done).collect();
    if open.is_empty() {
        return "Nothing to do.\n".into();
    }
    open.sort_by_key(|t| (Reverse(t.priority), t.id));

    let paint = |code: &str, s: String| -> String {
        if color { format!("\x1b[{code}m{s}\x1b[0m") } else { s }
    };
    let id_width = open.iter().map(|t| t.id.to_string().len()).max().unwrap_or(0).max(2);

    let mut out = paint("2", format!("{:>id_width$}  {:<4}  TASK", "ID", "PRI")) + "\n";
    for t in open {
        let pri = match t.priority {
            Some(p) => paint(priority_color(p), format!("{p:<4}")),
            None => " ".repeat(4),
        };
        let mut line = format!("{}  {pri}  {}", paint("2", format!("{:>id_width$}", t.id)), t.title);
        if !t.tags.is_empty() {
            let tags: Vec<String> = t.tags.iter().map(|tag| format!("+{tag}")).collect();
            line += "  ";
            line += &paint("36", tags.join(" "));
        }
        out += &line;
        out.push('\n');
    }
    out
}

fn priority_color(p: Priority) -> &'static str {
    match p {
        Priority::High => "31",
        Priority::Med => "33",
        Priority::Low => "2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: u32, title: &str, priority: Option<Priority>, status: Status) -> Task {
        Task {
            id,
            title: title.into(),
            status,
            priority,
            tags: vec![],
        }
    }

    #[test]
    fn lists_open_tasks_by_priority_then_id() {
        let mut rent = task(4, "pay rent", Some(Priority::High), Status::Todo);
        rent.tags = vec!["home".into()];
        let tasks = [
            task(1, "buy milk", None, Status::Todo),
            task(2, "call mum", Some(Priority::Low), Status::Doing),
            task(3, "old thing", Some(Priority::High), Status::Done),
            rent,
            task(5, "fix bike", Some(Priority::Med), Status::Todo),
            task(6, "read", None, Status::Todo),
        ];
        assert_eq!(
            render(&tasks, false),
            concat!(
                "ID  PRI   TASK\n",
                " 4  high  pay rent  +home\n",
                " 5  med   fix bike\n",
                " 2  low   call mum\n",
                " 1        buy milk\n",
                " 6        read\n",
            )
        );
    }

    #[test]
    fn nothing_open_says_so() {
        let tasks = [task(1, "done", None, Status::Done)];
        assert_eq!(render(&tasks, false), "Nothing to do.\n");
        assert_eq!(render(&[], false), "Nothing to do.\n");
    }
}
