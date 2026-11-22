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
//! path, so memory is O(D²). [`MAX_DIFFERENCES`] caps that.
//!
//! Hitting the cap used to end the whole diff: the file was reported as one
//! block out and one block in. That treats "many differences" as "nothing in
//! common", and they are not the same thing. Deleting a 2,200-line function
//! is 2,200 differences and one of the simplest diffs there is; a regenerated
//! lockfile has thousands of small ones scattered through text that is
//! otherwise identical. Both read as a file that had been deleted and written
//! again.
//!
//! So the cap now ends only the *search*, and what it could not finish is cut
//! into pieces it can — see [`align`]. A block replacement is what is left
//! for a region that genuinely shares nothing, and only for that region;
//! [`Diff::truncated`] says so when it happens, rather than silently lying.

use std::collections::HashMap;

/// Beyond this many differences one search stops, and the region it was
/// working on is divided instead. At the cap the trace is ~4M entries
/// (~32 MB).
pub const MAX_DIFFERENCES: usize = 2000;

/// How many times a region may be divided before what is left is given up on.
///
/// Each division is paid for with a search that ran to the cap and failed, so
/// this is a bound on time. Real files need one or two; only something built
/// to defeat it nests deeper, and it is welcome to its block replacement.
const MAX_DIVISIONS: usize = 12;

/// How much searching one diff may do, in lines compared and diagonals tried.
///
/// About twenty-four searches run all the way to the cap: enough that no file
/// yet seen in a real history comes near it, and little enough — tens of
/// milliseconds — that a file built to cost as much as possible cannot hold a
/// request for longer than that. A commit page diffs up to sixty files, so
/// "per file, however awkward" is the bound that has to hold.
pub const BUDGET: u64 = 48_000_000;

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
    /// Some part of the two files could not be lined up at all, and is shown
    /// as one block removed and one block added. Everything outside that part
    /// is still an ordinary line diff.
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
    diff_within(old, new, &mut { BUDGET })
}

/// Diff two files by line, drawing on a budget the caller holds.
///
/// For a caller that diffs many files to answer one request. Each file is
/// still limited to [`BUDGET`]; `effort` is what is left for all of them
/// together, and comes back reduced by what this one used. Once it is spent,
/// later files are still diffed — a small edit costs almost nothing and is
/// unaffected — but a file that needs real searching gets its awkward parts
/// as blocks, and says so. The page is bounded however the commit was built.
pub fn diff_within(old: &[u8], new: &[u8], effort: &mut u64) -> Diff {
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
    let mut ids: HashMap<&[u8], u32> = HashMap::with_capacity(a.len() + b.len());
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

    let (script, abandoned) = align(&ai, &bi, effort);
    out.truncated = abandoned;
    out.removed = script.iter().filter(|op| **op == Op::Delete).count();
    out.added = script.iter().filter(|op| **op == Op::Insert).count();
    out.hunks = group(&script, &a, &b);
    out
}

/// Collect changed lines into hunks with `CONTEXT` lines either side, merging
/// hunks that would otherwise overlap.
///
/// Works from the script and builds a line's text only once it is known to be
/// in a hunk. A file is mostly lines nothing happened to, and turning each of
/// those into a `String` to then drop it was most of the time a small diff of
/// a large file took.
fn group(script: &[Op], a: &[&[u8]], b: &[&[u8]]) -> Vec<Hunk> {
    // How far into each file the script is *before* each step. Indexed by
    // step, so a hunk can start anywhere without replaying what precedes it.
    let mut at: Vec<(u32, u32)> = Vec::with_capacity(script.len());
    let mut changed: Vec<usize> = Vec::new();
    let (mut oi, mut ni) = (0u32, 0u32);
    for (i, op) in script.iter().enumerate() {
        at.push((oi, ni));
        match op {
            Op::Equal => {
                oi += 1;
                ni += 1;
            }
            Op::Delete => {
                oi += 1;
                changed.push(i);
            }
            Op::Insert => {
                ni += 1;
                changed.push(i);
            }
        }
    }
    if changed.is_empty() {
        return Vec::new();
    }

    let last = script.len() - 1;
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CONTEXT);
        let mut end = (changed[i] + CONTEXT).min(last);

        // Absorb every following change whose context touches this hunk.
        let mut j = i + 1;
        while j < changed.len() && changed[j].saturating_sub(CONTEXT) <= end + 1 {
            end = (changed[j] + CONTEXT).min(last);
            j += 1;
        }
        i = j;

        let lines: Vec<DiffLine> = (start..=end)
            .map(|p| {
                let (o, n) = (at[p].0 as usize, at[p].1 as usize);
                match script[p] {
                    Op::Equal => DiffLine { op: Op::Equal, old_no: Some(o + 1), new_no: Some(n + 1), text: text(a[o]) },
                    Op::Delete => DiffLine { op: Op::Delete, old_no: Some(o + 1), new_no: None, text: text(a[o]) },
                    Op::Insert => DiffLine { op: Op::Insert, old_no: None, new_no: Some(n + 1), text: text(b[n]) },
                }
            })
            .collect();
        let old_lines = lines.iter().filter(|l| l.old_no.is_some()).count();
        let new_lines = lines.iter().filter(|l| l.new_no.is_some()).count();
        let old_start = lines.iter().find_map(|l| l.old_no).unwrap_or(0);
        let new_start = lines.iter().find_map(|l| l.new_no).unwrap_or(0);

        hunks.push(Hunk { old_start, old_lines, new_start, new_lines, lines });
    }
    hunks
}

/// An edit script between two sequences, whatever their size.
///
/// The shortest one, whenever the search can find it within its cap — which
/// is every ordinary edit, and for those nothing here is new.
///
/// Past the cap the region is divided at **anchors**: lines that occur exactly
/// once on each side. Such a line can only be matched with itself, so it is a
/// point the two files certainly have in common, and the longest run of them
/// that appears in the same order in both is a skeleton the rest can hang
/// from. Each gap between two anchors is then its own, smaller problem, and
/// is solved the same way: exactly if it fits under the cap, by dividing
/// again if not. This is the idea behind patience diff, used here only where
/// the exact search has already failed.
///
/// A region with no unique lines — a table, a file of repeated rows — has
/// nothing to divide it by. There the search's own progress is used instead:
/// it stopped at the cap having lined up *some* prefix of the two sides as
/// well as that prefix can be, so that much is kept and the search starts
/// again from where it got to.
///
/// All of it draws on one [`BUDGET`]. When that is gone, whatever has not
/// been lined up is replaced as a block, and the second value says so.
fn align(a: &[u32], b: &[u32], effort: &mut u64) -> (Vec<Op>, bool) {
    let allowed = (*effort).min(BUDGET);
    let mut search = Search { v: Vec::new(), trace: Vec::new(), budget: allowed };
    let mut ops = Vec::with_capacity(a.len().max(b.len()));
    let mut abandoned = false;
    align_into(&mut search, a, b, 0, &mut ops, &mut abandoned);
    *effort -= allowed - search.budget;
    settle(&mut ops);
    (ops, abandoned)
}

/// Put each run of changes in the order a reader expects: what went, then
/// what came.
///
/// Between two unchanged lines the deletions and insertions can be taken in
/// any order and mean the same thing. The search hands them back in whatever
/// order its path happened to cross them, and a script assembled from several
/// searches interleaves them further — "three in, two out, four in" for what
/// is one block replaced by another.
fn settle(ops: &mut [Op]) {
    let mut i = 0;
    while i < ops.len() {
        if ops[i] == Op::Equal {
            i += 1;
            continue;
        }
        let run = ops[i..].iter().take_while(|op| **op != Op::Equal).count();
        let gone = ops[i..i + run].iter().filter(|op| **op == Op::Delete).count();
        ops[i..i + gone].fill(Op::Delete);
        ops[i + gone..i + run].fill(Op::Insert);
        i += run;
    }
}

fn align_into(
    search: &mut Search,
    a: &[u32],
    b: &[u32],
    depth: usize,
    ops: &mut Vec<Op>,
    abandoned: &mut bool,
) {
    // Trim the common prefix and suffix: the search then only runs over the
    // part that actually differs, which is what makes this practical on real
    // files where one line changed in a thousand. It also means anchors are
    // looked for among the lines that differ — a line repeated in the shared
    // part is not unique over the whole file and may well be unique in here.
    let (pre, suf) = common_ends(a, b);
    let (mut a, mut b) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    ops.extend(std::iter::repeat_n(Op::Equal, pre));

    // What is known about this region's lines, once something has needed to
    // know it. Counting them is a pass over both sides, so it is not done for
    // a region the search simply solves — which is nearly all of them.
    let mut counted: Option<(Vec<(usize, usize)>, bool)> = None;

    loop {
        match search.run(a, b, counted.is_none()) {
            Found::Whole(script) => {
                ops.extend(script);
                break;
            }
            Found::Blind => {
                let (marks, shared) = anchors(a, b);
                if !shared {
                    // Not one line in common. Out with one, in with the
                    // other is not a fallback here: it is the answer, and it
                    // has been proven rather than searched for.
                    ops.extend(std::iter::repeat_n(Op::Delete, a.len()));
                    ops.extend(std::iter::repeat_n(Op::Insert, b.len()));
                    break;
                }
                // They do share lines; the search had just not reached one.
                // Run it again knowing that, and keep the count for later.
                counted = Some((marks, shared));
            }
            Found::Part { script, x, y } => {
                let (marks, _) = counted.take().unwrap_or_else(|| anchors(a, b));
                let marks = if depth < MAX_DIVISIONS { marks } else { Vec::new() };
                if marks.is_empty() {
                    // Nothing to divide by. Keep what the search lined up and
                    // go again from there; each round costs budget, so this
                    // ends.
                    ops.extend(script);
                    (a, b) = (&a[x..], &b[y..]);
                    continue;
                }
                let (mut i, mut j) = (0usize, 0usize);
                for (x, y) in marks {
                    align_into(search, &a[i..x], &b[j..y], depth + 1, ops, abandoned);
                    ops.push(Op::Equal);
                    (i, j) = (x + 1, y + 1);
                }
                align_into(search, &a[i..], &b[j..], depth + 1, ops, abandoned);
                break;
            }
            Found::Nothing => {
                *abandoned = true;
                ops.extend(std::iter::repeat_n(Op::Delete, a.len()));
                ops.extend(std::iter::repeat_n(Op::Insert, b.len()));
                break;
            }
        }
    }

    ops.extend(std::iter::repeat_n(Op::Equal, suf));
}

/// The lines that occur exactly once in each sequence, as far as they agree
/// on an order: `(index in a, index in b)`, increasing in both.
///
/// Unique lines can appear in a different order on the two sides — a function
/// moved above another — and then they cannot all be kept, because an edit
/// script only moves forward. The most that can be kept is the longest
/// subsequence that increases on both sides, found the way a patience game is
/// played: each card goes on the leftmost pile whose top is not smaller, and
/// remembers the top of the pile to its left.
///
/// Also says whether the two share any line at all, unique or not, since the
/// counting that finds the anchors has already found that out.
fn anchors(a: &[u32], b: &[u32]) -> (Vec<(usize, usize)>, bool) {
    struct Seen {
        in_a: u32,
        at_a: usize,
        in_b: u32,
        at_b: usize,
    }
    let mut seen: HashMap<u32, Seen> = HashMap::with_capacity(a.len());
    for (i, id) in a.iter().enumerate() {
        let e = seen.entry(*id).or_insert(Seen { in_a: 0, at_a: i, in_b: 0, at_b: 0 });
        e.in_a += 1;
    }
    let mut shared = false;
    for (j, id) in b.iter().enumerate() {
        // A line only in `b` can never be an anchor, so it is not recorded.
        if let Some(e) = seen.get_mut(id) {
            e.in_b += 1;
            e.at_b = j;
            shared = true;
        }
    }

    let mut pairs: Vec<(usize, usize)> = seen
        .values()
        .filter(|e| e.in_a == 1 && e.in_b == 1)
        .map(|e| (e.at_a, e.at_b))
        .collect();
    pairs.sort_unstable();

    // `tops[n]` is the pair ending the best increasing run of length n + 1.
    let mut tops: Vec<usize> = Vec::new();
    let mut before: Vec<Option<usize>> = vec![None; pairs.len()];
    for (at, &(_, y)) in pairs.iter().enumerate() {
        let pile = tops.partition_point(|&t| pairs[t].1 < y);
        if pile > 0 {
            before[at] = Some(tops[pile - 1]);
        }
        if pile == tops.len() {
            tops.push(at);
        } else {
            tops[pile] = at;
        }
    }

    let mut out = Vec::with_capacity(tops.len());
    let mut at = tops.last().copied();
    while let Some(i) = at {
        out.push(pairs[i]);
        at = before[i];
    }
    out.reverse();
    (out, shared)
}

/// How many leading and trailing elements two sequences share.
fn common_ends(a: &[u32], b: &[u32]) -> (usize, usize) {
    let (n, m) = (a.len(), b.len());
    let mut pre = 0;
    while pre < n && pre < m && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < n - pre && suf < m - pre && a[n - 1 - suf] == b[m - 1 - suf] {
        suf += 1;
    }
    (pre, suf)
}

/// What one search came back with.
enum Found {
    /// The shortest script from one sequence to the other.
    Whole(Vec<Op>),
    /// The cap was reached first. `script` is the shortest way from the start
    /// to `(x, y)` — `x` elements into `a`, `y` into `b` — which is as far as
    /// any path of that many edits gets.
    Part { script: Vec<Op>, x: usize, y: usize },
    /// A good many edits in, and not one line has matched yet. Either the two
    /// have nothing in common, which a count settles far more cheaply than
    /// searching to the cap does, or the first match is simply further in.
    /// Only reported when asked for.
    Blind,
    /// The budget ran out, and nothing here should be relied on.
    Nothing,
}

/// How many edits a search makes without a single match before saying so.
///
/// A search that fails outright costs the square of the cap; one stopped here
/// costs the square of this, some two hundred times less. Two blocks of
/// unrelated text are the common way to reach the cap, so it is worth asking
/// early whether that is what these are.
const BLIND_AFTER: usize = 128;

/// Myers' greedy shortest-edit-script search, and the memory it works in.
///
/// The memory is kept between searches because a divided diff runs a great
/// many of them, and at the cap one search's trace is sixteen megabytes: asked
/// for afresh each time, allocating it was a visible part of the cost.
struct Search {
    /// Furthest `x` reached on each diagonal, for the current number of edits.
    v: Vec<i32>,
    /// `v` as it stood before each step, end to end. Step `d` reaches
    /// diagonals `-d..=d` and reads one beyond on each side, so its row is
    /// `2d + 3` wide and starts at `d * (d + 2)` — the sum of the rows before.
    trace: Vec<i32>,
    /// Steps left to spend, across every search in this diff.
    budget: u64,
}

impl Search {
    /// `probe` asks for [`Found::Blind`] if the search gets [`BLIND_AFTER`]
    /// edits in without matching anything.
    fn run(&mut self, a: &[u32], b: &[u32], probe: bool) -> Found {
        if a.is_empty() {
            return Found::Whole(vec![Op::Insert; b.len()]);
        }
        if b.is_empty() {
            return Found::Whole(vec![Op::Delete; a.len()]);
        }
        // Positions are kept as i32: half the trace of an isize, and no file
        // of two thousand million lines is going to be diffed by line.
        let (Ok(n), Ok(m)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
            return Found::Nothing;
        };
        let Some(span) = n.checked_add(m) else { return Found::Nothing };

        // `k` runs over -max ..= max, and the loop reads v[k-1] and v[k+1], so
        // the array needs one slot of margin past that range on each side.
        // `base` is where k == 0 lives. Sized by the cap rather than by the
        // files: a search never gets further than `max` diagonals out, however
        // long the sequences are.
        let max = (span as usize).min(MAX_DIFFERENCES);
        let base = max + 1;
        self.v.clear();
        self.v.resize(2 * max + 3, 0);
        self.trace.clear();
        let mut matched = false;

        for d in 0..=max {
            let di = d as i32;
            if probe && d == BLIND_AFTER && !matched {
                return Found::Blind;
            }
            // Only the reachable window of `v` is kept per step, so memory is
            // O(D²) rather than O(D * (N + M)). The window includes the ±1
            // margin the backtrack also needs — without it, reconstructing
            // the path reads one past the end at the boundary diagonals.
            self.trace.extend_from_slice(&self.v[base - d - 1..=base + d + 1]);

            let mut k = -di;
            while k <= di {
                let ki = (base as i32 + k) as usize;
                let mut x = if k == -di || (k != di && self.v[ki - 1] < self.v[ki + 1]) {
                    self.v[ki + 1]
                } else {
                    self.v[ki - 1] + 1
                };
                let mut y = x - k;

                let from = x;
                while x < n && y < m && a[x as usize] == b[y as usize] {
                    x += 1;
                    y += 1;
                }
                self.v[ki] = x;
                matched |= x > from;

                if x >= n && y >= m {
                    return Found::Whole(backtrack(&self.trace, d, n, m));
                }

                // One for the diagonal, one for each line compared along it.
                // Charged here rather than per step so that a single long run
                // of matching lines cannot outspend the budget unnoticed.
                let cost = 1 + (x - from) as u64;
                if cost >= self.budget {
                    self.budget = 0;
                    return Found::Nothing;
                }
                self.budget -= cost;
                k += 2;
            }
        }

        // Out of edits. Of everything reached in the last step, the point
        // furthest along both files is the most that has been lined up. A
        // diagonal can run off the edge of the grid — the search lets it,
        // since such a path can never finish — so only points inside count.
        let di = max as i32;
        let mut best: Option<(i32, i32)> = None;
        let mut k = -di;
        while k <= di {
            let x = self.v[(base as i32 + k) as usize];
            let y = x - k;
            if (0..=n).contains(&x) && (0..=m).contains(&y) && best.is_none_or(|(bx, by)| x + y > bx + by) {
                best = Some((x, y));
            }
            k += 2;
        }
        match best {
            Some((x, y)) => Found::Part {
                script: backtrack(&self.trace, max, x, y),
                x: x as usize,
                y: y as usize,
            },
            None => Found::Nothing,
        }
    }
}

/// Walk the recorded traces backwards from `(x, y)`, reached at step `steps`,
/// to recover the edit script that got there.
fn backtrack(trace: &[i32], steps: usize, x: i32, y: i32) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    let (mut x, mut y) = (x, y);

    for d in (0..=steps).rev() {
        let di = d as i32;
        // The row is v[base-d-1 ..= base+d+1], so diagonal k sits at k + d + 1.
        let row = &trace[d * (d + 2)..];
        let at = |k: i32| -> i32 { row[(k + di + 1) as usize] };

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
/// `None` when some part of the two could not be lined up. A reader can be
/// shown that part as a block replaced; a merge acting on it would be
/// deciding that one side rewrote a region it may only have edited, so it is
/// told there is no script and makes it a conflict instead.
pub fn script<S: AsRef<str>>(a: &[S], b: &[S]) -> Option<Vec<Op>> {
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let mut intern = |s: &str| -> u32 {
        let key: &str = unsafe { std::mem::transmute::<&str, &'static str>(s) };
        let next = ids.len() as u32;
        *ids.entry(key).or_insert(next)
    };
    let ai: Vec<u32> = a.iter().map(|l| intern(l.as_ref())).collect();
    let bi: Vec<u32> = b.iter().map(|l| intern(l.as_ref())).collect();
    let (ops, abandoned) = align(&ai, &bi, &mut { BUDGET });
    (!abandoned).then_some(ops)
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
        let abandoned = reconstructs_within(old, new, &mut { BUDGET });
        assert!(!abandoned, "small inputs must be lined up in full");
    }

    /// As [`reconstructs`], with a budget of the caller's choosing. Says
    /// whether any part was given up on — which must never cost correctness:
    /// a block replaced is still a script that produces the new file.
    fn reconstructs_within(old: &str, new: &str, effort: &mut u64) -> bool {
        let a = split_lines(old.as_bytes());
        let b = split_lines(new.as_bytes());

        let mut ids: HashMap<&[u8], u32> = HashMap::new();
        let mut intern = |s: &'_ [u8]| -> u32 {
            let key: &[u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(s) };
            let next = ids.len() as u32;
            *ids.entry(key).or_insert(next)
        };
        let ai: Vec<u32> = a.iter().map(|l| intern(content(l))).collect();
        let bi: Vec<u32> = b.iter().map(|l| intern(content(l))).collect();

        let (script, abandoned) = align(&ai, &bi, effort);
        let mut rebuilt: Vec<String> = Vec::new();
        let (mut oi, mut ni) = (0usize, 0usize);
        for op in script {
            match op {
                Op::Equal => {
                    assert_eq!(ai[oi], bi[ni], "an unchanged line that is not the same line");
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
        abandoned
    }

    /// `n` numbered lines, each run through `line`; `None` leaves one out.
    fn lines(n: usize, line: impl Fn(usize) -> Option<String>) -> String {
        (0..n).filter_map(line).map(|l| l + "\n").collect()
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
    fn files_with_nothing_in_common_are_one_block_out_and_one_in() {
        // Two files sharing nothing, well past the difference cap.
        let n = MAX_DIFFERENCES + 500;
        let old = lines(n, |i| Some(format!("aaa {i}")));
        let new = lines(n, |i| Some(format!("bbb {i}")));

        let d = diff(old.as_bytes(), new.as_bytes());
        assert_eq!((d.removed, d.added), (n, n));
        assert_eq!(d.hunks.len(), 1);
        // What went, then what came -- not whatever order the search met them.
        let ops: Vec<Op> = d.hunks[0].lines.iter().map(|l| l.op).collect();
        assert!(ops[..n].iter().all(|op| *op == Op::Delete) && ops[n..].iter().all(|op| *op == Op::Insert));
        // Nothing was given up on: with no line in common this *is* the diff.
        assert!(!d.truncated);
    }

    /// The bug this module had: more than `MAX_DIFFERENCES` changed lines was
    /// reported as the whole file removed and the whole file added. A long
    /// function deleted is that many differences and nothing else.
    #[test]
    fn a_deletion_larger_than_the_cap_is_still_a_deletion() {
        let n = 6 * MAX_DIFFERENCES;
        let gone = MAX_DIFFERENCES + 200;
        let old = lines(n, |i| Some(format!("line {i}")));
        // A long block out of the middle, and a line edited either side of it.
        let new = lines(n, |i| match i {
            500 | 9_000 => Some(format!("edited {i}")),
            i if (3_000..3_000 + gone).contains(&i) => None,
            i => Some(format!("line {i}")),
        });

        let d = diff(old.as_bytes(), new.as_bytes());
        assert!(!d.truncated);
        assert_eq!((d.removed, d.added), (gone + 2, 2), "the block, and the two edits");
        assert_eq!(d.hunks.len(), 3);
        reconstructs(&old, &new);
    }

    /// The other way to pass the cap: not one large change but thousands of
    /// small ones, as in a regenerated lockfile.
    #[test]
    fn thousands_of_scattered_edits_are_each_found() {
        let n = 30_000;
        let old = lines(n, |i| Some(format!("line {i}")));
        let new = lines(n, |i| Some(if i % 4 == 0 { format!("edited {i}") } else { format!("line {i}") }));

        let d = diff(old.as_bytes(), new.as_bytes());
        assert!(!d.truncated);
        assert_eq!((d.removed, d.added), (n / 4, n / 4), "exactly the lines that changed");
        reconstructs(&old, &new);
    }

    /// No line here is unique, so there are no anchors to divide by. The
    /// search's own progress has to carry it.
    #[test]
    fn a_file_of_repeated_lines_is_lined_up_without_anchors() {
        let n = 40_000;
        let old = lines(n, |i| Some(format!("row {}", i % 30)));
        let new = lines(n, |i| Some(if i % 10 == 0 { "changed".into() } else { format!("row {}", i % 30) }));

        let d = diff(old.as_bytes(), new.as_bytes());
        assert!(!d.truncated);
        assert_eq!((d.removed, d.added), (n / 10, n / 10));
        reconstructs(&old, &new);
    }

    /// Unique lines that changed order cannot all be anchors. The ones kept
    /// have to be the longest run both sides agree on.
    #[test]
    fn anchors_are_the_longest_run_in_the_same_order_on_both_sides() {
        //            a: 10 11 12 13 14 15      b: 13 10 11 99 12 15 14
        let (marks, shared) = anchors(&[10, 11, 12, 13, 14, 15], &[13, 10, 11, 99, 12, 15, 14]);
        assert!(shared);
        // 10 11 12 then one of 14/15 -- four, in order on both sides. 13 moved
        // to the front and is the one that has to give way.
        assert_eq!(marks.len(), 4);
        assert_eq!(&marks[..3], &[(0, 1), (1, 2), (2, 4)]);
        assert!(marks.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));

        // Repeated on either side is not unique, and not an anchor.
        assert_eq!(anchors(&[1, 2, 2, 3], &[1, 2, 3, 3]).0, vec![(0, 0)]);
        assert!(!anchors(&[1, 2], &[3, 4]).1, "nothing in common at all");
    }

    /// Running out of budget costs detail, never correctness, and says so.
    #[test]
    fn an_exhausted_budget_degrades_to_blocks_and_admits_it() {
        let n = 12_000;
        let old = lines(n, |i| Some(format!("row {}", i % 30)));
        let new = lines(n, |i| Some(if i % 3 == 0 { "changed".into() } else { format!("row {}", i % 30) }));

        // Plenty: lined up in full.
        let mut effort = BUDGET;
        assert!(!reconstructs_within(&old, &new, &mut effort));
        let used = BUDGET - effort;
        assert!(used > 0, "the caller is told what it cost");

        // A fraction of what that took: still a correct script, and flagged.
        let mut effort = used / 10;
        assert!(reconstructs_within(&old, &new, &mut effort));
        let d = diff_within(old.as_bytes(), new.as_bytes(), &mut (used / 10));
        assert!(d.truncated);

        // None at all, and a small edit is still found: it costs next to
        // nothing, so a spent budget does not turn every later file to blocks.
        let small = lines(n, |i| Some(if i == 77 { "changed".into() } else { format!("row {}", i % 30) }));
        let d = diff_within(old.as_bytes(), small.as_bytes(), &mut 64);
        assert!(!d.truncated);
        assert_eq!((d.removed, d.added), (1, 1));
    }

    /// Whatever the edits and however little budget there is to find them
    /// with, the script turns the old file into the new one.
    #[test]
    fn every_script_reconstructs_at_any_size_and_any_budget() {
        let mut x = 0x2545_f491u64;
        let mut rnd = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for round in 0..24 {
            let n = 500 + rnd(9_000) as usize;
            // Few distinct lines in some rounds, so that anchors are scarce.
            let vocabulary = if round % 3 == 0 { 25 } else { 1_000_000 };
            let old: Vec<String> = (0..n).map(|_| format!("l{}", rnd(vocabulary))).collect();
            let mut new: Vec<String> = Vec::with_capacity(n);
            let rate = 2 + rnd(40);
            for l in &old {
                match rnd(rate) {
                    0 => {}
                    1 => new.push(format!("n{}", rnd(vocabulary))),
                    2 => {
                        new.push(l.clone());
                        new.push(format!("n{}", rnd(vocabulary)));
                    }
                    _ => new.push(l.clone()),
                }
            }
            let (old, new) = (old.join("\n") + "\n", new.join("\n") + "\n");
            let mut effort = [BUDGET, 500_000, 20_000, 0][round % 4];
            reconstructs_within(&old, &new, &mut effort);
        }
    }

    #[test]
    fn a_run_of_changes_reads_deletions_first() {
        let mut ops = [Op::Insert, Op::Delete, Op::Equal, Op::Insert, Op::Insert, Op::Delete, Op::Equal, Op::Delete];
        settle(&mut ops);
        assert_eq!(ops, [Op::Delete, Op::Insert, Op::Equal, Op::Delete, Op::Insert, Op::Insert, Op::Equal, Op::Delete]);
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
