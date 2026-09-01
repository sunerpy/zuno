# 图像与文件引用

Zuno 接受本地图像作为类型化的提示词内容，接受有界的 UTF-8 文件作为显式文本上下文。TUI、子会话编辑区、持久 inbox、重放、无界面 `run` 命令和 provider 请求路径共用同一套富内容模型；客户端不会自建一套仅处理图像的私有 Agent 循环。

## 在 TUI 中粘贴图像

把一个已存在的图像路径作为完整的粘贴内容粘进来。Zuno 会校验该文件，并把可见的路径替换为一个草稿句柄：

```text
[Image #1]
```

这个句柄是展示状态。在提交之前删除该句柄会把那张图像从草稿中移除。手动键入的路径只是普通文本；只有当一次粘贴事件解析到一个受支持的本地图像时，才会自动附加。在输入进入持久 inbox 之前，Zuno 会把字节接纳并规范化到当前数据库专属的附件对象存储中，再用 `ImageAttachmentRef` 替换草稿载荷。

被接受的路径形式包括普通平台路径、成对引号包裹的路径、`file://` URL、`~/...`，以及带转义空格的 POSIX 路径。原生 Windows 盘符路径和 UNC 路径由 Windows 解析。在 WSL 下，像 `C:\\Users\\me\\image.png` 这样的已存在路径可能通过 `/mnt/c/...` 解析。

当终端剪贴板后端直接提供图像字节时，粘贴剪贴板图像会创建同样的 `[Image #N]` 草稿附件。剪贴板 MIME 与探测到的文件内容必须一致。

Zuno 通过 magic bytes 而不是文件扩展名来探测内容。支持的格式为：

- PNG（`image/png`）；
- JPEG（`image/jpeg`）；
- GIF（`image/gif`）；
- WebP（`image/webp`）。

源文件默认上限为 20 MiB。Zuno 会在无界解码之前检查源尺寸与像素数，应用 EXIF 方向，动画只保留第一帧，把像素转换为 8-bit，并移除全部元数据。透明输出使用 PNG，不透明输出使用 JPEG。直接粘贴的图像不会写入提示词召回历史，因为仅凭显示句柄无法重建图像；提交之后，持久引用会在重放与子会话续跑中存活。

## 引用项目文件

在 TUI 中输入 `@` 并选择一个项目文件，或者输入一个项目相对的 token，例如：

```text
Review @src/main.rs and compare it with @docs/architecture.png
```

引用在规范化之后于当前项目根之下解析。绝对路径、不存在的文件、目录，以及逃出项目范围的路径都会被拒绝。一条提示词最多可以引用 16 个不同的文件。

- 受支持的图像会进入同一条规范化对象管线。
- 任何其他引用必须是 UTF-8 文本，不超过 51,200 字节与 2,000 行。它的有界内容会带显式的起止标记插入。
- 不受支持的二进制文件，包括 PDF，不会被静默转换或上传。

一次图像路径粘贴和一个或多个 `@file` 引用可以出现在同一条提示词中。排队与引导提交保留同样的类型化内容。

## 在无界面模式下附加文件

`zuno run -f/--file` 可重复使用：

```sh
zuno run "Explain the evidence" \
  --file ./screenshot.png \
  --file ./notes.txt
```

图像使用与 TUI 相同的格式探测、接纳策略与持久对象存储。其他文件必须是有界的 UTF-8 文本，小于 51,200 字节与 2,000 行。必须是普通文件。`--command` 与 `--file` 不能同时使用，因为自定义命令展开目前还不携带类型化附件；Zuno 会显式失败，而不是把它们丢掉。

## 接纳策略

根配置的默认值为：

```json
{
  "attachment": {
    "image": {
      "auto_resize": true,
      "max_source_bytes": 20971520,
      "max_width": 2000,
      "max_height": 2000,
      "max_pixels": 4000000,
      "max_encoded_bytes": 5242880
    }
  }
}
```

图像先使用 Lanczos3 缩放到尺寸与像素预算以内。不透明图像依次尝试 JPEG 质量 90、80、70、60、50；仍超过编码上限时，把当前宽高缩到 85% 后重试。透明 PNG 同样按 85% 继续缩小并重编码。`auto_resize: false` 或最终无法满足硬编码上限时，接纳以类型化错误失败，不发布对象。

`max_base64_bytes` 不是配置字段。限制针对源字节、解码尺寸/像素与规范化后的编码对象，而不是某一种传输专用的 base64 表示。

## 持久化与 provider 行为

新写入的持久文件 part 只保存 `ImageAttachmentRef`：`sha256:<hex>` 内容 id、展示文件名、规范化 MIME、尺寸与编码大小，不保存 base64。规范对象位于：

```text
$DATA/attachments/v1/<database-identity>/objects/<prefix>/<digest>
```

目录与文件使用私有权限。发布过程使用临时文件、文件同步与原子 rename；并发接纳相同的规范化字节会收敛到同一个 digest。请求衍生图按 attachment id、策略版本与 `ImageRequestPolicy` 缓存。

Provider 请求组装只在真正发请求之前解析对象，并继续向现有 provider 适配器提供 inline 的 provider-neutral 图像块。因此 provider 不拥有存储或重放生命周期。TUI、`zuno run --file`、ACP 与 Server 的图像入口都会先接纳，再写入持久 inbox。

历史上包含 `media_type`/`data` 的文件 part 仍可读取和重放，但不会被静默重写。对象缺失、digest 不符或引用元数据不符属于永久持久状态失败；Zuno 不会回退到原始路径，也不会机械重试 provider 调用。

压缩绝不会把历史图像字节发给压缩模型。它会把摘要输入中的每张图像替换为一个标签，例如 `[Attached diagram.png (image/png)]`；原始的持久会话记录保持不变。

所选模型路由必须公布图像输入模态。仅有一个通用的附件标志、却没有图像输入是不够的。纯文本模型会在发出兼容的传输调用之前，以一个带类型的永久 `unsupported_capability` 错误失败，而不是静默省略图像或重试同一个无效请求。

Session export 默认把持久对象重新内联为 data URL，使导出保持可移植。Session prune 与附件 GC 只清理同一个数据库身份下已经没有存活引用的对象；一个数据库绝不会删除另一个数据库的附件对象。

本地附件路径由客户端进程以运行 Zuno 的操作系统账户读取。粘贴进来的 `http://` 或 `https://` URL 是文本，不是一次下载请求。
