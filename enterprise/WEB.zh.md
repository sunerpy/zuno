# 企业会话工作台

`enterprise/web` 的 React＋TypeScript 应用使用统一的公共应用与活动协议。浏览器不
持有 Agent 循环、Worker 凭证或本地命令执行器。企业服务部署支持 Linux amd64／arm64；
Windows、macOS 浏览器可以访问 Linux 服务。

## 构建与启动

```sh
npm --prefix enterprise/sdk ci --ignore-scripts
npm --prefix enterprise/web ci --ignore-scripts
npm --prefix enterprise/web run build
```

为控制面配置已有的 `browser` OIDC 选项，并将 `service.webAssetsDirectory` 设置为
`enterprise/web/dist` 的绝对路径，或预览压缩包解出的 `web` 目录。登录回调仍为
`/auth/callback`，`/` 跳转到 `/app/`。详见[部署](DEPLOYMENT.zh.md)与
[浏览器认证](BROWSER.zh.md)。

仅在成功加载真实资源包时注册静态路由。启动拒绝符号链接、未知类型和缺少 `index.html`
的目录；上限为 128 个文件、32 个目录、单文件 8 MiB、总计 32 MiB。控制面从内存提供
启动时加载的同一批内容，部署新资源包需要受控重启，请求不能指定宿主文件路径。

资源包自带 Inter、Noto Sans SC 字体与 `/app/assets/licenses.txt` 第三方许可说明。
Markdown 使用独立代码分块。浏览器策略只允许同源脚本、样式、字体与 API，不开启内联
脚本或 `eval`。静态资源通过 ETag 重新验证，不嵌入环境 token 或服务地址。反向代理
需要保留配置的公共 Host 与 HTTPS。

## 当前能力

- 企业登录／退出、已配置工作区选择、自有会话分页。
- 创建会话、持久输入接纳、停止任务与响应丢失后的原请求查询。输入接纳结果不确定时，
  固定原始内容与 request ID，直到重试确认或确认拒绝。
- 已提交消息、工具动作／来源／状态、默认折叠的可展示思考、独立临时进度和权威审批详情。
- 最多 2,000 项的历史窗口。翻阅旧页时保留旧内容，同时推进提交游标；返回最新记录时
  获取新快照。缺失 frame 时重新获取经过授权的快照。
- 响应式导航与详情抽屉、键盘焦点约束和恢复、深色主题及减少动态效果。

用户和模型文本不启用原始 HTML；模型提供的外部图片不会自动请求，链接由用户主动打开。
加密推理、签名、私有续接快照与执行凭证只留在服务端。浏览器不在 Web storage 保存
access token 或 ID token。

切换会话会取消旧订阅。请求携带页面已载入的租户／主体／应用坐标；即使另一标签页更换
了 HttpOnly 登录 cookie，旧页面也不能静默借用新账号执行。服务端将坐标与已认证身份
比较，它只能收紧请求范围，不能授予权限。

审批展示当前操作、效果和有效期。组织服务在事务提交时继续检查角色和审批应用资格；
UI 展示或 ACP 回复均不能替代该判定。

当前工作台注册会话及其审批详情。Memory 管理、组织队列、Workflow／Council 运行图、
学习、管理审计、ACP bridge 与完整 TUI 接入仍属于待实施范围；处理器未完成前不出现
相关菜单。

## 验证与预览产物

`npm --prefix enterprise/web test` 构建实际资源包，在明确隔离的模拟 API 上运行
Chromium，验证 320／375／414／768／1280 宽度、键盘焦点、外部图片不请求、登录退出、
账号切换、响应丢失、历史／会话分页以及迟到审批回复。

构建 Web 后，将 `ZUNO_ENTERPRISE_WEB_DIST` 设为资源包绝对路径，再运行
`python3 scripts/check_enterprise_docker.py`。原生可执行测试启动 TLS 控制面、网关、
两个 Worker、PostgreSQL 和 rootless Docker；Chromium 使用签名 issuer fixture
完成 OIDC／PKCE 登录，只读取自己的历史，提交任务并明确批准命令，最后观察续接后的
模型结果。这是真实服务与浏览器验证，身份及模型提供商为测试 fixture。

企业客户端 CI 检查生成协议、SDK 恢复和浏览器行为；Linux amd64／arm64 Docker
通道也运行原生浏览器场景。预览发布下载客户端任务验证后的 Web 包，将同一批字节与
各平台二进制一起打包，校验和及来源证明覆盖整个压缩包。正式安装器、文档站点不受影响；
发布继续等待预览验收完成。

另见 [English](WEB.md)、[活动协议](ACTIVITY.zh.md)、[应用 API](APPLICATION.zh.md)
和[实施状态](STATUS.md)。
