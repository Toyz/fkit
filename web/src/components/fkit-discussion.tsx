/**
 * The pieces a conversation is made of.
 *
 * Shared by issues, by a merge request's conversation, and by a comment
 * pinned to a line of a diff — those are the same object in three places, and
 * three copies of a comment is three chances for one to render differently
 * from the others.
 */
import { LoomElement, component, css, styles, prop, reactive } from "@toyz/loom";
import "./fkit-avatar";

const reset = css`
  *, *::before, *::after { box-sizing: border-box; }
`;

const commentSheet = css`
  :host { display: block; }
  .box {
    border: 1px solid var(--border); border-radius: var(--radius);
    overflow: hidden; background: var(--surface);
  }
  :host([mine]) .box { border-color: color-mix(in srgb, var(--accent) 35%, var(--border)); }

  header {
    display: flex; align-items: center; gap: 8px;
    padding: 7px 12px; background: var(--raised);
    border-bottom: 1px solid var(--border);
    font-size: 12px;
  }
  .who { color: var(--text); font-weight: 600; }
  .when { color: var(--faint); font-size: 11.5px; }
  .grow { flex: 1; }
  .edited { color: var(--faint); font-size: 11px; font-style: italic; }

  .body {
    padding: 11px 12px; font-family: var(--sans); font-size: 13px;
    line-height: 1.55; color: var(--text);
    white-space: pre-wrap; overflow-wrap: anywhere;
  }
  ::slotted([slot="actions"]) { display: flex; gap: 8px; }
`;

/** One comment: who wrote it, when, and what it says. */
@component("fkit-comment")
@styles(reset, commentSheet)
export class FkitComment extends LoomElement {
  @prop accessor author = "";
  @prop accessor when = "";
  @prop accessor body = "";
  @prop accessor edited = false;
  /** Written by the person reading it — worth a quieter mark than a badge. */
  @prop accessor mine = false;

  update() {
    return (
      <div class="box">
        <header>
          <fkit-avatar name={this.author} size={20}></fkit-avatar>
          <span class="who">{this.author || "someone"}</span>
          <span class="when">{this.when}</span>
          {this.edited ? <span class="edited">edited</span> : null}
          <span class="grow"></span>
          <slot name="actions"></slot>
        </header>
        <div class="body">{this.body}</div>
      </div>
    );
  }
}

const composerSheet = css`
  :host { display: block; }
  .box {
    border: 1px solid var(--border); border-radius: var(--radius);
    background: var(--surface); overflow: hidden;
  }
  .box:focus-within { border-color: var(--accent); }

  textarea {
    display: block; width: 100%; border: 0; outline: 0; resize: vertical;
    min-height: var(--rows, 84px);
    padding: 10px 12px; background: transparent; color: var(--text);
    font-family: var(--sans); font-size: 13px; line-height: 1.55;
  }
  textarea::placeholder { color: var(--faint); }

  footer {
    display: flex; align-items: center; gap: 9px;
    padding: 8px 10px; border-top: 1px solid var(--border);
    background: var(--raised);
  }
  .hint { flex: 1; font-size: 11px; color: var(--faint); }
`;

/**
 * The box you write in.
 *
 * Emits `send` with the text. It does not clear itself — the page decides
 * that, after the request it fires has actually succeeded, so a failed post
 * never costs someone what they typed.
 */
@component("fkit-composer")
@styles(reset, composerSheet)
export class FkitComposer extends LoomElement {
  @prop accessor placeholder = "Leave a comment";
  @prop accessor label = "Comment";
  @prop accessor busy = false;
  /** Starts smaller for a line comment, which is usually a sentence. */
  @prop accessor compact = false;
  @reactive accessor text = "";

  private send() {
    const body = this.text.trim();
    if (!body) return;
    this.dispatchEvent(new CustomEvent("send", { detail: body, bubbles: true }));
  }

  /** Called by the page once the post has landed. */
  clear() {
    this.text = "";
    const el = this.shadowRoot?.querySelector("textarea");
    if (el) el.value = "";
  }

  update() {
    return (
      <div class="box" style={this.compact ? "--rows:60px" : ""}>
        <textarea
          placeholder={this.placeholder}
          onInput={(e: Event) => (this.text = (e.target as HTMLTextAreaElement).value)}
          onKeyDown={(e: Event) => {
            // Ctrl/Cmd+Enter sends, because a comment box that submits on
            // Enter cannot hold a paragraph.
            const k = e as KeyboardEvent;
            if (k.key === "Enter" && (k.metaKey || k.ctrlKey)) {
              e.preventDefault();
              this.send();
            }
          }}
        ></textarea>
        <footer>
          <span class="hint">{this.text.trim() ? "Ctrl+Enter to send" : ""}</span>
          <slot name="extra"></slot>
          <button
            class="primary"
            disabled={this.busy || !this.text.trim()}
            onClick={() => this.send()}
          >
            {this.label}
          </button>
        </footer>
      </div>
    );
  }
}
