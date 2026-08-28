/**
 * Push a stylesheet into another custom element's shadow root.
 *
 * `<loom-virtual>` renders its rows inside its own shadow root, which the
 * surrounding component's stylesheet cannot reach. Without this, virtualized
 * rows have to be styled with inline `style=` strings — which works for layout
 * but cannot express `:hover`, `:nth-child`, or `position: sticky` cleanly, and
 * turns the row template into a wall of concatenated CSS text.
 *
 * Adopting a constructable stylesheet into that shadow root gives the rows real
 * classes again. It is idempotent, so calling it on every render is safe.
 */
export function adoptInto(host: Element | null | undefined, sheet: CSSStyleSheet): void {
  if (!host) return;

  const apply = () => {
    const root = (host as HTMLElement).shadowRoot;
    if (!root) return false;
    if (!root.adoptedStyleSheets.includes(sheet)) {
      root.adoptedStyleSheets = [...root.adoptedStyleSheets, sheet];
    }
    return true;
  };

  // The shadow root usually exists immediately; if the element has not upgraded
  // yet, try again on the next frame rather than silently rendering unstyled.
  if (!apply()) requestAnimationFrame(apply);
}
