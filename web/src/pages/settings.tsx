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
      <div class="sec">
        <h1>profile</h1>
        <div class="panel">
          <div class="panel-body">
            <form
              class="stack"
              onSubmit={(e: Event) => {
                e.preventDefault();
                const f = e.target as HTMLFormElement;
                const display_name = (f.elements.namedItem("display_name") as HTMLInputElement).value;
                const email = (f.elements.namedItem("email") as HTMLInputElement).value;
                void this.act(async () => {
                  const next = await api.updateProfile({ display_name, email });
                  this.me = next;
                  await this.session.load();
                }, "saved");
              }}
            >
              <div class="field">
                <label>username</label>
                <input value={u?.username ?? ""} disabled />
                <div class="fd">Usernames are permanent — they appear in every repository URL.</div>
              </div>
              <div class="field">
                <label>display name</label>
                <input name="display_name" value={u?.display_name ?? ""} placeholder="Your Name" />
              </div>
              <div class="field">
                <label>email</label>
                <input name="email" type="email" value={u?.email ?? ""} required />
              </div>
              <div class="row">
                <button class="primary" type="submit" disabled={this.busy}>save</button>
                {this.notice ? <span class="ok">{this.notice}</span> : null}
              </div>
            </form>
          </div>
        </div>
      </div>
    );
  }

  private password() {
    return (
      <div class="sec">
        <h1>password</h1>
        <p class="lead">
          Changing it signs out every other session, so a stolen one stops working immediately.
        </p>
        <div class="panel">
          <div class="panel-body">
            <form
              class="stack"
              onSubmit={(e: Event) => {
                e.preventDefault();
                const f = e.target as HTMLFormElement;
                const cur = (f.elements.namedItem("current") as HTMLInputElement);
                const next = (f.elements.namedItem("next") as HTMLInputElement);
                const again = (f.elements.namedItem("again") as HTMLInputElement);
                if (next.value !== again.value) {
                  this.error = "the two new passwords do not match";
                  return;
                }
                void this.act(async () => {
                  await api.changePassword(cur.value, next.value);
                  cur.value = "";
                  next.value = "";
                  again.value = "";
                }, "password changed; other sessions signed out");
              }}
            >
              <div class="field">
                <label>current password</label>
                <input name="current" type="password" autocomplete="current-password" required />
              </div>
              <div class="field">
                <label>new password</label>
                <input name="next" type="password" autocomplete="new-password" required />
                <div class="fd">At least 10 characters. Length beats punctuation.</div>
              </div>
              <div class="field">
                <label>confirm</label>
                <input name="again" type="password" autocomplete="new-password" required />
              </div>
              <div class="row">
                <button class="primary" type="submit" disabled={this.busy}>change password</button>
                {this.notice ? <span class="ok">{this.notice}</span> : null}
              </div>
            </form>
          </div>
        </div>
      </div>
    );
  }

  private tokensSection() {
    return (
      <div class="sec">
        <h1>access tokens</h1>
        <p class="lead">
          Used by the <code>fkit</code> CLI. A token can only narrow what you may do — a
          read-only one cannot push, even to your own repositories.
        </p>

        {this.fresh ? (
          <div class="panel fresh">
            <div class="panel-head"><span>copy this now — it is not shown again</span></div>
            <div class="panel-body">
              <div class="secret">
                <code>{this.fresh.secret}</code>
                <button
                  onClick={async () => {
                    await navigator.clipboard.writeText(this.fresh!.secret).catch(() => {});
                    this.copied = true;
                    this.clearCopied();
                  }}
                >
                  <loom-icon name={this.copied ? "check" : "copy"} size={12}></loom-icon>
                  {this.copied ? "copied" : "copy"}
                </button>
              </div>
              <div class="fd" style="margin-top:8px">
                Use it as <code>FKIT_TOKEN</code>, or{" "}
                <code>fkit config --global token &lt;value&gt;</code>.
              </div>
            </div>
          </div>
        ) : null}

        <div class="panel">
          <div class="panel-body">
            <form
              class="new-token"
              onSubmit={(e: Event) => {
                e.preventDefault();
                const input = (e.target as HTMLFormElement).elements.namedItem(
                  "name",
                ) as HTMLInputElement;
                void this.act(async () => {
                  this.fresh = await api.createToken({
                    name: input.value.trim(),
                    can_write: this.canWrite,
                  });
                  input.value = "";
                  this.copied = false;
                  this.tokens = await api.tokens();
                });
              }}
            >
              <div class="field">
                <label>new token</label>
                <input name="name" placeholder="laptop" required />
              </div>
              <span class="check" style="margin-bottom:7px">
                <fkit-toggle
                  checked={this.canWrite}
                  label="allow push"
                  onToggle={(e: Event) => (this.canWrite = (e as CustomEvent<boolean>).detail)}
                ></fkit-toggle>
                allow push
              </span>
              <button class="primary" type="submit" disabled={this.busy} style="margin-bottom:1px">
                <loom-icon name="key" size={12}></loom-icon> generate
              </button>
            </form>
          </div>

          {this.tokens === null ? (
            <div class="row-item"><span class="sk" style="width:200px"></span></div>
          ) : this.tokens.length === 0 ? (
            <div class="row-item"><span class="muted">no tokens yet</span></div>
          ) : (
            this.tokens.map((t) => (
              <div class="row-item" loom-key={t.id}>
                <span>
                  <span class="rt">{t.name}</span>
                  <span class="rs">
                    fkit_pat_{t.prefix}… · {t.last_used_at ? `used ${relativeTime(t.last_used_at)}` : "never used"}
                  </span>
                </span>
                <span class={`tag ${t.can_write ? "on" : ""}`}>
                  {t.can_write ? "read+write" : "read"}
                </span>
                <button
                  class="danger bare"
                  disabled={this.busy}
                  onClick={async () => {
                    const ok = await confirmAction({
                      title: `Revoke "${t.name}"?`,
                      body: "Anything using this token stops working immediately. This cannot be undone — you would need to create a new one.",
                      confirm: "revoke token",
                      danger: true,
                    });
                    if (!ok) return;
                    await this.act(async () => {
                      await api.revokeToken(t.id);
                      this.tokens = await api.tokens();
                    });
                  }}
                >
                  revoke
                </button>
              </div>
            ))
          )}
        </div>
      </div>
    );
  }

  private sessionsSection() {
    const list = this.sessions;
    const others = (list ?? []).filter((s) => !s.current).length;

    return (
      <div class="sec">
        <h1>sessions</h1>
        <p class="lead">
          Browsers signed in to this account. Access tokens are listed separately.
        </p>

        <div class="panel">
          <div class="panel-head">
            <span>active</span>
            <span class="val faint">{list ? String(list.length) : ""}</span>
          </div>

          {list === null ? (
            <div class="row-item"><span class="sk" style="width:220px"></span></div>
          ) : list.length === 0 ? (
            <div class="row-item"><span class="muted">no active sessions</span></div>
          ) : (
            list.map((s) => (
              <div class={`sess ${s.current ? "cur" : ""}`}>
                <span class="si">
                  <loom-icon name={s.current ? "check" : "history"} size={14}></loom-icon>
                </span>
                <span>
                  <span class="rt">
                    {shortAgent(s.user_agent)}
                    {s.current ? <span class="tag on" style="margin-left:8px">this browser</span> : null}
                  </span>
                  <span class="rs">
                    signed in {relativeTime(s.created_at)} · expires {relativeTime(s.expires_at)}
                  </span>
                </span>
                <button
                  class="danger bare"
                  disabled={this.busy}
                  onClick={async () => {
                    const ok = await confirmAction({
                      title: s.current ? "Sign out of this browser?" : "Revoke this session?",
                      body: s.current
                        ? "You will be signed out here and returned to the sign-in page."
                        : `${shortAgent(s.user_agent)} will be signed out immediately.`,
                      confirm: s.current ? "sign out" : "revoke",
                      danger: true,
                    });
                    if (!ok) return;
                    await this.act(async () => {
                      await api.revokeSession(s.id);
                      if (s.current) {
                        await this.session.logout().catch(() => {});
                        go("/login");
                        return;
                      }
                      this.sessions = await api.sessions();
                    });
                  }}
                >
                  {s.current ? "sign out" : "revoke"}
                </button>
              </div>
            ))
          )}
        </div>

        {others > 0 ? (
          <div class="panel">
            <div class="panel-body">
              <div class="row">
                <button
                  class="danger"
                  disabled={this.busy}
                  onClick={async () => {
                    const ok = await confirmAction({
                      title: "Sign out everywhere else?",
                      body: `${others} other session(s) will be signed out. This browser stays signed in.`,
                      confirm: "sign out others",
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
                  sign out everywhere else
                </button>
                {this.notice ? <span class="ok">{this.notice}</span> : null}
              </div>
              <div class="fd">
                Use this if you think someone else is signed in. Changing your password does
                the same thing automatically.
              </div>
            </div>
          </div>
        ) : null}
      </div>
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
