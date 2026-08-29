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

/**
 * ## Adding a language
 *
 * One entry in `DEFS` below, and nothing else. Extensions and exact filenames
 * live in the entry, so there is no second table to remember:
 *
 * ```ts
 * lua: {
 *   ext: "lua",
 *   line: "--",
 *   block: ["--[[", "]]"],
 *   strings: [`"`, `'`],
 *   keywords: "and break do else elseif end for function if in local nil not or repeat return then true until while",
 * },
 * ```
 *
 * Everything is optional. `keywords` and `types` are space-separated strings,
 * `strings` are quote characters (or `str()` for anything unusual), and
 * `like: "ts"` borrows another language's grammar wholesale for a dialect.
 *
 * For a format that is line-oriented rather than nested — a manifest, an
 * ini file — give `patterns` instead of a grammar and skip the tokenizer
 * entirely.
 */
interface LangDef {
  /** Space-separated extensions, without dots. */
  ext?: string;
  /** Space-separated exact filenames, for things like `Dockerfile`. */
  files?: string;
  /** Start this from another language's grammar. */
  like?: string;
  line?: string | string[];
  block?: [string, string];
  strings?: (string | StringRule)[];
  keywords?: string;
  types?: string;
  /** Line-oriented alternative to the tokenizer: first match at each position wins. */
  patterns?: [RegExp, Cls][];
}

interface Lang {
  line: string[];
  block?: [string, string];
  strings: StringRule[];
  keywords: Set<string>;
  types: Set<string>;
  patterns?: { re: RegExp; c: Cls }[];
}

const w = (s: string) => new Set(s.split(/\s+/).filter(Boolean));

/** A string rule for anything a bare quote character cannot express. */
export const str = (
  open: string,
  close = open,
  escape = true,
  multiline = false,
): StringRule => ({ open, close, escape, multiline });


const COMMON_TYPES =
  "string number boolean object void any never unknown bool int uint float double char byte " +
  "i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize f32 f64 str String Vec Option Result Box " +
  "Arc Rc HashMap HashSet BTreeMap Self Array Object Promise Map Set Date RegExp Error";

const DEFS: Record<string, LangDef> = {
  rust: {
    ext: "rs",
    line: ["//"],
    block: ["/*", "*/"],
    strings: [str('"', '"', true, true), str("'", "'")],
    keywords: `as async await break const continue crate dyn else enum extern false fn for if
      impl in let loop match mod move mut pub ref return self static struct super trait true type
      unsafe use where while yield macro_rules`,
    types: COMMON_TYPES,
  },
  ts: {
    ext: "ts tsx js jsx mjs cjs",
    line: ["//"],
    block: ["/*", "*/"],
    strings: [`"`, `'`, str("`", "`", true, true)],
    keywords: `abstract as async await break case catch class const continue debugger declare
      default delete do else enum export extends false finally for from function get if implements
      import in instanceof interface let new null of private protected public readonly return
      satisfies set static super switch this throw true try type typeof undefined var void while
      with yield accessor`,
    types: COMMON_TYPES,
  },
  python: {
    ext: "py pyi",
    line: ["#"],
    strings: [str('"""', '"""', true, true), str("'''", "'''", true, true), `"`, `'`],
    keywords: `and as assert async await break class continue def del elif else except False
      finally for from global if import in is lambda None nonlocal not or pass raise return True
      try while with yield match case`,
    types: "int float str bytes bool list dict set tuple frozenset object type self cls",
  },
  go: {
    ext: "go",
    line: ["//"],
    block: ["/*", "*/"],
    strings: [`"`, `'`, str("`", "`", false, true)],
    keywords: `break case chan const continue default defer else fallthrough for func go goto
      if import interface map package range return select struct switch type var nil true false`,
    types: "bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uintptr any",
  },
  shell: {
    ext: "sh bash zsh fish", files: "Dockerfile Makefile",
    line: ["#"],
    strings: [str('"', '"', true, true), str("'", "'", false, true)],
    keywords: `if then else elif fi for while until do done case esac function return exit
      export local readonly set unset shift source echo cd mkdir rm cp mv test`,
  },
  json: {
    ext: "json",
    line: [],
    strings: [`"`],
    keywords: "true false null",
  },
  toml: {
    ext: "toml",
    line: ["#"],
    strings: [str('"""', '"""', true, true), `"`, str("'", "'", false)],
    keywords: "true false",
  },
  yaml: {
    ext: "yaml yml",
    line: ["#"],
    strings: [`"`, str("'", "'", false)],
    keywords: "true false null yes no on off",
  },
  sql: {
    ext: "sql",
    line: ["--"],
    block: ["/*", "*/"],
    strings: [str("'", "'", false), `"`],
    keywords: `select from where insert into values update set delete create table alter drop
      index unique primary key foreign references default not null and or as join left right inner
      outer on group by order limit offset returning with begin commit rollback constraint check
      exists case when then else end distinct union all`,
    types: "int integer bigint smallint serial bigserial text varchar char boolean bytea uuid timestamptz timestamp date jsonb json numeric real double",
  },
  css: {
    ext: "css scss",
    line: [],
    block: ["/*", "*/"],
    strings: [`"`, `'`],
    keywords: "important media supports keyframes import from to and not only",
  },
  html: {
    ext: "html htm xml svg",
    line: [],
    block: ["<!--", "-->"],
    strings: [`"`, `'`],
  },
  markdown: { ext: "md markdown", line: [], strings: [], keywords: "", types: "" },
  c: {
    ext: "c h cpp cc hpp java cs",
    line: ["//"],
    block: ["/*", "*/"],
    strings: [`"`, `'`],
    keywords: `auto break case char const continue default do double else enum extern float for
      goto if inline int long register restrict return short signed sizeof static struct switch
      typedef union unsigned void volatile while class public private protected virtual override
      template typename namespace using new delete this nullptr true false`,
    types: COMMON_TYPES + " size_t ssize_t int8_t int16_t int32_t int64_t uint8_t uint32_t uint64_t",
  },

  /**
   * Ignore files. Line-oriented, and what matters is seeing the shape of a
   * pattern at a glance: what is negated, what is anchored to the root, what
   * is a directory, and which parts are wildcards rather than literal text.
   */
  ignore: {
    files: ".fkitignore .gitignore .dockerignore .npmignore .eslintignore .prettierignore .fkthat",
    patterns: [
      [/#.*/, "cm"],
      // A leading ! flips the rule, which is the easiest thing to miss.
      [/^!/, "kw"],
      // Anchored to the repository root, or matching a directory only.
      [/^\//, "kw"],
      [/\/$/, "kw"],
      // The wildcards, so the literal part of a pattern reads as literal.
      [/\*\*|[*?]/, "nu"],
      [/\[[^\]]*\]/, "nu"],
    ],
  },

  /**
   * The submodule manifest. Line-oriented, so it gets patterns rather than a
   * grammar: what matters is telling the three parts of a line apart — where
   * it is mounted, where it comes from, and which revision it is pinned at.
   */
  "fkit-submodules": {
    files: ".fkit-submodules",
    patterns: [
      [/#.*/, "cm"],
      // The mount path, which is only ever at the start of a line.
      [/^[^=\n]+(?==)/, "ty"],
      [/=/, "pu"],
      // The URL, stopping short of a trailing @revision rather than eating it.
      [/[a-z][a-z0-9+.-]*:\/\/(?:(?!@[0-9a-f]{7,64}\s*$)\S)+/, "st"],
      [/@[0-9a-f]{7,64}/, "nu"],
    ],
  },
};

/** Fill in the defaults, and resolve `like`. */
function build(name: string, seen = new Set<string>()): Lang {
  const d = DEFS[name];
  if (!d) throw new Error(`unknown language: ${name}`);

  let base: Lang = { line: [], strings: [], keywords: w(""), types: w("") };
  if (d.like) {
    // A dialect cannot inherit from itself, directly or in a ring.
    if (seen.has(name)) throw new Error(`circular \`like\` at ${name}`);
    seen.add(name);
    base = build(d.like, seen);
  }

  return {
    line: d.line === undefined ? base.line : Array.isArray(d.line) ? d.line : [d.line],
    block: d.block ?? base.block,
    strings:
      d.strings?.map((s) => (typeof s === "string" ? str(s) : s)) ?? base.strings,
    keywords: d.keywords === undefined ? base.keywords : w(d.keywords),
    types: d.types === undefined ? base.types : w(d.types),
    // Sticky, so a pattern can only match where the scanner currently is.
    patterns: d.patterns?.map(([re, c]) => ({
      re: new RegExp(re.source, re.flags.replace(/[gy]/g, "") + "y"),
      c,
    })),
  };
}

const LANGS: Record<string, Lang> = Object.fromEntries(
  Object.keys(DEFS).map((k) => [k, build(k)]),
);

/** Derived from the entries themselves, so adding one cannot miss a table. */
const BY_EXT: Record<string, string> = {};
const BY_FILE: Record<string, string> = {};
for (const [name, d] of Object.entries(DEFS)) {
  for (const e of (d.ext ?? "").split(/\s+/).filter(Boolean)) BY_EXT[e] = name;
  for (const f of (d.files ?? "").split(/\s+/).filter(Boolean)) BY_FILE[f] = name;
}

/** Pick a grammar from a file path, or `null` when we have none. */
export function languageFor(path: string): string | null {
  const name = path.split("/").pop() ?? "";
  if (BY_FILE[name]) return BY_FILE[name];
  const ext = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  return BY_EXT[ext] ?? null;
}

/** Every language known, for anything that wants to list them. */
export function languages(): string[] {
  return Object.keys(DEFS).sort();
}

/** One line of a line-oriented format. */
function byPattern(line: string, pats: { re: RegExp; c: Cls }[]): Tok[] {
  const out: Tok[] = [];
  let plain = "";
  const flush = () => {
    if (plain) out.push({ t: plain, c: "" });
    plain = "";
  };
  let i = 0;
  while (i < line.length) {
    let hit: Tok | null = null;
    for (const p of pats) {
      p.re.lastIndex = i;
      const m = p.re.exec(line);
      if (m && m[0].length > 0) {
        hit = { t: m[0], c: p.c };
        break;
      }
    }
    if (hit) {
      flush();
      out.push(hit);
      i += hit.t.length;
    } else {
      plain += line[i];
      i++;
    }
  }
  flush();
  return out;
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
  // A line-oriented format carries no state across lines, so it never needs
  // the block-and-string machinery below.
  if (g.patterns) return src.split("\n").map((l) => byPattern(l, g.patterns!));

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
