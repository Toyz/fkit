/**
 * Initials in a box.
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

const sheet = css`
  :host { display: inline-block; }
  .a {
    display: grid; place-items: center;
    width: var(--sz, 34px); height: var(--sz, 34px);
    border-radius: var(--radius);
    background: var(--accent-weak); color: var(--accent);
    font-family: var(--mono); font-weight: 600; text-transform: uppercase;
    letter-spacing: .02em;
    /* Scales with the box, so one component covers 34px and 84px without a
       second prop for the type size. */
    font-size: calc(var(--sz, 34px) * .36);
    user-select: none;
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
      <span class="a" style={`--sz:${this.size}px`}>{this.initials()}</span>
    );
  }
}
