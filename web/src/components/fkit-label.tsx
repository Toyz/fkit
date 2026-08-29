/**
 * A label on an issue.
 *
 * The colour is a hue, not a hex value. A label picked as `#ff5544` on a dark
 * background is unreadable on a light one, and the person choosing it has no
 * way to know that — so what gets stored is the hue alone, and each theme
 * derives its own lightness and saturation from it. One decision, correct on
 * both backgrounds.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }
  :host { display: inline-flex; }

  .l {
    display: inline-flex; align-items: center; gap: 5px;
    padding: 1px 9px; line-height: 18px;
    border-radius: var(--radius-pill); font-size: 11px; white-space: nowrap;
    background: hsl(var(--h, 0) 30% 14%);
    color: hsl(var(--h, 0) 55% 68%);
    box-shadow: inset 0 0 0 1px hsl(var(--h, 0) 28% 26%);
  }
  @media (prefers-color-scheme: light) {
    .l {
      background: hsl(var(--h, 0) 60% 94%);
      color: hsl(var(--h, 0) 48% 28%);
      box-shadow: inset 0 0 0 1px hsl(var(--h, 0) 40% 78%);
    }
  }

  :host([clickable]) .l { cursor: pointer; }
  :host([clickable]) .l:hover { filter: brightness(1.15); }

  /* Shown while choosing: an unpicked label is the same colour, drained. */
  :host([off]) .l {
    background: transparent;
    color: var(--faint);
    box-shadow: inset 0 0 0 1px var(--border-hi);
  }

  .x {
    display: flex; border: 0; background: none; padding: 0; cursor: pointer;
    color: inherit; opacity: .65;
  }
  .x:hover { opacity: 1; }
`;

@component("fkit-label")
@styles(sheet)
export class FkitLabel extends LoomElement {
  @prop accessor name = "";
  @prop accessor hue = 0;
  /** Draw it drained, for a label offered but not applied. */
  @prop accessor off = false;
  @prop accessor clickable = false;
  /** Show an x that emits `remove`. */
  @prop accessor removable = false;

  update() {
    return (
      <span class="l" style={`--h:${this.hue}`} title={this.name}>
        {this.name}
        {this.removable ? (
          <button
            type="button"
            class="x"
            aria-label={`Remove ${this.name}`}
            onClick={(e: Event) => {
              e.stopPropagation();
              this.dispatchEvent(new CustomEvent("remove", { bubbles: true }));
            }}
          >
            <loom-icon name="x" size={9}></loom-icon>
          </button>
        ) : null}
      </span>
    );
  }
}
