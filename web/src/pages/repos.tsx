/** Repository index. */
import { LoomElement, component, css, styles, reactive, inject } from "@toyz/loom";
// Shadows the global `fetch` in this module, which is why it is renamed.
import { fetch as query, type ApiState } from "@toyz/loom/query";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { type Repo } from "../api";
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
  @query<Repo[]>({ url: "/api/repos", init: { credentials: "same-origin" } })
  accessor repos!: ApiState<Repo[]>;

  /** This server, spelled the way the CLI wants it. */
  private origin(): string {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    return `${scheme}://${location.host}`;
  }

  private visible(): Repo[] {
    const q = this.filter.trim().toLowerCase();
    const all = this.repos.data ?? [];
    return q ? all.filter((r) => r.full_name.toLowerCase().includes(q)) : all;
  }

  update() {
    const list = this.visible();
    const all = this.repos.data ?? [];
    const priv = all.filter((r) => r.visibility === "private").length;
    // Worth a column of faces only if the faces differ.
    // Say what the list is, and — while filtering — how much of it you are
    // being shown, since a filtered count alone reads as the whole total.
    const value = this.repos.loading
      ? ""
      : this.filter
        ? `${list.length} of ${all.length}`
        : `${all.length - priv} public${priv ? ` · ${priv} private` : ""}`;

    return (
      <div class="wrap">
        {this.repos.error ? <fkit-notice message={this.repos.error.message}></fkit-notice> : null}

        {/* Somebody arriving with no account got a bare list under the word
            "Repositories" and no indication of what any of it is. The only
            explanation on the page was in the footer, under the fold, in grey.
            Signed in you already know where you are, so this is for the people
            who do not -- and it says what the thing does rather than selling
            it, because a self-hosted forge has nothing to sell. */}
        {!this.session.isAuthed ? (
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
              <a class="btn primary" href="/register" onClick={linkHandler("/register")}>
                register
              </a>
            </div>
          </div>
        ) : null}

        <fkit-section heading="Repositories" value={value}>
          <span slot="action" class="head-acts">
            <input
              placeholder="filter"
              value={this.filter}
              onInput={(e: Event) => (this.filter = (e.target as HTMLInputElement).value)}
            />
            {this.session.canCreateRepo ? (
              <a class="btn" href="/new" onClick={linkHandler("/new")}>
                <loom-icon name="plus" size={11}></loom-icon> new repository
              </a>
            ) : null}
          </span>

          <fkit-list>
            {this.repos.loading ? (
              [0, 1, 2, 3, 4].map(() => (
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
                    : this.session.isAuthed
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
        </fkit-section>
      </div>
    );
  }
}
