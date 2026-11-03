use crate::api::Task;
use std::collections::{HashMap, HashSet};

pub struct Row<'a> {
    pub task: &'a Task,
    pub depth: usize,
    pub children: usize,
}
/// Iterative traversal: no recursion limit, and malformed cycles never hide tasks.
pub fn rows<'a>(tasks: &[&'a Task], collapsed: &HashSet<String>, query: &str) -> Vec<Row<'a>> {
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    let indices: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.as_str(), i))
        .collect();
    let mut roots = Vec::new();
    for (i, t) in tasks.iter().enumerate() {
        if let Some(parent) = t
            .parent_id
            .as_deref()
            .filter(|p| *p != t.id && indices.contains_key(p))
        {
            children.entry(parent).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    // Search includes each matching task's ancestry and expands it without changing stored folds.
    let mut included = HashSet::new();
    let query = query.to_lowercase();
    if !query.is_empty() {
        for (i, _t) in tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.title.to_lowercase().contains(&query))
        {
            let mut next = Some(i);
            while let Some(index) = next {
                if !included.insert(index) {
                    break;
                }
                next = tasks[index]
                    .parent_id
                    .as_deref()
                    .and_then(|p| indices.get(p))
                    .copied();
            }
        }
    }
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    // Visit rootless cycles too. A separate covered set keeps collapsed descendants hidden.
    let mut covered = HashSet::new();
    for root in roots.into_iter().chain(0..tasks.len()) {
        if covered.contains(&root) {
            continue;
        }
        let mut stack = vec![(root, 0, false)];
        while let Some((index, depth, hidden)) = stack.pop() {
            if !visited.insert(index) {
                continue;
            }
            covered.insert(index);
            let t = tasks[index];
            let child_indices = children
                .get(t.id.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            if !hidden && (query.is_empty() || included.contains(&index)) {
                result.push(Row {
                    task: t,
                    depth,
                    children: child_indices.len(),
                });
            }
            let hide = hidden || (query.is_empty() && collapsed.contains(&t.id));
            for &child in child_indices.iter().rev() {
                stack.push((child, depth + 1, hide));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_depth_folds_search_orphans_and_cycles() {
        let mut tasks: Vec<Task> = (0..1500)
            .map(|i| Task {
                id: i.to_string(),
                title: format!("Task {i}"),
                parent_id: if i == 0 {
                    None
                } else {
                    Some((i - 1).to_string())
                },
                ..Default::default()
            })
            .collect();
        let refs = tasks.iter().collect::<Vec<_>>();
        let mut folds = HashSet::new();
        assert_eq!(rows(&refs, &folds, "").last().unwrap().depth, 1499);
        folds.insert("0".into());
        assert_eq!(rows(&refs, &folds, "").len(), 1);
        assert_eq!(rows(&refs, &folds, "Task 1499").len(), 1500);
        tasks[0].parent_id = Some("1499".into());
        assert_eq!(
            rows(&tasks.iter().collect::<Vec<_>>(), &HashSet::new(), "").len(),
            1500
        );
        tasks[0].parent_id = Some("missing".into());
        assert_eq!(
            rows(&tasks.iter().collect::<Vec<_>>(), &HashSet::new(), "").len(),
            1500
        );
    }
}
