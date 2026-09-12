# Enterprise activity SDK

This private package consumes the public history/frame API. It is separate from
Worker credentials and transport. Enterprise services run on Linux; this SDK can
run in browsers or Node.js 22+ on client platforms, including Windows.

```sh
npm ci --ignore-scripts
npm run generate
npm test
```

Generation reads `zuno-types` directly. Commit the generated Schema, TypeScript
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
Merge it with `state.mergeHistory(page)`; newer frame revisions win. A sequence
gap requests missing frames, while a conflicting immutable frame requires a
fresh authorized snapshot. Keep live provider deltas separate from this state.

中文：该包只消费经过认证的公共历史／frame 接口；动态名称、工具来源和调用状态来自
Rust 生成的判别联合。来源不代表执行授权。历史分页固定快照，断线从最后提交游标补读，
整页验证失败不会留下部分更新。完整说明见 [ACTIVITY.zh.md](../ACTIVITY.zh.md)。
