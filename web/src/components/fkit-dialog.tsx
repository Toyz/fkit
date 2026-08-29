/**
 * A confirmation dialog, replacing `window.confirm`.
 *
 * The native one is an operating-system alert: it cannot be styled, blocks the
 * whole page, and gives no room to say *what* is about to happen — which for a
 * destructive action is exactly the information a person needs.
 *
 * Used through `confirmAction`, which returns a promise, so call sites read the
 * same as the `confirm()` they replace.
 */
import { LoomElement, component, css, styles, reactive } from "@toyz/loom";
import { hotkey } from "@toyz/loom/element";

/** One consequence of going ahead. */
export interface Effect {
  text: string;
  /**
   * What this line costs. `loss` is something that breaks or goes away,
   * `safe` is a reassurance — the thing people most want to know before a
   * destructive-sounding action is what it does *not* touch.
   */
  tone?: "loss" | "safe";
}

export interface ConfirmOptions {
  title: string;
  body?: string;
  /**
   * What will happen, one item per consequence.
   *
   * Preferred over `body` for anything destructive. The same facts written as
   * a paragraph have to be read start to finish to find the one that matters;
   * as a list they can be counted at a glance, which is the whole job of the
   * moment before someone commits to something.
   */
  effects?: Effect[];
  /** Label on the confirming button. */
  confirm?: string;
  /** Style the action as destructive. */
  danger?: boolean;
  /** When set, the exact text the person must type to proceed. */
  typeToConfirm?: string;
}

const sheet = css`
  /* Shadow roots do not inherit the document's box-sizing reset, so an input
     with width:100% and padding overflows its own dialog. */
  *, *::before, *::after { box-sizing: border-box; }

  :host { position: fixed; inset: 0; z-index: 200; display: block; }

  .scrim {
    position: absolute; inset: 0;
    background: color-mix(in srgb, #000 62%, transparent);
    backdrop-filter: blur(1.5px);
  }
  .box {
    position: relative; margin: 14vh auto 0; width: min(420px, calc(100vw - 32px));
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius); font-family: var(--mono);
    box-shadow: 0 18px 48px rgb(0 0 0 / .45);
  }
  .box.danger { border-color: color-mix(in srgb, var(--removed) 55%, transparent); }
  .head {
    padding: 12px 15px; border-bottom: 1px solid var(--border);
    font-size: 13px; font-weight: 600; color: var(--text);
  }
  .box.danger .head { color: var(--removed); }
  .body {
    padding: 14px 15px; font-family: var(--sans); font-size: 13px;
    color: var(--muted); line-height: 1.55;
  }
  .body code { font-family: var(--mono); font-size: 12px; color: var(--text); }

  /* One consequence per line, each opening with the same mark in the same
     column, so the list can be counted without being read. */
  .fx { list-style: none; margin: 0; padding: 0; }
  .fx + input { margin-top: 12px; }
  .fx li {
    display: grid; grid-template-columns: 14px minmax(0, 1fr);
    gap: 8px; align-items: baseline;
    padding: 3px 0; font-size: 12.5px; line-height: 1.5;
  }
  .fx .mk {
    font-family: var(--mono); font-size: 13px; text-align: center;
    /* Not a colour on its own: the marks differ in shape too, so the
       distinction survives being unable to tell the red from the green. */
  }
  .fx li.loss { color: var(--text); }
  .fx li.loss .mk { color: var(--removed); }
  .fx li.safe { color: var(--muted); }
  .fx li.safe .mk { color: var(--accent); }
  .body input {
    width: 100%; margin-top: 10px; font: inherit; font-family: var(--mono); font-size: 12px;
    padding: 6px 9px; border: 1px solid var(--border-hi); border-radius: var(--radius);
    background: var(--bg); color: var(--text);
  }
  .body input:focus { outline: none; border-color: var(--accent); }
  .foot {
    display: flex; justify-content: flex-end; gap: 8px;
    padding: 11px 15px; border-top: 1px solid var(--border);
  }
  button {
    font: inherit; font-family: var(--mono); font-size: 12px; padding: 5px 12px;
    border-radius: var(--radius); border: 1px solid var(--border-hi);
    background: transparent; color: var(--text); cursor: pointer;
  }
  button:hover { background: var(--raised); }
  button.go { border-color: var(--accent); color: var(--accent); background: var(--accent-weak); }
  button.go:hover { background: var(--accent); color: var(--bg); }
  button.go.danger { border-color: var(--removed); color: var(--removed); background: transparent; }
  button.go.danger:hover { background: var(--removed); color: var(--bg); }
  button:disabled { opacity: .45; cursor: not-allowed; }
`;

@component("fkit-dialog")
@styles(sheet)
export class FkitDialog extends LoomElement {
  @reactive accessor opts: ConfirmOptions = { title: "" };
  @reactive accessor typed = "";
  /** Set by `confirmAction`; resolves the caller's promise. */
  resolve: ((ok: boolean) => void) | null = null;

  /// Bound globally rather than to the box. A handler on the element only
  /// fires while focus is inside it, so dismissing depended on focus having
  /// landed there — which is not something a person should have to know.
  ///
  /// Not private: the decorator calls these, and nothing in the class does.
  @hotkey("escape", { global: true })
  cancelOnEscape() {
    this.finish(false);
  }

  /// Enter confirms, but not past a gate that exists precisely to make someone
  /// stop and type the name of the thing they are about to destroy.
  @hotkey("enter", { global: true })
  confirmOnEnter() {
    const o = this.opts;
    const ready = !o.typeToConfirm || this.typed === o.typeToConfirm;
    if (ready) this.finish(true);
  }

  private finish(ok: boolean) {
    this.resolve?.(ok);
    this.resolve = null;
    this.remove();
  }

  update() {
    const o = this.opts;
    const gated = !!o.typeToConfirm;
    const ready = !gated || this.typed === o.typeToConfirm;

    return (
      <div>
        <div class="scrim" onClick={() => this.finish(false)}></div>
        <div class={`box ${o.danger ? "danger" : ""}`} role="dialog" aria-modal="true">
          <div class="head">{o.title}</div>
          {o.body || o.effects?.length || gated ? (
            <div class="body">
              {o.body}
              {o.effects?.length ? (
                <ul class="fx">
                  {o.effects.map((e) => (
                    <li class={e.tone ?? "loss"}>
                      <span class="mk" aria-hidden="true">
                        {e.tone === "safe" ? "+" : "−"}
                      </span>
                      <span>{e.text}</span>
                    </li>
                  ))}
                </ul>
              ) : null}
              {gated ? (
                <input
                  autofocus
                  placeholder={o.typeToConfirm}
                  value={this.typed}
                  onInput={(e: Event) => (this.typed = (e.target as HTMLInputElement).value)}
                />
              ) : null}
            </div>
          ) : null}
          <div class="foot">
            <button onClick={() => this.finish(false)}>cancel</button>
            <button
              class={`go ${o.danger ? "danger" : ""}`}
              disabled={!ready}
              onClick={() => this.finish(true)}
            >
              {o.confirm ?? "confirm"}
            </button>
          </div>
        </div>
      </div>
    );
  }
}

/**
 * Ask for confirmation. Resolves true if the person went ahead.
 *
 * Mounted on `document.body` rather than inside the calling component so the
 * dialog is never clipped by an ancestor's overflow or stacking context.
 */
export function confirmAction(opts: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    const el = document.createElement("fkit-dialog") as FkitDialog;
    el.opts = opts;
    el.resolve = resolve;
    document.body.appendChild(el);
    // Focus the dialog so Escape and Enter work without a click first.
    requestAnimationFrame(() => {
      const focusable = el.shadowRoot?.querySelector("input, button.go") as HTMLElement | null;
      focusable?.focus();
    });
  });
}
