/**
 * A small, line-aware syntax highlighter.
 *
 * # Why not a library
 *
 * Every off-the-shelf highlighter takes a whole document and returns a single
 * HTML string. That is fundamentally incompatible with a virtualized file view:
 * to render only the visible slice of a 40 000-line file you need *per-line*
 * tokens, and you cannot get them by splitting highlighted HTML on newlines —
 * block comments and multi-line strings leave tags open across the boundary.
 *
 * So this tokenizer carries its state (inside a block comment, inside a
 * multi-line string) across line boundaries and emits a token list per line.
 * Highlighting composes with virtualization instead of fighting it, and the
 * whole thing is a few hundred lines rather than a megabyte of grammars.
 *
 * It is a lexer, not a parser: it will not tell a type from a variable in an
 * unusual position. For reading code in a browser that trade is worth it.
 */

export type Cls = "" | "cm" | "st" | "nu" | "kw" | "ty" | "fn" | "pu";

export interface Tok {
  t: string;
  c: Cls;
}

interface StringRule {
  open: string;
  close: string;
  /** Backslash escapes apply inside. */
  escape: boolean;
  /** May span lines. */
  multiline: boolean;
}

interface Lang {
  line: string[];
  block?: [string, string];
  strings: StringRule[];
  keywords: Set<string>;
  types: Set<string>;
}

const w = (s: string) => new Set(s.split(/\s+/).filter(Boolean));

const q = (open: string, close = open, escape = true, multiline = false): StringRule => ({
  open,
  close,
  escape,
  multiline,
});

const COMMON_TYPES =
  "string number boolean object void any never unknown bool int uint float double char byte " +
  "i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize f32 f64 str String Vec Option Result Box " +
  "Arc Rc HashMap HashSet BTreeMap Self Array Object Promise Map Set Date RegExp Error";

const LANGS: Record<string, Lang> = {
  rust: {
    line: ["//"],
    block: ["/*", "*/"],
    strings: [q('"', '"', true, true), q("'", "'")],
    keywords: w(`as async await break const continue crate dyn else enum extern false fn for if
      impl in let loop match mod move mut pub ref return self static struct super trait true type
      unsafe use where while yield macro_rules`),
    types: w(COMMON_TYPES),
  },
  ts: {
    line: ["//"],
    block: ["/*", "*/"],
    strings: [q('"'), q("'"), q("`", "`", true, true)],
    keywords: w(`abstract as async await break case catch class const continue debugger declare
      default delete do else enum export extends false finally for from function get if implements
      import in instanceof interface let new null of private protected public readonly return
      satisfies set static super switch this throw true try type typeof undefined var void while
      with yield accessor`),
    types: w(COMMON_TYPES),
  },
  python: {
    line: ["#"],
    strings: [q('"""', '"""', true, true), q("'''", "'''", true, true), q('"'), q("'")],
    keywords: w(`and as assert async await break class continue def del elif else except False
      finally for from global if import in is lambda None nonlocal not or pass raise return True
      try while with yield match case`),
    types: w("int float str bytes bool list dict set tuple frozenset object type self cls"),
  },
  go: {
    line: ["//"],
    block: ["/*", "*/"],
    strings: [q('"'), q("'"), q("`", "`", false, true)],
    keywords: w(`break case chan const continue default defer else fallthrough for func go goto
      if import interface map package range return select struct switch type var nil true false`),
    types: w("bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uintptr any"),
  },
  shell: {
    line: ["#"],
    strings: [q('"', '"', true, true), q("'", "'", false, true)],
    keywords: w(`if then else elif fi for while until do done case esac function return exit
      export local readonly set unset shift source echo cd mkdir rm cp mv test`),
    types: w(""),
  },
  json: {
    line: [],
    strings: [q('"')],
    keywords: w("true false null"),
    types: w(""),
  },
  toml: {
    line: ["#"],
    strings: [q('"""', '"""', true, true), q('"'), q("'", "'", false)],
    keywords: w("true false"),
    types: w(""),
  },
  yaml: {
    line: ["#"],
    strings: [q('"'), q("'", "'", false)],
    keywords: w("true false null yes no on off"),
    types: w(""),
  },
  sql: {
    line: ["--"],
    block: ["/*", "*/"],
    strings: [q("'", "'", false), q('"')],
    keywords: w(`select from where insert into values update set delete create table alter drop
      index unique primary key foreign references default not null and or as join left right inner
      outer on group by order limit offset returning with begin commit rollback constraint check
      exists case when then else end distinct union all`),
    types: w("int integer bigint smallint serial bigserial text varchar char boolean bytea uuid timestamptz timestamp date jsonb json numeric real double"),
  },
  css: {
    line: [],
    block: ["/*", "*/"],
    strings: [q('"'), q("'")],
    keywords: w("important media supports keyframes import from to and not only"),
    types: w(""),
  },
  html: {
    line: [],
    block: ["<!--", "-->"],
    strings: [q('"'), q("'")],
    keywords: w(""),
    types: w(""),
  },
  markdown: { line: [], strings: [], keywords: w(""), types: w("") },
  c: {
    line: ["//"],
    block: ["/*", "*/"],
    strings: [q('"'), q("'")],
    keywords: w(`auto break case char const continue default do double else enum extern float for
      goto if inline int long register restrict return short signed sizeof static struct switch
      typedef union unsigned void volatile while class public private protected virtual override
      template typename namespace using new delete this nullptr true false`),
    types: w(COMMON_TYPES + " size_t ssize_t int8_t int16_t int32_t int64_t uint8_t uint32_t uint64_t"),
  },
};

const BY_EXT: Record<string, keyof typeof LANGS> = {
  rs: "rust",
  ts: "ts", tsx: "ts", js: "ts", jsx: "ts", mjs: "ts", cjs: "ts",
  py: "python", pyi: "python",
  go: "go",
  sh: "shell", bash: "shell", zsh: "shell", fish: "shell",
  json: "json",
  toml: "toml",
  yaml: "yaml", yml: "yaml",
  sql: "sql",
  css: "css", scss: "css",
  html: "html", htm: "html", xml: "html", svg: "html",
  md: "markdown", markdown: "markdown",
  c: "c", h: "c", cpp: "c", cc: "c", hpp: "c", java: "c", cs: "c",
};

/** Pick a grammar from a file path, or `null` when we have none. */
export function languageFor(path: string): string | null {
  const name = path.split("/").pop() ?? "";
  if (name === "Dockerfile") return "shell";
  if (name === "Makefile") return "shell";
  const ext = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  return BY_EXT[ext] ?? null;
}

const isIdentStart = (ch: string) => /[A-Za-z_$]/.test(ch);
const isIdent = (ch: string) => /[A-Za-z0-9_$]/.test(ch);
const isDigit = (ch: string) => ch >= "0" && ch <= "9";

/**
 * Tokenize `code` into one token array per line.
 *
 * Returns plain text lines when the language is unknown, so callers can use a
 * single rendering path either way.
 */
export function highlight(code: string, lang: string | null): Tok[][] {
  const src = code.replace(/\n$/, "");
  const g = lang ? LANGS[lang] : undefined;
  if (!g) return src.split("\n").map((l) => [{ t: l, c: "" as Cls }]);

  const lines: Tok[][] = [];
  let cur: Tok[] = [];
  let buf = "";
  let bufCls: Cls = "";

  const flush = () => {
    if (buf) cur.push({ t: buf, c: bufCls });
    buf = "";
  };
  const push = (text: string, c: Cls) => {
    if (bufCls !== c) {
      flush();
      bufCls = c;
    }
    // A token may span a newline (block comments, template strings): break the
    // line here and carry the class over, which is the whole point of doing
    // this per line rather than over a finished HTML string.
    let start = 0;
    for (let k = 0; k < text.length; k++) {
      if (text[k] === "\n") {
        buf += text.slice(start, k);
        flush();
        lines.push(cur);
        cur = [];
        bufCls = c;
        start = k + 1;
      }
    }
    buf += text.slice(start);
  };

  let i = 0;
  const n = src.length;

  while (i < n) {
    const rest = src.slice(i, i + 8);

    // line comment
    const lc = g.line.find((m) => rest.startsWith(m));
    if (lc) {
      const end = src.indexOf("\n", i);
      const stop = end === -1 ? n : end;
      push(src.slice(i, stop), "cm");
      i = stop;
      continue;
    }

    // block comment
    if (g.block && rest.startsWith(g.block[0])) {
      const close = src.indexOf(g.block[1], i + g.block[0].length);
      const stop = close === -1 ? n : close + g.block[1].length;
      push(src.slice(i, stop), "cm");
      i = stop;
      continue;
    }

    // string — longest opener first so """ beats "
    const rule = [...g.strings]
      .sort((a, b) => b.open.length - a.open.length)
      .find((r) => src.startsWith(r.open, i));
    if (rule) {
      let j = i + rule.open.length;
      while (j < n) {
        if (rule.escape && src[j] === "\\") {
          j += 2;
          continue;
        }
        if (src.startsWith(rule.close, j)) {
          j += rule.close.length;
          break;
        }
        // An unterminated single-line string ends at the newline rather than
        // swallowing the rest of the file.
        if (src[j] === "\n" && !rule.multiline) break;
        j++;
      }
      push(src.slice(i, Math.min(j, n)), "st");
      i = Math.min(j, n);
      continue;
    }

    const ch = src[i];

    if (isDigit(ch) || (ch === "." && isDigit(src[i + 1] ?? ""))) {
      let j = i;
      while (j < n && /[0-9a-fA-FxXoObB._]/.test(src[j])) j++;
      push(src.slice(i, j), "nu");
      i = j;
      continue;
    }

    if (isIdentStart(ch)) {
      let j = i;
      while (j < n && isIdent(src[j])) j++;
      const word = src.slice(i, j);
      let k = j;
      while (k < n && (src[k] === " " || src[k] === "\t")) k++;
      const cls: Cls = g.keywords.has(word)
        ? "kw"
        : g.types.has(word)
          ? "ty"
          : src[k] === "("
            ? "fn"
            : /^[A-Z]/.test(word)
              ? "ty"
              : "";
      push(word, cls);
      i = j;
      continue;
    }

    if (/[{}()[\];:,.<>=+\-*/%!&|^~?@#]/.test(ch)) {
      push(ch, "pu");
      i++;
      continue;
    }

    push(ch, "");
    i++;
  }

  flush();
  lines.push(cur);
  return lines;
}
