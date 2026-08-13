# Research: TencentCloud/TencentDB-Agent-Memory

Repo: https://github.com/TencentCloud/TencentDB-Agent-Memory
Commit pinned for all permalinks: `4dca55c41bf11cb19b49728dbe495c8e05d25abb` (default branch `feat/server_team`)

Context: evaluating for `opencode-rust` `crates/oc-memory` (SQLite+FTS5+trigram, no system deps, no Node, `unsafe_code = "forbid"`, resists new crates).

**CONCLUSION: worth mining, selectively.** Not a pure TencentDB wrapper — it maintains a real first-class SQLite+FTS5 backend that works with zero embeddings, and ~8 of its ideas port to pure Rust over SQLite. But its *quality* advantages come mostly from an LLM in the write path, not from data structures, and its CJK strategy (jieba write-side segmentation) is worse than `oc-memory`'s trigram table for a no-dependency binary. Top three takeaways: **version-as-rows with `is_head` + partial unique index**, **RRF rank fusion**, **structured `type`/`priority`/validity-interval columns**. License is `NOASSERTION` — read for ideas, do not copy code.

---

## 0. Repo identity (this matters for how much transfers)

| Fact | Value |
|---|---|
| Language | TypeScript (Node), with a Python sidecar plugin for hermes |
| Size | ~34 MB checkout; `MemoryCore/src` alone is ~1.5 MB of TS |
| Stars / forks | 21,053 / 1,913 |
| Created / last push | 2026-04-07 / 2026-08-11 (very active) |
| License | `NOASSERTION` ("Other") — **not a standard OSI license; treat code copying as legally unclear** |
| Topics | `local-first`, `vector-search`, `long-term-memory`, `openclaw-plugin`, `embedding` |
| Top-level | `MemoryCore/`, `MemoryKnowledge/`, `MemoryPanel/` (UI), `MemoryProxy/`, `deploy/`, `sdk/` |

**Is it a TencentDB product wrapper?** Partly, but **not only**. It is a two-backend system:

- `MemoryCore/src/core/store/tcvdb.ts` (76 KB) — Tencent Cloud VectorDB backend
- `MemoryCore/src/core/store/sqlite.ts` (139 KB) — **a genuine, first-class local SQLite backend** with FTS5 + `sqlite-vec`
- Same duality in the governance layer: `metadata/store/mongodb-adapter.ts` vs `metadata/store/sqlite-adapter.ts` (64 KB)
- `MemoryCore/scripts/migrate-sqlite-to-tcvdb/` exists precisely because SQLite is the real starting point, not a toy

So the local path is maintained, and that is where the transferable material lives. The commercial pull is real (quota manager, credit calculator, ClickHouse/Kafka/OTLP reporting, team ACL, a Panel UI), but it is separable from the memory algorithms.

One caveat that deflates novelty: the SQLite FTS5 code is annotated as borrowed *from the host agent CLI*, not invented here —
`sqlite.ts:148` `// FTS5 helpers (adapted from openclaw core hybrid.ts)` and `sqlite.ts:297` `* Mirrors the formula in openclaw core hybrid.ts`.
Check whether `opencode`/`openclaw` upstream already ships that `hybrid.ts` before crediting this repo for it.

---

## 1. The memory model — an L0/L1/L2/L3 pyramid, plus separate "assets"

It does **not** use the episodic/semantic/procedural vocabulary. It uses a numbered layer pyramid, and separately a governed "asset" catalog. Four asset types are advertised: **Chat Memory, Skill, LLM-Wiki, Code-Graph**.

### L0 — raw conversation log (immutable)

`sqlite.ts:731-745`:

```sql
CREATE TABLE IF NOT EXISTS l0_conversations (
  record_id TEXT PRIMARY KEY,
  session_key TEXT NOT NULL,
  session_id TEXT DEFAULT 'default',
  team_id TEXT DEFAULT 'default',
  task_id TEXT DEFAULT '',
  user_id TEXT NOT NULL DEFAULT 'default',
  agent_id TEXT NOT NULL DEFAULT 'default',
  role TEXT NOT NULL DEFAULT '',
  message_text TEXT NOT NULL,
  recorded_at TEXT DEFAULT '',
  timestamp INTEGER DEFAULT 0
)
```

L0 is explicitly declared immutable and excluded from audit — `sqlite.ts:966`: `L0 不参与（不可变流水）` ("L0 does not participate — immutable ledger").

### L1 — extracted, deduplicated memory records (the "real" memory)

`sqlite.ts:610-630`:

```sql
CREATE TABLE IF NOT EXISTS l1_records (
  record_id TEXT PRIMARY KEY,
  content TEXT NOT NULL,
  type TEXT DEFAULT '',
  priority INTEGER DEFAULT 50,
  scene_name TEXT DEFAULT '',
  session_key TEXT DEFAULT '',
  session_id TEXT DEFAULT 'default',
  team_id TEXT DEFAULT 'default',
  task_id TEXT DEFAULT '',
  user_id TEXT NOT NULL DEFAULT 'default',
  agent_id TEXT NOT NULL DEFAULT 'default',
  version INTEGER NOT NULL DEFAULT 0,
  timestamp_str TEXT DEFAULT '',
  timestamp_start TEXT DEFAULT '',
  timestamp_end TEXT DEFAULT '',
  created_time TEXT DEFAULT '',
  updated_time TEXT DEFAULT '',
  metadata_json TEXT DEFAULT '{}'
)
```

Note the fields a flat character-capped store lacks: **`type`** (memory kind), **`priority` (default 50)**, **`version`** (monotonic, for optimistic concurrency + audit), **`scene_name`** (topic/scene clustering), and a **validity interval** (`timestamp_start`/`timestamp_end`) distinct from `created_time`/`updated_time`. That last pair is the schema-level hook for staleness.

L2 (scene/summary) and L3 (context offload) exist as layers in code (`src/core/scene/`, `src/offload/`) and appear in the audit CHECK constraint, but are not separate SQLite base tables in `sqlite.ts`.

### Vector + FTS side tables (per layer)

`sqlite.ts:668-673` and `sqlite.ts:790-795` — `sqlite-vec` virtual tables, created **only when an embedding provider exists**:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS l1_vec USING vec0(
  record_id TEXT PRIMARY KEY,
  embedding float[${this.dimensions}] distance_metric=cosine,
  updated_time TEXT DEFAULT ''
)
```

`sqlite.ts:1001-1039` — FTS5, **best-effort with graceful degradation** if FTS5 is not compiled in (`sqlite.ts:1089-1096`):

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS l1_fts USING fts5(
  content,
  content_original UNINDEXED,
  record_id UNINDEXED,
  type UNINDEXED,
  priority UNINDEXED,
  scene_name UNINDEXED,
  session_key UNINDEXED,
  ... team_id/task_id/user_id/agent_id/version/timestamps/metadata_json all UNINDEXED
)
```

Two design points here are directly relevant to `oc-memory`:

1. **`content` holds *segmented* text; `content_original UNINDEXED` holds the raw text for display.** This is their CJK strategy — a *write-side tokenizer* feeding the default `unicode61` tokenizer, instead of a trigram table.
2. **All filter/scope columns are carried in the FTS row as `UNINDEXED`**, so a single FTS query returns everything needed to filter and rank without joining back to the base table.

### Embedding-provenance table + automatic reindex

`sqlite.ts:536-541`:

```sql
CREATE TABLE IF NOT EXISTS embedding_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

`sqlite.ts:549-601` compares saved `provider`/`model`/`dimensions` against current config and **drops + rebuilds vector tables when any changed**, returning `needsReindex` with a human-readable reason. It also handles the legacy case (data exists but no meta ⇒ cannot verify ⇒ drop for safety) and the dimension-mismatch case. This is good hygiene that plugin-based embedding backends usually get wrong.

### Governance/asset schema (separate DB)

`MemoryCore/scripts/db/sqlite-init.sql` is **not** the memory-content schema — it is the metadata/governance schema (users, teams, agents, tasks, ACL). One table there is conceptually interesting, `sqlite-init.sql:148-168`:

```sql
CREATE TABLE IF NOT EXISTS meta_assets (
  asset_id TEXT PRIMARY KEY,
  team_id TEXT NOT NULL,
  asset_type TEXT NOT NULL,
  name TEXT NOT NULL,
  description TEXT,
  owner_user_id TEXT NOT NULL,
  source_type TEXT NOT NULL,
  source_ref TEXT,
  version INTEGER NOT NULL DEFAULT 1,
  visibility TEXT NOT NULL DEFAULT 'team',
  status TEXT NOT NULL DEFAULT 'draft',
  confidence REAL,
  expires_at TEXT,
  last_used_at TEXT,
  usage_count INTEGER NOT NULL DEFAULT 0,
  content_ref TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}'
)
```

`confidence REAL`, `expires_at`, `last_used_at`, `usage_count`, `status='draft'` — a full lifecycle for a memory item. And `sqlite-init.sql:172-183` is how an asset gets attached to an agent:

```sql
CREATE TABLE IF NOT EXISTS meta_agent_fixed_assets (
  id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL,
  asset_id TEXT NOT NULL,
  asset_type TEXT NOT NULL,
  injection_mode TEXT NOT NULL DEFAULT 'summary',
  priority INTEGER NOT NULL DEFAULT 50,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(agent_id, asset_id)
)
```

`injection_mode` ('summary' vs presumably 'full') + `priority` = an explicit **resident-memory budget policy**, which is the closest analog to a character-capped resident store — but with per-entry priority and a summary/full switch rather than one global cap.

### Modification audit (append-only, no old values)

`sqlite.ts:967-981`:

```sql
CREATE TABLE IF NOT EXISTS memory_audit (
  audit_id      TEXT PRIMARY KEY,
  record_id     TEXT NOT NULL,
  layer         TEXT NOT NULL CHECK (layer IN ('L1','L2','L3')),
  action        TEXT NOT NULL CHECK (action IN ('update','delete')),
  team_id       TEXT,
  agent_id      TEXT,
  user_id       TEXT,
  task_id       TEXT,
  version       INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  request_id    TEXT
)
```

Design notes at `sqlite.ts:962-966` are explicit: base tables untouched, events appended only, **no historical content or old values stored** — only "when, by whom, which record". Cheap tamper-evident history without doubling storage.

---

## 2. Retrieval: FTS5 keyword + vector, fused; the portable parts are pure arithmetic

### CJK handling: write-side segmentation instead of trigrams

`sqlite.ts:262-275`:

```ts
export function tokenizeForFts(raw: string): string {
  const jieba = getJieba();
  if (!jieba) return raw;
  const tokens = jieba.cutForSearch(raw, true);
  return tokens.join(" ");
}
```

`cutForSearch` emits full words *and* sub-words, so "人工智能" is indexed as `人工 智能 人工智能`. The query side uses the same function so tokens always align (`sqlite.ts:249-252`).

### Query construction: OR-joined quoted tokens + a small CJK stop-word list

`sqlite.ts:179-184` — deliberately tiny stop-word set (high-frequency function words only):

```ts
const ZH_STOP_WORDS = new Set([
  "的", "了", "在", "是", "我", "有", "和", "就", "不", "人", "都", "一",
  "一个", "上", "也", "很", "到", "说", "要", "去", "你", "会", "着",
  "没有", "看", "好", "自己", "这", "他", "她", "它", "们", "那",
  "吗", "吧", "呢", "啊", "呀", "哦", "嗯",
]);
```

`sqlite.ts:207-239`:

```ts
export function buildFtsQuery(raw: string): string | null {
  const jieba = getJieba();
  let tokens: string[];
  if (jieba) {
    tokens = jieba.cutForSearch(raw, true)
      .map((t) => t.trim())
      .filter((t) => {
        if (!t) return false;
        if (!/[\p{L}\p{N}]/u.test(t)) return false;   // drop punctuation-only
        if (ZH_STOP_WORDS.has(t)) return false;        // drop stop-words
        return true;
      });
    tokens = [...new Set(tokens)];                     // dedupe sub-words
  } else {
    tokens = raw.match(/[\p{L}\p{N}_]+/gu)?.map((t) => t.trim()).filter(Boolean) ?? [];
  }
  if (tokens.length === 0) return null;
  const quoted = tokens.map((t) => `"${t.replaceAll('"', "")}"`);
  return quoted.join(" OR ");
}
```

Rationale at `sqlite.ts:196-200`: OR-join maximizes recall, and **BM25 alone restores precision** because documents matching more tokens rank higher. Explicitly aimed at the FTS-only, no-embedding fallback mode — i.e. exactly `oc-memory`'s situation.

### BM25 normalization to 0–1 (the single most portable line of code here)

`sqlite.ts:295-306`:

```ts
/**
 * Convert a BM25 rank (negative = more relevant) to a 0–1 score.
 * Mirrors the formula in openclaw core `hybrid.ts`.
 */
export function bm25RankToScore(rank: number): number {
  if (!Number.isFinite(rank)) return 1 / (1 + 999);
  if (rank < 0) {
    const relevance = -rank;
    return relevance / (1 + relevance);
  }
  return 1 / (1 + rank);
}
```

A saturating map of SQLite's unbounded negative `bm25()` into `(0,1)`. Needed for any fusion with recency/priority. Pure arithmetic, no dependency.

The FTS queries themselves are plain (`sqlite.ts:1051-1061`), selecting `bm25(l1_fts) AS rank` and `ORDER BY rank ASC` — ranking happens in application code, not SQL.

### Fusion: Reciprocal Rank Fusion, k=60

`MemoryCore/src/core/store/search-utils.ts:14-62`:

```ts
/** Standard RRF constant from the original RRF paper.
 *  Higher k → more weight on lower-ranked items (smoother distribution). */
export const RRF_K = 60;

export function rrfMerge<T>(lists: T[][], getId: (item: T) => string, k: number = RRF_K):
  Array<T & { rrfScore: number }> {
  const map = new Map<string, { item: T; rrfScore: number }>();
  for (const list of lists) {
    for (let rank = 0; rank < list.length; rank++) {
      const item = list[rank];
      const id = getId(item);
      const score = 1 / (k + rank + 1);
      const existing = map.get(id);
      if (existing) existing.rrfScore += score;
      else map.set(id, { item, rrfScore: score });
    }
  }
  return [...map.values()].sort((a, b) => b.rrfScore - a.rrfScore)
    .map(({ item, rrfScore }) => ({ ...item, rrfScore }));
}
```

Rank-based, so it needs **no score calibration between the two retrievers** — the reason it is the right primitive for fusing FTS5 BM25 with anything else (a trigram list, a recency list, a vector list). Duplicated inline at `auto-recall.ts:726-761`.

Three retrieval modes are configured (`auto-recall.ts:429`): `keyword` (FTS5 only), `embedding` (vector only), `hybrid` (RRF of both). When the backend is TCVDB, a *native* server-side hybrid short-circuits the client path (`auto-recall.ts:501-510`); on SQLite it runs both legs in parallel and fuses client-side (`auto-recall.ts:513-514`).

### Small-corpus BM25 guard (a real practical fix)

`auto-recall.ts:544-563`:

```ts
const filtered = ftsResults.filter((r) => r.score >= threshold).slice(0, maxResults);
if (filtered.length > 0) { ...return filtered... }

// BM25 absolute scores are unreliable when the document set is very
// small (e.g. 1–3 records) because IDF approaches 0.  In that case,
// trust FTS5's MATCH + rank ordering and return the top results anyway.
if (ftsResults.length <= maxResults) {
  return ftsResults.slice(0, maxResults).map((r) => formatMemoryLine(ftsResultToFormatable(r)));
}
```

Default threshold is `0.3` (`auto-recall.ts:459`). The guard exists because absolute BM25 thresholds are meaningless on a tiny corpus (IDF → 0) — precisely the regime a personal memory store lives in. Any store that applies a fixed relevance cutoff over a few dozen entries needs this or it silently returns nothing.

They also deliberately **refuse an in-memory fallback** when FTS5 is missing (`auto-recall.ts:568-570`): "skip in-memory fallback to avoid O(N) full scan" — keyword search just returns empty.

### Two-level character budget on the recall output

`auto-recall.ts:835-899` — `maxCharsPerMemory` **and** `maxTotalRecallChars`, applied together:

```ts
const separatorChars = budgeted.length > 0 ? RECALL_LINE_SEPARATOR.length : 0;
const remainingChars = maxTotalRecallChars - usedChars - separatorChars;
if (remainingChars <= 0) { droppedCount += lines.length - i; break; }

if (perMemoryBounded.length > remainingChars) {
  const canFit = remainingChars >= MIN_TRUNCATED_RECALL_LINE_CHARS;
  if (canFit) { /* truncate to exactly remainingChars, push */ }
  droppedCount += lines.length - i - (canFit ? 1 : 0);
  break;
}
```

Two details worth stealing: the separator length is **counted against the budget**, and a final entry is only admitted if at least `MIN_TRUNCATED_RECALL_LINE_CHARS` survive — no useless 4-character stubs. Note this budget is applied to the *recall result*, not to the resident store; it is a per-turn injection cap, which is a different axis from `oc-memory`'s total-store cap.

### Injection rendering carries type + validity interval

`auto-recall.ts:805-833`:

```ts
const tag = m.scene_name ? `${m.type}|${m.scene_name}` : m.type;
let line = `- [${tag}] ${m.content}`;
// then one of:
//   (活动时间: {start} ~ {end})   both bounds
//   (活动时间: {start}起)         open-ended
//   (活动时间: 至{end})           only end
//   (活动时间: {point})           point-in-time
// if all empty → append nothing (graceful)
```

The distinction between 段时间 (interval) and 点时间 (point-in-time) is carried all the way into the prompt text, so the model can reason about whether a memory is still in force.

---

## 3. Write path: L0 append → threshold/idle trigger → LLM extract → LLM dedup → L1 apply

### Memory type taxonomy (7 types, two prompt families)

`MemoryCore/src/core/record/l1-dedup.ts:313`:

```ts
const VALID_TYPES: MemoryType[] = ["persona", "episodic", "instruction",
  "work_fact", "work_task", "work_method", "work_artifact"];
```

Mapped to the classic vocabulary: `persona` ≈ semantic/profile facts, `episodic` ≈ episodic, `instruction` ≈ standing directives, `work_method` ≈ procedural SOP, `work_fact` ≈ semantic domain fact, `work_task` ≈ task state, `work_artifact` ≈ resource pointer. Two prompt families exist — a "chat" family and a "work" family (`prompts/l1-dedup.ts`), each with type-specific merge policy. The `work_*` family is the one relevant to a coding agent.

`priority` is a 0–100 integer with published bands (`prompts/l1-dedup.ts:69`): 80–100 core traits / important events, 60–79 ordinary preferences / activities, <60 secondary. **Merging is expected to raise priority** — "two memories at priority 70 merged may become 80" — because a consolidated memory is more complete and more certain.

### Trigger scheduling — the most reusable non-LLM logic in the repo

`MemoryCore/src/utils/pipeline-manager.ts:42-70`:

```
## Trigger paths for L1
  A. Conversation threshold (primary): when conversation_count >= effectiveThreshold
     in notifyConversation(), L1 is triggered immediately with all buffered messages.
  B. Idle timeout (catch-up): when a session goes idle for l1IdleTimeoutSeconds,
     L1 fires with whatever messages have been buffered (below threshold).
  C. Shutdown flush: on graceful shutdown, all pending buffers are flushed
     through L1 then L2.
```

**Warm-up mode** (`pipeline-manager.ts:53-64`) — the neatest idea in the file:

```
When `enableWarmup` is true (default), new sessions use an exponentially
increasing L1 trigger threshold instead of jumping straight to
`everyNConversations`. The sequence is: 1 → 2 → 4 → 8 → ... → everyNConversations.
This ensures early conversations are processed quickly (first conversation
triggers L1 immediately), while gradually reducing processing frequency as
the session matures.
...The threshold doubles after each successful L1 run.  A value of 0 means
warm-up is complete (graduated to steady-state).
```

So a brand-new session gets memory after **one** exchange, and the extraction cost amortizes as the session grows. Pure integer state (`warmup_threshold` in the session row), no dependency.

**Timer discipline** (`pipeline-manager.ts:29-40`): L1 uses a resettable idle/debounce timer; L2 uses a **downward-only timer** — the scheduled fire time "can only be moved earlier, never later", giving both a `maxInterval` guarantee and delay-after-L1 responsiveness, with `minInterval` as a floor. If the session is cold (inactive > `sessionActiveWindowHours`) when the L2 timer fires, the timer is **cancelled rather than firing**, and gets re-armed by the next L1 event. L3 is a global mutex, concurrency 1, with a pending-flag dedup.

**Cursor-based incremental consolidation** (`pipeline-manager.ts:150, 174, 929, 968`): each layer keeps a watermark (`last_l1_cursor`, `last_extraction_updated_time`); the runner processes only rows past the cursor, returns `hasMore` when a backlog remains, and the cursor is advanced from the record timestamp the runner reports. That is why the `idx_l1_session_updated ON l1_records(session_id, updated_time)` composite index exists (`sqlite.ts:653`).

### Consolidation / reflection passes

There is a real multi-stage reflection chain, all LLM-driven:
- **L1 extraction** — `core/record/l1-extractor.ts` + `prompts/l1-extraction.ts`
- **L1.5** — an intermediate pass with its own prompt/parser (`offload_server/prompts/l15-prompt.ts`)
- **L2 scene extraction** — `core/scene/scene-extractor.ts` + `prompts/scene-extraction.ts` (30 KB prompt), clusters records into named "scenes" (`scene_name`), which then act as a retrieval facet and a display tag
- **L3 persona generation** — `core/persona/persona-generator.ts` + `prompts/persona-generation.ts`, a whole-profile synthesis triggered after L2
- **Skill extraction** — `core/skill/skill-extractor.ts`, turning conversations into reusable procedural docs

Every one of these needs an LLM call. None is a heuristic.

---

## 4. Conflict and staleness

### Conflict: candidate recall (cheap) → one batched LLM verdict (expensive)

`l1-dedup.ts:33-43`:

```
 * Candidate recall strategy (3-tier degradation):
 * 1. Vector recall (vectorStore + embeddingService) — cosine similarity (best)
 * 2. FTS5 keyword recall (vectorStore with FTS available) — BM25 ranking (degraded)
 * 3. Skip conflict detection entirely — all memories go straight to "store"
 *
 * The old JSONL-based Jaccard fallback has been removed. If neither vector search
 * nor FTS is available, we skip dedup rather than paying the O(N) full-file-scan cost.
```

Top-K default is 5 per new memory (`l1-dedup.ts:76`), and **all** new memories plus their candidate pools go into a **single** LLM call (`l1-dedup.ts:33-35`). Verdict schema (`l1-dedup.ts:350, 372-375`; prompt at `prompts/l1-dedup.ts:56-70`):

```json
{
  "record_id": "...",
  "action": "store|update|skip|merge",
  "target_ids": ["ids of old memories to delete/replace — array, 1 or many"],
  "merged_content": "final text (required for merge/update)",
  "merged_type": "persona|episodic|instruction|work_fact|work_task|work_method|work_artifact",
  "merged_priority": 85,
  "merged_timestamps": ["union of all new+old timestamps, deduped and sorted"]
}
```

Action semantics (`prompts/l1-dedup.ts:35-38`, my translation):
- **store** — genuinely new information, insert.
- **skip** — the existing memory is better; the new one adds nothing or is vaguer. Drop the new one.
- **update** — same fact/event, new memory is better (more specific, later, or a correction). New supersedes old; still-correct details of the old may be retained.
- **merge** — same fact or same evolving process, mutually complementary and non-contradictory. Collapse into one more complete memory.

Two policy details worth noting. First, **cross-type merge is explicitly allowed** (`prompts/l1-dedup.ts:43`): an `episodic` "user started a podcast in 2018" plus a `persona` "user has podcast production experience" may merge into a single record of whichever type fits. Second, **timestamps are unioned, not overwritten** (`prompts/l1-dedup.ts:46-47`, `110-111`): "merged_timestamps should contain the union of the timestamps of all related memories (deduped, sorted) — this preserves the complete timeline of how the fact/task/method evolved."

Failure mode is fail-open: any parse or LLM error defaults every memory to `store` (`l1-dedup.ts:188`).

**There is no numeric contradiction detector, no confidence score, and no decay function in the memory path.** Supersession is a hard delete of `target_ids` plus an insert, recorded in `memory_audit` only as `(record_id, version, action, when, by whom)`.

### Versioning done properly — but only for Skills

The Skill layer (procedural memory) has the clean model. `MemoryCore/src/core/skill/skill-store-ddl.ts:22-66`:

```sql
CREATE TABLE IF NOT EXISTS skills (
  row_id          TEXT PRIMARY KEY,
  skill_id        TEXT NOT NULL,
  version         INTEGER NOT NULL,
  is_head         INTEGER NOT NULL DEFAULT 1,
  user_id         TEXT NOT NULL,
  owner_agent_id  TEXT NOT NULL,
  team_id         TEXT NOT NULL,
  task_id         TEXT NOT NULL DEFAULT '',
  name            TEXT NOT NULL,
  description     TEXT NOT NULL DEFAULT '',
  content         TEXT NOT NULL,
  content_hash    TEXT NOT NULL,
  manifest_json   TEXT NOT NULL DEFAULT '[]',
  storage_dir     TEXT NOT NULL,
  status          TEXT NOT NULL DEFAULT 'active',
  metadata_json   TEXT NOT NULL DEFAULT '{}',
  created_at_ms   INTEGER NOT NULL,
  updated_at_ms   INTEGER NOT NULL,
  UNIQUE(skill_id, version)
);

CREATE UNIQUE INDEX IF NOT EXISTS uniq_skills_team_agent_name_head
  ON skills(team_id, owner_agent_id, name) WHERE is_head=1 AND status='active';

CREATE INDEX IF NOT EXISTS idx_skills_skill_version
  ON skills(skill_id, version DESC);
```

The header comment states the model plainly: *"skills — main table, each row = one immutable snapshot of (skill_id, version)"*. Three mechanisms make this work, all pure SQLite:

1. **Version-as-rows**: never mutate; append `version+1` and flip `is_head`.
2. **A partial unique index** enforces "exactly one live entry per (scope, name)" at the database level while retaining all history — `WHERE is_head=1 AND status='active'`.
3. **`content_hash` idempotence** (`skill-versioning.ts:210, 217-222`): if the new content hashes to the same value as head and no resources changed, return head and **write nothing** — no new version, no storage write, no DB write.

Retention for old versions is count-based, not time-based (`skill-versioning.ts:401-412`): keep head plus the `KEEP_RECENT` most recent non-head versions, prune the rest.

FTS indexes **only head rows** (`skill-store-ddl.ts:70-82`), with an explicit content cap (`skill-store-ddl.ts:104`):

```ts
/** Max chars of `content` in the FTS index (prevents a huge SKILL.md from blowing up fts5). */
export const FTS_CONTENT_MAX = 4000;
```

Note the Skill FTS table sets `tokenize = 'unicode61 remove_diacritics 1'` explicitly, while the L0/L1 FTS tables rely on write-side jieba segmentation instead.

### Staleness / forgetting: time-based retention with guardrails, and nothing else

`MemoryCore/src/utils/memory-cleaner.ts` — a scheduled cleaner (`retentionDays`, `cleanTime`) using a **calendar-day** cutoff (`memory-cleaner.ts:339-345`):

```ts
function computeCutoffMsByLocalDay(nowMs: number, retentionDays: number): number {
  // 自然日策略，保留"今天 + 往前 retentionDays-1 天"
  const keepStart = new Date(now.getFullYear(), now.getMonth(), now.getDate(), 0, 0, 0, 0);
  keepStart.setDate(keepStart.getDate() - (retentionDays - 1));
  return keepStart.getTime();
}
```

Three independent safety guards, all worth copying:

1. **Minimum-retention floor** (`memory-cleaner.ts:28-29, 130-150`) — never clean a store that is already small:
   ```ts
   const MIN_RETAIN_L0 = 50;
   const MIN_RETAIN_L1 = 20;
   ```
2. **80% ratio protection** in the store itself (`sqlite.ts:1592-1600` for L1, `sqlite.ts:2072-2080` for L0) — refuse a mass deletion outright:
   ```ts
   // Ratio protection: refuse to delete > 80% in one pass
   const ratio = total > 0 ? expiredCount / total : 0;
   if (ratio > 0.8) { /* BLOCKED: would delete N/total ... */ }
   ```
3. **Clock-skew detection** (`memory-cleaner.ts:351, 359`) — warn when the computed cutoff looks impossible.

What is **absent**: no confidence decay, no LRU/last-used eviction, no importance-weighted forgetting. The `meta_assets` table has `confidence REAL`, `expires_at`, `last_used_at`, `usage_count` (`sqlite-init.sql:160-163`) — but those columns live in the **governance** database, and the memory-content path (`l1_records`) does not use them. The lifecycle model is declared in the asset catalog and not implemented in retrieval or eviction. Do not read that schema as evidence of a working decay system.

---

## 5. What genuinely improves on a character-capped flat store — blunt assessment

**The good news, stated first: this is not a design whose advantages come from having a vector database.** The SQLite backend is a maintained peer of the TCVDB backend, and every retrieval mode degrades explicitly: hybrid → FTS5-only → skip. The system is written to *keep working* with no embeddings at all (`dimensions=0` defers vec0 table creation entirely, `sqlite.ts:666`, `sqlite.ts:788`). So the transfer question is real, not a formality.

**The bad news: most of what is genuinely better than a flat capped store is better because an LLM is in the write path, not because of clever data structures.** The quality wins — 7-type classification, priority assignment, merge vs update vs skip judgment, scene clustering, persona synthesis — are all LLM calls with large prompts. If `oc-memory` writes entries as-authored without an extraction/dedup LLM pass, that is the gap, and closing it costs tokens and latency on every capture, not schema work.

Ranking what actually beats a character-capped flat store, independent of embeddings:

1. **Versioned entries with `is_head` + a partial unique index.** A flat store can only overwrite or append. This gives supersession, history, and "one live entry per name" enforced by the DB. Pure SQLite. This is the single strongest idea in the repo.
2. **Structured fields instead of opaque text**: `type`, `priority`, `scene_name`, and a validity interval (`timestamp_start`/`timestamp_end`) separate from row mtime. These make ranking, filtering, and staleness expressible in SQL. A `\n§\n`-delimited flat store cannot filter or rank at all.
3. **RRF fusion** — lets you combine FTS5 BM25 with *any* second signal (trigram hits, recency, priority) without calibrating scores against each other. Directly useful given `oc-memory` already has two indexes (FTS5 + trigram) that currently cannot be merged principledly.
4. **A two-axis budget**: cap per entry *and* cap the total, applied to the *injected recall set* per turn, separate from the store's own size cap.
5. **Guardrails around deletion** (min-retain floor, 80% ratio block, clock-skew check) — cheap, and they prevent the worst failure mode of any automatic forgetting.
6. **Warm-up trigger threshold (1→2→4→8→…→N)** — a scheduling trick, costs one integer.
7. **The small-corpus BM25 guard** — a fixed relevance threshold over a few dozen entries silently returns nothing; this is the fix.

Where the design does **not** transfer, and where it is worse for this use case:

- **Multi-tenancy is pervasive and irrelevant here.** `team_id`/`user_id`/`agent_id`/`task_id` are threaded through every table, index, prompt, and filter. For a single-user local CLI this is pure cost. `oc-memory`'s global/project scopes are the right size.
- **The operational surface is enormous**: quota manager, credit calculator, ClickHouse exporter, Kafka producer, Langfuse span processor, OTLP backend, MongoDB adapter, a Panel UI, an offload server. `MemoryCore/src/offload/index.ts` alone is 118 KB; `gateway/server.ts` is 120 KB. None of this is memory design.
- **Their CJK strategy is strictly worse for a no-dependency binary.** Write-side jieba segmentation needs a dictionary and a native/WASM module, and it degrades to a plain Unicode regex split when unavailable (`sqlite.ts:227-233`) — which is materially worse for Chinese than `oc-memory`'s existing trigram table. Keep the trigram approach. The *pattern* worth taking is only the `content` (indexed, processed) + `content_original UNINDEXED` (raw, for display) column split.
- **License is `NOASSERTION`.** Read for ideas; do not copy code verbatim.
- **The FTS5 helpers are self-declared as borrowed from the host agent CLI** (`sqlite.ts:148`, `sqlite.ts:297`: "adapted from openclaw core hybrid.ts", "Mirrors the formula in openclaw core hybrid.ts"). Check upstream `opencode`/`openclaw` before treating `bm25RankToScore` and `buildFtsQuery` as this repo's contribution — the port may already have a sibling implementation.

---

## 6. Concrete adoption candidates, ranked

Categories: **(a)** expressible over SQLite/FTS5 in pure Rust · **(b)** needs an external service, at most an optional plugin · **(c)** inapplicable.

### 1. Version-as-rows with `is_head` + partial unique index — **(a)** — highest value
Replace overwrite-in-place with append-a-version. `UNIQUE(entry_id, version)`, plus `CREATE UNIQUE INDEX ... ON entries(scope, name) WHERE is_head=1 AND status='active'`. Add `content_hash` so a re-write of identical content is a no-op. Prune with a `KEEP_RECENT` count, not a time rule.
*Improves*: supersession and contradiction handling become structural rather than textual; edit history at near-zero cost; idempotent writes.
*Touches*: `oc-memory` schema + migration, the batch-atomic apply path (the head flip must be inside the same transaction as the insert), FTS triggers must index head rows only.
*Evidence*: `skill-store-ddl.ts:22-66`, `skill-versioning.ts:210-222, 401-412`.

### 2. Structured columns on entries: `type`, `priority`, plus a validity interval — **(a)**
Add `type TEXT`, `priority INTEGER DEFAULT 50`, `valid_from`/`valid_to` separate from `created_at`/`updated_at`. Adopt the 0–100 priority bands. For a coding agent, the `work_fact` / `work_task` / `work_method` / `work_artifact` quartet is a better starting taxonomy than the chat-oriented one.
*Improves*: enables ranking and staleness filtering in SQL; renders as a type tag in the injected text so the model knows what kind of claim it is reading.
*Touches*: schema, the `\n§\n` entry format (or a metadata sidecar to keep the format stable), the injection renderer.
*Evidence*: `sqlite.ts:610-630`; `l1-dedup.ts:313`; `prompts/l1-dedup.ts:69`.

### 3. RRF fusion of FTS5 and trigram result lists — **(a)** — cheapest real win
~25 lines of Rust. `score = Σ 1/(60 + rank + 1)` over each ranked list, summed per id, sorted descending.
*Improves*: `oc-memory` has two independent indexes whose scores are not comparable; RRF merges them by rank and needs no calibration. Also the natural place to fuse a recency or priority ranking later.
*Touches*: the search function only. No schema change.
*Evidence*: `search-utils.ts:14-62`.

### 4. Small-corpus BM25 guard + `bm25RankToScore` normalization — **(a)**
If every FTS hit falls below the relevance threshold *and* the total hit count is within `max_results`, return them anyway — IDF collapses on tiny corpora. Pair with the saturating normalization `rank<0 ? -rank/(1-rank) : 1/(1+rank)` mapped into (0,1) when a comparable score is needed.
*Improves*: prevents the "memory store has 12 entries and recall silently returns nothing" failure.
*Touches*: search function only.
*Evidence*: `auto-recall.ts:544-563`; `sqlite.ts:295-306`.

### 5. Two-axis recall budget with separator accounting and a minimum-viable-truncation floor — **(a)**
`max_chars_per_entry` and `max_total_chars` applied together at *injection* time; count the `\n§\n` separator against the total; refuse to admit a final entry that would be truncated below a minimum useful length.
*Improves*: complements — does not replace — the existing store-level cap. The store cap bounds what is kept; this bounds what is injected per turn. The separator accounting and the anti-stub floor are the details that get hand-rolled implementations wrong.
*Touches*: the resident-store rendering / injection path.
*Evidence*: `auto-recall.ts:835-899`.

### 6. Deletion guardrails: min-retain floor, 80% ratio block, clock-skew check — **(a)**
Before any bulk prune: skip entirely if total ≤ floor; abort if the deletion would remove >80% of rows; warn if the computed cutoff implies a clock jump.
*Improves*: makes any future automatic forgetting safe to enable by default. Relevant even today for the character-cap eviction path.
*Touches*: the eviction/prune function.
*Evidence*: `memory-cleaner.ts:28-29, 130-150, 339-359`; `sqlite.ts:1592-1600`, `sqlite.ts:2072-2080`.

### 7. `content` (processed, indexed) + `content_original UNINDEXED` (raw, display) + scope columns UNINDEXED in the FTS row — **(a)**
Keep the trigram approach for CJK; adopt only the column split and the habit of carrying every filter column in the FTS row as UNINDEXED so one query returns everything needed to filter and rank.
*Improves*: no join back to the base table on the hot search path; a clean place to put any future write-side normalization (case folding, punctuation stripping) without corrupting displayed text.
*Touches*: FTS table definition + rebuild path.
*Evidence*: `sqlite.ts:1001-1039`.

### 8. Append-only audit table with no old values — **(a)**
`(audit_id, entry_id, action ∈ {update,delete}, version, updated_at_ms, request_id)`. Records *that* something changed, never *what it was*.
*Improves*: "why did my memory change?" becomes answerable without storing content twice. Pairs naturally with candidate 1.
*Touches*: new table, hooks in the apply path.
*Evidence*: `sqlite.ts:961-984`.

### 9. Warm-up trigger threshold and idle/debounce + downward-only timers — **(a)**, conditional
Only relevant if `oc-memory` ever gains an automatic capture path. Exponential threshold 1→2→4→8→…→N so a fresh session gets memory after one exchange; resettable idle timer for catch-up; a consolidation timer whose fire time may only move earlier; cancel rather than fire when the session is cold.
*Improves*: memory is useful immediately in a new session without paying extraction cost on every turn thereafter.
*Touches*: a capture scheduler that may not exist yet — skip if capture stays explicit/manual.
*Evidence*: `pipeline-manager.ts:29-70`.

### 10. Cursor/watermark-based incremental consolidation — **(a)**, conditional
Store a `last_processed` watermark per scope; process only rows past it; report `has_more` to drain a backlog. Needs the composite index `(scope, updated_at)`.
*Improves*: consolidation cost proportional to new content, not store size.
*Touches*: a consolidation pass, if one is added.
*Evidence*: `pipeline-manager.ts:150, 174, 929, 968`; `sqlite.ts:653`.

### 11. LLM-judged dedup with the `store | update | merge | skip` verdict — **(a)** structurally, but expensive
The *architecture* is fully SQLite-compatible: recall top-K≈5 candidates via FTS5, batch all pending writes plus their candidate pools into **one** LLM call, apply the verdict transactionally, fail open to `store` on any error. Take the timestamp-union rule (`merged_timestamps` = union of all involved, deduped, sorted) and the merge-raises-priority rule.
*Improves*: real contradiction resolution instead of duplicate accumulation.
*Cost*: an LLM call on the write path, plus prompt maintenance. Only worth it if capture is automatic. If the user authors memories explicitly, candidates 1 and 2 give most of the benefit for none of the cost.
*Evidence*: `l1-dedup.ts:33-43, 313, 350, 372-375`; `prompts/l1-dedup.ts:35-70, 110-111`.

### 12. `embedding_meta` provenance table + auto-reindex on provider/model/dimension change — **(b)**
Only matters for the optional embedding plugins (LanceDB/mem0/honcho). Persist `(provider, model, dimensions)` alongside the vectors; on mismatch, drop and rebuild, and treat "vectors exist but no provenance" as untrustworthy.
*Improves*: silently-wrong similarity search after a model swap is a real and hard-to-diagnose bug. The right place for this is inside each plugin, behind the existing boundary.
*Evidence*: `sqlite.ts:536-601`.

### 13. `sqlite-vec` (`vec0`) as an in-process vector index — **(b)**
It is not a running service, so it is *architecturally* closer than TencentDB — but it is a C SQLite extension that must be compiled/loaded, which conflicts with a single self-contained `unsafe_code = "forbid"` binary. Belongs behind the same plugin boundary as LanceDB, if anywhere. Note their own graceful pattern: `dimensions=0` ⇒ no vec tables at all, and the system runs FTS-only.
*Evidence*: `sqlite.ts:666-674, 788-796`.

### 14. TCVDB backend, native server-side hybrid search, sparse-vector BM25 encoding — **(b)** / **(c)**
`tcvdb.ts` (76 KB), `tcvdb-client.ts`, `tcvdb-skill-store.ts`, `searchL1Hybrid` native path. Requires a Tencent Cloud VectorDB instance. Note `bm25-local.ts` is *not* a local search engine — it is a sparse-vector **encoder** (`encodeTexts` / `encodeQueries` via their own npm package + jieba-wasm) whose only purpose is feeding TCVDB's server-side hybrid search. FTS5's built-in `bm25()` already covers this need locally. Not adoptable.
*Evidence*: `bm25-local.ts:1-91`; `auto-recall.ts:501-510`.

### 15. Team/tenant governance, ACL, Panel UI, quota/credits, ClickHouse/Kafka/OTLP telemetry, MongoDB metadata adapter — **(c)**
Inapplicable to a single-user local CLI. `sqlite-init.sql` in full is this layer.

### 16. `meta_assets` lifecycle columns (`confidence`, `expires_at`, `last_used_at`, `usage_count`) — **(c)** as evidence, **(a)** as an idea you would be inventing yourself
The columns exist; nothing in the retrieval or eviction path reads them. Adopting `last_used_at` + `usage_count` for LRU-ish eviction is a reasonable idea, but you would be designing it, not porting it. Do not treat this schema as a validated design.
*Evidence*: `sqlite-init.sql:148-183`.
