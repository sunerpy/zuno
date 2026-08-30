# Images and file references

Zuno admits local images as typed prompt content and bounded UTF-8 files as
explicit text context. The TUI, child-session composer, durable inbox, replay,
headless `run` command, and provider request path share the same rich-content
model; clients do not create a private image-only agent loop.

## Paste an image in the TUI

Paste one existing image path as the complete paste payload. Zuno validates the
file and replaces the visible path with a draft handle:

```text
[Image #1]
```

The handle is presentation state. The image filename, detected MIME type, and
base64 bytes travel separately as a typed image block when the prompt is sent.
Deleting the handle before submission drops that image from the draft. A typed
path is ordinary text; automatic attachment occurs only for a paste event that
resolves to a supported local image.

Accepted path forms include an ordinary platform path, a matching quoted path,
`file://` URLs, `~/...`, and POSIX paths with escaped spaces. Native Windows
drive and UNC paths are resolved by Windows. Under WSL, an existing path such
as `C:\\Users\\me\\image.png` may resolve through `/mnt/c/...`.

When the terminal clipboard backend supplies image bytes directly, pasting the
clipboard image creates the same `[Image #N]` draft attachment. Clipboard MIME
and detected file content must agree.

Zuno detects content by magic bytes rather than filename extension. Supported
formats are:

- PNG (`image/png`);
- JPEG (`image/jpeg`);
- GIF (`image/gif`);
- WebP (`image/webp`).

Each decoded image is limited to 20 MiB. Direct image pastes are not written to
prompt recall history because the display handle alone cannot reconstruct the
bytes; after submission, the actual rich input is stored in durable session
parts and survives replay and child-session continuation.

## Reference project files

In the TUI, type `@` and select a project file, or enter a project-relative
token such as:

```text
Review @src/main.rs and compare it with @docs/architecture.png
```

References are resolved below the active project root after canonicalization.
Absolute paths, missing files, directories, and paths that escape the project
are refused. One prompt may reference at most 16 distinct files.

- A supported image becomes a typed image block and uses the 20 MiB image
  limit.
- Any other reference must be UTF-8 text no larger than 51,200 bytes and 2,000
  lines. Its bounded contents are inserted with explicit begin/end markers.
- Unsupported binary files, including PDFs, are not silently converted or
  uploaded.

An image path paste and one or more `@file` references can be combined in the
same prompt. Queue and steer submissions preserve the same typed content.

## Attach files in headless mode

`zuno run -f/--file` is repeatable:

```sh
zuno run "Explain the evidence" \
  --file ./screenshot.png \
  --file ./notes.txt
```

Images use the same format detection and 20 MiB limit as the TUI. Other files
must be bounded UTF-8 text under 51,200 bytes and 2,000 lines. A regular file is
required. `--command` and `--file` cannot be combined because custom-command
expansion does not yet carry typed attachments; Zuno fails explicitly instead
of dropping them.

## Durable and provider behavior

Before a model request, each image is persisted as a durable file part with its
filename, MIME type, data URL, and base64 payload. Reopening a session rebuilds
the same typed image block. Child sessions accept the same rich prompt shape as
root sessions.

Compaction never sends historical image bytes to the compaction model. It
replaces each image in the summary input with a label such as
`[Attached diagram.png (image/png)]`; the original durable session record is
unchanged.

The selected model route must advertise an image input modality. A generic
attachment flag without image input is not enough. A text-only model fails with
a typed permanent `unsupported_capability` error before a compatible transport
call, rather than silently omitting the image or retrying the same invalid
request.

Local attachment paths are read by the client process under the operating
system account running Zuno. A pasted `http://` or `https://` URL is text, not a
download request.
