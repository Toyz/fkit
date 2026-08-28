/**
 * Server administration: instance policy and user accounts.
 *
 * Everything here was previously a config file and a restart. Registration in
 * particular needs to be closable the moment it is being abused, which is not a
 * moment anyone wants to be editing TOML over SSH.
 */
import { LoomElement, component, css, styles, reactive, mount, inject } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { settingsLayout } from "../ui-settings";
import { Session } from "../session";
import { linkHandler, go } from "../nav";
import { confirmAction } from "../components/fkit-dialog";
import "../components/fkit-toggle";
import "../components/fkit-select";
import "../components/fkit-choice";
import {
  api,
  humanSize,
  relativeTime,
  type AdminStats,
  type AdminUser,
  type EmailStatus,
  type Invite,
  type CreatedInvite,
  type InstanceSettings,
} from "../api";

type Section = "overview" | "instance" | "email" | "users" | "invites";

const SECTIONS: [Section, string, string][] = [
  ["overview", "overview", "repo"],
  ["instance", "instance", "settings"],
  ["email", "email", "link"],
  ["users", "users", "key"],
  ["invites", "invites", "branch"],
];

const sheet = css`
  .panel-body { padding: 14px; }
  /* One line per account, columns that line up down the list. The previous
     row stacked a name and an email into a single inline span, so a username
     ran straight into its own address with nothing between them. */
  .urow {
    display: grid;
    /* Every column is fixed except the two text ones. An auto-width action
       column measured its own contents, so the empty header cell resolved
       narrower than the data cells and every heading sat right of its column. */
    grid-template-columns: minmax(120px, 1.1fr) minmax(0, 1.6fr) 52px 96px 64px 120px;
    gap: 12px; align-items: center;
    height: 34px; padding: 0 14px;
    border-top: 1px solid var(--border);
  }
  .urow.head {
    height: 26px; border-top: 0;
    color: var(--faint); font-size: 10.5px;
    text-transform: uppercase; letter-spacing: .07em;
  }
  .urow.head + .urow { border-top: 0; }
  .urow > span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .urow .un { font-size: 12.5px; display: flex; align-items: center; gap: 7px; }
  .urow .ue { color: var(--muted); font-size: 11.5px; }
  .urow .num { color: var(--faint); font-size: 11.5px; text-align: right;
               font-variant-numeric: tabular-nums; }
  .urow .when { color: var(--faint); font-size: 11px; }
  .urow .mid { display: flex; justify-content: center; overflow: visible; }
  .urow .acts { display: flex; gap: 12px; justify-content: flex-end; overflow: visible; }
  .urow.off .un { color: var(--faint); }
  .urow.off .ue, .urow.off .num, .urow.off .when { opacity: .55; }
  @media (max-width: 900px) {
    .urow { grid-template-columns: minmax(90px, 1fr) 44px 64px 120px; }
    .urow .ue, .urow .when, .urow.head span:nth-child(2),
    .urow.head span:nth-child(4) { display: none; }
  }
  .domains { display: flex; flex-direction: column; gap: 6px; }
  .domains input { font-size: 12px; }
  .ok { color: var(--added); font-size: 12px; }
  form.stack { display: flex; flex-direction: column; gap: 13px; }
  .mono { font-family: var(--mono); }

  .empty { padding: 26px 14px; text-align: center; color: var(--faint); font-size: 12px; }

  /* The composer reads as a sentence: invite <who> as <what>, good for <n>. */
  .composer { display: flex; flex-direction: column; gap: 9px; }
  .composer .line { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .composer .line.end { margin-top: 2px; }
  .composer .w { color: var(--faint); font-size: 11.5px; flex: none; }
  .composer .grow { flex: 1; min-width: 200px; margin-top: 0; }
  .composer input {
    flex: 1; min-width: 170px; font-size: 12px; height: 27px; padding: 0 8px;
  }

  /* Segmented control: both options visible, so the one you are not on is
     readable — which is the whole point when one of them grants the server. */
  .seg { display: inline-flex; border: 1px solid var(--border); border-radius: var(--radius); }
  .seg button, .chip {
    font: inherit; font-size: 11.5px; height: 25px; padding: 0 10px;
    background: transparent; color: var(--muted); border: 0; cursor: pointer;
  }
  .seg button + button { border-left: 1px solid var(--border); }
  .seg button.on, .chip.on { background: var(--accent-weak); color: var(--accent); }
  .seg button:hover:not(.on), .chip:hover:not(.on) { color: var(--text); background: var(--raised); }
  .chip {
    border: 1px solid var(--border); border-radius: var(--radius);
    font-variant-numeric: tabular-nums; padding: 0 9px;
  }
  .chip.on { border-color: var(--accent); }

  /* A button that is really a link: no chrome, no weight in the row. */
  .link-btn {
    background: none; border: 0; padding: 0; font: inherit; font-size: 11.5px;
    color: var(--muted); cursor: pointer;
  }
  .link-btn:hover { color: var(--text); background: none; }
  .link-btn.danger:hover { color: var(--removed); }
  .link-btn:disabled { opacity: .5; cursor: not-allowed; }

  /* One row per invite: state, who, note, when, action. */
  .irow {
    display: grid; grid-template-columns: 7px minmax(0, auto) minmax(0, 1fr) auto auto;
    gap: 11px; align-items: center;
    padding: 0 14px; height: 34px;
    border-top: 1px solid var(--border);
  }
  .irow:first-of-type { border-top: 0; }
  .irow .dot {
    width: 6px; height: 6px; border-radius: 50%; background: var(--faint);
  }
  .irow.live .dot { background: var(--accent); }
  .irow.expired .dot { background: var(--removed); opacity: .6; }
  .irow .who {
    font-size: 12.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .irow .who .anon { color: var(--faint); }
  .irow .who .tag { margin-left: 7px; }
  .irow .tag.warn { color: var(--removed); border-color: color-mix(in srgb, var(--removed) 45%, transparent); }
  .irow .note {
    font-family: var(--sans); color: var(--muted); font-size: 11.5px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .irow .when { color: var(--faint); font-size: 11px; white-space: nowrap; }
  .irow.used .who, .irow.used .when { color: var(--faint); }

  /* The one look at a link. Loud on purpose: dismissing it loses the link. */
  .link {
    border: 1px solid var(--accent); border-radius: var(--radius);
    background: color-mix(in srgb, var(--accent) 7%, transparent);
    padding: 12px 14px; margin: 0 0 14px;
  }
  .link .lt {
    display: flex; align-items: center; gap: 7px;
    font-size: 11px; color: var(--accent);
    text-transform: uppercase; letter-spacing: .08em; margin-bottom: 9px;
  }
  .link .lu { display: flex; gap: 8px; align-items: center; }
  .link input {
    flex: 1; min-width: 0; font-family: var(--mono); font-size: 11.5px;
    height: 28px; padding: 0 8px;
  }
`;

@route("/admin")
@route("/admin/:section")
@component("page-admin")
@styles(base, settingsLayout, sheet)
export class PageAdmin extends LoomElement {
  @inject("session") accessor session!: Session;
  @reactive accessor section: Section = "overview";
  @reactive accessor settings: InstanceSettings | null = null;
  @reactive accessor stats: AdminStats | null = null;
  @reactive accessor users: AdminUser[] | null = null;
  @reactive accessor email: EmailStatus | null = null;
  @reactive accessor invites: Invite[] | null = null;
  /** The link is readable exactly once, right after it is made. */
  @reactive accessor fresh: CreatedInvite | null = null;
  @reactive accessor copied = false;
  @reactive accessor inviteAdmin = false;
  @reactive accessor inviteDays = 14;
  @reactive accessor showSpent = false;
  @reactive accessor error = "";
  @reactive accessor notice = "";
  @reactive accessor busy = false;

  @mount
  init() {
    const sync = () => {
      const seg = location.pathname.split("/").filter(Boolean)[1] ?? "overview";
      this.section = (SECTIONS.find(([s]) => s === seg)?.[0] ?? "overview") as Section;
      void this.load();
    };
    sync();
    window.addEventListener("popstate", sync);
    return () => window.removeEventListener("popstate", sync);
  }

  private async load() {
    if (this.session.current === undefined) {
      await this.session.load();
    }
    if (!this.session.current?.is_admin) {
      // Not an administrator: nothing on this page would load anyway.
      go("/");
      return;
    }
    this.error = "";
    try {
      if (this.section === "overview" || this.stats === null) {
        this.stats = await api.adminStats();
      }
      if (this.settings === null) this.settings = await api.adminSettings();
      if (this.section === "users") this.users = await api.adminUsers();
      if (this.section === "email") this.email = await api.adminEmail();
      if (this.section === "invites") {
        this.invites = await api.adminInvites();
        // The email status drives whether we can offer to send the link.
        if (this.email === null) this.email = await api.adminEmail();
      }
    } catch (e) {
      this.error = (e as Error).message;
    }
  }

  private async act(fn: () => Promise<unknown>, ok?: string) {
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

  private async patch(input: Partial<InstanceSettings>) {
    await this.act(async () => {
      this.settings = await api.updateAdminSettings(input);
    }, "saved");
  }

  private rail() {
    return (
      <div class="rail">
        <h2>server</h2>
        {SECTIONS.map(([id, label, ic]) => {
          const href = `/admin/${id}`;
          return (
            <a class={this.section === id ? "on" : ""} href={href} onClick={linkHandler(href)}>
              <loom-icon name={ic} size={12}></loom-icon>
              {label}
            </a>
          );
        })}
        <h2 style="margin-top:14px">account</h2>
        <a href="/settings" onClick={linkHandler("/settings")}>
          <loom-icon name="repo" size={12}></loom-icon>
          your settings
        </a>
      </div>
    );
  }

  private overview() {
    const s = this.stats;
    const cells: [string, string][] = s
      ? [
          [String(s.users), "users"],
          [String(s.admins), "admins"],
          [String(s.repos), "repositories"],
          [String(s.public_repos), "public"],
          [String(s.open_merge_requests), "open merges"],
          [humanSize(s.disk_bytes), "on disk"],
        ]
      : [];

    return (
      <div class="sec">
        <h1>overview</h1>
        <div class="panel">
          {s === null ? (
            <div class="panel-body"><span class="sk" style="width:200px"></span></div>
          ) : (
            <div class="stat-grid">
              {cells.map(([v, l]) => (
                <div class="stat-cell">
                  <b>{v}</b>
                  <span>{l}</span>
                </div>
              ))}
            </div>
          )}
        </div>
        <p class="lead">
          Registration is currently{" "}
          <strong>{this.settings?.open_registration ? "open" : "closed"}</strong>, and public
          repositories are{" "}
          <strong>{this.settings?.require_auth ? "hidden from signed-out visitors" : "readable by anyone"}</strong>.
        </p>
      </div>
    );
  }

  private instance() {
    const s = this.settings;
    if (!s) return <div class="panel"><div class="panel-body">loading</div></div>;

    return (
      <div class="sec">
        <h1>instance</h1>
        <p class="lead">
          These take effect immediately, for every request — no restart, and they override
          what the config file says.
        </p>

        <div class="panel">
          <div class="field-row">
            <div>
              <div class="fl">open registration</div>
              <div class="fd">
                Anyone can create an account. Turn this off for a private server; the first
                account is always allowed so a new server is never locked out.
              </div>
            </div>
            <fkit-toggle
              checked={s.open_registration}
              label="open registration"
              onToggle={(e: Event) =>
                void this.patch({ open_registration: (e as CustomEvent<boolean>).detail })
              }
            ></fkit-toggle>
          </div>

          <div class="field-row">
            <div>
              <div class="fl">require sign-in for everything</div>
              <div class="fd">
                Even repositories marked public become invisible to signed-out visitors. Use
                this for an instance that should not be readable from the internet at all.
              </div>
            </div>
            <fkit-toggle
              checked={s.require_auth}
              label="require sign-in"
              onToggle={(e: Event) =>
                void this.patch({ require_auth: (e as CustomEvent<boolean>).detail })
              }
            ></fkit-toggle>
          </div>

        </div>

        <div class="panel">
          <div class="panel-head"><span>default repository visibility</span></div>
          <fkit-choice
            value={s.default_repo_visibility}
            disabled={this.busy}
            options={[
              { value: "private", label: "private", icon: "lock",
                hint: "New repositories start private unless asked otherwise." },
              { value: "public", label: "public", icon: "repo",
                hint: "New repositories are readable by anyone by default." },
            ]}
            onPick={(e: Event) =>
              void this.patch({
                default_repo_visibility: (e as CustomEvent<string>).detail as "public" | "private",
              })
            }
          ></fkit-choice>
        </div>

        <div class="panel">
          <div class="panel-head"><span>registration email domains</span></div>
          <div class="panel-body">
            <form
              class="domains"
              onSubmit={(e: Event) => {
                e.preventDefault();
                const v = (
                  (e.target as HTMLFormElement).elements.namedItem("domains") as HTMLInputElement
                ).value;
                const list = v.split(",").map((d) => d.trim()).filter(Boolean);
                void this.patch({ allowed_email_domains: list });
              }}
            >
              <input
                name="domains"
                value={s.allowed_email_domains.join(", ")}
                placeholder="example.com, corp.test — blank allows any"
              />
              <div class="fd">
                Comma separated. Only these domains may register. Existing accounts are
                unaffected.
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

  /** A setting the environment owns: shown, explained, not editable. */
  private pinned(value: string, variable: string, note: string) {
    return (
      <div class="fd">
        <span class="mono">{value}</span> — set by <span class="mono">{variable}</span>, which
        overrides anything stored here. {note}
      </div>
    );
  }

  private emailSection() {
    const e = this.email;
    return (
      <div class="sec">
        <h1>email</h1>
        <p class="lead">
          Password resets are the only mail this server sends. Without a key configured
          there is no reset flow at all, and the sign-in page says so rather than offering
          one that cannot work.
        </p>

        <div class="panel">
          <div class="panel-head">
            <span>resend</span>
            <span class={`tag ${e?.configured ? "on" : ""}`}>
              {e?.configured ? "configured" : "not configured"}
            </span>
            {e?.key_from_env ? <span class="tag">from environment</span> : null}
          </div>
          <div class="panel-body">
            <form
              class="stack"
              onSubmit={(e2: Event) => {
                e2.preventDefault();
                const f = e2.target as HTMLFormElement;
                const at = (n: string) => f.elements.namedItem(n) as HTMLInputElement | null;
                const key = at("key"), from = at("from"), url = at("url");
                void this.act(async () => {
                  this.email = await api.updateAdminEmail({
                    // Absent leaves the stored key untouched; the field is
                    // blank because the key is never readable back. A field the
                    // environment pins is not rendered at all, and sending it
                    // would be rejected.
                    ...(key?.value.trim() ? { resend_api_key: key.value.trim() } : {}),
                    ...(from ? { email_from: from.value } : {}),
                    ...(url ? { public_url: url.value } : {}),
                  });
                  if (key) key.value = "";
                }, "saved");
              }}
            >
              <div class="field">
                <label>api key</label>
                {e?.key_from_env ? (
                  this.pinned(
                    "••••••••",
                    "RESEND_API_KEY",
                    "Change it where the server gets its environment, then restart.",
                  )
                ) : (
                  <>
                    <input
                      name="key"
                      type="password"
                      placeholder={e?.has_api_key ? "•••••••• (stored — type to replace)" : "re_..."}
                      autocomplete="off"
                    />
                    <div class="fd">
                      Stored write-only: it is never sent back to this page, so leaving this
                      blank keeps the existing key. From{" "}
                      <span class="mono">resend.com/api-keys</span>. Prefer{" "}
                      <span class="mono">RESEND_API_KEY</span> in the environment if you have
                      somewhere to put it.
                    </div>
                  </>
                )}
              </div>

              <div class="field">
                <label>from address</label>
                {e?.sender_from_env ? (
                  this.pinned(e.email_from, "FKIT_EMAIL_FROM", "")
                ) : (
                  <>
                    <input name="from" value={e?.email_from ?? ""} placeholder="hub@yourdomain.com" />
                    <div class="fd">
                      Must be on a domain you have verified with Resend, or every message is
                      rejected.
                    </div>
                  </>
                )}
              </div>

              <div class="field">
                <label>public url</label>
                {e?.url_from_env ? (
                  this.pinned(e.public_url, "FKIT_PUBLIC_URL", "Reset links are built from it.")
                ) : (
                  <>
                    <input name="url" value={e?.public_url ?? ""} placeholder="https://hub.yourdomain.com" />
                    <div class="fd">
                      Reset links are built from this. Behind a proxy the request's own host is
                      not reliable, so it is stated explicitly.
                    </div>
                  </>
                )}
              </div>

              <div class="row">
                {e?.key_from_env && e?.sender_from_env && e?.url_from_env ? null : (
                  <button class="primary" type="submit" disabled={this.busy}>save</button>
                )}
                <button
                  type="button"
                  disabled={this.busy || !e?.configured}
                  onClick={() =>
                    void this.act(async () => {
                      const r = await api.testAdminEmail();
                      this.notice = `test message sent to ${r.sent_to}`;
                    })
                  }
                >
                  send a test
                </button>
                {this.notice ? <span class="ok">{this.notice}</span> : null}
              </div>
              <div class="fd">
                A test is worth sending: an unverified domain or a key scoped to the wrong
                account both fail silently until someone actually needs a reset.
              </div>
            </form>
          </div>
        </div>
      </div>
    );
  }

  private invitesSection() {
    const open = this.settings?.open_registration ?? true;
    const list = this.invites ?? [];
    const now = Date.now();
    const live = list.filter((i) => !i.used_at && new Date(i.expires_at).getTime() > now);
    const spent = list.filter((i) => i.used_at);
    const dead = list.filter((i) => !i.used_at && new Date(i.expires_at).getTime() <= now);
    const shown = this.showSpent ? list : [...live, ...dead];

    return (
      <div class="sec">
        <h1>invites</h1>
        <p class="lead">
          {open
            ? "Anyone can sign up here, so an invite is a convenience: a link that skips the sign-up page and names who it was for."
            : "Registration is closed, so an invite is the only way in. Each link admits exactly one account, then stops working."}
        </p>

        {this.fresh ? this.freshLink(this.fresh) : null}

        <div class="panel">
          <div class="panel-head">
            <span>issue a link</span>
            <span class="fd">
              {this.email?.configured
                ? `sent from ${this.email.email_from}`
                : "this server cannot send mail — you deliver the link"}
            </span>
          </div>
          <div class="panel-body">{this.composer()}</div>
        </div>

        <div class="panel" style="margin-top:16px">
          <div class="panel-head">
            <span>
              issued
              {list.length ? (
                <span class="fd" style="margin-left:9px">
                  {live.length} outstanding · {spent.length} used
                  {dead.length ? ` · ${dead.length} expired` : ""}
                </span>
              ) : null}
            </span>
            {spent.length ? (
              <button type="button" class="link-btn" onClick={() => (this.showSpent = !this.showSpent)}>
                {this.showSpent ? "hide used" : "show used"}
              </button>
            ) : null}
          </div>
          {this.invites === null ? (
            <div class="panel-body"><span class="sk" style="width:200px"></span></div>
          ) : shown.length === 0 ? (
            <div class="empty">
              {list.length ? "Every invite here has been used." : "No invites yet."}
            </div>
          ) : (
            shown.map((i) => this.inviteRow(i))
          )}
        </div>
      </div>
    );
  }

  /**
   * One line, the way you would say it out loud: invite <someone> as <role>,
   * good for <n> days. A stacked form of four labelled boxes was three times
   * the height for the same four values.
   */
  private composer() {
    return (
      <form
        class="composer"
        onSubmit={(ev: Event) => {
          ev.preventDefault();
          const f = ev.target as HTMLFormElement;
          const at = (n: string) => (f.elements.namedItem(n) as HTMLInputElement).value.trim();
          void this.act(async () => {
            this.fresh = await api.createInvite({
              email: at("email") || undefined,
              note: at("note") || undefined,
              is_admin: this.inviteAdmin,
              expires_days: this.inviteDays,
            });
            this.copied = false;
            this.invites = await api.adminInvites();
            f.reset();
            this.inviteAdmin = false;
          });
        }}
      >
        <div class="line">
          <span class="w">invite</span>
          <input name="email" type="email" placeholder="name@example.com" autocomplete="off" />
          <span class="w">as</span>
          <span class="seg">
            {[
              [false, "member"],
              [true, "administrator"],
            ].map(([v, label]) => (
              <button
                type="button"
                class={this.inviteAdmin === v ? "on" : ""}
                onClick={() => (this.inviteAdmin = v as boolean)}
              >
                {label}
              </button>
            ))}
          </span>
        </div>

        <div class="line">
          <span class="w">good for</span>
          {[3, 7, 14, 30].map((d) => (
            <button
              type="button"
              class={`chip ${this.inviteDays === d ? "on" : ""}`}
              onClick={() => (this.inviteDays = d)}
            >
              {d}d
            </button>
          ))}
          <span class="w">note</span>
          <input name="note" placeholder="optional — who this is for" autocomplete="off" />
        </div>

        <div class="line end">
          <span class="fd grow">
            {this.inviteAdmin
              ? "An administrator arrives with full control of this server, including the ability to invite more."
              : "Leave the address blank for a link anyone you hand it to can use, once."}
          </span>
          <button class="primary" type="submit" disabled={this.busy}>create</button>
        </div>
      </form>
    );
  }

  private freshLink(i: CreatedInvite) {
    return (
      <div class="link">
        <div class="lt">
          <loom-icon name="key" size={12}></loom-icon>
          {i.emailed ? `sent to ${i.email} — and shown here once` : "copy this link now"}
        </div>
        <div class="lu">
          <input
            readonly
            value={i.url}
            onFocus={(e: Event) => (e.target as HTMLInputElement).select()}
          />
          <button
            type="button"
            class={this.copied ? "" : "primary"}
            onClick={() => void navigator.clipboard.writeText(i.url).then(() => (this.copied = true))}
          >
            {this.copied ? "copied" : "copy"}
          </button>
          <button type="button" onClick={() => (this.fresh = null)}>dismiss</button>
        </div>
        <div class="fd" style="margin-top:9px">
          {i.email_error
            ? `The invite exists, but sending failed: ${i.email_error}`
            : "Only a digest is stored, so this cannot be shown again."}
        </div>
      </div>
    );
  }

  private inviteRow(i: Invite) {
    const spent = i.used_at !== null;
    const expired = !spent && new Date(i.expires_at).getTime() < Date.now();
    const state = spent ? "used" : expired ? "expired" : "live";
    return (
      <div class={`irow ${state}`}>
        <span class="dot"></span>
        <span class="who">
          {i.email ?? <span class="anon">open link</span>}
          {i.is_admin ? <span class="tag warn">administrator</span> : null}
        </span>
        <span class="note">{i.note}</span>
        <span class="when">
          {spent
            ? `taken by ${i.used_by ?? "a deleted account"} ${relativeTime(i.used_at!)}`
            : expired
              ? `expired ${relativeTime(i.expires_at)}`
              : `expires ${relativeTime(i.expires_at)}`}
        </span>
        {spent ? (
          <span></span>
        ) : (
          <button
            type="button"
            class="link-btn danger"
            disabled={this.busy}
            onClick={() => {
              void confirmAction({
                title: "revoke this invite?",
                body: `The link stops working immediately. ${
                  i.email ?? "Whoever is holding it"
                } will need a new one.`,
                confirm: "revoke",
                danger: true,
              }).then((yes) => {
                if (!yes) return;
                void this.act(async () => {
                  await api.revokeInvite(i.id);
                  this.invites = await api.adminInvites();
                }, "revoked");
              });
            }}
          >
            revoke
          </button>
        )}
      </div>
    );
  }

  private usersSection() {
    const me = this.session.current;
    const rows = this.users ?? [];
    const admins = rows.filter((u) => u.is_admin && u.is_active).length;
    return (
      <div class="sec">
        <h1>users</h1>
        <p class="lead">
          The last active administrator cannot be demoted, disabled or deleted — a server
          with no administrator cannot be recovered from the web.
        </p>

        <div class="panel">
          <div class="panel-head">
            <span>
              accounts
              {this.users ? (
                <span class="fd" style="margin-left:9px">
                  {rows.length} total · {admins} administrator{admins === 1 ? "" : "s"}
                </span>
              ) : null}
            </span>
          </div>

          {/* A header row, so the toggle column does not need the word "admin"
              repeated on every line to say what it is. */}
          <div class="urow head">
            <span>user</span>
            <span>email</span>
            <span class="num">repos</span>
            <span>joined</span>
            <span class="mid">admin</span>
            <span></span>
          </div>

          {this.users === null
            ? [0, 1, 2].map(() => (
                <div class="urow sk-row">
                  <span><span class="sk tall" style="width:70px"></span></span>
                  <span><span class="sk" style="width:130px"></span></span>
                  <span class="num"><span class="sk" style="width:18px"></span></span>
                  <span><span class="sk" style="width:74px"></span></span>
                  <span class="mid"><span class="sk" style="width:30px"></span></span>
                  <span></span>
                </div>
              ))
            : rows.map((u) => this.userRow(u, u.id === me?.id))}
        </div>
      </div>
    );
  }

  private userRow(u: AdminUser, self: boolean) {
    return (
      <div class={`urow ${u.is_active ? "" : "off"}`}>
        <span class="un">
          {u.username}
          {self ? <span class="tag on">you</span> : null}
          {u.is_active ? null : <span class="tag">disabled</span>}
        </span>
        <span class="ue">{u.email}</span>
        <span class="num">{u.repo_count}</span>
        <span class="when">{relativeTime(u.created_at)}</span>
        <span class="mid">
          <fkit-toggle
            checked={u.is_admin}
            disabled={self || this.busy}
            label={`administrator: ${u.username}`}
            onToggle={(e: Event) =>
              void this.act(async () => {
                await api.updateAdminUser(u.id, {
                  is_admin: (e as CustomEvent<boolean>).detail,
                });
                this.users = await api.adminUsers();
              })
            }
          ></fkit-toggle>
        </span>
        <span class="acts">
          <button
            class="link-btn"
            disabled={self || this.busy}
            onClick={() =>
              void this.act(async () => {
                await api.updateAdminUser(u.id, { is_active: !u.is_active });
                this.users = await api.adminUsers();
              })
            }
          >
            {u.is_active ? "disable" : "enable"}
          </button>
          <button
            class="link-btn danger"
            disabled={self || this.busy}
            onClick={async () => {
              const ok = await confirmAction({
                title: `Delete ${u.username}?`,
                body: `This permanently removes their account and all ${u.repo_count} of their repositories, including every object stored for them. It cannot be undone.`,
                confirm: "delete account",
                danger: true,
                typeToConfirm: u.username,
              });
              if (!ok) return;
              await this.act(async () => {
                await api.deleteAdminUser(u.id);
                this.users = await api.adminUsers();
                this.stats = await api.adminStats();
              });
            }}
          >
            delete
          </button>
        </span>
      </div>
    );
  }

  update() {
    return (
      <div class="wrap">
        {this.error ? <div class="error">{this.error}</div> : null}
        <div class="cols">
          {this.rail()}
          {this.section === "overview"
            ? this.overview()
            : this.section === "instance"
              ? this.instance()
              : this.section === "email"
                ? this.emailSection()
                : this.section === "invites"
                  ? this.invitesSection()
                  : this.usersSection()}
        </div>
      </div>
    );
  }
}
