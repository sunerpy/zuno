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

Over HTTP, `prompt.files[].mimeType` (or `mime`) on
`POST /api/session/{sessionID}/prompt` is checked before the base64 payload is
decoded. The declared value is read as an RFC 2045 media type: surrounding
whitespace and any `;parameter` suffix are dropped and ASCII case is folded, so
`IMAGE/PNG` and `image/png; charset=binary` are accepted. Exactly the four types
above are admitted, together with five aliases browsers emit: `image/apng`,
`image/x-png`, and `image/vnd.mozilla.apng` for PNG, `image/jpg` and
`image/pjpeg` for JPEG. Every other `image/` subtype, such as `image/svg+xml`,
`image/bmp`, or `image/x-icon`, is refused with `400` and the message
`prompt.files[N] uses unsupported MIME type <value>; only PNG, JPEG, GIF and
WebP images are accepted`. A declaration that passes this check is still
compared with the detected bytes; see [Declared media types and display
names](#declared-media-types-and-display-names).

The source byte limit is 20 MiB by default. Zuno reads the source header and
checks dimensions, pixel count, and decoded byte count before any decoder
allocates; see [Decode limits](#decode-limits). EXIF orientation is applied,
animated input keeps its first frame, pixels are converted to 8-bit, and
metadata is removed. Transparent output is PNG; opaque output is JPEG. When a
caller declares a source media type, the declared spelling is normalized before
it is compared with the detected bytes; see [Declared media types and display
names](#declared-media-types-and-display-names). Direct image pastes are not
written to prompt recall history because the display handle alone cannot
reconstruct the image; after submission, the durable reference survives replay
and child-session continuation.

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

### Decode limits

Zuno reads the source header and applies two absolute limits before any decoder
allocates. No configuration value can raise them:

- 64,000,000 pixels. A larger source fails with `PixelLimit`.
- 167,772,160 decoded bytes. A larger source fails with `DecodedTooLarge`.

`max_pixels` lowers the byte limit but never raises it. The limit is
`max_pixels * 40 + 16 MiB`, clamped to at least 33,554,432 bytes and at most
167,772,160 bytes. Lowering `attachment.image.max_pixels` is therefore the
operator's lever over per-admission decode memory and CPU. At the default
`max_pixels` of 4,000,000, an 8000x8000 grayscale PNG is admitted and resized.
At `max_pixels` 1,000,000 the same source is refused as `image decodes to
64000000 bytes, exceeding the 56777216-byte decode limit` before a decoder
allocates. The 40-bytes-per-output-pixel exchange rate covers the resize
intermediate, which dominates peak memory: it is the source width multiplied by
the target height multiplied by 16 bytes.

Reading a canonical object back is never gated. `read` verifies the digest and
returns the stored bytes. Building a route-specific derived image from a stored
object is bounded differently, because that object was already admitted by this
host's own configuration: it decodes when the current admission policy could
itself have written it, or when it is inside the floor every released build
resolved, under absolute backstops of 128,000,000 pixels and 512,000,000
decoded bytes that no configuration raises. The floor, and what lowering a limit
does to objects above it, are described under
[Durable and provider behavior](#durable-and-provider-behavior). An object above
those backstops remains readable and exportable but cannot be re-encoded for a
model request; the failure is typed rather than reported as a corrupt file.

### Declared media types and display names

When a caller declares a source media type -- the ACP and HTTP ingress paths do
-- the declared spelling is normalized before it is compared with the detected
type: surrounding whitespace and any `;parameter` suffix are removed, the value
is lowercased, and `image/jpg`, `image/x-png`, `image/apng`,
`image/vnd.mozilla.apng`, and `image/pjpeg` are folded onto the canonical type
they name. A declared type that still disagrees with the detected bytes is
refused with a typed mismatch error. The stored media type is always the
detected one.

A display filename is a presentation value, never a path. Zuno keeps only the
final path segment, then removes every character that cannot be displayed
safely: C0/C1 controls, every whitespace character other than a plain space,
format characters such as `U+00AD` and `U+FEFF`, bidi controls and isolates,
`Default_Ignorable_Code_Point` characters such as `U+3164` HANGUL FILLER and the
variation selectors, `U+2800` BRAILLE PATTERN BLANK, private-use code points,
and noncharacters. The result is truncated on a character boundary to at most
255 characters and 255 bytes. If nothing displayable remains -- a name that is
`.` or `..` once forbidden characters are stripped and the ends trimmed, a
control-only name, or an empty string -- the display name becomes `image`. A
name that merely looks like a drive-relative path, such as `x:y.png`, keeps its
first character; only a prefix that carries a separator is removed. The same
reduction runs when a stored or client-supplied `ImageAttachmentRef` is
deserialized, so a hand-edited reference, or one written by an earlier release,
cannot reintroduce an unsanitized name.

## Durable and provider behavior

New durable file parts contain only an `ImageAttachmentRef`: a
`sha256:<hex>` content id, display filename, normalized media type, dimensions,
and encoded size. They do not contain base64. Canonical objects are stored
under:

```text
$DATA/attachments/v1/<database-identity>/objects/<prefix>/<digest>
```

Directories and files are private. A filesystem failure reports a store-relative
path such as `objects/<prefix>/<digest>` rather than an absolute path, so an
error rendered to a model or written to a log does not disclose the data root.
Publication uses a temporary file, file sync, and atomic rename. Concurrent
admission of identical normalized bytes converges on the same digest. A
route-specific derived image is cached by attachment id, policy version, and
`ImageRequestPolicy`; raising the internal policy version is what invalidates a
cached derived image.

`image.max_width`, `image.max_height` and `image.max_pixels` also bound which
stored objects Zuno will decode again when a provider route needs a smaller
derived image, with one floor: an object whose decoded size at its own bytes per
pixel is within the route's `max_pixels x 4 + 16 MiB` (32,777,216 bytes on the
default route, for example a 2828x2828 RGBA8 PNG or a 3300x3300 RGB8 JPEG)
always resolves, whatever the current admission values are, because the released
builds resolved it. Above that floor, a stored object is decoded only if the
current admission policy could itself have written it: lowering
`image.max_pixels`, `image.max_width` or `image.max_height` below the values that
admitted such an object makes every later turn of a session whose history
carries it fail with a typed `PixelLimit`, `DecodedTooLarge` or
`DecodeWorkTooLarge` attachment error until the value is raised back. Reading the
object and serving an already cached derived image are not gated; only a cold
derivation is.

Provider request assembly resolves the durable object immediately before the
request and continues to give provider adapters the existing inline
provider-neutral image block. Providers therefore do not own storage or replay.
TUI, `zuno run --file`, ACP, and server image ingress all admit the image before
the durable inbox write. Resolution runs off the request reactor inside a
process-wide budget of two concurrent resolutions, sized against the
900,000,000-byte working set a stored object may cost to re-encode; a turn whose
history needs a third waits for a slot rather than starting a third decode, and
the slot is released when the work finishes, not when the waiting turn is
interrupted.

Historical file parts containing `media_type`/`data` remain readable and
replayable. They are not silently rewritten. A missing object, digest mismatch,
or reference mismatch is a permanent durable-state failure: Zuno does not fall
back to an original path or mechanically retry the provider call.

A durable file part also carries a top-level `filename` beside the typed
reference, because the TUI and ACP replays label a part with it, and a part
written by an earlier release stores every field exactly as the client sent it.
Zuno leaves the stored row as written and sanitizes each field on its way into a
model request instead. `filename` gets the display-name reduction: only the final
path segment is kept, control and format characters are stripped, and the result
is capped at 255 characters and 255 bytes — the same reduction as [Declared media
types and display names](#declared-media-types-and-display-names). A resource
link's `title`, `description`, and `mime` are free text and lose the same
forbidden characters without the basename reduction, since a title may
legitimately be a path; `title` and `mime` are capped at 255 characters and
`description` at 1,024, and a field left empty is omitted. The `url` loses the
same characters before it becomes the link's URI and is not capped, because it
may be a `data:` URL. The `mime` of a historical inline image is a wire token
rather than text: it is read the way a declared type is read above — parameters
dropped, case folded, the same aliases mapped — and must name one of PNG, JPEG,
GIF, or WebP. A row whose declared type does not is projected as a resource link
when it has a `url` and contributes nothing to the request otherwise; earlier
releases sent such a value to the provider, which rejected the whole request.
When a stored file part carries inline image data under a media type Zuno cannot
send (anything other than PNG, JPEG, GIF, or WebP) and its `url` is a `data:`
URL, the part is omitted from the model request entirely rather than being sent
as a resource link, so the refused image's base64 payload never reaches the model
as text; a refused image whose `url` points to an external location is still
sent as a resource link. A value this release wrote is already a fixed point of
every reduction and passes through unchanged.

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
