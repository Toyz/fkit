/**
 * Typed client for the fkit-hub API.
 *
 * Every call goes through `request`, so authentication (cookies), error shape,
 * and JSON decoding are handled in exactly one place.
 */

export interface User {
  id: string;
  username: string;
  display_name: string | null;
  is_admin: boolean;
  created_at: string;
  email?: string;
}

/** A one-line summary of a commit, as carried by repository listings. */
export interface Head {
  commit: string;
  short: string;
  summary: string;
  author: string;
  timestamp: number;
}

export interface Repo {
  id: string;
  owner: string;
  name: string;
  full_name: string;
  description: string | null;
  visibility: "public" | "private";
  default_branch: string;
  created_at: string;
  updated_at: string;
  access: "none" | "read" | "write" | "admin";
  /** Tip of the default branch; null until something has been pushed. */
  head: Head | null;
  branches: number;
}

export interface Ref {
  name: string;
  target: string;
  short: string;
  updated_at: string;
  is_default: boolean;
}

export interface Entry {
  name: string;
  path: string;
  kind: "dir" | "file" | "exec" | "symlink";
  hash: string;
  size: number;
}

export interface TreeResponse {
  path: string;
  commit: string;
  entries: Entry[];
}

export interface BlobResponse {
  path: string;
  hash: string;
  size: number;
  binary: boolean;
  truncated: boolean;
  content: string | null;
  lines: number;
}

export interface Commit {
  hash: string;
  short: string;
  tree: string;
  parents: string[];
  author: string;
  timestamp: number;
  message: string;
  summary: string;
}

export interface Change {
  status: "added" | "removed" | "modified" | "typechanged";
  path: string;
  old_size: number;
  new_size: number;
}

export interface DiffLine {
  op: " " | "-" | "+";
  old_no: number | null;
  new_no: number | null;
  text: string;
}

export interface Hunk {
  header: string;
  lines: DiffLine[];
}

export interface FileDiff {
  path: string;
  status: "added" | "removed" | "modified" | "typechanged";
  added: number;
  removed: number;
  binary: boolean;
  truncated: boolean;
  too_large: boolean;
  only_line_endings: boolean;
  hunks: Hunk[];
  old_size: number;
  new_size: number;
}

export interface Patch {
  files: FileDiff[];
  /** More files changed than were diffed. */
  truncated: boolean;
}

export interface CommitDetail extends Commit {
  changes: Change[];
}

export interface Token {
  id: string;
  name: string;
  prefix: string;
  can_write: boolean;
  created_at: string;
  last_used_at: string | null;
  expires_at: string | null;
}

export interface NewToken extends Token {
  secret: string;
}

export interface Collaborator {
  user_id: string;
  username: string;
  role: "read" | "write" | "admin";
  granted_at: string;
}

/**
 * The WebSocket URL for a repository.
 *
 * Derived from the page's own protocol: served over HTTPS the sync endpoint is
 * `wss://`, and printing `ws://` there would hand people a command that fails
 * against their own deployment. `location.host` already carries a non-default
 * port, so nothing else needs assembling.
 */
export function syncUrl(owner: string, name: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/${owner}/${name}`;
}

/** An error carrying the server's message and HTTP status. */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    // Session cookies are HttpOnly; the browser attaches them, JS never sees them.
    credentials: "same-origin",
    headers: {
      ...(init.body ? { "Content-Type": "application/json" } : {}),
      ...init.headers,
    },
  });

  if (res.status === 204) return undefined as T;

  const text = await res.text();
  const data = text ? JSON.parse(text) : null;

  if (!res.ok) {
    throw new ApiError(data?.error ?? `request failed (${res.status})`, res.status);
  }
  return data as T;
}

const body = (v: unknown) => JSON.stringify(v);

export interface LastCommit {
  hash: string;
  short: string;
  summary: string;
  author: string;
  timestamp: number;
}

export interface ConflictView {
  path: string;
  kind: "content" | "binary" | "delete-modify" | "type-change";
  detail: string;
}

export interface Comparison {
  base: string;
  head: string;
  merge_base: string | null;
  merge_base_short: string | null;
  commits: Commit[];
  ahead: number;
  behind: number;
  up_to_date: boolean;
  fast_forward: boolean;
  mergeable: boolean;
  conflicts: ConflictView[];
  files: FileDiff[];
  files_truncated: boolean;
}

export interface MergeRequest {
  number: number;
  title: string;
  description: string | null;
  source_branch: string;
  target_branch: string;
  state: "open" | "merged" | "closed";
  author: string | null;
  merge_commit: string | null;
  merged_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface MergeRequestDetail extends MergeRequest {
  /** Recomputed live from the branches on every view. */
  comparison: Comparison | null;
  can_merge: boolean;
}

export interface SessionInfo {
  id: string;
  user_agent: string | null;
  created_at: string;
  expires_at: string;
  /** The browser making the request. */
  current: boolean;
}

export interface InstanceSettings {
  site_name: string;
  open_registration: boolean;
  require_auth: boolean;
  default_repo_visibility: "public" | "private";
  allowed_email_domains: string[];
}

export interface EmailStatus {
  configured: boolean;
  email_from: string;
  public_url: string;
  /** Whether a key is stored. The key itself never leaves the server. */
  has_api_key: boolean;
  key_from_env: boolean;
  sender_from_env: boolean;
  url_from_env: boolean;
}

export interface Profile {
  username: string;
  display_name: string | null;
  is_admin: boolean;
  created_at: string;
  repos: Repo[];
}

export interface Invite {
  id: string;
  email: string | null;
  note: string;
  is_admin: boolean;
  created_by: string | null;
  created_at: string;
  expires_at: string;
  used_at: string | null;
  used_by: string | null;
}

/** A created invite, plus the one and only look at its link. */
export interface CreatedInvite extends Invite {
  url: string;
  emailed: boolean;
  email_error: string | null;
}

export interface AdminUser {
  id: string;
  username: string;
  email: string;
  display_name: string | null;
  is_admin: boolean;
  is_active: boolean;
  created_at: string;
  repo_count: number;
}

export interface AdminStats {
  users: number;
  admins: number;
  repos: number;
  public_repos: number;
  merge_requests: number;
  open_merge_requests: number;
  disk_bytes: number;
}

export interface Meta {
  site_name?: string;
  email_enabled?: boolean;
  open_registration: boolean;
  require_auth: boolean;
  default_repo_visibility: "public" | "private";
}

export const api = {
  /** Instance policy. Readable without authentication. */
  meta: () => request<Meta>("/meta"),

  // auth
  me: () => request<User>("/auth/me"),
  login: (username: string, password: string) =>
    request<User>("/auth/login", { method: "POST", body: body({ username, password }) }),
  register: (username: string, email: string, password: string, invite?: string) =>
    request<User>("/auth/register", {
      method: "POST",
      body: body({ username, email, password, ...(invite ? { invite } : {}) }),
    }),
  logout: () => request<{ ok: boolean }>("/auth/logout", { method: "POST" }),
  forgotPassword: (email: string) =>
    request<{ ok: boolean; message: string }>("/auth/forgot", {
      method: "POST",
      body: body({ email }),
    }),
  resetPassword: (token: string, password: string) =>
    request<{ ok: boolean; message: string }>("/auth/reset", {
      method: "POST",
      body: body({ token, password }),
    }),

  // repos
  repos: () => request<Repo[]>("/repos"),
  profile: (username: string) => request<Profile>(`/users/${encodeURIComponent(username)}`),
  repo: (owner: string, name: string) => request<Repo>(`/repos/${owner}/${name}`),
  createRepo: (input: { name: string; description?: string; visibility: string }) =>
    request<Repo>("/repos", { method: "POST", body: body(input) }),
  updateRepo: (owner: string, name: string, input: Record<string, unknown>) =>
    request<Repo>(`/repos/${owner}/${name}`, { method: "PATCH", body: body(input) }),
  deleteRepo: (owner: string, name: string) =>
    request<{ ok: boolean }>(`/repos/${owner}/${name}`, { method: "DELETE" }),

  refs: (owner: string, name: string) => request<Ref[]>(`/repos/${owner}/${name}/refs`),

  // content
  tree: (owner: string, name: string, ref: string, path = "") =>
    request<TreeResponse>(
      `/repos/${owner}/${name}/tree/${encodeURIComponent(ref)}${path ? "/" + path : ""}`,
    ),
  blob: (owner: string, name: string, ref: string, path: string) =>
    request<BlobResponse>(`/repos/${owner}/${name}/blob/${encodeURIComponent(ref)}/${path}`),
  commits: (owner: string, name: string, ref: string, limit = 50, skip = 0) =>
    request<Commit[]>(
      `/repos/${owner}/${name}/commits/${encodeURIComponent(ref)}?limit=${limit}&skip=${skip}`,
    ),
  commit: (owner: string, name: string, hash: string) =>
    request<CommitDetail>(`/repos/${owner}/${name}/commit/${hash}`),
  /** Most recent commit touching each entry of a directory. */
  lastCommits: (owner: string, name: string, ref: string, path = "") =>
    request<Record<string, LastCommit>>(
      `/repos/${owner}/${name}/lastcommits/${encodeURIComponent(ref)}${path ? "/" + path : ""}`,
    ),

  /** URL for the raw bytes of a file. Not fetched through `request`: this is a
   *  link the browser navigates to, and the server sets its own headers. */
  rawUrl: (owner: string, name: string, ref: string, path: string) =>
    `/api/repos/${owner}/${name}/raw/${encodeURIComponent(ref)}/${path}`,

  patch: (owner: string, name: string, hash: string) =>
    request<Patch>(`/repos/${owner}/${name}/patch/${hash}`),

  compare: (owner: string, name: string, base: string, head: string) =>
    request<Comparison>(
      `/repos/${owner}/${name}/compare/${encodeURIComponent(base)}/${encodeURIComponent(head)}`,
    ),

  merges: (owner: string, name: string, state = "open") =>
    request<MergeRequest[]>(`/repos/${owner}/${name}/merges?state=${state}`),
  mergeRequest: (owner: string, name: string, number: number) =>
    request<MergeRequestDetail>(`/repos/${owner}/${name}/merges/${number}`),
  createMerge: (
    owner: string,
    name: string,
    input: { title: string; description?: string; source_branch: string; target_branch: string },
  ) => request<MergeRequest>(`/repos/${owner}/${name}/merges`, { method: "POST", body: body(input) }),
  performMerge: (owner: string, name: string, number: number, message?: string) =>
    request<MergeRequest>(`/repos/${owner}/${name}/merges/${number}/merge`, {
      method: "POST",
      body: body({ message }),
    }),
  closeMerge: (owner: string, name: string, number: number) =>
    request<MergeRequest>(`/repos/${owner}/${name}/merges/${number}/close`, { method: "POST" }),
  reopenMerge: (owner: string, name: string, number: number) =>
    request<MergeRequest>(`/repos/${owner}/${name}/merges/${number}/reopen`, { method: "POST" }),

  readme: (owner: string, name: string, ref: string) =>
    request<{ name: string; content: string } | null>(
      `/repos/${owner}/${name}/readme/${encodeURIComponent(ref)}`,
    ),

  // account
  updateProfile: (input: { display_name?: string; email?: string }) =>
    request<User>("/auth/me", { method: "PATCH", body: body(input) }),
  changePassword: (current: string, next: string) =>
    request<{ ok: boolean }>("/auth/password", {
      method: "POST",
      body: body({ current, new: next }),
    }),
  sessions: () => request<SessionInfo[]>("/auth/sessions"),
  revokeSession: (id: string) =>
    request<{ ok: boolean }>(`/auth/sessions/${id}`, { method: "DELETE" }),
  revokeOtherSessions: () =>
    request<{ revoked: number }>("/auth/sessions", { method: "DELETE" }),

  // administration
  adminSettings: () => request<InstanceSettings>("/admin/settings"),
  updateAdminSettings: (input: Partial<InstanceSettings>) =>
    request<InstanceSettings>("/admin/settings", { method: "PATCH", body: body(input) }),
  adminStats: () => request<AdminStats>("/admin/stats"),
  adminEmail: () => request<EmailStatus>("/admin/email"),
  updateAdminEmail: (input: {
    email_from?: string;
    public_url?: string;
    resend_api_key?: string;
  }) => request<EmailStatus>("/admin/email", { method: "PATCH", body: body(input) }),
  testAdminEmail: () => request<{ sent_to: string }>("/admin/email/test", { method: "POST" }),
  adminUsers: () => request<AdminUser[]>("/admin/users"),
  adminInvites: () => request<Invite[]>("/admin/invites"),
  createInvite: (input: {
    email?: string;
    note?: string;
    is_admin?: boolean;
    expires_days?: number;
  }) => request<CreatedInvite>("/admin/invites", { method: "POST", body: body(input) }),
  revokeInvite: (id: string) =>
    request<{ ok: boolean }>(`/admin/invites/${id}`, { method: "DELETE" }),
  /** Is a `?invite=` token worth showing a registration form for? */
  peekInvite: (token: string) =>
    request<{ valid: boolean; email: string | null }>(
      `/auth/invite?token=${encodeURIComponent(token)}`,
    ),
  updateAdminUser: (id: string, input: { is_admin?: boolean; is_active?: boolean }) =>
    request<AdminUser>(`/admin/users/${id}`, { method: "PATCH", body: body(input) }),
  deleteAdminUser: (id: string) =>
    request<{ ok: boolean }>(`/admin/users/${id}`, { method: "DELETE" }),

  // tokens
  tokens: () => request<Token[]>("/tokens"),
  createToken: (input: { name: string; can_write: boolean; expires_in_days?: number }) =>
    request<NewToken>("/tokens", { method: "POST", body: body(input) }),
  revokeToken: (id: string) => request<{ ok: boolean }>(`/tokens/${id}`, { method: "DELETE" }),

  // collaborators
  collaborators: (owner: string, name: string) =>
    request<Collaborator[]>(`/repos/${owner}/${name}/collaborators`),
  addCollaborator: (owner: string, name: string, username: string, role: string) =>
    request<{ ok: boolean }>(`/repos/${owner}/${name}/collaborators`, {
      method: "POST",
      body: body({ username, role }),
    }),
  removeCollaborator: (owner: string, name: string, username: string) =>
    request<{ ok: boolean }>(`/repos/${owner}/${name}/collaborators/${username}`, {
      method: "DELETE",
    }),
};

// ---- small shared formatters -------------------------------------------

export function humanSize(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

export function relativeTime(unixOrIso: number | string): string {
  const t = typeof unixOrIso === "number" ? unixOrIso * 1000 : Date.parse(unixOrIso);
  const delta = (Date.now() - t) / 1000;
  // Expiry dates are in the future. Clamping those to zero rendered every
  // outstanding invite as "just now".
  const ahead = delta < 0;
  const secs = Math.abs(delta);
  // Each pair is "divide by this to reach that unit". The labels were
  // previously shifted one place earlier, so every value came out naming the
  // unit below the one it had actually been converted into: an 83-minute-old
  // commit read "1 minute ago", and anything under an hour read "just now".
  const steps: [number, string][] = [
    [60, "minute"],
    [60, "hour"],
    [24, "day"],
    [30, "month"],
    [12, "year"],
  ];
  let v = secs;
  let unit = "second";
  for (const [div, next] of steps) {
    if (v < div) break;
    v /= div;
    unit = next;
  }
  const n = Math.floor(v);
  if (unit === "second" && n < 30) return ahead ? "any moment" : "just now";
  const span = `${n} ${unit}${n === 1 ? "" : "s"}`;
  return ahead ? `in ${span}` : `${span} ago`;
}

/** Strip the `<...>` email from a commit author string. */
export function authorName(author: string): string {
  const i = author.indexOf("<");
  return (i === -1 ? author : author.slice(0, i)).trim() || author;
}
