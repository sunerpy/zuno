import { lazy, Suspense } from "react";
import type { ContentBlock } from "@zuno/enterprise-sdk";

const Text = lazy(() => import("./MarkdownText"));
export function Content({ blocks }: { blocks: ContentBlock[] }) {
  return <div className="content-blocks">{blocks.map((block, index) => {
    switch (block.kind) {
      case "text": return <div key={index} className="markdown"><Suspense fallback={<p>{block.text}</p>}><Text text={block.text} /></Suspense>{block.truncated && <small>显示截断预览</small>}</div>;
      case "terminal": return <div key={index}><pre className="terminal">{block.text}</pre>{block.truncated && <small>输出已截断</small>}</div>;
      case "code": return <div key={index}><pre><code>{block.code}</code></pre>{block.truncated && <small>代码已截断</small>}</div>;
      case "diff": return <pre key={index} className="diff">{block.diff}</pre>;
      case "structured": return <pre key={index}>{JSON.stringify(block.value, null, 2)}</pre>;
      case "image": return <span key={index} className="resource-label">{block.alt || block.resource.name}</span>;
      case "resource": return <span key={index} className="resource-label">{block.resource.name}</span>;
    }
  })}</div>;
}
