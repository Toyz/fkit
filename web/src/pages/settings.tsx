/**
 * Account settings: profile, password, access tokens, sessions.
 *
 * One component across all four sections rather than four routed pages — they
 * share a rail, a layout and a loading model, and splitting them would mean
 * four copies of that.
 */
import { LoomElement, component, css, styles, reactive, mount, on, inject } from "@toyz/loom";
import { debounce, clipboard } from "@toyz/loom/element";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { settingsLayout } from "../ui-settings";
import { Session } from "../session";
import { linkHandler, go } from "../nav";
import { confirmAction } from "../components/fkit-dialog";
import { notify } from "../components/fkit-notice";
import "../components/fkit-toggle";
import {
  api,
  relativeTime,
  type NewToken,
  type SessionInfo,
  type Token,
  type User,
} from "../api";

type Section = "profile" | "password" | "tokens" | "sessions";

const SECTIONS: [Section, string, string][] = [
  ["profile", "profile", "repo"],
  ["password", "password", "lock"],
  ["tokens", "access tokens", "key"],
  ["sessions", "sessions", "history"],
];

/**
 * What a token line says about itself: which token it is, whether anything is
 * still using it, and when it stops working.
 *
 * Expiry was missing, which made a page of tokens unable to answer the one
 * question you go there to ask — why did my push stop working.
 */
function tokenMeta(t: Token): string {
  const bits = [`fkit_pat_${t.prefix}…`];
  bits.push(t.last_used_at ? `last used ${relativeTime(t.last_used_at)}` : "never used");
  if (t.expires_at) {
    const done = new Date(t.expires_at).getTime() <= Date.now();
    bits.push(done ? "expired" : `expires ${relativeTime(t.expires_at)}`);
  } else {
    bits.push("never expires");
  }
  return bits.join(" · ");
}

/** Said the same way in the mint form and on every existing token. */
const LINK_HINT =
  "Link commits pushed with this token to your account. Turn it off for a mirror of someone else's work.";

const sheet = css`
  .panel-body { padding: 14px; }
  form.stack { display: flex; flex-direction: column; gap: 12px; }
  /* Making a token: one row, one baseline. */
  .mint {
    display: flex; align-items: center; gap: 10px;
    margin-bottom: 14px; flex-wrap: wrap;
  }
  .mint-name { flex: 1; min-width: 220px; }
  .mint-push {
    display: flex; align-items: center; gap: 7px; flex: none;
    /* The base sheet styles a label as an upper-case caption, which is right
       above a field and wrong beside a switch. */
    text-transform: none; letter-spacing: 0; font-weight: 400;
    font-size: 12px; color: var(--muted); cursor: pointer; user-select: none;
  }

  /* Attribution sits under the row rather than in it. It is the rarer
     choice, and a second toggle beside the first made the row wrap on a
     narrow window and read as two equal decisions, which they are not. */
  .mint-link {
    display: flex; align-items: center; gap: 7px;
    margin: -6px 0 14px; cursor: pointer; user-select: none;
    /* Same reset as .mint-push: the base sheet makes a label an upper-case
       caption, which is right above a field and wrong beside a switch. */
    text-transform: none; letter-spacing: 0; font-weight: 400;
    font-size: 12px; color: var(--muted);
  }
  .mint-link .why { color: var(--faint); }

  /* The right end of a token row: what it can do, whether it links, and the
     one destructive action. Fixed columns, because ragged ones down a list of
     ten tokens read as ten different layouts. */
  .tmeta {
    display: grid; grid-template-columns: 88px 74px auto;
    align-items: center; gap: 10px; flex: none;
  }

  .chip {
    font: inherit; font-family: var(--mono); font-size: 11px;
    display: inline-flex; align-items: center; justify-content: center;
    padding: 3px 0; width: 100%;
    border: 1px solid var(--border); border-radius: var(--radius);
    background: none; color: var(--faint); white-space: nowrap;
  }
  .chip.on { border-color: var(--accent-weak); color: var(--accent); }
  /* Only the second chip is a control, so only it reacts to the pointer. */
  .chip.act { cursor: pointer; }
  .chip.act:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
  .chip.act:disabled { opacity: .5; cursor: default; }

  /* An action, not a state, and it has to read as one. Given a box on hover it
     became a third chip identical to the two beside it, so the thing that
     deletes a credential looked like a label describing one. Bare text
     instead, pinned to the row's right edge so it does not drift with the
     width of the word. */
  .revoke {
    font: inherit; font-family: var(--mono); font-size: 11px;
    justify-self: end; padding: 3px 0; cursor: pointer;
    border: 0; background: none; color: var(--muted);
    text-decoration: underline;
    text-decoration-color: transparent;
    text-underline-offset: 2px;
  }
  .revoke:hover:not(:disabled) {
    color: var(--removed); text-decoration-color: currentColor;
  }
  .revoke:disabled { opacity: .5; cursor: default; text-decoration-color: transparent; }

  /* The token that was just made. Accented because it is the one thing on
     this page that cannot be recovered by reloading. */
  .minted {
    border: 1px solid color-mix(in srgb, var(--accent) 45%, transparent);
    background: color-mix(in srgb, var(--accent) 7%, transparent);
    border-radius: var(--radius);
    padding: 10px 12px 12px; margin-bottom: 16px;
  }
  .minted-top {
    display: flex; align-items: center; gap: 8px; margin-bottom: 8px;
    font-size: 12px; color: var(--muted);
  }
  .minted-top b { color: var(--text); font-weight: 500; }
  .minted-top loom-icon { color: var(--accent); }
  .minted-top .grow { flex: 1; }
  .minted-top .once { color: var(--accent); font-size: 11.5px; }

  .secret { display: flex; gap: 8px; align-items: center; }
  .secret code {
    flex: 1; font-size: 11.5px; word-break: break-all;
    font-family: var(--mono);
    background: var(--bg); border: 1px solid var(--border);
    border-radius: var(--radius); padding: 8px 10px;
  }
  .fresh { border-color: var(--accent); }
  .fresh .panel-head span { color: var(--accent); }

  .row-item {
    display: grid; grid-template-columns: minmax(0, 1fr) auto auto;
    gap: 12px; align-items: center;
    padding: 9px 14px; border-top: 1px solid var(--border);
  }
  .row-item .rt { font-size: 12.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row-item .rs { color: var(--faint); font-size: 11px; margin-top: 2px; }

  .sess {
    display: grid; grid-template-columns: 18px minmax(0, 1fr) auto;
    gap: 11px; align-items: center;
    padding: 10px 14px; border-top: 1px solid var(--border);
  }
  .sess:first-of-type { border-top: 0; }
  .sess .si { color: var(--faint); display: flex; }
  .sess.cur .si { color: var(--accent); }
  .sess .rt { font-size: 12.5px; }
  .sess .rs { display: block; color: var(--faint); font-size: 11px; margin-top: 2px; }
  .new-token { display: flex; gap: 8px; align-items: flex-end; }
  .new-token .field { flex: 1; margin: 0; }
  .check { display: flex; align-items: center; gap: 7px; color: var(--muted); font-size: 12px; }
  .check input { width: auto; }
  .ok { color: var(--added); font-size: 12px; }

  /* The form, and beside it the thing the form produces. */
  .two {
    display: grid; grid-template-columns: minmax(0, 1fr) 250px;
    gap: 40px; align-items: start;
  }
  @media (max-width: 860px) { .two { grid-template-columns: 1fr; gap: 24px; } }

  .card {
    border: 1px solid var(--border); border-radius: var(--radius);
    padding: 18px; text-align: center;
  }
  .card .lbl {
    font-size: 10px; text-transform: uppercase; letter-spacing: .09em;
    color: var(--faint); margin-bottom: 14px;
  }
  .card .nm { font-size: 15px; margin-top: 12px; overflow-wrap: anywhere; }
  .card .dn {
    font-family: var(--sans); color: var(--muted); font-size: 12.5px; margin-top: 3px;
  }
  .card .dn.none { color: var(--faint); font-style: italic; }
  .card .go { display: inline-flex; margin-top: 14px; font-size: 11.5px; }

  .warn { color: var(--removed); font-size: 11.5px; margin-top: 6px; }
`;

abstract class SettingsBase extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor error = "";
  @reactive accessor notice = "";
  @reactive accessor busy = false;

  protected async act(fn: () => Promise<unknown>, ok?: string) {
    this.busy = true;
    this.error = "";
    this.notice = "";
    try {
      await fn();
      if (ok) this.notice = ok;
    } catch (e) {
      // A failed action is shown in front of the page rather than as a banner
      // somewhere on it. The person just pressed something and is looking at
      // where they pressed it — an inline message above the fold they are not
      // reading is how an action comes to look like it silently did nothing.
      // Reported once, in front of the page. Setting the inline banner too
      // would say the same thing twice — that one is for failures nobody
      // asked for, like a listing that would not load.
      void notify({
        title: "That did not happen",
        body: (e as Error).message,
        tone: "error",
      });
    } finally {
      this.busy = false;
    }
  }
}

@route("/settings")
@route("/settings/:section")
@component("page-settings")
@styles(base, settingsLayout, sheet)
export class PageSettings extends SettingsBase {
  @reactive accessor section: Section = "profile";
  @reactive accessor me: User | null = null;
  @reactive accessor tokens: Token[] | null = null;
  @reactive accessor fresh: NewToken | null = null;
  @reactive accessor sessions: SessionInfo[] | null = null;
  @reactive accessor canWrite = true;
  // Not named `attributes`: every custom element already has an `attributes`
  // property, and shadowing it on the class is a real collision, not a
  // stylistic one.
  @reactive accessor linkCommits = true;
  @reactive accessor copied = false;

  /// What the profile form currently holds, so the preview beside it can show
  /// what the change will actually look like before it is saved. Null means
  /// "whatever the server last said" — the two differ only once you type.
  @reactive accessor draftName: string | null = null;

  /// Whether the two new-password fields agree. Checked as you type, because
  /// finding out on submit means retyping both.
  @reactive accessor pwMismatch = false;

  /// Cancelled automatically if the component goes away, so the flash can
  /// never fire into a detached element. Repeated copies restart the timer
  /// rather than stacking one per click.
  @debounce(1400)
  clearCopied() {
    this.copied = false;
  }

  @mount
  init() {
    this.sync();
  }

  /// Bound on connect and released on disconnect by the decorator, so there is
  /// no cleanup to forget.
  @on(window, "popstate")
  private sync() {
    const seg = location.pathname.split("/").filter(Boolean)[1] ?? "profile";
    this.section = (SECTIONS.find(([s]) => s === seg)?.[0] ?? "profile") as Section;
    void this.load();
  }

  private async load() {
    // Waited on, not read. Before the session resolves `isAuthed` is false for
    // a signed-in visitor too, and redirecting on that made every one of these
    // pages impossible to refresh or link to.
    await this.session.ready();
    if (!this.session.isAuthed) {
      // Nothing here is meaningful signed out.
      go("/login");
      return;
    }
    this.me = this.session.current ?? null;
    if (this.section === "tokens" && this.tokens === null) {
      this.tokens = await api.tokens().catch(() => []);
    }
    // The password page states how many sessions it is about to end, so it
    // needs the same list the sessions page does.
    if (this.section === "sessions" || this.section === "password") {
      this.sessions = await api.sessions().catch(() => []);
    }
  }

  private rail() {
    return (
      <div class="rail">
        <h2>account</h2>
        {SECTIONS.map(([id, label, ic]) => {
          const href = `/settings/${id}`;
          return (
            <a class={this.section === id ? "on" : ""} href={href} onClick={linkHandler(href)}>
              <loom-icon name={ic} size={12}></loom-icon>
              {label}
            </a>
          );
        })}
        {this.me?.is_admin ? (
          <>
            <h2 style="margin-top:14px">server</h2>
            <a href="/admin" onClick={linkHandler("/admin")}>
              <loom-icon name="settings" size={12}></loom-icon>
              administration
            </a>
          </>
        ) : null}
      </div>
    );
  }

  private profile() {
    const u = this.me;
    // What the preview shows: what you have typed, or what is stored.
    const shownName = this.draftName ?? u?.display_name ?? "";
    const who = u?.username ?? "";

    return (
      <fkit-page heading="Profile" value={who}>
        <fkit-section blurb="Your username and display name appear beside your commits and on any repository you own.">
          <div class="two">
            <form
              onSubmit={(e: Event) => {
                e.preventDefault();
                const f = e.target as HTMLFormElement;
                const display_name = (f.elements.namedItem("display_name") as HTMLInputElement).value;
                const email = (f.elements.namedItem("email") as HTMLInputElement).value;
                void this.act(async () => {
                  const next = await api.updateProfile({ display_name, email });
                  this.me = next;
                  this.draftName = null;
                  await this.session.load();
                }, "Profile updated");
              }}
            >
              <fkit-field
                label="Username"
                help="Permanent. It is part of every repository URL you own, so changing it would break every clone anyone has taken."
              >
                <input value={who} disabled />
              </fkit-field>

              <fkit-field
                label="Display name"
                help="Shown beside your commits. Leave empty to use your username."
              >
                <input
                  name="display_name"
                  value={u?.display_name ?? ""}
                  placeholder="Your name"
                  onInput={(e: Event) => (this.draftName = (e.target as HTMLInputElement).value)}
                />
              </fkit-field>

              <fkit-field
                label="Email"
                help="Used for password resets. Never shown on a public page."
              >
                <input name="email" type="email" value={u?.email ?? ""} required />
              </fkit-field>

              <fkit-actions>
                <button class="primary" type="submit" disabled={this.busy}>Save profile</button>
                {this.notice ? <span class="ok">{this.notice}</span> : null}
              </fkit-actions>
            </form>

            {/* The page that edits an identity should show that identity.
                Initials are derived from the username, so this is the whole of
                what a visitor sees — there is nothing else to preview. */}
            <div class="card">
              <div class="lbl">how people see you</div>
              <fkit-avatar name={who} size={72}></fkit-avatar>
              <div class="nm">{who}</div>
              <div class={`dn ${shownName ? "" : "none"}`}>
                {shownName || "no display name"}
              </div>
              {who ? (
                <a class="btn go" href={`/${who}`} onClick={linkHandler(`/${who}`)}>
                  <loom-icon name="external" size={11}></loom-icon> view your page
                </a>
              ) : null}
            </div>
          </div>
        </fkit-section>
      </fkit-page>
    );
  }

  private password() {
    // Say what the button will actually do, with the real number.
    const others = (this.sessions ?? []).filter((x) => !x.current).length;
    return (
      <fkit-page heading="Password">
        <fkit-section
          blurb={
            this.sessions === null
              ? "Changing your password signs out every other session, so a stolen one stops working immediately."
              : others === 0
                ? "This is your only session, so nothing else will be signed out."
                : `Changing your password will also sign out your ${others} other ${others === 1 ? "session" : "sessions"}, so a stolen one stops working immediately.`
          }
        >
          <form
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const at = (n: string) => (f.elements.namedItem(n) as HTMLInputElement).value;
              if (at("next") !== at("again")) {
                this.pwMismatch = true;
                return;
              }
              void this.act(async () => {
                await api.changePassword(at("current"), at("next"));
                f.reset();
                this.pwMismatch = false;
              }, "Password changed");
            }}
          >
            <fkit-field label="Current password">
              <input name="current" type="password" autocomplete="current-password" required />
            </fkit-field>

            <fkit-field
              label="New password"
              help="At least 10 characters. Length beats punctuation."
            >
              <input name="next" type="password" autocomplete="new-password" required minLength={10} />
            </fkit-field>

            <fkit-field label="Confirm new password">
              <input
                name="again"
                type="password"
                autocomplete="new-password"
                required
                // Checked as you type: finding out on submit means retyping
                // both fields, since a password field cannot be read back.
                onInput={(e: Event) => {
                  const el = e.target as HTMLInputElement;
                  const form = el.closest("form") as HTMLFormElement;
                  const next = (form.elements.namedItem("next") as HTMLInputElement).value;
                  this.pwMismatch = !!el.value && el.value !== next;
                }}
              />
              {this.pwMismatch ? <div class="warn">The two passwords do not match.</div> : null}
            </fkit-field>

            <fkit-actions>
              <button class="primary" type="submit" disabled={this.busy || this.pwMismatch}>
                Change password
              </button>
              {this.notice ? <span class="ok">{this.notice}</span> : null}
            </fkit-actions>
          </form>
        </fkit-section>
      </fkit-page>
    );
  }

  private tokensSection() {
    const list = this.tokens;
    return (
      <fkit-page heading="Access tokens" value={list ? `${list.length} active` : ""}>
        <fkit-section blurb="Used by the fkit CLI to clone and push. A token can only narrow what you may do — a read-only one cannot push, even to your own repositories.">
          {/* One row, one baseline. This was a labelled field beside a toggle
              beside a button: the label made the field taller than its
              neighbours, so the three sat at three different heights. The
              input says what it wants in its placeholder instead. */}
          <form
            class="mint"
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const input = f.elements.namedItem("name") as HTMLInputElement;
              const name = input.value.trim();
              if (!name) return;
              void this.act(async () => {
                this.fresh = await api.createToken({
                  name,
                  can_write: this.canWrite,
                  // A read-only token pushes nothing, so it has nothing to
                  // attribute. Send the honest value rather than a stale one
                  // left behind by a toggle the user could not see.
                  attributes: this.canWrite && this.linkCommits,
                });
                this.copied = false;
                this.tokens = await api.tokens();
                input.value = "";
              });
            }}
          >
            <input
              name="name"
              class="mint-name"
              placeholder="what is it for — laptop, CI, that server"
              aria-label="Token name"
              required
            />
            <label class="mint-push">
              <fkit-toggle
                checked={this.canWrite}
                label="allow push"
                onToggle={(e: Event) => (this.canWrite = (e as CustomEvent<boolean>).detail)}
              ></fkit-toggle>
              allow push
            </label>
            <button class="primary" type="submit" disabled={this.busy}>
              Generate
            </button>
          </form>

          {/* Only shown for a token that can push: a read-only token never
              delivers a commit, so there is nothing for it to attribute. */}
          {this.canWrite ? (
            <label class="mint-link">
              <fkit-toggle
                checked={this.linkCommits}
                label="link commits to my account"
                onToggle={(e: Event) => (this.linkCommits = (e as CustomEvent<boolean>).detail)}
              ></fkit-toggle>
              link commits to my account
              <span class="why">— off for a mirror of someone else's work</span>
            </label>
          ) : null}

          {/* The token belongs directly under the thing that made it, not in a
              section above it. It is the form's output. */}
          {this.fresh ? (
            <div class="minted">
              <div class="minted-top">
                <loom-icon name="key" size={13}></loom-icon>
                <b>{this.fresh.name}</b>
                <span class={`tag ${this.fresh.can_write ? "on" : ""}`}>
                  {this.fresh.can_write ? "read + write" : "read"}
                </span>
                {this.fresh.can_write && !this.fresh.attributes ? (
                  <span class="tag">unlinked</span>
                ) : null}
                <span class="grow"></span>
                <span class="once">copy it now — it is not shown again</span>
              </div>
              <div class="secret">
                <code>{this.fresh.secret}</code>
                <button
                  class="bare"
                  onClick={() => {
                    this.copySecret(this.fresh!.secret);
                    this.copied = true;
                    setTimeout(() => (this.copied = false), 1400);
                  }}
                >
                  <loom-icon name={this.copied ? "check" : "copy"} size={12}></loom-icon>
                  {this.copied ? "copied" : "copy"}
                </button>
              </div>
            </div>
          ) : null}

          <fkit-list heading="Tokens" count={list ? String(list.length) : ""}>
            {list === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : list.length === 0 ? (
              <fkit-empty>No tokens yet. Generate one to clone or push from the CLI.</fkit-empty>
            ) : (
              list.map((t) => (
                <fkit-row loom-key={t.id} icon="key" name={t.name} meta={tokenMeta(t)}>
                  {/* One shape for both facts about a token, on fixed columns
                      so they line up down the list rather than shuffling with
                      the width of each word. The second is a button because
                      it is the one that can be changed. */}
                  <span class="tmeta">
                    <span class={`chip ${t.can_write ? "on" : ""}`}>
                      {t.can_write ? "read + write" : "read"}
                    </span>

                    {t.can_write ? (
                      <button
                        class={`chip act ${t.attributes ? "on" : ""}`}
                        disabled={this.busy}
                        aria-pressed={String(t.attributes)}
                        title={LINK_HINT}
                        onClick={() =>
                          void this.act(async () => {
                            await api.updateToken(t.id, { attributes: !t.attributes });
                            this.tokens = await api.tokens();
                          })
                        }
                      >
                        {t.attributes ? "linked" : "unlinked"}
                      </button>
                    ) : (
                      <span></span>
                    )}

                    <button
                      class="revoke"
                      disabled={this.busy}
                      onClick={async () => {
                        const ok = await confirmAction({
                          title: `Revoke "${t.name}"?`,
                          effects: [
                            { text: "Anything using this token stops working, immediately" },
                            { text: "Cannot be undone — you would generate a new one" },
                            { text: "Your other tokens are unaffected", tone: "safe" },
                          ],
                          confirm: "revoke token",
                          danger: true,
                        });
                        if (!ok) return;
                        void this.act(async () => {
                          await api.revokeToken(t.id);
                          this.tokens = await api.tokens();
                        });
                      }}
                    >
                      revoke
                    </button>
                  </span>
                </fkit-row>
              ))
            )}
          </fkit-list>
        </fkit-section>
      </fkit-page>
    );
  }

  /// Loom's clipboard, which carries a fallback for where the async API is
  /// unavailable — a token you cannot copy is a token you cannot use.
  @clipboard("write")
  private copySecret(secret: string) {
    return secret;
  }

  private sessionsSection() {
    const list = this.sessions;
    const current = (list ?? []).find((x) => x.current) ?? null;
    const others = (list ?? []).filter((x) => !x.current);

    return (
      <fkit-page heading="Sessions" value={list ? `${list.length} active` : ""}>
        {/* This browser first, and alone. Finding the session you are actually
            using inside a list of fifty identical "Chrome" rows was the whole
            problem with the old page. */}
        <fkit-section
          heading="This browser"
          blurb="The session you are using right now."
        >
          <fkit-list>
            {list === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : current ? (
              this.sessionRow(current)
            ) : (
              <fkit-empty>Signed in with an access token rather than a browser session.</fkit-empty>
            )}
          </fkit-list>
        </fkit-section>

        <fkit-section
          heading="Everywhere else"
          value={list ? `${others.length} ${others.length === 1 ? "session" : "sessions"}` : ""}
          blurb="Other browsers signed in to this account. Access tokens are listed separately, under access tokens."
        >
          {/* The action that ends them all rides the heading, not the bottom
              of the list — with fifty sessions it was a scroll away from the
              thing it acts on. */}
          {others.length ? (
            <button
              slot="action"
              class="danger bare"
              disabled={this.busy}
              onClick={async () => {
                const ok = await confirmAction({
                  title: "Sign out everywhere else?",
                  effects: [
                    {
                      text: `${others.length} other ${
                        others.length === 1 ? "session is" : "sessions are"
                      } signed out`,
                    },
                    { text: "This browser stays signed in", tone: "safe" },
                    { text: "Access tokens are unaffected", tone: "safe" },
                  ],
                  confirm: "Sign out everywhere else",
                  danger: true,
                });
                if (!ok) return;
                await this.act(async () => {
                  const r = await api.revokeOtherSessions();
                  this.sessions = await api.sessions();
                  this.notice = `${r.revoked} session(s) signed out`;
                });
              }}
            >
              Sign out all
            </button>
          ) : null}

          <fkit-list>
            {list === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : others.length === 0 ? (
              <fkit-empty>Nowhere else. This is your only session.</fkit-empty>
            ) : (
              others.map((sess) => this.sessionRow(sess))
            )}
          </fkit-list>
          {this.notice ? <fkit-actions><span class="ok">{this.notice}</span></fkit-actions> : null}
        </fkit-section>
      </fkit-page>
    );
  }

  /// One session, wherever it is listed.
  private sessionRow(sess: SessionInfo) {
    return (
      <fkit-row
        loom-key={sess.id}
        icon={sess.current ? "check" : "history"}
        current={sess.current}
        name={shortAgent(sess.user_agent)}
        meta={`Signed in ${relativeTime(sess.created_at)} · expires ${relativeTime(sess.expires_at)}`}
      >
        <button
          class="danger bare"
          disabled={this.busy}
          onClick={async () => {
            const ok = await confirmAction({
              title: sess.current ? "Sign out of this browser?" : "Revoke this session?",
              body: sess.current
                ? "You will be signed out here and returned to the sign-in page."
                : `${shortAgent(sess.user_agent)} will be signed out immediately.`,
              confirm: sess.current ? "Sign out" : "Revoke",
              danger: true,
            });
            if (!ok) return;
            void this.act(async () => {
              await api.revokeSession(sess.id);
              if (sess.current) {
                await this.session.logout();
                go("/login");
                return;
              }
              this.sessions = await api.sessions();
            });
          }}
        >
          {sess.current ? "Sign out" : "Revoke"}
        </button>
      </fkit-row>
    );
  }

  update() {
    return (
      <div class="wrap">
        {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}
        <div class="cols">
          {this.rail()}
          {this.section === "profile"
            ? this.profile()
            : this.section === "password"
              ? this.password()
              : this.section === "tokens"
                ? this.tokensSection()
                : this.sessionsSection()}
        </div>
      </div>
    );
  }
}

/** A user-agent string is unreadable; the browser name is what identifies it. */
function shortAgent(ua: string | null): string {
  if (!ua) return "unknown browser";
  for (const [needle, label] of [
    ["Edg/", "Edge"],
    ["Firefox/", "Firefox"],
    ["Chrome/", "Chrome"],
    ["Safari/", "Safari"],
    ["fkit", "fkit CLI"],
  ] as const) {
    if (ua.includes(needle)) return label;
  }
  return ua.slice(0, 40);
}
