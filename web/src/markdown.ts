/**
 * A small, dependency-free Markdown renderer for READMEs.
 *
 * Scope is deliberate: headings, fenced and inline code, bold/italic, links,
 * lists, blockquotes, rules, and tables. That covers essentially every README
 * without shipping a 40 KB parser into a page whose job is to be fast.
 *
 * Everything is HTML-escaped *before* any markup is generated, and link hrefs
 * are restricted to http/https/mailto and relative paths — a README is
 * untrusted input authored by whoever pushed the repository.
 */

const escapeHtml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

function safeHref(raw: string): string | null {
  const url = raw.trim();
  if (/^(https?:|mailto:)/i.test(url)) return url;
  // Relative links are fine; anything with a scheme (javascript:, data:) is not.
  if (!/^[a-z][a-z0-9+.-]*:/i.test(url)) return url;
  return null;
}

/**
 * How a repository-relative path in a document becomes a URL.
 *
 * A README that says `![logo](docs/logo.png)` means a file in the repository,
 * not a path on the site — without this the browser resolves it against the
 * current page and asks for something that does not exist.
 */
export interface MarkdownContext {
  /** A path whose *bytes* are wanted: images. */
  raw: (path: string) => string;
  /** A path a reader should be *taken to*: links. */
  page: (path: string) => string;
}

/** Is this URL relative to the document, rather than absolute or an anchor? */
function isRelative(url: string): boolean {
  return !/^[a-z][a-z0-9+.-]*:/i.test(url) && !url.startsWith("/") && !url.startsWith("#");
}

/** Inline formatting, applied to already-escaped text. */
function inline(text: string, ctx?: MarkdownContext): string {
  return (
    text
      // `code` first, so its contents are not treated as markup
      .replace(/`([^`]+)`/g, (_, c) => `<code>${c}</code>`)
      .replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (m, alt, src) => {
        const href = safeHref(src);
        if (!href) return m;
        const url = ctx && isRelative(href) ? ctx.raw(href) : href;
        return `<img src="${url}" alt="${alt}" loading="lazy">`;
      })
      .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, label, url) => {
        const href = safeHref(url);
        if (!href) return m;
        const to = ctx && isRelative(href) ? ctx.page(href) : href;
        return `<a href="${to}" rel="noopener noreferrer">${label}</a>`;
      })
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|\W)_([^_]+)_(?=\W|$)/g, "$1<em>$2</em>")
      .replace(/\*([^*]+)\*/g, "<em>$1</em>")
      .replace(/~~([^~]+)~~/g, "<del>$1</del>")
  );
}

export function renderMarkdown(src: string, ctx?: MarkdownContext): string {
  const lines = escapeHtml(src.replace(/\r\n/g, "\n")).split("\n");
  const out: string[] = [];

  let i = 0;
  let listType: "ul" | "ol" | null = null;

  const closeList = () => {
    if (listType) {
      out.push(`</${listType}>`);
      listType = null;
    }
  };

  while (i < lines.length) {
    const line = lines[i];

    // Fenced code — emitted verbatim, no inline processing.
    const fence = line.match(/^```(\w*)\s*$/);
    if (fence) {
      closeList();
      const lang = fence[1];
      const buf: string[] = [];
      i++;
      while (i < lines.length && !/^```\s*$/.test(lines[i])) buf.push(lines[i++]);
      i++; // closing fence
      out.push(
        `<pre><code${lang ? ` class="lang-${lang}"` : ""}>${buf.join("\n")}</code></pre>`,
      );
      continue;
    }

    if (/^\s*$/.test(line)) {
      closeList();
      i++;
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      closeList();
      const level = heading[1].length;
      out.push(`<h${level}>${inline(heading[2], ctx)}</h${level}>`);
      i++;
      continue;
    }

    if (/^\s*([-*_])\1{2,}\s*$/.test(line)) {
      closeList();
      out.push("<hr>");
      i++;
      continue;
    }

    // Table: a header row followed by a |---|---| separator.
    if (/^\s*\|/.test(line) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      closeList();
      const cells = (r: string) =>
        r.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = cells(line);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && /^\s*\|/.test(lines[i])) rows.push(cells(lines[i++]));
      out.push(
        `<table><thead><tr>${head.map((h) => `<th>${inline(h, ctx)}</th>`).join("")}</tr></thead>` +
          `<tbody>${rows
            .map((r) => `<tr>${r.map((c) => `<td>${inline(c, ctx)}</td>`).join("")}</tr>`)
            .join("")}</tbody></table>`,
      );
      continue;
    }

    const bullet = line.match(/^\s*[-*+]\s+(.*)$/);
    const numbered = line.match(/^\s*\d+\.\s+(.*)$/);
    if (bullet || numbered) {
      const want = bullet ? "ul" : "ol";
      if (listType !== want) {
        closeList();
        out.push(`<${want}>`);
        listType = want;
      }
      out.push(`<li>${inline((bullet ?? numbered)![1], ctx)}</li>`);
      i++;
      continue;
    }

    if (/^\s*&gt;\s?/.test(line)) {
      closeList();
      const buf: string[] = [];
      while (i < lines.length && /^\s*&gt;\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^\s*&gt;\s?/, ""));
        i++;
      }
      out.push(`<blockquote>${inline(buf.join(" "), ctx)}</blockquote>`);
      continue;
    }

    // Paragraph: gather until a blank line or a block-level construct.
    closeList();
    const para: string[] = [];
    while (
      i < lines.length &&
      !/^\s*$/.test(lines[i]) &&
      !/^(#{1,6}\s|```|\s*[-*+]\s|\s*\d+\.\s|\s*&gt;)/.test(lines[i])
    ) {
      para.push(lines[i++]);
    }
    out.push(`<p>${inline(para.join(" "), ctx)}</p>`);
  }

  closeList();
  return out.join("\n");
}
