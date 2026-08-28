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

  get isAuthed(): boolean {
    return !!this.user.value;
  }

  /** Resolve the current session once at boot. A 401 is expected, not an error. */
  async load(): Promise<void> {
    // Both are independent; a failure of one must not blank the other.
    const [me, meta] = await Promise.allSettled([api.me(), api.meta()]);
    this.user.set(me.status === "fulfilled" ? me.value : null);
    if (meta.status === "fulfilled") this.meta.set(meta.value);
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
