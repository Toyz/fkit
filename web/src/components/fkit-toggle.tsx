/**
 * A switch.
 *
 * `<input type="checkbox">` is drawn by the operating system: it ignores the
 * page's colours and density, and it reads as "a form to fill in" rather than
 * "a setting that is currently on".
 *
 * Built on a real `<button role="switch">` so keyboard and screen-reader
 * behaviour comes from the platform even though the pixels do not.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";

const sheet = css`
  /* Shadow roots do not inherit the document's box-sizing reset, so an input
     with width:100% and padding overflows its own dialog. */
  *, *::before, *::after { box-sizing: border-box; }

  :host { display: inline-flex; }
  button {
    position: relative; width: 34px; height: 19px; flex: none; padding: 0;
    border-radius: var(--radius-pill); border: 1px solid var(--border-hi);
    background: var(--bg); cursor: pointer;
    transition: background .14s, border-color .14s;
  }
  button::after {
    content: ""; position: absolute; top: 2px; left: 2px;
    width: 13px; height: 13px; border-radius: 50%;
    background: var(--faint); transition: transform .14s, background .14s;
  }
  button[aria-checked="true"] { background: var(--accent-weak); border-color: var(--accent); }
  button[aria-checked="true"]::after { transform: translateX(15px); background: var(--accent); }
  button:disabled { opacity: .45; cursor: not-allowed; }
  button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  @media (prefers-reduced-motion: reduce) {
    button, button::after { transition: none; }
  }
`;

@component("fkit-toggle")
@styles(sheet)
export class FkitToggle extends LoomElement {
  @prop accessor checked = false;
  @prop accessor disabled = false;
  @prop accessor label = "";

  update() {
    return (
      <button
        type="button"
        role="switch"
        aria-checked={this.checked ? "true" : "false"}
        aria-label={this.label || undefined}
        disabled={this.disabled}
        onClick={() => {
          if (this.disabled) return;
          this.dispatchEvent(
            new CustomEvent("toggle", {
              detail: !this.checked,
              bubbles: true,
              composed: true,
            }),
          );
        }}
      ></button>
    );
  }
}
