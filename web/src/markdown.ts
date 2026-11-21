/**
 * A small, dependency-free Markdown renderer for READMEs.
 *
 * Scope is deliberate: headings, fenced and inline code, bold/italic, links,
 * images, lists, blockquotes, rules, tables, and the handful of HTML tags
 * READMEs actually reach for. That covers essentially every README without
 * shipping a 40 KB parser into a page whose job is to be fast.
 *
 * Everything is HTML-escaped *before* any markup is generated, and link hrefs
 * are restricted to http/https/mailto and relative paths — a README is
 * untrusted input authored by whoever pushed the repository.
 *
 * That ordering is the whole security model, and the HTML support keeps it
 * rather than working around it. Nothing a document says is ever passed
 * through: a tag is *recognised* in the escaped text, and a new one is written
 * from scratch out of a name on a list and attributes on a list, each value
 * checked for what it is. Whatever is not recognised is still escaped text,
 * which is what it was before this file knew about tags at all.
 */

const escapeHtml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

function safeHref(raw: string): string | null {
  const url = raw.trim();
  // A browser deletes tabs and newlines from a URL before reading its scheme,
  // so "java<tab>script:" is `javascript:` to it and a relative path to the
  // test below. No real address contains a control character; refuse them all
  // rather than try to predict which ones get stripped.
  if (/[\u0000-\u001f\u007f]/.test(url)) return null;
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
  /**
   * Where `#4` should go, when the document is somewhere `#4` means
   * something — a comment on a repository, rather than a README.
   *
   * Issues and merge requests share one counter precisely so that a number
   * names one thing, which is what makes this link possible to build without
   * knowing which of the two it is.
   */
  ref?: (n: number) => string;
}

/** Is this URL relative to the document, rather than absolute or an anchor? */
function isRelative(url: string): boolean {
  return !/^[a-z][a-z0-9+.-]*:/i.test(url) && !url.startsWith("/") && !url.startsWith("#");
}

/**
 * The HTML a document may use, and the attributes each tag may carry.
 *
 * Chosen by what READMEs do with it: centre a logo, size an image, wrap a
 * badge in a link, fold a section away, swap a picture for dark mode. No
 * `style`, `class` or `id` anywhere — the first restyles the page, the other
 * two reach into it.
 */
const ALIGN = ["align"];
const TAGS: Record<string, readonly string[]> = {
  img: ["src", "alt", "title", "width", "height", "align"],
  a: ["href", "title"],
  picture: [],
  source: ["srcset", "media", "type", "width", "height"],
  p: ALIGN, div: ALIGN, center: [], span: [],
  h1: ALIGN, h2: ALIGN, h3: ALIGN, h4: ALIGN, h5: ALIGN, h6: ALIGN,
  br: [], hr: [],
  b: [], strong: [], i: [], em: [], u: [], s: [], del: [], ins: [],
  code: [], kbd: [], samp: [], sub: [], sup: [], small: [],
  pre: [], blockquote: [],
  details: ["open"], summary: [],
  ul: [], ol: ["start"], li: [],
  dl: [], dt: [], dd: [],
  table: [], thead: [], tbody: [], tr: [],
  th: ["align", "colspan", "rowspan"], td: ["align", "colspan", "rowspan"],
};

/** Tags that never have a closing half. */
const VOID = new Set(["img", "br", "hr", "source"]);

/**
 * Tags that begin a block when a line opens with one.
 *
 * The distinction matters to the paragraph logic and to nothing else: a line
 * starting `<p align="center">` must not be wrapped in a `<p>` of its own,
 * while a line starting `<img>` is ordinary text that happens to be a picture.
 */
const BLOCK =
  "p|div|center|h[1-6]|hr|pre|blockquote|details|summary|ul|ol|li|dl|dt|dd|table|thead|tbody|tr|th|td";
const HTML_BLOCK = new RegExp(`^\\s*&lt;/?(?:${BLOCK})(?:\\s|/|&gt;|$)`, "i");

/**
 * Check one attribute's value, and say what to write for it.
 *
 * Returns null to drop the attribute. Every branch is a positive description
 * of what the value may be; none of them is a list of things to strip out.
 */
function attribute(tag: string, name: string, value: string, ctx?: MarkdownContext): string | null {
  switch (name) {
    case "src":
    case "href": {
      const href = safeHref(value);
      if (!href) return null;
      if (!ctx || !isRelative(href)) return href;
      return name === "src" ? ctx.raw(href) : ctx.page(href);
    }
    case "srcset": {
      // A comma-separated list of `url descriptor`. Each URL is held to the
      // same rule as a `src`; one bad candidate drops the whole attribute,
      // because a list with a hole in it is not the list that was written.
      const out: string[] = [];
      for (const part of value.split(",")) {
        const [url, size, ...extra] = part.trim().split(/\s+/);
        const href = url ? safeHref(url) : null;
        if (!href || extra.length || (size && !/^\d+(\.\d+)?[wx]$/.test(size))) return null;
        const to = ctx && isRelative(href) ? ctx.raw(href) : href;
        out.push(size ? `${to} ${size}` : to);
      }
      return out.join(", ");
    }
    case "width":
    case "height":
      return /^\d{1,5}%?$/.test(value) ? value : null;
    case "colspan":
    case "rowspan":
    case "start":
      return /^\d{1,4}$/.test(value) ? value : null;
    case "align": {
      const v = value.toLowerCase();
      const ok = tag === "img" ? ["left", "right", "middle", "top", "bottom"] : ["left", "center", "right"];
      // "center" on an image is not a thing HTML ever had, but it is what
      // people write. Dropping it is right: the centring comes from the parent.
      return ok.includes(v) ? v : null;
    }
    case "media":
      // A media query, in practice `(prefers-color-scheme: dark)`.
      return /^[\w\s():.,/-]{1,120}$/.test(value) ? value : null;
    case "type":
      return /^[\w.+-]+\/[\w.+-]+$/.test(value) ? value : null;
    case "open":
      return "";
    case "alt":
    case "title":
      return value;
    default:
      return null;
  }
}

/**
 * Write a tag from a recognised name and whatever attributes survive.
 *
 * `attrs` is escaped text, so a double quote in it is the six characters
 * `&quot;` and a value can be cut out of it without a parser — and can be
 * put back between real quotes without being able to end them.
 */
function tag(closing: boolean, rawName: string, attrs: string, ctx?: MarkdownContext): string | null {
  const name = rawName.toLowerCase();
  const allowed = TAGS[name];
  if (!allowed) return null;
  if (closing) return VOID.has(name) ? "" : `</${name}>`;

  let out = `<${name}`;
  const seen = new Set<string>();
  const each = /([a-zA-Z][\w-]*)(?:\s*=\s*(?:&quot;([\s\S]*?)&quot;|'([^']*)'|([^\s'=`]+)))?/g;
  for (const m of attrs.matchAll(each)) {
    const key = m[1].toLowerCase();
    // First one wins, as it does in a browser, so a second `src` cannot
    // replace the one that was checked.
    if (!allowed.includes(key) || seen.has(key)) continue;
    seen.add(key);

    const raw = (m[2] ?? m[3] ?? m[4] ?? "").trim();
    const value = attribute(name, key, raw, ctx);
    if (value === null) continue;
    // An attribute written in HTML was already entity-encoded by its author,
    // and escaping it again turned `a&amp;b` into the literal text "a&amp;b".
    // Only this one entity is put back, and only now, after the value has
    // been checked: restoring the rest earlier is how `javascript&colon;`
    // gets past a test for "javascript:".
    out += value === "" ? ` ${key}` : ` ${key}="${value.replace(/&amp;amp;/g, "&amp;")}"`;
  }
  if (name === "a") out += ` rel="noopener noreferrer"`;
  if (name === "img") out += ` loading="lazy"`;
  return out + ">";
}

/**
 * Inline formatting, applied to already-escaped text.
 *
 * Each construct, once turned into markup, is lifted out of the text and a
 * placeholder left where it was; the markup goes back at the end. Without
 * that, every later rule runs over the output of every earlier one — and a
 * `_` in an image URL becomes emphasis, a `#4` in a title becomes a link
 * inside an attribute, and the result is a tag that no longer parses the way
 * it was written. Markup that has been produced is finished, so it is put
 * where no rule can see it.
 */
function inline(text: string, ctx?: MarkdownContext, htmlOnly = false): string {
  const held: string[] = [];
  const hold = (html: string) => `\u0001${held.push(html) - 1}\u0001`;

  let out = text;
  if (!htmlOnly) {
    // `code` first, so its contents are not treated as markup
    out = out.replace(/`([^`]+)`/g, (_, c) => hold(`<code>${c}</code>`));
  }

  out = out
    // A comment is a note to whoever edits the file, not to whoever reads it.
    .replace(/&lt;!--[\s\S]*?--&gt;/g, "")
    .replace(/&lt;(\/?)([a-zA-Z][a-zA-Z0-9]*)((?:\s(?:(?!&gt;)[\s\S])*?)?)\s*\/?&gt;/g, (m, slash, name, attrs) => {
      const html = tag(slash === "/", name, attrs ?? "", ctx);
      return html === null ? m : hold(html);
    });

  if (!htmlOnly) {
    out = out
      .replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (m, alt, src) => {
        const href = safeHref(src);
        if (!href) return m;
        const url = ctx && isRelative(href) ? ctx.raw(href) : href;
        return hold(`<img src="${url}" alt="${alt}" loading="lazy">`);
      })
      .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, label, url) => {
        const href = safeHref(url);
        if (!href) return m;
        const to = ctx && isRelative(href) ? ctx.page(href) : href;
        // Only the tags are put away. The label is still text, and may be
        // bold, or code, or a picture that was lifted out a moment ago.
        return `${hold(`<a href="${to}" rel="noopener noreferrer">`)}${label}${hold("</a>")}`;
      })
      // `#4` links to whatever #4 is. Only where a context says numbers mean
      // something here, and never inside a word, so `a#4` and `#4` in a URL
      // fragment are left alone.
      .replace(/(^|[\s(\[])#(\d+)\b/g, (m, lead: string, n: string) =>
        ctx?.ref ? `${lead}${hold(`<a href="${ctx.ref(Number(n))}">`)}#${n}${hold("</a>")}` : m,
      )
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|\W)_([^_]+)_(?=\W|$)/g, "$1<em>$2</em>")
      .replace(/\*([^*]+)\*/g, "<em>$1</em>")
      .replace(/~~([^~]+)~~/g, "<del>$1</del>");
  }

  // Held markup can itself hold a placeholder — code inside an `alt`, say —
  // but only ever an earlier one, so this always bottoms out.
  const restore = (s: string): string =>
    s.replace(/\u0001(\d+)\u0001/g, (_, n) => restore(held[Number(n)] ?? ""));
  return restore(out);
}

/**
 * A heading's fragment id.
 *
 * The same rule GitHub uses, so a link copied from there — or written by
 * someone who assumed it — lands on the right heading: lower-case, spaces to
 * hyphens, and anything that is not a letter, digit, hyphen or underscore
 * dropped. Inline markup is stripped first, or `## `code` here` would slugify
 * the tags.
 */
export function slug(text: string): string {
  return text
    .replace(/<[^>]*>/g, "")
    .replace(/&[a-z]+;|&#\d+;/gi, "")
    .trim()
    .toLowerCase()
    .replace(/[^\w\- ]+/g, "")
    .replace(/\s+/g, "-");
}

export function renderMarkdown(src: string, ctx?: MarkdownContext): string {
  // U+0001 is what `inline` marks its placeholders with, so a document does
  // not get to contain one of its own.
  const lines = escapeHtml(src.replace(/\r\n/g, "\n").replace(/\u0001/g, "")).split("\n");
  const out: string[] = [];

  let i = 0;
  let listType: "ul" | "ol" | null = null;
  const slugs = new Map<string, number>();

  const closeList = () => {
    if (listType) {
      out.push(`</${listType}>`);
      listType = null;
    }
  };

  while (i < lines.length) {
    const line = lines[i];

    // Fenced code — emitted verbatim, no inline processing.
    // The info string is whatever follows the fence, and its first word is
    // the language. That word is not always `\w+`: "c++", "c#", "objective-c"
    // and "shell-session" are all real, and a fence this did not recognise
    // fell through to the paragraph rule, which refuses to start on one.
    const fence = line.match(/^```\s*([^\s`]*)[^`]*$/);
    if (fence) {
      closeList();
      const lang = /^[\w+#.-]+$/.test(fence[1]) ? fence[1] : "";
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

    // A comment on lines of its own, however many it runs to.
    if (/^\s*&lt;!--/.test(line)) {
      let end = i;
      while (end < lines.length && !lines[end].includes("--&gt;")) end++;
      if (end < lines.length) {
        closeList();
        const tail = lines[end].slice(lines[end].indexOf("--&gt;") + 6);
        if (tail.trim()) lines[end] = tail;
        else end++;
        i = end;
        continue;
      }
      // Never closed: it is not a comment, it is text that starts oddly.
    }

    // A block of HTML: from a line that opens with a block-level tag to the
    // next blank line, which is where CommonMark ends one too. It is emitted
    // as it stands rather than wrapped in a paragraph, and Markdown inside it
    // is not interpreted — both of which are what make
    // `<p align="center">` followed by an image come out as one centred
    // paragraph instead of three broken ones. A blank line hands control
    // back, which is how Markdown ends up inside a `<details>`.
    if (HTML_BLOCK.test(line)) {
      closeList();
      const buf: string[] = [];
      while (i < lines.length && !/^\s*$/.test(lines[i])) buf.push(lines[i++]);
      out.push(inline(buf.join("\n"), ctx, true));
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      closeList();
      const level = heading[1].length;
      const body = inline(heading[2], ctx);

      // Two headings can slugify the same; a document with "## Usage" twice
      // would otherwise have one id pointing at two places and links that go
      // to whichever the browser found first.
      let id = slug(body);
      if (id) {
        const seen = slugs.get(id) ?? 0;
        slugs.set(id, seen + 1);
        if (seen > 0) id = `${id}-${seen}`;
      }

      // The anchor is a real link so it can be copied, middle-clicked and
      // bookmarked; it is hidden until the heading is hovered. It follows the
      // text rather than preceding it, so it cannot land on top of anything.
      out.push(
        id
          ? `<h${level} id="${id}">${body}` +
              `<a class="anchor" href="#${id}" aria-label="Link to this section">#</a>` +
              `</h${level}>`
          : `<h${level}>${body}</h${level}>`,
      );
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
    // The first line is taken unconditionally. Everything above had its
    // chance at it; if none of them wanted it, it is text — and a loop that
    // can decline its own first line is a loop that never ends.
    closeList();
    const para: string[] = [lines[i++]];
    while (
      i < lines.length &&
      !/^\s*$/.test(lines[i]) &&
      !/^(#{1,6}\s|```|\s*[-*+]\s|\s*\d+\.\s|\s*&gt;)/.test(lines[i]) &&
      !HTML_BLOCK.test(lines[i])
    ) {
      para.push(lines[i++]);
    }
    out.push(`<p>${inline(para.join(" "), ctx)}</p>`);
  }

  closeList();
  return out.join("\n");
}
