//! Line-level diffing — Myers' O(ND) algorithm.
//!
//! [`crate::repo::diff_trees`] answers *which paths changed*, cheaply, by
//! comparing subtree hashes. This module answers the other question: given two
//! versions of one file, which **lines** changed.
//!
//! # The algorithm
//!
//! Myers models a diff as the shortest path through an edit graph: moving right
//! deletes a line from the old file, moving down inserts one from the new file,
//! and a diagonal is a line both files share and costs nothing. The shortest
//! such path is the smallest edit script, which is what a human reads as "the
//! obvious diff".
//!
//! Two things keep it fast in practice:
//!
//! * **Common prefix and suffix are trimmed first.** Real edits touch a small
//!   part of a file, and `D` — the number of differences, which dominates the
//!   runtime — is measured only over what is left.
//! * **Lines are interned to `u32` ids.** The inner loop compares integers
//!   rather than byte slices, which is where the time actually goes.
//!
//! # Bounds
//!
//! The search records one trace row per edit-script step to reconstruct the
//! path, so memory is O(D²). [`MAX_DIFFERENCES`] caps that; past it the two
//! files are so unrelated that a line diff would be noise anyway, and we report
//! the whole thing as one replaced block. Callers get [`Diff::truncated`] so
//! they can say so rather than silently lying.

use std::collections::HashMap;

/// Beyond this many differences we stop searching and emit a whole-block
/// replacement. At the cap the trace is ~4M entries (~32 MB).
pub const MAX_DIFFERENCES: usize = 2000;

/// Lines of surrounding context kept around each change.
pub const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Equal,
    Delete,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: Op,
    /// 1-based line number in the old file, if this line exists there.
    pub old_no: Option<usize>,
    /// 1-based line number in the new file, if this line exists there.
    pub new_no: Option<usize>,
    pub text: String,
}

/// A run of changes plus its surrounding context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The `@@ -a,b +c,d @@` header of a unified diff.
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_lines, self.new_start, self.new_lines
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
    /// The edit script exceeded [`MAX_DIFFERENCES`] and was replaced with a
    /// single delete-all / insert-all block.
    pub truncated: bool,
    /// Either side contained a NUL byte in its first 8 KiB.
    pub binary: bool,
    /// The old side did not end with a newline.
    pub old_no_eol: bool,
    pub new_no_eol: bool,
    /// The files differ only in their line terminators (CRLF vs LF, or a
    /// trailing newline). Reported explicitly because the alternative is a
    /// diff in which every line is marked changed and both sides look
    /// character-for-character identical on screen.
    pub only_line_endings: bool,
}

/// Split into lines *without* their terminators.
///
/// A trailing newline terminates the final line rather than starting an empty
/// one — otherwise every file would appear to end with a phantom blank line and
/// adding a newline at EOF would show as an inserted line.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&[u8]> = data.split(|b| *b == b'\n').collect();
    if data.ends_with(b"\n") {
        out.pop();
    }
    out
}

fn looks_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|b| *b == 0)
}

/// A line's content without its terminator, which is what we both compare and
/// display. Comparing something other than what is rendered is how you get a
/// diff whose two sides look identical.
fn content(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(content(line)).into_owned()
}

/// Diff two files by line.
pub fn diff(old: &[u8], new: &[u8]) -> Diff {
    let mut out = Diff {
        binary: looks_binary(old) || looks_binary(new),
        old_no_eol: !old.is_empty() && !old.ends_with(b"\n"),
        new_no_eol: !new.is_empty() && !new.ends_with(b"\n"),
        ..Default::default()
    };
    if out.binary || old == new {
        return out;
    }

    let a = split_lines(old);
    let b = split_lines(new);

    // Intern lines so the inner loop compares u32s.
    let mut ids: HashMap<&[u8], u32> = HashMap::new();
    let mut intern = |s: &'_ [u8]| -> u32 {
        // SAFETY-free trick: the map borrows from `old`/`new`, both of which
        // outlive this function.
        let key: &[u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(s) };
        let next = ids.len() as u32;
        *ids.entry(key).or_insert(next)
    };
    let ai: Vec<u32> = a.iter().map(|l| intern(content(l))).collect();
    let bi: Vec<u32> = b.iter().map(|l| intern(content(l))).collect();

    // Same lines, different bytes: the only difference is the terminators.
    if ai == bi {
        out.only_line_endings = true;
        return out;
    }

    let script = myers(&ai, &bi);

    let Some(script) = script else {
        // Too different to be worth a line diff: one block out, one block in.
        out.truncated = true;
        let mut lines = Vec::with_capacity(a.len() + b.len());
        for (i, l) in a.iter().enumerate() {
            lines.push(DiffLine { op: Op::Delete, old_no: Some(i + 1), new_no: None, text: text(l) });
        }
        for (i, l) in b.iter().enumerate() {
            lines.push(DiffLine { op: Op::Insert, old_no: None, new_no: Some(i + 1), text: text(l) });
        }
        out.removed = a.len();
        out.added = b.len();
        out.hunks = vec![Hunk {
            old_start: if a.is_empty() { 0 } else { 1 },
            old_lines: a.len(),
            new_start: if b.is_empty() { 0 } else { 1 },
            new_lines: b.len(),
            lines,
        }];
        return out;
    };

    // Turn the edit script into numbered lines.
    let mut flat: Vec<DiffLine> = Vec::with_capacity(script.len());
    let (mut oi, mut ni) = (0usize, 0usize);
    for op in script {
        match op {
            Op::Equal => {
                oi += 1;
                ni += 1;
                flat.push(DiffLine { op, old_no: Some(oi), new_no: Some(ni), text: text(a[oi - 1]) });
            }
            Op::Delete => {
                oi += 1;
                out.removed += 1;
                flat.push(DiffLine { op, old_no: Some(oi), new_no: None, text: text(a[oi - 1]) });
            }
            Op::Insert => {
                ni += 1;
                out.added += 1;
                flat.push(DiffLine { op, old_no: None, new_no: Some(ni), text: text(b[ni - 1]) });
            }
        }
    }

    out.hunks = group(&flat);
    out
}

/// Collect changed lines into hunks with `CONTEXT` lines either side, merging
/// hunks that would otherwise overlap.
fn group(flat: &[DiffLine]) -> Vec<Hunk> {
    let changed: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, l)| l.op != Op::Equal)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }

    let mut hunks = Vec::new();
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CONTEXT);
        let mut end = (changed[i] + CONTEXT).min(flat.len() - 1);

        // Absorb every following change whose context touches this hunk.
        let mut j = i + 1;
        while j < changed.len() && changed[j].saturating_sub(CONTEXT) <= end + 1 {
            end = (changed[j] + CONTEXT).min(flat.len() - 1);
            j += 1;
        }
        i = j;

        let lines: Vec<DiffLine> = flat[start..=end].to_vec();
        let old_lines = lines.iter().filter(|l| l.old_no.is_some()).count();
        let new_lines = lines.iter().filter(|l| l.new_no.is_some()).count();
        let old_start = lines.iter().find_map(|l| l.old_no).unwrap_or(0);
        let new_start = lines.iter().find_map(|l| l.new_no).unwrap_or(0);

        hunks.push(Hunk { old_start, old_lines, new_start, new_lines, lines });
    }
    hunks
}

/// Myers' greedy shortest-edit-script search.
///
/// Returns `None` when the edit distance exceeds [`MAX_DIFFERENCES`].
fn myers(a: &[u32], b: &[u32]) -> Option<Vec<Op>> {
    let n = a.len();
    let m = b.len();

    // Trim the common prefix and suffix: the search then only runs over the
    // part that actually differs, which is what makes this practical on real
    // files where one line changed in a thousand.
    let mut pre = 0;
    while pre < n && pre < m && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < n - pre && suf < m - pre && a[n - 1 - suf] == b[m - 1 - suf] {
        suf += 1;
    }

    let mid = search(&a[pre..n - suf], &b[pre..m - suf])?;

    let mut ops = Vec::with_capacity(pre + mid.len() + suf);
    ops.extend(std::iter::repeat_n(Op::Equal, pre));
    ops.extend(mid);
    ops.extend(std::iter::repeat_n(Op::Equal, suf));
    Some(ops)
}

fn search(a: &[u32], b: &[u32]) -> Option<Vec<Op>> {
    let n = a.len() as isize;
    let m = b.len() as isize;

    if n == 0 {
        return Some(vec![Op::Insert; b.len()]);
    }
    if m == 0 {
        return Some(vec![Op::Delete; a.len()]);
    }

    let max = ((n + m) as usize).min(MAX_DIFFERENCES);

    // `k` runs over -(n+m) ..= (n+m), and the loop reads v[k-1] and v[k+1], so
    // the array needs one slot of margin past that range on each side. `base`
    // is where k == 0 lives.
    let span = (n + m) as usize;
    let base = span + 1;
    let mut v = vec![0isize; 2 * span + 3];

    // Only the reachable window of `v` is kept per step, so memory is O(D²)
    // rather than O(D * (N + M)). The window includes the ±1 margin the
    // backtrack also needs — without it, reconstructing the path reads one past
    // the end at the boundary diagonals.
    let mut trace: Vec<Vec<isize>> = Vec::new();

    for d in 0..=max {
        let di = d as isize;
        trace.push(v[base - d - 1..=base + d + 1].to_vec());

        let mut k = -di;
        while k <= di {
            let ki = (base as isize + k) as usize;
            let mut x = if k == -di || (k != di && v[ki - 1] < v[ki + 1]) {
                v[ki + 1]
            } else {
                v[ki - 1] + 1
            };
            let mut y = x - k;

            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[ki] = x;

            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

/// Walk the recorded traces backwards to recover the edit script.
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    let (mut x, mut y) = (n, m);

    for d in (0..trace.len()).rev() {
        let di = d as isize;
        let row = &trace[d];
        // `row` is v[base-d-1 ..= base+d+1], so diagonal k sits at k + d + 1.
        let at = |k: isize| -> isize { row[(k + di + 1) as usize] };

        let k = x - y;
        let prev_k = if k == -di || (k != di && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;

        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push(Op::Insert);
                y -= 1;
            } else {
                ops.push(Op::Delete);
                x -= 1;
            }
        }
    }

    ops.reverse();
    ops
}

/// The raw edit script between two line sequences.
///
/// Exposed for the three-way merge, which needs the full script (including the
/// unchanged runs) rather than the hunks a reader sees — hunks deliberately
/// omit the parts nothing happened to, and a merge has to account for those.
///
/// `None` when the two sequences differ by more than [`MAX_DIFFERENCES`].
pub fn script<S: AsRef<str>>(a: &[S], b: &[S]) -> Option<Vec<Op>> {
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let mut intern = |s: &str| -> u32 {
        let key: &str = unsafe { std::mem::transmute::<&str, &'static str>(s) };
        let next = ids.len() as u32;
        *ids.entry(key).or_insert(next)
    };
    let ai: Vec<u32> = a.iter().map(|l| intern(l.as_ref())).collect();
    let bi: Vec<u32> = b.iter().map(|l| intern(l.as_ref())).collect();
    myers(&ai, &bi)
}

/// Render a diff as a unified patch, the way `diff -u` would.
pub fn to_unified(d: &Diff, old_path: &str, new_path: &str) -> String {
    if d.binary {
        return format!("--- {old_path}\n+++ {new_path}\nBinary files differ\n");
    }
    let mut out = format!("--- {old_path}\n+++ {new_path}\n");
    for h in &d.hunks {
        out.push_str(&h.header());
        out.push('\n');
        for l in &h.lines {
            out.push(match l.op {
                Op::Equal => ' ',
                Op::Delete => '-',
                Op::Insert => '+',
            });
            out.push_str(&l.text);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(old: &str, new: &str) -> Vec<(Op, String)> {
        diff(old.as_bytes(), new.as_bytes())
            .hunks
            .into_iter()
            .flat_map(|h| h.lines)
            .map(|l| (l.op, l.text))
            .collect()
    }

    /// The property that matters: applying the edit script to the old file must
    /// reproduce the new file exactly. Everything else is presentation.
    fn reconstructs(old: &str, new: &str) {
        let d = diff(old.as_bytes(), new.as_bytes());
        let a = split_lines(old.as_bytes());

        // Rebuild the full script (hunks only carry changed regions + context),
        // by re-running against untrimmed input.
        let mut ids: HashMap<&[u8], u32> = HashMap::new();
        let mut intern = |s: &'_ [u8]| -> u32 {
            let key: &[u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(s) };
            let next = ids.len() as u32;
            *ids.entry(key).or_insert(next)
        };
        let ai: Vec<u32> = a.iter().map(|l| intern(content(l))).collect();
        let b = split_lines(new.as_bytes());
        let bi: Vec<u32> = b.iter().map(|l| intern(content(l))).collect();

        let script = myers(&ai, &bi).expect("small inputs must not truncate");
        let mut rebuilt: Vec<String> = Vec::new();
        let (mut oi, mut ni) = (0usize, 0usize);
        for op in script {
            match op {
                Op::Equal => {
                    rebuilt.push(text(a[oi]));
                    oi += 1;
                    ni += 1;
                }
                Op::Delete => oi += 1,
                Op::Insert => {
                    rebuilt.push(text(b[ni]));
                    ni += 1;
                }
            }
        }
        let want: Vec<String> = b.iter().map(|l| text(l)).collect();
        assert_eq!(rebuilt, want, "script did not reconstruct the new file");
        assert_eq!(oi, a.len(), "script did not consume the old file");
        let _ = d;
    }

    #[test]
    fn identical_files_have_no_hunks() {
        let d = diff(b"a\nb\nc\n", b"a\nb\nc\n");
        assert!(d.hunks.is_empty());
        assert_eq!((d.added, d.removed), (0, 0));
    }

    #[test]
    fn a_single_changed_line() {
        let got = ops("one\ntwo\nthree\n", "one\nTWO\nthree\n");
        assert_eq!(
            got,
            vec![
                (Op::Equal, "one".into()),
                (Op::Delete, "two".into()),
                (Op::Insert, "TWO".into()),
                (Op::Equal, "three".into()),
            ]
        );
    }

    #[test]
    fn pure_insertion_and_deletion() {
        let d = diff(b"a\nb\n", b"a\nx\nb\n");
        assert_eq!((d.added, d.removed), (1, 0));

        let d = diff(b"a\nx\nb\n", b"a\nb\n");
        assert_eq!((d.added, d.removed), (0, 1));
    }

    #[test]
    fn empty_files_are_handled() {
        let d = diff(b"", b"a\nb\n");
        assert_eq!((d.added, d.removed), (2, 0));

        let d = diff(b"a\nb\n", b"");
        assert_eq!((d.added, d.removed), (0, 2));

        assert!(diff(b"", b"").hunks.is_empty());
    }

    #[test]
    fn a_trailing_newline_is_not_a_phantom_line() {
        // "a\nb" vs "a\nb\n" differ only in the final newline, which does not
        // add a line — the flag records it instead.
        let d = diff(b"a\nb", b"a\nb\n");
        assert!(d.old_no_eol && !d.new_no_eol);
        assert_eq!(split_lines(b"a\nb\n").len(), 2);
        assert_eq!(split_lines(b"a\nb").len(), 2);
    }

    #[test]
    fn binary_content_is_flagged_and_not_diffed() {
        let d = diff(b"a\n\0\nb\n", b"a\n\0\nc\n");
        assert!(d.binary);
        assert!(d.hunks.is_empty());
    }

    #[test]
    fn a_line_ending_only_change_is_reported_as_such() {
        // Every byte after the first character differs, but nothing a reader
        // would call a change has happened.
        let d = diff(b"a\r\nb\r\n", b"a\nb\n");
        assert!(d.only_line_endings);
        assert!(d.hunks.is_empty(), "must not mark every line changed");
        assert_eq!((d.added, d.removed), (0, 0));

        // A real edit alongside a CRLF file is still a real edit.
        let d = diff(b"a\r\nb\r\n", b"a\nCHANGED\n");
        assert!(!d.only_line_endings);
        assert_eq!((d.added, d.removed), (1, 1));
    }

    #[test]
    fn hunks_carry_context_and_correct_line_numbers() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let mut new_lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        new_lines[9] = "CHANGED".into();
        let new = new_lines.join("\n") + "\n";

        let d = diff(old.as_bytes(), new.as_bytes());
        assert_eq!(d.hunks.len(), 1, "one change is one hunk");
        let h = &d.hunks[0];
        // 3 lines of context either side of a delete+insert pair.
        assert_eq!(h.lines.len(), 3 + 2 + 3);
        assert_eq!(h.old_start, 7);
        assert_eq!(h.new_start, 7);
        assert_eq!((d.added, d.removed), (1, 1));
    }

    #[test]
    fn distant_changes_make_separate_hunks_but_close_ones_merge() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();

        let mut a: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        a[2] = "X".into();
        a[30] = "Y".into();
        let d = diff(old.as_bytes(), (a.join("\n") + "\n").as_bytes());
        assert_eq!(d.hunks.len(), 2, "changes 27 lines apart are separate hunks");

        let mut b: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        b[10] = "X".into();
        b[12] = "Y".into();
        let d = diff(old.as_bytes(), (b.join("\n") + "\n").as_bytes());
        assert_eq!(d.hunks.len(), 1, "changes 2 lines apart share one hunk");
    }

    #[test]
    fn reconstruction_holds_for_a_range_of_edits() {
        reconstructs("a\nb\nc\n", "a\nb\nc\n");
        reconstructs("a\nb\nc\n", "c\nb\na\n");
        reconstructs("", "x\ny\n");
        reconstructs("x\ny\n", "");
        reconstructs("the\nquick\nbrown\nfox\n", "the\nslow\nbrown\ncat\njumped\n");

        // A larger, pseudo-random pair with many scattered edits.
        let old: String = (0..300).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..300)
            .filter(|i| i % 7 != 0)
            .map(|i| if i % 5 == 0 { format!("changed {i}\n") } else { format!("line {i}\n") })
            .collect();
        reconstructs(&old, &new);
    }

    #[test]
    fn wildly_different_files_truncate_instead_of_hanging() {
        // Two files sharing nothing, well past the difference cap.
        let old: String = (0..(MAX_DIFFERENCES + 500)).map(|i| format!("aaa {i}\n")).collect();
        let new: String = (0..(MAX_DIFFERENCES + 500)).map(|i| format!("bbb {i}\n")).collect();

        let d = diff(old.as_bytes(), new.as_bytes());
        assert!(d.truncated, "should give up rather than search forever");
        assert_eq!(d.hunks.len(), 1);
        assert!(d.added > 0 && d.removed > 0);
    }

    #[test]
    fn unified_output_looks_like_a_patch() {
        let d = diff(b"one\ntwo\nthree\n", b"one\nTWO\nthree\n");
        let p = to_unified(&d, "a/f.txt", "b/f.txt");
        assert!(p.starts_with("--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n"));
        assert!(p.contains("\n-two\n"));
        assert!(p.contains("\n+TWO\n"));
    }
}
