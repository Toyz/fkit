/** Sign in and create account. */
import { LoomElement, component, css, styles, reactive, mount, inject } from "@toyz/loom";
import { route } from "@toyz/loom/router";
import { base } from "../ui";
import { Session } from "../session";
import { go, linkHandler } from "../nav";
import { api, ApiError, type Meta } from "../api";

const styleSheet = css`
  .box { max-width: 340px; margin: 12vh auto 0; }
  .panel-body { padding: 18px; }
  h1 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: var(--muted); margin-bottom: 14px; }
  button[type="submit"] { width: 100%; justify-content: center; margin-top: 6px; }
  .alt { margin-top: 12px; color: var(--faint); font-size: 12px; text-align: center; }
  .hint { color: var(--faint); font-size: 11px; margin-top: 4px; font-family: var(--sans); }
`;

abstract class AuthPage extends LoomElement {
  @inject("session") accessor session!: Session;

  /**
   * A local copy of instance policy.
   *
   * `session.meta` is a store, and reading `.value` inside `update()` does not
   * subscribe — so a page rendered before the first `/api/meta` lands keeps
   * whatever it assumed forever. That is how a closed server went on showing a
   * sign-up form that could only ever fail.
   */
  @reactive accessor meta: Meta | null = null;

  @mount
  watchMeta() {
    this.meta = this.session.meta.value;
    const off = this.session.meta.subscribe((m: Meta | null) => (this.meta = m));
    // The page may be the first thing rendered, before anyone has asked.
    if (this.session.meta.value === null) void this.session.load();
    return off;
  }
  /** Offering a reset flow the server cannot deliver is worse than omitting it. */
  protected emailEnabled(): boolean {
    return this.meta?.email_enabled !== false;
  }
  protected signupOpen(): boolean {
    return this.meta?.open_registration !== false;
  }
  /**
   * A server with no accounts yet.
   *
   * The API lets the first registration through whatever the policy says, so
   * that an instance deployed with registration already closed can still be
   * claimed. Without this the sign-up page would hide the only form that
   * works, and a correctly-configured private server would be unusable.
   */
  protected needsSetup(): boolean {
    return this.meta?.needs_setup === true;
  }
  @reactive accessor error = "";
  @reactive accessor busy = false;

  protected async attempt(fn: () => Promise<void>) {
    this.error = "";
    this.busy = true;
    try {
      await fn();
      go("/");
    } catch (e) {
      this.error =
        e instanceof ApiError
          ? e.status === 401
            ? "Incorrect username or password."
            : e.message
          : "Something went wrong. Try again.";
    } finally {
      this.busy = false;
    }
  }

  protected field(el: EventTarget | null, name: string): string {
    const form = (el as HTMLElement)?.closest("form");
    return (form?.elements.namedItem(name) as HTMLInputElement)?.value ?? "";
  }
}

@route("/login")
@component("page-login")
@styles(base, styleSheet)
export class PageLogin extends AuthPage {
  update() {
    return (
      <div class="wrap">
        <div class="box">
          <div class="panel"><div class="panel-body">
            <h1>sign in</h1>
            {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}
            <form
              onSubmit={(e: Event) => {
                e.preventDefault();
                const t = e.target;
                void this.attempt(() =>
                  this.session.login(this.field(t, "username"), this.field(t, "password")),
                );
              }}
            >
              <div class="field">
                <label>username</label>
                <input name="username" autocomplete="username" autofocus required />
              </div>
              <div class="field">
                <label>password</label>
                <input name="password" type="password" autocomplete="current-password" required />
              </div>
              <button class="primary" type="submit" disabled={this.busy}>
                {this.busy ? "signing in…" : "sign in"}
              </button>
            </form>
          </div></div>
          {this.emailEnabled() ? (
            <div class="alt">
              <a href="/forgot" onClick={linkHandler("/forgot")}>forgot your password?</a>
            </div>
          ) : null}
          {this.needsSetup() ? (
            <div class="alt">
              <a href="/register" onClick={linkHandler("/register")}>
                claim this server — create the administrator account
              </a>
            </div>
          ) : this.signupOpen() ? (
            <div class="alt">
              no account? <a href="/register" onClick={linkHandler("/register")}>register</a>
            </div>
          ) : null}
        </div>
      </div>
    );
  }
}

@route("/register")
@component("page-register")
@styles(base, styleSheet)
export class PageRegister extends AuthPage {
  /** From `?invite=`. Present means this page may work even when sign-up is closed. */
  private invite = new URLSearchParams(location.search).get("invite") ?? "";
  /** null = still checking, false = no good, string|"" = valid (bound address or open). */
  @reactive accessor inviteEmail: string | null | false = null;

  @mount
  checkInvite() {
    if (!this.invite) {
      this.inviteEmail = "";
      return;
    }
    void api
      .peekInvite(this.invite)
      .then((r) => (this.inviteEmail = r.valid ? (r.email ?? "") : false))
      // A network failure is not a verdict on the token; let the form try.
      .catch(() => (this.inviteEmail = ""));
  }

  update() {
    if (this.invite && this.inviteEmail === null) {
      return (
        <div class="wrap">
          <div class="box">
            <div class="panel"><div class="panel-body">
              <h1>checking your invitation…</h1>
            </div></div>
          </div>
        </div>
      );
    }

    if (this.inviteEmail === false) {
      return (
        <div class="wrap">
          <div class="box">
            <div class="panel"><div class="panel-body">
              <h1>this invitation is no longer valid</h1>
              <p class="hint">
                It has been used, revoked, or has expired. Invitations admit exactly one
                account — ask whoever sent it for a new link.
              </p>
              <a class="btn" href="/login" onClick={linkHandler("/login")}>sign in</a>
            </div></div>
          </div>
        </div>
      );
    }

    if (!this.invite && !this.signupOpen() && !this.needsSetup()) {
      return (
        <div class="wrap">
          <div class="box">
            <div class="panel"><div class="panel-body">
              <h1>registration is closed</h1>
              <p class="hint">
                This server does not take public sign-ups. An administrator can send you an
                invitation link, which creates an account without opening the door to
                everyone.
              </p>
              <a class="btn" href="/login" onClick={linkHandler("/login")}>sign in</a>
            </div></div>
          </div>
        </div>
      );
    }

    // An invite bound to an address is not transferable, so the field is fixed.
    const bound = this.invite ? this.inviteEmail : "";

    return (
      <div class="wrap">
        <div class="box">
          <div class="panel"><div class="panel-body">
            <h1>create account</h1>
            <div class="hint" style="margin:-8px 0 14px">
              {this.invite
                ? "You were invited to this server. The link works once."
                : this.needsSetup()
                  ? "This server has no accounts yet. This one becomes its administrator, and it is allowed even though registration is closed."
                  : "The first account on a new server becomes its administrator."}
            </div>
            {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}
            <form
              onSubmit={(e: Event) => {
                e.preventDefault();
                const t = e.target;
                void this.attempt(() =>
                  this.session.register(
                    this.field(t, "username"),
                    bound || this.field(t, "email"),
                    this.field(t, "password"),
                    this.invite || undefined,
                  ),
                );
              }}
            >
              <div class="field">
                <label>username</label>
                <input name="username" autocomplete="username" autofocus required />
                <div class="hint">Lower-case letters, digits, dot, underscore or hyphen.</div>
              </div>
              <div class="field">
                <label>email</label>
                {bound ? (
                  <>
                    <input value={bound} disabled />
                    <div class="hint">
                      The invitation was sent here, and only this address can use it.
                    </div>
                  </>
                ) : (
                  <input name="email" type="email" autocomplete="email" required />
                )}
              </div>
              <div class="field">
                <label>password</label>
                <input name="password" type="password" autocomplete="new-password" required />
                <div class="hint">At least 10 characters. Length beats punctuation.</div>
              </div>
              <button class="primary" type="submit" disabled={this.busy}>
                {this.busy ? "creating…" : "create account"}
              </button>
            </form>
          </div></div>
          <div class="alt">
            already registered? <a href="/login" onClick={linkHandler("/login")}>sign in</a>
          </div>
        </div>
      </div>
    );
  }
}


@route("/forgot")
@component("page-forgot")
@styles(base, styleSheet)
export class PageForgot extends AuthPage {
  @reactive accessor sent = "";

  update() {
    return (
      <div class="wrap">
        <div class="box">
          <div class="panel"><div class="panel-body">
            <h1>reset your password</h1>
            {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}

            {this.sent ? (
              <div>
                <p class="hint" style="font-size:12.5px">{this.sent}</p>
                <p class="hint">
                  The link works once and expires in 30 minutes. Check spam if it does not
                  arrive.
                </p>
              </div>
            ) : (
              <form
                onSubmit={(e: Event) => {
                  e.preventDefault();
                  const email = this.field(e.target, "email");
                  this.error = "";
                  this.busy = true;
                  void api
                    .forgotPassword(email)
                    .then((r) => (this.sent = r.message))
                    .catch((err) => (this.error = (err as Error).message))
                    .finally(() => (this.busy = false));
                }}
              >
                <div class="field">
                  <label>email</label>
                  <input name="email" type="email" autofocus required />
                  <div class="hint">
                    We will send a link if an account exists — the answer is deliberately the
                    same either way, so this page cannot be used to test addresses.
                  </div>
                </div>
                <button class="primary" type="submit" disabled={this.busy}>
                  {this.busy ? "sending…" : "send reset link"}
                </button>
              </form>
            )}
          </div></div>
          <div class="alt">
            <a href="/login" onClick={linkHandler("/login")}>back to sign in</a>
          </div>
        </div>
      </div>
    );
  }
}

@route("/reset")
@component("page-reset")
@styles(base, styleSheet)
export class PageReset extends AuthPage {
  private token(): string {
    return new URLSearchParams(location.search).get("token") ?? "";
  }

  update() {
    const token = this.token();
    if (!token) {
      return (
        <div class="wrap">
          <div class="box">
            <div class="panel"><div class="panel-body">
              <h1>reset link missing</h1>
              <p class="hint">
                This page needs the link from your email. Request a new one if you no longer
                have it.
              </p>
              <a class="btn" href="/forgot" onClick={linkHandler("/forgot")}>request a link</a>
            </div></div>
          </div>
        </div>
      );
    }

    return (
      <div class="wrap">
        <div class="box">
          <div class="panel"><div class="panel-body">
            <h1>choose a new password</h1>
            {this.error ? <fkit-notice message={this.error}></fkit-notice> : null}
            <form
              onSubmit={(e: Event) => {
                e.preventDefault();
                const t = e.target;
                const pw = this.field(t, "password");
                if (pw !== this.field(t, "again")) {
                  this.error = "the two passwords do not match";
                  return;
                }
                void this.attempt(async () => {
                  await api.resetPassword(token, pw);
                  await this.session.load();
                });
              }}
            >
              <div class="field">
                <label>new password</label>
                <input name="password" type="password" autocomplete="new-password" autofocus required />
                <div class="hint">At least 10 characters.</div>
              </div>
              <div class="field">
                <label>confirm</label>
                <input name="again" type="password" autocomplete="new-password" required />
              </div>
              <button class="primary" type="submit" disabled={this.busy}>
                {this.busy ? "saving…" : "set password"}
              </button>
              <div class="hint" style="margin-top:10px">
                Every other session and access token is revoked, so whoever else had access
                loses it.
              </div>
            </form>
          </div></div>
        </div>
      </div>
    );
  }
}
