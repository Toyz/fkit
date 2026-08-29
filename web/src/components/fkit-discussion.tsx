/**
 * The pieces a conversation is made of.
 *
 * Shared by issues, by a merge request's conversation, and by a comment
 * pinned to a line of a diff — those are the same object in three places, and
 * three copies of a comment is three chances for one to render differently
 * from the others.
 */
import { LoomElement, component, css, styles, prop, reactive } from "@toyz/loom";
import { buttons } from "../ui";
import { renderMarkdown } from "../markdown";
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
    line-height: 1.6; color: var(--text); overflow-wrap: anywhere;
  }
  /* Markdown, kept modest: a comment is not a document, so headings do not
     get to be twice the size of what is around them. */
  .body > :first-child { margin-top: 0; }
  .body > :last-child { margin-bottom: 0; }
  .body p { margin: 0 0 9px; }
  .body h1, .body h2, .body h3 {
    font-size: 14px; font-weight: 600; margin: 14px 0 6px;
  }
  .body ul, .body ol { margin: 0 0 9px; padding-left: 20px; }
  .body li { margin: 2px 0; }
  .body a { color: var(--accent); }
  .body code {
    font-family: var(--mono); font-size: 12px;
    background: var(--raised); border-radius: 3px; padding: 1px 4px;
  }
  .body pre {
    background: var(--bg); border: 1px solid var(--border);
    border-radius: var(--radius); padding: 9px 11px; overflow-x: auto;
    margin: 0 0 9px;
  }
  .body pre code { background: none; padding: 0; }
  .body blockquote {
    margin: 0 0 9px; padding-left: 11px;
    border-left: 2px solid var(--border-hi); color: var(--muted);
  }
  .body table { border-collapse: collapse; margin: 0 0 9px; font-size: 12px; }
  .body th, .body td { border: 1px solid var(--border); padding: 4px 8px; }
  .body img { max-width: 100%; }
  .body hr { border: 0; border-top: 1px solid var(--border); margin: 12px 0; }
  ::slotted([slot="actions"]) { display: flex; gap: 8px; }
`;

/** One comment: who wrote it, when, and what it says. */
@component("fkit-comment")
@styles(reset, buttons, commentSheet)
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
        {/* The renderer escapes what it does not recognise, which is what
            makes it safe to hand a comment straight to it. */}
        <div class="body" innerHTML={renderMarkdown(this.body)}></div>
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

  /* A toolbar of the four marks people actually reach for, and a preview.
     Anything more is a text editor, which is not what a comment box is. */
  .tools {
    display: flex; align-items: center; gap: 2px;
    padding: 5px 7px; border-bottom: 1px solid var(--border);
    background: var(--raised);
  }
  .tools button {
    height: 22px; min-width: 24px; padding: 0 6px;
    border: 1px solid transparent; background: transparent; color: var(--muted);
    border-radius: 3px; font-size: 11.5px;
    font-family: var(--mono);
  }
  .tools button:hover { background: var(--surface); color: var(--text); border-color: transparent; }
  .tools button:hover { background: var(--surface); color: var(--text); }
  .tools .gap { flex: 1; }
  .tools .tab { font-family: var(--sans); }
  .tools .tab.on { background: var(--surface); color: var(--text); }

  .preview {
    padding: 10px 12px; min-height: var(--rows, 84px);
    font-family: var(--sans); font-size: 13px; line-height: 1.6; color: var(--text);
  }
  .preview .none { color: var(--faint); font-style: italic; }
  .preview p { margin: 0 0 9px; }
  .preview > :last-child { margin-bottom: 0; }
  .preview code {
    font-family: var(--mono); font-size: 12px;
    background: var(--raised); border-radius: 3px; padding: 1px 4px;
  }
  .preview pre {
    background: var(--bg); border: 1px solid var(--border);
    border-radius: var(--radius); padding: 9px 11px; overflow-x: auto;
  }
  .preview a { color: var(--accent); }
`;

/**
 * The box you write in.
 *
 * Emits `send` with the text. It does not clear itself — the page decides
 * that, after the request it fires has actually succeeded, so a failed post
 * never costs someone what they typed.
 */
@component("fkit-composer")
@styles(reset, buttons, composerSheet)
export class FkitComposer extends LoomElement {
  @prop accessor placeholder = "Leave a comment";
  @prop accessor label = "Comment";
  @prop accessor busy = false;
  /** Starts smaller for a line comment, which is usually a sentence. */
  @prop accessor compact = false;
  /** Hide the send button when the surrounding form owns the action. */
  @prop accessor headless = false;
  /** Seeded when editing an existing comment rather than writing a new one. */
  @prop accessor value = "";
  @reactive accessor text = "";
  @reactive accessor previewing = false;
  @reactive accessor ready = false;

  private send() {
    const body = this.text.trim();
    if (!body) return;
    this.dispatchEvent(new CustomEvent("send", { detail: body, bubbles: true }));
  }

  /** Called by the page once the post has landed. */
  clear() {
    this.text = "";
    this.previewing = false;
    const el = this.shadowRoot?.querySelector("textarea");
    if (el) el.value = "";
  }

  /**
   * Wrap the selection, or insert the marks and put the caret between them.
   *
   * Operating on the textarea directly rather than through state, because the
   * selection *is* the thing being acted on and re-rendering would lose it.
   */
  private wrap(before: string, after = before) {
    const el = this.shadowRoot?.querySelector("textarea");
    if (!el) return;
    const { selectionStart: a, selectionEnd: b, value } = el;
    const picked = value.slice(a, b);
    el.value = value.slice(0, a) + before + picked + after + value.slice(b);
    el.focus();
    // With nothing selected the caret lands between the marks, ready to type;
    // with a selection it lands after it, ready to carry on.
    el.selectionStart = a + before.length;
    el.selectionEnd = a + before.length + picked.length;
    this.text = el.value;
  }

  update() {
    // Seeded once: after that the textarea owns its own value, or every
    // keystroke would fight the prop it was initialised from.
    if (!this.ready && this.value) {
      this.text = this.value;
      this.ready = true;
    }

    return (
      <div class="box" style={this.compact ? "--rows:60px" : ""}>
        <div class="tools">
          <button type="button" title="Bold" onClick={() => this.wrap("**")}>B</button>
          <button type="button" title="Italic" onClick={() => this.wrap("_")}>i</button>
          <button type="button" title="Code" onClick={() => this.wrap("`")}>{"</>"}</button>
          <button type="button" title="Link" onClick={() => this.wrap("[", "](url)")}>link</button>
          <span class="gap"></span>
          <button
            type="button"
            class={`tab ${this.previewing ? "" : "on"}`}
            onClick={() => (this.previewing = false)}
          >
            write
          </button>
          <button
            type="button"
            class={`tab ${this.previewing ? "on" : ""}`}
            onClick={() => (this.previewing = true)}
          >
            preview
          </button>
        </div>

        {this.previewing ? (
          this.text.trim() ? (
            <div class="preview" innerHTML={renderMarkdown(this.text)}></div>
          ) : (
            <div class="preview"><span class="none">Nothing to preview yet.</span></div>
          )
        ) : (
        <textarea
          value={this.text}
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
        )}
        <footer>
          <span class="hint">{this.text.trim() ? "Ctrl+Enter to send" : ""}</span>
          <slot name="extra"></slot>
          {this.headless ? null : (
            <button
              type="button"
              class="primary"
              disabled={this.busy || !this.text.trim()}
              onClick={() => this.send()}
            >
              {this.label}
            </button>
          )}
        </footer>
      </div>
    );
  }
}
