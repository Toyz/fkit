/**
 * The repository page: file browser, blob view, history, commit diffs, settings.
 *
 * This is a single component registered on the catch-all `*` route rather than
 * one component per view. Two reasons:
 *
 *  1. Loom's route patterns match a single segment per `:param` and have no
 *     splat, but file paths contain slashes. Parsing the path here handles
 *     `/owner/repo/blob/main/crates/core/src/lib.rs` cleanly.
 *  2. The repo header, branch picker and tabs stay mounted while you navigate
 *     between files, so sub-navigation does not refetch repository metadata or
 *     flash the whole page.
 */
import { LoomElement, component, css, styles, reactive, mount, on, inject, watch } from "@toyz/loom";
// Renamed: importing it as `fetch` would shadow the global in this module.
import { fetch as query, type ApiState } from "@toyz/loom/query";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { settingsLayout } from "../ui-settings";
import {
  api,
  authorName,
  humanSize,
  syncUrl,
  relativeTime,
  type Comment,
  type Issue,
  type GcReport,
  type BlobResponse,
  type TreeResponse,
  type Commit,
  type CommitDetail,
  type Entry,
  type Collaborator,
  type Comparison,
  type FileDiff,
  type MergeRequest,
  type MergeRequestDetail,
  type LastCommit,
  type Patch,
  type Ref,
  type RepoStats,
  type Repo,
} from "../api";
import { linkHandler, go } from "../nav";
import { renderMarkdown, type MarkdownContext } from "../markdown";
import { Session } from "../session";
import { highlight, languageFor, type Tok } from "../highlight";
import "../components/branch-picker";
import "../components/clone-button";
import "../components/fkit-select";
import "../components/fkit-choice";
import { adoptInto } from "../adopt";
import { dirIcon, fileIcon } from "../file-icon";
import { confirmAction } from "../components/fkit-dialog";

type View =
  | { kind: "tree"; ref: string; path: string }
  | { kind: "blob"; ref: string; path: string }
  | { kind: "commits"; ref: string }
  | { kind: "commit"; hash: string }
  | { kind: "compare"; base: string; head: string }
  | { kind: "tags" }
  | { kind: "merges" }
  | { kind: "merge"; number: number }
  | { kind: "issues" }
  | { kind: "issue"; number: number }
  | { kind: "settings"; section: string }
  | { kind: "unknown" };


/**
 * The `<ref>/<path>` suffix the content endpoints share.
 *
 * One function because three queries build the same string, and three copies
 * is three chances for one to encode a segment differently from the others —
 * which would quietly give that query a cache key of its own.
 */
function refAndPath(el: PageRepo): string {
  const v = el.loc!.view;
  const path = "path" in v ? v.path : "";
  return (
    encodeURIComponent(el.refName()) +
    (path ? "/" + path.split("/").map(encodeURIComponent).join("/") : "")
  );
}

/** True once there is a repository, and the view is a directory listing. */
function onTree(el: PageRepo): boolean {
  return Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "tree");
}

/** Parse `/owner/repo/<kind>/<ref>/<path…>` out of the current location. */
function parse(): { owner: string; name: string; view: View } | null {
  const segs = location.pathname.split("/").filter(Boolean).map(decodeURIComponent);
  if (segs.length < 2) return null;
  const [owner, name, kind, ...rest] = segs;

  if (!kind) return { owner, name, view: { kind: "tree", ref: "", path: "" } };
  if (kind === "settings") {
    return { owner, name, view: { kind: "settings", section: rest[0] ?? "general" } };
  }
  if (kind === "tags") return { owner, name, view: { kind: "tags" } };
  if (kind === "issues") {
    const n = rest[0] ? Number(rest[0]) : NaN;
    return {
      owner,
      name,
      view: Number.isFinite(n) ? { kind: "issue", number: n } : { kind: "issues" },
    };
  }
  if (kind === "merges") {
    const n = rest[0] ? Number(rest[0]) : NaN;
    return {
      owner,
      name,
      view: Number.isFinite(n) ? { kind: "merge", number: n } : { kind: "merges" },
    };
  }
  if (kind === "commit" && rest[0]) return { owner, name, view: { kind: "commit", hash: rest[0] } };
  // A commit list has no path after the ref, so the whole remainder is the
  // name — which is what makes `/commits/feature/thing` work without having to
  // wait for the ref list the way a tree does.
  if (kind === "commits") return { owner, name, view: { kind: "commits", ref: rest.join("/") } };
  if (kind === "compare") {
    // GitHub's spelling: /compare/base...head. Falling back to an empty head
    // lets /compare/<base> mean "pick something to compare against".
    const spec = rest.join("/");
    const [b, h] = spec.includes("...") ? spec.split("...") : [spec, ""];
    return { owner, name, view: { kind: "compare", base: b ?? "", head: h ?? "" } };
  }
  if (kind === "tree" || kind === "blob") {
    const [ref, ...path] = rest;
    return { owner, name, view: { kind, ref: ref ?? "", path: path.join("/") } };
  }
  return { owner, name, view: { kind: "unknown" } };
}

/**
 * Split `<ref>/<path>` when the ref itself contains slashes.
 *
 * `/owner/repo/tree/feature/settings-redesign/web` is ambiguous on its face:
 * the branch could be `feature` with path `settings-redesign/web`, or the
 * branch `feature/settings-redesign` with path `web`. Nothing in the URL says
 * which, so [`parse`] guesses the shortest ref and this corrects it once the
 * ref list has actually arrived.
 *
 * Only wrong guesses are corrected. If the first segment really is a ref, that
 * reading stands — otherwise a repository with both `feature` and
 * `feature/x` would lose the ability to browse a directory called `x` on
 * `feature`, and every link that already worked would change meaning.
 */
function widenRef(known: string[], ref: string, path: string): { ref: string; path: string } {
  if (!ref || !path || known.includes(ref)) return { ref, path };

  const segs = path.split("/");
  // Longest first: with both `a/b` and `a/b/c` present, the deeper name is the
  // one the URL was more specific about.
  for (let take = segs.length; take >= 1; take--) {
    const candidate = [ref, ...segs.slice(0, take)].join("/");
    if (known.includes(candidate)) {
      return { ref: candidate, path: segs.slice(take).join("/") };
    }
  }
  return { ref, path };
}

/**
 * Code rows, adopted into `<loom-virtual>`'s shadow root as well as used by the
 * non-virtualized path, so one set of rules covers both.
 *
 * The gutter is `position: sticky; left: 0` — scroll a long line sideways and
 * the line numbers stay put instead of sliding away, which is the difference
 * between a code viewer and a `<pre>` in a box.
 */
const codeSheet = css`
  .cl { display: flex; font-size: 12px; line-height: 19px; min-height: 19px; }
  .cl:hover { background: var(--raised); }
  .cl:hover .ln { color: var(--muted); }
  .ln {
    flex: none;
    position: sticky; left: 0; z-index: 1;
    text-align: right;
    padding-right: 14px;
    color: var(--gutter-fg);
    background: var(--gutter-bg);
    user-select: none;
    font-variant-numeric: tabular-nums;
    font-size: 11px;
  }
  .src { white-space: pre; padding: 0 16px 0 14px; }

  .cm { color: var(--tok-comment); font-style: italic; }
  .st { color: var(--tok-string); }
  .nu { color: var(--tok-number); }
  .kw { color: var(--tok-keyword); }
  .ty { color: var(--tok-type); }
  .fn { color: var(--tok-fn); }
  .pu { color: var(--tok-punct); }
`;

/* Shared with `loom-virtual`'s shadow root, so one set of rules covers the
   grouped list and the virtualized one. */
const commitSheet = css`
  /* The rail is drawn on the row, not built from an element inside it: one
     fewer node per commit, and nothing that can be laid out into the wrong
     grid row. */
  .c {
    position: relative;
    display: grid; grid-template-columns: minmax(0, 1fr) auto;
    gap: 10px; align-items: center;
    padding: 7px 12px 7px 34px;
    border-bottom: 1px solid var(--border);
  }
  .c::before {
    content: ""; position: absolute; left: 18px; top: 0; bottom: 0;
    width: 1px; background: var(--border);
  }
  .c::after {
    content: ""; position: absolute; left: 15px; top: 50%;
    width: 7px; height: 7px; margin-top: -3.5px;
    border-radius: 50%; background: var(--faint);
    box-shadow: 0 0 0 3px var(--surface);
  }
  .c:hover::after { background: var(--accent); box-shadow: 0 0 0 3px var(--raised); }
  /* The line starts at the first commit of a day and stops at the last one,
     rather than running out of the panel at either end. */
  .day + .c::before { top: 50%; }
  .c:last-child::before { bottom: 50%; }
  .c:last-child { border-bottom: 0; }
  .c:hover { background: var(--raised); }

  /* A continuous line with a node per commit: history reads as a sequence
     rather than as a table that happens to be in order. */

  .body { min-width: 0; display: flex; flex-direction: column; gap: 2px; }
  .m {
    color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    text-decoration: none; font-size: 12.5px;
  }
  .m:hover { color: var(--accent); }
  .by { color: var(--faint); font-size: 11px; font-family: var(--sans); }

  .acts { display: flex; align-items: center; gap: 10px; }
  .sha {
    color: var(--muted); font-size: 11.5px; text-decoration: none;
    font-variant-numeric: tabular-nums;
  }
  .sha:hover { color: var(--accent); }
  .ghost { color: var(--faint); display: flex; padding: 2px; }
  .ghost:hover { color: var(--accent); }

  /* A date header, not a row: no hover, no border, and it carries the rail
     through so the line is unbroken between days. */
  .day {
    display: flex; align-items: center; gap: 8px;
    padding: 8px 12px 8px 14px;
    font-size: 11px; color: var(--faint);
    background: var(--raised);
    border-bottom: 1px solid var(--border);
  }
  .day loom-icon { opacity: .8; }
  .day .n {
    margin-left: auto; font-variant-numeric: tabular-nums;
  }
  .empty { padding: 30px 14px; text-align: center; color: var(--faint); font-size: 12px; }
`;

const sheet = css`
  /* Header reads like a path, because that is what it is. */
  .head { border-bottom: 1px solid var(--border); margin-bottom: 12px; }
  .title { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding-bottom: 8px; }
  .title .ic { color: var(--faint); display: flex; }
  .title .p { font-size: 15px; font-weight: 600; }
  .title .p .own { color: var(--muted); font-weight: 400; }
  .desc { font-family: var(--sans); color: var(--muted); font-size: 12px; margin: -4px 0 8px; }

  .tabs { display: flex; gap: 2px; }
  .tabs a {
    display: flex; align-items: center; gap: 6px;
    padding: 5px 10px; color: var(--muted); font-size: 12px;
    border-bottom: 2px solid transparent; margin-bottom: -1px;
  }
  .tabs a loom-icon { opacity: .7; }
  .tabs a.on loom-icon { opacity: 1; color: var(--accent); }
  .tabs a:hover { color: var(--text); text-decoration: none; }
  .tabs a.on { color: var(--text); border-bottom-color: var(--accent); }

  .toolbar { display: flex; align-items: center; gap: 8px; margin-bottom: 8px; flex-wrap: wrap; }
  select.branch { width: auto; font-size: 12px; padding: 3px 6px; }
  .crumbs { display: flex; align-items: center; gap: 4px; flex-wrap: wrap; font-size: 13px; }
  .crumbs .sep { color: var(--faint); }
  .crumbs .cur { font-weight: 600; }

  /* ---- file rows: fixed height, aligned size column ---- */
  /* name | last commit | when | size — the commit column is the widest thing
     that is still optional, so it gets the flexible track and truncates. */
  /* The bar above the file list: who touched this last, and what they said.
     Sits directly on top of the panel, sharing its border, so the two read as
     one object rather than as a strip floating above a box. */
  .latest {
    display: flex; align-items: center; gap: 10px;
    padding: 7px 12px; font-size: 12px;
    background: var(--raised);
    border: 1px solid var(--border);
    border-bottom: 0;
    border-radius: var(--radius) var(--radius) 0 0;
  }
  .latest .who { color: var(--text); flex: none; }
  .latest .msg {
    color: var(--muted); font-family: var(--sans);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    flex: 1; min-width: 0;
  }
  .latest .msg:hover { color: var(--accent); }
  .latest .sha {
    color: var(--muted); font-size: 11.5px; flex: none;
    font-variant-numeric: tabular-nums;
  }
  .latest .sha:hover { color: var(--accent); }
  .latest .when { color: var(--faint); font-size: 11px; flex: none; }
  .latest .count {
    display: inline-flex; align-items: center; gap: 6px;
    color: var(--muted); font-size: 11.5px; flex: none;
    padding-left: 12px; border-left: 1px solid var(--border);
  }
  .latest .count:hover { color: var(--text); text-decoration: none; }
  .latest .count b { color: var(--text); font-weight: 400; }
  /* The list below it loses its own top corners so the seam is invisible. */
  .latest + .panel.files { border-radius: 0 0 var(--radius) var(--radius); }
  @media (max-width: 700px) {
    .latest .sha, .latest .when { display: none; }
  }

  /* The columns live on the panel and the rows adopt them with subgrid.
     Each row used to be its own grid, so an auto-width name column sized to
     that row's own filename and every message started somewhere different —
     six distinct left edges in a twenty-file listing. A grid only aligns
     columns within one container, so the container has to be the list.
     (The previous 8–22rem band avoided this by making the column a fixed
     width, at the cost of parking every message mid-row.) */
  .panel.files {
    display: grid;
    /* The icon column is sized for the glyph, not the other way round: a
       loom-icon has no intrinsic width, so left to itself it collapsed to a
       3px sliver in the subgrid cell. */
    grid-template-columns: 34px auto minmax(0, 1fr) 92px 72px;
    column-gap: 12px;
  }
  .files .r {
    display: grid;
    grid-template-columns: subgrid;
    grid-column: 1 / -1;
    /* Stated again rather than inherited: a subgrid is supposed to take the
       parent's gutters, and this one did not — the icon ended up flush
       against the filename. */
    column-gap: 12px;
    align-items: center;
    height: var(--row); padding: 0 12px;
    border-bottom: 1px solid var(--border);
  }
  .files .fn { white-space: nowrap; }
  .files .sz {
    color: var(--faint); font-size: 11px; text-align: right;
    font-variant-numeric: tabular-nums; white-space: nowrap;
  }
  .files .sz.sum { opacity: .62; }
  .files .when { text-align: right; }
  .files .msg {
    color: var(--muted); font-size: 12px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    text-decoration: none;
  }
  .files a.msg:hover { color: var(--accent); }
  .files .when { color: var(--faint); font-size: 11px; white-space: nowrap; }
  @media (max-width: 900px) {
    .files .r { grid-template-columns: 15px minmax(0, 1fr) auto; }
    .files .msg, .files .when { display: none; }
  }
  .files .r:last-child { border-bottom: 0; }
  .files .r:hover { background: var(--raised); }
  /* The space after the glyph is inside this cell rather than a grid gap:
     the subgrid did not take the parent's column gutters, so the icon sat
     flush against the filename. Padding here does not depend on that. */
  .files .ic {
    color: var(--faint);
    display: flex; align-items: center; justify-content: flex-start;
    width: 34px; height: 18px;
  }
  .files .ic loom-icon { display: block; width: 14px; height: 14px; }
  .files .ic.d { color: var(--accent); }
  /* Executable is worth noticing but is not a warning. */
  .files .ic.x { color: var(--added); }
  .files a { color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .files a:hover { color: var(--accent); }
  .files .sz { color: var(--faint); font-size: 11px; font-variant-numeric: tabular-nums; }

  /* ---- blob ----
     Row rules live in codeSheet so the plain and virtualized paths share them. */
  .code { overflow-x: auto; }

  /* The virtual list scrolls inside itself, so give it a real height. */
  .vcode { display: block; height: calc(100vh - 230px); min-height: 320px; }
  .vlist { display: block; height: calc(100vh - 210px); min-height: 320px; }

  /* ---- commits ---- (rules in commitSheet, shared with the virtual path) */
  .commits .c:last-child { border-bottom: 0; }

  /* ---- diff ---- */
  .ch {
    display: grid; grid-template-columns: 12px minmax(0, 1fr) auto;
    gap: 10px; align-items: center; height: var(--row); padding: 0 12px;
    border-bottom: 1px solid var(--border); font-size: 12px;
  }
  .ch:last-child { border-bottom: 0; }
  .st { font-weight: 700; text-align: center; }
  .st.added { color: var(--added); }
  .st.removed { color: var(--removed); }
  .st.modified { color: var(--modified); }
  .st.typechanged { color: var(--muted); }
  .delta { color: var(--faint); font-size: 11px; font-variant-numeric: tabular-nums; }

  /* ---- collaborators ---- */
  .collab-add { display: flex; gap: 8px; align-items: center; }
  .collab-add input { flex: 1; }
  .ok { color: var(--added); font-size: 12px; }
  .sec .panel { margin-top: 12px; }
  .sec > h1 + .lead + .panel, .sec > h1 + .panel { margin-top: 12px; }
  form.stack { display: flex; flex-direction: column; gap: 13px; }
  .fd {
    color: var(--muted); font-size: 11.5px; font-family: var(--sans);
    margin-top: 4px; line-height: 1.45;
  }
  /* Two columns: the tree, and what the repository is. Collapses rather than
     squeezing — a 200px sidebar is worse than no sidebar. */
  .split { display: grid; grid-template-columns: minmax(0, 1fr) 250px; gap: 20px; }
  @media (max-width: 900px) {
    .split { grid-template-columns: minmax(0, 1fr); }
    .aside { order: -1; }
  }
  .aside section { padding-bottom: 15px; margin-bottom: 15px; border-bottom: 1px solid var(--border); }
  .aside section:last-child { border-bottom: 0; margin-bottom: 0; padding-bottom: 0; }
  .aside h3 {
    display: flex; align-items: center; gap: 7px;
    font-size: 10.5px; font-weight: 600; margin: 0 0 8px;
    text-transform: uppercase; letter-spacing: .07em; color: var(--faint);
  }
  .aside h3 a { color: inherit; }
  .aside h3 a:hover { color: var(--accent); text-decoration: none; }
  .aside h3 .n {
    margin-left: auto; color: var(--muted);
    font-variant-numeric: tabular-nums; letter-spacing: 0;
  }
  .aside .prose {
    font-family: var(--sans); font-size: 12.5px; color: var(--muted);
    margin: 0 0 10px; line-height: 1.5;
  }
  .aside .prose.faint { color: var(--faint); }

  /* Label/value pairs, aligned — the same shape as the stats panels. */
  .aside .facts {
    display: grid; grid-template-columns: auto 1fr; gap: 3px 10px; margin: 0;
    font-size: 11.5px;
  }
  .aside .facts dt { color: var(--faint); }
  .aside .facts dd { margin: 0; color: var(--muted); text-align: right; }
  .aside .facts dd.mono { font-family: var(--mono); }

  /* README / LICENSE / CONTRIBUTING over the rendered document, which is
     where a forge puts them — they are things to read, not facts about the
     repository, so a sidebar list was the wrong shelf. */
  .doctabs {
    display: flex; align-items: center; gap: 2px;
    padding: 0 8px;
    background: var(--raised);
    border-bottom: 1px solid var(--border);
  }
  .doctabs button {
    display: inline-flex; align-items: center; gap: 6px;
    font: inherit; font-size: 11.5px; height: 30px; padding: 0 9px;
    background: transparent; border: 0; border-bottom: 2px solid transparent;
    color: var(--muted); cursor: pointer;
  }
  .doctabs button:hover { color: var(--text); background: transparent; }
  .doctabs button.on { color: var(--text); border-bottom-color: var(--accent); }
  .doctabs button.on loom-icon { color: var(--accent); }
  .doctabs .grow { flex: 1; }
  .doctabs .val {
    font-size: 11px; color: var(--faint); padding-right: 4px;
  }

  /* The homepage, as a link that shows where it goes rather than its scheme. */
  .aside .home {
    display: inline-flex; align-items: center; gap: 6px;
    font-size: 12px; color: var(--accent); margin-bottom: 10px;
    max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .aside .home loom-icon { flex: none; opacity: .8; }

  /* Topics: labels, not buttons — nothing here is clickable yet, and a chip
     that looks pressable and is not is worse than plain text. */
  .aside .topics { display: flex; flex-wrap: wrap; gap: 5px; margin-bottom: 11px; }
  .aside .topic {
    font-size: 11px; padding: 1px 7px; line-height: 17px;
    border: 1px solid var(--border-hi); border-radius: 999px;
    color: var(--muted); background: var(--raised);
  }

  .aside .docs li { grid-template-columns: 12px minmax(0, 1fr); }

  .aside .mini { list-style: none; margin: 0; padding: 0; }
  .aside .mini li {
    display: grid; grid-template-columns: 12px minmax(0, 1fr) auto;
    align-items: center; gap: 7px; height: 22px; font-size: 12px;
  }
  .aside .mini loom-icon { color: var(--faint); }
  .aside .mini a { color: var(--accent); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .aside .mini .when { color: var(--faint); font-size: 10.5px; white-space: nowrap; }
  .aside .more { display: inline-block; margin-top: 7px; font-size: 11.5px; color: var(--muted); }
  .aside .more:hover { color: var(--accent); }

  /* Images sit on a chequerboard, so transparency reads as transparency
     rather than as whatever the page background happens to be. */
  .imgview {
    display: grid; place-items: center;
    padding: 24px; min-height: 120px;
    background-color: var(--bg);
    background-image:
      linear-gradient(45deg, var(--raised) 25%, transparent 25%),
      linear-gradient(-45deg, var(--raised) 25%, transparent 25%),
      linear-gradient(45deg, transparent 75%, var(--raised) 75%),
      linear-gradient(-45deg, transparent 75%, var(--raised) 75%);
    background-size: 16px 16px;
    background-position: 0 0, 0 8px, 8px -8px, -8px 0;
  }
  .imgview img {
    max-width: 100%; max-height: 70vh;
    /* Nearest-neighbour would be wrong for photos; this keeps small sprites
       from turning to mush without wrecking anything else. */
    image-rendering: auto;
    border: 1px solid var(--border);
    background: var(--surface);
  }

  /* "12 tags" beside the ref picker — a count that is also the way in, which
     is how GitHub surfaces them. Not a button: it is a fact with a link on it. */
  .refcount {
    display: inline-flex; align-items: center; gap: 6px;
    font-size: 12px; color: var(--muted); white-space: nowrap;
  }
  .refcount:hover { color: var(--text); text-decoration: none; }
  .refcount b { color: var(--text); font-weight: 400; }
  .refcount loom-icon { opacity: .75; }

  /* Tags: one line each, same rhythm as a file row. */
  .tagrow {
    display: grid; grid-template-columns: 16px minmax(0, 1fr) auto auto;
    align-items: center; gap: 10px;
    height: var(--row); padding: 0 12px;
    border-bottom: 1px solid var(--border);
  }
  .tagrow:last-child { border-bottom: 0; }
  .tagrow:hover { background: var(--raised); text-decoration: none; }
  .tagrow .ic { color: var(--faint); display: flex; }
  .tagrow .nm {
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    color: var(--accent);
  }
  .tagrow .msg {
    font-family: var(--sans); color: var(--muted); font-size: 12px; margin-left: 10px;
  }
  .tagrow .sha {
    font-size: 11.5px; color: var(--muted); font-variant-numeric: tabular-nums;
    cursor: pointer;
  }
  .tagrow .sha:hover { color: var(--accent); text-decoration: underline; }
  .tagrow .when { color: var(--faint); font-size: 11px; white-space: nowrap; }

  .collab-note {
    color: var(--muted); font-size: 11.5px; font-family: var(--sans);
    margin-top: 9px; line-height: 1.5;
  }
  .collab {
    display: grid; grid-template-columns: minmax(0, 1fr) auto auto auto;
    align-items: center; gap: 12px;
    padding: 0 14px; height: var(--row); border-top: 1px solid var(--border);
  }
  .collab .cu { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .collab-empty, .loading {
    padding: 12px 14px; color: var(--muted); font-size: 12px;
    border-top: 1px solid var(--border); font-family: var(--sans);
  }

  .gc-report {
    font-family: var(--sans); font-size: 11.5px; color: var(--muted);
    margin: 12px 0 0; line-height: 1.5; max-width: 78ch;
  }

  /* A count riding a tab. Quiet by default; it picks up the accent on the
     tab you are actually on, the same way the label does. */
  .tabs .tabn {
    display: inline-flex; align-items: center; justify-content: center;
    min-width: 16px; height: 16px; padding: 0 5px; margin-left: 2px;
    border-radius: 999px; background: var(--raised); color: var(--muted);
    font-size: 10.5px; font-variant-numeric: tabular-nums;
  }
  .tabs a.on .tabn { background: var(--accent-weak); color: var(--accent); }

  /* ---- issues ---- */
  .seg { display: flex; border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden; }
  .seg button {
    border: 0; border-radius: 0; background: transparent; color: var(--muted);
    font-size: 11.5px; padding: 3px 10px; height: 24px;
  }
  .seg button + button { border-left: 1px solid var(--border); }
  .seg button:hover { background: var(--raised); color: var(--text); }
  .seg button.on { background: var(--raised); color: var(--text); }

  /* The row component centres one line; an issue needs two, so the whole of
     it goes in the slot rather than fighting the name/meta props. */
  .issue-line { display: flex; flex-direction: column; flex: 1; min-width: 0; gap: 2px; }
  .issue-line .t { font-size: 13px; color: var(--text); text-decoration: none; }
  .issue-line .t:hover { color: var(--accent); }
  .issue-line .sub { font-size: 11.5px; color: var(--faint); }
  .cbadge { display: flex; align-items: center; gap: 4px; color: var(--faint); font-size: 11.5px; }

  .issue-meta {
    font-family: var(--sans); font-size: 12px; color: var(--muted);
    margin: -4px 0 4px;
  }
  .new-issue fkit-field:last-of-type { margin-bottom: 0; }

  /* ---- reviewing a change ----------------------------------------------
   *
   * A file tree beside the diff rather than above it. Twenty stacked panels
   * told you nothing about the shape of a change and gave you no way to reach
   * the one file you came for; the tree is the map, and it stays put while the
   * diff scrolls under it.
   */
  .review {
    display: grid;
    grid-template-columns: 264px minmax(0, 1fr);
    gap: 18px;
    align-items: start;
    margin-top: 14px;
  }
  @media (max-width: 1000px) {
    .review { grid-template-columns: 1fr; }
    .review fkit-file-tree { position: static; }
  }
  .review fkit-file-tree { position: sticky; top: 52px; }

  /* Scrolled to from the tree: the sticky header must not land on top of the
     filename you just asked for. */
  .df { scroll-margin-top: 56px; }

  /* ---- a comment pinned to a line ---- */
  .dl { position: relative; }
  /* The gutter button only appears on the row under the pointer — a plus on
     every line of a thousand-line diff is noise, not an affordance. */
  .dl .addc {
    position: absolute; left: 2px; top: 50%; transform: translateY(-50%);
    width: 17px; height: 17px; padding: 0; display: none;
    align-items: center; justify-content: center;
    border: 0; border-radius: 3px; cursor: pointer;
    background: var(--accent); color: var(--bg); z-index: 2;
  }
  .dl:hover .addc { display: flex; }
  .dl .addc:hover { filter: brightness(1.12); }

  .thread {
    padding: 10px 12px 12px calc(var(--gutter, 96px));
    border-top: 1px solid var(--border);
    border-bottom: 1px solid var(--border);
    background: var(--surface);
    display: flex; flex-direction: column; gap: 9px;
  }
  .thread .stale {
    font-family: var(--sans); font-size: 11.5px; color: var(--modified);
  }
  .thread-out {
    display: flex; flex-direction: column; gap: 9px;
    padding: 11px 12px; border-top: 1px solid var(--border);
    background: var(--surface);
  }
  .thread-out .where {
    font-size: 11.5px; color: var(--muted); font-family: var(--sans);
  }
  .thread-out .where b { font-family: var(--mono); font-weight: 400; color: var(--text); }

  /* ---- the conversation under a merge request or an issue ---- */
  .talk { display: flex; flex-direction: column; gap: 12px; margin-top: 18px; }
  .talk .none {
    font-family: var(--sans); font-size: 12.5px; color: var(--faint);
    padding: 4px 0;
  }

  /* ---- setup instructions ---- */
  /* A centred column: the panel is full width but the instructions are a
     reading measure, and left-anchoring them left a dead half-screen. */
  .setup { display: flex; flex-direction: column; gap: 14px; }
  .setup-block { display: flex; flex-direction: column; gap: 5px; }
  /* Label and copy button share a baseline above the block, so the button is
     where the eye already is rather than floating over the code. */
  .setup-label {
    display: flex; align-items: baseline; justify-content: space-between; gap: 8px;
    font-size: 11px; text-transform: uppercase; letter-spacing: .07em; color: var(--muted);
  }
  .setup-label button { font-size: 11px; text-transform: none; letter-spacing: 0; }
  fkit-list .cmd-block { border: 0; border-radius: 0; padding: 11px 14px; background: transparent; }
  .cmd-block {
    margin: 0; padding: 9px 12px; overflow-x: auto;
    background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius);
    font-family: var(--mono); font-size: 12px; line-height: 1.7; color: var(--text);
    white-space: pre;
  }
  .cmd-block.url { color: var(--accent); }
  .setup-note {
    display: flex; align-items: flex-start; gap: 8px;
    color: var(--muted); font-size: 12px; font-family: var(--sans); line-height: 1.5;
    padding-top: 3px;
  }
  .setup-note loom-icon { flex: none; margin-top: 3px; }
  .setup-note code { font-family: var(--mono); font-size: 11px; }

  /* ---- commit header ----
     One block. The summary is the only large type; author, age and hash sit on
     a single quiet line beneath it rather than in a second bordered box. */
  .chead {
    border: 1px solid var(--border); border-radius: var(--radius);
    background: var(--surface); padding: 13px 15px; margin-bottom: 12px;
  }
  .chead-top { display: flex; align-items: flex-start; gap: 12px; }
  .csummary { font-size: 14px; font-weight: 600; flex: 1; line-height: 1.4; }
  .cbody {
    margin: 9px 0 0; white-space: pre-wrap; color: var(--muted);
    font-size: 12px; line-height: 1.55;
  }
  .cmeta {
    display: flex; align-items: center; gap: 14px; flex-wrap: wrap;
    margin-top: 11px; padding-top: 10px; border-top: 1px solid var(--border);
    color: var(--faint); font-size: 11px;
  }
  .cmeta .who { color: var(--muted); }
  .cmeta .hash { color: var(--muted); }
  .cmeta .parent a { font-size: 11px; }

  /* ---- compare ---- */
  .cmp-bar {
    display: flex; align-items: center; gap: 9px; flex-wrap: wrap;
    padding: 9px 12px; margin-bottom: 12px;
    border: 1px solid var(--border); border-radius: var(--radius);
    background: var(--surface); font-size: 12px;
  }

  /* The verdict is the whole point of the page, so it gets the only coloured
     rail on the screen and sits above everything else. */
  .verdict {
    display: flex; align-items: flex-start; gap: 11px;
    padding: 12px 14px; margin-bottom: 12px;
    border: 1px solid var(--border); border-left-width: 2px;
    border-radius: var(--radius); background: var(--surface);
  }
  .verdict.ok  { border-left-color: var(--added); }
  .verdict.bad { border-left-color: var(--removed); }
  .verdict .vmark { display: flex; margin-top: 1px; }
  .verdict.ok  .vmark { color: var(--added); }
  .verdict.bad .vmark { color: var(--removed); }
  .vtitle { font-size: 13px; font-weight: 600; }
  .vsub { color: var(--muted); font-size: 11px; margin-top: 3px; }
  .howto {
    font-size: 11px; color: var(--muted); background: var(--bg);
    border: 1px solid var(--border); border-radius: var(--radius);
    padding: 4px 8px; white-space: nowrap;
  }

  /* ---- merge requests ---- */
  .mrow {
    display: grid; grid-template-columns: auto minmax(0, 1fr) auto auto;
    align-items: center; gap: 12px;
    padding: 0 12px; height: 34px; border-bottom: 1px solid var(--border);
  }
  .mrow:last-child { border-bottom: 0; }
  .mrow:hover { background: var(--raised); }
  .mtitle {
    color: var(--text); text-decoration: none;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .mtitle:hover { color: var(--accent); }
  .num { color: var(--faint); }
  .mbr { color: var(--muted); font-size: 11px; white-space: nowrap; }

  /* State is the first thing scanned in a list of requests, so it gets a solid
     chip rather than the hairline tag used elsewhere. */
  .mstate {
    font-size: 10px; text-transform: uppercase; letter-spacing: .07em;
    padding: 2px 7px; border-radius: 2px; white-space: nowrap;
    border: 1px solid transparent;
  }
  .mstate.open   { color: var(--added);    border-color: var(--added); }
  .mstate.merged { color: var(--bg);       background: var(--accent); border-color: var(--accent); }
  .mstate.closed { color: var(--muted);    border-color: var(--border-hi); }

  /* ---- patch ----
     Two gutters, old line and new line, because a single number cannot tell you
     where a line sits on both sides. Added and removed rows are tinted rather
     than only marked with a sigil: the eye finds a colour band down the page
     faster than it finds a column of + and -. */
  .patch-bar {
    display: flex; align-items: center; gap: 12px;
    padding: 7px 12px; margin-bottom: 10px;
    border: 1px solid var(--border); border-radius: var(--radius);
    background: var(--surface); font-size: 12px; color: var(--muted);
  }
  .plus  { color: var(--added); }
  .minus { color: var(--removed); }

  /* One file's diff. Named .df, not .fd — the shared settings sheet uses .fd
     for a field description, and the later sheet wins, which put a bordered
     surface around every line of help text on the settings tabs. */
  .df {
    border: 1px solid var(--border); border-radius: var(--radius);
    background: var(--surface); margin-bottom: 10px; overflow: hidden;
  }
  .df-head {
    display: flex; align-items: center; gap: 8px;
    padding: 6px 10px; background: var(--raised);
    border-bottom: 1px solid var(--border); font-size: 12px;
  }
  .df-toggle { padding: 2px 4px; }
  /* The rotation is a class, not a style= prop on the icon. LoomIcon sizes
     itself by writing --_s/--_c/--_f/--_sw as inline styles on its own host,
     and loom's JSX sets style with setAttribute, which replaces the whole
     declaration -- so a style prop there wipes the icon's own size and it
     renders at the SVG's intrinsic size instead of 12px. It only appears after
     a toggle: applyVars() re-runs on the icon's own render and on its size
     watcher, and neither fires when just the parent re-renders. */
  .df-toggle loom-icon { transition: transform .12s; display: block; }
  .df-toggle loom-icon.closed { transform: rotate(-90deg); }
  .df-path { color: var(--text); text-decoration: none; overflow: hidden;
             text-overflow: ellipsis; white-space: nowrap; }
  a.df-path:hover { color: var(--accent); }
  .df-head .counts { margin-left: auto; display: flex; gap: 8px; font-variant-numeric: tabular-nums; }
  .df-note { padding: 10px 12px; color: var(--muted); font-size: 12px; font-family: var(--sans); }
  .df-body { overflow-x: auto; }

  .hh {
    padding: 3px 12px; color: var(--faint); font-size: 11px;
    background: var(--gutter-bg); border-top: 1px solid var(--border);
    border-bottom: 1px solid var(--border);
  }
  .df-body > div:first-child .hh { border-top: 0; }

  .dl { display: flex; font-size: 12px; line-height: 19px; min-height: 19px; }
  .dl .no {
    flex: none; width: 3.5rem; text-align: right; padding-right: 10px;
    color: var(--gutter-fg); background: var(--gutter-bg);
    user-select: none; font-variant-numeric: tabular-nums; font-size: 11px;
  }
  .dl .mk { flex: none; width: 1.6ch; text-align: center; user-select: none; color: var(--faint); }
  .dl .dsrc { white-space: pre; padding-right: 16px; }

  .dl.ins { background: color-mix(in srgb, var(--added) 13%, transparent); }
  .dl.ins .mk { color: var(--added); }
  .dl.del { background: color-mix(in srgb, var(--removed) 13%, transparent); }
  .dl.del .mk { color: var(--removed); }
  .dl:hover { filter: brightness(1.18); }

  .cmsg .panel-body h2 { font-size: 13px; margin-bottom: 6px; }
  .cmsg pre {
    margin: 0 0 10px; white-space: pre-wrap; color: var(--muted); font-size: 12px;
  }
  .cmeta { color: var(--faint); font-size: 11px; display: flex; gap: 14px; flex-wrap: wrap; }

  /* ---- readme: the one place prose typography takes over ---- */
  .md { padding: 20px 24px; overflow-x: auto; font-family: var(--sans); font-size: 14px; line-height: 1.65; max-width: 900px; }
  .md h1, .md h2 { border-bottom: 1px solid var(--border); padding-bottom: .25em; margin: 1.4em 0 .6em; }
  .md h1:first-child { margin-top: 0; }
  .md h3, .md h4 { margin: 1.3em 0 .4em; }
  .md p { margin: 0 0 .9em; }
  .md code { font-family: var(--mono); font-size: .85em; background: var(--raised); padding: .1em .35em; border-radius: 2px; }
  .md pre { background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius); padding: 11px 13px; overflow-x: auto; }
  .md pre code { background: none; padding: 0; font-size: 12px; }
  .md table { border-collapse: collapse; margin: 0 0 1em; font-size: 13px; }
  .md th, .md td { border: 1px solid var(--border); padding: 5px 11px; text-align: left; }
  .md th { background: var(--raised); }
  .md blockquote { margin: 0 0 1em; padding: .1em 1em; border-left: 2px solid var(--border-hi); color: var(--muted); }
  .md hr { border: 0; border-top: 1px solid var(--border); margin: 1.6em 0; }
  .md img { max-width: 100%; }
  .md ul, .md ol { padding-left: 1.5em; margin: 0 0 .9em; }
  .md a { color: var(--accent); }

  /* ---- settings ---- */
  .settings { max-width: 620px; }
  .danger { border-color: color-mix(in srgb, var(--removed) 35%, transparent); }
  .cmd {
    background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius);
    padding: 7px 10px; font-size: 12px; overflow-x: auto; white-space: nowrap; color: var(--muted);
  }
  .cmd b { color: var(--text); font-weight: 400; }
`;

@route("*")
@component("page-repo")
@styles(base, settingsLayout, sheet, codeSheet, commitSheet)
export class PageRepo extends LoomElement {
  @inject("session") accessor session!: Session;
  /**
   * These two used to be awaited one after the other inside `reload()`, so a
   * page waited two round trips before it could draw anything. They do not
   * depend on each other and are independent queries now, which also means the
   * key does the invalidation: navigate to another repository and both refetch
   * because the URL they are built from changed.
   */
  @query<Repo>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}`,
    enabled: (el: PageRepo) => Boolean(el.loc),
    init: { credentials: "same-origin" },
  })
  accessor repoQuery!: ApiState<Repo>;

  private get repo(): Repo | null {
    return this.repoQuery.data ?? null;
  }
  /**
   * "Not loaded" and "no branches" are different answers, and only one of them
   * should draw "this repository is empty". As a nullable array that took a
   * comment to explain and a `!== null` at the call site to honour; as a query
   * it is `.loading` versus `.data.length`, and getting it wrong is not
   * expressible.
   */
  @query<Ref[]>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/refs`,
    enabled: (el: PageRepo) => Boolean(el.loc),
    init: { credentials: "same-origin" },
  })
  accessor refsQuery!: ApiState<Ref[]>;

  private get refs(): Ref[] | null {
    return this.refsQuery.data ?? null;
  }
  /**
   * Decoration: counts and sizes for the sidebar. A failure here must not take
   * the page with it, which `@fetch` gives for free — the error lands in
   * `.error` and nothing reads it.
   *
   * `enabled` keeps it from firing before there is a repository to ask about.
   */
  @query<RepoStats>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/stats`,
    enabled: (el: PageRepo) => Boolean(el.loc),
    init: { credentials: "same-origin" },
  })
  accessor statsQuery!: ApiState<RepoStats>;

  private get stats(): RepoStats | null {
    return this.statsQuery.data ?? null;
  }
  /** A non-README document selected from the tab strip, and its content. */
  @reactive accessor docPath = "";
  @reactive accessor doc: string | null = null;
  @reactive accessor error = "";
  @reactive accessor notFound = false;

  @query<TreeResponse>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/tree/${refAndPath(el)}`,
    enabled: onTree,
    init: { credentials: "same-origin" },
  })
  accessor treeQuery!: ApiState<TreeResponse>;

  private get entries(): Entry[] | null {
    return this.treeQuery.data?.entries ?? null;
  }
  @query<BlobResponse>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/blob/${refAndPath(el)}`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "blob"),
    init: { credentials: "same-origin" },
  })
  accessor blobQuery!: ApiState<BlobResponse>;

  private get blob(): BlobResponse | null {
    return this.blobQuery.data ?? null;
  }
  /**
   * `params` rather than a hand-built query string, so the key is derived from
   * the same values the request carries — a limit that changed without the key
   * changing would serve the old page back.
   */
  @query<Commit[]>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/commits/${encodeURIComponent(el.refName())}`,
    params: { limit: 100, skip: 0 },
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "commits"),
    init: { credentials: "same-origin" },
  })
  accessor commitsQuery!: ApiState<Commit[]>;

  private get commits(): Commit[] | null {
    return this.commitsQuery.data ?? null;
  }
  @query<CommitDetail>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/commit/${el.loc!.view.kind === "commit" ? el.loc!.view.hash : ""}`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "commit"),
    init: { credentials: "same-origin" },
  })
  accessor detailQuery!: ApiState<CommitDetail>;

  private get detail(): CommitDetail | null {
    return this.detailQuery.data ?? null;
  }
  /**
   * Its own query, so a repository without a README is an empty result rather
   * than a failed page — the endpoint answers null, and null is data.
   */
  @query<{ name: string; content: string }>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/readme/${refAndPath(el)}`,
    enabled: onTree,
    init: { credentials: "same-origin" },
  })
  accessor readmeQuery!: ApiState<{ name: string; content: string }>;

  private get readme(): { name: string; content: string } | null {
    return this.readmeQuery.data ?? null;
  }
  /** Filled in after the tree renders, so the listing is never blocked on it. */
  /**
   * The commit column. Walking history is the slow part, so this deliberately
   * arrives after the file names — the difference now is that a response for a
   * directory you have already navigated away from cannot land in it, because
   * it is keyed by the path it was asked for.
   */
  @query<Record<string, LastCommit>>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/lastcommits/${refAndPath(el)}`,
    enabled: onTree,
    init: { credentials: "same-origin" },
  })
  accessor lastCommitsQuery!: ApiState<Record<string, LastCommit>>;

  private get lastCommits(): Record<string, LastCommit> | null {
    return this.lastCommitsQuery.data ?? null;
  }
  @reactive accessor copied = false;
  /// Which setup block was most recently copied.
  @reactive accessor copiedKey = "";

  /// Topics being edited, or null while they are still whatever the server
  /// last said. Distinguishing the two is what lets "remove every topic" save
  /// as an empty list rather than reading as "no edit".
  @reactive accessor topicDraft: string[] | null = null;

  /// The last collection report, kept so a dry run can be read before the
  /// real one is asked for.
  @reactive accessor gcReport: GcReport | null = null;

  /// Which line is being written on. Keyed `path:side:line` so one composer
  /// is open at a time — two half-written comments on one screen is a way to
  /// lose one.
  @reactive accessor writingAt = "";
  /// Which issues the list is showing. Part of the query URL, so changing it
  /// refetches rather than filtering a list that was never loaded.
  @reactive accessor issueFilter: "open" | "closed" | "all" = "open";
  /// Set while composing a new issue, so the form and the list are one page.
  @reactive accessor newIssue = false;
  /// The comment currently open for editing. One at a time, for the same
  /// reason only one line composer is open at a time.
  @reactive accessor editing = "";
  /// Which file the tree should highlight.
  @reactive accessor viewing = "";
  /**
   * The line diff, which can be real work on a large commit. It is its own
   * query so the summary is not waiting behind it — and being keyed by the
   * hash, a slow one cannot land under a commit you have since navigated to.
   */
  @query<Patch>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/patch/${el.loc!.view.kind === "commit" ? el.loc!.view.hash : ""}`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "commit"),
    init: { credentials: "same-origin" },
  })
  accessor patchQuery!: ApiState<Patch>;

  private get patch(): Patch | null {
    return this.patchQuery.data ?? null;
  }
  @query<Comparison>({
    url: (el: PageRepo) => {
      const v = el.loc!.view;
      const d = el.repoQuery.data?.default_branch ?? "main";
      const base = v.kind === "compare" ? v.base || d : d;
      const head = v.kind === "compare" ? v.head || d : d;
      return `/api/repos/${el.loc!.owner}/${el.loc!.name}/compare/${encodeURIComponent(base)}/${encodeURIComponent(head)}`;
    },
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "compare"),
    init: { credentials: "same-origin" },
  })
  accessor comparisonQuery!: ApiState<Comparison>;

  private get comparison(): Comparison | null {
    return this.comparisonQuery.data ?? null;
  }
  /**
   * `params` carries the state filter, so switching between open and closed
   * changes the key and refetches — the filter and the cache cannot disagree.
   */
  @query<MergeRequest[]>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/merges`,
    params: (el: PageRepo) => ({ state: el.mergeState }),
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "merges"),
    init: { credentials: "same-origin" },
  })
  accessor mergesQuery!: ApiState<MergeRequest[]>;

  private get merges(): MergeRequest[] | null {
    return this.mergesQuery.data ?? null;
  }
  @reactive accessor mergeState: "open" | "merged" | "closed" | "all" = "open";
  @query<MergeRequestDetail>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/merges/${el.loc!.view.kind === "merge" ? el.loc!.view.number : 0}`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "merge"),
    init: { credentials: "same-origin" },
  })
  accessor mrQuery!: ApiState<MergeRequestDetail>;

  @query<Comment[]>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/merges/${
        el.loc!.view.kind === "merge" ? el.loc!.view.number : 0
      }/comments`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "merge"),
    init: { credentials: "same-origin" },
  })
  accessor commentsQuery!: ApiState<Comment[]>;

  @query<Issue[]>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/issues?state=${el.issueFilter}`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "issues"),
    init: { credentials: "same-origin" },
  })
  accessor issuesQuery!: ApiState<Issue[]>;

  @query<Issue>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/issues/${
        el.loc!.view.kind === "issue" ? el.loc!.view.number : 0
      }`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "issue"),
    init: { credentials: "same-origin" },
  })
  accessor issueQuery!: ApiState<Issue>;

  @query<Comment[]>({
    url: (el: PageRepo) =>
      `/api/repos/${el.loc!.owner}/${el.loc!.name}/issues/${
        el.loc!.view.kind === "issue" ? el.loc!.view.number : 0
      }/comments`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "issue"),
    init: { credentials: "same-origin" },
  })
  accessor issueTalkQuery!: ApiState<Comment[]>;

  /// Null while unknown, which is not the same as "nobody has said anything"
  /// — the difference decides whether the page shows a skeleton or a prompt.
  private get comments(): Comment[] | null {
    return this.commentsQuery.data ?? null;
  }

  private get mr(): MergeRequestDetail | null {
    return this.mrQuery.data ?? null;
  }
  @reactive accessor busy = false;
  /**
   * Only an admin may read this. A 403 is a permissions answer rather than a
   * fault, so the render treats "no data" as "none to show" and nothing reads
   * the error.
   */
  @query<Collaborator[]>({
    url: (el: PageRepo) => `/api/repos/${el.loc!.owner}/${el.loc!.name}/collaborators`,
    enabled: (el: PageRepo) =>
      Boolean(el.loc && el.repoQuery.data && el.loc.view.kind === "settings"),
    init: { credentials: "same-origin" },
  })
  accessor collaboratorsQuery!: ApiState<Collaborator[]>;

  private get collaborators(): Collaborator[] | null {
    return this.collaboratorsQuery.data ?? null;
  }
  @reactive accessor newRole = "write";
  /// Transient "saved" confirmation on settings forms.
  @reactive accessor notice = "";
  /** Paths the reader has collapsed. */
  @reactive accessor collapsed: Record<string, boolean> = {};

  /**
   * Reactive, because query URLs are built from it. `@fetch` derives its cache
   * key from the resolved URL, so navigating changes the key and the request
   * re-runs — which is the manual `reload()` sequencing, done by the decorator.
   */
  @reactive accessor loc = parse();

  @mount
  init() {
    void this.reload();
  }

  // Not private: it is invoked by the decorator, and TypeScript reports an
  // unused private member for a method nothing in the class calls.
  @on(window, "popstate")
  onNav() {
    // Widened here as well as in `onRefs`, and that is not belt-and-braces.
    // `onRefs` fires when the refs *query* resolves, which on a second visit
    // to the same repository it never does — the list is already cached. So
    // navigating main -> feature/x worked the first time and broke on every
    // one after, because nothing re-split the URL.
    const next = this.widened(parse());
    const changedRepo =
      !this.loc || !next || next.owner !== this.loc.owner || next.name !== this.loc.name;
    this.loc = next;
    void this.reload(changedRepo);
  }

  /** Fetch repo metadata (only when the repo changed) plus the current view. */
  private async reload(refetchRepo = true) {
    const at = this.loc;
    if (!at) {
      this.notFound = true;
      return;
    }
    this.error = "";

    await this.loadView();
  }

  /**
   * The view's own data waits on the refs, because which ref to ask about
   * comes from them. Subscribing to the query rather than awaiting it is what
   * lets the two above run in parallel.
   */
  @watch("refsQuery")
  onRefs() {
    if (!this.refsQuery.ok) return;
    const wide = this.widened(this.loc);
    if (wide !== this.loc) this.loc = wide;
    void this.loadView();
  }

  /**
   * Re-split the location now that the ref names are known.
   *
   * Assigning a new `loc` rather than mutating the view in place, because the
   * queries key off it and would not otherwise notice.
   */
  private widened(at: ReturnType<typeof parse>): ReturnType<typeof parse> {
    if (!at) return at;
    const v = at.view;
    if (v.kind !== "tree" && v.kind !== "blob") return at;

    // Nothing to widen against until the refs have arrived; the first load
    // goes out naive and `onRefs` corrects it.
    const known = (this.refs ?? []).map((r) => (r.kind === "tag" ? `tags/${r.name}` : r.name));
    const wide = widenRef(known, v.ref, v.path);
    if (wide.ref === v.ref) return at;

    return { ...at, view: { kind: v.kind, ref: wide.ref, path: wide.path } };
  }

  /** A repository that does not exist is a 404, not an error banner. */
  @watch("repoQuery")
  onRepo() {
    const e = this.repoQuery.error as { status?: number } | undefined;
    this.notFound = e?.status === 404;
    this.error = !e || this.notFound ? "" : String((e as Error).message ?? e);
  }

  /** Branches only. `refs` carries tags too, and neither the branch picker
   *  nor the default-branch setting may offer one: a tag is not somewhere you
   *  can commit. */
  private branches(): Ref[] {
    return (this.refs ?? []).filter((r) => r.kind !== "tag");
  }

  private tags(): Ref[] {
    return (this.refs ?? []).filter((r) => r.kind === "tag");
  }

  /** Public for the query URL builders, which are module-level. */
  refName(): string {
    const v = this.loc?.view;
    const explicit = v && "ref" in v ? v.ref : "";
    return explicit || this.repo?.default_branch || "main";
  }

  private async loadView() {
    const at = this.loc;
    if (!at || !this.repo) return;
    const v = at.view;

    this.copied = false;
    this.copiedKey = "";
    this.topicDraft = null;
    this.notice = "";
    this.busy = false;
    this.collapsed = {};

    // A repository with no commits has no refs to browse.
    if (this.branches().length === 0 && v.kind !== "settings") return;

    // The document tab is a selection, not a response — everything this method
    // used to fetch is a query now, keyed by what it was asked for.
    if (v.kind === "tree") {
      this.docPath = "";
      this.doc = null;
    }
  }

  private switchRef(ref: string) {
    const at = this.loc!;
    const v = at.view;
    const path = v.kind === "tree" || v.kind === "blob" ? v.path : "";
    const kind = v.kind === "blob" ? "blob" : v.kind === "commits" ? "commits" : "tree";
    go(`/${at.owner}/${at.name}/${kind}/${ref}${path ? "/" + path : ""}`);
  }

  // ---- rendering -------------------------------------------------------

  private renderCrumbs(path: string) {
    const at = this.loc!;
    const r = this.refName();
    const parts = path.split("/").filter(Boolean);
    const rootHref = `/${at.owner}/${at.name}/tree/${r}`;
    return (
      <div class="crumbs">
        <a href={rootHref} onClick={linkHandler(rootHref)}>{at.name}</a>
        {parts.map((p, i) => {
          const sub = parts.slice(0, i + 1).join("/");
          const last = i === parts.length - 1;
          const kind = last && this.loc!.view.kind === "blob" ? "blob" : "tree";
          const href = `/${at.owner}/${at.name}/${kind}/${r}/${sub}`;
          return (
            <>
              <span class="sep">/</span>
              {last ? (
                <strong>{p}</strong>
              ) : (
                <a href={href} onClick={linkHandler(href)}>{p}</a>
              )}
            </>
          );
        })}
      </div>
    );
  }

  /**
   * The right-hand column: what this repository *is*, next to what is in it.
   *
   * Shown for every directory, not only the root. It was root-only on the
   * argument that a panel about the whole repository is noise beside one
   * folder — but the cost is the page snapping between one column and two as
   * you walk the tree, which is worse than the noise. Reading a *file* still
   * takes the full width, because code wants the room.
   */
  private renderAside() {
    const at = this.loc!;
    const r = this.repo!;

    const tags = this.tags().slice().sort(
      (a, b) => (b.head?.timestamp ?? 0) - (a.head?.timestamp ?? 0),
    );
    const tagsHref = `/${at.owner}/${at.name}/tags`;

    return (
      <aside class="aside">
        <section>
          <h3>about</h3>
          {r.description ? (
            <p class="prose">{r.description}</p>
          ) : (
            <p class="prose faint">No description.</p>
          )}

          {/* rel=noopener stops the target reaching back through window.opener;
              nofollow because this is a link somebody else chose to put on our
              page. The scheme is checked server-side before it is stored. */}
          {r.homepage ? (
            <a
              class="home"
              href={r.homepage}
              target="_blank"
              rel="noopener noreferrer nofollow"
            >
              <loom-icon name="link" size={12}></loom-icon>
              {r.homepage.replace(/^https?:\/\//, "").replace(/\/$/, "")}
            </a>
          ) : null}

          {r.topics?.length ? (
            <div class="topics">
              {r.topics.map((t) => <span class="topic">{t}</span>)}
            </div>
          ) : null}

          <dl class="facts">
            <dt>visibility</dt>
            <dd>{r.visibility}</dd>
            <dt>default</dt>
            <dd class="mono">{r.default_branch}</dd>
            {/* A count, not a list. The ref picker above already enumerates
                branches and can filter them; a sidebar list capped at five is
                a lie once a repository has fifty. */}
            <dt>branches</dt>
            <dd>{this.branches().length}</dd>
            {this.stats ? (
              <>
                <dt>commits</dt>
                <dd>{this.stats.commits.toLocaleString()}</dd>
                <dt>objects</dt>
                <dd>{this.stats.objects.toLocaleString()}</dd>
                {/* What the store holds, after chunk deduplication and
                    compression — not the size of a checkout. */}
                <dt>size</dt>
                <dd>{humanSize(this.stats.bytes)}</dd>
              </>
            ) : null}
            <dt>created</dt>
            <dd>{relativeTime(r.created_at)}</dd>
          </dl>
        </section>

        <section>
          <h3>
            <a href={tagsHref} onClick={linkHandler(tagsHref)}>tags</a>
            <span class="n">{tags.length}</span>
          </h3>
          {tags.length === 0 ? (
            <p class="prose faint">None yet.</p>
          ) : (
            <ul class="mini">
              {tags.slice(0, 5).map((t) => {
                const href = `/${at.owner}/${at.name}/tree/${t.name}`;
                return (
                  <li>
                    <loom-icon name="tag" size={12}></loom-icon>
                    <a href={href} onClick={linkHandler(href)}>{t.name}</a>
                    <span class="when">
                      {t.head ? relativeTime(t.head.timestamp) : ""}
                    </span>
                  </li>
                );
              })}
            </ul>
          )}
          {tags.length > 5 ? (
            <a class="more" href={tagsHref} onClick={linkHandler(tagsHref)}>
              all {tags.length} tags
            </a>
          ) : null}
        </section>

      </aside>
    );
  }

  /**
   * The latest commit on whatever is being browsed, above the file list.
   *
   * Taken from the ref's own head rather than fetching the log: the refs call
   * already carries the tip commit for every branch and tag, so this costs
   * nothing extra. Browsing a bare commit hash has no ref to read, and the bar
   * is simply omitted rather than showing a guess.
   */
  private renderLatest() {
    const at = this.loc!;
    const ref = this.refName();
    const head = (this.refs ?? []).find((r) => r.name === ref)?.head;
    if (!head) return null;

    const commit = `/${at.owner}/${at.name}/commit/${head.commit}`;
    const history = `/${at.owner}/${at.name}/commits/${ref}`;

    return (
      <div class="latest">
        <span class="who">{authorName(head.author)}</span>
        <a class="msg" href={commit} onClick={linkHandler(commit)} title={head.summary}>
          {head.summary || "(no message)"}
        </a>
        <a class="sha" href={commit} onClick={linkHandler(commit)}>{head.short}</a>
        <span class="when">{relativeTime(head.timestamp)}</span>
        <a class="count" href={history} onClick={linkHandler(history)}>
          <loom-icon name="history" size={12}></loom-icon>
          {this.stats ? (
            <>
              <b>{this.stats.commits.toLocaleString()}</b>{" "}
              {this.stats.commits === 1 ? "commit" : "commits"}
            </>
          ) : (
            "history"
          )}
        </a>
      </div>
    );
  }

  /**
   * The well-known documents in the root, as tabs over the rendered pane.
   *
   * Detected from the root listing that has already been fetched, so nothing
   * is claimed that is not actually in the tree, and it costs no request.
   * Matched on the stem, so LICENSE, LICENSE.md and LICENCE.txt all count.
   */
  private docTabs(): { label: string; icon: string; path: string; name: string }[] {
    const entries = this.entries;
    if (!entries) return [];

    const known: [RegExp, string, string][] = [
      [/^readme$/, "readme", "book"],
      [/^licen[cs]e$/, "license", "scale"],
      [/^contributing$/, "contributing", "people"],
      [/^code[_-]?of[_-]?conduct$/, "code of conduct", "heart"],
      [/^security$/, "security", "shield"],
      [/^changelog$/, "changelog", "history"],
    ];

    const found: { label: string; icon: string; path: string; name: string }[] = [];
    for (const [re, label, icon] of known) {
      const hit = entries.find(
        (e) => e.kind !== "dir" && re.test((e.name.split(".")[0] ?? "").toLowerCase()),
      );
      if (hit) found.push({ label, icon, path: hit.path, name: hit.name });
    }
    return found;
  }

  private renderTree(path: string) {
    const at = this.loc!;
    const r = this.refName();
    const rows: unknown[] = [];

    if (path) {
      const up = path.split("/").slice(0, -1).join("/");
      const href = `/${at.owner}/${at.name}/tree/${r}${up ? "/" + up : ""}`;
      rows.push(
        <div class="r">
          <span class="ic d"><loom-icon name="up" size={14}></loom-icon></span>
          <a class="fn" href={href} onClick={linkHandler(href)}>..</a>
          <span></span>
          <span></span>
          <span></span>
        </div>,
      );
    }

    for (const e of this.entries ?? []) {
      const kind = e.kind === "dir" ? "tree" : "blob";
      const href = `/${at.owner}/${at.name}/${kind}/${r}/${e.path}`;
      const lc = this.lastCommits?.[e.name];
      const chref = lc ? `/${at.owner}/${at.name}/commit/${lc.hash}` : "";
      rows.push(
        <div class="r">
          <span
            class={`ic ${e.kind === "dir" ? "d" : ""} ${e.kind === "exec" ? "x" : ""}`}
            title={e.kind === "exec" ? "executable" : e.kind}
          >
            <loom-icon
              name={
                e.kind === "dir"
                  ? dirIcon(e.name)
                  : e.kind === "symlink"
                    ? "link"
                    : e.kind === "exec"
                      ? "terminal"
                      : fileIcon(e.name)
              }
              size={14}
            ></loom-icon>
          </span>
          <a class="fn" href={href} onClick={linkHandler(href)}>
            {e.name}
          </a>
          {lc ? (
            <a class="msg" href={chref} onClick={linkHandler(chref)} title={lc.summary}>
              {lc.summary || "(no message)"}
            </a>
          ) : (
            <span class="msg"><span class="sk" style="width:min(80%,180px)"></span></span>
          )}
          <span class="when">
            {lc ? relativeTime(lc.timestamp) : <span class="sk" style="width:52px"></span>}
          </span>
          {/* A directory entry already carries the total bytes beneath it —
              the tree records it at ingest, so this is a `du` that costs
              nothing. Dimmed, because it is a sum rather than a file. */}
          <span class={`sz ${e.kind === "dir" ? "sum" : ""}`}>{humanSize(e.size)}</span>
        </div>,
      );
    }
    return <div class="panel files">{rows}</div>;
  }

  /** Above this many lines, the file is virtualized instead of fully rendered. */
  private static readonly VIRTUALIZE_OVER = 400;

  private renderBlob() {
    const b = this.blob!;
    const at = this.loc!;
    const rawHref = api.rawUrl(at.owner, at.name, this.refName(), b.path);
    const head = (
      <div class="panel-head">
        <span class="val">
            {b.image
              ? b.image.replace("image/", "").toUpperCase()
              : b.binary
                ? "binary"
                : `${b.lines} ${b.lines === 1 ? "line" : "lines"}`} ·{" "}
            {humanSize(b.size)}
          </span>
        <span class="row">
          <span class="faint val">{b.hash.slice(0, 12)}</span>
          {b.content !== null ? (
            <button class="bare val" onClick={() => void this.copyBlob(b.content ?? "")}>
              <loom-icon name={this.copied ? "check" : "copy"} size={12}></loom-icon>
              {this.copied ? "copied" : "copy"}
            </button>
          ) : null}
          {/* A real link, so middle-click and "save as" behave. The server
              forces text/plain + nosniff, so pushed HTML cannot execute here. */}
          <a class="btn bare val" href={rawHref} target="_blank" rel="noopener noreferrer">
            <loom-icon name="external" size={12}></loom-icon> raw
          </a>
        </span>
      </div>
    );

    if (b.truncated) {
      return <div class="panel">{head}<div class="empty">file is too large to display</div></div>;
    }
    // An image is served with its own content type by the raw endpoint — the
    // only formats that get one, because they decode to pixels and cannot run
    // anything. An SVG deliberately arrives as text and renders as source.
    if (b.image) {
      return (
        <div class="panel">
          {head}
          <div class="imgview">
            <img src={rawHref} alt={b.path} loading="lazy" />
          </div>
        </div>
      );
    }
    if (b.binary) {
      return <div class="panel">{head}<div class="empty">binary file not shown</div></div>;
    }

    const lang = languageFor(b.path);
    const lines = highlight(b.content ?? "", lang);
    // The gutter is sized from the widest line number so the code column never
    // shifts as you scroll — the usual giveaway of a naively virtualized list.
    const gutter = `${String(lines.length).length}ch`;

    const row = (toks: Tok[], i: number) => (
      <div class="cl">
        <span class="ln" style={`width:calc(${gutter} + 26px)`}>{i + 1}</span>
        <span class="src">
          {toks.length === 0 ? " " : toks.map((t) => <span class={t.c}>{t.t}</span>)}
        </span>
      </div>
    );

    // A 40 000-line file is 40 000 DOM nodes rendered eagerly. Past a few
    // hundred lines the virtual list keeps it to whatever fits on screen.
    if (lines.length > PageRepo.VIRTUALIZE_OVER) {
      const v = (
        <loom-virtual class="vcode" items={lines} estimatedHeight={19} overscan={12}>
          {row}
        </loom-virtual>
      ) as unknown as HTMLElement & { pinToBottom: boolean };
      // `pinToBottom` defaults to true (it is built for chat logs) and JSX turns
      // a `false` boolean into `removeAttribute`, which cannot clear it. Setting
      // the property directly is what actually opens the file at the top.
      v.pinToBottom = false;
      adoptInto(v, codeSheet);
      return <div class="panel">{head}{v}</div>;
    }

    return (
      <div class="panel">
        {head}
        <div class="code">{lines.map(row)}</div>
      </div>
    );
  }

  private async copyBlob(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      this.copied = true;
      setTimeout(() => (this.copied = false), 1400);
    } catch {
      this.error = "could not copy — the browser refused clipboard access";
    }
  }

  private renderCommits() {
    const at = this.loc!;
    const list = this.commits ?? [];

    // Grouped by the day the commit was made, the way you actually think about
    // history — "what happened Tuesday", not "rows 40 through 60". The flat
    // list gave every commit the same weight and left a gulf between the
    // message and its metadata.
    const groups: { day: string; items: Commit[] }[] = [];
    for (const c of list) {
      const day = new Date(c.timestamp * 1000).toLocaleDateString(undefined, {
        weekday: "short",
        day: "numeric",
        month: "short",
        year: "numeric",
      });
      const last = groups[groups.length - 1];
      if (last && last.day === day) last.items.push(c);
      else groups.push({ day, items: [c] });
    }

    const row = (c: Commit) => {
      const href = `/${at.owner}/${at.name}/commit/${c.hash}`;
      const tree = `/${at.owner}/${at.name}/tree/${c.hash}`;
      return (
        <div class="c">
          <span class="body">
            <a class="m" href={href} onClick={linkHandler(href)}>
              {c.summary || "(no message)"}
            </a>
            <span class="by">
              {authorName(c.author)} committed {relativeTime(c.timestamp)}
            </span>
          </span>
          <span class="acts">
            <a class="sha" href={href} onClick={linkHandler(href)} title="View this commit">
              {c.short}
            </a>
            <a class="ghost" href={tree} onClick={linkHandler(tree)} title="Browse files at this commit">
              <loom-icon name="folder" size={12}></loom-icon>
            </a>
          </span>
        </div>
      );
    };

    // Virtualizing cannot carry the day headers with it, so a long history
    // stays flat rather than pretending to be grouped and scrolling wrong.
    if (list.length > 200) {
      const v = (
        <loom-virtual class="vlist" items={list} estimatedHeight={44} overscan={8}>
          {row}
        </loom-virtual>
      ) as unknown as HTMLElement & { pinToBottom: boolean };
      v.pinToBottom = false;
      adoptInto(v, commitSheet);
      return <div class="panel commits">{v}</div>;
    }

    if (list.length === 0) {
      return (
        <div class="panel commits">
          <div class="empty">nothing committed on this branch yet</div>
        </div>
      );
    }

    // Flattened rather than nested in fragments: a fragment returned from
    // inside a .map did not keep its children in order here, which put the
    // rail dots one row out and left one dangling past the end of the list.
    const out: unknown[] = [];
    for (const g of groups) {
      out.push(
        <div class="day">
          <loom-icon name="commit" size={12}></loom-icon>
          {g.day}
          <span class="n">{g.items.length}</span>
        </div>,
      );
      for (const c of g.items) out.push(row(c));
    }

    return <div class="panel commits">{out}</div>;
  }

  private renderCommitDetail() {
    const d = this.detail!;
    const at = this.loc!;
    const body = d.message.split("\n").slice(1).join("\n").trim();
    const browse = `/${at.owner}/${at.name}/tree/${d.hash}`;

    return (
      <div>
        {/* One block, not two stacked boxes: the message is the headline, the
            metadata is a quiet line under it, and the actions sit on the
            baseline of the headline where the eye already is. */}
        <div class="chead">
          <div class="chead-top">
            <h2 class="csummary">{d.summary || "(no message)"}</h2>
            <a class="btn" href={browse} onClick={linkHandler(browse)}>
              <loom-icon name="folder" size={12}></loom-icon> browse files
            </a>
          </div>
          {body ? <pre class="cbody">{body}</pre> : null}
          <div class="cmeta">
            <span class="who">{authorName(d.author)}</span>
            <span>{relativeTime(d.timestamp)}</span>
            <span class="hash">{d.short}</span>
            {d.parents.length > 1 ? <span class="tag on">merge</span> : null}
            {d.parents.map((pp, i) => {
              const href = `/${at.owner}/${at.name}/commit/${pp}`;
              return (
                <span class="parent">
                  {d.parents.length > 1 ? `parent ${i + 1}` : "parent"}{" "}
                  <a href={href} onClick={linkHandler(href)}>{pp.slice(0, 10)}</a>
                </span>
              );
            })}
          </div>
        </div>
        {this.renderPatch(d)}
      </div>
    );
  }

  /** One file's diff: a header strip, then its hunks. */
  private renderFileDiff(f: FileDiff, atRef?: string) {
    const at = this.loc!;
    // Only a merge request has somewhere to attach a line comment, and only a
    // signed-in viewer can write one.
    const canTalk = this.comments !== null && this.session.isAuthed;
    const isOpen = !this.collapsed[f.path];
    const lang = languageFor(f.path);
    const ref = atRef ?? this.detail?.hash ?? this.refName();
    const href = `/${at.owner}/${at.name}/blob/${ref}/${f.path}`;

    return (
      <div class="df" loom-key={f.path} data-file={f.path}>
        <div class="df-head">
          <button
            class="bare df-toggle"
            onClick={() =>
              (this.collapsed = { ...this.collapsed, [f.path]: isOpen })
            }
            title={isOpen ? "collapse" : "expand"}
          >
            <loom-icon
              class={isOpen ? "" : "closed"}
              name="chevron"
              size={12}
            ></loom-icon>
          </button>
          <span class={`st ${f.status}`}>
            {f.status === "added" ? "+" : f.status === "removed" ? "−" : f.status === "modified" ? "~" : "t"}
          </span>
          {f.status === "removed" ? (
            <span class="df-path muted">{f.path}</span>
          ) : (
            <a class="df-path" href={href} onClick={linkHandler(href)}>{f.path}</a>
          )}
          <span class="counts">
            {f.added > 0 ? <span class="plus">+{f.added}</span> : null}
            {f.removed > 0 ? <span class="minus">{`−${f.removed}`}</span> : null}
          </span>
        </div>

        {!isOpen ? null : f.too_large ? (
          <div class="df-note">file is too large to diff ({humanSize(Math.max(f.old_size, f.new_size))})</div>
        ) : f.binary ? (
          <div class="df-note">binary file</div>
        ) : f.only_line_endings ? (
          <div class="df-note">line endings changed only</div>
        ) : f.hunks.length === 0 ? (
          <div class="df-note">no line changes</div>
        ) : (
          <div class="df-body">
            {f.truncated ? (
              <div class="df-note">
                the two versions differ too much for a line diff — shown as a full replacement
              </div>
            ) : null}
            {f.hunks.map((h) => (
              <div>
                <div class="hh">{h.header}</div>
                {h.lines.map((l) => {
                  const cls = l.op === "+" ? "ins" : l.op === "-" ? "del" : "eq";
                  // Highlighting is per line here: a hunk is a fragment, so
                  // multi-line constructs have no context to carry anyway.
                  const toks = highlight(l.text, lang)[0] ?? [];
                  // A comment belongs to a side: on a removed line it is about
                  // the old file, on anything else about the new one.
                  const side: "old" | "new" = l.op === "-" ? "old" : "new";
                  const no = side === "old" ? l.old_no : l.new_no;
                  const blob = side === "old" ? f.old_hash : f.new_hash;
                  const key = no && blob ? `${f.path}:${side}:${no}` : "";
                  const here = key ? this.lineComments(f, side, no!) : [];

                  return (
                    <>
                      <div class={`dl ${cls}`}>
                        {canTalk && key ? (
                          <button
                            class="addc"
                            title="Comment on this line"
                            onClick={() => (this.writingAt = this.writingAt === key ? "" : key)}
                          >
                            <loom-icon name="plus" size={11}></loom-icon>
                          </button>
                        ) : null}
                        <span class="no">{l.old_no ?? ""}</span>
                        <span class="no">{l.new_no ?? ""}</span>
                        <span class="mk">{l.op}</span>
                        <span class="dsrc">
                          {toks.length === 0 ? " " : toks.map((t) => <span class={t.c}>{t.t}</span>)}
                        </span>
                      </div>
                      {here.length || this.writingAt === key
                        ? this.renderThread(here, key, blob ?? "", f.path, side, no ?? 0)
                        : null}
                    </>
                  );
                })}
              </div>
            ))}
          </div>
        )}
      </div>
    );
  }

  /// One comment, with the actions its author gets.
  ///
  /// Written once because the merge request conversation, a line thread and an
  /// issue all render the same thing, and three copies is how they drift.
  private renderComment(c: Comment, refetch: () => Promise<void>, note?: string) {
    const at = this.loc!;
    const me = this.session.current?.username;
    const mine = !!me && c.author === me;

    if (this.editing === c.id) {
      return (
        <fkit-composer
          loom-key={`edit-${c.id}`}
          compact
          label="Save"
          value={c.body}
          busy={this.busy}
          onSend={(e: Event) => {
            const body = (e as CustomEvent<string>).detail;
            void this.act(async () => {
              await api.editComment(at.owner, at.name, c.id, body);
              this.editing = "";
              await refetch();
            });
          }}
        >
          <button type="button" slot="extra" class="bare" onClick={() => (this.editing = "")}>
            cancel
          </button>
        </fkit-composer>
      );
    }

    return (
      <fkit-comment
        loom-key={c.id}
        author={c.author ?? ""}
        when={note ? `${relativeTime(c.created_at)} · ${note}` : relativeTime(c.created_at)}
        body={c.body}
        edited={!!c.edited_at}
        mine={mine}
      >
        {mine ? (
          <span slot="actions">
            <button type="button" class="bare" onClick={() => (this.editing = c.id)}>
              edit
            </button>
            <button
              type="button"
              class="bare danger"
              onClick={() =>
                void (async () => {
                  const ok = await confirmAction({
                    title: "Delete this comment?",
                    body: "It is removed for everyone. This cannot be undone.",
                    confirm: "Delete",
                    danger: true,
                  });
                  if (!ok) return;
                  await this.act(async () => {
                    await api.deleteComment(at.owner, at.name, c.id);
                    await refetch();
                  });
                })()
              }
            >
              delete
            </button>
          </span>
        ) : null}
      </fkit-comment>
    );
  }

  /// Comments anchored to one line of one side of a file.
  ///
  /// Matched on the *content* hash, not the path: the same file at a different
  /// version is a different blob, so a comment written against an older
  /// version simply does not appear here — which is what makes it possible to
  /// say it is outdated rather than draw it against a line it never described.
  private lineComments(f: FileDiff, side: "old" | "new", line: number): Comment[] {
    const blob = side === "old" ? f.old_hash : f.new_hash;
    if (!blob) return [];
    return (this.comments ?? []).filter(
      (c) => c.blob === blob && c.side === side && c.line === line,
    );
  }

  /// Comments on a file whose content has moved on since they were written.
  private staleComments(f: FileDiff): Comment[] {
    const live = new Set([f.old_hash, f.new_hash].filter(Boolean) as string[]);
    return (this.comments ?? []).filter(
      (c) => c.file_path === f.path && c.blob && !live.has(c.blob),
    );
  }

  /// A line's thread, and the box to add to it.
  private renderThread(
    here: Comment[],
    key: string,
    blob: string,
    path: string,
    side: "old" | "new",
    line: number,
  ) {
    const open = this.writingAt === key;

    return (
      <div class="thread">
        {here.map((c) => this.renderComment(c, () => this.commentsQuery.refetch()))}

        {open ? (
          <fkit-composer
            compact
            label="Comment"
            placeholder={`Comment on line ${line}`}
            busy={this.busy}
            onSend={(e: Event) =>
              void this.postComment((e as CustomEvent<string>).detail, {
                file_path: path,
                line,
                side,
                blob,
              })
            }
          >
            <button slot="extra" class="bare" onClick={() => (this.writingAt = "")}>
              cancel
            </button>
          </fkit-composer>
        ) : null}
      </div>
    );
  }

  /// Post a comment on the merge request being viewed.
  ///
  /// The composer is cleared only after the request lands, so a failed post
  /// never costs someone what they wrote.
  private async postComment(body: string, anchor?: {
    file_path: string; line: number; side: "old" | "new"; blob: string;
  }) {
    const at = this.loc!;
    const v = at.view;
    if (v.kind !== "merge") return;
    await this.act(async () => {
      await api.commentOnMerge(at.owner, at.name, v.number, { body, ...(anchor ?? {}) });
      await this.commentsQuery.refetch();
      this.writingAt = "";
      for (const el of this.shadowRoot?.querySelectorAll("fkit-composer") ?? []) {
        (el as HTMLElement & { clear(): void }).clear();
      }
    });
  }

  private renderPatch(d: CommitDetail) {
    if (d.changes.length === 0) {
      return <div class="panel"><div class="empty">no changes</div></div>;
    }

    // Until the patch lands, show the path list we already have — it is the
    // same information the summary carried, so nothing regresses while loading.
    if (!this.patch) {
      return (
        <div class="panel">
          {d.changes.map((c) => (
            <div class="ch" loom-key={c.path}>
              <span class={`st ${c.status}`}>
                {c.status === "added" ? "+" : c.status === "removed" ? "−" : c.status === "modified" ? "~" : "t"}
              </span>
              <span>{c.path}</span>
              <span class="sk" style="width:70px"></span>
            </div>
          ))}
        </div>
      );
    }

    const total = this.patch.files.reduce(
      (acc, f) => ({ a: acc.a + f.added, r: acc.r + f.removed }),
      { a: 0, r: 0 },
    );

    return (
      <div>
        <div class="patch-bar">
          <span>{this.patch.files.length} file(s) changed</span>
          <span class="plus">+{total.a}</span>
          <span class="minus">{`−${total.r}`}</span>
          {this.patch.truncated ? (
            <span class="muted">
              showing the first {this.patch.files.length} of {d.changes.length}
            </span>
          ) : null}
        </div>
        {this.patch.files.map((f) => this.renderFileDiff(f))}
      </div>
    );
  }

  private renderReadme() {
    const tabs = this.docTabs();
    // Nothing to show, and nothing to offer.
    if (!this.readme && tabs.length === 0) return null;

    // The README is the default view; any other tab loads on demand.
    const active = this.docPath;
    const body = active ? this.doc : this.readme?.content;
    const name = active
      ? (tabs.find((t) => t.path === active)?.name ?? active)
      : (this.readme?.name ?? "");

    return (
      <div class="panel doc" style="margin-top:12px">
        <div class="doctabs">
          {tabs.map((t) => {
            const isReadme = t.label === "readme";
            const on = isReadme ? !active : active === t.path;
            return (
              <button
                class={on ? "on" : ""}
                type="button"
                onClick={() => this.openDoc(isReadme ? "" : t.path)}
              >
                <loom-icon name={t.icon} size={12}></loom-icon>
                {t.label}
              </button>
            );
          })}
          <span class="grow"></span>
          <span class="val">{name}</span>
        </div>
        {body === null || body === undefined ? (
          <div class="panel-body"><span class="sk" style="width:min(60%,320px)"></span></div>
        ) : (
          // The renderer escapes everything before generating markup, so
          // rawHTML here is safe for untrusted repository content.
          <div
            class="md"
            rawHTML={renderMarkdown(
              body,
              // Both the README and any doc tab sit in the directory being
              // browsed, so relative paths resolve against that.
              this.mdContext(
                this.loc?.view.kind === "tree" ? (this.loc.view.path ?? "") : "",
              ),
            )}
          ></div>
        )}
      </div>
    );
  }

  /**
   * Resolve repository-relative paths in a rendered document.
   *
   * Relative to the directory the document itself lives in, so a README in
   * `docs/` referring to `logo.png` means `docs/logo.png`, and `../a.png`
   * climbs out. Images go to the raw endpoint — which serves real image types
   * and nothing else executable — and links go to the blob view rather than
   * downloading.
   */
  private mdContext(docDir: string): MarkdownContext {
    const at = this.loc!;
    const ref = this.refName();

    const resolve = (rel: string): string => {
      // Strip a query or fragment before joining, and put it back after.
      const cut = rel.search(/[?#]/);
      const suffix = cut === -1 ? "" : rel.slice(cut);
      const bare = cut === -1 ? rel : rel.slice(0, cut);

      const parts = docDir ? docDir.split("/") : [];
      for (const seg of bare.split("/")) {
        if (seg === "" || seg === ".") continue;
        if (seg === "..") parts.pop();
        else parts.push(seg);
      }
      return parts.map(encodeURIComponent).join("/") + suffix;
    };

    return {
      raw: (rel) => api.rawUrl(at.owner, at.name, ref, resolve(rel)),
      page: (rel) => `/${at.owner}/${at.name}/blob/${encodeURIComponent(ref)}/${resolve(rel)}`,
    };
  }

  /** Switch the document pane. "" means the README, which is already loaded. */
  private openDoc(path: string) {
    this.docPath = path;
    if (!path) return;
    const at = this.loc!;
    this.doc = null;
    void api
      .blob(at.owner, at.name, this.refName(), path)
      .then((b) => {
        // Guard against a slow response for a tab the reader has left.
        if (this.docPath === path) this.doc = b.content ?? "(not a text file)";
      })
      .catch((e) => {
        if (this.docPath === path) this.doc = `Could not load ${path}: ${(e as Error).message}`;
      });
  }

  private renderCompare() {
    const at = this.loc!;
    const v = at.view as Extract<View, { kind: "compare" }>;
    const c = this.comparison;

    const swap = () => go(`/${at.owner}/${at.name}/compare/${v.head}...${v.base}`);
    const pick = (which: "base" | "head") => (e: Event) => {
      const value = (e as CustomEvent<string>).detail;
      const base = which === "base" ? value : v.base;
      const head = which === "head" ? value : v.head;
      go(`/${at.owner}/${at.name}/compare/${base}...${head}`);
    };

    return (
      <div>
        <div class="cmp-bar">
          <span class="muted">merge</span>
          <branch-picker refs={this.branches()} current={v.head} onPick={pick("head")}></branch-picker>
          <span class="muted">into</span>
          <branch-picker refs={this.branches()} current={v.base} onPick={pick("base")}></branch-picker>
          <button class="bare" onClick={swap} title="swap sides">swap</button>
        </div>

        {!c ? (
          <div class="panel"><div class="loading">comparing</div></div>
        ) : (
          <div>
            <div class={`verdict ${c.up_to_date ? "ok" : c.mergeable ? "ok" : "bad"}`}>
              <span class="vmark">
                <loom-icon name={c.mergeable && !c.up_to_date ? "check" : c.up_to_date ? "check" : "commit"} size={14}></loom-icon>
              </span>
              <div class="grow">
                <div class="vtitle">
                  {c.up_to_date
                    ? `${v.base} already contains everything in ${v.head}`
                    : c.fast_forward
                      ? "fast-forward — no merge commit needed"
                      : c.mergeable
                        ? "these branches can be merged automatically"
                        : `${c.conflicts.length} conflict(s) must be resolved by hand`}
                </div>
                <div class="vsub">
                  {c.up_to_date ? (
                    "Nothing to merge."
                  ) : (
                    <>
                      {c.ahead} commit(s) ahead, {c.behind} behind
                      {c.merge_base_short ? ` · common ancestor ${c.merge_base_short}` : " · unrelated histories"}
                    </>
                  )}
                </div>
              </div>
              {!c.up_to_date && this.repo!.access !== "read" ? (
                <button
                  class="primary"
                  disabled={this.busy}
                  onClick={() => void this.openRequest(v.base, v.head)}
                >
                  open merge request
                </button>
              ) : null}
              {!c.up_to_date ? (
                <code class="howto">fkit merge {v.head}</code>
              ) : null}
            </div>

            {c.conflicts.length > 0 ? (
              <div class="panel" style="margin-bottom:12px">
                <div class="panel-head"><span>conflicts</span></div>
                {c.conflicts.map((x) => (
                  <div class="ch">
                    <span class="st modified"><loom-icon name="alert" size={12}></loom-icon></span>
                    <span>{x.path}</span>
                    <span class="muted" style="font-size:11px">{x.detail}</span>
                  </div>
                ))}
              </div>
            ) : null}

            {c.commits.length > 0 ? (
              <div class="panel commits" style="margin-bottom:12px">
                <div class="panel-head">
                  <span>commits on {v.head}</span>
                  <span class="val faint">{c.ahead}</span>
                </div>
                {c.commits.map((cm) => {
                  const href = `/${at.owner}/${at.name}/commit/${cm.hash}`;
                  return (
                    <div class="c">
                      <a class="m" href={href} onClick={linkHandler(href)}>
                        {cm.summary || "(no message)"}
                      </a>
                      <span class="by">{authorName(cm.author)} · {relativeTime(cm.timestamp)}</span>
                      <a class="sha" href={href} onClick={linkHandler(href)}>{cm.short}</a>
                    </div>
                  );
                })}
              </div>
            ) : null}

            {c.files.length === 0 ? (
              <div class="panel"><div class="empty">no file changes</div></div>
            ) : (
              <div>
                <div class="patch-bar">
                  <span>{c.files.length} file(s) changed</span>
                  <span class="plus">
                    +{c.files.reduce((a, f) => a + f.added, 0)}
                  </span>
                  <span class="minus">
                    {`\u2212${c.files.reduce((a, f) => a + f.removed, 0)}`}
                  </span>
                </div>
                {c.files.map((f) => this.renderFileDiff(f, v.head))}
              </div>
            )}
          </div>
        )}
      </div>
    );
  }

  private stateTag(state: string) {
    return <span class={`mstate ${state}`}>{state}</span>;
  }

  /**
   * Tags — the releases view.
   *
   * A tag is a ref that does not move, so the row says what it names rather
   * than only that it exists: the commit's summary, who made it, and when.
   * Sorted newest-first by the commit's own timestamp, not the ref's
   * updated_at, because pushing an old tag later should not put it on top.
   */
  private renderTags() {
    const at = this.loc!;
    const tags = this.tags().slice().sort((a, b) => {
      const ta = a.head?.timestamp ?? 0;
      const tb = b.head?.timestamp ?? 0;
      return tb - ta || a.name.localeCompare(b.name);
    });

    if (tags.length === 0) {
      // The commands are only useful to someone who could run them. Showing
      // them to a visitor reads as an instruction they cannot follow.
      const canPush = this.repo?.access === "write" || this.repo?.access === "admin";
      return (
        <div class="panel">
          <div class="panel-head"><span>tags</span></div>
          <div class="empty">
            <h2>no tags yet</h2>
            <p class="prose">
              A tag marks a commit with a name that does not move — a release, a
              revision someone else has to be able to find again.
            </p>
            {canPush ? (
              <pre class="cmd">fkit tag v1.0
fkit push</pre>
            ) : null}
          </div>
        </div>
      );
    }

    return (
      <div class="panel">
        <div class="panel-head">
          <span>tags</span>
          <span class="val">{tags.length}</span>
        </div>
        {tags.map((t) => {
          const tree = `/${at.owner}/${at.name}/tree/${t.name}`;
          const commit = `/${at.owner}/${at.name}/commit/${t.target}`;
          return (
            // The row is the link — the whole thing, the way a file row is.
            // The commit hash is a link inside it to somewhere else, so it
            // stops the click from bubbling out to the row's own navigation.
            <a class="tagrow" href={tree} onClick={linkHandler(tree)}>
              <span class="ic"><loom-icon name="tag" size={13}></loom-icon></span>
              <span class="nm">
                {t.name}
                {t.head ? <span class="msg">{t.head.summary}</span> : null}
              </span>
              <span
                class="sha"
                onClick={(e: MouseEvent) => {
                  e.preventDefault();
                  e.stopPropagation();
                  go(commit);
                }}
                title="View this commit"
              >
                {t.short}
              </span>
              <span class="when">
                {t.head ? relativeTime(t.head.timestamp) : relativeTime(t.updated_at)}
              </span>
            </a>
          );
        })}
      </div>
    );
  }

  /// The issue list, and the form that adds to it.
  private renderIssues() {
    const at = this.loc!;
    const list = this.issuesQuery.data ?? null;
    const tabs: ["open" | "closed" | "all", string][] = [
      ["open", "open"],
      ["closed", "closed"],
      ["all", "all"],
    ];

    return (
      <div class="wrap">
        <fkit-section
          heading="Issues"
          value={list ? `${list.length} ${this.issueFilter}` : ""}
        >
          <span slot="action" class="head-acts">
            <span class="seg">
              {tabs.map(([id, label]) => (
                <button
                  loom-key={id}
                  class={this.issueFilter === id ? "on" : ""}
                  onClick={() => (this.issueFilter = id)}
                >
                  {label}
                </button>
              ))}
            </span>
            {this.session.isAuthed ? (
              <button class="btn" onClick={() => (this.newIssue = !this.newIssue)}>
                <loom-icon name="plus" size={11}></loom-icon> new issue
              </button>
            ) : null}
          </span>

          <fkit-modal
            open={this.newIssue}
            heading="New issue"
            width="680px"
            onClose={() => (this.newIssue = false)}
          >
            <form
              id="new-issue"
              class="new-issue"
              onSubmit={(e: Event) => {
                e.preventDefault();
                const f = e.target as HTMLFormElement;
                const title = (f.elements.namedItem("title") as HTMLInputElement).value;
                const md = f.querySelector("fkit-composer") as
                  | (HTMLElement & { text: string })
                  | null;
                void this.act(async () => {
                  const made = await api.createIssue(at.owner, at.name, {
                    title,
                    body: md?.text ?? "",
                  });
                  this.newIssue = false;
                  go(`/${at.owner}/${at.name}/issues/${made.number}`);
                });
              }}
            >
              <fkit-field label="Title" size="full">
                <input name="title" placeholder="What is wrong, or what should exist" required />
              </fkit-field>
              <fkit-field
                label="Description"
                help="Optional. What you did, what happened, and what you expected instead. Markdown works."
                size="full"
              >
                {/* The composer rather than a bare textarea, so writing an
                    issue and commenting on one are the same box. Its own send
                    button is hidden here: the modal's footer owns the action. */}
                <fkit-composer headless placeholder="Describe it"></fkit-composer>
              </fkit-field>
            </form>

            <span slot="footer">
              <button type="button" class="bare" onClick={() => (this.newIssue = false)}>
                Cancel
              </button>
              <button
                type="button"
                class="primary"
                disabled={this.busy}
                onClick={() => {
                  const f = this.shadowRoot?.querySelector("#new-issue") as HTMLFormElement | null;
                  // requestSubmit rather than submit(), so `required` on the
                  // title is actually enforced.
                  f?.requestSubmit();
                }}
              >
                Open issue
              </button>
            </span>
          </fkit-modal>

          <fkit-list>
            {list === null ? (
              <fkit-empty><span class="sk" style="width:220px"></span></fkit-empty>
            ) : list.length === 0 ? (
              <fkit-empty>
                {this.issueFilter === "open"
                  ? "No open issues. Anything wrong, or anything missing, goes here."
                  : `No ${this.issueFilter} issues.`}
              </fkit-empty>
            ) : (
              list.map((i) => {
                const href = `/${at.owner}/${at.name}/issues/${i.number}`;
                return (
                  <fkit-row
                    loom-key={i.number}
                    icon={i.state === "open" ? "alert" : "check"}
                    current={i.state === "open"}
                    name=""
                    meta=""
                  >
                    <span slot="main" class="issue-line">
                      <a class="t" href={href} onClick={linkHandler(href)}>{i.title}</a>
                      <span class="sub">
                        #{i.number} opened {relativeTime(i.created_at)}
                        {i.author ? ` by ${i.author}` : ""}
                        {i.state === "closed" && i.closed_at
                          ? ` · closed ${relativeTime(i.closed_at)}`
                          : ""}
                      </span>
                    </span>
                    {i.comments > 0 ? (
                      <span class="cbadge">
                        <loom-icon name="merge" size={11}></loom-icon> {i.comments}
                      </span>
                    ) : null}
                  </fkit-row>
                );
              })
            )}
          </fkit-list>
        </fkit-section>
      </div>
    );
  }

  /// One issue: what it says, and everything said about it.
  private renderIssue(number: number) {
    const at = this.loc!;
    const i = this.issueQuery.data ?? null;
    const talk = this.issueTalkQuery.data ?? null;
    const me = this.session.current?.username;

    if (this.issueQuery.error) {
      return (
        <div class="wrap">
          <div class="panel"><div class="empty"><h2>no such issue</h2></div></div>
        </div>
      );
    }

    return (
      <div class="wrap">
        <fkit-section
          heading={i ? i.title : "Issue"}
          value={i ? `#${i.number}` : ""}
        >
          {i ? (
            <span slot="action" class="head-acts">
              <span class={`tag ${i.state === "open" ? "on" : ""}`}>{i.state}</span>
              {this.session.isAuthed ? (
                <button
                  disabled={this.busy}
                  onClick={() =>
                    void this.act(async () => {
                      await api.setIssueState(at.owner, at.name, i.number, i.state === "closed");
                      await this.issueQuery.refetch();
                    })
                  }
                >
                  {i.state === "open" ? "Close issue" : "Reopen"}
                </button>
              ) : null}
            </span>
          ) : null}

          {i === null ? (
            <span class="sk" style="width:260px"></span>
          ) : (
            <>
              <div class="issue-meta">
                {i.author ?? "someone"} opened this {relativeTime(i.created_at)}
              </div>

              <div class="talk">
                {i.body ? (
                  <fkit-comment
                    author={i.author ?? ""}
                    when={relativeTime(i.created_at)}
                    body={i.body}
                    mine={i.author === me}
                  ></fkit-comment>
                ) : null}

                {talk === null ? (
                  <span class="sk" style="width:200px"></span>
                ) : (
                  talk.map((c) =>
                    this.renderComment(c, () => this.issueTalkQuery.refetch()),
                  )
                )}

                {this.session.isAuthed ? (
                  <fkit-composer
                    label="Comment"
                    placeholder="Add to this issue"
                    busy={this.busy}
                    onSend={(e: Event) => {
                      const body = (e as CustomEvent<string>).detail;
                      void this.act(async () => {
                        await api.commentOnIssue(at.owner, at.name, number, { body });
                        await this.issueTalkQuery.refetch();
                        for (const el of this.shadowRoot?.querySelectorAll("fkit-composer") ?? []) {
                          (el as HTMLElement & { clear(): void }).clear();
                        }
                      });
                    }}
                  ></fkit-composer>
                ) : null}
              </div>
            </>
          )}
        </fkit-section>
      </div>
    );
  }

  private renderMergeList() {
    const at = this.loc!;
    const list = this.merges;
    const canOpen = this.repo!.access !== "read" && this.repo!.access !== "none";

    return (
      <div>
        <div class="cmp-bar">
          {(["open", "merged", "closed", "all"] as const).map((k) => (
            <button
              class={this.mergeState === k ? "" : "bare"}
              onClick={async () => {
                // The query takes its state through `params`, so this
                // assignment changes the key and the refetch is the decorator's.
                this.mergeState = k;
              }}
            >
              {k}
            </button>
          ))}
          <div class="grow"></div>
          {canOpen ? (
            <a
              class="btn primary"
              href={`/${at.owner}/${at.name}/compare/${this.repo!.default_branch}...${this.refName()}`}
              onClick={linkHandler(
                `/${at.owner}/${at.name}/compare/${this.repo!.default_branch}...${this.refName()}`,
              )}
            >
              <loom-icon name="plus" size={12}></loom-icon> new merge request
            </a>
          ) : null}
        </div>

        {list === null ? (
          <div class="panel">
            {[0, 1, 2].map(() => (
              <div class="mrow sk-row">
                <span class="sk" style="width:13px;height:13px"></span>
                <span class="sk" style="width:min(50%,280px)"></span>
                <span class="sk" style="width:90px"></span>
              </div>
            ))}
          </div>
        ) : list.length === 0 ? (
          <div class="panel">
            <div class="empty">
              <h2>no {this.mergeState === "all" ? "" : this.mergeState} merge requests</h2>
              <p class="prose">
                Compare two branches to propose one.
              </p>
            </div>
          </div>
        ) : (
          <div class="panel">
            {list.map((m) => {
              const href = `/${at.owner}/${at.name}/merges/${m.number}`;
              return (
                <div class="mrow">
                  {this.stateTag(m.state)}
                  <a class="mtitle" href={href} onClick={linkHandler(href)}>
                    <span class="num">#{m.number}</span> {m.title}
                  </a>
                  <span class="mbr">
                    {m.source_branch} <span class="faint">into</span> {m.target_branch}
                  </span>
                  <span class="faint" style="font-size:11px">{relativeTime(m.created_at)}</span>
                </div>
              );
            })}
          </div>
        )}
      </div>
    );
  }

  private async act(fn: () => Promise<unknown>, ok?: string) {
    this.busy = true;
    this.error = "";
    this.notice = "";
    try {
      await fn();
      if (ok) this.notice = ok;
      const at = this.loc!;
      if (at.view.kind === "merge") {
        await this.mrQuery.refetch();
        // A merge moves a branch, so the refs the page is holding are stale.
        await this.refsQuery.refetch();
      }
    } catch (e) {
      this.error = (e as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private renderMergeRequest() {
    const at = this.loc!;
    const m = this.mr;
    if (!m) return <div class="panel"><div class="loading">loading merge request</div></div>;

    const c = m.comparison;
    const open = m.state === "open";

    return (
      <div>
        <div class="chead">
          <div class="chead-top">
            <h2 class="csummary">
              <span class="num">#{m.number}</span> {m.title}
            </h2>
            {this.stateTag(m.state)}
          </div>
          {m.description ? <pre class="cbody">{m.description}</pre> : null}
          <div class="cmeta">
            <span class="who">{m.author ?? "unknown"}</span>
            <span>opened {relativeTime(m.created_at)}</span>
            <span class="mbr">
              {m.source_branch} <span class="faint">into</span> {m.target_branch}
            </span>
            {m.merge_commit ? (
              <span class="parent">
                merged as{" "}
                <a
                  href={`/${at.owner}/${at.name}/commit/${m.merge_commit}`}
                  onClick={linkHandler(`/${at.owner}/${at.name}/commit/${m.merge_commit}`)}
                >
                  {m.merge_commit.slice(0, 10)}
                </a>
              </span>
            ) : null}
          </div>
        </div>

        {!c ? (
          <div class="panel">
            <div class="empty">
              <h2>branches unavailable</h2>
              <p class="prose">
                One of the branches for this request no longer exists, so its diff cannot be shown.
              </p>
            </div>
          </div>
        ) : (
          <div>
            <div class={`verdict ${c.mergeable || c.up_to_date ? "ok" : "bad"}`}>
              <span class="vmark">
                <loom-icon name={c.mergeable || c.up_to_date ? "check" : "commit"} size={14}></loom-icon>
              </span>
              <div class="grow">
                <div class="vtitle">
                  {!open
                    ? `This request is ${m.state}.`
                    : c.up_to_date
                      ? "Already merged — nothing left to apply."
                      : c.fast_forward
                        ? "Fast-forward: no merge commit needed."
                        : c.mergeable
                          ? "No conflicts. This can be merged."
                          : `${c.conflicts.length} conflict(s) must be resolved first.`}
                </div>
                <div class="vsub">
                  {c.ahead} commit(s) ahead, {c.behind} behind
                  {c.merge_base_short ? ` · common ancestor ${c.merge_base_short}` : ""}
                </div>
              </div>

              {open && m.can_merge && c.mergeable && !c.up_to_date ? (
                <button
                  class="primary"
                  disabled={this.busy}
                  onClick={() =>
                    void this.act(() => api.performMerge(at.owner, at.name, m.number))
                  }
                >
                  {this.busy ? "merging…" : `merge into ${m.target_branch}`}
                </button>
              ) : null}
              {open && m.can_merge ? (
                <button
                  class="bare"
                  disabled={this.busy}
                  onClick={() => void this.act(() => api.closeMerge(at.owner, at.name, m.number))}
                >
                  close
                </button>
              ) : null}
              {!open && m.state === "closed" && m.can_merge === false && this.repo!.access !== "read" ? (
                <button
                  class="bare"
                  disabled={this.busy}
                  onClick={() => void this.act(() => api.reopenMerge(at.owner, at.name, m.number))}
                >
                  reopen
                </button>
              ) : null}
            </div>

            {c.conflicts.length > 0 ? (
              <div class="panel" style="margin-bottom:12px">
                <div class="panel-head"><span>conflicts</span></div>
                {c.conflicts.map((x) => (
                  <div class="ch">
                    <span class="st modified"><loom-icon name="alert" size={12}></loom-icon></span>
                    <span>{x.path}</span>
                    <span class="muted" style="font-size:11px">{x.detail}</span>
                  </div>
                ))}
              </div>
            ) : null}

            {c.commits.length > 0 ? (
              <div class="panel commits" style="margin-bottom:12px">
                <div class="panel-head">
                  <span>commits</span>
                  <span class="val faint">{c.ahead}</span>
                </div>
                {c.commits.map((cm) => {
                  const href = `/${at.owner}/${at.name}/commit/${cm.hash}`;
                  return (
                    <div class="c">
                      <a class="m" href={href} onClick={linkHandler(href)}>
                        {cm.summary || "(no message)"}
                      </a>
                      <span class="by">{authorName(cm.author)} · {relativeTime(cm.timestamp)}</span>
                      <a class="sha" href={href} onClick={linkHandler(href)}>{cm.short}</a>
                    </div>
                  );
                })}
              </div>
            ) : null}

            {c.files.length > 0 ? (
              <div class="review">
                <fkit-file-tree
                  files={c.files.map((f) => ({
                    path: f.path,
                    status: f.status,
                    added: f.added,
                    removed: f.removed,
                  }))}
                  active={this.viewing}
                  onPick={(e: Event) => this.jumpToFile((e as CustomEvent<string>).detail)}
                ></fkit-file-tree>

                <div>
                  {c.files.map((f) => (
                    <>
                      {this.renderFileDiff(f, m.source_branch)}
                      {this.renderOutdated(f)}
                    </>
                  ))}
                </div>
              </div>
            ) : null}

            {this.renderConversation(m.number)}
          </div>
        )}
      </div>
    );
  }

  /// Scroll a file into view when it is picked from the tree.
  private jumpToFile(path: string) {
    this.viewing = path;
    const el = this.shadowRoot?.querySelector(`[data-file="${CSS.escape(path)}"]`);
    el?.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  /// Comments written against a version of this file that no longer exists.
  ///
  /// Shown under the file rather than beside a line, because the line they
  /// described is gone. Drawing them against whatever now occupies that line
  /// number would be worse than not drawing them at all.
  private renderOutdated(f: FileDiff) {
    const stale = this.staleComments(f);
    if (stale.length === 0) return null;

    return (
      <div class="thread-out">
        <div class="where">
          {stale.length} {stale.length === 1 ? "comment" : "comments"} on an earlier version of{" "}
          <b>{f.path}</b>. The lines they were written against have changed.
        </div>
        {stale.map((c) =>
          this.renderComment(c, () => this.commentsQuery.refetch(), `line ${c.line}`),
        )}
      </div>
    );
  }

  /// The comments that are about the change as a whole rather than a line.
  private renderConversation(number: number) {
    const all = this.comments;
    const talk = (all ?? []).filter((c) => !c.blob);

    return (
      <div class="talk">
        <div class="patch-bar">
          <span>Conversation</span>
          {all ? <span class="muted">{talk.length}</span> : null}
        </div>

        {all === null ? (
          <span class="sk" style="width:200px"></span>
        ) : talk.length === 0 ? (
          <div class="none">Nothing said yet about this change as a whole.</div>
        ) : (
          talk.map((c) => this.renderComment(c, () => this.commentsQuery.refetch()))
        )}

        {this.session.isAuthed ? (
          <fkit-composer
            label="Comment"
            placeholder="Leave a comment on this merge request"
            busy={this.busy}
            onSend={(e: Event) => void this.postComment((e as CustomEvent<string>).detail)}
          ></fkit-composer>
        ) : null}
      </div>
    );
  }

  /// Create a request from the compare view and go straight to it.
  private async openRequest(base: string, head: string) {
    const at = this.loc!;
    const title = `Merge ${head} into ${base}`;
    this.busy = true;
    this.error = "";
    try {
      const m = await api.createMerge(at.owner, at.name, {
        title,
        source_branch: head,
        target_branch: base,
      });
      go(`/${at.owner}/${at.name}/merges/${m.number}`);
    } catch (e) {
      this.error = (e as Error).message;
    } finally {
      this.busy = false;
    }
  }

  /// A copyable command block.
  ///
  /// Every one of these exists to be pasted into a terminal, so every one gets
  /// a copy button — selecting three lines of shell by hand is exactly the
  /// friction this panel is supposed to remove.
  private codeBlock(id: string, label: string, text: string) {
    const done = this.copiedKey === id;
    return (
      <fkit-list heading={label}>
        <button slot="action" class="bare" onClick={() => void this.copyText(id, text)}>
          <loom-icon name={done ? "check" : "copy"} size={11}></loom-icon>
          {done ? "copied" : "copy"}
        </button>
        <pre class="cmd-block">{text}</pre>
      </fkit-list>
    );
  }

  private async copyText(id: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      this.copiedKey = id;
      setTimeout(() => {
        if (this.copiedKey === id) this.copiedKey = "";
      }, 1400);
    } catch {
      this.error = "could not copy — the browser refused clipboard access";
    }
  }

  /// What actually gets someone from nothing to a pushed repository.
  ///
  /// The previous version showed `fkit remote … && fkit push`, which assumed a
  /// repository that already existed, already had a commit, and already had a
  /// token — every step except the one it printed.
  private renderSetup(r: Repo) {
    const url = syncUrl(r.owner, r.name);
    // Only offer push instructions to someone who could actually push. An
    // anonymous visitor being told to run `fkit push` is noise at best and a
    // confusing dead end at worst.
    const canPush = r.access === "write" || r.access === "admin";
    const signedIn = this.session.isAuthed;

    return (
      <div class="setup">
        {this.codeBlock("url", "remote", url)}

        {canPush
          ? this.codeBlock(
              "existing",
              "push an existing project",
              `cd my-project\nfkit remote ${url}\nfkit push`,
            )
          : null}

        {canPush
          ? this.codeBlock(
              "new",
              "or start a new one",
              `fkit init my-project && cd my-project\n` +
                `fkit config --global author.name "Your Name"\n` +
                `fkit config --global author.email you@example.com\n` +
                `fkit commit -m "first commit"\n` +
                `fkit remote ${url}\nfkit push`,
            )
          : null}

        {this.codeBlock("clone", "clone it", `fkit clone ${url}`)}

        <div class="setup-note">
          <loom-icon name={canPush ? "key" : "lock"} size={12}></loom-icon>
          <span>
            {canPush ? (
              <>
                Pushing needs <code>FKIT_TOKEN</code>.{" "}
                <a href="/settings/tokens" onClick={linkHandler("/settings/tokens")}>
                  Create one
                </a>
                {r.visibility === "private" ? " — cloning too, it's private." : "."}
              </>
            ) : signedIn ? (
              <>Read access only. Ask {r.owner} for write.</>
            ) : (
              <>
                <a href="/login" onClick={linkHandler("/login")}>Sign in</a> to push.
              </>
            )}
          </span>
        </div>
      </div>
    );
  }

  private renderSettings() {
    const r = this.repo!;
    const at = this.loc!;
    const section = (at.view as Extract<View, { kind: "settings" }>).section;

    if (r.access !== "admin") {
      return (
        <div class="panel">
          <div class="empty">
            <h2>not available</h2>
            <p class="prose">Repository settings need admin access.</p>
          </div>
        </div>
      );
    }

    const tabs: [string, string, string][] = [
      ["general", "general", "settings"],
      ["branches", "branches", "branch"],
      ["access", "access", "lock"],
      ["setup", "push & clone", "link"],
      ["danger", "danger zone", "alert"],
    ];

    return (
      <div class="cols">
        <div class="rail">
          <h2>{r.name}</h2>
          {tabs.map(([id, label, ic]) => {
            const href = `/${at.owner}/${at.name}/settings/${id}`;
            return (
              <a class={section === id ? "on" : ""} href={href} onClick={linkHandler(href)}>
                <loom-icon name={ic} size={12}></loom-icon>
                {label}
              </a>
            );
          })}
        </div>

        <div class="sec">
          {section === "general" ? this.settingsGeneral(r, at) : null}
          {section === "branches" ? this.settingsBranches(r, at) : null}
          {section === "access" ? this.settingsAccess(r, at) : null}
          {section === "setup" ? (
            <fkit-page heading="Push & clone" value={syncUrl(r.owner, r.name)}>
              <fkit-section blurb="Every command below is copyable. Pushing needs a token with write access; cloning a public repository needs nothing.">
                {this.renderSetup(r)}
              </fkit-section>
            </fkit-page>
          ) : null}
          {section === "danger" ? this.settingsDanger(r, at) : null}
        </div>
      </div>
    );
  }

  private settingsGeneral(r: Repo, at: { owner: string; name: string }) {
    const branches = this.branches();
    return (
      <fkit-page heading="General" value={r.full_name}>
        <fkit-section blurb="Everything here is visible to anyone who can see the repository.">
          <form
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const at2 = (n: string) => (f.elements.namedItem(n) as HTMLInputElement).value;
              void this.act(async () => {
                await api.updateRepo(at.owner, at.name, {
                  description: at2("description"),
                  homepage: at2("homepage"),
                  // Comma or space separated; the server normalises and
                  // de-duplicates, so this only has to split.
                  topics: this.topicDraft ?? r.topics ?? [],
                });
              }, "Repository updated");
            }}
          >
            <fkit-field
              label="Repository name"
              help="Permanent. The name is part of every clone URL anyone has taken."
            >
              <input value={r.name} disabled />
            </fkit-field>

            <fkit-field
              label="Description"
              help="Shown beside the name in listings and at the top of the page."
              size="wide"
            >
              <input
                name="description"
                value={r.description ?? ""}
                placeholder="One line about what this is"
              />
            </fkit-field>

            <fkit-field
              label="Website"
              help="Linked from the About panel. Must start with http:// or https:// — anything else would run in a visitor's session instead of taking them somewhere."
              size="wide"
            >
              <input name="homepage" value={r.homepage ?? ""} placeholder="https://fkit.work" />
            </fkit-field>

            <fkit-field
              label="Topics"
              help="Enter, comma or space adds one. Backspace takes the last one back. Letters, digits, hyphen and dot; up to 20."
              size="wide"
            >
              <fkit-tags
                value={this.topicDraft ?? r.topics ?? []}
                placeholder="rust, version-control, merkle"
                onChange={(e: Event) => (this.topicDraft = (e as CustomEvent<string[]>).detail)}
              ></fkit-tags>
            </fkit-field>

            <fkit-actions>
              <button class="primary" type="submit" disabled={this.busy}>Save changes</button>
              {this.notice ? <span class="ok">{this.notice}</span> : null}
            </fkit-actions>
          </form>
        </fkit-section>

        <fkit-section
          heading="Default branch"
          value={r.default_branch}
          blurb={`Opened when a URL names no branch. ${branches.length} ${branches.length === 1 ? "branch" : "branches"} available.`}
        >
          <fkit-field size="narrow">
            <fkit-select
              value={r.default_branch}
              options={branches.map((b) => ({ value: b.name, label: b.name, hint: b.short }))}
              onPick={(e: Event) => {
                const v = (e as CustomEvent<string>).detail;
                void this.act(async () => {
                  await api.updateRepo(at.owner, at.name, { default_branch: v });
                  await this.repoQuery.refetch();
                }, "Default branch updated");
              }}
            ></fkit-select>
          </fkit-field>
        </fkit-section>
      </fkit-page>
    );
  }

  /// Branches and tags, and the only place either can be removed.
  ///
  /// Deleting a ref removes a name, not history: the commits stay in the
  /// store, addressable by hash. That is worth saying on the page, because
  /// "delete branch" reads as destructive and here it very nearly is not.
  private settingsBranches(r: Repo, at: { owner: string; name: string }) {
    const branches = this.branches();
    const tags = this.tags();
    const loading = this.refs === null;

    const remove = (kind: "branch" | "tag", name: string, why: string) => async () => {
      const ok = await confirmAction({
        title: `Delete ${kind} ${name}?`,
        body: why,
        confirm: `Delete ${kind}`,
        danger: true,
      });
      if (!ok) return;
      await this.act(async () => {
        await api.deleteRef(at.owner, at.name, kind, name);
        await this.refsQuery.refetch();
      }, `${name} deleted`);
    };

    return (
      <fkit-page>
        <fkit-section
          heading="Branches"
          value={loading ? "" : `${branches.length} · default ${r.default_branch}`}
          blurb="Deleting a branch removes the name. The commits it pointed at stay in the store and are still reachable by hash, so this is not a way to erase work."
        >
          <fkit-list>
            {loading ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : (
              branches.map((b) => (
                <fkit-row
                  loom-key={b.name}
                  icon="branch"
                  current={b.is_default}
                  name={b.name}
                  meta={b.head ? `${b.short} · ${b.head.summary}` : b.short}
                >
                  {b.is_default ? <span class="tag on">default</span> : null}
                  <button
                    class="danger bare"
                    // The default branch is what a clone checks out; removing
                    // it would leave the repository pointing at nothing, so
                    // the server refuses it and so does the button.
                    disabled={b.is_default || this.busy}
                    title={b.is_default ? "Choose a different default branch first" : ""}
                    onClick={remove(
                      "branch",
                      b.name,
                      `The branch name goes; its commits stay in the store. Anyone who has ${b.name} checked out keeps their copy.`,
                    )}
                  >
                    Delete
                  </button>
                </fkit-row>
              ))
            )}
          </fkit-list>
        </fkit-section>

        <fkit-section
          heading="Tags"
          value={loading ? "" : `${tags.length}`}
          blurb="A tag names one commit and is not expected to move."
        >
          <fkit-list>
            {loading ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : tags.length === 0 ? (
              <fkit-empty>No tags. Push one with fkit tag.</fkit-empty>
            ) : (
              tags.map((t) => (
                <fkit-row
                  loom-key={t.name}
                  icon="tag"
                  name={t.name}
                  meta={t.head ? `${t.short} · ${t.head.summary}` : t.short}
                >
                  <button
                    class="danger bare"
                    disabled={this.busy}
                    onClick={remove(
                      "tag",
                      t.name,
                      `The tag name goes; the commit it points at stays in the store.`,
                    )}
                  >
                    Delete
                  </button>
                </fkit-row>
              ))
            )}
          </fkit-list>
        </fkit-section>
      </fkit-page>
    );
  }

  private settingsAccess(r: Repo, at: { owner: string; name: string }) {
    const people = this.collaborators;
    return (
      <fkit-page>
        <fkit-section
          heading="Visibility"
          value={r.visibility}
          blurb="Who can read this repository and clone it."
        >
          <fkit-list>
            <fkit-choice
              value={r.visibility}
              disabled={this.busy}
              options={[
                {
                  value: "private",
                  label: "Private",
                  icon: "lock",
                  hint: "Only you and the collaborators below. Cloning requires a token.",
                },
                {
                  value: "public",
                  label: "Public",
                  icon: "repo",
                  hint: "Anyone can read and clone it, with or without an account. Pushing still requires write access.",
                },
              ]}
              onPick={(e: Event) => {
                const v = (e as CustomEvent<string>).detail;
                void this.act(async () => {
                  await api.updateRepo(at.owner, at.name, { visibility: v });
                  await this.repoQuery.refetch();
                }, "Visibility updated");
              }}
            ></fkit-choice>
          </fkit-list>
        </fkit-section>

        <fkit-section
          heading="Collaborators"
          value={people ? `${people.length + 1} with access` : ""}
          blurb="Read can clone; write can also push; admin can also change these settings. There is no self-service join — you add people here."
        >
          <form
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const user = f.elements.namedItem("username") as HTMLInputElement;
              void this.act(async () => {
                await api.addCollaborator(at.owner, at.name, user.value.trim(), this.newRole);
                await this.collaboratorsQuery.refetch();
                user.value = "";
              }, "Collaborator added");
            }}
          >
            <fkit-add>
              <fkit-field label="Username" size="mid">
                <input name="username" placeholder="username" required />
              </fkit-field>
              <fkit-field label="Role" size="narrow">
                <fkit-select
                  value={this.newRole}
                  options={[
                    { value: "read", label: "read", hint: "Clone only." },
                    { value: "write", label: "write", hint: "Clone and push." },
                    { value: "admin", label: "admin", hint: "Also settings." },
                  ]}
                  onPick={(e: Event) => (this.newRole = (e as CustomEvent<string>).detail)}
                ></fkit-select>
              </fkit-field>
              <button class="primary" type="submit" disabled={this.busy}>
                <loom-icon name="plus" size={12}></loom-icon> Add
              </button>
            </fkit-add>
          </form>

          <fkit-list>
            <fkit-row icon="check" current name={r.owner} meta="Owner — created this repository">
              <span class="tag on">owner</span>
            </fkit-row>
            {people === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : (
              people.map((c) => (
                <fkit-row
                  loom-key={c.username}
                  icon="user"
                  name={c.username}
                  meta={`Added ${relativeTime(c.granted_at)}`}
                >
                  <span class="tag">{c.role}</span>
                  <button
                    class="danger bare"
                    disabled={this.busy}
                    onClick={async () => {
                      const ok = await confirmAction({
                        title: `Remove ${c.username}?`,
                        body: `They lose access to ${r.full_name} immediately. Anything already cloned stays on their machine.`,
                        confirm: "Remove",
                        danger: true,
                      });
                      if (!ok) return;
                      await this.act(async () => {
                        await api.removeCollaborator(at.owner, at.name, c.username);
                        await this.collaboratorsQuery.refetch();
                      }, "Collaborator removed");
                    }}
                  >
                    Remove
                  </button>
                </fkit-row>
              ))
            )}
          </fkit-list>
        </fkit-section>
      </fkit-page>
    );
  }

  private settingsDanger(r: Repo, at: { owner: string; name: string }) {
    const g = this.gcReport;
    return (
      <fkit-page heading="Danger zone" value={r.full_name}>
        {/* Not destructive in the way the box below is — it removes only what
            no branch or tag can reach, keeps anything recent whatever it is
            asked, and verifies what survives. It lives here because it is the
            other maintenance task nobody visits this page for by accident. */}
        <fkit-section
          heading="Storage"
          value={g ? `${g.reachable} reachable · ${g.unreachable} not` : ""}
          blurb="Deleting a branch removes a name, not the commits under it. This is what actually reclaims the space: objects no branch or tag can reach are removed, and shared chunks are kept as long as anything still points at them. Objects younger than a day are always kept, because a push writes its objects before it moves the ref."
        >
          <fkit-actions>
            <button
              disabled={this.busy}
              onClick={() =>
                void this.act(async () => {
                  this.gcReport = await api.gc(at.owner, at.name, true);
                })
              }
            >
              Check what is unreferenced
            </button>
            {g && g.unreachable > g.too_young ? (
              <button
                class="danger"
                disabled={this.busy}
                onClick={async () => {
                  const ok = await confirmAction({
                    title: "Reclaim unreferenced objects?",
                    body: `${g.unreachable - g.too_young} object(s) can be removed. Anything still reachable from a branch or tag is kept, and every surviving object is verified afterwards.`,
                    confirm: "Reclaim",
                    danger: true,
                  });
                  if (!ok) return;
                  await this.act(async () => {
                    this.gcReport = await api.gc(at.owner, at.name, false);
                    await this.statsQuery.refetch();
                  }, "Storage reclaimed");
                }}
              >
                Reclaim
              </button>
            ) : null}
            {this.notice ? <span class="ok">{this.notice}</span> : null}
          </fkit-actions>

          {g ? (
            <p class="gc-report">
              {g.total} object(s): {g.reachable} reachable, {g.unreachable} not.{" "}
              {g.too_young > 0 ? `${g.too_young} of those are too recent to touch yet. ` : ""}
              {g.dry_run
                ? g.unreachable > g.too_young
                  ? "Nothing has been removed."
                  : "Nothing to reclaim."
                : `Removed ${g.loose_removed + g.packed_dropped}, compacting ${g.segments_compacted} segment(s) and reclaiming ${humanSize(g.bytes_reclaimed)}.`}
            </p>
          ) : null}
        </fkit-section>

        <fkit-section heading="Delete" blurb="Nothing here can be undone from this page.">
          <fkit-danger>
            <fkit-danger-row
              name="Delete this repository"
              why="Removes the repository, every branch, and all objects stored for it. Clones on other machines keep working; this server keeps nothing."
            >
              <button
                class="danger"
                disabled={this.busy}
                onClick={async () => {
                  const ok = await confirmAction({
                    title: `Delete ${r.full_name}?`,
                    body: "This cannot be undone. Type the repository name to confirm.",
                    confirm: "Delete repository",
                    danger: true,
                    typeToConfirm: r.name,
                  });
                  if (!ok) return;
                  await this.act(async () => {
                    await api.deleteRepo(at.owner, at.name);
                    go("/");
                  });
                }}
              >
                <loom-icon name="trash" size={12}></loom-icon> Delete
              </button>
            </fkit-danger-row>
          </fkit-danger>
        </fkit-section>
      </fkit-page>
    );
  }

  update() {
    if (this.notFound || !this.loc) {
      return (
        <div class="wrap">
          <div class="panel">
            <div class="empty">
              <h2>not found</h2>
              <p class="prose">There is no repository here, or you do not have access to it.</p>
              <a class="btn" href="/" onClick={linkHandler("/")}>back to repositories</a>
            </div>
          </div>
        </div>
      );
    }

    const at = this.loc;
    const r = this.repo;

    if (!r) {
      return (
        <div class="wrap">
          <div class="head">
            <div class="title">
              <span class="sk" style="width:14px;height:14px"></span>
              <span class="sk tall" style="width:200px"></span>
              <span class="sk" style="width:48px"></span>
            </div>
            <div class="tabs" style="gap:14px;padding:5px 0 9px">
              <span class="sk" style="width:36px"></span>
              <span class="sk" style="width:48px"></span>
            </div>
          </div>
          <div class="panel files">
            {[0, 1, 2, 3, 4].map(() => (
              <div class="r sk-row">
                <span class="sk" style="width:13px;height:13px"></span>
                <span class="sk" style="width:min(70%,150px)"></span>
                <span class="sk" style="width:min(60%,190px)"></span>
                <span class="sk" style="width:58px"></span>
                <span class="sk" style="width:44px"></span>
              </div>
            ))}
          </div>
        </div>
      );
    }

    const v = at.view;
    const ref = this.refName();
    // Tags hang off the files view rather than owning a tab, so "files" stays
    // lit while you are on /tags — the same place GitHub leaves you.
    const tab =
      v.kind === "commit"
        ? "commits"
        : v.kind === "merge"
          ? "merges"
          : v.kind === "issue"
            ? "issues"
          : v.kind === "tags"
            ? "tree"
            : v.kind;
    const other =
      this.branches().find((b) => b.name !== r.default_branch)?.name ?? r.default_branch;
    // The trailing number is what is *open*, which is the only count anyone
    // is deciding anything from; a total including years of closed issues
    // would be a bigger number saying less.
    const tabs: [string, string, string, string, number?][] = [
      ["tree", "files", "file", `/${at.owner}/${at.name}/tree/${ref}`],
      ["commits", "history", "history", `/${at.owner}/${at.name}/commits/${ref}`],
      ["issues", "issues", "alert", `/${at.owner}/${at.name}/issues`, r.open_issues],
      ["merges", "merges", "merge", `/${at.owner}/${at.name}/merges`, r.open_merges],
      [
        "compare",
        "compare",
        "compare",
        `/${at.owner}/${at.name}/compare/${r.default_branch}...${other}`,
      ],
    ];
    if (r.access === "admin") {
      tabs.push(["settings", "settings", "settings", `/${at.owner}/${at.name}/settings`]);
    }

    return (
      <div class="wrap">
        <div class="head">
          <div class="title">
            <span class="ic"><loom-icon name={r.visibility === "private" ? "lock" : "repo"} size={14}></loom-icon></span>
            <span class="p">
              <a class="own" href="/" onClick={linkHandler("/")}>{r.owner}</a>
              <span class="own">/</span>
              <a
                href={`/${r.owner}/${r.name}`}
                onClick={linkHandler(`/${r.owner}/${r.name}`)}
                style="color:var(--text)"
              >
                {r.name}
              </a>
            </span>
            <span class="tag">{r.visibility}</span>
            {r.access !== "read" ? <span class="tag on">{r.access}</span> : null}
          </div>
          {r.description ? <div class="desc">{r.description}</div> : null}
          <div class="tabs">
            {tabs.map(([key, label, ic, href, count]) => (
              <a class={tab === key ? "on" : ""} href={href} onClick={linkHandler(href)}>
                <loom-icon name={ic} size={12}></loom-icon>
                {label}
                {/* Zero is not worth a badge — an empty issue tracker should
                    look empty rather than decorated with a nought. */}
                {count ? <span class="tabn">{count}</span> : null}
              </a>
            ))}
          </div>
        </div>

        {this.error ? <div class="error">{this.error}</div> : null}

        {this.refs !== null && this.branches().length === 0 && v.kind !== "settings" ? (
          <div class="panel">
            <div class="panel-head"><span>this repository is empty</span></div>
            <div class="panel-body">{this.renderSetup(r)}</div>
          </div>
        ) : v.kind === "settings" ? (
          this.renderSettings()
        ) : v.kind === "compare" ? (
          this.renderCompare()
        ) : v.kind === "tags" ? (
          this.renderTags()
        ) : v.kind === "issues" ? (
          this.renderIssues()
        ) : v.kind === "issue" ? (
          this.renderIssue(v.number)
        ) : v.kind === "merges" ? (
          this.renderMergeList()
        ) : v.kind === "merge" ? (
          this.renderMergeRequest()
        ) : v.kind === "commit" ? (
          this.detail ? (
            this.renderCommitDetail()
          ) : (
            <div>
              <div class="panel cmsg">
                <div class="panel-body">
                  <span class="sk tall" style="width:min(50%,380px)"></span>
                  <div style="height:10px"></div>
                  <span class="sk" style="width:min(70%,520px)"></span>
                </div>
              </div>
              <div class="panel">
                {[0, 1, 2, 3].map((i) => (
                  <div class="ch sk-row">
                    <span class="sk" style="width:10px"></span>
                    <span class="sk" style={`width:${[46, 62, 34, 54][i]}%`}></span>
                    <span class="sk" style="width:80px"></span>
                  </div>
                ))}
              </div>
            </div>
          )
        ) : (
          <div>
            <div class="toolbar">
              {v.kind !== "commits" ? this.renderCrumbs(v.kind === "tree" || v.kind === "blob" ? v.path : "") : null}
              <div style="flex:1"></div>
              <branch-picker
                refs={this.branches()}
                tags={this.tags()}
                current={ref}
                onPick={(e: Event) => this.switchRef((e as CustomEvent<string>).detail)}
              ></branch-picker>
              <a
                class="refcount"
                href={`/${at.owner}/${at.name}/tags`}
                onClick={linkHandler(`/${at.owner}/${at.name}/tags`)}
                title="Tags"
              >
                <loom-icon name="tag" size={12}></loom-icon>
                <b>{this.tags().length}</b> {this.tags().length === 1 ? "tag" : "tags"}
              </a>
              <a
                class="btn"
                href={`/${at.owner}/${at.name}/commits/${ref}`}
                onClick={linkHandler(`/${at.owner}/${at.name}/commits/${ref}`)}
              >
                <loom-icon name="commit" size={12}></loom-icon> history
              </a>
              <clone-button
                url={syncUrl(r.owner, r.name)}
                visibility={r.visibility}
                archive={`/api/repos/${r.owner}/${r.name}/archive/${encodeURIComponent(ref)}`}
                archiveBytes={this.stats?.archive_bytes ?? 0}
                archiveLimit={this.stats?.archive_limit ?? 0}
              ></clone-button>
            </div>

            {v.kind === "tree" ? (
              this.entries === null ? (
                <div class="panel files">
                  {[0, 1, 2, 3, 4, 5].map(() => (
                    <div class="r sk-row">
                      <span class="sk" style="width:13px;height:13px"></span>
                      <span class="sk" style="width:min(70%,150px)"></span>
                      <span class="sk" style="width:min(60%,190px)"></span>
                      <span class="sk" style="width:58px"></span>
                      <span class="sk" style="width:44px"></span>
                    </div>
                  ))}
                </div>
              ) : (
                <div class="split">
                  <div class="main">
                    {this.renderLatest()}
                    {this.renderTree(v.path)}
                    {this.renderReadme()}
                  </div>
                  {this.renderAside()}
                </div>
              )
            ) : v.kind === "blob" ? (
              this.blob === null ? (
                <div class="panel">
                  <div class="panel-head"><span class="sk" style="width:120px"></span></div>
                  <div style="padding:2px 0">
                    {Array.from({ length: 14 }, (_, i) => (
                      <div class="sk-row" style="display:flex;gap:14px;padding:3px 14px">
                        <span class="sk" style="width:22px;flex:none"></span>
                        <span
                          class="sk"
                          style={`width:${[62, 38, 74, 46, 84, 30, 58][i % 7]}%`}
                        ></span>
                      </div>
                    ))}
                  </div>
                </div>
              ) : (
                this.renderBlob()
              )
            ) : v.kind === "commits" ? (
              this.commits === null ? (
                <div class="panel commits">
                  {[0, 1, 2, 3, 4, 5, 6].map((i) => (
                    <div class="c sk-row">
                      <span class="sk tall" style={`width:${[58, 42, 70, 36, 64, 48, 54][i]}%`}></span>
                      <span class="sk" style="width:120px"></span>
                      <span class="sk" style="width:64px"></span>
                    </div>
                  ))}
                </div>
              ) : (
                this.renderCommits()
              )
            ) : (
              <div class="panel"><div class="empty"><h2>unknown page</h2></div></div>
            )}
          </div>
        )}
      </div>
    );
  }
}
