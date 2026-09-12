import type { ButtonHTMLAttributes, ReactNode } from "react";

export function Button({ children, tone = "normal", ...props }: ButtonHTMLAttributes<HTMLButtonElement> & { tone?: "normal" | "primary" | "danger" }) {
  return <button {...props} className={`button button--${tone} ${props.className ?? ""}`}>{children}</button>;
}
const labels: Record<string, string> = {
  pending: "等待中", queued: "排队中", ready: "排队中", active: "进行中", running: "进行中",
  waiting: "等待回复", paused: "已暂停", completed: "已完成", complete: "已完成",
  succeeded: "已完成", failed: "失败", denied: "已拒绝", cancelled: "已取消",
  uncertain: "待核查", approved: "已批准", rejected: "已拒绝", expired: "已过期",
  invalidated: "已失效", revoked: "已撤销", automatic: "自动批准", interrupted: "已中断",
};
export function Badge({ state }: { state: string }) {
  return <span className={`badge badge--${state}`}><span aria-hidden="true" className="badge-dot" />{labels[state] ?? state}</span>;
}
export function Alert({ children }: { children: ReactNode }) { return <div className="alert" role="alert">{children}</div>; }
export function Empty({ title, children }: { title: string; children: ReactNode }) {
  return <section className="empty-state"><div className="empty-mark" aria-hidden="true">Z</div><h2>{title}</h2><p>{children}</p></section>;
}
export function Loading({ label = "正在加载…" }: { label?: string }) {
  return <div className="loading" role="status"><span className="spinner" aria-hidden="true" />{label}</div>;
}
