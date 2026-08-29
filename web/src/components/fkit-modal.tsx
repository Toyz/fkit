/**
 * A modal that holds whatever you put in it.
 *
 * `fkit-dialog` answers a question and returns a promise; this one hosts a
 * form. They share a look on purpose — the scrim, the box, the shadow — so
 * that "something is in front of the page" means one thing in this app rather
 * than two.
 *
 * Open is a prop rather than an imperative call, because the thing that knows
 * whether the form should be up is the page's own state, and a modal with its
 * own idea of that is a modal that reopens after a successful save.
 */
import { LoomElement, component, css, styles, prop, on } from "@toyz/loom";
import { buttons } from "../ui";

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }

  :host { display: none; }
  :host([open]) { display: block; position: fixed; inset: 0; z-index: 190; }

  .scrim {
    position: absolute; inset: 0;
    background: color-mix(in srgb, #000 62%, transparent);
    backdrop-filter: blur(1.5px);
  }
  .box {
    position: relative;
    margin: 9vh auto 0;
    width: min(var(--w, 620px), calc(100vw - 32px));
    max-height: 82vh;
    display: flex; flex-direction: column;
    background: var(--surface);
    border: 1px solid var(--border-hi); border-radius: var(--radius);
    font-family: var(--mono);
    box-shadow: 0 18px 48px rgb(0 0 0 / .45);
    overflow: hidden;
  }

  header {
    display: flex; align-items: center; gap: 10px;
    padding: 11px 14px; border-bottom: 1px solid var(--border);
    font-size: 13px; font-weight: 600; color: var(--text);
  }
  header .grow { flex: 1; }
  .x {
    border: 0; background: transparent; color: var(--faint);
    padding: 2px; border-radius: var(--radius); cursor: pointer;
    display: flex;
  }
  .x:hover { background: var(--raised); color: var(--text); border-color: transparent; }

  /* The body scrolls, the header and footer do not — a long form should not
     push its own submit button off the bottom of the screen. */
  .body { padding: 15px; overflow: auto; }
  footer {
    display: flex; align-items: center; justify-content: flex-end; gap: 8px;
    padding: 10px 14px; border-top: 1px solid var(--border);
    background: var(--raised);
  }
  footer .grow { flex: 1; }
`;

@component("fkit-modal")
@styles(buttons, sheet)
export class FkitModal extends LoomElement {
  @prop accessor open = false;
  @prop accessor heading = "";
  /** Width of the box, for a form that wants more or less room. */
  @prop accessor width = "";

  private close() {
    this.dispatchEvent(new CustomEvent("close", { bubbles: true }));
  }

  /** Escape closes it, wherever the focus happens to be. Not private: the
   *  decorator is what calls it, and nothing in the class does. */
  @on(document, "keydown")
  key(e: KeyboardEvent) {
    if (this.open && e.key === "Escape") {
      e.preventDefault();
      this.close();
    }
  }

  update() {
    if (!this.open) return <></>;
    return (
      <>
        <div class="scrim" onClick={() => this.close()}></div>
        <div class="box" style={this.width ? `--w:${this.width}` : ""}>
          <header>
            <span>{this.heading}</span>
            <span class="grow"></span>
            <button type="button" class="x" aria-label="Close" onClick={() => this.close()}>
              <loom-icon name="x" size={13}></loom-icon>
            </button>
          </header>
          <div class="body"><slot></slot></div>
          <footer><slot name="footer"></slot></footer>
        </div>
      </>
    );
  }
}
