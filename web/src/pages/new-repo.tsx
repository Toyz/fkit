/** Create a repository, then show exactly how to push to it. */
import { LoomElement, component, css, styles, reactive, inject, mount } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { notify } from "../components/fkit-notice";
import { base } from "../ui";
import { api, syncUrl, type Repo } from "../api";
import { go, linkHandler } from "../nav";
import { Session } from "../session";

const sheet = css`
  .box { max-width: 520px; margin: 6vh auto 0; }
  .panel-body { padding: 18px; }
  h1 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: var(--muted); margin-bottom: 14px; }

  .vis { display: grid; gap: 6px; }
  .opt {
    display: flex; gap: 9px; align-items: flex-start;
    padding: 9px 11px; border: 1px solid var(--border); border-radius: var(--radius); cursor: pointer;
  }
  .opt:hover { border-color: var(--border-hi); }
  .opt.sel { border-color: var(--accent); background: var(--accent-weak); }
  .opt input { width: auto; margin-top: 2px; }
  .opt .t { font-size: 12px; }
  .opt .d { color: var(--muted); font-size: 11px; font-family: var(--sans); display: block; }

  .cmd {
    background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius);
    padding: 10px 12px; overflow-x: auto; font-size: 12px; color: var(--muted);
    white-space: pre; margin: 0;
  }
  .hint { color: var(--faint); font-size: 11px; margin-top: 4px; font-family: var(--sans); }
`;

@route("/new")
@component("page-new-repo")
@styles(base, sheet)
export class PageNewRepo extends LoomElement {
  @inject("session") accessor session!: Session;

  /**
   * Set once `/auth/me` has answered.
   *
   * This page has no query of its own, so nothing else would re-render it
   * when the session arrives — the pages that appear to work without this
   * only do so because a query resolving happens to render them again.
   */
  @reactive accessor sessionReady = false;

  @mount
  async waitForSession() {
    await this.session.ready();
    this.sessionReady = true;
  }

  @reactive accessor visibility: "private" | "public" = "private";
  @reactive accessor error = "";
  @reactive accessor busy = false;
  @reactive accessor created: Repo | null = null;

  private async submit(e: Event) {
    e.preventDefault();
    const form = e.target as HTMLFormElement;
    const name = (form.elements.namedItem("name") as HTMLInputElement).value.trim();
    const description = (form.elements.namedItem("description") as HTMLInputElement).value.trim();

    this.error = "";
    this.busy = true;
    try {
      this.created = await api.createRepo({
        name,
        description: description || undefined,
        visibility: this.visibility,
      });
    } catch (err) {
      // Reported in front of the page: a name already taken is the common
      // case, and it is the sort of thing people re-press the button over.
      void notify({
        title: "Could not create it",
        body: (err as Error).message,
        tone: "error",
      });
    } finally {
      this.busy = false;
    }
  }

  update() {
    // Hiding the buttons is not a guard: /new is a URL anyone can type. The
    // server refuses regardless, but arriving at a form that cannot submit is
    // a worse way to find out than being told here.
    if (this.sessionReady && this.session.current && !this.session.canCreateRepo) {
      return (
        <div class="wrap">
          <div class="panel">
            <div class="empty">
              <h2>repositories are not yours to create here</h2>
              <p class="prose">
                This account can read, open issues and comment, but an
                administrator has not granted it the ability to create
                repositories on this server. Ask one to change that if you
                need it.
              </p>
              <a class="btn" href="/" onClick={linkHandler("/")}>
                back to repositories
              </a>
            </div>
          </div>
        </div>
      );
    }

    if (this.created) {
      const r = this.created;
      const url = syncUrl(r.owner, r.name);
      return (
        <div class="wrap">
          <div class="box">
            <div class="panel"><div class="panel-body">
              <h1>{r.full_name} is ready</h1>
              <div class="hint" style="margin-bottom:6px">push an existing repository</div>
              <pre class="cmd">
{`fkit remote ${url}
export FKIT_TOKEN=<your access token>
fkit push`}
              </pre>
              <div class="hint" style="margin:14px 0 6px">or start a new one</div>
              <pre class="cmd">
{`fkit init my-project && cd my-project
fkit config --global author.name "Your Name"
fkit config --global author.email you@example.com
fkit commit -m "first commit"
fkit remote ${url}
fkit push`}
              </pre>
              <div class="row" style="margin-top:16px">
                <a
                  class="btn primary"
                  href={`/${r.owner}/${r.name}`}
                  onClick={linkHandler(`/${r.owner}/${r.name}`)}
                >
                  go to repository
                </a>
                <a class="btn" href="/settings/tokens" onClick={linkHandler("/settings/tokens")}>
                  create an access token
                </a>
              </div>
            </div></div>
          </div>
        </div>
      );
    }

    return (
      <div class="wrap">
        <div class="box">
          <div class="panel"><div class="panel-body">
            <h1>new repository</h1>
            {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}
            <form onSubmit={(e: Event) => void this.submit(e)}>
              <div class="field">
                <label>name</label>
                <input name="name" placeholder="my-project" autofocus required />
              </div>
              <div class="field">
                <label>description (optional)</label>
                <input name="description" placeholder="what is it for?" />
              </div>
              <div class="field">
                <label>visibility</label>
                <div class="vis">
                  {(
                    [
                      ["private", "private", "Only you and collaborators you add can see it."],
                      ["public", "public", "Anyone can read it — including people without an account. Only collaborators can push."],
                    ] as const
                  ).map(([value, title, desc]) => (
                    <label class={`opt ${this.visibility === value ? "sel" : ""}`}>
                      <input
                        type="radio"
                        name="visibility"
                        checked={this.visibility === value}
                        onChange={() => (this.visibility = value)}
                      />
                      <span>
                        <span class="t">{title}</span>
                        <span class="d" style="display:block">{desc}</span>
                      </span>
                    </label>
                  ))}
                </div>
              </div>
              <div class="row" style="margin-top:16px">
                <button class="primary" type="submit" disabled={this.busy}>
                  {this.busy ? "creating…" : "create repository"}
                </button>
                <button type="button" class="bare" onClick={() => go("/")}>cancel</button>
              </div>
            </form>
          </div></div>
        </div>
      </div>
    );
  }
}
