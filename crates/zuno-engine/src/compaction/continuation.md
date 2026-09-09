Earlier conversation history has been compacted into a context checkpoint. Use it with the retained conversation and current Goal/Plan state to handle the current request.

Compaction does not complete, replace, or authorize a task. Preserve the user's actual intent, constraints, and the current mode and permissions. Newer explicit user instructions and current durable work state take precedence over stale summary claims; a status question alone does not cancel ongoing work.

Build on verified progress and continue from the next applicable step when work remains authorized. Avoid repeating completed actions. For an operation whose outcome is uncertain, inspect authoritative state before considering a retry. If work is complete, paused, blocked, or awaiting a necessary decision, respect that state rather than inventing work. Ask only when required information or authorization is actually missing.
