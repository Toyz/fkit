/** Repository index. */
import { LoomElement, component, css, styles, reactive, inject, mount, debounce } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { api, type Repo, type User } from "../api";
import { linkHandler } from "../nav";
import { repoRow, repoRowSheet } from "../repo-row";
import { Session } from "../session";

const sheet = css`
  /* The front door, for people who have not been through it.
     Deliberately not a hero: no gradient, no slab of type, nothing that would
     make the repository list below it look like an afterthought. It is a
     masthead -- the name, what the thing is, and the one command that gets you
     started -- closed by the same hairline every heading here draws. */
  .hail {
    padding: 4px 0 22px; margin-bottom: 26px;
    border-bottom: 1px solid var(--border);
  }
  .hail h1 {
    display: flex; align-items: baseline; flex-wrap: wrap; gap: 10px;
    margin: 0 0 10px; font-size: 15px; font-weight: 500;
  }
  .hail .mark {
    font-family: var(--mono); font-weight: 700; letter-spacing: -0.02em;
    font-size: 21px; color: var(--text);
  }
  .hail .tag {
    font-family: var(--sans); font-size: 12px; color: var(--muted);
  }
  .hail .lede {
    font-family: var(--sans); font-size: 12.5px; line-height: 1.6;
    color: var(--muted); margin: 0; max-width: 68ch;
  }
  /* The command is the point of the row, so it is the thing set in type you
     could copy, and the buttons sit at the far end of it. */
  .hail .try {
    display: flex; align-items: center; flex-wrap: wrap; gap: 10px;
    margin-top: 16px;
  }
  .hail code {
    font-family: var(--mono); font-size: 11.5px; color: var(--faint);
    background: var(--raised); border: 1px solid var(--border);
    border-radius: var(--radius); padding: 5px 10px;
    overflow-x: auto; max-width: 100%;
  }
  .hail .fill { flex: 1; }

  .more { display: flex; justify-content: center; margin-top: 14px; }

  /* A search in flight, said on the control that started it. */
  .filter.busy { animation: seek 1.1s ease-in-out infinite; }
  @keyframes seek {
    0%, 100% { border-color: var(--border-hi); }
    50%      { border-color: var(--accent); }
  }
  @media (prefers-reduced-motion: reduce) {
    .filter.busy { animation: none; border-color: var(--accent); }
  }

  .empty { padding: 40px 14px; text-align: center; }
  .empty h2 { color: var(--muted); }
  .empty .prose {
    font-family: var(--sans); color: var(--faint); font-size: 12.5px;
    margin: 7px auto 0; max-width: 46ch; line-height: 1.5;
  }
  .empty .btn { margin-top: 14px; }
`;

@route("/")
@component("page-repos")
@styles(base, repoRowSheet, sheet)
export class PageRepos extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor filter = "";
  @reactive accessor items: Repo[] | null = null;
  @reactive accessor cursor: string | null = null;
  @reactive accessor total = 0;
  @reactive accessor busy = false;
  @reactive accessor failed = "";
  /** Whether this load has gone on long enough to be worth a placeholder. */
  @reactive accessor slow = false;

  /**
   * The session, mirrored so this page re-renders when it resolves.
   *
   * Reading `session.isAuthed` straight out of the service does not subscribe
   * to anything, so nothing re-rendered when the answer arrived: whatever the
   * page happened to draw first was what stayed. And before the answer
   * arrives `isAuthed` is false, so the first draw was the signed-out one --
   * a signed-in visitor refreshing this page got the front-door masthead with
   * "sign in" and "register" on it, and kept it until some unrelated state
   * change happened to redraw.
   *
   * `undefined` here means "not known yet", which is a different thing from
   * "signed out" and is deliberately not collapsed into it.
   */
  @reactive accessor user: User | null | undefined = undefined;

  @mount
  watchSession() {
    this.user = this.session.current;
    return this.session.user.subscribe((u: User | null | undefined) => {
      this.user = u;
    });
  }

  @mount
  first() {
    void this.load();
  }

  /** Placeholders only once the wait is long enough to need them. */
  @debounce(180)
  private admitSlow() {
    if (this.items === null) this.slow = true;
  }

  /**
   * Search on the server, a beat after the typing stops.
   *
   * The listing used to fetch two hundred rows once and filter that array in
   * the browser, which is a search that cannot find the two hundred and first
   * repository and reports "no matches" about something that exists.
   */
  private onFilter(v: string) {
    this.filter = v;
    // Set on the keystroke, not on the request, so the wait itself shows.
    this.busy = true;
    this.runSearch();
  }

  /** Loom's decorator: it cancels itself when the element goes away. */
  @debounce(220)
  private runSearch() {
    void this.load();
  }

  private async load() {
    this.busy = true;
    this.failed = "";
    if (this.items === null) this.admitSlow();
    try {
      const page = await api.repoPage({ q: this.filter });
      this.items = page.items;
      this.cursor = page.next;
      this.total = page.total;
    } catch (e) {
      this.failed = (e as Error).message;
      this.items = [];
      this.cursor = null;
      this.total = 0;
    } finally {
      this.busy = false;
    }
  }

  private async more() {
    if (!this.cursor || this.busy) return;
    this.busy = true;
    try {
      const page = await api.repoPage({ q: this.filter, cursor: this.cursor });
      // Appended: this is the next page of the same list.
      this.items = [...(this.items ?? []), ...page.items];
      this.cursor = page.next;
      this.total = page.total;
    } finally {
      this.busy = false;
    }
  }

  /**
   * The listing, as a request rather than as a nullable field.
   *
   * `ApiState` keeps "no data yet" and "no repositories" apart — as separate
   * members, not as two readings of the same `null`. Conflating them is what
   * put "this repository is empty" on screen mid-load elsewhere in this app,
   * and here it is not expressible.
   *
   * The decorator also does the part that is easy to skip by hand: `fetch()`
   * resolves for a 404 or a 500, so a hand-written `.then(r => r.json())` puts
   * an error body in `data` and reports success. This throws instead.
   */
  /** This server, spelled the way the CLI wants it. */
  private origin(): string {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    return `${scheme}://${location.host}`;
  }

  update() {
    const list = this.items ?? [];
    const loading = this.items === null;
    // What the list is, and how much of it is on screen. The total comes from
    // the server: this page holds one page, and a page cannot say how many
    // there are.
    const value = loading
      ? ""
      : list.length === this.total
        ? `${this.total} ${this.total === 1 ? "repository" : "repositories"}`
        : `${list.length} of ${this.total}`;

    return (
      <div class="wrap">
        {this.failed ? <fkit-notice message={this.failed}></fkit-notice> : null}

        {/* Somebody arriving with no account got a bare list under the word
            "Repositories" and no indication of what any of it is. The only
            explanation on the page was in the footer, under the fold, in grey.
            Signed in you already know where you are, so this is for the people
            who do not -- and it says what the thing does rather than selling
            it, because a self-hosted forge has nothing to sell. */}
        {this.user === null ? (
          <div class="hail">
            <h1>
              <span class="mark">fkit</span>
              <span class="tag">content-addressed version control</span>
            </h1>
            <p class="lede">
              Every chunk, file, tree and commit is named by the BLAKE3 digest of
              what it holds. A hash means one exact state of one exact tree, and
              content that is identical is stored once — across branches, across
              forks, across every repository on this server that has it.
            </p>
            <div class="try">
              <code>fkit clone {this.origin()}/owner/repo</code>
              <span class="fill"></span>
              <a class="btn" href="/login" onClick={linkHandler("/login")}>sign in</a>
              {/* Offered only where it leads somewhere. The header has always
                  checked this; the front page did not, so a server with
                  registration closed put its most prominent button on the one
                  page a stranger is most likely to land on, and the only thing
                  behind it was a refusal. */}
              {this.session.meta.value?.open_registration !== false ? (
                <a class="btn primary" href="/register" onClick={linkHandler("/register")}>
                  register
                </a>
              ) : null}
            </div>
          </div>
        ) : null}

        <fkit-section heading="Repositories" value={value}>
          <span slot="action" class="head-acts">
            <input
              class={this.busy ? "filter busy" : "filter"}
              placeholder="filter"
              value={this.filter}
              onInput={(e: Event) => this.onFilter((e.target as HTMLInputElement).value)}
            />
            {this.session.canCreateRepo ? (
              <a class="btn" href="/new" onClick={linkHandler("/new")}>
                <loom-icon name="plus" size={11}></loom-icon> new repository
              </a>
            ) : null}
          </span>

          <fkit-list>
            {loading ? (
              /* Still the loading branch either way: falling through to the
                 next one would render "nothing here yet" at somebody whose
                 list is on its way, which is a worse thing to flash than a
                 placeholder. Empty until the wait earns the placeholder. */
              (this.slow ? [0, 1, 2, 3, 4] : []).map(() => (
                <div class="rr sk-row">
                  <span class="ic"><span class="sk" style="width:19px;height:19px"></span></span>
                  <span class="top"><span class="sk tall" style="width:min(38%,220px)"></span></span>
                  <span class="when"><span class="sk" style="width:60px"></span></span>
                  <span class="last"><span class="sk" style="width:min(52%,320px)"></span></span>
                </div>
              ))
            ) : list.length === 0 ? (
              <div class="empty">
                <h2>{this.filter ? "no matches" : "nothing here yet"}</h2>
                <p class="prose">
                  {this.filter
                    ? "No repository matches that filter."
                    : this.user
                      ? "Create a repository, then push to it from the fkit CLI."
                      : "Sign in to see private repositories you have access to."}
                </p>
                {!this.filter && this.session.canCreateRepo ? (
                  <a class="btn primary" href="/new" onClick={linkHandler("/new")}>
                    <loom-icon name="plus" size={12}></loom-icon> new repository
                  </a>
                ) : null}
              </div>
            ) : (
              list.map((r) => repoRow(r, { withOwner: true }))
            )}
          </fkit-list>

          {list.length && this.cursor ? (
            <div class="more">
              <button class="btn" disabled={this.busy} onClick={() => void this.more()}>
                {this.busy ? "loading" : "show more"}
              </button>
            </div>
          ) : null}
        </fkit-section>
      </div>
    );
  }
}
