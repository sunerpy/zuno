# 文档架构与覆盖地图

Zuno 把文档视为公共契约的一部分。只有用户、扩展作者、运维者和维护者都能找到负责
说明行为、边界、失败模式与验收路径的页面，一项能力才算完成。

“一切有文档”不是记录每个私有 helper，而是保证每个公共命令、配置字段、协议、持久
状态切换、安全边界、扩展接口、运维流程和发布产物都有一份权威页面，并且不存在互相
冲突的副本。

## 所有权规则

每次改变行为的变更都必须：

1. 指明公共表面及其权威页面；
2. 在实现变更中同步更新该页面；
3. 按适用范围说明默认值、权限、持久性、失败、恢复和平台差异；
4. 从最近的任务指南或索引链接到它；
5. 对不能静默漂移的安全或兼容边界增加文档契约测试。

Reference 页面负责精确字段与协议，Guide 负责操作顺序，Design 记录理由与被拒绝方案，
Operate 负责诊断、迁移、回滚与证据。README 只提供入口，不复制完整契约。

## 覆盖地图

| 公共表面 | 权威文档 |
| --- | --- |
| 产品范围与执行模型 | [Zuno 是什么](/zh/guide/what-is-zuno)、[Harness 运行时](/zh/operate/harness-runtime) |
| 安装与平台前置条件 | [安装](/zh/guide/installation)、[快速开始](/zh/guide/quick-start) |
| 配置、Provider、模型与凭据 | [配置参考](/zh/config/reference)、[Provider 与凭据](/zh/config/providers) |
| Agent、权限、Skill 与委派 | [Agent](/zh/guide/agents)、[自定义 Agent](/zh/config/custom-agents)、[权限](/zh/guide/permissions)、[编排](/zh/guide/orchestration) |
| Agent 与扩展实现 | [开发 Agent 与扩展](/zh/guide/extension-development)、[插件](/zh/guide/plugins)、[进程插件（英文）](https://github.com/sunerpy/zuno/blob/main/docs/process-plugin-development.md) |
| 原生组件、Profile、Driver 与生命周期 | [Harness 运行时](/zh/operate/harness-runtime)、[开发 Agent 与扩展](/zh/guide/extension-development) |
| 工具、MCP、LSP、网络、Shell 与沙箱 | [工具](/zh/guide/tools)、[权限](/zh/guide/permissions)、[Shell 沙箱路线图（英文）](/design/shell-sandbox-roadmap) |
| 会话、Prompt、Inbox、Goal、Plan、重试与恢复 | [会话](/zh/guide/sessions)、[持久状态](/zh/guide/durable-state)、[Harness 运行时](/zh/operate/harness-runtime) |
| TUI、headless、ACP、HTTP 与客户端 projection | [CLI 参考](/zh/cli/)、[编辑器与 ACP](/zh/guide/editors)、[客户端接口（英文）](/design/client-interfaces) |
| 图片、文件引用、导入与导出 | [附件](/zh/guide/attachments)、[可移植环境包](/zh/operate/portable-bundles) |
| SQLite schema、迁移、保留与连续性 | [数据库生命周期](/zh/operate/migration)、[会话保留](/zh/operate/session-retention)、[History 与 Notes](/zh/config/continuity) |
| 日志、诊断、资源门禁与性能 | [日志](/zh/operate/logging)、[FAQ](/zh/operate/faq)、[诊断](/zh/operate/diagnostics)、[资源门禁](/zh/operate/resource-gates)、[性能方法（英文）](/perf-methodology) |
| Product Agent、记忆与学习 | [Product Agent（英文）](/design/product-agents)、[常驻记忆（英文）](/design/memory-learning)、[学习闭环（英文）](/design/user-learning-flywheel) |
| Self-update、CI、发布资产与回滚 | [Self-update](/zh/operate/self-update)、[发布流水线](/zh/operate/release-pipeline) |

生成 schema 或穷举协议尚未翻译时，英文页面是精确权威来源；中文任务指南仍必须说明
可执行流程与安全边界，并链接到精确参考。

## 变更检查表

合并公共变更前确认：

- 命令帮助、配置 schema、运行时行为与文档一致；
- 新状态与错误说明运维动作和恢复方式；
- 持久 schema 变更包含迁移与数据保全证据；
- 支持的 OS 与架构差异明确；
- 扩展变更明确接口、Provider 与消费者；
- 示例验证实际交付产物，而不只是源码树代码；
- 已移除行为与过期兼容描述从搜索结果和导航中消失；
- 中英文入口都能到达更新后的契约。

## 站点发布

`docs/` 下的 Markdown 由 Zuno 仓库拥有。文档进入 `main` 后，
`.github/workflows/publish-docs.yml` 会检出 Firlab 仓库并运行
`docs/scripts/sync-zuno-docs.sh`。同步脚本复制 Zuno 拥有的文档树、记录精确 Zuno
commit，随后 Firlab 的 VitePress workflow 发布到 `zuno.firlab.app`。

只有以下证据都存在，发布才算完成：

1. 合并 commit 对应的 Zuno 文档 workflow 成功；
2. 对应 Firlab commit 与部署成功；
3. 公网站点的中英文路由都能渲染；
4. 部署页面中的链接与代码块可用。

CI 前可用本地检查发现结构和渲染问题：

```sh
cargo test -p zuno --test docs
git diff --check

# 在一次性 Firlab checkout 中：
docs/scripts/sync-zuno-docs.sh /path/to/zuno
pnpm --dir docs build
```

## 维护本地图

新公共表面没有现有所有者时才新增一行。受众或生命周期不同导致所有权含糊时拆页。
不要为重复现有契约而新建页面；应链接权威页面，只补充任务特有的上下文。
