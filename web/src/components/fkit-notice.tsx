/**
 * Saying that something is wrong, in the two places it needs saying.
 *
 * Inline, when the message belongs beside the thing it is about and the reader
 * can carry on without it — a form that will not submit, a list that failed to
 * load. Modal, when the reader just did something and it did not happen: they
 * are looking at the button they pressed, not at the top of the page where a
 * banner would appear, and an action that silently fails is worse than one
 * that interrupts.
 *
 * One vocabulary of tones across both, so "this is a problem" and "this is a
 * caution" look the same wherever they are said.
 */
import { LoomElement, component, css, styles, prop, reactive } from "@toyz/loom";
import { hotkey } from "@toyz/loom/element";
import { buttons } from "../ui";

/** What kind of thing is being said. */
export type Tone = "error" | "warn" | "info" | "ok";

const ICON: Record<Tone, string> = {
  error: "alert",
  warn: "alert",
  info: "commit",
  ok: "check",
};

/** Each tone's colour, derived from the palette rather than picked. */
const toneVars = css`
  :host([tone="error"]) { --tc: var(--removed); }
  :host([tone="warn"])  { --tc: var(--modified); }
  :host([tone="info"])  { --tc: var(--accent); }
  :host([tone="ok"])    { --tc: var(--added); }
  :host { --tc: var(--removed); }
`;

const noticeSheet = css`
  *, *::before, *::after { box-sizing: border-box; }
  :host { display: block; }
  :host([hidden]) { display: none; }

  .n {
    display: flex; align-items: flex-start; gap: 9px;
    padding: 9px 12px; margin-bottom: 12px;
    border: 1px solid color-mix(in srgb, var(--tc) 40%, transparent);
    border-radius: var(--radius);
    background: color-mix(in srgb, var(--tc) 7%, transparent);
    font-family: var(--sans); font-size: 12.5px; line-height: 1.5;
    color: var(--text);
  }
  .ic { flex: none; margin-top: 1px; color: var(--tc); display: flex; }
  .msg { flex: 1; min-width: 0; overflow-wrap: anywhere; }
  .msg .t { font-weight: 600; }
  .x {
    flex: none; border: 0; background: none; padding: 2px; cursor: pointer;
    color: var(--faint); display: flex; border-radius: var(--radius);
  }
  .x:hover { background: var(--raised); color: var(--text); }
`;

/** A message that sits beside what it is about. */
@component("fkit-notice")
@styles(toneVars, noticeSheet)
export class FkitNotice extends LoomElement {
  @prop accessor tone: Tone = "error";
  /** A short lead, when the body alone would not say what kind of thing it is. */
  @prop accessor title = "";
  @prop accessor message = "";
  /** Offer a dismiss button, for something the reader can simply be done with. */
  @prop accessor dismissible = false;

  update() {
    if (!this.message && !this.title) return <></>;
    return (
      <div class="n" role={this.tone === "error" ? "alert" : "status"}>
        <span class="ic">
          <loom-icon name={ICON[this.tone] ?? "alert"} size={13}></loom-icon>
        </span>
        <span class="msg">
          {this.title ? <span class="t">{this.title} </span> : null}
          {this.message}
        </span>
        {this.dismissible ? (
          <button
            type="button"
            class="x"
            aria-label="Dismiss"
            onClick={() => this.dispatchEvent(new CustomEvent("dismiss", { bubbles: true }))}
          >
            <loom-icon name="x" size={11}></loom-icon>
          </button>
        ) : null}
      </div>
    );
  }
}

// ---- the modal form ------------------------------------------------------

export interface NoticeOptions {
  title: string;
  body?: string;
  tone?: Tone;
  /** Label on the button that closes it. */
  dismiss?: string;
}

const modalSheet = css`
  *, *::before, *::after { box-sizing: border-box; }
  :host { position: fixed; inset: 0; z-index: 210; display: block; }

  .scrim {
    position: absolute; inset: 0;
    background: color-mix(in srgb, #000 62%, transparent);
    backdrop-filter: blur(1.5px);
  }
  /* The same box the confirmation dialog draws, so "something is in front of
     the page" means one thing in this app. */
  .box {
    position: relative; margin: 16vh auto 0;
    width: min(430px, calc(100vw - 32px));
    background: var(--surface);
    border: 1px solid color-mix(in srgb, var(--tc) 45%, var(--border-hi));
    border-radius: var(--radius);
    font-family: var(--mono);
    box-shadow: 0 18px 48px rgb(0 0 0 / .45);
  }
  .head {
    display: flex; align-items: center; gap: 9px;
    padding: 12px 15px; border-bottom: 1px solid var(--border);
    font-size: 13px; font-weight: 600; color: var(--tc);
  }
  .head loom-icon { flex: none; }
  .body {
    padding: 14px 15px; font-family: var(--sans); font-size: 13px;
    color: var(--muted); line-height: 1.55; overflow-wrap: anywhere;
  }
  .foot {
    display: flex; justify-content: flex-end;
    padding: 11px 15px; border-top: 1px solid var(--border);
  }
`;

@component("fkit-notice-modal")
@styles(toneVars, buttons, modalSheet)
export class FkitNoticeModal extends LoomElement {
  @reactive accessor opts: NoticeOptions = { title: "" };
  /** Set by `notify`; resolves the caller's promise. */
  resolve: (() => void) | null = null;

  /// Not private: the decorator is what calls these, and nothing in the class
  /// does.
  ///
  /// Bound globally rather than to the element. A keydown handler on the box
  /// only fires while focus is inside it, so dismissing with Escape depended
  /// on focus having landed there — which it may not have, and which is not
  /// something a person should have to know.
  @hotkey("escape", { global: true })
  dismissOnEscape() {
    this.finish();
  }

  @hotkey("enter", { global: true })
  dismissOnEnter() {
    this.finish();
  }

  private finish() {
    this.resolve?.();
    this.resolve = null;
    this.remove();
  }

  update() {
    const o = this.opts;
    return (
      <div>
        <div class="scrim" onClick={() => this.finish()}></div>
        <div class="box" role="alertdialog" aria-modal="true">
          <div class="head">
            <loom-icon name={ICON[o.tone ?? "error"] ?? "alert"} size={13}></loom-icon>
            {o.title}
          </div>
          {o.body ? <div class="body">{o.body}</div> : null}
          <div class="foot">
            <button type="button" class="primary" onClick={() => this.finish()}>
              {o.dismiss ?? "OK"}
            </button>
          </div>
        </div>
      </div>
    );
  }
}

/**
 * Say something that has to be read before carrying on.
 *
 * Mounted on `document.body` rather than inside the calling component, so it
 * is never clipped by an ancestor's overflow or stacking context — the same
 * reason `confirmAction` does.
 */
export function notify(opts: NoticeOptions): Promise<void> {
  return new Promise((resolve) => {
    const el = document.createElement("fkit-notice-modal") as FkitNoticeModal;
    el.opts = opts;
    el.setAttribute("tone", opts.tone ?? "error");
    el.resolve = resolve;
    document.body.appendChild(el);
    queueMicrotask(() => el.shadowRoot?.querySelector("button")?.focus());
  });
}
