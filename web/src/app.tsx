/**
 * App shell — the persistent header and the route outlet.
 */
import { LoomElement, component, css, styles, reactive, mount, inject } from "@toyz/loom";
import { base } from "./ui";
import "./components/user-menu";
import { Session } from "./session";
import { linkHandler, go } from "./nav";
import type { Meta, User } from "./api";

const shell = css`
  :host { min-height: 100vh; display: flex; flex-direction: column; background: var(--bg); }

  /* A thin status bar rather than a tall marketing navbar: this is chrome for a
     tool, so it takes as little vertical space as it can. */
  header {
    border-bottom: 1px solid var(--border);
    background: var(--surface);
    position: sticky; top: 0; z-index: 20;
  }
  header .wrap { display: flex; align-items: center; gap: 14px; height: 38px; }

  .brand { display: flex; align-items: baseline; gap: 1px; color: var(--text); font-weight: 600; }
  .brand:hover { text-decoration: none; }
  .brand .k { color: var(--accent); }
  .brand .tail { color: var(--faint); font-weight: 400; margin-left: 7px; font-size: 11px; }

  nav { display: flex; gap: 12px; flex: 1; }
  nav a { color: var(--muted); font-size: 12px; }
  nav a:hover { color: var(--text); text-decoration: none; }

  .who { display: flex; align-items: center; gap: 8px; font-size: 12px; }
  .who .name { color: var(--muted); }

  /* "flex: 1 0 auto" rather than "flex: 1" — with the latter a short page
     leaves the footer floating mid-viewport instead of at the bottom. */
  main { flex: 1 0 auto; padding: 16px 0 48px; }

  footer { border-top: 1px solid var(--border); padding: 10px 0; color: var(--faint); font-size: 11px; }
  footer .wrap { display: flex; gap: 16px; flex-wrap: wrap; }
`;

@component("fkit-app")
@styles(base, shell)
export class FkitApp extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor user: User | null | undefined = undefined;
  @reactive accessor meta: Meta | null = null;

  @mount
  init() {
    this.user = this.session.current;
    this.meta = this.session.meta.value;
    const offMeta = this.session.meta.subscribe((m: Meta | null) => {
      this.meta = m;
    });
    // The service owns the source of truth; mirroring it into a @reactive
    // field is what re-renders this component when login state changes.
    const offUser = this.session.user.subscribe((u: User | null | undefined) => {
      this.user = u;
    });
    return () => {
      offUser();
      offMeta();
    };
  }

  private async signOut() {
    await this.session.logout();
    go("/");
  }

  update() {
    const u = this.user;
    return (
      <div>
        <header>
          <div class="wrap">
            <a class="brand" href="/" onClick={linkHandler("/")}>
              <span class="k">f</span>kit<span class="tail">hub</span>
            </a>
            <nav>
              <a href="/" onClick={linkHandler("/")}>Repositories</a>
            </nav>

            {u === undefined ? (
              <span class="faint" style="font-size:12px">…</span>
            ) : u ? (
              <div class="who">
                <a class="btn" href="/new" onClick={linkHandler("/new")}>
                  <loom-icon name="plus" size={12}></loom-icon> new
                </a>
                <user-menu
                  username={u.username}
                  email={u.email ?? ""}
                  admin={u.is_admin}
                  onSignout={() => void this.signOut()}
                ></user-menu>
              </div>
            ) : (
              <div class="who">
                <a class="btn bare" href="/login" onClick={linkHandler("/login")}>sign in</a>
                {this.meta?.open_registration !== false ? (
                  <a class="btn" href="/register" onClick={linkHandler("/register")}>register</a>
                ) : null}
              </div>
            )}
          </div>
        </header>

        <main>
          <loom-outlet></loom-outlet>
        </main>

        <footer>
          <div class="wrap">
            <span>content-addressed version control</span>
            <span>fkit clone {location.protocol === "https:" ? "wss" : "ws"}://{location.host}/owner/repo</span>
          </div>
        </footer>
      </div>
    );
  }
}
