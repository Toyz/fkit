/**
 * Session state, shared across components via Loom's DI container.
 *
 * `@service` registers a singleton; components pull it in with `@inject`. The
 * reactive `user` field means a login or logout re-renders every component that
 * reads it, without any event plumbing.
 */

import { service } from "@toyz/loom";
import { Reactive } from "@toyz/loom/store";
import { api, type Meta, type User } from "./api";

@service("session")
export class Session {
  /** `undefined` while the initial /auth/me is still in flight. */
  readonly user = new Reactive<User | null | undefined>(undefined);

  /** Instance policy — what this server allows. */
  readonly meta = new Reactive<Meta | null>(null);

  get current(): User | null | undefined {
    return this.user.value;
  }

  /**
   * Whether there is a signed-in user *right now*.
   *
   * False while the answer is still unknown, so this is not a question to
   * redirect on — see [`ready`]. It is fine for deciding what to render,
   * because a render happens again when the answer arrives.
   */
  get isAuthed(): boolean {
    return !!this.user.value;
  }

  /** True once `/auth/me` has answered, whichever way it answered. */
  get resolved(): boolean {
    return this.user.value !== undefined;
  }

  private pending: Promise<void> | null = null;

  /**
   * Resolves once the session is known.
   *
   * Anything that *acts* on whether someone is signed in — a redirect above
   * all — has to wait for this. `isAuthed` is false before the answer arrives,
   * so a guard that reads it directly bounces a signed-in visitor to the login
   * page on any direct navigation or refresh. That was the behaviour of every
   * /settings page.
   *
   * The in-flight promise is shared rather than started again, so several
   * pages asking at once produce one request.
   */
  ready(): Promise<void> {
    if (this.resolved) return Promise.resolve();
    return this.pending ?? this.load();
  }

  /** Resolve the current session once at boot. A 401 is expected, not an error. */
  async load(): Promise<void> {
    if (this.pending) return this.pending;
    this.pending = (async () => {
      // Both are independent; a failure of one must not blank the other.
      const [me, meta] = await Promise.allSettled([api.me(), api.meta()]);
      this.user.set(me.status === "fulfilled" ? me.value : null);
      if (meta.status === "fulfilled") this.meta.set(meta.value);
    })();
    try {
      await this.pending;
    } finally {
      this.pending = null;
    }
  }

  async login(username: string, password: string): Promise<void> {
    this.user.set(await api.login(username, password));
  }

  async register(
    username: string,
    email: string,
    password: string,
    invite?: string,
  ): Promise<void> {
    this.user.set(await api.register(username, email, password, invite));
  }

  async logout(): Promise<void> {
    await api.logout();
    this.user.set(null);
  }
}
