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
  .bar { display: flex; align-items: baseline; gap: 10px; margin-bottom: 10px; }
  .bar h1 { margin-right: 2px; }
  /* The count belongs to the title, not to the filter it was sitting beside. */
  .count { flex: 1; color: var(--faint); font-size: 11px; font-variant-numeric: tabular-nums; }
  .bar input { width: 180px; font-size: 12px; height: 26px; padding: 0 9px; }

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

  private visible(): Repo[] {
    const q = this.filter.trim().toLowerCase();
    const all = this.repos.data ?? [];
    return q ? all.filter((r) => r.full_name.toLowerCase().includes(q)) : all;
  }

  update() {
    const list = this.visible();
    return (
      <div class="wrap">
        <div class="bar">
          <h1>repositories</h1>
          <span class="count">
            {this.repos.loading
              ? ""
              : `${list.length}${this.filter ? ` / ${this.repos.data?.length ?? 0}` : ""}`}
          </span>
          <input
            placeholder="filter"
            value={this.filter}
            onInput={(e: Event) => (this.filter = (e.target as HTMLInputElement).value)}
          />
        </div>

        {this.repos.error ? <div class="error">{this.repos.error.message}</div> : null}

        {this.repos.loading ? (
          <div class="panel">
            {[0, 1, 2, 3, 4].map(() => (
              <div class="rr sk-row">
                <span class="ic"><span class="sk" style="width:13px;height:13px"></span></span>
                <span class="top"><span class="sk tall" style="width:min(38%,220px)"></span></span>
                <span class="right"><span class="sk" style="width:60px"></span></span>
                <span class="last"><span class="sk" style="width:min(52%,320px)"></span></span>
              </div>
            ))}
          </div>
        ) : list.length === 0 ? (
          <div class="panel">
            <div class="empty">
              <h2>{this.filter ? "no matches" : "nothing here yet"}</h2>
              <p class="prose">
                {this.filter
                  ? "No repository matches that filter."
                  : this.session.isAuthed
                    ? "Create a repository, then push to it from the fkit CLI."
                    : "Sign in to see private repositories you have access to."}
              </p>
              {!this.filter && this.session.isAuthed ? (
                <a class="btn primary" href="/new" onClick={linkHandler("/new")}>
                  <loom-icon name="plus" size={12}></loom-icon> new repository
                </a>
              ) : null}
            </div>
          </div>
        ) : (
          <div class="panel">
            {list.map((r) => repoRow(r, { withOwner: true }))}
          </div>
        )}
      </div>
    );
  }
}
