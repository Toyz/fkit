/**
 * Server administration: instance policy and user accounts.
 *
 * Everything here was previously a config file and a restart. Registration in
 * particular needs to be closable the moment it is being abused, which is not a
 * moment anyone wants to be editing TOML over SSH.
 */
import { LoomElement, component, css, styles, reactive, mount, on, inject } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { settingsLayout } from "../ui-settings";
import { Session } from "../session";
import { linkHandler, go } from "../nav";
import { confirmAction } from "../components/fkit-dialog";
import { notify } from "../components/fkit-notice";
import "../components/fkit-toggle";
import "../components/fkit-avatar";
import "../components/fkit-select";
import "../components/fkit-choice";
import {
  api,
  humanSize,
  relativeTime,
  type AdminStats,
  type SystemView,
  type SiteRole,
  type CacheStats,
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
  .hint-after {
    font-size: 11.5px; color: var(--muted); font-family: var(--sans);
    margin: 10px 0 0; line-height: 1.45; max-width: 78ch;
  }

  .urow {
    display: grid;
    /* Every column is fixed except the two text ones. An auto-width action
       column measured its own contents, so the empty header cell resolved
       narrower than the data cells and every heading sat right of its column. */
    grid-template-columns: minmax(150px, 1.1fr) minmax(0, 1.5fr) 64px 104px 132px 132px;
    gap: 14px; align-items: center;
    height: 44px; padding: 0 14px;
    border-top: 1px solid var(--border);
  }
  .urow:not(.head):hover { background: var(--raised); }
  .urow.head {
    height: 26px; border-top: 0;
    color: var(--faint); font-size: 10.5px;
    text-transform: uppercase; letter-spacing: .07em;
  }
  .urow.head + .urow { border-top: 0; }
  /* The list draws the box; the first row must not draw a line against it. */
  fkit-list .urow:first-child { border-top: 0; }
  .urow > span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  /* People get faces. Every other list of accounts on the site has them, and
     without one this read as a table of rows rather than a list of people. */
  .urow .un { font-size: 12.5px; display: flex; align-items: center; gap: 8px; min-width: 0; }
  .urow .un .nm { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .urow .un fkit-avatar { flex: none; }
  .urow .ue { color: var(--muted); font-size: 11.5px; }
  .urow .num { color: var(--faint); font-size: 11.5px; text-align: right;
               font-variant-numeric: tabular-nums; padding-right: 4px; }
  .urow .when { color: var(--faint); font-size: 11px; }
  .urow .mid { display: flex; justify-content: center; overflow: visible; }
  /* Real buttons rather than bare words: these disable and delete accounts,
     and the pair used to sit at the same weight as the text beside them. */
  .urow .acts { display: flex; gap: 14px; justify-content: flex-end; overflow: visible; }
  /* Same treatment as the token list: an action is text that colours and
     underlines, never a box — a box is what a state wears. */
  .urow .acts button {
    font: inherit; font-size: 11.5px; font-family: var(--mono);
    padding: 3px 0; cursor: pointer; border: 0; background: none;
    color: var(--muted);
    text-decoration: underline;
    text-decoration-color: transparent;
    text-underline-offset: 2px;
  }
  .urow .acts button:hover:not(:disabled) {
    color: var(--text); text-decoration-color: currentColor;
  }
  .urow .acts button.danger:hover:not(:disabled) { color: var(--removed); }
  .urow .acts button:disabled { opacity: .35; cursor: default; text-decoration-color: transparent; }

  /* Filter. Worth having before the list is long enough to need scrolling —
     an administrator usually arrives looking for one person by name. */
  .ufilter { display: flex; align-items: center; gap: 10px; margin-bottom: 12px; }
  .ufilter input { flex: 1; font-size: 12px; }
  .ufilter .count { color: var(--faint); font-size: 11.5px; flex: none; }
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
  @reactive accessor system: SystemView | null = null;
  /** Repeating sample, so the CPU share has two points to work from. */
  private ticking: ReturnType<typeof setInterval> | null = null;
  @reactive accessor cache: CacheStats | null = null;
  @reactive accessor users: AdminUser[] | null = null;
  @reactive accessor userFilter = "";
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
    this.sync();
  }

  @on(window, "popstate")
  private sync() {
    const seg = location.pathname.split("/").filter(Boolean)[1] ?? "overview";
    this.section = (SECTIONS.find(([s]) => s === seg)?.[0] ?? "overview") as Section;
    void this.load();
  }

  private async load() {
    await this.session.ready();
    if (!this.session.current?.is_admin) {
      // Not an administrator: nothing on this page would load anyway.
      go("/");
      return;
    }
    this.error = "";
    try {
      if (this.section === "overview" || this.stats === null) {
        this.stats = await api.adminStats();
        this.watchSystem();
        this.cache = await api.cacheStats().catch(() => null);
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

  /**
   * The object cache.
   *
   * The number people actually want is the hit rate: memory held with nothing
   * to show for it is waste, and memory held at 90% hits is the whole point.
   * "Held" rather than "used", because this memory is deliberate — the reason
   * this panel exists is that it otherwise reads as a leak.
   */
  /**
   * Sample the process while this page is open.
   *
   * Twice, at least: the CPU figure is a share of the interval between two
   * asks, so a single reading has nothing to compare against and says so
   * rather than reporting zero.
   */
  private watchSystem() {
    if (this.ticking) return;
    const take = () => {
      void api
        .systemStats()
        .then((v) => {
          this.system = v;
        })
        .catch(() => {});
    };
    take();
    this.ticking = setInterval(take, 4000);
  }

  @mount
  stopSampling() {
    return () => {
      if (this.ticking) clearInterval(this.ticking);
      this.ticking = null;
    };
  }

  private renderSystem() {
    const y = this.system;
    const secs = (n: number) => {
      const d = Math.floor(n / 86400);
      const h = Math.floor((n % 86400) / 3600);
      const m = Math.floor((n % 3600) / 60);
      return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
    };
    const used =
      y && y.memory_total_bytes && y.memory_available_bytes
        ? y.memory_total_bytes - y.memory_available_bytes
        : null;

    return (
      <fkit-section heading="This process">
        <fkit-list>
          {y === null ? (
            <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
          ) : (
            <div class="stat-grid">
              <div class="stat-cell">
                <b>{y.transfers_open}</b>
                <span>transfers in flight</span>
              </div>
              <div class="stat-cell">
                <b>{y.cpu_percent === null ? "—" : `${y.cpu_percent.toFixed(0)}%`}</b>
                <span>of one core</span>
              </div>
              <div class="stat-cell">
                <b>{y.load ? y.load[0].toFixed(2) : "—"}</b>
                <span>load, 1 min</span>
              </div>
              <div class="stat-cell">
                <b>{y.rss_bytes === null ? "—" : humanSize(y.rss_bytes)}</b>
                <span>memory here</span>
              </div>
              <div class="stat-cell">
                <b>{used === null ? "—" : humanSize(used)}</b>
                <span>of {humanSize(y.memory_total_bytes ?? 0)} on the machine</span>
              </div>
              <div class="stat-cell">
                <b>{secs(y.uptime)}</b>
                <span>up</span>
              </div>
              <div class="stat-cell">
                <b>{y.pushes.count.toLocaleString()}</b>
                <span>pushes served</span>
              </div>
              <div class="stat-cell">
                <b>{y.pulls.count.toLocaleString()}</b>
                <span>pulls served</span>
              </div>
              <div class="stat-cell">
                <b>{humanSize(y.pushes.bytes + y.pulls.bytes)}</b>
                <span>moved</span>
              </div>
            </div>
          )}
        </fkit-list>
      </fkit-section>
    );
  }

  private renderCache() {
    const c = this.cache;
    return (
      <fkit-section
        heading="Object cache"
        blurb={
          "Decompressed objects, so a hot one is not read and inflated twice. " +
          "A cached object can never be stale — its key is a digest of its " +
          "value — so this is bounded by size and age rather than invalidated."
        }
      >
        <fkit-list>
          {c === null ? (
            <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
          ) : (
            <div class="stat-grid">
              <div class="stat-cell">
                <b>{c.hit_rate === null ? "—" : `${c.hit_rate.toFixed(1)}%`}</b>
                <span>hit rate</span>
              </div>
              <div class="stat-cell">
                <b>{humanSize(c.bytes)}</b>
                <span>of {humanSize(c.capacity)} held</span>
              </div>
              <div class="stat-cell">
                <b>{c.entries.toLocaleString()}</b>
                <span>objects</span>
              </div>
              <div class="stat-cell">
                <b>{c.hits.toLocaleString()}</b>
                <span>hits</span>
              </div>
              <div class="stat-cell">
                <b>{c.misses.toLocaleString()}</b>
                <span>misses</span>
              </div>
            </div>
          )}
        </fkit-list>

        {c ? (
          <fkit-actions>
            <span class="held">
              held in <b>{c.backend}</b>
              {c.fill >= 0.5 ? ` · ${c.fill.toFixed(0)}% full` : null}
            </span>
            <span class="grow"></span>
            <button
              class="bare"
              disabled={this.busy}
              title="hand the memory back; nothing else changes"
              onClick={() =>
                void this.act(async () => {
                  this.cache = await api.clearCache();
                })
              }
            >
              <loom-icon name="trash" size={12}></loom-icon> clear
            </button>
          </fkit-actions>
        ) : null}
      </fkit-section>
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
      <fkit-page heading="Overview" value={s ? humanSize(s.disk_bytes) + " on disk" : ""}>
        <fkit-section>
          <fkit-list>
            {s === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
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
          </fkit-list>
        </fkit-section>

        {this.renderSystem()}

        {this.renderCache()}

        <fkit-section heading="How this server is set up">
          <fkit-list>
            <fkit-setting-row
              name="Registration"
              why={
                this.settings?.open_registration
                  ? "Open — anyone can create an account."
                  : "Closed — an invite is the only way in."
              }
            >
              <span class={`tag ${this.settings?.open_registration ? "on" : ""}`}>
                {this.settings?.open_registration ? "open" : "closed"}
              </span>
            </fkit-setting-row>
            <fkit-setting-row
              name="Public repositories"
              why={
                this.settings?.require_auth
                  ? "Hidden from signed-out visitors, whatever a repository says about itself."
                  : "Readable by anyone, with or without an account."
              }
            >
              <span class={`tag ${this.settings?.require_auth ? "" : "on"}`}>
                {this.settings?.require_auth ? "sign-in required" : "readable"}
              </span>
            </fkit-setting-row>
          </fkit-list>
        </fkit-section>
      </fkit-page>
    );
  }

  private instance() {
    const s = this.settings;
    if (!s) return <fkit-page heading="Instance"><fkit-section><fkit-list><fkit-empty>loading</fkit-empty></fkit-list></fkit-section></fkit-page>;

    return (
      <fkit-page heading="Instance">
        <fkit-section blurb="These take effect immediately, for every request — no restart, and they override what the config file says.">
          <fkit-list>
            <fkit-setting-row
              name="Open registration"
              why="Anyone can create an account. Turn this off for a private server; the first account is always allowed so a new server is never locked out."
            >
              <fkit-toggle
                checked={s.open_registration}
                label="open registration"
                onToggle={(e: Event) =>
                  void this.patch({ open_registration: (e as CustomEvent<boolean>).detail })
                }
              ></fkit-toggle>
            </fkit-setting-row>

            <fkit-setting-row
              name="Require sign-in for everything"
              why="Even repositories marked public become invisible to signed-out visitors. Use this for an instance that should not be readable from the internet at all."
            >
              <fkit-toggle
                checked={s.require_auth}
                label="require sign-in"
                onToggle={(e: Event) =>
                  void this.patch({ require_auth: (e as CustomEvent<boolean>).detail })
                }
              ></fkit-toggle>
            </fkit-setting-row>
          </fkit-list>
        </fkit-section>

        <fkit-section
          heading="What a new account can do"
          value={s.default_site_role}
          blurb="The role every registration gets. Someone can always be promoted afterwards, and an invitation can name a different one."
        >
          <fkit-list>
            <fkit-choice
              value={s.default_site_role}
              disabled={this.busy}
              options={[
                { value: "observer", label: "Observer", icon: "user",
                  hint: "Read what is public, open issues, comment. Cannot create repositories." },
                { value: "member", label: "Member", icon: "repo",
                  hint: "Everything an observer can do, and create and own repositories." },
              ]}
              onPick={(e: Event) =>
                void this.patch({
                  default_site_role: (e as CustomEvent<string>).detail as SiteRole,
                })
              }
            ></fkit-choice>
          </fkit-list>
        </fkit-section>

        <fkit-section
          heading="Default repository visibility"
          value={s.default_repo_visibility}
          blurb="What a new repository starts as. It can always be changed afterwards."
        >
          <fkit-list>
            <fkit-choice
              value={s.default_repo_visibility}
              disabled={this.busy}
              options={[
                { value: "private", label: "Private", icon: "lock",
                  hint: "New repositories start private unless asked otherwise." },
                { value: "public", label: "Public", icon: "repo",
                  hint: "New repositories are readable by anyone by default." },
              ]}
              onPick={(e: Event) =>
                void this.patch({
                  default_repo_visibility: (e as CustomEvent<string>).detail as "public" | "private",
                })
              }
            ></fkit-choice>
          </fkit-list>
        </fkit-section>

        <fkit-section
          heading="Registration email domains"
          value={s.allowed_email_domains.length ? `${s.allowed_email_domains.length} allowed` : "any"}
          blurb="Only these domains may register. Leave it empty to allow any. Existing accounts are unaffected."
        >
          <fkit-field size="wide">
            <fkit-tags
              value={s.allowed_email_domains}
              placeholder="example.com, corp.test — blank allows any"
              onChange={(e: Event) =>
                void this.patch({ allowed_email_domains: (e as CustomEvent<string[]>).detail })
              }
            ></fkit-tags>
          </fkit-field>
        </fkit-section>
      </fkit-page>
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
    const pinned = e?.key_from_env && e?.sender_from_env && e?.url_from_env;
    return (
      <fkit-page
        heading="Email"
        value={e ? (e.configured ? "configured" : "not configured") : ""}
      >
        <fkit-section blurb="Password resets are the only mail this server sends. Without a key configured there is no reset flow at all, and the sign-in page says so rather than offering one that cannot work.">
          <form
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
              }, "Saved");
            }}
          >
            <fkit-field
              label="Resend API key"
              help={
                e?.key_from_env
                  ? ""
                  : "Stored write-only: it is never sent back to this page, so leaving this blank keeps the existing key. From resend.com/api-keys. Prefer RESEND_API_KEY in the environment if you have somewhere to put it."
              }
            >
              {e?.key_from_env ? (
                this.pinned(
                  "••••••••",
                  "RESEND_API_KEY",
                  "Change it where the server gets its environment, then restart.",
                )
              ) : (
                <input
                  name="key"
                  type="password"
                  placeholder={e?.has_api_key ? "•••••••• (stored — type to replace)" : "re_..."}
                  autocomplete="off"
                />
              )}
            </fkit-field>

            <fkit-field
              label="From address"
              help={
                e?.sender_from_env
                  ? ""
                  : "Must be on a domain you have verified with Resend, or every message is rejected."
              }
            >
              {e?.sender_from_env ? (
                this.pinned(e.email_from, "FKIT_EMAIL_FROM", "")
              ) : (
                <input name="from" value={e?.email_from ?? ""} placeholder="hub@yourdomain.com" />
              )}
            </fkit-field>

            <fkit-field
              label="Public URL"
              help={
                e?.url_from_env
                  ? ""
                  : "Reset links are built from this. Behind a proxy the request's own host is not reliable, so it is stated explicitly."
              }
            >
              {e?.url_from_env ? (
                this.pinned(e.public_url, "FKIT_PUBLIC_URL", "Reset links are built from it.")
              ) : (
                <input name="url" value={e?.public_url ?? ""} placeholder="https://hub.yourdomain.com" />
              )}
            </fkit-field>

            <fkit-actions>
              {pinned ? null : (
                <button class="primary" type="submit" disabled={this.busy}>Save</button>
              )}
              <button
                type="button"
                disabled={this.busy || !e?.configured}
                onClick={() =>
                  void this.act(async () => {
                    const r = await api.testAdminEmail();
                    this.notice = `Test message sent to ${r.sent_to}`;
                  })
                }
              >
                Send a test
              </button>
              {this.notice ? <span class="ok">{this.notice}</span> : null}
            </fkit-actions>
            <p class="hint-after">
              A test is worth sending: an unverified domain or a key scoped to the wrong
              account both fail silently until someone actually needs a reset.
            </p>
          </form>
        </fkit-section>
      </fkit-page>
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
      <fkit-page
        heading="Invites"
        value={this.invites ? `${live.length} outstanding` : ""}
      >
        <fkit-section
          blurb={
            open
              ? "Anyone can sign up here, so an invite is a convenience: a link that skips the sign-up page and names who it was for."
              : "Registration is closed, so an invite is the only way in. Each link admits exactly one account, then stops working."
          }
        >
          {this.fresh ? this.freshLink(this.fresh) : null}
        </fkit-section>

        <fkit-section
          heading="Issue a link"
          value={
            this.email?.configured
              ? `sent from ${this.email.email_from}`
              : "this server cannot send mail — you deliver the link"
          }
        >
          {this.composer()}
        </fkit-section>

        <fkit-section
          heading="Issued"
          value={
            list.length
              ? `${live.length} outstanding · ${spent.length} used${dead.length ? ` · ${dead.length} expired` : ""}`
              : ""
          }
        >
          <fkit-list>
            {spent.length ? (
              <button
                slot="action"
                type="button"
                class="link-btn"
                onClick={() => (this.showSpent = !this.showSpent)}
              >
                {this.showSpent ? "hide used" : "show used"}
              </button>
            ) : null}
            {this.invites === null ? (
              <fkit-empty><span class="sk" style="width:200px"></span></fkit-empty>
            ) : shown.length === 0 ? (
              <fkit-empty>
                {list.length ? "Every invite here has been used." : "No invites yet."}
              </fkit-empty>
            ) : (
              shown.map((i) => this.inviteRow(i))
            )}
          </fkit-list>
        </fkit-section>
      </fkit-page>
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
      <div class={`irow ${state}`} loom-key={i.id}>
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
    const q = this.userFilter.trim().toLowerCase();
    const shown = q
      ? rows.filter(
          (u) => u.username.toLowerCase().includes(q) || u.email.toLowerCase().includes(q),
        )
      : rows;
    return (
      <fkit-page
        heading="Users"
        value={
          this.users
            ? `${rows.length} total · ${admins} administrator${admins === 1 ? "" : "s"}`
            : ""
        }
      >
        <fkit-section blurb="Disabling signs someone out everywhere and stops their access tokens at once; it is undone by enabling them again. The last active administrator cannot be demoted, disabled or deleted — a server with no administrator cannot be recovered from the web.">
          {rows.length > 6 ? (
            <div class="ufilter">
              <input
                type="search"
                placeholder="filter by name or email"
                value={this.userFilter}
                aria-label="Filter users"
                onInput={(e: Event) => (this.userFilter = (e.target as HTMLInputElement).value)}
              />
              <span class="count">
                {shown.length === rows.length
                  ? `${rows.length} shown`
                  : `${shown.length} of ${rows.length}`}
              </span>
            </div>
          ) : null}

          <fkit-list>
            {/* A header row, so the toggle column does not need the word
                "admin" repeated on every line to say what it is. */}
            <div class="urow head">
              <span>user</span>
              <span>email</span>
              <span class="num">repos</span>
              <span>joined</span>
              <span class="mid">role</span>
              <span></span>
            </div>

            {this.users === null
              ? [0, 1, 2].map(() => (
                  <div class="urow sk-row">
                    <span class="un"><span class="sk" style="width:22px;height:22px"></span><span class="sk" style="width:70px"></span></span>
                    <span><span class="sk" style="width:130px"></span></span>
                    <span class="num"><span class="sk" style="width:18px"></span></span>
                    <span><span class="sk" style="width:74px"></span></span>
                    <span class="mid"><span class="sk" style="width:30px"></span></span>
                    <span></span>
                  </div>
                ))
              : shown.length === 0
                ? <fkit-empty>Nobody matches "{this.userFilter}".</fkit-empty>
                : shown.map((u) => this.userRow(u, u.id === me?.id))}
          </fkit-list>
        </fkit-section>
      </fkit-page>
    );
  }

  /** What each role is allowed to do, shown inside the picker itself. */
  private static readonly ROLES = [
    {
      value: "observer",
      label: "observer",
      hint: "Reads what is public, opens issues and comments. Cannot create repositories.",
    },
    {
      value: "member",
      label: "member",
      hint: "Everything an observer can do, and creates and owns repositories.",
    },
    {
      value: "admin",
      label: "admin",
      hint: "Everything a member can do, and administers this server — including reading every repository on it.",
    },
  ];

  private userRow(u: AdminUser, self: boolean) {
    return (
      <div class={`urow ${u.is_active ? "" : "off"}`} loom-key={u.id}>
        <span class="un">
          <fkit-avatar name={u.username} size={22}></fkit-avatar>
          <span class="nm">{u.username}</span>
          {self ? <span class="tag on">you</span> : null}
          {u.is_active ? null : <span class="tag">disabled</span>}
        </span>
        <span class="ue">{u.email}</span>
        <span class="num">{u.repo_count}</span>
        <span class="when">{relativeTime(u.created_at)}</span>
        <span class="mid">
          {/* A toggle could only say administrator or not, and "not" meant
              "can create repositories", which is a second decision it was
              making silently. The picker carries what each role means, so the
              page does not need a key underneath explaining all three. */}
          <fkit-select
            value={u.site_role}
            disabled={self || this.busy}
            options={PageAdmin.ROLES}
            onPick={(e: Event) =>
              void this.act(async () => {
                await api.updateAdminUser(u.id, {
                  site_role: (e as CustomEvent<string>).detail as SiteRole,
                });
                this.users = await api.adminUsers();
              })
            }
          ></fkit-select>
        </span>
        <span class="acts">
          {/* Disabling asks, enabling does not. The ladder is what it costs to
              be wrong, not whether it writes to the database: enabling only
              restores access, disabling signs someone out of everything they
              are in the middle of, and deleting cannot be undone at all — so
              they get nothing, a question, and a question you have to type an
              answer to. */}
          <button
            disabled={self || this.busy}
            title={u.is_active ? `Disable ${u.username}` : `Enable ${u.username}`}
            onClick={async () => {
              if (u.is_active) {
                const ok = await confirmAction({
                  title: `Disable ${u.username}?`,
                  effects: [
                    { text: "Signed out everywhere, immediately" },
                    { text: "Access tokens stop working — a laptop or CI job that pushes will fail" },
                    { text: "Nothing is deleted", tone: "safe" },
                    { text: "Undone whenever you enable them again", tone: "safe" },
                  ],
                  confirm: "disable account",
                });
                if (!ok) return;
              }
              await this.act(async () => {
                await api.updateAdminUser(u.id, { is_active: !u.is_active });
                this.users = await api.adminUsers();
              });
            }}
          >
            {u.is_active ? "disable" : "enable"}
          </button>
          <button
            class="danger"
            disabled={self || this.busy}
            title={`Delete ${u.username}`}
            onClick={async () => {
              const ok = await confirmAction({
                title: `Delete ${u.username}?`,
                effects: [
                  { text: "Their account, permanently" },
                  // Omitted at zero: "all 0 of their repositories" is a line
                  // that makes someone stop and re-read to learn nothing.
                  ...(u.repo_count > 0
                    ? [
                        {
                          text: `All ${u.repo_count} of their ${
                            u.repo_count === 1 ? "repository" : "repositories"
                          }, and every object stored for them`,
                        },
                      ]
                    : []),
                  { text: "Cannot be undone" },
                ],
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
        {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}
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
