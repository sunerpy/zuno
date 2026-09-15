# Optional user workflow templates

Files in this directory are documentation templates only. They are not packaged
into Zuno's workflow discovery roots and are never selected automatically.

To opt in, copy and edit a template under one explicit source root:

- user: `$ZUNO_HOME/workflows/`
- project: `<project>/.zuno/workflows/`
- plugin: a directory declared by that plugin's `workflows` manifest field

`design-review.yaml` deliberately names logical execution profiles rather than
models or providers. Create `design-agent.config.toml`,
`review-agent.config.toml`, and `build-agent.config.toml` under `$ZUNO_HOME`, or
rename the `executionProfile` values to profiles you already use. Those profiles
own provider, model, reasoning effort, service tier, approval, and sandbox
settings. The workflow owns only the user-authored graph and prompts.

For example, a user may map `design-agent` to Claude Code and a chosen Claude
model, while mapping `review-agent` and `build-agent` to native Codex children
using Kiro Provider or Amazon Bedrock. Nothing in the Zuno binary requires that
mapping.
