/**
 * Shared layout for every settings surface — account, administration, and a
 * repository's own settings.
 *
 * All three are the same shape: a narrow rail of sections beside a single
 * column of panels. Sharing the sheet keeps them from drifting into three
 * slightly different designs.
 */
import { css } from "@toyz/loom";

export const settingsLayout = css`
  .cols {
    display: grid;
    grid-template-columns: 168px minmax(0, 1fr);
    gap: 30px;
    align-items: start;
  }
  @media (max-width: 760px) {
    .cols { grid-template-columns: 1fr; gap: 14px; }
    .rail { display: flex; flex-wrap: wrap; gap: 4px; position: static; }
  }

  .rail { display: flex; flex-direction: column; gap: 1px; position: sticky; top: 52px; }
  .rail h2 {
    font-size: 10px; text-transform: uppercase; letter-spacing: .09em;
    color: var(--faint); margin: 0 0 6px; padding: 0 9px;
  }
  .rail a {
    display: flex; align-items: center; gap: 8px;
    padding: 6px 9px; border-radius: var(--radius);
    color: var(--muted); font-size: 12px; text-decoration: none;
    border-left: 2px solid transparent;
  }
  .rail a:hover { background: var(--raised); color: var(--text); text-decoration: none; }
  .rail a.on { color: var(--text); background: var(--raised); border-left-color: var(--accent); }
  .rail a loom-icon { opacity: .75; }
  .rail a.on loom-icon { color: var(--accent); opacity: 1; }

  .sec { display: flex; flex-direction: column; gap: 12px; }
  .sec > h1 {
    font-size: 14px; font-weight: 600; margin: 0 0 2px;
  }
  /* Choice groups get body padding so their cards are inset from the border. */
  .sec .panel > fkit-choice { display: block; padding: 12px; }

  .sec > .lead {
    color: var(--muted); font-size: 12px; font-family: var(--sans);
    margin: -8px 0 4px; max-width: 62ch; line-height: 1.5;
  }

  /* A row of label + control + explanation, the unit these pages are made of. */
  .field-row {
    display: grid; grid-template-columns: minmax(0, 1fr) auto;
    gap: 14px; align-items: center;
    padding: 11px 14px; border-top: 1px solid var(--border);
  }
  .field-row:first-of-type { border-top: 0; }
  .field-row .fl { font-size: 12.5px; }
  /* Standalone, not scoped to .field-row — the same explanatory line appears
     under inputs and inside panel bodies, and scoping it silently dropped it
     back to monospace everywhere else. */
  .fd {
    color: var(--muted); font-size: 11.5px; font-family: var(--sans);
    margin-top: 3px; line-height: 1.45; max-width: 58ch;
  }
  .field-row .fd { max-width: 52ch; }


  /* ---- settings, as a settings screen -------------------------------
   *
   * One vocabulary for every surface in here, because there were three: a
   * form on one page, a bordered card on another, a bare list on a third.
   *
   *   h1        the page. One per screen, with a rule under it.
   *   .block    a group of related settings, with its own heading and its own
   *             save. Groups are separated by space, not by borders — a border
   *             around everything makes nothing stand out.
   *   .f        one field: label, control, help. The control is as wide as the
   *             value it holds rather than as wide as the page.
   *   .box      a list of things that exist — tokens, sessions, collaborators.
   *             Bordered, because a list has edges; rows separated by hairlines.
   */

  .sec > h1 {
    font-size: 21px; font-weight: 500; letter-spacing: -0.01em;
    margin: 0 0 16px; padding-bottom: 11px;
    border-bottom: 1px solid var(--border);
  }

  .block { margin-bottom: 30px; max-width: 760px; }
  .block:last-child { margin-bottom: 0; }
  .block > h2 {
    font-size: 14px; font-weight: 600; margin: 0 0 3px; color: var(--text);
  }
  .block > .blurb {
    font-size: 12px; color: var(--muted); font-family: var(--sans);
    margin: 0 0 14px; line-height: 1.5; max-width: 66ch;
  }
  .block > h2 + .f, .block > .blurb + .f { margin-top: 0; }

  /* A field is as wide as its value. Full-bleed inputs are what made these
     pages read as unstyled: nothing about a 700px box agrees with a username. */
  .f { margin-bottom: 15px; max-width: 420px; }
  .f.narrow { max-width: 260px; }
  .f.wide { max-width: 620px; }
  .f > label {
    display: block; font-size: 12px; font-weight: 600;
    margin-bottom: 5px; color: var(--text);
  }
  .f > input, .f > select, .f > textarea { width: 100%; }
  .f > .help {
    font-size: 11.5px; color: var(--muted); font-family: var(--sans);
    margin-top: 5px; line-height: 1.45;
  }
  .block .actions { display: flex; align-items: center; gap: 12px; margin-top: 4px; }

  /* ---- a list of things ---- */
  .box {
    border: 1px solid var(--border); border-radius: var(--radius);
    overflow: hidden;
  }
  .box-head {
    display: flex; align-items: center; gap: 10px;
    padding: 9px 14px; background: var(--raised);
    border-bottom: 1px solid var(--border);
    font-size: 12.5px; font-weight: 600;
  }
  .box-head .grow { flex: 1; }
  .box-head .count { font-weight: 400; color: var(--faint); font-size: 11.5px; }
  .box-row {
    display: flex; align-items: center; gap: 14px;
    padding: 11px 14px; border-bottom: 1px solid var(--border);
  }
  .box-row:last-child { border-bottom: 0; }
  .box-row .rmain { flex: 1; min-width: 0; }
  .box-row .rname { font-size: 12.5px; }
  .box-row .rmeta {
    font-size: 11.5px; color: var(--faint); margin-top: 2px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .box-row .ric { color: var(--faint); display: flex; flex: none; }
  .box-row.cur .ric { color: var(--accent); }
  .box-empty { padding: 15px 14px; color: var(--muted); font-size: 12px; }
  /* A form that creates one of these sits in the same box, above the list. */
  .box-form {
    display: flex; align-items: flex-end; gap: 10px;
    padding: 12px 14px; border-bottom: 1px solid var(--border);
  }
  .box-form .f { margin: 0; flex: 1; max-width: none; }

  /* ---- the destructive corner ---- */
  .danger-box {
    border: 1px solid color-mix(in srgb, var(--removed) 40%, transparent);
    border-radius: var(--radius); overflow: hidden;
  }
  .danger-box .box-head {
    background: color-mix(in srgb, var(--removed) 9%, transparent);
    color: var(--removed);
    border-bottom-color: color-mix(in srgb, var(--removed) 28%, transparent);
  }
  .danger-row {
    display: flex; align-items: center; gap: 18px; padding: 13px 14px;
    border-bottom: 1px solid color-mix(in srgb, var(--removed) 22%, transparent);
  }
  .danger-row:last-child { border-bottom: 0; }
  .danger-row .dmain { flex: 1; min-width: 0; }
  .danger-row .dname { font-size: 12.5px; font-weight: 600; }
  .danger-row .dwhy {
    font-size: 11.5px; color: var(--muted); font-family: var(--sans);
    margin-top: 3px; line-height: 1.45;
  }

  /* The toggle in an add-row sits on the control line, not above it. */
  fkit-add .check {
    display: flex; align-items: center; gap: 7px; flex: none;
    font-size: 12px; color: var(--muted); white-space: nowrap;
    height: 30px;
  }
  fkit-add button { flex: none; }

  /* A switch reads as state at a glance; a checkbox reads as a form to fill in. */
  .toggle {
    position: relative; width: 34px; height: 19px; flex: none;
    border-radius: var(--radius-pill); border: 1px solid var(--border-hi);
    background: var(--bg); cursor: pointer; padding: 0;
    transition: background .14s, border-color .14s;
  }
  .toggle::after {
    content: ""; position: absolute; top: 2px; left: 2px;
    width: 13px; height: 13px; border-radius: 50%;
    background: var(--faint); transition: transform .14s, background .14s;
  }
  .toggle.on { background: var(--accent-weak); border-color: var(--accent); }
  .toggle.on::after { transform: translateX(15px); background: var(--accent); }
  .toggle:disabled { opacity: .5; cursor: not-allowed; }

  .stat-grid {
    display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 1px; background: var(--border);
  }
  .stat-cell { background: var(--surface); padding: 11px 13px; }
  .stat-cell b {
    display: block; font-size: 17px; font-weight: 600; color: var(--accent);
    font-variant-numeric: tabular-nums; letter-spacing: -0.02em;
  }
  .stat-cell span {
    display: block; font-size: 10.5px; text-transform: uppercase;
    letter-spacing: .07em; color: var(--muted); margin-top: 3px;
  }

  .danger { border-color: color-mix(in srgb, var(--removed) 35%, transparent); }
  .danger .panel-head span { color: var(--removed); }
`;
