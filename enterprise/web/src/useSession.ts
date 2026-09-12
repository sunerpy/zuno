import { useCallback, useEffect, useRef, useState } from "react";
import { ActivityProtocolError, ActivityState, EnterpriseHttpError, LiveActivity } from "@zuno/enterprise-sdk";
import type { HistoryItem, LiveItem } from "@zuno/enterprise-sdk";
import { client, errorText } from "./api";

export function useSession(sessionId: string | null, unavailable: () => void) {
  const [items, setItems] = useState<HistoryItem[]>([]);
  const [live, setLive] = useState<LiveItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [older, setOlder] = useState(false);
  const [paging, setPaging] = useState(false);
  const [historical, setHistorical] = useState(false);
  const [connected, setConnected] = useState(true);
  const [error, setError] = useState("");
  const state = useRef<{
    sessionId: string; store: ActivityState; before?: string | null; abort: AbortController; paging: boolean;
  } | null>(null);
  useEffect(() => {
    setItems([]); setLive([]); setError(""); setOlder(false); setPaging(false); setHistorical(false); setConnected(true);
    if (!sessionId) { setLoading(false); return; }
    const abort = new AbortController();
    const store = new ActivityState(sessionId, 2000);
    const drafts = new LiveActivity(sessionId);
    const current = { sessionId, store, before: null as string | null | undefined, abort, paging: false };
    state.current = current;
    setLoading(true);
    void (async () => {
      try {
        const page = await client.history(sessionId, { signal: abort.signal });
        if (abort.signal.aborted) return;
        store.replaceHistory(page); current.before = page.before;
        setItems(store.items()); setOlder(page.before != null); setLoading(false);
        let failures = 0;
        while (!abort.signal.aborted) {
          try {
            const page = await client.frames(sessionId, store.cursor, { signal: abort.signal });
            if (abort.signal.aborted) return;
            store.apply(page);
            if (page.frames.length) setItems(store.items());
            if (!page.more) {
              const progress = await client.live(sessionId, abort.signal);
              if (abort.signal.aborted) return;
              if (drafts.apply(progress, store.cursor)) setLive(store.historicalWindow ? [] : drafts.items());
            }
            failures = 0; setConnected(true); setError("");
            if (!page.more) await wait(500, abort.signal);
          } catch (error) {
            if (abort.signal.aborted) return;
            if (error instanceof EnterpriseHttpError && [401, 403, 404].includes(error.status)) {
              setItems([]); setLive([]); unavailable(); return;
            }
            if (error instanceof ActivityProtocolError || (error instanceof EnterpriseHttpError && error.status === 409)) {
              const snapshot = await client.history(sessionId, { signal: abort.signal });
              if (abort.signal.aborted) return;
              store.replaceHistory(snapshot); current.before = snapshot.before;
              drafts.apply(null, store.cursor);
              setItems(store.items()); setLive([]); setOlder(snapshot.before != null); setHistorical(false);
            }
            setConnected(false); failures++;
            await wait(Math.min(5000, 500 * 2 ** Math.min(failures, 4)), abort.signal);
          }
        }
      } catch (error) {
        if (abort.signal.aborted) return;
        setLoading(false); setError(errorText(error));
        if (error instanceof EnterpriseHttpError && [401, 403, 404].includes(error.status)) unavailable();
      }
    })();
    return () => { abort.abort(); if (state.current === current) state.current = null; };
  }, [sessionId, unavailable]);
  const loadOlder = useCallback(async () => {
    const current = state.current;
    if (!current?.before || current.paging) return;
    current.paging = true; setPaging(true);
    try {
      const page = await client.history(current.sessionId, {
        before: current.before, through: current.store.historyThrough, signal: current.abort.signal,
      });
      if (state.current !== current || current.abort.signal.aborted) return;
      current.store.mergeHistory(page, { retain: "older" }); current.before = page.before;
      setItems(current.store.items()); setOlder(page.before != null); setHistorical(true); setLive([]);
    } catch (error) { if (!current.abort.signal.aborted) setError(errorText(error)); }
    finally { current.paging = false; if (state.current === current) setPaging(false); }
  }, []);
  const latest = useCallback(async () => {
    const current = state.current;
    if (!current || current.paging) return;
    current.paging = true; setPaging(true);
    try {
      const page = await client.history(current.sessionId, { signal: current.abort.signal });
      if (state.current !== current || current.abort.signal.aborted) return;
      current.store.replaceHistory(page); current.before = page.before;
      setItems(current.store.items()); setOlder(page.before != null); setHistorical(false); setError("");
    } catch (error) { if (!current.abort.signal.aborted) setError(errorText(error)); }
    finally { current.paging = false; if (state.current === current) setPaging(false); }
  }, []);
  return { items, live, loading, older, paging, historical, connected, error, loadOlder, latest };
}

function wait(milliseconds: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) { resolve(); return; }
    const finish = () => { clearTimeout(timer); signal.removeEventListener("abort", finish); resolve(); };
    const timer = setTimeout(finish, milliseconds);
    signal.addEventListener("abort", finish, { once: true });
  });
}
