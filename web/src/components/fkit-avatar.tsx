/**
 * A derived tile: initials for a person, a glyph for a thing.
 *
 * Not an avatar service: no external request, no tracking, and it works on a
 * server with no route to the internet — which is the kind of server fkit is
 * usually run on. The initials are derived, so a person never has to upload
 * anything to stop being a grey silhouette.
 *
 * A component rather than two copies of the same CSS, because the profile
 * page and the form that edits it must show the same face — a preview that
 * renders differently from the real thing is worse than no preview.
 */
import { LoomElement, component, css, styles, prop } from "@toyz/loom";
import { hueFor } from "../tint";

const sheet = css`
  :host { display: inline-block; }

  /* The colour is derived from the name, so two people are never the same
     tile and nobody has to upload anything to stop being interchangeable.
     Saturation stays low on purpose: these should read as tinted greys with
     a character each, not as a box of crayons next to a single teal accent.
   */
  .a {
    display: grid; place-items: center;
    width: var(--sz, 34px); height: var(--sz, 34px);
    border-radius: var(--radius);
    background: hsl(var(--h, 174) 26% 13%);
    color: hsl(var(--h, 174) 46% 62%);
    box-shadow: inset 0 0 0 1px hsl(var(--h, 174) 24% 24%);
    font-family: var(--mono); font-weight: 600; text-transform: uppercase;
    letter-spacing: .02em;
    /* Scales with the box, so one component covers 28px and 62px without a
       second prop for the type size. */
    font-size: calc(var(--sz, 34px) * .38);
    user-select: none;
  }
  @media (prefers-color-scheme: light) {
    .a {
      background: hsl(var(--h, 174) 38% 92%);
      color: hsl(var(--h, 174) 44% 30%);
      box-shadow: inset 0 0 0 1px hsl(var(--h, 174) 28% 80%);
    }
  }
`;

@component("fkit-avatar")
@styles(sheet)
export class FkitAvatar extends LoomElement {
  /** The name to derive initials from. */
  @prop accessor name = "";
  /** Pixel size of the square. */
  @prop accessor size = 34;
  /**
   * Draw this icon instead of initials.
   *
   * A repository is not a person and does not have initials — "fk" for
   * `aria/fkit` says nothing — but it does deserve the same derived colour, so
   * that two repositories are told apart by the same means two people are.
   */
  @prop accessor glyph = "";

  /**
   * Two letters. A display name gives one per word ("Ada Lovelace" -> AL); a
   * single word gives its first two, which is what makes usernames legible.
   */
  private initials(): string {
    const words = this.name.trim().split(/[\s._-]+/).filter(Boolean);
    if (!words.length) return "";
    if (words.length === 1) return words[0].slice(0, 2);
    return words[0][0] + words[1][0];
  }


  update() {
    return (
      <span class="a" style={`--sz:${this.size}px;--h:${hueFor(this.name)}`}>
        {this.glyph ? (
          <loom-icon name={this.glyph} size={Math.round(this.size * 0.46)}></loom-icon>
        ) : (
          this.initials()
        )}
      </span>
    );
  }
}
