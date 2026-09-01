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

The handle is presentation state. Deleting it before submission drops that
image from the draft. A typed path is ordinary text; automatic attachment
occurs only for a paste event that resolves to a supported local image. Before
the input enters the durable inbox, Zuno admits and normalizes the bytes into
the database-scoped attachment store and replaces the draft payload with an
`ImageAttachmentRef`.

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

The source byte limit is 20 MiB by default. Source dimensions and decoded pixel
count are checked before an unbounded decode. EXIF orientation is applied,
animated input keeps its first frame, pixels are converted to 8-bit, and
metadata is removed. Transparent output is PNG; opaque output is JPEG. Direct
image pastes are not written to prompt recall history because the display
handle alone cannot reconstruct the image; after submission, the durable
reference survives replay and child-session continuation.

## Reference project files

In the TUI, type `@` and select a project file, or enter a project-relative
token such as:

```text
Review @src/main.rs and compare it with @docs/architecture.png
```

References are resolved below the active project root after canonicalization.
Absolute paths, missing files, directories, and paths that escape the project
are refused. One prompt may reference at most 16 distinct files.

- A supported image is admitted through the same normalized object pipeline.
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

Images use the same format detection, admission policy, and durable object
store as the TUI. Other files must be bounded UTF-8 text under 51,200 bytes and
2,000 lines. A regular file is required. `--command` and `--file` cannot be
combined because custom-command expansion does not yet carry typed attachments;
Zuno fails explicitly instead of dropping them.

## Admission policy

The default root configuration is:

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

Images are first resized with Lanczos3 to satisfy dimensions and pixel budget.
Opaque images try JPEG quality 90, 80, 70, 60, then 50. If the encoded limit is
still exceeded, the image is reduced to 85 percent of its current dimensions
and the ladder runs again. Transparent PNG uses the same 85-percent reduction
until it fits. With `auto_resize: false`, or when no valid image can meet the
hard encoded limit, admission fails with a typed error and publishes no object.

`max_base64_bytes` is not a configuration field. Limits apply to source bytes,
decoded dimensions/pixels, and the normalized encoded object rather than to a
transport-specific base64 representation.

## Durable and provider behavior

New durable file parts contain only an `ImageAttachmentRef`: a
`sha256:<hex>` content id, display filename, normalized media type, dimensions,
and encoded size. They do not contain base64. Canonical objects are stored
under:

```text
$DATA/attachments/v1/<database-identity>/objects/<prefix>/<digest>
```

Directories and files are private. Publication uses a temporary file, file
sync, and atomic rename. Concurrent admission of identical normalized bytes
converges on the same digest. A route-specific derived image is cached by
attachment id, policy version, and `ImageRequestPolicy`.

Provider request assembly resolves the durable object immediately before the
request and continues to give provider adapters the existing inline
provider-neutral image block. Providers therefore do not own storage or replay.
TUI, `zuno run --file`, ACP, and server image ingress all admit the image before
the durable inbox write.

Historical file parts containing `media_type`/`data` remain readable and
replayable. They are not silently rewritten. A missing object, digest mismatch,
or reference mismatch is a permanent durable-state failure: Zuno does not fall
back to an original path or mechanically retry the provider call.

Compaction never sends historical image bytes to the compaction model. It
replaces each image in the summary input with a label such as
`[Attached diagram.png (image/png)]`; the original durable session record is
unchanged.

The selected model route must advertise an image input modality. A generic
attachment flag without image input is not enough. A text-only model fails with
a typed permanent `unsupported_capability` error before a compatible transport
call, rather than silently omitting the image or retrying the same invalid
request.

Session export re-inlines durable objects as data URLs by default so the export
remains portable. Session prune and attachment garbage collection remove only
objects not referenced by the same database identity; one database never
deletes another database's attachment objects.

Local attachment paths are read by the client process under the operating
system account running Zuno. A pasted `http://` or `https://` URL is text, not a
download request.
