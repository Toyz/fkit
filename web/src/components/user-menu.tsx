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
    position: absolute; top: calc(100% + 5px); right: 0; z-index: 60;
    min-width: 190px; padding: 3px;
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius);
  }
  .who {
    padding: 7px 9px 8px; border-bottom: 1px solid var(--border); margin-bottom: 3px;
  }
  .who .n { font-size: 12.5px; color: var(--text); }
  .who .e {
    display: block; color: var(--faint); font-size: 11px; margin-top: 2px;
    font-family: var(--sans); overflow: hidden; text-overflow: ellipsis;
  }
  a.item, button.item {
    display: flex; align-items: center; gap: 9px; width: 100%;
    padding: 6px 9px; border-radius: var(--radius);
    font: inherit; font-size: 12px; text-align: left;
    color: var(--muted); background: transparent; border: 0; cursor: pointer;
    text-decoration: none;
  }
  a.item:hover, button.item:hover { background: var(--raised); color: var(--text); text-decoration: none; }
  .item loom-icon { opacity: .75; flex: none; }
  .sep { height: 1px; background: var(--border); margin: 3px 0; }
  button.item.out:hover { color: var(--removed); }
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
    const items: [string, string, string][] = [
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
              <span class="n">{this.username}</span>
              <span class="e">{this.email}</span>
            </div>

            {items.map(([href, label, ic]) => (
              <a class="item" href={href} onClick={this.go(href)}>
                <loom-icon name={ic} size={13}></loom-icon>
                {label}
              </a>
            ))}

            {this.admin ? (
              <>
                <div class="sep"></div>
                <a class="item" href="/admin" onClick={this.go("/admin")}>
                  <loom-icon name="lock" size={13}></loom-icon>
                  server administration
                </a>
              </>
            ) : null}

            <div class="sep"></div>
            <button
              class="item out"
              onClick={() => {
                this.open = false;
                this.dispatchEvent(
                  new CustomEvent("signout", { bubbles: true, composed: true }),
                );
              }}
            >
              <loom-icon name="external" size={13}></loom-icon>
              sign out
            </button>
          </div>
        ) : null}
      </div>
    );
  }
}
