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
  /* An identity beside a body of work. The person is a column of facts that
     stays put while the list scrolls, which is what makes this page worth
     sharing: whoever opens the link can see who you are without scrolling
     back up out of your repositories.
   */
  .cols2 {
    display: grid;
    grid-template-columns: 250px minmax(0, 1fr);
    gap: 36px;
    align-items: start;
  }
  @media (max-width: 820px) {
    .cols2 { grid-template-columns: 1fr; gap: 22px; }
    .who { position: static; }
  }

  .who { position: sticky; top: 52px; }

  /* Initials, not an avatar service: no external request, no tracking, and it
     works on a server with no route to the internet. */
  .av {
    width: 84px; height: 84px; border-radius: var(--radius);
    background: var(--accent-weak); color: var(--accent);
    display: grid; place-items: center;
    font-size: 30px; font-weight: 600; text-transform: uppercase;
    letter-spacing: .02em;
    margin-bottom: 14px;
  }
  .who h1 {
    font-size: 19px; font-weight: 500; letter-spacing: -0.01em;
    margin: 0; overflow-wrap: anywhere;
  }
  .who .dn {
    font-family: var(--sans); color: var(--muted);
    font-size: 13px; margin-top: 3px;
  }
  .who .tags { display: flex; flex-wrap: wrap; gap: 5px; margin-top: 10px; }
  .who .btn { display: inline-flex; margin-top: 14px; }

  /* The facts, in the same shape the repository aside states its own. */
  .facts {
    display: grid; grid-template-columns: auto minmax(0, 1fr);
    gap: 5px 12px; margin: 16px 0 0;
    padding-top: 14px; border-top: 1px solid var(--border);
    font-size: 11.5px;
  }
  .facts dt { color: var(--faint); }
  .facts dd { margin: 0; color: var(--text); text-align: right; }
  .facts dd.mono { font-family: var(--mono); }

  /* What they actually work on, drawn from their own repositories rather than
     from a bio nobody fills in. */
  .works { margin-top: 16px; padding-top: 14px; border-top: 1px solid var(--border); }
  .works .lbl {
    font-size: 10px; text-transform: uppercase; letter-spacing: .09em;
    color: var(--faint); margin-bottom: 8px;
  }
  .works .chips { display: flex; flex-wrap: wrap; gap: 5px; }
  .works .chip {
    font-size: 11px; padding: 1px 8px; line-height: 18px;
    border: 1px solid var(--border-hi); border-radius: 999px;
    color: var(--muted); background: var(--raised);
  }

  .filter {
    max-width: 180px; font-size: 12px; height: 24px; padding: 0 8px;
  }

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
    // Their own topics, most used first — what they work on, taken from the
    // repositories themselves rather than from a bio nobody fills in.
    const topics = p ? rankTopics(p.repos) : [];
    const latest = p?.repos.reduce<Repo | null>(
      (best, r) => (!best || r.updated_at > best.updated_at ? r : best),
      null,
    );

    return (
      <div class="wrap">
        <div class="cols2">
          <div class="who">
            {p ? (
              <>
                <div class="av">{p.username.slice(0, 2)}</div>
                <h1>{p.username}</h1>
                {p.display_name ? <div class="dn">{p.display_name}</div> : null}

                {p.is_admin || mine ? (
                  <div class="tags">
                    {mine ? <span class="tag on">you</span> : null}
                    {p.is_admin ? <span class="tag">administrator</span> : null}
                  </div>
                ) : null}

                {mine ? (
                  <a class="btn" href="/new" onClick={linkHandler("/new")}>
                    <loom-icon name="plus" size={12}></loom-icon> new repository
                  </a>
                ) : null}

                <dl class="facts">
                  <dt>joined</dt>
                  <dd>{relativeTime(p.created_at)}</dd>
                  <dt>repositories</dt>
                  <dd>{p.repos.length}</dd>
                  {priv > 0 ? (
                    <>
                      <dt>private</dt>
                      <dd>{priv}</dd>
                    </>
                  ) : null}
                  {latest ? (
                    <>
                      <dt>last push</dt>
                      <dd>{relativeTime(latest.updated_at)}</dd>
                    </>
                  ) : null}
                  {/* The tip of their most recent push. A hash is the only
                      thing in this program that means exactly one state of
                      exactly one tree, so it is the honest answer to "what
                      are they on right now". */}
                  {latest?.head ? (
                    <>
                      <dt>at</dt>
                      <dd class="mono">{latest.head.short}</dd>
                    </>
                  ) : null}
                </dl>

                {topics.length ? (
                  <div class="works">
                    <div class="lbl">works on</div>
                    <div class="chips">
                      {topics.map((t) => (
                        <span class="chip" loom-key={t}>{t}</span>
                      ))}
                    </div>
                  </div>
                ) : null}
              </>
            ) : (
              <>
                <div class="av"></div>
                <span class="sk tall" style="width:130px"></span>
                <dl class="facts">
                  <dt>joined</dt>
                  <dd><span class="sk" style="width:60px"></span></dd>
                  <dt>repositories</dt>
                  <dd><span class="sk" style="width:18px"></span></dd>
                </dl>
              </>
            )}
          </div>

          <div>
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
              {filterable ? (
                <input
                  slot="action"
                  class="filter"
                  placeholder="filter"
                  value={this.filter}
                  onInput={(e: Event) => (this.filter = (e.target as HTMLInputElement).value)}
                />
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
        </div>
      </div>
    );
  }
}
