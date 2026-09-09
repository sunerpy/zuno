Create a context checkpoint for the assistant that will handle the next request. Summarize the supplied history; do not answer its questions, perform its actions, or invent an objective.

Preserve the user's actual task and its boundaries. Work may involve implementation, debugging, research, explanation, review, planning, or another activity. Distinguish requested work, authorized actions, proposals, and actions that still require a user decision. A summary does not grant permission or turn a read-only task into an implementation task.

When updating an earlier checkpoint:
- Carry forward still-relevant objectives, constraints, decisions, unresolved questions, and parallel work, even when newer messages do not repeat them.
- Apply explicit corrections and changes of scope from newer messages. A status question or clarification does not by itself replace the ongoing task.
- Move verified results to Completed, update resolved blockers, and remove superseded claims. Keep enough evidence to avoid repeating work.
- Distinguish observed results from hypotheses and intended actions. Preserve uncertain side effects and pending external work; never report them as completed or safe to repeat.

Output exactly the following Markdown sections, in this order, without the template tags. Keep empty sections with "(none)".
<template>
## Objective
- [current user intent, requested deliverable, and any explicitly unfinished parallel work]

## Constraints and Decisions
- [user instructions, preferences, current mode, permission boundaries, decisions and their reasons]

## Work State
### Completed
- [verified findings, completed actions, and validation evidence]

### Active
- [current progress, partial work, hypotheses, running operations, and uncertain outcomes]

### Blocked
- [missing information, unresolved failures, pending decisions, or external dependencies]

## Next Steps
1. [next concrete action consistent with current scope, or the reason work is waiting or finished]

## References
- [exact paths, symbols, commands, errors, URLs, artifact identifiers, and why they matter]
</template>

Use concise bullets and the conversation's language. Preserve exact identifiers needed to resume. Do not invent commands, evidence, approvals, or remaining work. Do not repeat large tool outputs or the instructions for creating this checkpoint.
