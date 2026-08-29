/**
 * A way back to the top of a long page.
 *
 * A diff of three thousand lines, a commit touching sixty files, an issue with
 * forty comments: all of them are pages you end up a long way down, with no
 * way back except a scroll that takes as long as the one that got you there.
 *
 * Appears only once there is something to go back to, so it is not a control
 * sitting on every short page waiting to be useful.
 */
import { LoomElement, component, css, styles, reactive, mount } from "@toyz/loom";
import { hotkey } from "@toyz/loom/element";

/** How far down before it is worth offering. About one screen. */
const SHOW_AFTER = 700;

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }

  :host {
    position: fixed; right: 20px; bottom: 20px; z-index: 60;
    /* Not in the layout at all until it is wanted, so it can never be the
       thing that shifts the page. */
    display: none;
  }
  :host([show]) { display: block; }

  button {
    display: flex; align-items: center; gap: 7px;
    padding: 7px 12px;
    border: 1px solid var(--border-hi); border-radius: 999px;
    background: var(--surface); color: var(--muted);
    font: inherit; font-family: var(--mono); font-size: 11.5px;
    cursor: pointer;
    box-shadow: 0 6px 20px rgb(0 0 0 / .32);
  }
  button:hover { color: var(--text); border-color: var(--accent); }
  button loom-icon { transform: rotate(180deg); }

  @media (prefers-reduced-motion: reduce) {
    button { box-shadow: none; }
  }
`;

@component("fkit-to-top")
@styles(sheet)
export class FkitToTop extends LoomElement {
  @reactive accessor show = false;

  @mount
  watchScroll() {
    // Passive: this never calls preventDefault, and saying so keeps it off
    // the critical path of every scroll frame.
    const onScroll = () => {
      const past = window.scrollY > SHOW_AFTER;
      if (past !== this.show) this.show = past;
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    onScroll();
    return () => window.removeEventListener("scroll", onScroll);
  }

  private toTop() {
    // Honours a reduced-motion preference: a long smooth scroll is exactly the
    // kind of movement that setting exists to avoid.
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    window.scrollTo({ top: 0, behavior: reduced ? "auto" : "smooth" });
  }

  /// Not private: the decorator calls it. Home is what people already press.
  @hotkey("home", { global: true, preventDefault: false })
  homeToTop() {
    // Only when nothing is being typed into — Home means "start of line" there.
    const el = document.activeElement;
    const typing =
      el instanceof HTMLInputElement ||
      el instanceof HTMLTextAreaElement ||
      (el as HTMLElement | null)?.isContentEditable;
    if (!typing) this.toTop();
  }

  update() {
    // The attribute drives the `:host([show])` rule above.
    if (this.show) {
      this.setAttribute("show", "");
    } else {
      this.removeAttribute("show");
    }

    return (
      <button type="button" title="Back to top" onClick={() => this.toTop()}>
        <loom-icon name="chevron" size={12}></loom-icon>
        top
      </button>
    );
  }
}
