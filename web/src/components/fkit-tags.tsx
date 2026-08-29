/**
 * A topic input that behaves like the thing it edits.
 *
 * Topics were a text field holding "rust, version-control, merkle" — a list
 * pretending to be a sentence, where a typo in the separator silently made one
 * topic out of two and there was no way to remove the third without counting
 * commas. Here each topic is an object: it exists, it is visible, and it has
 * an x.
 *
 * The chip is deliberately not a coloured pill. A topic in fkit is a name in a
 * namespace, so it is set in the mono face and lowercased on entry, the same
 * way a branch or a path renders everywhere else in the app. Uniform casing is
 * also the only thing that makes duplicates detectable.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";

/** Letters, digits, hyphen and dot — what the server will accept. */
const LEGAL = /[^a-z0-9.-]+/g;

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }

  /* The whole control is one field: the chips live inside the border rather
     than above it, so it reads as an input that happens to contain topics. */
  .box {
    display: flex; flex-wrap: wrap; align-items: center; gap: 5px;
    min-height: 30px; padding: 4px 6px;
    background: var(--bg);
    border: 1px solid var(--border); border-radius: var(--radius);
    cursor: text;
  }
  .box:focus-within { border-color: var(--accent); }
  :host([disabled]) .box { opacity: .6; cursor: default; }

  .chip {
    display: inline-flex; align-items: center; gap: 5px;
    padding: 1px 3px 1px 8px; line-height: 18px;
    font-family: var(--mono); font-size: 11px;
    color: var(--text); background: var(--raised);
    border: 1px solid var(--border-hi); border-radius: var(--radius-pill);
  }
  .chip button {
    display: flex; align-items: center; justify-content: center;
    width: 15px; height: 15px; padding: 0; border: 0; border-radius: var(--radius-pill);
    background: transparent; color: var(--faint); cursor: pointer;
  }
  .chip button:hover { background: var(--removed); color: var(--bg); }

  input {
    flex: 1; min-width: 90px;
    padding: 2px 4px; margin: 0;
    background: transparent; border: 0; outline: 0;
    font-family: var(--mono); font-size: 12px; color: var(--text);
  }
  input::placeholder { color: var(--faint); }

  .count { font-size: 11px; color: var(--faint); padding-right: 4px; }
`;

@component("fkit-tags")
@styles(sheet)
export class FkitTags extends LoomElement {
  @prop accessor value: string[] = [];
  @prop accessor placeholder = "";
  @prop accessor max = 20;
  @prop accessor disabled = false;

  private commit(next: string[]) {
    this.value = next;
    this.dispatchEvent(new CustomEvent("change", { detail: next, bubbles: true }));
  }

  /** Accepts one entry or several at once — a paste is just a long entry. */
  private add(raw: string) {
    const parts = raw
      .toLowerCase()
      .split(/[,\s]+/)
      .map((t) => t.replace(LEGAL, ""))
      .filter(Boolean);
    if (!parts.length) return;
    const next = [...this.value];
    for (const t of parts) {
      if (next.length >= this.max) break;
      if (!next.includes(t)) next.push(t);
    }
    if (next.length !== this.value.length) this.commit(next);
  }

  private key(e: KeyboardEvent) {
    const el = e.target as HTMLInputElement;
    if (e.key === "Enter" || e.key === "," || e.key === " " || e.key === "Tab") {
      // Tab only commits when there is something to commit, so an empty field
      // still moves focus on. Enter must not reach the form, or adding a topic
      // would submit the page.
      if (!el.value.trim() && e.key === "Tab") return;
      e.preventDefault();
      this.add(el.value);
      el.value = "";
      return;
    }
    // Backspace on an empty field takes back the last topic — the standard
    // gesture, and the reason a chip needs no confirmation to remove.
    if (e.key === "Backspace" && !el.value && this.value.length) {
      e.preventDefault();
      this.commit(this.value.slice(0, -1));
    }
  }

  /** Leaving the field commits what is in it; nobody expects a half-typed
   *  topic to vanish because they clicked Save. */
  private leave(e: Event) {
    const el = e.target as HTMLInputElement;
    if (!el.value.trim()) return;
    this.add(el.value);
    el.value = "";
  }

  update() {
    const full = this.value.length >= this.max;
    return (
      <div
        class="box"
        onClick={(e: Event) => {
          if ((e.target as HTMLElement).closest("button")) return;
          this.shadowRoot?.querySelector("input")?.focus();
        }}
      >
        {this.value.map((t) => (
          <span class="chip" loom-key={t}>
            {t}
            <button
              type="button"
              aria-label={`Remove ${t}`}
              disabled={this.disabled}
              onClick={() => this.commit(this.value.filter((x) => x !== t))}
            >
              <loom-icon name="x" size={9}></loom-icon>
            </button>
          </span>
        ))}
        {full ? (
          <span class="count">{this.max} is the limit</span>
        ) : (
          <input
            type="text"
            disabled={this.disabled}
            placeholder={this.value.length ? "" : this.placeholder}
            autocomplete="off"
            spellcheck={false}
            onKeyDown={(e: Event) => this.key(e as KeyboardEvent)}
            onBlur={(e: Event) => this.leave(e)}
          />
        )}
      </div>
    );
  }
}
