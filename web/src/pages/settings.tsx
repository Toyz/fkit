/**
 * Account settings: profile, password, access tokens, sessions.
 *
 * One component across all four sections rather than four routed pages — they
 * share a rail, a layout and a loading model, and splitting them would mean
 * four copies of that.
 */
import { LoomElement, component, css, styles, reactive, mount, on, inject } from "@toyz/loom";
import { debounce } from "@toyz/loom/element";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { settingsLayout } from "../ui-settings";
import { Session } from "../session";
import { linkHandler, go } from "../nav";
import { confirmAction } from "../components/fkit-dialog";
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

const sheet = css`
  .panel-body { padding: 14px; }
  form.stack { display: flex; flex-direction: column; gap: 12px; }
  .secret {
    display: flex; gap: 8px; align-items: center; margin-top: 4px;
  }
  .secret code {
    flex: 1; font-size: 11.5px; word-break: break-all;
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
      this.error = (e as Error).message;
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
  @reactive accessor copied = false;

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
    if (this.section === "sessions") {
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
    return (
      <fkit-page heading="Profile" value={this.me?.username ?? ""}>
        <fkit-section
          blurb="Your username and display name appear beside your commits and on any repository you own."
        >
          <form
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const display_name = (f.elements.namedItem("display_name") as HTMLInputElement).value;
              const email = (f.elements.namedItem("email") as HTMLInputElement).value;
              void this.act(async () => {
                const next = await api.updateProfile({ display_name, email });
                this.me = next;
                await this.session.load();
              }, "Profile updated");
            }}
          >
            <fkit-field
              label="Username"
              help="Permanent. It is part of every repository URL you own, so changing it would break every clone anyone has taken."
            >
              <input value={u?.username ?? ""} disabled />
            </fkit-field>

            <fkit-field
              label="Display name"
              help="Shown beside your commits. Leave empty to use your username."
            >
              <input name="display_name" value={u?.display_name ?? ""} placeholder="Your name" />
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
        </fkit-section>
      </fkit-page>
    );
  }

  private password() {
    return (
      <fkit-page heading="Password">
        <fkit-section
          blurb="Changing your password signs out every other session, so a stolen one stops working immediately."
        >
          <form
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const at = (n: string) => (f.elements.namedItem(n) as HTMLInputElement).value;
              if (at("next") !== at("again")) {
                this.error = "The new passwords do not match.";
                return;
              }
              void this.act(async () => {
                await api.changePassword(at("current"), at("next"));
                f.reset();
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
              <input name="next" type="password" autocomplete="new-password" required />
            </fkit-field>

            <fkit-field label="Confirm new password">
              <input name="again" type="password" autocomplete="new-password" required />
            </fkit-field>

            <fkit-actions>
              <button class="primary" type="submit" disabled={this.busy}>Change password</button>
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
      <fkit-page heading="Access tokens" value={this.tokens ? `${this.tokens.length} active` : ""}>
        {this.fresh ? (
          <fkit-section heading="Your new token">
            <p class="blurb-warn">Copy it now — it is not shown again.</p>
            <div class="secret">
              <code>{this.fresh.secret}</code>
              <button
                onClick={async () => {
                  await navigator.clipboard.writeText(this.fresh!.secret).catch(() => {});
                  this.copied = true;
                  setTimeout(() => (this.copied = false), 1400);
                }}
              >
                <loom-icon name={this.copied ? "check" : "copy"} size={12}></loom-icon>
                {this.copied ? "Copied" : "Copy"}
              </button>
            </div>
          </fkit-section>
        ) : null}

        <fkit-section
          blurb="Used by the fkit CLI to clone and push. A token can only narrow what you may do — a read-only one cannot push, even to your own repositories."
        >
          <form
            onSubmit={(e: Event) => {
              e.preventDefault();
              const f = e.target as HTMLFormElement;
              const input = f.elements.namedItem("name") as HTMLInputElement;
              const name = input.value.trim();
              if (!name) return;
              void this.act(async () => {
                this.fresh = await api.createToken({ name, can_write: this.canWrite });
                this.copied = false;
                this.tokens = await api.tokens();
                input.value = "";
              });
            }}
          >
            <fkit-add>
              <fkit-field label="Token name">
                <input name="name" placeholder="laptop" required />
              </fkit-field>
              <span class="check">
                <fkit-toggle
                  checked={this.canWrite}
                  label="allow push"
                  onToggle={(e: Event) => (this.canWrite = (e as CustomEvent<boolean>).detail)}
                ></fkit-toggle>
                Allow push
              </span>
              <button class="primary" type="submit" disabled={this.busy}>Generate</button>
            </fkit-add>
          </form>

          <fkit-list heading="Tokens">
            {list === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : list.length === 0 ? (
              <fkit-empty>No tokens yet. Generate one to clone or push from the CLI.</fkit-empty>
            ) : (
              list.map((t) => (
                <fkit-row
                  loom-key={t.id}
                  icon="key"
                  name={t.name}
                  meta={`fkit_pat_${t.prefix}… · ${t.last_used_at ? `last used ${relativeTime(t.last_used_at)}` : "never used"}`}
                >
                  <span class={`tag ${t.can_write ? "on" : ""}`}>
                    {t.can_write ? "read + write" : "read"}
                  </span>
                  <button
                    class="danger bare"
                    disabled={this.busy}
                    onClick={async () => {
                      const ok = await confirmAction({
                        title: `Revoke "${t.name}"?`,
                        body: "Anything using this token stops working immediately. This cannot be undone — you would need to create a new one.",
                        confirm: "Revoke token",
                        danger: true,
                      });
                      if (!ok) return;
                      void this.act(async () => {
                        await api.revokeToken(t.id);
                        this.tokens = await api.tokens();
                      });
                    }}
                  >
                    Revoke
                  </button>
                </fkit-row>
              ))
            )}
          </fkit-list>
        </fkit-section>
      </fkit-page>
    );
  }

  private sessionsSection() {
    const list = this.sessions;
    const others = (list ?? []).filter((x) => !x.current).length;
    return (
      <fkit-page heading="Sessions" value={this.sessions ? `${this.sessions.length} active` : ""}>
        <fkit-section
          blurb="Browsers signed in to this account. Access tokens are listed separately."
        >
          <fkit-list heading="Active sessions">
            {list === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : list.length === 0 ? (
              <fkit-empty>No active sessions.</fkit-empty>
            ) : (
              list.map((sess) => (
                <fkit-row
                  loom-key={sess.id}
                  icon={sess.current ? "check" : "history"}
                  current={sess.current}
                  name={shortAgent(sess.user_agent)}
                  meta={`Signed in ${relativeTime(sess.created_at)} · expires ${relativeTime(sess.expires_at)}`}
                >
                  {sess.current ? <span class="tag on">This browser</span> : null}
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
              ))
            )}
          </fkit-list>
        </fkit-section>

        {others > 0 ? (
          <fkit-section
            heading="Sign out everywhere else"
            blurb={`Ends ${others} other ${others === 1 ? "session" : "sessions"} and leaves this one alone. Changing your password does the same thing.`}
          >
            <fkit-actions>
              <button
                class="danger"
                disabled={this.busy}
                onClick={async () => {
                  const ok = await confirmAction({
                    title: "Sign out everywhere else?",
                    body: `${others} other ${others === 1 ? "session" : "sessions"} will be signed out. This browser stays signed in.`,
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
                Sign out everywhere else
              </button>
              {this.notice ? <span class="ok">{this.notice}</span> : null}
            </fkit-actions>
          </fkit-section>
        ) : null}
      </fkit-page>
    );
  }

  update() {
    return (
      <div class="wrap">
        {this.error ? <div class="error">{this.error}</div> : null}
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
