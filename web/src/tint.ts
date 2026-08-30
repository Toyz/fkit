/**
 * A stable colour for a name.
 *
 * FNV-1a, because the only requirements are that it spreads short strings and
 * that it gives the same answer everywhere — the same reasons this program
 * identifies everything else by a digest of its content. Two repositories are
 * never the same tile, and nobody has to choose a colour for anything.
 *
 * Lives here rather than inside the avatar because more than the avatar wants
 * it now: a row standing for a repository should carry that repository's
 * colour too, and two implementations of "the colour for this name" would
 * drift the moment either was touched.
 *
 * Only the hue varies. Saturation and lightness are fixed by whoever draws it,
 * so these read as tinted greys with a character each rather than as a box of
 * crayons beside a single teal accent.
 */
export function hueFor(name: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < name.length; i++) {
    h ^= name.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h % 360;
}
