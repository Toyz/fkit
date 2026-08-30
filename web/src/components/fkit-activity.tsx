/**
 * A year of somebody's pushes, one square per day.
 *
 * The grid is the expected shape for this and there is no reason to be clever
 * about it -- people can already read one. What is not the expected shape is
 * the colour. Every other forge shades these squares in a single hue, so the
 * grid says how much and never what, and a year of work comes out as one
 * undifferentiated green field.
 *
 * This site already gives every repository a colour of its own, derived from
 * its name and used on its tile everywhere it appears. So a square takes the
 * colour of whatever took most of that day, and depth still means volume. The
 * grid then answers the question you actually have when you look at a year:
 * not "was I busy" but "what was I busy with" -- and a spring spent on one
 * project and a summer spent on another are two visibly different seasons.
 *
 * The legend under it is not decoration: without something naming the hues,
 * the colour is only pretty.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";
import { hueFor } from "../tint";
import { linkHandler } from "../nav";
import type { Activity } from "../api";

/* Depth is relative to this person's own busiest day. A fixed scale would
   render a steady four-commits-a-day year as barely-there and somebody else's
   one-day import as a solid wall, when the honest reading is the reverse. */
const LEVELS = 4;

/* How many projects get a colour of their own.
 *
 * The cap is the whole reason the grid is readable. Tinting every repository
 * sounds more informative and is not: somebody who touches nine projects in a
 * week gets nine hues in a row, and a colour nothing names is only confetti.
 * Five is what the legend can list on one line, so the palette is exactly the
 * set of colours the reader has been told the meaning of, and everything else
 * is honestly the same neutral. */
const NAMED = 5;

/* The neutral for everything outside the top few, and for the depth scale in
   the legend. Blue-grey rather than a hue any repository could land on. */
const OTHER_HUE = 214;

const sheet = css`
  :host { display: block; }

  .head {
    display: flex; align-items: baseline; gap: 8px;
    margin-bottom: 10px; font-size: 12px; color: var(--muted);
  }
  .head b { color: var(--text); font-weight: 500; }
  .head .fill { flex: 1; }

  /* Horizontal scroll rather than a squeeze: fifty-three columns have a
     minimum legible size, and below it the grid should slide under the
     page rather than turn into a smear. */
  .scroll { overflow-x: auto; padding-bottom: 2px; }
  .plot { display: inline-grid; grid-template-columns: auto 1fr; gap: 4px 6px; }

  .months {
    grid-column: 2; display: grid; grid-auto-flow: column;
    font-size: 9.5px; color: var(--faint); height: 11px;
  }
  .months span { grid-row: 1; }

  /* Sunday at the top, and only three labels: seven would be a wall of text
     beside a thing made of eleven-pixel squares. */
  .dows {
    grid-column: 1; grid-row: 2;
    display: grid; grid-template-rows: repeat(7, 11px); gap: 3px;
    font-size: 9.5px; color: var(--faint); text-align: right;
  }
  .dows span { line-height: 11px; }

  .grid {
    grid-column: 2; grid-row: 2;
    display: grid; grid-auto-flow: column;
    grid-template-rows: repeat(7, 11px); gap: 3px;
  }
  .d {
    width: 11px; height: 11px; border-radius: 2px;
    background: var(--raised); box-shadow: inset 0 0 0 1px var(--border);
  }
  /* Depth by saturation and lightness against the day's own hue. The empty
     square keeps the page's neutral, so a quiet week reads as quiet rather
     than as a very pale version of something. */
  .d.l1 { background: hsl(var(--h) 18% 24%); box-shadow: none; }
  .d.l2 { background: hsl(var(--h) 26% 34%); box-shadow: none; }
  .d.l3 { background: hsl(var(--h) 34% 47%); box-shadow: none; }
  .d.l4 { background: hsl(var(--h) 42% 62%); box-shadow: none; }
  /* Outside the window entirely -- the days before the first Sunday and after
     today. Drawn as nothing so the grid stays rectangular without claiming
     those days were empty. */
  .d.off { background: transparent; box-shadow: none; }

  .legend {
    display: flex; flex-wrap: wrap; align-items: center; gap: 4px 14px;
    margin-top: 12px; font-size: 11px; color: var(--faint);
  }
  .legend a {
    display: flex; align-items: center; gap: 5px;
    color: var(--muted); text-decoration: none;
  }
  .legend a:hover { color: var(--text); }
  .legend .sw {
    width: 9px; height: 9px; border-radius: 2px;
    background: hsl(var(--h) 40% 48%);
  }
  .legend .fill { flex: 1; }
  .scale { display: flex; align-items: center; gap: 3px; }
  .scale .d { width: 9px; height: 9px; }
  .scale .d.n1 { background: hsl(var(--gh) 18% 24%); box-shadow: none; }
  .scale .d.n2 { background: hsl(var(--gh) 26% 34%); box-shadow: none; }
  .scale .d.n3 { background: hsl(var(--gh) 34% 47%); box-shadow: none; }
  .scale .d.n4 { background: hsl(var(--gh) 42% 62%); box-shadow: none; }

  @media (prefers-color-scheme: light) {
    .d.l1 { background: hsl(var(--h) 26% 89%); }
    .d.l2 { background: hsl(var(--h) 30% 76%); }
    .d.l3 { background: hsl(var(--h) 34% 60%); }
    .d.l4 { background: hsl(var(--h) 40% 44%); }
    .scale .d.n1 { background: hsl(var(--gh) 26% 89%); }
    .scale .d.n2 { background: hsl(var(--gh) 30% 76%); }
    .scale .d.n3 { background: hsl(var(--gh) 34% 60%); }
    .scale .d.n4 { background: hsl(var(--gh) 40% 44%); }
    .legend .sw { background: hsl(var(--h) 42% 52%); }
  }

  .none { font-size: 12px; color: var(--faint); font-family: var(--sans); }
`;

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

@component("fkit-activity")
@styles(sheet)
export class FkitActivity extends LoomElement {
  @prop accessor data: Activity | null = null;
  /** Whose it is, for the sentence over the grid. */
  @prop accessor who = "";

  update() {
    const a = this.data;
    if (!a) return <div class="none">&nbsp;</div>;

    const by = new Map(a.days.map((d) => [d.date, d]));
    const named = new Set(topRepos(a).map(([repo]) => repo));
    const start = parseDay(a.since);
    const end = parseDay(a.until);
    /* Whole weeks, so the last column is a column. Days past today are drawn
       as gaps rather than as empty squares, which would claim a Thursday that
       has not happened yet was one nobody worked. */
    const cells = Math.ceil((diffDays(start, end) + 1) / 7) * 7;

    const squares: unknown[] = [];
    const months: unknown[] = [];
    let lastLabel = -9;
    let lastMonth = -1;

    for (let i = 0; i < cells; i++) {
      const day = addDays(start, i);
      const iso = isoDay(day);
      const hit = by.get(iso);

      if (i % 7 === 0) {
        /* One label per month, and never within three columns of the last --
           February beside March beside April on an eleven-pixel pitch is
           unreadable, and a missing label costs nothing. */
        const m = day.getUTCMonth();
        const col = i / 7 + 1;
        if (m !== lastMonth && col - lastLabel >= 3) {
          months.push(<span style={`grid-column:${col}`}>{MONTHS[m]}</span>);
          lastLabel = col;
        }
        lastMonth = m;
      }

      if (day > end) {
        squares.push(<i class="d off"></i>);
        continue;
      }
      if (!hit) {
        squares.push(<i class="d" title={`no commits on ${human(day)}`}></i>);
        continue;
      }
      const level = Math.min(
        LEVELS,
        Math.max(1, Math.ceil((hit.count / Math.max(a.busiest, 1)) * LEVELS)),
      );
      const hue = named.has(hit.repo) ? hueFor(hit.repo) : OTHER_HUE;
      squares.push(
        <i
          class={`d l${level}`}
          style={`--h:${hue}`}
          title={`${plural(hit.count, "commit")} on ${human(day)} — mostly ${hit.repo}`}
        ></i>,
      );
    }

    return (
      <>
        <div class="head">
          <b>{plural(a.total, "commit")}</b>
          {/* "in", not "pushed in". The squares are placed by when each commit
              says it was written, so a history imported on Tuesday sits on the
              days it was actually made rather than in a wall on Tuesday. What
              the push establishes is whose it is, not when -- and a heading
              that said "pushed" would be describing the wrong axis. */}
          <span
            title="Placed by when each commit says it was written, and credited to the account that pushed it."
          >
            in the last year
          </span>
          <span class="fill"></span>
          {a.busiest > 0 ? <span>busiest day {a.busiest}</span> : null}
        </div>

        <div class="scroll">
          <div class="plot">
            <div class="months">{months}</div>
            <div class="dows">
              <span></span><span>Mon</span><span></span>
              <span>Wed</span><span></span><span>Fri</span><span></span>
            </div>
            <div class="grid">{squares}</div>
          </div>
        </div>

        <div class="legend">
          {topRepos(a).map(([repo, n]) => (
            <a href={`/${repo}`} onClick={linkHandler(`/${repo}`)} title={plural(n, "commit")}>
              <span class="sw" style={`--h:${hueFor(repo)}`}></span>
              {repo}
            </a>
          ))}
          <span class="fill"></span>
          {/* The scale is shown in one neutral hue: it is explaining depth,
              and doing that in a colour that also means a repository would be
              explaining two things with one row of squares. */}
          <span class="scale" style={`--gh:${OTHER_HUE}`}>
            less
            <i class="d n1"></i><i class="d n2"></i><i class="d n3"></i><i class="d n4"></i>
            more
          </span>
        </div>
      </>
    );
  }
}

/** The projects worth naming under the grid, busiest first. */
function topRepos(a: Activity): [string, number][] {
  const tally = new Map<string, number>();
  for (const d of a.days) tally.set(d.repo, (tally.get(d.repo) ?? 0) + d.count);
  return [...tally.entries()].sort((x, y) => y[1] - x[1]).slice(0, NAMED);
}

/* Days are handled in UTC throughout, because that is what the server counted
   them in. Building them from local time would slide every square by a day for
   half the world and put commits in the wrong week. */
function parseDay(iso: string): Date {
  const [y, m, d] = iso.split("-").map(Number);
  return new Date(Date.UTC(y, (m ?? 1) - 1, d ?? 1));
}
function addDays(d: Date, n: number): Date {
  return new Date(d.getTime() + n * 86400000);
}
function diffDays(a: Date, b: Date): number {
  return Math.round((b.getTime() - a.getTime()) / 86400000);
}
function isoDay(d: Date): string {
  return d.toISOString().slice(0, 10);
}
function human(d: Date): string {
  return `${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`;
}
function plural(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}
