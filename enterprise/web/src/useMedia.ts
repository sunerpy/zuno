import { useSyncExternalStore } from "react";
import type { KeyboardEvent } from "react";

export function useMedia(query: string): boolean {
  return useSyncExternalStore(
    (listener) => { const media = matchMedia(query); media.addEventListener("change", listener); return () => media.removeEventListener("change", listener); },
    () => matchMedia(query).matches,
    () => false,
  );
}
export function containFocus(event: KeyboardEvent<HTMLElement>): void {
  if (event.key !== "Tab") return;
  const elements = [...event.currentTarget.querySelectorAll<HTMLElement>("button:not(:disabled),a[href],input:not(:disabled),select:not(:disabled),textarea:not(:disabled),summary,[tabindex='0']")]
    .filter((element) => element.getClientRects().length > 0);
  const first = elements[0]; const last = elements.at(-1);
  if (!first || !last) { event.preventDefault(); return; }
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
}
