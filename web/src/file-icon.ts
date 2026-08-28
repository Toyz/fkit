/**
 * Which icon a file gets.
 *
 * By extension, and only by extension — this is decoration, not a security
 * decision, so a name that lies about its contents costs a wrong glyph and
 * nothing more. (Where it *would* matter, the raw endpoint sniffs the bytes
 * instead and ignores the name entirely.)
 *
 * Deliberately coarse. Twenty file-type icons is a colour-coded mess at 13px;
 * seven categories is enough to make a directory listing scannable, which is
 * the entire job.
 */
const BY_EXT: Record<string, string> = {};

const add = (icon: string, exts: string) => {
  for (const e of exts.split(" ")) BY_EXT[e] = icon;
};

add("code", "rs ts tsx js jsx mjs cjs py go rb java c h cc cpp hpp cs kt swift " +
            "php sh bash zsh fish ps1 lua zig ex exs erl hs ml scala dart vue svelte sql");
add("doc", "md markdown mdx txt rst adoc org tex pdf doc docx rtf");
add("image", "png jpg jpeg gif webp avif bmp ico svg tiff heic");
add("gear", "json yaml yml toml ini cfg conf env properties xml plist editorconfig " +
            "gitignore fkitignore dockerignore lock");
add("archive", "zip tar gz tgz bz2 xz zst 7z rar jar war");
add("db", "db sqlite sqlite3 sql3 mdb parquet");
add("blob", "exe dll so dylib a o bin wasm class pyc elf img iso");

/**
 * Files whose whole name is the signal, extension or not.
 *
 * The well-known documents use the same icons the document tabs do — one file
 * showing a generic page in the listing and a scales-of-justice in the tab
 * strip is two answers to the same question.
 */
const BY_NAME: Record<string, string> = {
  // ".env", ".env.example", ".env.prod" — the stem is the whole signal.
  env: "gear",
  dockerfile: "gear",
  makefile: "gear",
  cargo: "gear",
  justfile: "gear",
  readme: "book",
  license: "scale",
  licence: "scale",
  copying: "scale",
  contributing: "people",
  code_of_conduct: "heart",
  "code-of-conduct": "heart",
  codeofconduct: "heart",
  security: "shield",
  changelog: "history",
  authors: "people",
  maintainers: "people",
};

export function fileIcon(name: string): string {
  const lower = name.toLowerCase();

  // A dotfile is its own name, not an extension: ".gitignore" must not read as
  // a file called "" with extension "gitignore" — though both land on gear.
  const bare = lower.startsWith(".") ? lower.slice(1) : lower;
  const stem = bare.split(".")[0] ?? "";
  if (BY_NAME[stem]) return BY_NAME[stem];

  const dot = bare.lastIndexOf(".");
  if (dot > 0 || (dot === 0 && bare.length > 1)) {
    const ext = bare.slice(dot + 1);
    if (BY_EXT[ext]) return BY_EXT[ext];
  }
  // A dotfile with no extension at all — .env, .gitignore — is configuration.
  if (lower.startsWith(".") && !bare.includes(".")) return BY_EXT[bare] ?? "gear";

  return "file";
}

/**
 * Directories that are recognisably something.
 *
 * Only a handful earn their own mark: a folder icon says "folder", which is
 * already obvious, so a different glyph has to buy real recognition.
 */
const DIRS: Record<string, string> = {
  ".github": "github",
  ".gitlab": "gitlab",
  ".vscode": "gear",
  ".idea": "gear",
  ".cargo": "gear",
  ".claude": "gear",
};

export function dirIcon(name: string): string {
  return DIRS[name.toLowerCase()] ?? "folder";
}
