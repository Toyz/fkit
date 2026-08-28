/**
 * Branch picker.
 *
 * A native `<select>` cannot show a hash, a timestamp, or a filter box, and it
 * renders as an OS widget that ignores the page's type and colour entirely. On
 * a page whose whole point is dense structured data, that is the one control
 * that looks borrowed. So this is a real listbox: filterable, keyboard-driven,
 * and styled like everything around it.
 *
 * Tags live in here too, behind a Branches/Tags switch, which is where people
 * already look for them: a tag is a thing you check out, so it belongs in the
 * control for choosing what to look at rather than in a tab of its own.
 *
 * Emits a `pick` CustomEvent carrying the chosen ref name.
 */
import { LoomElement, component, css, styles, reactive, prop, mount, query } from "@toyz/loom";
import type { Ref } from "../api";

const sheet = css`
  /* Shadow roots do not inherit the document's box-sizing reset, so an input
     with width:100% and padding overflows its own dialog. */
  *, *::before, *::after { box-sizing: border-box; }

  :host { position: relative; display: inline-block; font-family: var(--mono); }

  .trigger {
    display: inline-flex; align-items: center; gap: 6px;
    font: inherit; font-size: 12px;
    padding: 4px 8px;
    border: 1px solid var(--border-hi); border-radius: var(--radius);
    background: transparent; color: var(--text); cursor: pointer;
    max-width: 240px;
  }
  .trigger:hover { background: var(--raised); border-color: var(--faint); }
  .trigger .nm { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .trigger loom-icon { color: var(--muted); flex: none; }
  .trigger .chev { transition: transform .12s; }
  .trigger.open .chev { transform: rotate(180deg); }

  .pop {
    position: absolute; top: calc(100% + 4px); right: 0; z-index: 40;
    width: 280px;
    background: var(--surface);
    border: 1px solid var(--border-hi);
    border-radius: var(--radius);
    overflow: hidden;
  }
  .pop:focus-within { border-color: var(--accent); }
  /* The filter is part of the popover, not a widget sitting inside it: no box
     of its own, full bleed to the edges, and only the hairline below separating
     it from the list. */
  .search {
    display: flex; align-items: center; gap: 7px;
    padding: 0 10px; height: 32px;
    border-bottom: 1px solid var(--border);
    color: var(--faint);
  }
  .search loom-icon { flex: none; }
  .search input {
    flex: 1;
    font: inherit; font-size: 12px;
    border: 0; background: transparent; color: var(--text);
    padding: 0; height: 100%;
    border-radius: 0;
  }
  .search input:focus { outline: none; border: 0; box-shadow: none; }
  .search input::placeholder { color: var(--faint); }
  /* Focus is shown by the popover border rather than a ring around the field,
     which would re-introduce the box this is trying to avoid. */
  .search:focus-within { color: var(--accent); }

  /* Branches | Tags. Both visible, so the one you are not on is readable. */
  .modes { display: flex; border-bottom: 1px solid var(--border); }
  .modes button {
    flex: 1; font: inherit; font-size: 11.5px; height: 28px;
    background: transparent; border: 0; border-bottom: 2px solid transparent;
    color: var(--muted); cursor: pointer;
  }
  .modes button:hover { color: var(--text); background: var(--raised); }
  .modes button.on { color: var(--text); border-bottom-color: var(--accent); }
  .modes .n { color: var(--faint); margin-left: 5px; font-variant-numeric: tabular-nums; }

  .list { max-height: 300px; overflow-y: auto; padding: 3px 0; }
  .opt {
    display: grid; grid-template-columns: 14px minmax(0, 1fr) auto;
    align-items: center; gap: 8px;
    padding: 5px 9px; cursor: pointer; font-size: 12px;
  }
  .opt:hover, .opt.active { background: var(--raised); }
  .opt .tick { color: var(--accent); display: flex; }
  .opt .nm { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .opt.on .nm { color: var(--accent); }
  .opt .sha { color: var(--faint); font-size: 11px; font-variant-numeric: tabular-nums; }
  .none { padding: 12px; color: var(--faint); font-size: 12px; text-align: center; }
`;

@component("branch-picker")
@styles(sheet)
export class BranchPicker extends LoomElement {
  @prop accessor refs: Ref[] = [];
  @prop accessor tags: Ref[] = [];
  @prop accessor current = "";

  @reactive accessor open = false;
  @reactive accessor filter = "";
  @reactive accessor active = 0;
  @reactive accessor mode: "branches" | "tags" = "branches";

  @query(".pop input") accessor searchBox!: HTMLInputElement | null;

  @mount
  init() {
    // Dismiss on any pointer press outside this element.
    //
    // Three details matter here. It listens for `pointerdown` rather than
    // `click`, so a press that lands on something which re-renders still closes
    // the popover. It listens in the *capture* phase, so it runs regardless of
    // whether anything downstream stops propagation — relying on bubbling made
    // this get stuck open. And it tests `composedPath`, which is the only thing
    // that sees through shadow roots, so presses inside our own popover are
    // correctly ignored.
    const away = (e: Event) => {
      if (this.open && !e.composedPath().includes(this)) this.open = false;
    };
    document.addEventListener("pointerdown", away, true);
    return () => document.removeEventListener("pointerdown", away, true);
  }

  private source(): Ref[] {
    return this.mode === "tags" ? this.tags : this.refs;
  }

  private filtered(): Ref[] {
    const q = this.filter.trim().toLowerCase();
    const src = this.source();
    return q ? src.filter((r) => r.name.toLowerCase().includes(q)) : src;
  }

  private setMode(mode: "branches" | "tags") {
    this.mode = mode;
    this.filter = "";
    this.active = 0;
    requestAnimationFrame(() => this.searchBox?.focus());
  }

  private choose(name: string) {
    this.open = false;
    this.filter = "";
    if (name !== this.current) {
      this.dispatchEvent(new CustomEvent("pick", { detail: name, bubbles: true, composed: true }));
    }
  }

  private toggle() {
    this.open = !this.open;
    this.active = 0;
    // Open on whichever list the current ref came from, so the thing you are
    // looking at is the thing you see checked.
    if (this.open) {
      this.mode = this.tags.some((t) => t.name === this.current) ? "tags" : "branches";
      // Focus after the popover exists in the DOM.
      requestAnimationFrame(() => this.searchBox?.focus());
    }
  }

  private onKey(e: KeyboardEvent) {
    const list = this.filtered();
    if (e.key === "Escape") {
      this.open = false;
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      this.active = Math.min(this.active + 1, list.length - 1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      this.active = Math.max(this.active - 1, 0);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const hit = list[this.active];
      if (hit) this.choose(hit.name);
    }
  }

  update() {
    const list = this.filtered();
    return (
      <div>
        <button
          class={`trigger ${this.open ? "open" : ""}`}
          onClick={() => this.toggle()}
          title="Switch branch"
        >
          <loom-icon
            name={this.tags.some((t) => t.name === this.current) ? "tag" : "branch"}
            size={13}
          ></loom-icon>
          <span class="nm">{this.current || "—"}</span>
          <loom-icon class="chev" name="chevron" size={12}></loom-icon>
        </button>

        {this.open ? (
          <div class="pop">
            {this.tags.length > 0 ? (
              <div class="modes">
                {(["branches", "tags"] as const).map((m) => (
                  <button
                    class={this.mode === m ? "on" : ""}
                    onClick={() => this.setMode(m)}
                  >
                    {m}
                    <span class="n">{m === "tags" ? this.tags.length : this.refs.length}</span>
                  </button>
                ))}
              </div>
            ) : null}
            <div class="search">
              <loom-icon name="search" size={12}></loom-icon>
              <input
                placeholder={`filter ${this.mode}`}
                value={this.filter}
                onInput={(e: Event) => {
                  this.filter = (e.target as HTMLInputElement).value;
                  this.active = 0;
                }}
                onKeyDown={(e: Event) => this.onKey(e as KeyboardEvent)}
              />
            </div>
            <div class="list">
              {list.length === 0 ? (
                <div class="none">
                  {this.mode === "tags" ? "no tag matches" : "no branch matches"}
                </div>
              ) : (
                list.map((r, i) => (
                  <div
                    class={`opt ${r.name === this.current ? "on" : ""} ${i === this.active ? "active" : ""}`}
                    onClick={() => this.choose(r.name)}
                    onMouseEnter={() => (this.active = i)}
                  >
                    <span class="tick">
                      {r.name === this.current ? (
                        <loom-icon name="check" size={13}></loom-icon>
                      ) : null}
                    </span>
                    <span class="nm">{r.name}</span>
                    <span class="sha">{r.short}</span>
                  </div>
                ))
              )}
            </div>
          </div>
        ) : null}
      </div>
    );
  }
}
