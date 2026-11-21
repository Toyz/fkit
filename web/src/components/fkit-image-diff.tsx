/**
 * Two versions of a picture, compared.
 *
 * A line diff has nothing to say about an image — "binary file" is the whole
 * of it — and yet "what changed in this screenshot" is a question with an
 * obvious answer the moment both are on screen. So this puts them there, three
 * ways, because the three answer different questions:
 *
 *   side by side   what are these two pictures
 *   swipe          where, exactly, do they differ
 *   onion          did anything move
 *
 * Either side may be absent. A file that was added has no before and a file
 * that was removed has no after; those draw the one picture there is.
 *
 * Both are shown at the same scale in the overlaid modes — one pixel of the
 * old image is as large as one pixel of the new. Scaling each to fill the
 * frame is the tempting thing and the wrong one: a logo that was resized from
 * 64 to 128 would then look identical in both, which is precisely the change.
 */
import { LoomElement, component, css, styles, prop, reactive } from "@toyz/loom";
import { humanSize } from "../api";

type Mode = "side" | "swipe" | "onion";
type Side = "before" | "after";

const MODES: { key: Mode; label: string }[] = [
  { key: "side", label: "side by side" },
  { key: "swipe", label: "swipe" },
  { key: "onion", label: "onion" },
];

/**
 * The mode last chosen, for the next comparison that opens.
 *
 * Someone reviewing a change to forty icons picks swipe once. Making them pick
 * it forty times is the kind of thing nobody reports and everybody feels.
 */
let lastMode: Mode = "side";

const sheet = css`
  *, *::before, *::after { box-sizing: border-box; }
  :host { display: block; font-family: var(--mono); font-size: 12px; color: var(--text); }

  /* ---- the mode bar ---- */
  .bar {
    display: flex; align-items: center; gap: 12px;
    padding: 5px 10px; border-bottom: 1px solid var(--border);
  }
  .seg { display: flex; border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden; }
  .seg button {
    font: inherit; font-size: 11.5px; height: 22px; padding: 0 9px;
    border: 0; background: transparent; color: var(--muted); cursor: pointer;
  }
  .seg button + button { border-left: 1px solid var(--border); }
  .seg button:hover { background: var(--raised); color: var(--text); }
  .seg button.on { background: var(--raised); color: var(--text); }
  .seg button:focus-visible { outline: 1px solid var(--accent); outline-offset: -1px; }
  .bar .grow { flex: 1; }
  .bar .delta { color: var(--faint); font-size: 11px; font-variant-numeric: tabular-nums; }

  /* ---- where a picture sits ----
     A chequerboard, so transparency reads as transparency rather than as
     whatever the page background happens to be. The same one the file view
     uses; a picture should not look different depending on how you got to it. */
  .stage {
    display: grid; place-items: center;
    padding: 20px; min-height: 96px;
    background-color: var(--bg);
    background-image:
      linear-gradient(45deg, var(--raised) 25%, transparent 25%),
      linear-gradient(-45deg, var(--raised) 25%, transparent 25%),
      linear-gradient(45deg, transparent 75%, var(--raised) 75%),
      linear-gradient(-45deg, transparent 75%, var(--raised) 75%);
    background-size: 16px 16px;
    background-position: 0 0, 0 8px, 8px -8px, -8px 0;
  }
  img { display: block; }
  .stage > img {
    max-width: 100%; max-height: 60vh;
    border: 1px solid var(--border);
  }
  .gone { color: var(--faint); font-family: var(--sans); padding: 12px 0; }

  /* ---- side by side ---- */
  .sides { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); }
  .sides figure + figure { border-left: 1px solid var(--border); }
  @media (max-width: 720px) {
    .sides { grid-template-columns: minmax(0, 1fr); }
    .sides figure + figure { border-left: 0; border-top: 1px solid var(--border); }
  }
  figure { margin: 0; display: flex; flex-direction: column; min-width: 0; }
  figure .stage { flex: 1; }

  /* The caption is the one line of a diff this view has: a sigil in the
     colour of the side it describes, then the facts. */
  .cap {
    display: flex; align-items: baseline; gap: 8px;
    padding: 5px 10px; border-top: 1px solid var(--border);
    font-size: 11px; color: var(--muted);
    font-variant-numeric: tabular-nums; white-space: nowrap; overflow: hidden;
  }
  .cap .mk { font-weight: 700; width: 1ch; text-align: center; }
  .cap.before .mk { color: var(--removed); }
  .cap.after .mk { color: var(--added); }
  .cap .dim { color: var(--text); }
  .cap .chg { color: var(--modified); }
  .caps { display: grid; grid-template-columns: 1fr 1fr; }
  .caps .cap + .cap { border-left: 1px solid var(--border); }

  /* ---- overlaid ----
     One frame the size of the larger picture, and a layer for each. The
     frame is sized in CSS from the two natural sizes, so it is right before
     anything is measured and never jumps. */
  .frame {
    position: relative;
    border: 1px solid var(--border);
    touch-action: none; user-select: none;
  }
  .frame.swipe { cursor: ew-resize; }
  .layer { position: absolute; inset: 0; }
  .layer img { position: absolute; top: 0; left: 0; object-fit: contain; }

  /* The cut. A hairline in the accent, with a grip wide enough to find. */
  .cut {
    position: absolute; top: -1px; bottom: -1px; width: 1px;
    background: var(--accent); pointer-events: none;
  }
  .cut::after {
    content: ""; position: absolute; top: 50%; left: -3px;
    width: 7px; height: 22px; margin-top: -11px;
    background: var(--accent); border-radius: var(--radius-sm);
  }

  /* ---- the slider ----
     Drawn, because the native one is the operating system's and matches
     nothing here. A hairline track and a square thumb. */
  .slide {
    display: flex; align-items: center; gap: 10px;
    padding: 6px 10px; border-top: 1px solid var(--border);
    font-size: 11px; color: var(--faint);
  }
  input[type="range"] {
    flex: 1; min-width: 0; height: 14px; margin: 0;
    appearance: none; -webkit-appearance: none; background: transparent; cursor: pointer;
  }
  input[type="range"]::-webkit-slider-runnable-track { height: 1px; background: var(--border-hi); }
  input[type="range"]::-moz-range-track { height: 1px; background: var(--border-hi); }
  input[type="range"]::-webkit-slider-thumb {
    -webkit-appearance: none; appearance: none;
    width: 7px; height: 13px; margin-top: -6px;
    background: var(--accent); border: 0; border-radius: var(--radius-sm);
  }
  input[type="range"]::-moz-range-thumb {
    width: 7px; height: 13px;
    background: var(--accent); border: 0; border-radius: var(--radius-sm);
  }
  input[type="range"]:focus-visible { outline: 1px solid var(--accent); outline-offset: 2px; }
`;

@component("fkit-image-diff")
@styles(sheet)
export class FkitImageDiff extends LoomElement {
  /**
   * Where each side's bytes are. Empty for a side that does not exist.
   *
   * Not called `before` and `after`, which is what they are: every element
   * already has methods by those names, for inserting siblings.
   */
  @prop accessor oldSrc = "";
  @prop accessor newSrc = "";
  @prop accessor oldSize = 0;
  @prop accessor newSize = 0;
  /** The file's path, for alt text. */
  @prop accessor name = "";

  @reactive accessor mode: Mode = lastMode;
  /** 0–100: where the cut is, or how much of the new image shows through. */
  @reactive accessor pos = 50;
  /** Natural sizes, once each has loaded. Keyed by URL, so a new image does
   *  not inherit the measurements of the one it replaced. */
  @reactive accessor dims: Record<string, [number, number]> = {};
  @reactive accessor failed: Record<string, true> = {};

  private dimOf(url: string): [number, number] | null {
    return this.dims[url] ?? null;
  }

  private onLoad(url: string, e: Event) {
    const img = e.target as HTMLImageElement;
    // An SVG with no size of its own reports zero in some browsers. What it
    // was laid out at is the honest answer then: that is the size it is here.
    const w = img.naturalWidth || img.width;
    const h = img.naturalHeight || img.height;
    if (w && h && !this.dims[url]) this.dims = { ...this.dims, [url]: [w, h] };
  }

  private img(url: string, style = "") {
    return (
      <img
        src={url}
        alt={this.name}
        style={style}
        draggable={false}
        onLoad={(e: Event) => this.onLoad(url, e)}
        onError={() => (this.failed = { ...this.failed, [url]: true })}
      />
    );
  }

  private caption(side: Side) {
    const url = side === "before" ? this.oldSrc : this.newSrc;
    const size = side === "before" ? this.oldSize : this.newSize;
    const d = this.dimOf(url);
    const other = this.dimOf(side === "before" ? this.newSrc : this.oldSrc);
    // Only the new side is marked as changed: "it is now this" is the
    // statement, and saying it on both sides says it twice.
    const resized = side === "after" && d && other && (d[0] !== other[0] || d[1] !== other[1]);
    return (
      <div class={`cap ${side}`}>
        <span class="mk">{side === "before" ? "−" : "+"}</span>
        {d ? <span class={`dim ${resized ? "chg" : ""}`}>{`${d[0]} × ${d[1]}`}</span> : null}
        <span>{humanSize(size)}</span>
      </div>
    );
  }

  private pane(side: Side) {
    const url = side === "before" ? this.oldSrc : this.newSrc;
    return (
      <figure>
        <div class="stage">
          {this.failed[url] ? <span class="gone">could not load this image</span> : this.img(url)}
        </div>
        {this.caption(side)}
      </figure>
    );
  }

  /** Move the cut to wherever the pointer is, while it is held down. */
  private drag(e: PointerEvent) {
    if (this.mode !== "swipe") return;
    const frame = e.currentTarget as HTMLElement;
    if (e.type === "pointerdown") frame.setPointerCapture(e.pointerId);
    else if (!frame.hasPointerCapture(e.pointerId)) return;
    const box = frame.getBoundingClientRect();
    if (box.width <= 0) return;
    this.pos = Math.round(Math.min(100, Math.max(0, ((e.clientX - box.left) / box.width) * 100)));
  }

  private overlaid() {
    const a = this.dimOf(this.oldSrc);
    const b = this.dimOf(this.newSrc);
    // Until both have been measured there is no shared scale to draw at. The
    // images are in the document either way — that is what measures them.
    const w = Math.max(a?.[0] ?? 0, b?.[0] ?? 0) || 1;
    const h = Math.max(a?.[1] ?? 0, b?.[1] ?? 0) || 1;
    const ready = !!a && !!b;
    const pct = (n: number, of: number) => `${((n / of) * 100).toFixed(4)}%`;
    const fit = (d: [number, number] | null) =>
      d ? `width:${pct(d[0], w)};height:${pct(d[1], h)}` : "width:100%;height:100%";

    const swipe = this.mode === "swipe";
    // Both layers are clipped in a swipe, not just the top one. With only the
    // new image cut away, a transparent old image shows the new one through
    // itself on the side that is meant to be showing only the old.
    const beforeClip = swipe ? `clip-path:inset(0 ${100 - this.pos}% 0 0)` : "";
    const afterClip = swipe ? `clip-path:inset(0 0 0 ${this.pos}%)` : `opacity:${this.pos / 100}`;

    return (
      <>
        <div class="stage">
          <div
            class={`frame ${this.mode}`}
            style={
              `width:min(100%, ${w}px, calc(60vh * ${(w / h).toFixed(5)}));` +
              `aspect-ratio:${w} / ${h};` +
              (ready ? "" : "visibility:hidden")
            }
            onPointerDown={(e: PointerEvent) => this.drag(e)}
            onPointerMove={(e: PointerEvent) => this.drag(e)}
          >
            <div class="layer" style={beforeClip}>{this.img(this.oldSrc, fit(a))}</div>
            <div class="layer" style={afterClip}>{this.img(this.newSrc, fit(b))}</div>
            {swipe ? <div class="cut" style={`left:${this.pos}%`}></div> : null}
          </div>
        </div>
        <div class="slide">
          <span>{swipe ? "before" : "0%"}</span>
          <input
            type="range"
            min="0"
            max="100"
            value={String(this.pos)}
            aria-label={swipe ? "Position of the cut between before and after" : "Opacity of the new image"}
            onInput={(e: Event) => (this.pos = Number((e.target as HTMLInputElement).value))}
          />
          <span>{swipe ? "after" : "100%"}</span>
        </div>
        <div class="caps">
          {this.caption("before")}
          {this.caption("after")}
        </div>
      </>
    );
  }

  update() {
    // One side only: nothing to compare, so no modes — just the picture.
    if (!this.oldSrc || !this.newSrc) {
      return this.pane(this.oldSrc ? "before" : "after");
    }

    const grew = this.newSize - this.oldSize;
    // A real boolean, not "true or undefined". Loom writes anything that is
    // not a boolean as an attribute's text, and disabled="undefined" is
    // still disabled — which switched off all three buttons.
    const broken = !!(this.failed[this.oldSrc] || this.failed[this.newSrc]);
    // Overlaying needs both pictures; if one will not load, show what there is.
    const mode: Mode = broken ? "side" : this.mode;

    return (
      <>
        <div class="bar">
          <div class="seg" role="group" aria-label="How to compare the two images">
            {MODES.map((m) => (
              <button
                type="button"
                class={mode === m.key ? "on" : ""}
                aria-pressed={mode === m.key ? "true" : "false"}
                disabled={broken && m.key !== "side"}
                onClick={() => {
                  this.mode = lastMode = m.key;
                  this.pos = 50;
                }}
              >
                {m.label}
              </button>
            ))}
          </div>
          <span class="grow"></span>
          <span class="delta">
            {grew === 0
              ? "same size"
              : `${grew > 0 ? "+" : "−"}${humanSize(Math.abs(grew))}`}
          </span>
        </div>
        {mode === "side" ? (
          <div class="sides">
            {this.pane("before")}
            {this.pane("after")}
          </div>
        ) : (
          this.overlaid()
        )}
      </>
    );
  }
}
