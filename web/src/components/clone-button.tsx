/**
 * The "how do I get this" button.
 *
 * Every repository page needs one, not just an empty one: the clone URL is the
 * single most-copied string in a forge, and it was previously only reachable
 * from the empty state and the settings page.
 */
import { LoomElement, component, css, styles, reactive, prop, mount, query } from "@toyz/loom";

/** Bytes, for a label. Local so the component stays self-contained. */
function size(bytes: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return u === 0 ? `${bytes} B` : `${v.toFixed(1)} ${units[u]}`;
}

const sheet = css`
  /* Shadow roots do not inherit the document's box-sizing reset, so an input
     with width:100% and padding overflows its own dialog. */
  *, *::before, *::after { box-sizing: border-box; }

  :host { position: relative; display: inline-block; font-family: var(--mono); }

  .trigger {
    display: inline-flex; align-items: center; gap: 6px;
    font: inherit; font-size: 12px; padding: 4px 9px;
    border: 1px solid var(--accent); border-radius: var(--radius);
    background: var(--accent-weak); color: var(--accent); cursor: pointer;
  }
  .trigger:hover { background: var(--accent); color: var(--bg); }
  .trigger .chev { transition: transform .12s; }
  .trigger.open .chev { transform: rotate(180deg); }

  .pop {
    position: absolute; top: calc(100% + 4px); right: 0; z-index: 40;
    width: 340px; padding: 11px;
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius);
    display: flex; flex-direction: column; gap: 11px;
  }
  .label {
    display: flex; align-items: baseline; justify-content: space-between; gap: 8px;
    font-size: 10px; text-transform: uppercase; letter-spacing: .07em; color: var(--muted);
  }
  .label button {
    font: inherit; font-size: 11px; padding: 1px 5px;
    border: 1px solid transparent; border-radius: 2px;
    background: transparent; color: var(--muted); cursor: pointer;
  }
  .label button:hover { background: var(--raised); color: var(--text); }
  .val {
    background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius);
    padding: 6px 8px; font-size: 11.5px; color: var(--text);
    overflow-x: auto; white-space: nowrap;
  }
  .dl { padding: 9px 10px; border-top: 1px solid var(--border); }
  .dl .lbl {
    display: block; font-size: 10.5px; color: var(--faint);
    text-transform: uppercase; letter-spacing: .07em; margin-bottom: 7px;
  }
  .dl .lbl .sz { text-transform: none; letter-spacing: 0; margin-left: 7px; color: var(--muted); }
  .dl .fmts { display: flex; gap: 6px; }
  .fmt {
    display: inline-flex; align-items: center; gap: 6px;
    font-size: 11.5px; padding: 3px 9px;
    border: 1px solid var(--border-hi); border-radius: var(--radius);
    color: var(--muted); text-decoration: none;
  }
  .fmt:hover { color: var(--accent); border-color: var(--accent); text-decoration: none; }
  .fmt loom-icon { opacity: .8; }

  .note { color: var(--faint); font-size: 11px; font-family: var(--sans); line-height: 1.45; }
`;

@component("clone-button")
@styles(sheet)
export class CloneButton extends LoomElement {
  @prop accessor url = "";
  @prop accessor visibility = "public";
  /** Base for the archive links, e.g. `/api/repos/helba/fkit/archive/main`. */
  @prop accessor archive = "";
  /** Bytes an archive would hold, and the server's cap. 0 = no cap. */
  @prop accessor archiveBytes = 0;
  @prop accessor archiveLimit = 0;

  @reactive accessor open = false;
  @reactive accessor copied = "";

  @query(".pop") accessor pop!: HTMLElement | null;

  @mount
  init() {
    // Capture-phase pointerdown + composedPath: the only combination that
    // reliably dismisses through shadow roots and regardless of whether
    // anything downstream stops propagation.
    const away = (e: Event) => {
      if (this.open && !e.composedPath().includes(this)) this.open = false;
    };
    document.addEventListener("pointerdown", away, true);
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") this.open = false;
    };
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", esc);
    };
  }

  private async copy(key: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      this.copied = key;
      setTimeout(() => {
        if (this.copied === key) this.copied = "";
      }, 1400);
    } catch {
      /* clipboard blocked; the text is selectable either way */
    }
  }

  private row(key: string, label: string, value: string) {
    return (
      <div>
        <div class="label">
          <span>{label}</span>
          <button onClick={() => void this.copy(key, value)}>
            <loom-icon name={this.copied === key ? "check" : "copy"} size={11}></loom-icon>
            {this.copied === key ? " copied" : " copy"}
          </button>
        </div>
        <div class="val">{value}</div>
      </div>
    );
  }

  /**
   * The download row.
   *
   * When the server would refuse the archive, the links are not offered at
   * all — a button whose only possible outcome is an error is worse than no
   * button, and the reason is stated instead. The check is the same one the
   * server makes, from a size it computed off the tree.
   */
  private download() {
    const over =
      this.archiveLimit > 0 && this.archiveBytes > this.archiveLimit;

    if (over) {
      return (
        <div class="dl">
          <span class="lbl">download</span>
          <div class="note" style="margin:0;padding:0">
            This repository holds {size(this.archiveBytes)}, and this server
            will not build an archive over {size(this.archiveLimit)}. Clone it
            instead — that transfers only what you do not already have.
          </div>
        </div>
      );
    }

    return (
      <div class="dl">
        <span class="lbl">
          download this ref
          {this.archiveBytes > 0 ? <span class="sz">{size(this.archiveBytes)}</span> : null}
        </span>
        <div class="fmts">
          {/* Real links, so middle-click and "save as" behave and the browser
              owns the download. The server streams, so a large repository
              starts arriving immediately rather than after it is built. */}
          {["zip", "tar.gz", "tar"].map((f) => (
            <a class="fmt" href={`${this.archive}.${f}`} download>
              <loom-icon name="archive" size={12}></loom-icon>
              {f}
            </a>
          ))}
        </div>
      </div>
    );
  }

  update() {
    return (
      <div>
        <button class={`trigger ${this.open ? "open" : ""}`} onClick={() => (this.open = !this.open)}>
          <loom-icon name="link" size={12}></loom-icon>
          clone
          <loom-icon class="chev" name="chevron" size={11}></loom-icon>
        </button>

        {this.open ? (
          <div class="pop">
            {this.row("cmd", "clone", `fkit clone ${this.url}`)}
            {this.row("url", "url", this.url)}
            {this.archive ? this.download() : null}
            <div class="note">
              {this.visibility === "public"
                ? "Public — cloning needs no account. Pushing needs a token."
                : "Private — both cloning and pushing need an access token."}
            </div>
          </div>
        ) : null}
      </div>
    );
  }
}
