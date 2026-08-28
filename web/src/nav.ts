/**
 * Navigation helpers.
 *
 * The app uses history-mode routing, so links must be intercepted rather than
 * letting the browser do a full page load. `go()` is the single place that
 * pushes state and notifies the router.
 */

export function go(path: string): void {
  if (location.pathname + location.search === path) return;
  history.pushState({}, "", path);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

/** Use on any in-app anchor: keeps modifier-clicks and middle-clicks native. */
export function linkHandler(path: string) {
  return (e: MouseEvent) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
      return;
    }
    e.preventDefault();
    go(path);
  };
}

/** Current path split into non-empty segments. */
export function segments(): string[] {
  return location.pathname.split("/").filter(Boolean);
}
