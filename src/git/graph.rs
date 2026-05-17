use compact_str::CompactString;
use gix::ObjectId;

/// Tracks which commit is expected in each lane column.
#[derive(Default)]
pub struct GraphState {
    /// `lanes[i] = Some(ObjectId)` means we expect that commit in column i.
    lanes: Vec<Option<ObjectId>>,
    // Scratch buffers reused across `next()` calls.
    matching: Vec<usize>,
    prefix: String,
}

impl GraphState {
    /// Advance the graph state for a commit with the given id and parent ids.
    /// Returns the ASCII prefix string for this commit row.
    pub fn next(&mut self, id: ObjectId, parents: &[ObjectId]) -> CompactString {
        self.matching.clear();
        for (i, slot) in self.lanes.iter().enumerate() {
            if *slot == Some(id) {
                self.matching.push(i);
            }
        }

        // If no lane found for this commit, claim one.
        let commit_col = if let Some(&col) = self.matching.first() {
            col
        } else if let Some(empty) = self.lanes.iter().position(|s| s.is_none()) {
            empty
        } else {
            self.lanes.push(None);
            self.lanes.len() - 1
        };

        let num_lanes = self.lanes.len().max(commit_col + 1);
        self.prefix.clear();
        for col in 0..num_lanes {
            if col == commit_col {
                self.prefix.push('*');
            } else if self.lanes.get(col).and_then(|s| *s).is_some() {
                self.prefix.push('|');
            } else if self.matching.contains(&col) {
                // Merge convergence: another column also expected this commit.
                self.prefix.push('\\');
            } else {
                self.prefix.push(' ');
            }
            self.prefix.push(' ');
        }

        // Close all matching columns, then place first parent in commit_col.
        // Both indexes are produced earlier in this function — `matching`
        // by `enumerate()` over `self.lanes`, and `commit_col` is either
        // an existing lane index or `self.lanes.len() - 1` right after a
        // push. The `.get_mut`/`if let` form makes that explicit to
        // clippy without runtime cost.
        for &col in &self.matching {
            if let Some(slot) = self.lanes.get_mut(col) {
                *slot = None;
            }
        }
        if let Some(slot) = self.lanes.get_mut(commit_col) {
            *slot = parents.first().copied();
        }

        // Additional parents (merge commits) go into new lanes if not already tracked.
        for &parent in parents.iter().skip(1) {
            if self.lanes.contains(&Some(parent)) {
                continue;
            }
            if let Some(slot) = self.lanes.iter_mut().find(|s| s.is_none()) {
                *slot = Some(parent);
            } else {
                self.lanes.push(Some(parent));
            }
        }

        while self.lanes.last() == Some(&None) {
            self.lanes.pop();
        }

        CompactString::from(self.prefix.trim_end())
    }
}
