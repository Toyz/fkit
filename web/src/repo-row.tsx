/**
 * One repository in a list.
 *
 * Shared by the index and by a user's page so the two cannot drift.
 *
 * A row in this list answers one question: which of these do I want to open.
 * Everything here earns its place against that, which is why the head commit
 * no longer gets a line of its own -- a hash you cannot click, a message about
 * work already done and an author who is usually you is a lot of height spent
 * on the one thing you are not choosing between. What tells two repositories
 * apart is what they are for, whether they are somebody else's work you took a
 * copy of, and whether anything is waiting on you.
 */
import { css } from "@toyz/loom";
import "./components/fkit-avatar";
import { relativeTime, type Repo } from "./api";
import { linkHandler } from "./nav";

export const repoRowSheet = css`
  /* Two lines and one grid: line two starts in the same column as the name, so
     the list reads as a table rather than as text with a hanging indent. */
  .rr {
    display: grid;
    grid-template-columns: 22px minmax(0, 1fr) auto;
    column-gap: 10px;
    row-gap: 1px;
    padding: 7px 12px;
    border-bottom: 1px solid var(--border);
    color: inherit;
  }
  .rr:last-child { border-bottom: 0; }
  .rr:hover { background: var(--raised); text-decoration: none; }

  /* The tile spans both lines, so it reads as belonging to the row rather
     than to the name. Every repository has a colour of its own here for the
     same reason it has one everywhere else on the site: a list of twenty is a
     grey wall otherwise, and the colour is the fastest way back to the one you
     were in yesterday. The glyph still carries whether it is private, because
     a hue is not something to make anyone read a lock out of. */
  .rr .ic {
    grid-row: 1 / 3; grid-column: 1;
    display: flex; align-items: center; justify-content: center;
  }

  .rr .top {
    grid-row: 1; grid-column: 2;
    display: flex; align-items: baseline; gap: 8px; min-width: 0;
  }
  .rr .nm { white-space: nowrap; font-size: 13px; }
  .rr .own { color: var(--muted); }
  .rr .rep { color: var(--accent); }

  /* Private is a state worth reading and not worth shouting: no border, no
     box, no capitals. Set in caps beside a lowercase name it was the loudest
     thing on the row. */
  .rr .priv { font-size: 11px; color: var(--faint); flex: none; }

  /* Where a copy came from. Five forks of one project, which is what this list
     actually holds, are otherwise five rows that look like a mistake. */
  .rr .from {
    display: flex; align-items: center; gap: 4px;
    font-size: 11px; color: var(--faint); min-width: 0; flex: 0 1 auto;
  }
  .rr .from span {
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .rr .from loom-icon { flex: none; opacity: .7; }

  /* One right-hand column holding one kind of fact, so it aligns the whole way
     down. A repository with nothing pushed to it reports when it was made
     rather than dropping out of the column and leaving a hole. */
  .rr .when {
    grid-row: 1; grid-column: 3;
    color: var(--faint); font-size: 11px; white-space: nowrap;
    align-self: baseline;
  }

  .rr .sub {
    grid-row: 2; grid-column: 2;
    font-family: var(--sans); color: var(--muted); font-size: 11.5px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0;
  }
  /* What a repository with no description says instead: the last thing that
     happened to it, or that nothing has. Line two is never blank -- a row that
     is sometimes one line tall and sometimes two is what made the list look
     ragged more than anything in it did. */
  .rr .sub.quiet { color: var(--faint); font-style: italic; }

  /* Counts, and only the ones that are true: a default branch on its own is
     not news, and a nought is worse than nothing. Ordered by what wants doing
     first -- merges and issues are somebody waiting, branches and tags are
     shape. */
  .rr .counts {
    grid-row: 2; grid-column: 3;
    display: flex; align-items: center; gap: 10px;
    font-size: 11px; color: var(--faint); white-space: nowrap;
    align-self: center;
  }
  .rr .ct { display: flex; align-items: center; gap: 3px; }
  .rr .ct b { font-weight: 400; font-variant-numeric: tabular-nums; }
  .rr .ct loom-icon { opacity: .75; }
  .rr:hover .ct.live { color: var(--accent); }

  @media (max-width: 700px) {
    .rr .from, .rr .counts { display: none; }
  }
`;

export interface RepoRowOptions {
  /** Show `owner/name` rather than just `name`. False on a user's own page. */
  withOwner?: boolean;
}

export function repoRow(r: Repo, opts: RepoRowOptions = {}) {
  const href = `/${r.owner}/${r.name}`;
  const priv = r.visibility === "private";

  /* Merges and issues first: those are somebody waiting on an answer, and the
     hover tint says so. Branches and tags describe the shape of the history,
     which is worth knowing and not worth acting on. */
  const counts: [string, number, boolean][] = [
    ["merge", r.open_merges, true],
    ["alert", r.open_issues, true],
    ["branch", r.branches > 1 ? r.branches : 0, false],
    ["tag", r.tags, false],
  ];

  return (
    <a class="rr" loom-key={`${r.owner}/${r.name}`} href={href} onClick={linkHandler(href)}>
      <span class="ic" title={r.visibility}>
        <fkit-avatar
          name={r.full_name}
          glyph={priv ? "lock" : "repo"}
          size={22}
        ></fkit-avatar>
      </span>

      <span class="top">
        <span class="nm">
          {opts.withOwner ? <span class="own">{r.owner}/</span> : null}
          <span class="rep">{r.name}</span>
        </span>
        {priv ? <span class="priv">private</span> : null}
        {r.forked_from ? (
          /* Plain text, not a link: this is already inside the row's link, and
             a link inside a link is neither valid nor clickable. */
          <span class="from" title={`forked from ${r.forked_from}`}>
            <loom-icon name="merge" size={11}></loom-icon>
            <span>{r.forked_from}</span>
          </span>
        ) : null}
      </span>

      <span class="when">
        {r.head ? relativeTime(r.updated_at) : `made ${relativeTime(r.created_at)}`}
      </span>

      <span class={r.description ? "sub" : "sub quiet"}>
        {r.description
          ? r.description
          : r.head
            ? r.head.summary
            : "nothing pushed yet"}
      </span>

      <span class="counts">
        {counts
          .filter(([, n]) => n > 0)
          .map(([icon, n, live]) => (
            <span class={live ? "ct live" : "ct"} loom-key={icon}>
              <loom-icon name={icon} size={11}></loom-icon>
              <b>{n}</b>
            </span>
          ))}
      </span>
    </a>
  );
}
