/**
 * The "how do I get this" button.
 *
 * Every repository page needs one, not just an empty one: the clone URL is the
 * single most-copied string in a forge, and it was previously only reachable
 * from the empty state and the settings page.
 */
import { LoomElement, component, css, styles, reactive, prop, mount, query } from "@toyz/loom";
import { debounce, hotkey } from "@toyz/loom/element";

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
    position: absolute; top: calc(100% + 5px); right: 0; z-index: 40;
    width: 370px;
    background: var(--surface); border: 1px solid var(--border-hi);
    border-radius: var(--radius);
    box-shadow: 0 12px 32px rgb(0 0 0 / .38);
    overflow: hidden;
  }

  .sec { padding: 12px 13px; }
  .sec + .sec { border-top: 1px solid var(--border); }

  .lbl {
    display: flex; align-items: center; gap: 8px;
    font-size: 10px; text-transform: uppercase; letter-spacing: .09em;
    color: var(--faint); margin-bottom: 8px;
  }
  .lbl .grow { flex: 1; }
  .lbl .sz { text-transform: none; letter-spacing: 0; color: var(--muted); }

  /* The URL and the command, each in a box with its copy button inside it
     rather than floating above — the button belongs to the value, and the
     value is the reason the popover exists. */
  .field {
    display: flex; align-items: stretch;
    background: var(--bg);
    border: 1px solid var(--border); border-radius: var(--radius);
    overflow: hidden;
  }
  .field:focus-within { border-color: var(--accent); }
  .field .v {
    flex: 1; min-width: 0;
    padding: 7px 9px; font-size: 11.5px; color: var(--text);
    /* Wraps rather than scrolling sideways. A horizontal scrollbar inside a
       340px popover is a way to hide the end of the one string people came
       for. */
    overflow-wrap: anywhere; line-height: 1.45;
  }
  .field .cp {
    flex: none; display: flex; align-items: center; gap: 5px;
    padding: 0 10px; border: 0; border-left: 1px solid var(--border);
    background: var(--raised); color: var(--muted);
    font: inherit; font-size: 11px; cursor: pointer;
  }
  .field .cp:hover { background: var(--surface); color: var(--text); }
  .field .cp.done { color: var(--added); }

  .cmd { margin-top: 9px; }

  .fmts { display: flex; gap: 7px; }
  .fmt {
    display: inline-flex; align-items: center; gap: 6px;
    font-size: 11.5px; padding: 5px 11px;
    border: 1px solid var(--border-hi); border-radius: var(--radius);
    color: var(--muted); text-decoration: none;
  }
  .fmt:hover { color: var(--accent); border-color: var(--accent); text-decoration: none; }
  .fmt loom-icon { opacity: .8; }

  .note {
    padding: 9px 13px; border-top: 1px solid var(--border);
    background: var(--raised);
    color: var(--faint); font-size: 11px; font-family: var(--sans); line-height: 1.45;
  }
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

  /// Copying a second thing restarts this rather than leaving the first
  /// timer to fire against the wrong key, and disconnecting cancels it.
  @debounce(1400)
  clearCopied() {
    this.copied = "";
  }

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
    return () => document.removeEventListener("pointerdown", away, true);
  }

  /// Not private: the decorator calls it, and nothing in the class does.
  @hotkey("escape", { global: true })
  closeOnEscape() {
    this.open = false;
  }

  private async copy(key: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      this.copied = key;
      this.clearCopied();
    } catch {
      /* clipboard blocked; the text is selectable either way */
    }
  }

  /// A value and the button that copies it, as one control.
  private field(key: string, value: string) {
    const done = this.copied === key;
    return (
      <div class="field">
        <span class="v">{value}</span>
        <button
          type="button"
          class={`cp ${done ? "done" : ""}`}
          title={done ? "Copied" : "Copy"}
          onClick={() => void this.copy(key, value)}
        >
          <loom-icon name={done ? "check" : "copy"} size={11}></loom-icon>
          {done ? "copied" : "copy"}
        </button>
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
        <div class="sec">
          <div class="lbl"><span>download</span></div>
          <div style="color:var(--faint);font-size:11px;font-family:var(--sans);line-height:1.45">
            This repository holds {size(this.archiveBytes)}, and this server
            will not build an archive over {size(this.archiveLimit)}. Clone it
            instead — that transfers only what you do not already have.
          </div>
        </div>
      );
    }

    return (
      <div class="sec">
        <div class="lbl">
          <span>download this ref</span>
          <span class="grow"></span>
          {this.archiveBytes > 0 ? <span class="sz">{size(this.archiveBytes)}</span> : null}
        </div>
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
            <div class="sec">
              {/* The URL first, because it is the string people came for. The
                  command is the same URL with four words in front of it, so it
                  is offered second rather than given equal billing. */}
              <div class="lbl"><span>clone this repository</span></div>
              {this.field("url", this.url)}
              <div class="cmd">{this.field("cmd", `fkit clone ${this.url}`)}</div>
            </div>

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
