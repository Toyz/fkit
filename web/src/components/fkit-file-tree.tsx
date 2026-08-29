/**
 * The list of files a diff touches, as a tree.
 *
 * A review of twenty files was previously twenty stacked panels and a scrollbar
 * — no way to see the shape of a change, and no way to reach the one file you
 * came for. The tree is the map: it says what areas were touched before you
 * read a line of it.
 *
 * Directories with a single child collapse into their child (`web/src/pages`
 * rather than three nested rows), because the intermediate levels are not
 * decisions anyone made and each one costs a row and an indent.
 */
import { LoomElement, component, css, styles, prop, reactive } from "@toyz/loom";
import { dirIcon, fileIcon } from "../file-icon";

const reset = css`
  *, *::before, *::after { box-sizing: border-box; }
`;

const sheet = css`
  :host {
    display: block;
    border: 1px solid var(--border); border-radius: var(--radius);
    overflow: hidden; background: var(--surface);
  }
  header {
    display: flex; align-items: center; gap: 8px;
    padding: 8px 12px; background: var(--raised);
    border-bottom: 1px solid var(--border);
    font-size: 12px; font-weight: 600; color: var(--text);
  }
  header .grow { flex: 1; }
  header .n { font-weight: 400; color: var(--faint); font-size: 11.5px; }

  .scroll { max-height: min(70vh, 640px); overflow: auto; padding: 4px 0; }

  .row {
    display: flex; align-items: center; gap: 7px;
    padding: 3px 12px 3px calc(12px + var(--depth, 0) * 13px);
    font-size: 12px; color: var(--muted);
    cursor: pointer; border: 0; background: none; width: 100%;
    text-align: left; font-family: var(--mono);
  }
  .row:hover { background: var(--raised); color: var(--text); }
  .row.on { background: var(--raised); color: var(--text); }
  .row.on .nm { color: var(--accent); }

  .nm { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .dir { color: var(--faint); }
  .caret { display: flex; flex: none; transition: transform .12s; color: var(--faint); }
  .caret.shut { transform: rotate(-90deg); }

  /* One column of numbers, so the eye can compare sizes down the list. */
  .cnt { flex: none; font-size: 10.5px; font-variant-numeric: tabular-nums; }
  .cnt .p { color: var(--added); }
  .cnt .m { color: var(--removed); }

  /* The glyph says what kind of file it is; its colour says what happened to
     it. Two facts, one mark, no second column. */
  .ic { flex: none; display: flex; color: var(--faint); }
  .ic.added { color: var(--added); }
  .ic.removed { color: var(--removed); }
  .ic.modified { color: var(--modified); }
  .ic.dir { color: var(--faint); }

  /* A placeholder row: the same height a real one will be, so nothing moves
     when the real ones arrive. Still rather than pulsing — a shimmer draws
     the eye to the one part of the page that has nothing to read yet. */
  .row.ghost { cursor: default; }
  .row.ghost:hover { background: none; }
  .row .sk {
    display: block; height: 9px; border-radius: var(--radius);
    background: var(--raised);
    width: var(--w, 70%);
  }
  .row.ghost:nth-child(2n) .sk { --w: 55%; }
  .row.ghost:nth-child(3n) .sk { --w: 80%; }
`;

interface FileEntry {
  path: string;
  status: string;
  added: number;
  removed: number;
}

interface Node {
  name: string;
  /** Set on a leaf. */
  file?: FileEntry;
  children: Node[];
}

/** Build a directory tree, folding single-child directories into one row. */
function build(files: FileEntry[]): Node[] {
  const root: Node = { name: "", children: [] };

  for (const f of files) {
    const parts = f.path.split("/");
    let at = root;
    for (let i = 0; i < parts.length; i++) {
      const leaf = i === parts.length - 1;
      let next = at.children.find((c) => c.name === parts[i] && !!c.file === leaf);
      if (!next) {
        next = { name: parts[i], children: [], ...(leaf ? { file: f } : {}) };
        at.children.push(next);
      }
      at = next;
    }
  }

  const fold = (n: Node): Node => {
    n.children = n.children.map(fold);
    // A directory holding exactly one directory is not a decision anyone made.
    if (!n.file && n.children.length === 1 && !n.children[0].file) {
      const only = n.children[0];
      return { name: `${n.name}/${only.name}`, children: only.children };
    }
    return n;
  };

  return root.children.map(fold).sort((a, b) => {
    // Directories first, then files, each alphabetically — the order a file
    // manager uses, because it is the one people can predict.
    if (!!a.file !== !!b.file) return a.file ? 1 : -1;
    return a.name.localeCompare(b.name);
  });
}

/**
 * The paths a tree would list, in the order it would list them.
 *
 * Exported so the diff beside the tree can be sorted by the same function
 * rather than by one that happens to agree. They disagreed: the diff arrived
 * alphabetically and flat while the tree grouped by directory, so picking the
 * third row jumped to somewhere near the twentieth diff.
 */
export function treeOrder(files: FileEntry[]): string[] {
  const out: string[] = [];
  const walk = (nodes: Node[]) => {
    for (const n of nodes) {
      if (n.file) {
        out.push(n.file.path);
      } else {
        walk(n.children);
      }
    }
  };
  walk(build(files));
  return out;
}

@component("fkit-file-tree")
@styles(reset, sheet)
export class FkitFileTree extends LoomElement {
  @prop accessor files: FileEntry[] = [];
  /** Path of the file currently in view, highlighted in the list. */
  @prop accessor active = "";
  /**
   * Draw the shape without the content, while the diff is still arriving.
   *
   * The box is the same height it will be, so the diff beside it does not jump
   * down the page when the files land — the tree appearing *is* the layout
   * shift, and reserving its space is the whole fix.
   */
  @prop accessor loading = false;
  @reactive accessor shut: Record<string, boolean> = {};

  private pick(path: string) {
    this.dispatchEvent(new CustomEvent("pick", { detail: path, bubbles: true }));
  }

  private rows(nodes: Node[], depth: number, prefix: string): unknown[] {
    const out: unknown[] = [];
    for (const n of nodes) {
      const key = `${prefix}/${n.name}`;
      if (n.file) {
        const f = n.file;
        out.push(
          <button
            class={`row ${this.active === f.path ? "on" : ""}`}
            style={`--depth:${depth}`}
            loom-key={key}
            title={f.path}
            onClick={() => this.pick(f.path)}
          >
            {/* The kind of file, from the same table the repository listing
                uses — a tree of two hundred identical glyphs is a list you
                have to read rather than scan. */}
            <span class={`ic ${f.status}`}>
              <loom-icon name={fileIcon(n.name)} size={12}></loom-icon>
            </span>
            <span class="nm">{n.name}</span>
            <span class="cnt">
              {f.added ? <span class="p">+{f.added}</span> : null}
              {f.added && f.removed ? " " : null}
              {f.removed ? <span class="m">−{f.removed}</span> : null}
            </span>
          </button>,
        );
        continue;
      }

      const closed = !!this.shut[key];
      out.push(
        <button
          class="row"
          style={`--depth:${depth}`}
          loom-key={key}
          onClick={() => (this.shut = { ...this.shut, [key]: !closed })}
        >
          <span class={`caret ${closed ? "shut" : ""}`}>
            <loom-icon name="chevron" size={10}></loom-icon>
          </span>
          <span class="ic dir">
            <loom-icon name={dirIcon(n.name)} size={12}></loom-icon>
          </span>
          <span class="nm dir">{n.name}</span>
        </button>,
      );
      if (!closed) out.push(...this.rows(n.children, depth + 1, key));
    }
    return out;
  }

  update() {
    if (this.loading) {
      return (
        <>
          <header>
            <span>Files</span>
            <span class="grow"></span>
          </header>
          <div class="scroll">
            {[0, 1, 2, 3, 4, 5].map(() => (
              <div class="row ghost">
                <span class="sk"></span>
              </div>
            ))}
          </div>
        </>
      );
    }

    const tree = build(this.files);
    const added = this.files.reduce((n, f) => n + f.added, 0);
    const removed = this.files.reduce((n, f) => n + f.removed, 0);

    return (
      <>
        <header>
          <span>Files</span>
          <span class="grow"></span>
          <span class="n">
            {this.files.length} · <span style="color:var(--added)">+{added}</span>{" "}
            <span style="color:var(--removed)">−{removed}</span>
          </span>
        </header>
        <div class="scroll">{this.rows(tree, 0, "")}</div>
      </>
    );
  }
}
