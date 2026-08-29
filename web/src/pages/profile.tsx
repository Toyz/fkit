/**
 * A person's page: `/travis`.
 *
 * The account menu has always pointed here, but nothing was registered for a
 * single path segment, so the click fell through to the catch-all repository
 * page and did nothing. This is that page.
 *
 * It deliberately reuses the repository index's row vocabulary — icon, name,
 * branch, time — because it is the same list filtered to one owner, and a
 * second set of columns for the same data would be one to learn for no reason.
 *
 * Registered last among the fixed-arity routes on purpose: `/:owner` would
 * otherwise swallow `/settings` and `/admin`. Those names are reserved
 * usernames as well, so the two defences agree.
 */
import { LoomElement, component, css, styles, reactive, mount, on, inject } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { api, relativeTime, type Profile, type Repo } from "../api";
import { linkHandler } from "../nav";
import { repoRow, repoRowSheet } from "../repo-row";
import { Session } from "../session";

/**
 * The topics this person's repositories carry, most-used first.
 *
 * Ordering by count rather than alphabetically is what makes the list mean
 * something: the first chip is what they actually spend their time on.
 * Capped, because a sidebar is not a tag cloud.
 */
function rankTopics(repos: Repo[], cap = 8): string[] {
  const seen = new Map<string, number>();
  for (const r of repos) for (const t of r.topics ?? []) seen.set(t, (seen.get(t) ?? 0) + 1);
  return [...seen.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, cap)
    .map(([t]) => t);
}

const sheet = css`
  /* An identity band, not a sidebar.
   *
   * The column version left the avatar floating above a stack of unrelated
   * blocks, aligned to nothing and sharing no line with the list beside it.
   * Here the avatar anchors the text it belongs to, and the band closes with
   * the same rule every heading in the app draws — so the page reads top to
   * bottom like the rest of it instead of as two unrelated halves.
   */
  .band {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: 0 18px;
    align-items: center;
    margin-bottom: 30px;
  }
  .band .who { min-width: 0; }

  .nm {
    display: flex; align-items: center; flex-wrap: wrap; gap: 9px;
  }
  .nm h1 {
    font-size: 21px; font-weight: 500; letter-spacing: -0.015em;
    margin: 0; overflow-wrap: anywhere;
  }
  .dn {
    font-family: var(--sans); color: var(--muted);
    font-size: 13px; margin-top: 3px;
  }

  /* The facts read as a sentence about the person, because that is what they
     are — a stacked table of four rows was three more lines than they need. */
  .facts {
    display: flex; flex-wrap: wrap; align-items: center; gap: 4px 9px;
    margin-top: 9px; font-size: 11.5px; color: var(--faint);
  }
  .facts .sep { opacity: .45; }
  .facts b { font-weight: 400; color: var(--muted); }
  .facts .at { font-family: var(--mono); color: var(--accent); }

  /* What they work on, from their own repositories rather than from a bio
     nobody fills in. */
  .works { display: flex; flex-wrap: wrap; align-items: center; gap: 5px; margin-top: 11px; }
  .works .lbl {
    font-size: 10px; text-transform: uppercase; letter-spacing: .09em;
    color: var(--faint); margin-right: 3px;
  }
  .works .chip {
    font-size: 11px; padding: 1px 8px; line-height: 18px;
    border: 1px solid var(--border-hi); border-radius: 999px;
    color: var(--muted); background: var(--raised);
  }

  .filter { max-width: 180px; font-size: 12px; height: 24px; padding: 0 8px; }

  .empty { padding: 34px 14px; text-align: center; }
  .empty h2 { color: var(--muted); }
  .empty .prose {
    font-family: var(--sans); color: var(--faint); font-size: 12.5px;
    margin: 7px auto 0; max-width: 44ch; line-height: 1.5;
  }
  .empty .btn { margin-top: 14px; }

`;

@route("/:owner")
@component("page-profile")
@styles(base, repoRowSheet, sheet)
export class PageProfile extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor profile: Profile | null = null;
  @reactive accessor error = "";
  @reactive accessor filter = "";
  /**
   * A local copy of the signed-in user.
   *
   * `session.user` is a store, and reading `.value` inside `update()` does not
   * subscribe — so a page that renders before `/api/me` resolves keeps its
   * first answer forever. That is why your own profile showed no "you" tag and
   * no way to create a repository.
   */
  @reactive accessor me: string | null = null;

  @mount
  watchUser() {
    const take = (u: { username: string } | null | undefined) => (this.me = u?.username ?? null);
    take(this.session.current);
    return this.session.user.subscribe(take);
  }

  @mount
  init() {
    this.load();
  }

  @on(window, "popstate")
  private load() {
    const who = location.pathname.split("/").filter(Boolean)[0] ?? "";
    this.profile = null;
    this.error = "";
    this.filter = "";
    api
      .profile(who)
      .then((p) => (this.profile = p))
      .catch((e) => (this.error = (e as Error).message));
  }

  update() {
    if (this.error) {
      return (
        <div class="wrap">
          <div class="panel">
            <div class="empty">
              <h2>no such user</h2>
              <p class="prose">{this.error}</p>
              <a class="btn" href="/" onClick={linkHandler("/")}>all repositories</a>
            </div>
          </div>
        </div>
      );
    }

    const p = this.profile;
    const mine = !!p && this.me === p.username;
    const q = this.filter.trim().toLowerCase();
    const shown = p ? (q ? p.repos.filter((r) => r.name.toLowerCase().includes(q)) : p.repos) : [];
    // The filter is worth its own line only once scanning is actually work.
    const filterable = (p?.repos.length ?? 0) > 8;

    const pub = p?.repos.filter((r) => r.visibility === "public").length ?? 0;
    const priv = (p?.repos.length ?? 0) - pub;
    const topics = p ? rankTopics(p.repos) : [];
    // Only a repository that has a commit can have been pushed to. Ranking
    // every repository by updated_at made "last push" report the creation of
    // an empty one, and dropped the tip hash because that repository had no
    // tip to show.
    const latest = p?.repos
      .filter((r) => r.head)
      .reduce<Repo | null>((best, r) => (!best || r.updated_at > best.updated_at ? r : best), null);

    return (
      <div class="wrap">
        <div class="band">
          {p ? (
            <>
              <fkit-avatar name={p.username} size={58}></fkit-avatar>
              <div class="who">
                <div class="nm">
                  <h1>{p.username}</h1>
                  {mine ? <span class="tag on">you</span> : null}
                  {p.is_admin ? <span class="tag">administrator</span> : null}
                </div>
                {p.display_name ? <div class="dn">{p.display_name}</div> : null}

                {/* One line, because that is what these facts amount to. */}
                <div class="facts">
                  <span>joined <b>{relativeTime(p.created_at)}</b></span>
                  <span class="sep">·</span>
                  <span>
                    <b>{p.repos.length}</b>{" "}
                    {p.repos.length === 1 ? "repository" : "repositories"}
                  </span>
                  {latest ? (
                    <>
                      <span class="sep">·</span>
                      <span>last push <b>{relativeTime(latest.updated_at)}</b></span>
                    </>
                  ) : null}
                  {/* The tip of that push. A hash is the only thing here that
                      means exactly one state of exactly one tree, so it is the
                      honest answer to what someone is on right now. */}
                  {latest?.head ? (
                    <>
                      <span class="sep">·</span>
                      <span class="at">{latest.head.short}</span>
                    </>
                  ) : null}
                </div>

                {topics.length ? (
                  <div class="works">
                    <span class="lbl">works on</span>
                    {topics.map((t) => (
                      <span class="chip" loom-key={t}>{t}</span>
                    ))}
                  </div>
                ) : null}
              </div>
            </>
          ) : (
            <>
              <fkit-avatar size={58}></fkit-avatar>
              <div class="who">
                <span class="sk tall" style="width:150px"></span>
                <div class="facts"><span class="sk" style="width:230px"></span></div>
              </div>
            </>
          )}
        </div>

        <fkit-section
          heading="Repositories"
          value={
            p === null
              ? ""
              : mine || priv === 0
                ? `${pub} public${priv ? ` · ${priv} private` : ""}`
                : `${p.repos.length} you can see`
          }
        >
          {filterable || mine ? (
            <span slot="action" class="head-acts">
              {filterable ? (
                <input
                  class="filter"
                  placeholder="filter"
                  value={this.filter}
                  onInput={(e: Event) => (this.filter = (e.target as HTMLInputElement).value)}
                />
              ) : null}
              {mine ? (
                <a class="btn" href="/new" onClick={linkHandler("/new")}>
                  <loom-icon name="plus" size={11}></loom-icon> new repository
                </a>
              ) : null}
            </span>
          ) : null}

          <fkit-list>
            {p === null ? (
              [0, 1, 2].map(() => (
                <div class="r sk-row">
                  <span class="sk" style="width:13px;height:13px"></span>
                  <span class="name"><span class="sk tall" style="width:min(38%,200px)"></span></span>
                  <span class="sk" style="width:46px"></span>
                  <span class="sk" style="width:62px"></span>
                </div>
              ))
            ) : shown.length === 0 ? (
              <div class="empty">
                <h2>{q ? "no matches" : mine ? "no repositories yet" : "nothing to show"}</h2>
                <p class="prose">
                  {q
                    ? "No repository of theirs matches that filter."
                    : mine
                      ? "Create one, then point the CLI at it: fkit remote, fkit push."
                      : `${p.username} has no repositories you are allowed to see. Private ones are not listed, even by name.`}
                </p>
                {!q && mine ? (
                  <a class="btn primary" href="/new" onClick={linkHandler("/new")}>
                    <loom-icon name="plus" size={12}></loom-icon> new repository
                  </a>
                ) : null}
              </div>
            ) : (
              shown.map((r) => repoRow(r))
            )}
          </fkit-list>
        </fkit-section>
      </div>
    );
  }
}
