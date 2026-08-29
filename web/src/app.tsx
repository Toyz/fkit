/**
 * App shell — the persistent header and the route outlet.
 */
import { LoomElement, component, css, styles, reactive, mount, inject, on } from "@toyz/loom";
import { debounce } from "@toyz/loom/element";
import { base } from "./ui";
import "./components/user-menu";
import { Session } from "./session";
import { linkHandler, go } from "./nav";
import type { Meta, User } from "./api";
import { prefetchRoute } from "./api";

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
     leaves the footer floating mid-viewport instead of at the bottom.
   *
   * The min-height keeps the footer *below* the fold while a page is still
   * loading. Without it the footer sits right at the viewport edge, and every
   * block that arrives late — a README most of all — shoves it down through
   * the visible area. That single movement was most of the page's layout
   * shift. Content growing below the fold is not something anyone perceives;
   * the footer sliding past their eyes is. */
  main {
    flex: 1 0 auto;
    padding: 16px 0 48px;
    /* The rule the comment above describes, which had gone missing. Without
       it the footer starts just under whatever has loaded so far and is shoved
       down through the viewport by everything that arrives after — one
       movement that was most of the page's measured layout shift. */
    min-height: calc(100vh - 140px);
  }

  footer { border-top: 1px solid var(--border); padding: 10px 0; color: var(--faint); font-size: 11px; }
  footer .wrap { display: flex; gap: 16px; flex-wrap: wrap; }
`;

@component("fkit-app")
@styles(base, shell)
export class FkitApp extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor user: User | null | undefined = undefined;
  @reactive accessor meta: Meta | null = null;

  /** The link the pointer is currently over, waiting to see if it stays. */
  private hovering = "";

  /**
   * Start a page's first request while the pointer is still travelling.
   *
   * One delegated listener rather than a handler per link: `composedPath`
   * crosses shadow roots, so this reaches anchors inside any component,
   * including ones that do not exist yet.
   */
  @on(document, "pointerover")
  onLinkHover(e: Event) {
    const a = e.composedPath().find((n) => n instanceof HTMLAnchorElement) as
      | HTMLAnchorElement
      | undefined;
    const href = a?.getAttribute("href") ?? "";
    // Same-origin routes only. An absolute URL is somebody else's server.
    if (!href.startsWith("/") || href.startsWith("//")) return;
    this.hovering = href;
    this.warmHovered();
  }

  /**
   * Debounced, because a pointer crossing a file listing passes over every row
   * on the way to the one it wants, and prefetching all of them would cost
   * more than the wait it saves.
   */
  @debounce(90)
  warmHovered() {
    if (this.hovering) prefetchRoute(this.hovering);
  }

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

        <fkit-to-top></fkit-to-top>
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
