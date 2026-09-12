# Enterprise activity SDK

This private package consumes the public history/frame API. It is separate from
Worker credentials and transport. Enterprise services run on Linux; this SDK can
run in browsers or Node.js 22+ on client platforms, including Windows.

```sh
npm ci --ignore-scripts
npm run generate
npm test
```

Generation reads `zuno-types` and `zuno-application` directly. Commit the generated Schema, TypeScript
and bundled validators together; `npm run check:generated` detects drift.

```ts
import { ActivityClient, ActivityState } from "@zuno/enterprise-sdk";

const client = new ActivityClient({
  baseUrl: new URL("/app/api/v1/", location.origin).href,
});
const state = new ActivityState(sessionId);
state.replaceHistory(await client.history(sessionId));
for await (const page of client.watch(sessionId, state.cursor, { signal })) {
  state.apply(page);
  render(state.items());
}
```

An older page uses its original `through` snapshot and returned `before` cursor.
Merge it with `state.mergeHistory(page)`; newer frame revisions win. To keep older
content within a bounded reading window, use `state.mergeHistory(page, {retain:
"older"})`; new commits still advance the cursor. `replaceHistory` returns to a
fresh latest window. A sequence
gap requests missing frames, while a conflicting immutable frame requires a
fresh authorized snapshot. Keep live provider deltas separate from this state.

`EnterpriseClient` adds typed workspace/session listing, creation, input-version
CAS, submission and original request lookup, Job cancellation and approvals.
Mutations preserve the caller's request ID and do not retry automatically.
The Web passes `browserContext: () => JSON.stringify([tenant, principal, client])`
after verifying `/auth/session`; that option is limited to the cookie/BFF surface.
External API clients use a separately configured bearer token callback.

中文：该包只消费经过认证的公共历史／frame 接口；动态名称、工具来源和调用状态来自
Rust 生成的判别联合。来源不代表执行授权。历史分页固定快照，断线从最后提交游标补读，
整页验证失败不会留下部分更新。完整说明见 [ACTIVITY.zh.md](../ACTIVITY.zh.md)。
`EnterpriseClient` 补充类型化应用操作；修改失败不自动重试，调用方保存原 request ID
并通过接纳查询核查。旧历史阅读窗口用 `retain: "older"` 保留旧页，返回最新时获取新快照。
