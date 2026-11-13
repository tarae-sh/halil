use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One task: a line of the tasks file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Todo,
    Doing,
    Done,
}

/// Ordered low to high, so the highest sorts last (`Reverse` it to list it first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    Med,
    High,
}

impl FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "high" | "h" => Ok(Priority::High),
            "med" | "medium" | "m" => Ok(Priority::Med),
            "low" | "l" => Ok(Priority::Low),
            _ => Err(format!("`!{s}` isn't a priority: use !high, !med or !low")),
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad(match self {
            Priority::High => "high",
            Priority::Med => "med",
            Priority::Low => "low",
        })
    }
}

impl fmt::Display for Task {
    /// `3: pay rent +home !high`
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.id, self.title)?;
        for tag in &self.tags {
            write!(f, " +{tag}")?;
        }
        if let Some(p) = self.priority {
            write!(f, " !{p}")?;
        }
        Ok(())
    }
}

/// The id a new task gets: the smallest number from 1 up that no task has.
pub fn next_id(tasks: &[Task]) -> u32 {
    let used: HashSet<u32> = tasks.iter().map(|t| t.id).collect();
    (1..).find(|id| !used.contains(id)).expect("fewer than u32::MAX tasks")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_ids(ids: &[u32]) -> Vec<Task> {
        ids.iter()
            .map(|&id| Task {
                id,
                title: "x".into(),
                status: Status::Todo,
                priority: None,
                tags: vec![],
            })
            .collect()
    }

    #[test]
    fn next_id_is_the_smallest_free_number() {
        assert_eq!(next_id(&with_ids(&[])), 1);
        assert_eq!(next_id(&with_ids(&[1, 2, 3])), 4);
        assert_eq!(next_id(&with_ids(&[1, 2, 4])), 3);
        assert_eq!(next_id(&with_ids(&[5, 2, 3])), 1);
    }
}
