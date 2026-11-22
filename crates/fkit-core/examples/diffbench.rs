// Measures the line diff, and checks it against git.
//
//   diffbench lines          synthetic files, built to be large or awkward
//   diffbench git [commits]  every modified file in the current repository's
//                            recent history, beside what `git diff --numstat`
//                            says about the same change
//
// The second is how the "whole file removed, whole file added" bug was found:
// run it inside any git repository and it lists the files this diff shows as
// larger changes than git does.
use fkit_core::diff::diff;
use std::process::Command;
use std::time::Instant;

fn timed(name: &str, old: &str, new: &str) {
    let lines = |s: &str| s.bytes().filter(|b| *b == b'\n').count();
    let began = Instant::now();
    let d = diff(old.as_bytes(), new.as_bytes());
    let took = began.elapsed();
    println!(
        "{name:<44} {:>7} -> {:>7} lines  +{:<7} -{:<7} {:>4} hunks {:>9.2} ms{}",
        lines(old),
        lines(new),
        d.added,
        d.removed,
        d.hunks.len(),
        took.as_secs_f64() * 1e3,
        if d.truncated { "  (part replaced as a block)" } else { "" },
    );
}

fn file(n: usize, line: impl Fn(usize) -> String) -> String {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&line(i));
        s.push('\n');
    }
    s
}

fn lines() {
    let n = 100_000;
    let base = file(n, |i| format!("let value_{i} = compute({i});"));

    timed("ten edits in 100k lines", &base, &file(n, |i| {
        if i % 10_000 == 5_000 { format!("let value_{i} = changed({i});") } else { format!("let value_{i} = compute({i});") }
    }));
    timed("every fifth line of 100k changed", &base, &file(n, |i| {
        if i % 5 == 0 { format!("let value_{i} = changed({i});") } else { format!("let value_{i} = compute({i});") }
    }));
    timed("every other line of 100k changed", &base, &file(n, |i| {
        if i % 2 == 0 { format!("let value_{i} = changed({i});") } else { format!("let value_{i} = compute({i});") }
    }));
    timed("the middle 50k of 100k deleted", &base, &{
        let v: Vec<&str> = base.lines().enumerate().filter(|(i, _)| !(25_000..75_000).contains(i)).map(|(_, l)| l).collect();
        v.join("\n") + "\n"
    });
    // Two files that are both source code and otherwise unrelated: braces and
    // blank lines in common, and nothing else.
    let code = |tag: &str| file(20_000, |i| match i % 6 { 0 => String::new(), 1 => "}".into(), 2 => "    }".into(), _ => format!("    {tag}_{i}();") });
    timed("20k lines of code replaced by other code", &code("old"), &code("new"));
    timed("100k lines against 100k unrelated ones", &base, &file(n, |i| format!("something else entirely {i}")));
    timed("170k identical lines against 170k others", &file(170_000, |_| "a".into()), &file(170_000, |_| "b".into()));
    timed("100k lines reversed", &base, &{
        let mut v: Vec<&str> = base.lines().collect();
        v.reverse();
        v.join("\n") + "\n"
    });

    // Blocks that share nothing, fenced by lines that are shared: every block
    // costs a search that runs to the cap before it can be given up on.
    let blocks = |tag: &str| file(170_000, |i| if i % 2_200 == 0 { format!("fence {i}") } else { format!("{tag}{i}") });
    timed("77 unrelated blocks between shared lines", &blocks("a"), &blocks("b"));

    // No line is unique, so there is nothing to divide by.
    let rep = |shift: usize| file(120_000, |i| format!("row {}", (i + shift) % 40));
    timed("120k lines of a 40-line pattern, shifted", &rep(0), &rep(7));
    timed("the same, with every 50th line changed", &rep(0), &file(120_000, |i| {
        if i % 50 == 0 { format!("edited {i}") } else { format!("row {}", i % 40) }
    }));
}

fn git(args: &[&str]) -> Vec<u8> {
    Command::new("git").args(args).output().expect("git").stdout
}

fn history(commits: &str) {
    let log = String::from_utf8(git(&["log", "--no-merges", "-n", commits, "--format=%H"])).unwrap();
    let (mut files, mut worse, mut blocks) = (0usize, 0usize, 0usize);
    let mut slowest = (0f64, String::new());
    for c in log.lines() {
        let stat = git(&["diff-tree", "--no-commit-id", "-r", "--diff-filter=M", "--numstat", c]);
        for row in String::from_utf8_lossy(&stat).lines() {
            let mut cols = row.split('\t');
            let (Some(ga), Some(gr), Some(path)) = (cols.next(), cols.next(), cols.next()) else { continue };
            let (Ok(ga), Ok(gr)) = (ga.parse::<usize>(), gr.parse::<usize>()) else { continue }; // binary
            let old = git(&["show", &format!("{c}^:{path}")]);
            let new = git(&["show", &format!("{c}:{path}")]);
            if old.len().max(new.len()) > 512 * 1024 {
                continue; // the hub does not diff these either
            }
            let began = Instant::now();
            let d = diff(&old, &new);
            let ms = began.elapsed().as_secs_f64() * 1e3;
            files += 1;
            if ms > slowest.0 {
                slowest = (ms, format!("{} {path}", &c[..8]));
            }
            blocks += d.truncated as usize;
            // Git's default diff is not minimal either, so a few lines' slack.
            if d.added + d.removed > ga + gr + (ga + gr) / 10 + 4 {
                worse += 1;
                println!("{} {path}: +{} -{} here, +{ga} -{gr} in git", &c[..8], d.added, d.removed);
            }
        }
    }
    println!("{files} modified files; {worse} shown as a larger change than git shows, {blocks} with a part replaced as a block");
    println!("slowest: {:.2} ms  {}", slowest.0, slowest.1);
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("lines") => lines(),
        Some("git") => history(&args.next().unwrap_or("300".into())),
        _ => eprintln!("usage: diffbench lines | diffbench git [commits]"),
    }
}
