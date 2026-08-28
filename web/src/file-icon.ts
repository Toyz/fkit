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

/** Files whose whole name is the signal, extension or not. */
const BY_NAME: Record<string, string> = {
  dockerfile: "gear",
  makefile: "gear",
  cargo: "gear",
  license: "doc",
  licence: "doc",
  readme: "doc",
  changelog: "doc",
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
