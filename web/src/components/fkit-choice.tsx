/**
 * A list of mutually exclusive choices, each with its consequence spelled out.
 *
 * A dropdown is right when the options are interchangeable and numerous — a
 * branch name, a role. It is wrong for a small set of consequential choices,
 * because it hides every option you are *not* on, which is exactly the one you
 * need to understand before switching. Repository visibility is the clearest
 * case: "private" and "public" mean nothing until you read what each does.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";

export interface Choice {
  value: string;
  label: string;
  hint?: string;
  icon?: string;
}

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }
  :host { display: block; font-family: var(--mono); }

  /* Cards with their own border and a gap, not rows welded to the container.
     Edge-to-edge rows inside a bordered panel read as a double border and give
     the selected option a slab of colour running the full width. */
  .list { display: flex; flex-direction: column; gap: 6px; }
  .opt {
    display: grid; grid-template-columns: 14px auto minmax(0, 1fr);
    align-items: start; gap: 10px;
    padding: 10px 12px; cursor: pointer;
    border: 1px solid var(--border); border-radius: var(--radius);
    background: transparent; text-align: left; width: 100%;
    font: inherit; color: var(--text);
    transition: border-color .12s, background .12s;
  }
  .opt:hover { border-color: var(--border-hi); background: var(--raised); }
  .opt.on { border-color: var(--accent); background: var(--accent-weak); }
  .opt:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
  @media (prefers-reduced-motion: reduce) { .opt { transition: none; } }

  .dot {
    width: 13px; height: 13px; border-radius: 50%; margin-top: 2px;
    border: 1px solid var(--border-hi); position: relative; flex: none;
  }
  .opt.on .dot { border-color: var(--accent); }
  .opt.on .dot::after {
    content: ""; position: absolute; inset: 2.5px;
    border-radius: 50%; background: var(--accent);
  }

  .ic { color: var(--muted); display: flex; margin-top: 1px; }
  .opt.on .ic { color: var(--accent); }

  .lab { font-size: 12.5px; }
  .opt.on .lab { color: var(--accent); }
  .hint {
    display: block; color: var(--muted); font-size: 11.5px;
    font-family: var(--sans); margin-top: 3px; line-height: 1.45;
  }
  .opt:disabled { opacity: .5; cursor: not-allowed; }
`;

@component("fkit-choice")
@styles(sheet)
export class FkitChoice extends LoomElement {
  @prop accessor options: Choice[] = [];
  @prop accessor value = "";
  @prop accessor disabled = false;

  update() {
    return (
      <div class="list" role="radiogroup">
        {this.options.map((o) => (
          <button
            type="button"
            role="radio"
            aria-checked={o.value === this.value ? "true" : "false"}
            class={`opt ${o.value === this.value ? "on" : ""}`}
            disabled={this.disabled}
            onClick={() => {
              if (this.disabled || o.value === this.value) return;
              this.dispatchEvent(
                new CustomEvent("pick", { detail: o.value, bubbles: true, composed: true }),
              );
            }}
          >
            <span class="dot"></span>
            <span class="ic">
              {o.icon ? <loom-icon name={o.icon} size={13}></loom-icon> : null}
            </span>
            <span>
              <span class="lab">{o.label}</span>
              {o.hint ? <span class="hint">{o.hint}</span> : null}
            </span>
          </button>
        ))}
      </div>
    );
  }
}
