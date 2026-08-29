/**
 * The account menu.
 *
 * The username was previously plain text with the actions scattered beside it
 * as loose buttons — nothing to click to reach your own profile, and no obvious
 * home for anything account-shaped that gets added later.
 */
import { LoomElement, component, css, styles, reactive, prop, mount } from "@toyz/loom";
import { linkHandler } from "../nav";
import "./fkit-avatar";

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }
  :host { position: relative; display: inline-block; font-family: var(--mono); }

  .trigger {
    display: inline-flex; align-items: center; gap: 7px;
    font: inherit; font-size: 12px; padding: 3px 7px 3px 3px;
    border: 1px solid transparent; border-radius: var(--radius);
    background: transparent; color: var(--muted); cursor: pointer;
  }
  .trigger:hover, .trigger.open { background: var(--raised); color: var(--text); }

  /* The same component the profile page and its edit form use, so one person
     is one face and one colour everywhere they appear. */
  fkit-avatar { flex: none; }
  .chev { transition: transform .12s; }
  .trigger.open .chev { transform: rotate(180deg); }

  .pop {
    position: absolute; top: calc(100% + 6px); right: 0; z-index: 60;
    /* Sized so the longest label sits on one line. "server administration" was
       wrapping onto two, which reads as a broken row rather than a long one —
       and nowrap below makes that a guarantee rather than a width that
       happened to fit today. */
    min-width: 232px; padding: 5px;
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius);
    box-shadow: 0 12px 32px rgb(0 0 0 / .5), 0 0 0 1px rgb(0 0 0 / .2);
  }
  @media (prefers-reduced-motion: no-preference) {
    .pop { animation: pop-in .11s cubic-bezier(.22,.61,.36,1); }
  }
  @keyframes pop-in {
    from { opacity: 0; transform: translateY(-5px) scale(.985); }
    to   { opacity: 1; transform: translateY(0) scale(1); }
  }

  /* Who you are. The same avatar the trigger shows, so the menu is visibly
     attached to the thing that opened it, and the role is stated rather than
     inferred from whether an admin link happens to be present. */
  .who {
    display: flex; align-items: center; gap: 10px;
    padding: 9px 9px 10px; margin-bottom: 2px;
  }
  .who .id { min-width: 0; flex: 1; }
  .who .n {
    display: flex; align-items: center; gap: 6px;
    font-size: 13px; color: var(--text);
  }
  .who .n b {
    font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .who .e {
    display: block; color: var(--faint); font-size: 11px; margin-top: 2px;
    font-family: var(--sans);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .role {
    flex: none; font-size: 9.5px; letter-spacing: .06em; text-transform: uppercase;
    padding: 1px 5px; border-radius: var(--radius-sm);
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 15%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent) 35%, transparent);
  }

  /* The same caption the admin sidebar uses, so this reads as part of the app
     rather than a menu that arrived from somewhere else. */
  .grp {
    font-size: 9.5px; letter-spacing: .08em; text-transform: uppercase;
    color: var(--faint); padding: 7px 9px 4px;
  }

  a.item, button.item {
    display: flex; align-items: center; gap: 10px; width: 100%;
    padding: 7px 9px; border-radius: var(--radius);
    font: inherit; font-size: 12px; text-align: left; white-space: nowrap;
    color: var(--muted); background: transparent; border: 0; cursor: pointer;
    text-decoration: none;
  }
  a.item:hover, button.item:hover {
    background: var(--raised); color: var(--text); text-decoration: none;
  }
  a.item:focus-visible, button.item:focus-visible {
    outline: 2px solid var(--accent); outline-offset: -2px;
  }
  .item loom-icon { opacity: .65; flex: none; }
  .item:hover loom-icon { opacity: 1; }
  /* Reading someone else's repository because you administer the server is a
     different kind of act from editing your own profile, and the menu should
     not present them identically. */
  a.item.admin { color: var(--accent); }
  a.item.admin loom-icon { opacity: .9; }

  .sep { height: 1px; background: var(--border); margin: 5px 0; }
  button.item.out:hover { color: var(--removed); }
  button.item.out:hover loom-icon { color: var(--removed); }
`;

@component("user-menu")
@styles(sheet)
export class UserMenu extends LoomElement {
  @prop accessor username = "";
  @prop accessor email = "";
  @prop accessor admin = false;

  @reactive accessor open = false;

  @mount
  init() {
    const away = (e: Event) => {
      if (this.open && !e.composedPath().includes(this)) this.open = false;
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") this.open = false;
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", esc);
    };
  }

  private go(href: string) {
    return (e: MouseEvent) => {
      this.open = false;
      linkHandler(href)(e);
    };
  }

  update() {
    // Grouped rather than one flat list: four account links and a server-wide
    // one are different kinds of thing, and a separator alone does not say so.
    const account: [string, string, string][] = [
      [`/${this.username}`, "your repositories", "repo"],
      ["/settings", "profile", "settings"],
      ["/settings/tokens", "access tokens", "key"],
      ["/settings/sessions", "sessions", "history"],
    ];

    return (
      <div>
        <button
          class={`trigger ${this.open ? "open" : ""}`}
          onClick={() => (this.open = !this.open)}
          aria-haspopup="menu"
          aria-expanded={this.open ? "true" : "false"}
        >
          <fkit-avatar name={this.username} size={21}></fkit-avatar>
          {this.username}
          <loom-icon class="chev" name="chevron" size={11}></loom-icon>
        </button>

        {this.open ? (
          <div class="pop" role="menu">
            <div class="who">
              <fkit-avatar name={this.username} size={30}></fkit-avatar>
              <span class="id">
                <span class="n">
                  <b>{this.username}</b>
                  {this.admin ? <span class="role">admin</span> : null}
                </span>
                <span class="e" title={this.email}>{this.email}</span>
              </span>
            </div>

            <div class="grp">Account</div>
            {account.map(([href, label, ic]) => (
              <a class="item" href={href} onClick={this.go(href)} role="menuitem">
                <loom-icon name={ic} size={13}></loom-icon>
                {label}
              </a>
            ))}

            {this.admin ? (
              <>
                <div class="grp">Server</div>
                <a class="item admin" href="/admin" onClick={this.go("/admin")} role="menuitem">
                  <loom-icon name="shield" size={13}></loom-icon>
                  administration
                </a>
              </>
            ) : null}

            <div class="sep"></div>
            <button
              class="item out"
              role="menuitem"
              onClick={() => {
                this.open = false;
                this.dispatchEvent(
                  new CustomEvent("signout", { bubbles: true, composed: true }),
                );
              }}
            >
              <loom-icon name="signout" size={13}></loom-icon>
              sign out
            </button>
          </div>
        ) : null}
      </div>
    );
  }
}
