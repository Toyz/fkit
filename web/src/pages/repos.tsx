/** Repository index. */
import { LoomElement, component, css, styles, reactive, inject } from "@toyz/loom";
// Shadows the global `fetch` in this module, which is why it is renamed.
import { fetch as query, type ApiState } from "@toyz/loom/query";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { type Repo } from "../api";
import { linkHandler } from "../nav";
import { repoRow, repoRowSheet, spansOwners } from "../repo-row";
import { Session } from "../session";

const sheet = css`

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
    const tiles = spansOwners(all);
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
              list.map((r) => repoRow(r, { withOwner: true, ownerTiles: tiles }))
            )}
          </fkit-list>
        </fkit-section>
      </div>
    );
  }
}
