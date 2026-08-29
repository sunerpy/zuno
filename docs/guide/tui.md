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

An item is reported as queued only after SQLite commits it. Pending items can be edited or
cancelled by revision and survive a process restart.

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
| `/goal` | View or manage the durable session goal |
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

Resource pickers follow the same naming: `/model`, `/agent`, `/session`, `/skill`,
`/theme`, `/mcp`, `/diff`, `/commands`, `/help`.

`/council` appears only when the active agent's final capability snapshot can actually
reach `council_run`, so the picker cannot advertise a run the dispatcher would reject.

## Permission prompts and questions

Tool-owned human input replaces the composer region rather than adding a transcript card.
A permission prompt reports awaiting approval; a structured question reports awaiting
answer.

Permission choices accept Left and Right, the Up and Down aliases, Enter, and mouse
selection; explicit expansion moves the prompt to a larger overlay. Questions show
`Question i/n`, the remaining unanswered count, numbered choices, and a numbered `Other`
input, with per-question cursors and custom drafts surviving navigation. Cancelling either
resolves the tool as a typed denial and never fabricates an answer.

## Mouse and scrolling

With `mouse` absent or `true`, Zuno captures button, drag, release, and wheel events.
Releasing a drag copies the selection through the configured clipboard and leaves the
highlight visible. Transcript selection stays clamped rather than crossing into the
sidebar, disclosure rows are clickable, and an overflowing conversation mounts a draggable
scrollbar.

Wheel input starts precise: the first notch moves one row, then a sustained fast gesture
accelerates. `scroll_speed` selects a constant multiplier instead;
`scroll_acceleration.enabled` explicitly chooses velocity acceleration and wins when both
are present.

Set `"mouse": false` in `tui.json` to return drag selection to the terminal.

## See also

- [Themes and keybindings](/config/theming)
- [Headless runs](/guide/headless)
- [Images and file references](/reference/attachments)
- [zuno tui](/cli/tui)
