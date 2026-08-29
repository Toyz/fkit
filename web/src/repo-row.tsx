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
import "./components/fkit-avatar";
import { relativeTime, type Repo } from "./api";
import { linkHandler } from "./nav";

export const repoRowSheet = css`
  /* Two lines, but one grid: the second line starts in the same column as the
     name, so the rows read as a table rather than as text with a hanging
     indent. Everything secondary is one weight and one colour — the earlier
     version mixed a bordered tag, two sizes of grey and two fonts on the same
     row, which is what made it look busy. */
  .rr {
    display: grid;
    grid-template-columns: 20px minmax(0, 1fr) auto;
    column-gap: 10px;
    row-gap: 2px;
    padding: 9px 12px;
    border-bottom: 1px solid var(--border);
    color: inherit;
  }
  .rr:last-child { border-bottom: 0; }
  .rr:hover { background: var(--raised); text-decoration: none; }

  .rr .ic { grid-row: 1; color: var(--faint); display: flex; align-items: center; }
  .rr .ic fkit-avatar { margin-left: -1px; }
  .rr .top {
    grid-row: 1; grid-column: 2;
    display: flex; align-items: baseline; gap: 8px; min-width: 0;
  }
  .rr .own { color: var(--muted); }
  .rr .rep { color: var(--accent); }
  .rr .nm { white-space: nowrap; font-size: 13px; }
  .rr .ds {
    font-family: var(--sans); color: var(--muted); font-size: 12px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0;
  }

  /* One right-hand column, one size, aligned down the list. */
  .rr .when {
    grid-row: 1; grid-column: 3;
    color: var(--faint); font-size: 11px; white-space: nowrap;
    align-self: baseline;
  }

  .rr .last {
    grid-row: 2; grid-column: 2 / 4;
    display: flex; align-items: baseline; gap: 8px;
    min-width: 0; font-size: 11.5px; color: var(--faint);
  }
  .rr .sha { font-variant-numeric: tabular-nums; flex: none; }
  .rr .msg {
    font-family: var(--sans);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0;
  }
  .rr .meta { flex: none; margin-left: auto; padding-left: 12px; }

  /* Private is a state worth reading, but it should not shout: no border, no
     box, and not capitals either — set in caps beside a lowercase name it was
     the loudest thing on the row. */
  .rr .priv { font-size: 11px; color: var(--faint); flex: none; }

  /* A repository with no commits has nothing to say on a second line, so it
     does not get one — five of those in a row was half a screen of "no
     commits yet" under five names. It is said once, in the column that would
     otherwise hold a timestamp that means only "when this was created". */
  .rr .none { font-style: italic; font-family: var(--sans); }

  @media (max-width: 700px) {
    .rr .ds, .rr .meta { display: none; }
  }
`;

export interface RepoRowOptions {
  /** Show `owner/name` rather than just `name`. False on a user's own page. */
  withOwner?: boolean;
  /**
   * Lead each row with the owner's tile instead of a repository glyph.
   *
   * Only worth it when the list actually spans owners. On a server with one
   * account it is the same square repeated down the page, which is strictly
   * worse than the glyph it replaced — that at least distinguishes a private
   * repository from a public one.
   */
  ownerTiles?: boolean;
}

/** Whether a list is worth leading with owner tiles: do its owners differ? */
export function spansOwners(repos: Repo[]): boolean {
  return new Set(repos.map((r) => r.owner)).size > 1;
}

export function repoRow(r: Repo, opts: RepoRowOptions = {}) {
  const href = `/${r.owner}/${r.name}`;
  const priv = r.visibility === "private";
  const counts = [
    r.branches > 1 ? `${r.branches} branches` : null,
    r.tags > 0 ? `${r.tags} ${r.tags === 1 ? "tag" : "tags"}` : null,
  ].filter(Boolean).join(" · ");

  return (
    <a class="rr" loom-key={`${r.owner}/${r.name}`} href={href} onClick={linkHandler(href)}>
      {opts.ownerTiles ? (
        <span class="ic" title={r.owner}>
          <fkit-avatar name={r.owner} size={19}></fkit-avatar>
        </span>
      ) : (
        <span class="ic" title={r.visibility}>
          <loom-icon name={priv ? "lock" : "repo"} size={13}></loom-icon>
        </span>
      )}

      <span class="top">
        <span class="nm">
          {opts.withOwner ? <span class="own">{r.owner}/</span> : null}
          <span class="rep">{r.name}</span>
        </span>
        {priv ? <span class="priv">private</span> : null}
        {r.description ? <span class="ds">{r.description}</span> : null}
      </span>

      <span class="when">
        {r.head ? relativeTime(r.updated_at) : <span class="none">no commits yet</span>}
      </span>

      {r.head ? (
        <span class="last">
          <span class="sha">{r.head.short}</span>
          <span class="msg">
            {r.head.summary} — {authorName(r.head.author)}
          </span>
          {counts ? <span class="meta">{counts}</span> : null}
        </span>
      ) : null}
    </a>
  );
}

/** `Travis <t@e.com>` reads as a name in a list; the address does not. */
function authorName(author: string): string {
  const lt = author.indexOf("<");
  return (lt === -1 ? author : author.slice(0, lt)).trim() || author;
}
