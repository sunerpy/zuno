import { EnterpriseClient } from "@zuno/enterprise-sdk";

let browserContext: string | undefined;
export const client = new EnterpriseClient({ baseUrl: new URL("/app/api/v1/", location.origin).href, browserContext: () => browserContext });
export function bindIdentity(value: BrowserIdentity | null): void {
  browserContext = value ? JSON.stringify([value.tenantId, value.principalId, value.clientId]) : undefined;
}

export interface BrowserIdentity {
  tenantId: string;
  principalId: string;
  clientId: string;
  expiresAtSeconds: number;
}
export async function identity(signal?: AbortSignal): Promise<BrowserIdentity> {
  const response = await fetch("/auth/session", { credentials: "same-origin", cache: "no-store", redirect: "error", signal });
  if (!response.ok) throw new Error(response.status === 401 ? "signed_out" : "identity_unavailable");
  const value: unknown = await response.json();
  if (!value || typeof value !== "object" || !("principalId" in value) || typeof value.principalId !== "string"
    || !("tenantId" in value) || typeof value.tenantId !== "string"
    || !("clientId" in value) || typeof value.clientId !== "string"
    || !("expiresAtSeconds" in value) || typeof value.expiresAtSeconds !== "number") throw new Error("identity_unavailable");
  return value as BrowserIdentity;
}
export async function login(): Promise<void> {
  const response = await fetch("/auth/login", {
    method: "POST", credentials: "same-origin", redirect: "error",
    headers: { "x-zuno-csrf": "1", accept: "application/json" },
  });
  if (!response.ok) throw new Error("暂时无法登录，请重试。");
  const value: unknown = await response.json();
  if (!value || typeof value !== "object" || !("authorizationUrl" in value) || typeof value.authorizationUrl !== "string") {
    throw new Error("登录服务返回了无效地址。");
  }
  const destination = new URL(value.authorizationUrl);
  if (destination.protocol !== "https:" || destination.username || destination.password) throw new Error("登录地址无效。");
  location.assign(destination.href);
}
export async function logout(): Promise<void> {
  const response = await fetch("/auth/logout", { method: "POST", credentials: "same-origin", redirect: "error", headers: { "x-zuno-csrf": "1" } });
  if (!response.ok) throw new Error("退出登录失败，请重试。");
}
export function requestId(): string { return crypto.randomUUID(); }
export function errorText(error: unknown): string {
  if (error && typeof error === "object" && "status" in error) {
    if (error.status === 401) return "登录已过期，请重新登录。";
    if (error.status === 403) return "当前账号没有执行此操作的权限。";
    if (error.status === 404) return "此会话或资源已不可用。";
    if (error.status === 409) return "状态已变化，请刷新后重试。";
  }
  return "连接暂时不可用，请重试。";
}
