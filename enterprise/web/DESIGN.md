# Zuno enterprise workbench

This is the locked visual contract for the enterprise Web implementation.
Audience: enterprise developers, operators and designated approvers.
Primary job: advance an owned task, inspect what the Agent did, and make a
resource-specific approval decision. Tone: practical, restrained and technical.

## Structure

Use an authenticated application shell, not a marketing page. A narrow navigation
rail holds the workspace selector, new session action and owned session list.
The main column holds session context, grouped messages and the composer.
A collapsible details column shows the selected invocation, its source, arguments,
state, related Job and approval. On small screens navigation/details become
drawers; the conversation keeps the available width.

The first working routes expose sessions, actual pending approvals and private
Memory. Workflow, artifact and administration destinations appear only after
their handlers and authorization are implemented. No fake counts, demo users or
invented success metrics enter the application.

Queued input, committed content and transient drafts remain visually distinct.
Thinking and tool details are collapsed initially. Provider signatures and
encrypted data never reach components. Status is always expressed by text/icon
as well as color. An approval card shows the authoritative requested operation
and obtains a current server decision; presentation is never permission.

## Design system

The Figma draft is `4gXsDrGQgU67uyGV4hc6EH`. It uses the available Simple Design
System primitives as the component reference. Button, danger button, input and
navigation instances should stay linked in Figma. The implementation uses small
React primitives with corresponding semantic tokens.

There is no existing Web CSS or Code Connect in this repository. The design
adapts a restrained modern-minimal palette to a dense workbench. Hallmark's
marketing hero/footer and screenshot-tour structures do not apply to this
authenticated task surface.

| Token | Light value | Purpose |
| --- | --- | --- |
| `--color-canvas` | `#f5f6f8` | Application background |
| `--color-panel` | `#ffffff` | Conversation and details |
| `--color-subtle` | `#eef1f5` | Secondary surface and selected row |
| `--color-ink` | `#1d2433` | Main text |
| `--color-muted` | `#596477` | Secondary text |
| `--color-border` | `#d9dfe8` | Visible boundaries |
| `--color-accent` | `#2855c5` | Primary action and focus |
| `--color-accent-soft` | `#eaf0ff` | Selected/active context |
| `--color-danger` | `#b3261e` | Denial/destructive action |
| `--color-warning` | `#865400` | Waiting/uncertain attention |
| `--color-success` | `#21643d` | Confirmed success |

All component colors reference tokens. Add dark values as a second token set,
never ad-hoc component overrides. Body/display use Inter with native sans/CJK
fallbacks; code uses a system monospace stack. Fonts are self-hosted if added.
No external font/image request is required to use the application.

Use a 4/8/12/16/24/32 spacing scale; 6–10px control/panel radii; 14px body,
12px auxiliary text, 18–24px functional headings. A control has a 40px desktop
height and a minimum 44px touch target. A single visible focus ring must survive
keyboard navigation and high contrast. Motion is limited to short state changes
and respects reduced motion.

## Components and behavior

Shared primitives: Button, IconButton, Field, Select, Badge, Alert, Disclosure,
Dialog/Drawer, EmptyState and LoadingState. Cover resting, hover, focus, pressed,
disabled, loading, error and success states. Use semantic elements and accessible
names, restore focus when a drawer closes, and avoid keyboard traps.

Content primitives consume the generated `SessionItem`, `InvocationAction`,
`InvocationSource`, `InvocationState`, `ContentBlock` and `UiAction` unions.
Markdown uses safe rendering, code is plain text, remote images are not fetched
from model-controlled URLs, and external links require a user action. Large
results are bounded previews with explicit truncation.

Keep draft input until admission is confirmed. An uncertain submission retries
the same request ID/body, rather than silently creating a second task.
Session switches cancel old HTTP subscriptions and cannot mix another session's
frames. A stale approval or CAS conflict refreshes authoritative data.

For the bounded shell, scrolling children use `min-height: 0` and grid columns
use `minmax(0, 1fr)`. Long paths, identifiers and CJK text must wrap or scroll
within their own content region. Scrolling older history does not auto-jump to
new output; disclose a “new activity” control instead.

## Verification

Capture and interact with the actual browser at 375, 768 and 1280 CSS pixels,
with 320/414 narrow-width checks. Verify empty, loading, denied, expired login,
long history, long code/path, approval, cancellation and reconnect states.
Check focus/hover, computed layout overflow, console/network errors and reduced
motion. Use fixture data only in the test harness, clearly separate from runtime.
Screenshots and evidence belong under the task validation directory.

The backend and browser protocol remain authoritative; this document does not
grant capabilities or change enterprise execution policy.
