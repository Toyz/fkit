/**
 * The row of tabs under a page's title.
 *
 * Lifted out of the repository page, which had the only copy. Tabs are how
 * every section of this site is navigated, so a page that grows a second view
 * should not have to reinvent the underline, the icon opacity and the count
 * badge — and more to the point, should not end up with its own slightly
 * different version of them.
 *
 * Real links, not buttons. A tab is a place, so middle-click, copy address and
 * the back button all have to work; the click handler only exists to keep
 * navigation on the client side.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";
import { linkHandler } from "../nav";
import type { IconName } from "../icons";

export interface Tab {
  /** Matched against `current` to decide which is on. */
  key: string;
  label: string;
  /**
   * A `loom-icon` name. Omitted for a tab that reads fine without one.
   *
   * Typed against the icon set rather than left as a string: a name that does
   * not exist renders nothing at all, silently, and the tab just looks
   * slightly wrong forever.
   */
  icon?: IconName;
  href: string;
  /** A number worth showing beside the label — open issues, changed files. */
  count?: number | string;
}

const sheet = css`
  /* Tabs sit on a rule that runs the width of the page, so the row reads as a
     row of tabs rather than as some links that happen to be above the content.
     The active underline then lands on that rule instead of floating over
     nothing, which is what makes one of them look chosen. */
  :host { display: block; border-bottom: 1px solid var(--border); margin-bottom: 18px; }
  .tabs { display: flex; gap: 2px; }
  .tabs a {
    display: flex; align-items: center; gap: 6px;
    padding: 5px 10px; color: var(--muted); font-size: 12px;
    text-decoration: none;
    border-bottom: 2px solid transparent; margin-bottom: -1px;
  }
  .tabs a loom-icon { opacity: .7; }
  .tabs a.on loom-icon { opacity: 1; color: var(--accent); }
  .tabs a:hover { color: var(--text); text-decoration: none; }
  .tabs a.on { color: var(--text); border-bottom-color: var(--accent); }

  /* A count riding a tab. Quiet by default; it picks up the accent on the tab
     you are actually on, the same way the label does. */
  .tabn {
    display: inline-flex; align-items: center; justify-content: center;
    min-width: 16px; height: 16px; padding: 0 5px; margin-left: 2px;
    border-radius: var(--radius-pill); background: var(--raised); color: var(--muted);
    font-size: 10.5px; font-variant-numeric: tabular-nums;
  }
  .tabs a.on .tabn { background: var(--accent-weak); color: var(--accent); }
`;

@component("fkit-tabs")
@styles(sheet)
export class FkitTabs extends LoomElement {
  @prop accessor tabs: Tab[] = [];
  @prop accessor current = "";

  update() {
    return (
      <nav class="tabs" aria-label="Sections">
        {this.tabs.map((t) => (
          <a
            class={t.key === this.current ? "on" : ""}
            href={t.href}
            aria-current={t.key === this.current ? "page" : undefined}
            onClick={linkHandler(t.href)}
          >
            {t.icon ? <loom-icon name={t.icon} size={12}></loom-icon> : null}
            {t.label}
            {/* Zero is not worth a badge — an empty issue tracker should look
                empty rather than decorated with a nought. */}
            {t.count ? <span class="tabn">{t.count}</span> : null}
          </a>
        ))}
      </nav>
    );
  }
}
