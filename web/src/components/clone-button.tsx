/**
 * The "how do I get this" button.
 *
 * Every repository page needs one, not just an empty one: the clone URL is the
 * single most-copied string in a forge, and it was previously only reachable
 * from the empty state and the settings page.
 */
import { LoomElement, component, css, styles, reactive, prop, mount, query } from "@toyz/loom";

const sheet = css`
  /* Shadow roots do not inherit the document's box-sizing reset, so an input
     with width:100% and padding overflows its own dialog. */
  *, *::before, *::after { box-sizing: border-box; }

  :host { position: relative; display: inline-block; font-family: var(--mono); }

  .trigger {
    display: inline-flex; align-items: center; gap: 6px;
    font: inherit; font-size: 12px; padding: 4px 9px;
    border: 1px solid var(--accent); border-radius: var(--radius);
    background: var(--accent-weak); color: var(--accent); cursor: pointer;
  }
  .trigger:hover { background: var(--accent); color: var(--bg); }
  .trigger .chev { transition: transform .12s; }
  .trigger.open .chev { transform: rotate(180deg); }

  .pop {
    position: absolute; top: calc(100% + 4px); right: 0; z-index: 40;
    width: 340px; padding: 11px;
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius);
    display: flex; flex-direction: column; gap: 11px;
  }
  .label {
    display: flex; align-items: baseline; justify-content: space-between; gap: 8px;
    font-size: 10px; text-transform: uppercase; letter-spacing: .07em; color: var(--muted);
  }
  .label button {
    font: inherit; font-size: 11px; padding: 1px 5px;
    border: 1px solid transparent; border-radius: 2px;
    background: transparent; color: var(--muted); cursor: pointer;
  }
  .label button:hover { background: var(--raised); color: var(--text); }
  .val {
    background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius);
    padding: 6px 8px; font-size: 11.5px; color: var(--text);
    overflow-x: auto; white-space: nowrap;
  }
  .note { color: var(--faint); font-size: 11px; font-family: var(--sans); line-height: 1.45; }
`;

@component("clone-button")
@styles(sheet)
export class CloneButton extends LoomElement {
  @prop accessor url = "";
  @prop accessor visibility = "public";

  @reactive accessor open = false;
  @reactive accessor copied = "";

  @query(".pop") accessor pop!: HTMLElement | null;

  @mount
  init() {
    // Capture-phase pointerdown + composedPath: the only combination that
    // reliably dismisses through shadow roots and regardless of whether
    // anything downstream stops propagation.
    const away = (e: Event) => {
      if (this.open && !e.composedPath().includes(this)) this.open = false;
    };
    document.addEventListener("pointerdown", away, true);
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") this.open = false;
    };
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", esc);
    };
  }

  private async copy(key: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      this.copied = key;
      setTimeout(() => {
        if (this.copied === key) this.copied = "";
      }, 1400);
    } catch {
      /* clipboard blocked; the text is selectable either way */
    }
  }

  private row(key: string, label: string, value: string) {
    return (
      <div>
        <div class="label">
          <span>{label}</span>
          <button onClick={() => void this.copy(key, value)}>
            <loom-icon name={this.copied === key ? "check" : "copy"} size={11}></loom-icon>
            {this.copied === key ? " copied" : " copy"}
          </button>
        </div>
        <div class="val">{value}</div>
      </div>
    );
  }

  update() {
    return (
      <div>
        <button class={`trigger ${this.open ? "open" : ""}`} onClick={() => (this.open = !this.open)}>
          <loom-icon name="link" size={12}></loom-icon>
          clone
          <loom-icon class="chev" name="chevron" size={11}></loom-icon>
        </button>

        {this.open ? (
          <div class="pop">
            {this.row("cmd", "clone", `fkit clone ${this.url}`)}
            {this.row("url", "url", this.url)}
            <div class="note">
              {this.visibility === "public"
                ? "Public — cloning needs no account. Pushing needs a token."
                : "Private — both cloning and pushing need an access token."}
            </div>
          </div>
        ) : null}
      </div>
    );
  }
}
