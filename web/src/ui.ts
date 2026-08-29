/**
 * Shared style fragments and the icon set.
 *
 * Each Loom component owns a shadow root, so these sheets are adopted
 * explicitly with `@styles(base, own)`. Colours come from the `:root` custom
 * properties in styles.css, which do cross shadow boundaries.
 */
import { css } from "@toyz/loom";

export const base = css`
  :host { display: block; font-family: var(--mono); color: var(--text); }
  * { box-sizing: border-box; }

  a { color: var(--accent); text-decoration: none; }
  a:hover { text-decoration: underline; text-underline-offset: 2px; }

  .muted { color: var(--muted); }
  .faint { color: var(--faint); }
  /* Prose opts out of the monospace default. */
  .prose { font-family: var(--sans); }

  .row    { display: flex; align-items: center; gap: 8px; }
  .spread { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .stack  { display: flex; flex-direction: column; }
  .grow   { flex: 1; min-width: 0; }

  /* Wide, left-anchored. A file browser wants horizontal room, not a column. */
  .wrap { max-width: 1440px; margin: 0 auto; padding: 0 18px; }

  /* Controls that ride a section heading rather than belonging to a field —
     a filter over the list below it, a segmented state picker, the button
     that adds to it. Defined here because three pages wanted it and three
     copies is how they drift apart. */
  .head-acts {
    display: flex; align-items: center; gap: 9px;
    flex: none; white-space: nowrap;
  }
  .head-acts .btn { font-size: 11.5px; }
  .head-acts input { width: 170px; font-size: 12px; height: 24px; padding: 0 9px; }

  h1, h2, h3 { margin: 0; font-weight: 600; letter-spacing: 0; }
  h1 { font-size: 15px; }
  h2 { font-size: 13px; }

  /* ---- controls ----
     Square-ish, bordered, no fill by default. They read as instrument buttons
     rather than call-to-action buttons, which is what they are. */
  button, .btn {
    font: inherit;
    font-size: 12px;
    padding: 4px 10px;
    border-radius: var(--radius);
    border: 1px solid var(--border-hi);
    background: transparent;
    color: var(--text);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
    transition: background .1s, border-color .1s, color .1s;
  }
  button:hover, .btn:hover { background: var(--raised); border-color: var(--faint); text-decoration: none; }
  button:active { transform: translateY(0.5px); }
  button:disabled { opacity: .45; cursor: not-allowed; }
  button:focus-visible, a:focus-visible, input:focus-visible, select:focus-visible {
    outline: 1px solid var(--accent);
    outline-offset: 1px;
  }

  button.primary, .btn.primary {
    border-color: var(--accent);
    color: var(--accent);
    background: var(--accent-weak);
  }
  button.primary:hover, .btn.primary:hover { background: var(--accent); color: var(--bg); }

  button.danger { border-color: color-mix(in srgb, var(--removed) 50%, transparent); color: var(--removed); }
  button.danger:hover { background: color-mix(in srgb, var(--removed) 12%, transparent); border-color: var(--removed); }

  button.bare, .btn.bare { border-color: transparent; color: var(--muted); padding: 4px 7px; }
  button.bare:hover, .btn.bare:hover {
    background: var(--raised); color: var(--text); border-color: transparent; text-decoration: none;
  }

  input, select, textarea {
    font: inherit;
    font-size: 13px;
    width: 100%;
    padding: 5px 8px;
    border-radius: var(--radius);
    border: 1px solid var(--border-hi);
    background: var(--bg);
    color: var(--text);
  }
  input:focus, select:focus, textarea:focus { outline: none; border-color: var(--accent); }
  input::placeholder { color: var(--faint); }

  label {
    display: block;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: .07em;
    color: var(--muted);
    margin-bottom: 5px;
  }
  .field { margin-bottom: 13px; }

  /* ---- surfaces ----
     A panel is a bordered region, not a floating card: no shadow, no big radius. */
  .panel { background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius); }
  .panel + .panel { margin-top: 12px; }
  .panel-head {
    padding: 6px 12px;
    background: var(--raised);
    border-bottom: 1px solid var(--border);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: .07em;
    color: var(--muted);
    display: flex; align-items: center; justify-content: space-between; gap: 10px;
  }
  /* Labels are upper-cased; values are not. Upper-casing a hex hash or a "KiB"
     suffix makes it read as a different value than it is. */
  .panel-head .val { text-transform: none; letter-spacing: 0; font-size: 12px; }
  .panel-body { padding: 14px; }

  /* A tag, not a pill: square, hairline, lower-case. */
  .tag {
    font-size: 11px;
    padding: 0 5px;
    border: 1px solid var(--border-hi);
    border-radius: var(--radius-sm);
    color: var(--muted);
    line-height: 17px;
    display: inline-block;
  }
  .tag.on { color: var(--accent); border-color: var(--accent); }

  .error {
    border: 1px solid color-mix(in srgb, var(--removed) 45%, transparent);
    border-left-width: 2px;
    color: var(--removed);
    padding: 7px 11px;
    border-radius: var(--radius);
    font-size: 12px;
    margin-bottom: 12px;
  }

  .empty { padding: 40px 18px; text-align: center; color: var(--muted); }
  .empty h2 { color: var(--text); margin-bottom: 6px; }
  .empty p { margin: 0 0 14px; font-family: var(--sans); }

  .loading { padding: 22px 14px; color: var(--faint); font-size: 12px; }
  .loading::before { content: "· · ·  "; }

  /* ---- skeletons ----
     Structural, not generic: a skeleton reproduces the row height and column
     widths of the thing that is coming, so the layout does not jump when real
     content lands. That is the difference between a placeholder that tells you
     something and one that is just a shimmering rectangle.

     The animation is a slow opacity breathe rather than the usual white sheen
     sweeping left to right — quieter, and it does not fight the palette. */
  .sk {
    display: block;
    height: 9px;
    border-radius: var(--radius-sm);
    background: var(--border);
    animation: sk-pulse 1.5s ease-in-out infinite;
  }
  .sk.tall { height: 11px; }
  /* Stagger rows so the list reads as a group settling rather than one flat
     block flashing in unison. */
  .sk-row:nth-child(2n) .sk { animation-delay: .18s; }
  .sk-row:nth-child(3n) .sk { animation-delay: .36s; }

  @keyframes sk-pulse {
    0%, 100% { opacity: .45; }
    50%      { opacity: .9; }
  }
  @media (prefers-reduced-motion: reduce) {
    .sk { animation: none; opacity: .55; }
  }
`;

/**
 * The app's button, for components that render their own.
 *
 * A shadow root inherits custom properties but not rules, so `button` inside
 * one falls back to the browser's default — which is how a Comment button ends
 * up looking like it belongs to a different program. This is the same
 * declaration `base` makes for the light DOM, kept here for the roots that
 * cannot see it.
 */
export const buttons = css`
  button {
    font: inherit; font-size: 12px;
    padding: 4px 10px;
    border-radius: var(--radius);
    border: 1px solid var(--border-hi);
    background: transparent; color: var(--text);
    cursor: pointer;
    display: inline-flex; align-items: center; gap: 6px;
    white-space: nowrap;
    transition: background .1s, border-color .1s, color .1s;
  }
  button:hover { background: var(--raised); border-color: var(--faint); }
  button:active { transform: translateY(0.5px); }
  button:disabled { opacity: .45; cursor: not-allowed; }
  button:disabled:hover { background: transparent; border-color: var(--border-hi); }

  button.primary {
    border-color: var(--accent); color: var(--accent); background: var(--accent-weak);
  }
  button.primary:hover { background: var(--accent); color: var(--bg); }
  button.primary:disabled:hover { background: var(--accent-weak); color: var(--accent); }

  button.bare { border-color: transparent; color: var(--muted); padding: 4px 7px; }
  button.bare:hover { background: var(--raised); color: var(--text); border-color: transparent; }

  button.danger {
    border-color: color-mix(in srgb, var(--removed) 50%, transparent);
    color: var(--removed);
  }
  button.danger:hover {
    background: color-mix(in srgb, var(--removed) 12%, transparent);
    border-color: var(--removed);
  }
`;
