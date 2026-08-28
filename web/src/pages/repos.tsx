/** Repository index. */
import { LoomElement, component, css, styles, reactive, mount, inject } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { api, type Repo } from "../api";
import { linkHandler } from "../nav";
import { repoRow, repoRowSheet } from "../repo-row";
import { Session } from "../session";

const sheet = css`
  .bar { display: flex; align-items: center; gap: 12px; margin-bottom: 10px; }
  .bar h1 { flex: 1; }
  .bar input { max-width: 220px; font-size: 12px; }
  .count { color: var(--faint); font-size: 11px; }

`;

@route("/")
@component("page-repos")
@styles(base, repoRowSheet, sheet)
export class PageRepos extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor repos: Repo[] | null = null;
  @reactive accessor error = "";
  @reactive accessor filter = "";

  @mount
  async load() {
    try {
      this.repos = await api.repos();
    } catch (e) {
      this.error = (e as Error).message;
    }
  }

  private visible(): Repo[] {
    const q = this.filter.trim().toLowerCase();
    const all = this.repos ?? [];
    return q ? all.filter((r) => r.full_name.toLowerCase().includes(q)) : all;
  }

  update() {
    const list = this.visible();
    return (
      <div class="wrap">
        <div class="bar">
          <h1>repositories</h1>
          <span class="count">
            {this.repos === null ? "" : `${list.length}${this.filter ? ` / ${this.repos.length}` : ""}`}
          </span>
          <input
            placeholder="filter"
            value={this.filter}
            onInput={(e: Event) => (this.filter = (e.target as HTMLInputElement).value)}
          />
        </div>

        {this.error ? <div class="error">{this.error}</div> : null}

        {this.repos === null ? (
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
