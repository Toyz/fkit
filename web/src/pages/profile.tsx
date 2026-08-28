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
import { api, relativeTime, type Profile } from "../api";
import { linkHandler } from "../nav";
import { repoRow, repoRowSheet } from "../repo-row";
import { Session } from "../session";

const sheet = css`
  /* One rhythm for the whole page: 16px between blocks, and the identity
     header sits flush with the page edge exactly as the index's title does. */
  .id {
    display: grid;
    grid-template-columns: 34px minmax(0, 1fr) auto;
    align-items: center;
    gap: 12px;
    padding-bottom: 16px;
    margin-bottom: 16px;
    border-bottom: 1px solid var(--border);
  }

  /* Initials, not an avatar service: no external request, no tracking, and it
     works on a server with no route to the internet. */
  .av {
    width: 34px; height: 34px; border-radius: var(--radius);
    background: var(--accent-weak); color: var(--accent);
    display: grid; place-items: center;
    font-size: 13px; font-weight: 600; text-transform: uppercase;
  }
  .nm { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .nm h1 { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sub { color: var(--faint); font-size: 11px; margin-top: 3px; }
  .sub .dn { color: var(--muted); font-family: var(--sans); }

  .filter { max-width: 190px; font-size: 12px; height: 24px; padding: 0 8px; }

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

    return (
      <div class="wrap">
        <div class="id">
          {p ? <span class="av">{p.username.slice(0, 2)}</span> : <span class="av"></span>}
          <span>
            <span class="nm">
              {p ? <h1>{p.username}</h1> : <span class="sk tall" style="width:120px"></span>}
              {p?.is_admin ? <span class="tag">administrator</span> : null}
              {mine ? <span class="tag on">you</span> : null}
            </span>
            <span class="sub">
              {p ? (
                <>
                  {p.display_name ? <span class="dn">{p.display_name}</span> : null}
                  {p.display_name ? " · " : ""}
                  joined {relativeTime(p.created_at)} ·{" "}
                  {p.repos.length} {p.repos.length === 1 ? "repository" : "repositories"}
                  {mine || p.repos.length === 0 ? "" : " you can see"}
                </>
              ) : (
                <span class="sk" style="width:190px"></span>
              )}
            </span>
          </span>
          {mine ? (
            <a class="btn" href="/new" onClick={linkHandler("/new")}>
              <loom-icon name="plus" size={12}></loom-icon> new repository
            </a>
          ) : (
            <span></span>
          )}
        </div>

        <div class="panel">
          {filterable ? (
            <div class="panel-head">
              <span>repositories</span>
              <input
                class="filter"
                placeholder="filter"
                value={this.filter}
                onInput={(e: Event) => (this.filter = (e.target as HTMLInputElement).value)}
              />
            </div>
          ) : null}

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
        </div>
      </div>
    );
  }
}
