import type { CommittedFrame, FramePage, HistoryItem, HistoryPage } from "./generated/activity.js";
import { validateCommittedFrame, validateFramePage, validateHistoryPage } from "./generated/validators.mjs";

export class ActivityProtocolError extends Error {
  constructor(readonly kind: "invalid" | "gap" | "conflict", message: string) { super(message); }
}

export function counter(value: string): bigint {
  if (!/^(0|[1-9][0-9]{0,19})$/.test(value)) throw new ActivityProtocolError("invalid", "Invalid activity counter");
  const result = BigInt(value);
  if (result > 18446744073709551615n) throw new ActivityProtocolError("invalid", "Activity counter overflow");
  return result;
}

function version(value: number): void {
  if (value !== 1) throw new ActivityProtocolError("invalid", "Unsupported activity protocol");
}

export function decodeHistoryPage(value: unknown): HistoryPage {
  if (!validateHistoryPage(value)) throw new ActivityProtocolError("invalid", "Invalid history response");
  const page = value as HistoryPage;
  version(page.version);
  const through = counter(page.through);
  let previous = 0n;
  const ids = new Set<string>();
  for (const item of page.items) {
    const position = counter(item.position);
    const revision = counter(item.revision);
    counter(item.record.createdAt);
    if (position <= previous || revision < position || revision > through || ids.has(item.record.id)) {
      throw new ActivityProtocolError("invalid", "Invalid history ordering or revision");
    }
    previous = position;
    ids.add(item.record.id);
  }
  if (page.before != null && (page.items.length === 0 || counter(page.before) !== counter(page.items[0]!.position))) {
    throw new ActivityProtocolError("invalid", "Invalid history continuation");
  }
  return page;
}

export function decodeFramePage(value: unknown): FramePage {
  if (!validateFramePage(value)) throw new ActivityProtocolError("invalid", "Invalid frame response");
  const page = value as FramePage;
  version(page.version);
  counter(page.through);
  for (const frame of page.frames) validateFrame(frame, page.sessionId);
  return page;
}

function validateFrame(frame: CommittedFrame, session: string): void {
  if (!validateCommittedFrame(frame) || frame.sessionId !== session) {
    throw new ActivityProtocolError("invalid", "Activity frame belongs to another session");
  }
  version(frame.version);
  const sequence = counter(frame.sequence);
  if (sequence === 0n) throw new ActivityProtocolError("invalid", "Committed sequences start at one");
  if (frame.event.kind === "upsert") {
    const position = counter(frame.event.position);
    counter(frame.event.record.createdAt);
    if (position === 0n || position > sequence) throw new ActivityProtocolError("invalid", "Invalid item position");
  }
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const object = value as Record<string, unknown>;
    return `{${Object.keys(object).sort().map((key) => `${JSON.stringify(key)}:${canonical(object[key])}`).join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

/** Durable state only. Live provider deltas must never overwrite these records.
 * A bounded window can reload older pages without changing the stream cursor. */
export class ActivityState {
  private records = new Map<string, HistoryItem>();
  private recent = new Map<string, string>();
  private removed = new Map<string, bigint>();
  private sequence = 0n;
  private snapshotThrough = "0";
  private retain: "latest" | "older" = "latest";
  constructor(readonly sessionId: string, readonly maximumItems = 1000) {
    if (!Number.isInteger(maximumItems) || maximumItems < 1 || maximumItems > 10000) {
      throw new ActivityProtocolError("invalid", "Invalid history window");
    }
  }
  get cursor(): string { return this.sequence.toString(); }
  get historyThrough(): string { return this.snapshotThrough; }
  get historicalWindow(): boolean { return this.retain === "older"; }
  items(): HistoryItem[] {
    return structuredClone([...this.records.values()].sort((a, b) => counter(a.position) < counter(b.position) ? -1 : 1));
  }
  replaceHistory(value: unknown): void {
    const page = decodeHistoryPage(value);
    this.session(page.sessionId);
    const next = new Map(page.items.map((item) => [item.record.id, structuredClone(item)]));
    this.trim(next, "latest");
    this.records = next;
    this.sequence = counter(page.through);
    this.snapshotThrough = page.through;
    this.retain = "latest";
    this.recent.clear();
    this.removed.clear();
  }
  mergeHistory(value: unknown, options: { retain?: "latest" | "older" } = {}): void {
    const page = decodeHistoryPage(value);
    this.session(page.sessionId);
    if (page.through !== this.snapshotThrough) throw new ActivityProtocolError("conflict", "History page uses a different snapshot");
    const next = new Map(this.records);
    for (const item of page.items) {
      if ((this.removed.get(item.record.id) ?? 0n) > counter(item.revision)) continue;
      const previous = next.get(item.record.id);
      if (!previous || counter(previous.revision) < counter(item.revision)) next.set(item.record.id, structuredClone(item));
    }
    const retain = options.retain ?? this.retain;
    this.trim(next, retain);
    this.records = next;
    this.retain = retain;
  }
  apply(value: unknown): void {
    const page = decodeFramePage(value);
    this.session(page.sessionId);
    // Validate and stage the whole page before publishing any mutation.
    const next = new Map(this.records);
    const recent = new Map(this.recent);
    const removed = new Map(this.removed);
    let sequence = this.sequence;
    for (const frame of page.frames) {
      const incoming = counter(frame.sequence);
      const fingerprint = canonical(frame);
      if (incoming <= sequence) {
        const previous = recent.get(frame.sequence);
        if (previous !== undefined && previous !== fingerprint) throw new ActivityProtocolError("conflict", "A committed frame changed");
        continue;
      }
      if (incoming !== sequence + 1n) throw new ActivityProtocolError("gap", "Read missing committed frames before continuing");
      if (frame.event.kind === "upsert") {
        const previous = next.get(frame.event.record.id);
        if (previous && previous.position !== frame.event.position) throw new ActivityProtocolError("conflict", "An item's position changed");
        next.set(frame.event.record.id, {
          position: frame.event.position, revision: frame.sequence, record: structuredClone(frame.event.record),
        });
        removed.delete(frame.event.record.id);
      } else {
        next.delete(frame.event.id);
        removed.set(frame.event.id, incoming);
        if (removed.size > this.maximumItems) {
          throw new ActivityProtocolError("gap", "Refresh the history snapshot after its bounded removal window");
        }
      }
      recent.set(frame.sequence, fingerprint);
      while (recent.size > 32) recent.delete(recent.keys().next().value!);
      sequence = incoming;
    }
    const through = counter(page.through);
    if (through > sequence || (through < sequence && sequence > this.sequence)) {
      throw new ActivityProtocolError("gap", "Frame page omitted committed state");
    }
    if (page.more && page.frames.length === 0) throw new ActivityProtocolError("gap", "An incomplete page cannot make no progress");
    this.trim(next);
    this.records = next;
    this.recent = recent;
    this.removed = removed;
    this.sequence = sequence;
  }
  private session(id: string): void {
    if (id !== this.sessionId) throw new ActivityProtocolError("invalid", "History belongs to another session");
  }
  private trim(items: Map<string, HistoryItem>, retain = this.retain): void {
    if (items.size <= this.maximumItems) return;
    const ordered = [...items.values()].sort((a, b) => counter(a.position) < counter(b.position) ? -1 : 1);
    const remove = retain === "older" ? ordered.slice(this.maximumItems) : ordered.slice(0, items.size - this.maximumItems);
    for (const item of remove) items.delete(item.record.id);
  }
}
