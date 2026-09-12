import type { LiveFrame, LiveItem } from "./generated/activity.js";
import { validateLiveFrame } from "./generated/validators.mjs";
import { ActivityProtocolError, counter } from "./state.js";

/** Replaceable drafts never enter ActivityState or survive an inactive Job. */
export class LiveActivity {
  private generation: string | null = null;
  private sequence = 0n;
  private floor = 0n;
  private retired = new Set<string>();
  private drafts: LiveItem[] = [];
  constructor(readonly sessionId: string) {}
  items(): LiveItem[] { return structuredClone(this.drafts); }
  apply(frame: LiveFrame | null, committedCursor: string): boolean {
    if (frame === null) {
      // Silence/TTL can temporarily hide progress without ending the current
      // attempt. Keep its sequence so stale replies stay rejected while a
      // genuinely newer snapshot from that generation can appear again.
      this.floor = counter(committedCursor) > this.floor ? counter(committedCursor) : this.floor;
      this.drafts = [];
      return true;
    }
    if (!validateLiveFrame(frame) || frame.version !== 1 || frame.sessionId !== this.sessionId || frame.event.kind !== "snapshot") {
      throw new ActivityProtocolError("invalid", "Invalid live activity");
    }
    const through = counter(frame.afterCommitted);
    if (through > counter(committedCursor)) return false;
    if (through < this.floor || this.retired.has(frame.generation)) return false;
    const sequence = counter(frame.sequence);
    if (sequence === 0n) throw new ActivityProtocolError("invalid", "Invalid live sequence");
    if (frame.generation === this.generation && sequence <= this.sequence) return false;
    if (frame.generation !== this.generation && this.generation !== null) this.retired.add(this.generation);
    while (this.retired.size > 64) this.retired.delete(this.retired.values().next().value!);
    this.generation = frame.generation;
    this.sequence = sequence;
    this.floor = through;
    this.drafts = structuredClone(frame.event.items);
    return true;
  }
}
