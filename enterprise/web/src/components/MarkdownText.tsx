import Markdown from "react-markdown";

export default function MarkdownText({ text }: { text: string }) {
  return <Markdown skipHtml components={{
    a: ({ href, children }) => {
      if (!href || !/^https?:\/\//i.test(href)) return <span>{children}</span>;
      return <a href={href} target="_blank" rel="noopener noreferrer">{children}</a>;
    },
    img: ({ alt }) => <span className="resource-label">图片引用：{alt || "未命名图片"}</span>,
  }}>{text}</Markdown>;
}
