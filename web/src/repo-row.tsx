/**
 * One repository in a list.
 *
 * Shared by the index and by a user's page so the two cannot drift. It was
 * previously a single line carrying a name, the default branch, and a
 * timestamp — and since almost every repository's default branch is `main`,
 * two thirds of that said nothing. A list is worth its height only if a row
 * answers "what is this, and what happened to it last".
 */
import { css } from "@toyz/loom";
import { relativeTime, type Repo } from "./api";
import { linkHandler } from "./nav";

export const repoRowSheet = css`
  .rr {
    display: grid;
    grid-template-columns: 16px minmax(0, 1fr) auto;
    grid-template-rows: auto auto;
    align-items: center;
    column-gap: 10px;
    row-gap: 3px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
    color: inherit;
  }
  .rr:last-child { border-bottom: 0; }
  .rr:hover { background: var(--raised); text-decoration: none; }

  .rr .ic { grid-row: 1; color: var(--faint); display: flex; }
  .rr .top {
    grid-row: 1; grid-column: 2;
    display: flex; align-items: center; gap: 9px; min-width: 0;
  }
  .rr .own { color: var(--muted); }
  .rr .rep { color: var(--accent); }
  .rr .nm { white-space: nowrap; }
  .rr .ds {
    font-family: var(--sans); color: var(--muted); font-size: 12px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0;
  }
  .rr .right {
    grid-row: 1; grid-column: 3;
    display: flex; align-items: center; gap: 9px;
    color: var(--faint); font-size: 11px; white-space: nowrap;
  }

  /* The second line is the point of the row: what this repository last did. */
  .rr .last {
    grid-row: 2; grid-column: 2 / 4;
    display: flex; align-items: baseline; gap: 8px;
    min-width: 0; font-size: 11.5px; color: var(--faint);
  }
  .rr .sha { color: var(--muted); font-variant-numeric: tabular-nums; flex: none; }
  .rr .msg {
    color: var(--muted); font-family: var(--sans);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0;
  }
  .rr .by { flex: none; }
  .rr .none { font-family: var(--sans); font-style: italic; }

  /* Private is a state worth reading, not only an icon to decode. */
  .rr .tag.priv { color: var(--faint); }

  @media (max-width: 700px) {
    .rr .ds, .rr .by { display: none; }
  }
`;

export interface RepoRowOptions {
  /** Show `owner/name` rather than just `name`. False on a user's own page. */
  withOwner?: boolean;
}

export function repoRow(r: Repo, opts: RepoRowOptions = {}) {
  const href = `/${r.owner}/${r.name}`;
  const priv = r.visibility === "private";
  return (
    <a class="rr" href={href} onClick={linkHandler(href)}>
      <span class="ic" title={r.visibility}>
        <loom-icon name={priv ? "lock" : "repo"} size={13}></loom-icon>
      </span>

      <span class="top">
        <span class="nm">
          {opts.withOwner ? <span class="own">{r.owner}/</span> : null}
          <span class="rep">{r.name}</span>
        </span>
        {priv ? <span class="tag priv">private</span> : null}
        {r.description ? <span class="ds">{r.description}</span> : null}
      </span>

      <span class="right">
        {r.branches > 1 ? <span class="tag">{r.branches} branches</span> : null}
        <span>{relativeTime(r.updated_at)}</span>
      </span>

      <span class="last">
        {r.head ? (
          <>
            <span class="sha">{r.head.short}</span>
            <span class="msg">{r.head.summary}</span>
            <span class="by">
              {authorName(r.head.author)} · {relativeTime(r.head.timestamp)}
            </span>
          </>
        ) : (
          <span class="none">no commits yet</span>
        )}
      </span>
    </a>
  );
}

/** `Travis <t@e.com>` reads as a name in a list; the address does not. */
function authorName(author: string): string {
  const lt = author.indexOf("<");
  return (lt === -1 ? author : author.slice(0, lt)).trim() || author;
}
