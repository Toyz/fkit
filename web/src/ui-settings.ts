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
    gap: 26px;
    align-items: start;
    max-width: 900px;
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

  /* A switch reads as state at a glance; a checkbox reads as a form to fill in. */
  .toggle {
    position: relative; width: 34px; height: 19px; flex: none;
    border-radius: 999px; border: 1px solid var(--border-hi);
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
