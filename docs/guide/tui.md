# The terminal application

`zuno tui` is the interactive surface, and it is also what bare `zuno` runs. It is a view
over the durable runtime rather than a client with its own agent loop, which is why
anything you see in it can be reconstructed from session events afterwards.

```sh
zuno
zuno tui --continue
zuno tui --session ses_1a2b3c --sandbox read-only
zuno tui --model openai/gpt-5 --prompt "review the diff on this branch"
```

## Background TUI and SSH reconnection

`zuno tui --background` starts or reuses one per-user supervisor, launches the real
`zuno tui` inside a retained pseudo-terminal, and attaches the current terminal. The
supervisor owns the child process and scrollback; the attachment owns only the current
SSH terminal. A dropped SSH connection therefore closes the attachment without sending
shutdown to the TUI.

```sh
zuno tui --background
# Detach with Ctrl+]

zuno tui --background-list
zuno tui --attach pty_01abc...
```

Reattachment replays the retained terminal from the beginning, then hands off to live
output without a gap. The client forwards terminal resize changes while attached.
`--background-stop <pty-id>` terminates one retained TUI, and
`--background-shutdown` stops the supervisor and every PTY it owns.

The control server binds to `127.0.0.1` on an operating-system-assigned port. It uses a
random Basic-auth password stored with mode `0600` in a mode-`0700` data directory on
Unix. The connection also uses a scoped, single-use PTY ticket. Ambient HTTP proxies are
bypassed only for this declared loopback control plane. The supervisor is detached with
`nohup` plus a new process group on Unix and detached process flags on Windows.

This retained-terminal mode preserves the exact TUI process, including an active turn.
It is different from ordinary `--session` resume, which starts a new process and rebuilds
the session view from SQLite after the previous process ended.

## Screen regions

| Region | Contents |
| --- | --- |
| Transcript | Durable assistant content, tool cards, errors, interruption markers |
| Sidebar | Sessions, jobs, usage, and durable child sessions |
| Queue dock | Durable FIFO follow-ups waiting above the composer during active work |
| Composer | Your draft input |
| Identity row | Resolved agent, catalog model display name, configured reasoning effort |
| Final row | Live control surface: turn pulse, interrupt key, prompt occupancy, command key, agent and model badge |

The identity row follows the bottom of a short reply and becomes sticky above the composer
once content fills the viewport. The final row repeats the current agent, model, and effort
as a neutral badge, so the selection for the next turn stays visible while a turn runs.
Pressing Tab updates that badge immediately while the actual host replacement stays
deferred to the turn boundary. When the application opens with `--continue` or
`--session`, the identity row shows the Agent, model, and effort the session last ran
with; a `--model` or `--agent` flag outranks the saved value for this process.

Transient "working" rows are not inserted into the transcript. Durable activity, errors,
interruption markers, and assistant content are.

Plan and Todo sidebar state is pushed by the same `WorkStateObserver` that commits the
SQLite mutation. The mounted root session is filtered by session id, then its exact new
revision is applied and the screen is woken immediately; the sidebar does not wait for
the provider turn to finish, and a child Plan cannot overwrite the root panel.

Context occupancy is the most recent complete provider prompt divided by the catalog
context limit. It is replaced on each provider report rather than accumulated across the
session; cumulative token buckets live in the usage projection and sidebar.

## Submitting, queueing, and steering

| Key | While idle | During a turn |
| --- | --- | --- |
| `Enter` | Start a turn | Admit a FIFO queue item for the next turn |
| `Ctrl+X`, then `Enter` (also `Ctrl+Enter` where supported) | Send the draft, or choose a queued item when empty | Send the draft or selected queue item into the current turn |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Newline | Newline |
| `Escape` | — | Interrupt; a second press confirms |

An item is reported as queued only after SQLite commits it. The oldest entries stay fixed
in a dock directly above the composer, labelled `next` or `steer` in durable FIFO order.
The dock shows the effective `input_force_submit` binding rather than assuming the default
`Ctrl+Enter`, and also shows the queue-manager binding. Open `/queue`, select any row
with Up/Down or a mouse click, then use the displayed Send Now binding or the
**Send selected now** button. Selection sends that row, not the composer draft or
necessarily the first row. Other entries retain their order.

Queued drafts can be edited or cancelled by revision and survive a process restart;
text editing preserves already-admitted image attachments. After a row is accepted as
steering its content is immutable, but it can still be cancelled before consumption.
Promotion moves an entry into transcript history; cancellation never presents it as sent.
Actions bind the displayed row revision. An edit by another client disarms an old
cancellation confirmation rather than cancelling changed content without a new confirmation.

Steering can wake a provider stream or a retry delay: Zuno checkpoints partial assistant
output, promotes the durable input, and starts the next model step in the **same turn**,
without Stop, hard cancellation, or a new session. An executing tool is
not abandoned to steer, so its result reaches the next safe point first. If the turn ends
before a steer is consumed, the admitted item stays pending and is promoted in FIFO order
next turn. If the displayed turn ends or changes **before admission**, Send Now is
refused: an existing queue row keeps its original revision/order, and a new draft is
restored with its paste and image data. A newer composer draft is never overwritten;
the rejected draft is restored when that composer becomes empty. It is not automatically
retried against a different turn. A separate consumption receipt confirms that the
running model input actually received the message.

## Composer history and paste

With the composer focused, Up/Down never scroll the transcript. At either end of the
entire text buffer, both arrows browse submitted input history; at an interior position
they move the caret. Returning past the newest entry restores the complete draft,
including its caret, selection, undo state, paste blocks and image ownership. Dialog
arrows still navigate their own options; use the wheel, Page Up/Down or transcript view
to scroll conversation content.

A multiline paste is one text block. CRLF and CR are normalized to LF, including a
trailing newline; no newline inside the paste submits a prompt. Bracketed paste is
authoritative, and rapid legacy key bursts are grouped before shortcut dispatch.
Submit only with a later explicit Enter or Send Now gesture. Long pastes may display
a compact placeholder, but the complete text is sent.

On local Windows, clipboard reads and writes run asynchronously through PowerShell 7
(`pwsh.exe`) when available, otherwise `powershell.exe`. A paste stays pending until
the whole block arrives; submitting a partial paste is prevented. If its composer,
cursor, or session changes while reading, the result is not inserted into the new target.

## Default keys

`Ctrl+X` is the leader. A leader sequence keeps single characters usable as text.

| Binding | Keys | Purpose |
| --- | --- | --- |
| `leader` | `ctrl+x` | Leader chord |
| `command_list` | `ctrl+p` | Command palette |
| `session_interrupt` | `escape` | Interrupt the turn |
| `session_rename` | `ctrl+r` | Rename the session |
| `session_delete` | `ctrl+d` | Delete the session |
| `session_background` | `ctrl+b` | Send work to the background |
| `session_pin_toggle` | `ctrl+f` | Pin or unpin |
| `session_new` | `<leader>n` | New session |
| `session_list` | `<leader>l` | Session picker |
| `session_timeline` | `<leader>g` | Timeline |
| `session_compact` | `<leader>c` | Compact history |
| `session_export` | `<leader>x` | Export |
| `session_queued_prompts` | `<leader>q` | Queued prompts |
| `sidebar_toggle` | `<leader>b` | Show or hide the sidebar |
| `status_view` | `<leader>s` | Status |
| `theme_list` | `<leader>t` | Theme picker |
| `editor_open` | `<leader>e` | Open the external editor |
| `prompt_skills` | `<leader>k` | Skill picker |
| `mcp_list` | `<leader>p` | MCP servers |
| `display_thinking` | `<leader>i` | Toggle reasoning display |
| `tool_details` | `<leader>o` | Tool detail |
| `diff_open` | `<leader>d` | Diff browser |
| `app_exit` | `ctrl+c`, `ctrl+d`, `<leader>q` | Exit |

Printable input has one owner. While a dialog is open, unmatched text goes only
to that dialog and never to the composer behind it. Terminals may report a key
as Press, Repeat, and Release events; Press inserts once, Repeat preserves
normal key-repeat behavior, and Release never inserts text. A Windows
press/release pair therefore contributes exactly one character to model,
session, Agent, theme, Skill, and other searchable dialogs.

`Ctrl+C` and `Ctrl+D` are confirmed exits. The first press shows
`ctrl+c again to exit` or `ctrl+d again to exit` in the final row; the same chord must be
pressed again within 1.5 seconds. During a turn, the first press also requests a hard
interrupt. A different chord or an expired window starts a new confirmation rather than
exiting accidentally. `Ctrl+]` belongs to the outer background attachment and detaches
without sending either exit chord to the retained TUI.

`leader_timeout` defaults to 5000 milliseconds, so the continuation overlay stays readable
for five seconds unless another key completes or cancels the sequence. Interaction while
it is open restarts the deadline. Rebinding is covered in
[Themes and keybindings](/config/theming).

## Navigating child sessions

Delegation produces real child sessions, and the interface treats each observed native
child as a complete session surface rather than a detail popup.

| Binding | Keys | Movement |
| --- | --- | --- |
| `session_child_first` | `<leader>down` | Enter the first direct child |
| `session_child_cycle` | `<leader>right` | Next sibling |
| `session_child_cycle_reverse` | `<leader>left` | Previous sibling |
| `session_parent` | `<leader>up` | Return to the parent |

Each child keeps its own composer draft. Pressing Enter in a running child queues text in
that child's durable inbox; explicit Send Now steers its displayed active turn.
Pressing Enter after it settles
wakes the same child identity with its resolved agent, model, effort, permissions, and
lineage. Child text is literal, so `/help` typed in a child is sent to the child rather
than executed as a root command.

Root Agents also receive the root-only `session_message` tool. It can send durable peer
context to another root session in the same project or to a descendant of the current
root. A child never receives the sending tool, and runtime validation rejects child,
cross-project, self, archived, or foreign-child targets even if a stale schema were
replayed. Messages are attributed as peer context rather than user authorization. An
active root or child receives them at its next safe point; an idle TUI polls the durable
inbox and starts the target turn; an offline target keeps the queued row until it is
loaded again.

Product-agent invocations and workflow projections are not presented as resumable child
conversations.

## Slash commands

Native session commands resolve before Markdown commands and Skills, so a user workflow
cannot shadow a runtime control.

| Command | Purpose |
| --- | --- |
| `/compact` | Compact history through the durable compaction pipeline |
| `/goal [objective \| action]` | Set, view, or manage the durable session goal; use `/goal help` for syntax |
| `/plan` | Enter Plan mode idempotently |
| `/start-plan` | Enter read-only Plan mode immediately |
| `/start-work` | Authorize the exact handoff-ready Plan revision and start implementation |
| `/questions [list \| open <request-id> \| <request-id>]` | List pending questions or reopen a saved form |
| `/resume` | Explicitly resume paused or completed Work using its saved execution identity |
| `/preset` | Switch the configured model team, or choose one |
| `/council` | Run a native multi-agent Council preset |
| `/undo` | Restore the worktree before the last completed turn |
| `/redo` | Reapply the most recently undone turn |
| `/stop` | Stop one background terminal, or choose one |
| `/new` | Open an empty conversation shell |
| `/subagent` | Inspect seat and node progress |
| `/memory` | Review, edit, approve, reject, remove, and undo durable memory changes |
| `/memories` | Configure memory use and learning generation for the current session |
| `/learn [action]` | Inspect or manage Experience, feedback, patterns, and reviewed Skill candidates |
| `/reflect [turn\|session]` | Run the durable no-tools learning extractor manually |

The direct `/goal <objective>` form is handled by the same durable host command as ACP.
It creates a new goal when none exists or the previous one is complete or cancelled;
otherwise it updates the current goal. Objective changes also reconcile an active durable
Plan by archiving the previous visible Plan and installing a new root bound to the current
`goal_id` for multi-stage work. An atomic objective does not rebind an already terminal
historical Plan; one that belongs to a previous Goal is archived as completed history.
Explicit actions such as `/goal show`, `/goal edit ...`,
`/goal budget <positive tokens|none>`, and `/goal complete` remain available.

Zuno notices — a remote rule file that could not be fetched, a turn stopped by its token,
tool-call, or wall-clock allowance, a compaction the budget policy requested — appear as
toasts whose level follows the notice severity (`info`, `warning`, `error`) and end with the
notice code in brackets. They are not model output.

Resource pickers follow the same naming: `/model`, `/agent`, `/session`, `/skill`,
`/theme`, `/mcp`, `/diff`, `/commands`, `/help`.

Use `/session`, `/sessions`, or `/continue` to choose a saved session. `/resume` resumes
Work in the current session. It does not approve a Plan or bypass a required human or
external wait; an inactive Goal must be resumed explicitly with `/goal resume`.

`/session` reopens the chosen session under *its* saved Agent, model, and effort — the
current session's picks do not follow you — and a `/model`, `/agent`, `/preset`, or
effort pick is written to the current session so the next resume starts from it. A target
session whose Shell this platform would refuse keeps the current session and shows
`warning: keeping the current turn host:` instead of tearing it down.

`/council` appears only when the active agent's final capability snapshot can actually
reach `council_run`, so the picker cannot advertise a run the dispatcher would reject.

### What `/undo` covers

`/undo` and `/redo` move the whole worktree between the two trees Zuno captured around a
turn. The capture is not limited to the directory Zuno was started in, so a session
started in a subdirectory can restore files beside it.

A snapshot does not hold every file. Three kinds of path are left out, and a restore
never changes any of them:

- untracked files larger than 2 MiB;
- paths a `.gitignore` rule covers at the moment the snapshot is taken;
- paths Git could not read.

An excluded path keeps whatever content it already had, so a restore can leave part of
the tree looking untouched even though it succeeded. Zuno counts them in the line a
successful restore prints:

```text
undo complete: 3 file(s) restored to tree 250c08c795d9 (1 created, 1 modified, 1 deleted); 2 path(s) are outside this snapshot and were not restored: 1 over the 2 MiB untracked-file limit, 1 matching an ignore rule
```

If a listed path matters to you, recover it from your own version control or backups. The
snapshot store never held a copy.

A turn that failed or was interrupted still gets its snapshot, because it has usually
already written files.

A restore can also end in an **uncertain outcome**: files were rewritten and the
requested tree could not be confirmed afterwards. Zuno reports that as what it is rather
than as a refusal, writes `zuno-restore-uncertain.json` into the snapshot store, and
refuses every later `/undo` and `/redo` until that record is gone. Nothing is retried
automatically. Read the record, compare it with your worktree, resolve the difference
yourself, and then delete the file to re-enable restores.

## Permission prompts and questions

Tool-owned human input replaces the composer region rather than adding a transcript card.
A permission prompt reports awaiting approval. A required question can pause ordinary
Work or a Goal until a real answer arrives. Optional deferred questions remain available
while the Agent continues independent work and writes its final summary.

Permission choices accept Left and Right, the Up and Down aliases, Enter, and mouse
selection; explicit expansion moves the prompt to a larger overlay. Questions show
`Question i/n`, the remaining unanswered count, numbered choices, and a numbered `Other`
input, with per-question cursors and custom drafts surviving navigation. Cancelling a
permission prompt produces a typed denial; cancelling a question records its cancellation
without fabricating an answer.

Open `/questions` or `/questions list` to see pending request IDs, revisions, confirmed
answer counts, and saved draft counts. Select a row, or enter `/questions open <request-id>`
(`/questions <request-id>` also works), to reopen it. This control remains available
while a turn is running.

### Saving a question draft

`Ctrl+S` saves the current form as a draft and closes it after requesting the save.
The successful save receipt confirms that it is durable. Reopening the session after
a process restart restores those saved values through `/questions`, including custom
text and blank slots.

Drafts and submitted answers are separate. A complete draft remains pending even when
every question has a value. Navigation moves focus without selecting or submitting
anything; an explicit submit confirms the completed form. Previously submitted answers
remain confirmed while unsubmitted edits are shown as drafts. An empty draft slot stays
blank when reopened instead of selecting a default.

Saving a draft or submitting no answers creates no model input and does not resume
a Goal or authorize Work. Draft values stay out of model input and tool receipts.
Real submitted answers enter the durable FIFO once; retrying the same revision-bound
command cannot deliver the answer twice. A stale form is rejected and can be reloaded
with `/questions`.

### Plan approval and cancellation

Plan approval shows the stored Plan revision and Work Agent/model. Choose `approve` or
`decline` explicitly. Approving a Draft review also requires your nonempty risk reason.
Saving an `approve` choice with `Ctrl+S`, leaving a choice highlighted, or submitting
nothing grants no authority. An approval can be saved while the Plan summary continues;
Work starts only after a successful planning handoff and uses the stored Work identity.

`Escape` cancels the question without approving or resuming work and retains the global
turn-interruption behavior. Closing the TUI or disconnecting does not cancel pending
requests; saved drafts remain available when the session is reopened.

## Mouse and scrolling

With `mouse` absent or `true`, Zuno captures button, drag, release, and wheel events.
Releasing a drag copies the selection through the configured clipboard and leaves the
highlight visible. Local Windows prefers a native clipboard write and reports success
only after the helper completes. Remote SSH terminals prefer OSC 52; because that
protocol has no success acknowledgement, the UI says that a copy request was sent.
When the terminal write fails Zuno falls back to
one local helper — `pbcopy` on macOS, `wl-copy`, `xclip`, or `xsel` on Linux, and
`Set-Clipboard` through PowerShell on Windows. A host with no working mechanism reports
the failure instead of appearing to copy.
One serialized clipboard provider survives session switches. A failed newer copy attempt
invalidates older success notifications, so a late receipt cannot claim the current
selection was copied.

Every helper receives the selection on its
standard input as clipboard data, never as a script to run, so a copy can never execute
what the transcript contained. Transcript selection and copying share the last painted
row/grapheme mapping, including Chinese, combining characters and emoji. Visible Markdown
text is copied, not source punctuation. Speaker labels, borders,
padding, and terminal soft wraps are omitted; explicit content newlines become
clipboard newlines. Selection stays clamped rather than crossing into the sidebar,
disclosure rows are clickable, and an overflowing conversation mounts a draggable scrollbar.

Wheel input starts precise: the first notch moves one row, then a sustained fast gesture
accelerates. `scroll_speed` selects a constant multiplier instead;
`scroll_acceleration.enabled` explicitly chooses velocity acceleration and wins when both
are present.

Set `"mouse": false` in `tui.json` to return drag selection to the terminal. Alternate
scroll mode is enabled only while transcript scrolling owns the arrows, not while the
composer or a dialog is focused.

Quitting releases the capture modes it enabled, then discards the input it never read, so a
click or wheel notch that arrived while the session was shutting down cannot reach the shell
as a stray `0;54;31M` report.

## See also

- [Themes and keybindings](/config/theming)
- [Headless runs](/guide/headless)
- [Images and file references](/reference/attachments)
- [zuno tui](/cli/tui)
