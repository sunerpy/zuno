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
deferred to the turn boundary.

Transient "working" rows are not inserted into the transcript. Durable activity, errors,
interruption markers, and assistant content are.

Context occupancy is the most recent complete provider prompt divided by the catalog
context limit. It is replaced on each provider report rather than accumulated across the
session; cumulative token buckets live in the usage projection and sidebar.

## Submitting, queueing, and steering

| Key | While idle | During a turn |
| --- | --- | --- |
| `Enter` | Start a turn | Admit a FIFO queue item for the next turn |
| `Ctrl+Enter` | — | Steer: soft interrupt at the nearest safe step boundary |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Newline | Newline |
| `Escape` | — | Interrupt; a second press confirms |

An item is reported as queued only after SQLite commits it. The oldest entries stay fixed
in a dock directly above the composer, labelled `next` or `steer` in durable FIFO order.
The dock shows the effective `input_force_submit` binding rather than assuming the default
`Ctrl+Enter`, and also shows the queue-manager binding. Pending items can be edited or
cancelled by revision and survive a process restart. Promotion moves an entry into
transcript history; cancellation removes it without presenting it as sent.

Steering can wake a provider stream or a retry delay: Zuno checkpoints partial assistant
output, promotes the durable input, and starts the next model step. An executing tool is
not abandoned to steer, so its result reaches the next safe point first. If the turn ends
before a steer is consumed, the admitted item stays pending and is promoted in FIFO order
next turn. It is never lost or duplicated.

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

Each child keeps its own composer draft. Pressing Enter in a running child admits text to
that child's durable inbox and steers its active turn; pressing Enter after it settles
wakes the same child identity with its resolved agent, model, effort, permissions, and
lineage. Child text is literal, so `/help` typed in a child is sent to the child rather
than executed as a root command.

Product-agent invocations and workflow projections are not presented as resumable child
conversations.

## Slash commands

Native session commands resolve before Markdown commands and Skills, so a user workflow
cannot shadow a runtime control.

| Command | Purpose |
| --- | --- |
| `/compact` | Compact history through the durable compaction pipeline |
| `/goal [objective \| action]` | Set, view, or manage the durable session goal; use `/goal help` for syntax |
| `/plan` | Enter Plan mode, or confirm starting work when already planning |
| `/start-plan` | Enter read-only Plan mode immediately |
| `/start-work` | Review the durable plan and confirm implementation |
| `/preset` | Switch the configured model team, or choose one |
| `/council` | Run a native multi-agent Council preset |
| `/undo` | Restore the worktree before the last completed turn |
| `/redo` | Reapply the most recently undone turn |
| `/stop` | Stop one background terminal, or choose one |
| `/new` | Open an empty conversation shell |
| `/subagent` | Inspect seat and node progress |
| `/memory` | Review, edit, approve, reject, remove, and undo durable memory changes |
| `/learn [action]` | Inspect or manage Experience, feedback, patterns, and reviewed Skill candidates |
| `/reflect [turn\|session]` | Run the durable no-tools learning extractor manually |

The direct `/goal <objective>` form is handled by the same durable host command as ACP.
It creates a new goal when none exists or the previous one is complete or cancelled;
otherwise it updates the current goal. Objective changes also reconcile an active durable
Plan by archiving the previous visible Plan and installing a new root bound to the current
`goal_id` for multi-stage work. An atomic objective does not rebind an already terminal
historical Plan; one that belongs to a previous Goal is archived as completed history. Explicit actions such as `/goal show`, `/goal edit ...`,
and `/goal complete` remain available.

Zuno notices — a remote rule file that could not be fetched, a turn stopped by its token,
tool-call, or wall-clock allowance, a compaction the budget policy requested — appear as
toasts whose level follows the notice severity (`info`, `warning`, `error`) and end with the
notice code in brackets. They are not model output.

Resource pickers follow the same naming: `/model`, `/agent`, `/session`, `/skill`,
`/theme`, `/mcp`, `/diff`, `/commands`, `/help`.

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
A permission prompt reports awaiting approval; a Plan structured question reports awaiting
answer. Ordinary Work does not park this interaction and asks directly at a turn boundary
only when no safe default exists.

Permission choices accept Left and Right, the Up and Down aliases, Enter, and mouse
selection; explicit expansion moves the prompt to a larger overlay. Questions show
`Question i/n`, the remaining unanswered count, numbered choices, and a numbered `Other`
input, with per-question cursors and custom drafts surviving navigation. Cancelling either
resolves the tool as a typed denial and never fabricates an answer.

## Mouse and scrolling

With `mouse` absent or `true`, Zuno captures button, drag, release, and wheel events.
Releasing a drag copies the selection through the configured clipboard and leaves the
highlight visible. Transcript copy uses semantic message content: speaker labels, borders,
padding, and terminal soft wraps are omitted; only explicit source newlines become
clipboard newlines. Selection stays clamped rather than crossing into the sidebar,
disclosure rows are clickable, and an overflowing conversation mounts a draggable scrollbar.

Wheel input starts precise: the first notch moves one row, then a sustained fast gesture
accelerates. `scroll_speed` selects a constant multiplier instead;
`scroll_acceleration.enabled` explicitly chooses velocity acceleration and wins when both
are present.

Set `"mouse": false` in `tui.json` to return drag selection to the terminal.

Quitting releases the capture modes it enabled, then discards the input it never read, so a
click or wheel notch that arrived while the session was shutting down cannot reach the shell
as a stray `0;54;31M` report.

## See also

- [Themes and keybindings](/config/theming)
- [Headless runs](/guide/headless)
- [Images and file references](/reference/attachments)
- [zuno tui](/cli/tui)
