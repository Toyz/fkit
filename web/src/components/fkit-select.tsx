/**
 * A styled single-select.
 *
 * A native `<select>` renders as an operating-system widget: it ignores the
 * page's typeface, colours and density, cannot show a description per option,
 * and looks like a different application embedded in the page. On a surface
 * this dense that is the one control that gives the game away.
 *
 * Emits a `pick` CustomEvent carrying the chosen value.
 */
import { LoomElement, component, css, styles, reactive, prop, mount } from "@toyz/loom";

export interface SelectOption {
  value: string;
  label: string;
  /** Optional second line explaining what the choice means. */
  hint?: string;
}

const sheet = css`
  /* Shadow roots do not inherit the document's box-sizing reset, so an input
     with width:100% and padding overflows its own dialog. */
  *, *::before, *::after { box-sizing: border-box; }

  :host { position: relative; display: inline-block; font-family: var(--mono); }

  .trigger {
    display: inline-flex; align-items: center; justify-content: space-between; gap: 10px;
    font: inherit; font-size: 12px; padding: 5px 9px; min-width: 150px;
    border: 1px solid var(--border-hi); border-radius: var(--radius);
    background: var(--bg); color: var(--text); cursor: pointer; text-align: left;
  }
  .trigger:hover { border-color: var(--faint); }
  .trigger.open { border-color: var(--accent); }
  .trigger .chev { color: var(--muted); transition: transform .12s; }
  .trigger.open .chev { transform: rotate(180deg); }

  .pop {
    position: absolute; top: calc(100% + 4px); left: 0; z-index: 40;
    min-width: 100%; width: max-content; max-width: 320px;
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius); overflow: hidden; padding: 3px 0;
  }
  .opt {
    display: grid; grid-template-columns: 14px minmax(0, 1fr);
    align-items: start; gap: 8px; padding: 6px 10px; cursor: pointer; font-size: 12px;
  }
  .opt:hover, .opt.active { background: var(--raised); }
  .opt .tick { color: var(--accent); display: flex; padding-top: 2px; }
  .opt.on .lab { color: var(--accent); }
  .opt .hint {
    display: block; color: var(--muted); font-size: 11px; font-family: var(--sans);
    margin-top: 2px; line-height: 1.4;
  }
`;

@component("fkit-select")
@styles(sheet)
export class FkitSelect extends LoomElement {
  @prop accessor options: SelectOption[] = [];
  @prop accessor value = "";

  @reactive accessor open = false;
  @reactive accessor active = 0;

  @mount
  init() {
    const away = (e: Event) => {
      if (this.open && !e.composedPath().includes(this)) this.open = false;
    };
    const key = (e: KeyboardEvent) => {
      if (!this.open) return;
      if (e.key === "Escape") {
        this.open = false;
      } else if (e.key === "ArrowDown") {
        e.preventDefault();
        this.active = Math.min(this.active + 1, this.options.length - 1);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        this.active = Math.max(this.active - 1, 0);
      } else if (e.key === "Enter") {
        e.preventDefault();
        const hit = this.options[this.active];
        if (hit) this.choose(hit.value);
      }
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", key);
    };
  }

  private choose(value: string) {
    this.open = false;
    if (value !== this.value) {
      this.dispatchEvent(new CustomEvent("pick", { detail: value, bubbles: true, composed: true }));
    }
  }

  update() {
    const current = this.options.find((o) => o.value === this.value);
    return (
      <div>
        <button
          class={`trigger ${this.open ? "open" : ""}`}
          onClick={() => {
            this.open = !this.open;
            this.active = Math.max(0, this.options.findIndex((o) => o.value === this.value));
          }}
        >
          <span>{current?.label ?? this.value ?? "—"}</span>
          <loom-icon class="chev" name="chevron" size={11}></loom-icon>
        </button>

        {this.open ? (
          <div class="pop">
            {this.options.map((o, i) => (
              <div
                class={`opt ${o.value === this.value ? "on" : ""} ${i === this.active ? "active" : ""}`}
                onClick={() => this.choose(o.value)}
                onMouseEnter={() => (this.active = i)}
              >
                <span class="tick">
                  {o.value === this.value ? <loom-icon name="check" size={12}></loom-icon> : null}
                </span>
                <span>
                  <span class="lab">{o.label}</span>
                  {o.hint ? <span class="hint">{o.hint}</span> : null}
                </span>
              </div>
            ))}
          </div>
        ) : null}
      </div>
    );
  }
}
