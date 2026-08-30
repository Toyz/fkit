/**
 * The pieces every settings screen is made of.
 *
 * The shape is GitHub's, because it is the one people already know how to
 * read: a heading with a rule under it, prose sitting directly on the page,
 * bare form fields for things you type, and a border only around a list of
 * rows. Nothing is boxed for decoration — a box means "these rows belong
 * together", and that is all it ever means.
 *
 * These were CSS classes repeated across eight sections, which meant a change
 * to the design meant editing eight places and finding out later which one had
 * been missed. As components the shape lives once and lands everywhere.
 *
 * Note on slots: slotted children stay in the light DOM, so the page that
 * wrote them styles them. Each component styles its own chrome only.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";
import { linkHandler } from "../nav";

/** Shadow roots do not inherit the document reset. */
const reset = css`
  *, *::before, *::after { box-sizing: border-box; }
`;

/**
 * Every heading on these pages is the same heading, and it does two things
 * GitHub's does not.
 *
 * The rule under it is a hairline, except directly beneath the word, where it
 * is the accent. One deliberate mark: the heading underlines itself, and the
 * page stops looking like everyone else's settings page without costing a
 * single pixel of structure.
 *
 * The right end of that rule holds the setting's current value. fkit is a
 * program about addressing things — a commit is its hash, a branch is what it
 * points at — so a settings page should be readable the same way: scroll it
 * and read what the repository *is* right now, without opening a single
 * control. GitHub makes you read every widget to learn the same thing.
 */
const heading = css`
  .h {
    display: flex; align-items: baseline; gap: 12px;
    font-size: 15px; font-weight: 500; letter-spacing: -0.01em;
    color: var(--text); margin: 0 0 14px;
    padding-bottom: 8px; border-bottom: 1px solid var(--border);
  }
  /* The accent segment sits on top of the hairline, exactly as wide as the
     word — hence the inner span; a border on .h could only span the column. */
  .t { position: relative; }
  .t::after {
    content: ""; position: absolute; left: 0; right: 0; bottom: -9px;
    height: 1px; background: var(--accent);
  }
  .fill { flex: 1; }
  /* A control that belongs to the section rather than to any one field —
     a filter over the list below it, say — rides the heading line. */
  ::slotted([slot="action"]) { align-self: center; }
  .v {
    font-family: var(--mono); font-size: 11.5px; font-weight: 400;
    color: var(--faint); white-space: nowrap;
    overflow: hidden; text-overflow: ellipsis; max-width: 40%;
  }
`;

/* ------------------------------------------------------------------ page */

const pageSheet = css`
  :host { display: block; }
`;

/** A settings page. Holds the width; the first heading names the page. */
@component("fkit-page")
@styles(reset, heading, pageSheet)
export class FkitPage extends LoomElement {
  @prop accessor heading = "";
  /** Shown at the right of the rule: what this page currently amounts to. */
  @prop accessor value = "";

  update() {
    return (
      <>
        {this.heading ? (
          <h1 class="h">
            <span class="t">{this.heading}</span>
            <span class="fill"></span>
            {this.value ? <span class="v">{this.value}</span> : null}
          </h1>
        ) : null}
        <slot></slot>
      </>
    );
  }
}

/* --------------------------------------------------------------- section */

const sectionSheet = css`
  :host { display: block; }
  /* Space between sections, none before the first — the page heading's rule
     is already there. */
  :host(:not(:first-of-type)) { margin-top: 30px; }
  p {
    font-size: 12px; color: var(--muted); font-family: var(--sans);
    margin: 0 0 14px; line-height: 1.55;
  }
`;

/** A group of related settings, under its own heading. */
@component("fkit-section")
@styles(reset, heading, sectionSheet)
export class FkitSection extends LoomElement {
  @prop accessor heading = "";
  @prop accessor blurb = "";
  /** The setting's current value, shown at the right of the rule, so the page
   *  can be read for its state without opening anything. */
  @prop accessor value = "";

  update() {
    return (
      <>
        {this.heading ? (
          <h2 class="h">
            <span class="t">{this.heading}</span>
            <span class="fill"></span>
            {this.value ? <span class="v">{this.value}</span> : null}
            <slot name="action"></slot>
          </h2>
        ) : null}
        {this.blurb ? <p>{this.blurb}</p> : null}
        <slot></slot>
      </>
    );
  }
}

/* ----------------------------------------------------------------- field */

const fieldSheet = css`
  /* The column is full width; a control is not. A field is as wide as the
     value it can hold — a username stretched across a thousand pixels looks
     broken, and so does a description crammed into two hundred. The default
     suits a line of text; everything else says what it holds.
   */
  :host { display: block; margin-bottom: 16px; max-width: 520px; }
  :host([size="narrow"]) { max-width: 190px; }
  :host([size="mid"]) { max-width: 340px; }
  :host([size="wide"]) { max-width: 760px; }
  :host([size="full"]) { max-width: none; }

  label {
    display: block; font-size: 12px; font-weight: 600;
    margin-bottom: 6px; color: var(--text);
  }
  .help {
    font-size: 11.5px; color: var(--muted); font-family: var(--sans);
    margin-top: 6px; line-height: 1.45;
  }
  ::slotted(input), ::slotted(select), ::slotted(textarea),
  ::slotted(fkit-select), ::slotted(fkit-tags) {
    width: 100%; display: block;
  }
`;

/** One setting: its label, its control, and what it means. */
@component("fkit-field")
@styles(reset, fieldSheet)
export class FkitField extends LoomElement {
  @prop accessor label = "";
  @prop accessor help = "";
  /** "" | narrow | mid — how wide the value can be. Full width otherwise. */
  @prop accessor size = "";

  update() {
    return (
      <>
        {this.label ? <label>{this.label}</label> : null}
        <slot></slot>
        {this.help ? <div class="help">{this.help}</div> : null}
      </>
    );
  }
}

/* ------------------------------------------------------------------ row */

const rowSheet = css`
  :host { display: flex; align-items: flex-end; gap: 8px; margin-bottom: 16px; }
  ::slotted(fkit-field) { margin-bottom: 0; }
`;

/** Controls that belong on one line — an input and the button that acts on
 *  it, or the fields that add a new row to the list below. */
@component("fkit-add")
@styles(reset, rowSheet)
export class FkitAdd extends LoomElement {
  update() {
    return <slot></slot>;
  }
}

/* ------------------------------------------------------------------ list */

const listSheet = css`
  :host {
    display: block;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    overflow: hidden;
  }
  header {
    display: flex; align-items: center; gap: 10px;
    padding: 8px 14px; background: var(--raised);
    border-bottom: 1px solid var(--border);
    font-size: 12px; font-weight: 600; color: var(--text);
  }
  .grow { flex: 1; }
  .count { font-weight: 400; color: var(--faint); font-size: 11.5px; }
  ::slotted([slot="action"]) { margin: -3px 0; }
`;

/** A bordered list of things that exist. */
@component("fkit-list")
@styles(reset, listSheet)
export class FkitList extends LoomElement {
  @prop accessor heading = "";
  /** Shown on the right of the header. Blank while unknown. */
  @prop accessor count = "";

  update() {
    return (
      <>
        {this.heading ? (
          <header>
            <span>{this.heading}</span>
            <span class="grow"></span>
            {this.count ? <span class="count">{this.count}</span> : null}
            <slot name="action"></slot>
          </header>
        ) : null}
        <slot></slot>
      </>
    );
  }
}

const itemSheet = css`
  :host {
    display: flex; align-items: center; gap: 12px;
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
    /* So a row's primary link can stretch its clickable area over the whole
       row. The link stays a real link — keyboard, middle-click, copy address
       all still work — it simply also covers the space around itself. */
    position: relative;
  }
  :host(:hover) { background: var(--raised); }
  :host(:last-of-type) { border-bottom: 0; }

  .ic { color: var(--faint); display: flex; flex: none; }

  /* The row-wide link, and the one piece of stacking that matters here.
     It has to lie *over* the row's own text: under it, a click on the words —
     the obvious gesture, and most of the row's area — lands on a span and does
     nothing, which is a link that looks clickable everywhere except where you
     would click it. The text is inert, so covering it costs nothing.

     What the page slotted in is not inert. Those ride above the cover, so a
     button in a row is still a button and a link in a slotted body still goes
     where it says. The icon is the exception: it is decoration, so it stays
     under and the row stays clickable there too. */
  .cover { position: absolute; inset: 0; z-index: 1; }
  ::slotted(:not([slot="icon"])) { position: relative; z-index: 2; }
  :host([current]) .ic { color: var(--accent); }
  /* A row whose icon carries a state rather than a kind. Colour and shape
     together: someone who cannot tell the greens from the greys still has the
     glyph. */
  :host([tone="open"]) .ic { color: var(--added); }
  :host([tone="done"]) .ic { color: var(--accent); }
  :host([tone="off"]) .ic { color: var(--muted); }
  /* Stacked, not inline: as plain spans these ran together into
     "ChromeSigned in just now". */
  .main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
  ::slotted([slot="main"]) { flex: 1; min-width: 0; }
  .name { font-size: 12.5px; color: var(--text); }
  .meta {
    font-size: 11.5px; color: var(--faint); margin-top: 2px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
`;

/** One row of a list: what it is, what is true of it, and what you can do. */
@component("fkit-row")
@styles(reset, itemSheet)
export class FkitRow extends LoomElement {
  @prop accessor icon = "";
  @prop accessor name = "";
  @prop accessor meta = "";
  @prop accessor current = false;
  /** "" | open | done | off — what the icon's colour should say. */
  @prop accessor tone = "";
  /**
   * Where the row goes. Set it and the whole row is the link, which is what a
   * reader expects of a row standing for one thing — clicking the words is the
   * obvious gesture, and a ten-character hash is a target nobody can find.
   */
  @prop accessor href = "";

  update() {
    return (
      <>
        {/* Laid over the row rather than wrapped around it: a real link, so
            middle-click and copy-address work, but underneath everything else
            so a button in the row is still a button. */}
        {this.href ? (
          <a
            class="cover"
            href={this.href}
            onClick={linkHandler(this.href)}
            tabIndex={-1}
            aria-hidden="true"
          ></a>
        ) : null}
        {/* An icon says what kind of thing the row is. A row that stands for
            something with an identity of its own — a repository, an account,
            a stash — slots an `fkit-avatar` here instead, so it is told apart
            by the same derived colour it wears everywhere else on the site. */}
        {this.icon ? (
          <span class="ic">
            <loom-icon name={this.icon} size={14}></loom-icon>
          </span>
        ) : (
          <slot name="icon"></slot>
        )}
        {/* A row is usually a name over a line of metadata. When what it
            holds does not fit that — an issue title that is also a link, say —
            the whole middle is slotted instead of the two props being bent
            into a shape they do not have. */}
        {this.name || this.meta ? (
          <span class="main">
            <span class="name">{this.name}</span>
            {this.meta ? <span class="meta">{this.meta}</span> : null}
          </span>
        ) : (
          <slot name="main"></slot>
        )}
        <slot></slot>
      </>
    );
  }
}

const settingRowSheet = css`
  :host {
    display: flex; align-items: center; gap: 20px; padding: 11px 14px;
    border-bottom: 1px solid var(--border);
  }
  :host(:last-of-type) { border-bottom: 0; }
  .main { flex: 1; min-width: 0; }
  .name { display: block; font-size: 12.5px; color: var(--text); }
  .why {
    display: block; font-size: 11.5px; color: var(--muted);
    font-family: var(--sans); margin-top: 3px; line-height: 1.45;
    max-width: 78ch;
  }
`;

/** A setting you flip rather than type: what it is and what it does on the
 *  left, the control that changes it on the right. */
@component("fkit-setting-row")
@styles(reset, settingRowSheet)
export class FkitSettingRow extends LoomElement {
  @prop accessor name = "";
  @prop accessor why = "";

  update() {
    return (
      <>
        <span class="main">
          <span class="name">{this.name}</span>
          {this.why ? <span class="why">{this.why}</span> : null}
        </span>
        <slot></slot>
      </>
    );
  }
}

/** The row a list shows when it has nothing in it. An empty screen is an
 *  instruction, not a shrug — so this takes the text rather than defaulting. */
const emptySheet = css`
  :host { display: block; padding: 14px; color: var(--muted); font-size: 12px; }
`;

@component("fkit-empty")
@styles(reset, emptySheet)
export class FkitEmpty extends LoomElement {
  update() {
    return <slot></slot>;
  }
}

/* ---------------------------------------------------------------- danger */

const dangerSheet = css`
  :host {
    display: block;
    border: 1px solid color-mix(in srgb, var(--removed) 40%, transparent);
    border-radius: var(--radius);
    overflow: hidden;
  }
`;

/** The box around irreversible actions. Red is the only thing that marks it —
 *  the heading above it stays an ordinary heading. */
@component("fkit-danger")
@styles(reset, dangerSheet)
export class FkitDanger extends LoomElement {
  update() {
    return <slot></slot>;
  }
}

const dangerRowSheet = css`
  :host {
    display: flex; align-items: center; gap: 20px; padding: 13px 14px;
    border-bottom: 1px solid color-mix(in srgb, var(--removed) 22%, transparent);
  }
  :host(:last-of-type) { border-bottom: 0; }
  .main { flex: 1; min-width: 0; }
  .name { display: block; font-size: 12.5px; font-weight: 600; color: var(--text); }
  .why {
    display: block; font-size: 11.5px; color: var(--muted);
    font-family: var(--sans); margin-top: 3px; line-height: 1.45;
  }
`;

/** One irreversible action: what it does, and the button that does it. */
@component("fkit-danger-row")
@styles(reset, dangerRowSheet)
export class FkitDangerRow extends LoomElement {
  @prop accessor name = "";
  @prop accessor why = "";

  update() {
    return (
      <>
        <span class="main">
          <span class="name">{this.name}</span>
          {this.why ? <span class="why">{this.why}</span> : null}
        </span>
        <slot></slot>
      </>
    );
  }
}

/* --------------------------------------------------------------- actions */

const actionsSheet = css`
  :host { display: flex; align-items: center; gap: 12px; margin-top: 16px; }
`;

/** The button that commits a section, and whatever it has to say afterwards. */
@component("fkit-actions")
@styles(reset, actionsSheet)
export class FkitActions extends LoomElement {
  update() {
    return <slot></slot>;
  }
}
