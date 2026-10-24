//! `difflib.unified_diff` over lines, as CPython writes it.
//!
//! The short-fact read shows Jev the literal diff between an earlier quote and the block it came from, and that diff
//! is part of the request body, so it has to be the hunks `SequenceMatcher` finds: its longest-match search, its
//! "popular line" heuristic for long inputs, and its three lines of context.

use std::collections::HashMap;

struct Matcher<'a> {
    a: &'a [&'a str],
    b: &'a [&'a str],
    b2j: HashMap<&'a str, Vec<usize>>,
}

type Opcode = (&'static str, usize, usize, usize, usize);

impl<'a> Matcher<'a> {
    fn new(a: &'a [&'a str], b: &'a [&'a str]) -> Matcher<'a> {
        let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
        for (j, line) in b.iter().enumerate() {
            b2j.entry(line).or_default().push(j);
        }
        // autojunk: in a sequence of 200 or more, a line making up over one percent of it is dropped from the index.
        if b.len() >= 200 {
            let limit = b.len() / 100 + 1;
            b2j.retain(|_, indices| indices.len() <= limit);
        }
        Matcher { a, b, b2j }
    }

    fn longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let (a, b) = (self.a, self.b);
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, line) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = self.b2j.get(line) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).copied().unwrap_or(0) + 1;
                    next.insert(j, k);
                    if k > bestsize {
                        (besti, bestj, bestsize) = (i + 1 - k, j + 1 - k, k);
                    }
                }
            }
            j2len = next;
        }
        // No line is junk, so the match grows over equal lines the popular-line heuristic left out of the index.
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi && bestj + bestsize < bhi && a[besti + bestsize] == b[bestj + bestsize] {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    fn matching_blocks(&self) -> Vec<(usize, usize, usize)> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0, la, 0, lb)];
        let mut blocks = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort();
        let (mut i1, mut j1, mut k1) = (0, 0, 0);
        let mut joined = Vec::new();
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    joined.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            joined.push((i1, j1, k1));
        }
        joined.push((la, lb, 0));
        joined
    }

    fn opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0, 0);
        let mut codes = Vec::new();
        for (ai, bj, size) in self.matching_blocks() {
            let tag = match (i < ai, j < bj) {
                (true, true) => "replace",
                (true, false) => "delete",
                (false, true) => "insert",
                _ => "",
            };
            if !tag.is_empty() {
                codes.push((tag, i, ai, j, bj));
            }
            (i, j) = (ai + size, bj + size);
            if size > 0 {
                codes.push(("equal", ai, i, bj, j));
            }
        }
        codes
    }

    fn grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.opcodes();
        if codes.is_empty() {
            codes.push(("equal", 0, 1, 0, 1));
        }
        if let Some(first) = codes.first_mut()
            && first.0 == "equal"
        {
            let (tag, i1, i2, j1, j2) = *first;
            *first = (tag, i1.max(i2.saturating_sub(n)), i2, j1.max(j2.saturating_sub(n)), j2);
        }
        if let Some(last) = codes.last_mut()
            && last.0 == "equal"
        {
            let (tag, i1, i2, j1, j2) = *last;
            *last = (tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n));
        }
        let mut groups = Vec::new();
        let mut group = Vec::new();
        for (tag, mut i1, i2, mut j1, j2) in codes {
            if tag == "equal" && i2 - i1 > n + n {
                group.push((tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n)));
                groups.push(std::mem::take(&mut group));
                (i1, j1) = (i1.max(i2.saturating_sub(n)), j1.max(j2.saturating_sub(n)));
            }
            group.push((tag, i1, i2, j1, j2));
        }
        if !(group.is_empty() || (group.len() == 1 && group[0].0 == "equal")) {
            groups.push(group);
        }
        groups
    }
}

fn range(start: usize, stop: usize) -> String {
    let length = stop - start;
    match length {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        _ => format!("{},{length}", start + 1),
    }
}

/// `"".join(difflib.unified_diff(a, b, fromfile=..., tofile=...))` for lines that keep their endings.
pub(crate) fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str) -> String {
    let matcher = Matcher::new(a, b);
    let mut out = String::new();
    for (index, group) in matcher.grouped_opcodes(3).into_iter().enumerate() {
        if index == 0 {
            out.push_str(&format!("--- {fromfile}\n+++ {tofile}\n"));
        }
        let (first, last) = (group[0], group[group.len() - 1]);
        out.push_str(&format!("@@ -{} +{} @@\n", range(first.1, last.2), range(first.3, last.4)));
        for (tag, i1, i2, j1, j2) in group {
            if tag == "equal" {
                a[i1..i2].iter().for_each(|line| {
                    out.push(' ');
                    out.push_str(line);
                });
                continue;
            }
            if tag == "replace" || tag == "delete" {
                a[i1..i2].iter().for_each(|line| {
                    out.push('-');
                    out.push_str(line);
                });
            }
            if tag == "replace" || tag == "insert" {
                b[j1..j2].iter().for_each(|line| {
                    out.push('+');
                    out.push_str(line);
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_line_is_one_hunk_with_context() {
        let a = ["a\n", "b\n", "c\n", "d\n", "e\n", "f\n", "g\n", "h\n"];
        let b = ["a\n", "b\n", "c\n", "D\n", "e\n", "f\n", "g\n", "h\n"];
        assert_eq!(
            unified_diff(&a, &b, "earlier quote", "current block"),
            "--- earlier quote\n+++ current block\n@@ -1,7 +1,7 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n"
        );
    }

    #[test]
    fn equal_lines_write_nothing() {
        assert_eq!(unified_diff(&["x"], &["x"], "a", "b"), "");
    }

    #[test]
    fn an_empty_side_is_written_as_line_zero() {
        assert_eq!(unified_diff(&[], &["x"], "a", "b"), "--- a\n+++ b\n@@ -0,0 +1 @@\n+x");
    }
}
