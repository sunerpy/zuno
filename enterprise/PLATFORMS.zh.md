# 企业与个人模式的平台边界

这是用户在 2026 年 9 月 12 日对已批准计划的补充：企业服务部署只验收 Linux
amd64／arm64；个人 Zuno 保持现有 Linux、macOS、Windows 支持，包含 TUI 和 ACP。

| 范围 | 运行与验证平台 |
| --- | --- |
| 企业控制面、Agent Worker、执行网关 | Linux amd64／arm64 原生运行 |
| 企业 Web 部署和 ACP bridge 服务 | Linux 服务部署 |
| 企业 Web 浏览器客户端 | 浏览器协议与 UI 验证，Windows 浏览器可以访问 Linux 服务 |
| 个人 TUI、ACP、本地 HTTP 及其共享内核 | 保留现有个人平台矩阵，包含 Windows |

`zuno-server` 默认 feature 只包含个人 HTTP 功能。显式启用 `enterprise` 才接入
企业身份、PostgreSQL、Worker 和环境适配器，企业集成测试也要求该 feature。
个人构建不启用它。

企业专用 crate 声明 `package.metadata.zuno.distribution = "enterprise"`。
`scripts/cargo_surface.py` 据此计算个人模式的 workspace 包集合，并检查 CLI
正常依赖图不含企业服务。新增企业二进制和适配器同样声明该标记；共享 application、
types、engine 仍进入个人测试。

Windows Clippy 和测试调度只构建个人集合。预览 PR 若只修改明确识别的企业专用路径，
可以跳过 Windows；根 manifest／lockfile、共享代码、TUI、ACP、通用 CI 工具以及
未知路径仍保守运行个人 Windows 回归，不能仅凭分支名跳过。

企业 PostgreSQL 和 Docker gates 显式启用 `zuno-server/enterprise`，在两种 Linux
架构运行。企业发布产物原本就只包含这两个目标，现在预览发布的共享检查也限定 Linux。
涉及共享代码的 PR 仍在集成前完成个人 Windows 回归。

此边界不改变正式发布的平台矩阵、tag、安装器或本地数据命名空间。未完成的企业命令
仍不注册，预览发布等待完整运行时与故障验收后再启用。

另见 [English](PLATFORMS.md)、[计划](PLAN.zh.md)和[状态](STATUS.md)。
