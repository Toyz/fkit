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
import {
  api,
  relativeTime,
  type Activity,
  type MyStash,
  type Profile,
  type Push,
  type Repo,
} from "../api";
import { linkHandler } from "../nav";
import { repoRow, repoRowSheet } from "../repo-row";
import { Session } from "../session";
import { confirmAction } from "../components/fkit-dialog";
import "../components/fkit-tabs";
import "../components/fkit-activity";

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

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** A day, in UTC, written for a person.
 *
 *  UTC because the grid above counts days in UTC, and two views of the same
 *  commit disagreeing about which day it was is the kind of thing that makes
 *  a page look like it is guessing. Local time reads more naturally and is not
 *  worth having the feed say Thursday while the square above it says Friday.
 */
function utcDay(iso: string): string {
  const d = new Date(iso);
  return `${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`;
}

/** Commits bucketed by the day they say they were written. */
function byDay(list: Push[]): { day: string; items: Push[] }[] {
  const out: { day: string; items: Push[] }[] = [];
  for (const c of list) {
    const day = utcDay(c.committed_at);
    const last = out[out.length - 1];
    if (last && last.day === day) last.items.push(c);
    else out.push({ day, items: [c] });
  }
  return out;
}

/** Did these two happen on the same day, counted the way the grid counts? */
function sameDay(a: string, b: string): boolean {
  return utcDay(a) === utcDay(b);
}

/** `Travis <t@e.com>` reads as a name in a list; the address does not. */
function authorName(author: string): string {
  const lt = author.indexOf("<");
  return (lt === -1 ? author : author.slice(0, lt)).trim() || author;
}

/** Stashes bucketed by the repository they belong to, order preserved.
 *
 *  A run rather than a map: the list arrives newest first, so walking it and
 *  starting a bucket when the repository changes keeps the most recent work at
 *  the top — which is the order somebody looking for what they were doing
 *  yesterday actually wants. Grouping properly would sort by project name and
 *  bury it.
 */
function byRepo(list: MyStash[]): { slug: string; items: MyStash[] }[] {
  const out: { slug: string; items: MyStash[] }[] = [];
  for (const st of list) {
    const slug = `${st.owner}/${st.repo}`;
    const last = out[out.length - 1];
    if (last && last.slug === slug) last.items.push(st);
    else out.push({ slug, items: [st] });
  }
  return out;
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
  /* The identity, laid out down its column rather than across a page: the
     tile and the name share the top line, and everything true of the person
     hangs under them. */
  .band { display: block; margin-bottom: 30px; }
  .band .who { min-width: 0; }
  .band fkit-avatar { float: left; margin: 2px 12px 0 0; }

  /* Who they are, and what they have been doing, as one band with two halves.
   *
   * The year of pushes was sitting loose between the band and the tabs: the
   * largest and only coloured thing on the page, wearing no frame, between two
   * things that had one. It is not a section of the page -- nothing switches to
   * it and nothing else belongs with it -- it is the second half of the answer
   * to who this is, so it goes where that answer already is.
   *
   * The hairline is the whole reason this is not the sidebar that was tried
   * here before and taken out again. That one had blocks stacked in a column
   * aligned to nothing, reading as two unrelated halves of a page; this shares
   * one top line, one baseline and one rule, which is what makes two columns
   * read as one band.
   */
  .id {
    display: grid;
    /* A fixed column, not an automatic one. Left to size itself the identity
       took the width of its longest sentence -- some seven hundred pixels of
       "joined, 21 repositories, last push, hash" on one line -- which pushed
       the grid into a scroller and hid the most recent weeks, the ones anybody
       actually came to look at. Pinned, the sentence wraps into a paragraph
       the shape of the column it is in, and the year gets the rest. */
    grid-template-columns: 320px minmax(0, 1fr);
    gap: 0 30px;
    align-items: start;
    margin-bottom: 26px;
  }
  .id .band { margin-bottom: 0; }
  .id .year {
    border-left: 1px solid var(--border);
    padding-left: 30px; min-width: 0;
  }
  /* Nothing pushed yet, so there is no second half and the first should not
     be squeezed into a column two thirds of the way across the page. */
  .id.solo { grid-template-columns: minmax(0, 1fr); }

  /* Under about this width the grid cannot sit beside anything without one of
     them being unreadable, so the band stacks and the rule moves with it. */
  @media (max-width: 1080px) {
    .id { grid-template-columns: minmax(0, 1fr); gap: 22px 0; }
    .id .year {
      border-left: 0; padding-left: 0;
      border-top: 1px solid var(--border); padding-top: 20px;
    }
  }

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

  /* One row per fact, label left and value right, aligned down the column. */
  .facts {
    display: grid; grid-template-columns: auto minmax(0, 1fr);
    gap: 3px 12px; margin: 14px 0 0; clear: both;
    font-size: 11.5px;
  }
  .facts dt {
    color: var(--faint); white-space: nowrap;
    font-size: 10px; text-transform: uppercase; letter-spacing: .08em;
    align-self: baseline; padding-top: 1px;
  }
  .facts dd { margin: 0; color: var(--muted); min-width: 0; }
  .facts .q { color: var(--faint); }
  .facts .at {
    font-family: var(--mono); color: var(--accent);
    text-decoration: none; margin-left: 7px;
  }
  .facts .at:hover { text-decoration: underline; }

  /* What they work on, from their own repositories rather than from a bio
     nobody fills in. */
  .works {
    display: flex; flex-wrap: wrap; align-items: center; gap: 5px;
    margin-top: 14px; clear: both;
  }
  .works .lbl {
    font-size: 10px; text-transform: uppercase; letter-spacing: .09em;
    color: var(--faint); margin-right: 3px;
  }
  .works .chip {
    font-size: 11px; padding: 1px 8px; line-height: 18px;
    border: 1px solid var(--border-hi); border-radius: var(--radius-pill);
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


  /* A group header inside the stash list. Slotted into fkit-list, so it is
     the page's to style — the same bar the commit history puts between days,
     because it is doing the same job. */
  .grp {
    display: flex; align-items: center; gap: 8px;
    padding: 7px 14px;
    background: var(--raised);
    border-bottom: 1px solid var(--border);
    font-size: 11.5px;
  }
  .grp + .grp, fkit-row + .grp { border-top: 1px solid var(--border); }
  .grp a { color: var(--text); text-decoration: none; }
  .grp a:hover { color: var(--accent); text-decoration: underline; }
  .grp .n {
    margin-left: auto; color: var(--faint); font-variant-numeric: tabular-nums;
  }


  /* A commit that sat somewhere before it was sent. Quiet: it is a footnote on
     the row, not a warning about it. */
  .late { font-size: 11px; color: var(--faint); white-space: nowrap; }
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

  /**
   * Which view is showing. Read from the query string rather than a path
   * segment, because `/{owner}/stashes` is already how a repository called
   * "stashes" is spelled.
   *
   * Parked work is behind a click on purpose: it should not render just
   * because you opened your own profile, which is a thing people do while
   * somebody else is looking at the screen.
   */
  @reactive accessor tab: "repos" | "activity" | "stashes" = "repos";
  @reactive accessor stashes: MyStash[] | null = null;
  @reactive accessor activity: Activity | null = null;
  @reactive accessor pushes: Push[] | null = null;
  @reactive accessor busy = false;
  /** A fetch is already out; a second caller should not start another. */
  private loading = false;

  @mount
  readTab() {
    const sync = () => {
      const want = new URLSearchParams(location.search).get("tab");
      this.tab = want === "stashes" ? "stashes" : want === "activity" ? "activity" : "repos";
      this.maybeLoadStashes();
      // Behind its own click. Reading twenty commit objects is not work worth
      // doing for the many more people who came to look at the repositories.
      if (this.tab === "activity" && this.pushes === null) void this.loadPushes();
    };
    sync();
    addEventListener("popstate", sync);
    return () => removeEventListener("popstate", sync);
  }

  /**
   * Fetch the list once we know it is ours to fetch.
   *
   * Called from everywhere that could be the moment we find out: the tab, the
   * session arriving, the profile arriving. Whichever is last wins and the
   * other two are a no-op, which is cheaper than working out an order.
   *
   * The tab wears the count, so waiting for the tab to be opened before asking
   * meant the count only appeared after you had already gone looking -- which
   * is exactly when a number telling you there is something to look at is of
   * no use to anybody.
   */
  private maybeLoadStashes() {
    if (this.stashes !== null || this.loading) return;
    if (!this.me || this.me !== this.profile?.username) return;
    this.loading = true;
    void this.loadStashes();
  }

  private async loadPushes() {
    const who = this.profile?.username ?? location.pathname.split("/").filter(Boolean)[0] ?? "";
    try {
      this.pushes = await api.pushes(who);
    } catch {
      this.pushes = [];
    }
  }

  private async loadStashes() {
    try {
      this.stashes = await api.myStashes();
    } catch {
      // Not fatal: the rest of the profile is still worth showing, and an
      // empty list reads the same as a failed one here.
      this.stashes = [];
    } finally {
      this.loading = false;
    }
  }

  @mount
  watchUser() {
    const take = (u: { username: string } | null | undefined) => {
      this.me = u?.username ?? null;
      this.maybeLoadStashes();
    };
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
    this.stashes = null;
    this.activity = null;
    this.pushes = null;
    // Its own request: a year of pushes is a different question from a list of
    // repositories, it is the slower of the two, and a profile that will not
    // render until both have landed is a worse profile than one that fills in.
    api
      .activity(who)
      .then((a) => (this.activity = a))
      .catch(() => (this.activity = null));
    api
      .profile(who)
      .then((p) => {
        this.profile = p;
        this.maybeLoadStashes();
      })
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
        <div class={`id${this.activity && this.activity.total > 0 ? "" : " solo"}`}>
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

                {/* Labelled rows rather than one sentence.
                    As a sentence these ran to about seven hundred pixels, which
                    is fine across a page and unreadable down a column -- it
                    wrapped wherever it ran out of room and stranded its own
                    separators at the start of lines. Labelled, each fact ends
                    where it ends, they align with each other, and the column
                    fills to something near the height of the grid beside it. */}
                <dl class="facts">
                  <dt>joined</dt>
                  <dd>{relativeTime(p.created_at)}</dd>

                  <dt>repositories</dt>
                  <dd>
                    {p.repos.length}
                    {priv && mine ? <span class="q"> · {priv} private</span> : null}
                  </dd>

                  {latest ? (
                    <>
                      <dt>last push</dt>
                      <dd>
                        {relativeTime(latest.updated_at)}
                        {/* The tip of that push. A hash is the only thing here
                            that means exactly one state of exactly one tree, so
                            it is the honest answer to what someone is on. */}
                        {latest.head ? (
                          <a
                            class="at"
                            href={`/${latest.owner}/${latest.name}/commit/${latest.head.commit}`}
                            onClick={linkHandler(
                              `/${latest.owner}/${latest.name}/commit/${latest.head.commit}`,
                            )}
                          >
                            {latest.head.short}
                          </a>
                        ) : null}
                      </dd>
                    </>
                  ) : null}

                  {this.activity && this.activity.total > 0 ? (
                    <>
                      <dt>pushed</dt>
                      <dd>{this.activity.total} commits this year</dd>
                    </>
                  ) : null}
                </dl>

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

          {/* The band's other half. Not gated on the tab: this is part of who
              the page is about, so it does not come and go as you switch
              between what they own and what they parked. Absent only when
              there is nothing to draw -- a new account should not be met with
              a year of empty squares as its welcome. */}
          {this.activity && this.activity.total > 0 ? (
            <div class="year">
              <fkit-activity
                data={this.activity}
                who={p?.username ?? ""}
              ></fkit-activity>
            </div>
          ) : null}
        </div>

        {/* Shown to everybody now, not only to the account itself: activity is
            a public view of a public person, filtered by what the viewer may
            see like everything else. Only stashes are yours alone. */}
        {p ? (
          <fkit-tabs
            current={this.tab}
            tabs={[
              { key: "repos", label: "repositories", icon: "repo", href: `/${p.username}`,
                count: p.repos.length },
              { key: "activity", label: "activity", icon: "history",
                href: `/${p.username}?tab=activity` },
              ...(mine
                ? [
                    {
                      key: "stashes",
                      label: "stashes",
                      icon: "archive" as const,
                      href: `/${p.username}?tab=stashes`,
                      count: this.stashes?.length,
                    },
                  ]
                : []),
            ]}
          ></fkit-tabs>
        ) : null}

        {mine && this.tab === "stashes" ? this.renderStashes() : this.tab === "activity" ? (
          this.renderPushes()
        ) : (
        <fkit-section
          heading={mine ? "" : "Repositories"}
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
              {mine && this.session.canCreateRepo ? (
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
                {!q && mine && this.session.canCreateRepo ? (
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
        )}
      </div>
    );
  }

  /**
   * Parked work, across every repository.
   *
   * Across, because "where did I leave that" is not a question you can ask a
   * repository — the answer is that you cannot remember which one. Each row
   * links into the ordinary commit page, which renders a stash correctly with
   * no special handling: a stash commit's first parent is the tree it was
   * taken from, and a commit page diffs a commit against its first parent.
   */
  /** What this person has actually been doing, newest first. */
  private renderPushes() {
    const list = this.pushes;
    return (
      <fkit-section
        value={list === null ? "" : list.length ? `last ${list.length}` : ""}
        blurb="Commits this account delivered, newest first. Work in repositories you cannot see is counted in the graph above but not listed here — there is no way to show what a commit was without showing it."
      >
        <fkit-list>
          {list === null ? (
            <fkit-empty><span class="sk" style="width:260px"></span></fkit-empty>
          ) : list.length === 0 ? (
            <fkit-empty>
              Nothing yet. Commits are linked to an account by the push that
              delivered them, so anything pushed before that existed — or with a
              token that declines to attribute — is not counted.
            </fkit-empty>
          ) : (
            byDay(list).flatMap((g) => [
              <div class="grp" loom-key={`h:${g.day}`}>
                <loom-icon name="commit" size={12}></loom-icon>
                {g.day}
                <span class="n">{g.items.length}</span>
              </div>,
              ...g.items.map((c) => {
                const href = `/${c.repo}/commit/${c.commit}`;
                return (
                  <fkit-row
                    loom-key={c.commit}
                    href={href}
                    name={c.summary}
                    meta={`${c.repo} · ${authorName(c.author)}`}
                  >
                    <fkit-avatar
                      slot="icon"
                      name={c.repo}
                      glyph="commit"
                      size={22}
                    ></fkit-avatar>
                    {/* When it arrived, but only when that is not the same day
                        it was written. A commit pushed the moment it was made
                        needs no note; one that sat on a laptop for a week is
                        the case this column exists for. */}
                    {sameDay(c.committed_at, c.pushed_at) ? null : (
                      <span class="late" title={`pushed ${relativeTime(c.pushed_at)}`}>
                        pushed {relativeTime(c.pushed_at)}
                      </span>
                    )}
                    <span class="sha">{c.short}</span>
                  </fkit-row>
                );
              }),
            ])
          )}
        </fkit-list>
      </fkit-section>
    );
  }

  private renderStashes() {
    const list = this.stashes;
    return (
      <fkit-section
        value={list === null ? "" : `${list.length} parked`}
        blurb="Work you set aside and sent to a server so it follows you between machines. Only you can see these, including administrators. They expire on their own."
      >
        <fkit-list>
          {list === null ? (
            <fkit-empty><span class="sk" style="width:220px"></span></fkit-empty>
          ) : list.length === 0 ? (
            <fkit-empty>
              Nothing parked. `fkit stash push` sends one here from a working copy.
            </fkit-empty>
          ) : (
            byRepo(list).flatMap((g) => [
              // A header, then what is parked under it — the shape the commit
              // history already uses for days. A stash only means anything
              // against the repository it was taken from, so that is the thing
              // worth naming once rather than repeating down every row.
              <div class="grp" loom-key={`h:${g.slug}`}>
                <fkit-avatar name={g.slug} glyph="repo" size={14}></fkit-avatar>
                <a href={`/${g.slug}`} onClick={linkHandler(`/${g.slug}`)}>
                  {g.slug}
                </a>
                <span class="n">{g.items.length}</span>
              </div>,
              ...g.items.map((st) => {
              const href = `/${st.owner}/${st.repo}/commit/${st.commit_hash}`;
              return (
                <fkit-row
                  loom-key={st.id}
                  href={href}
                  name={st.message}
                  meta={`${relativeTime(st.created_at)} · expires ${relativeTime(
                    st.expires_at,
                  )}`}
                >
                  {/* Keyed by the commit, not the repository: the repository is
                      already named in the header above, and this way two
                      stashes on one project are still told apart at a glance. */}
                  <fkit-avatar
                    slot="icon"
                    name={st.commit_hash}
                    glyph="archive"
                    size={22}
                  ></fkit-avatar>
                  <span class="sha">{st.commit_hash.slice(0, 10)}</span>
                  <button
                    class="revoke"
                    disabled={this.busy}
                    onClick={async () => {
                      const ok = await confirmAction({
                        title: `Drop this stash?`,
                        effects: [
                          { text: `Removed from ${st.owner}/${st.repo} on this server` },
                          { text: "Any copy on one of your machines is untouched", tone: "safe" },
                        ],
                        confirm: "drop stash",
                        danger: true,
                      });
                      if (!ok) return;
                      this.busy = true;
                      try {
                        await api.dropStash(st.owner, st.repo, st.id);
                        await this.loadStashes();
                      } finally {
                        this.busy = false;
                      }
                    }}
                  >
                    drop
                  </button>
                </fkit-row>
              );
            }),
            ])
          )}
        </fkit-list>
      </fkit-section>
    );
  }
}
