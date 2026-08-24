# Customize Zuno

Use this Skill when the user asks to inspect or change Zuno configuration,
providers, authentication, permissions, Agents, workflows, Skills, MCP servers,
or extension manifests.

1. Inspect the matching checkout's generated schema and current configuration
   before proposing an edit. Zuno rejects unknown or malformed fields.
2. Resolve scope explicitly: project configuration is `zuno.json` or
   `zuno.jsonc`; project Agents live under `.zuno/agent/`; global configuration
   belongs under the resolved Zuno config directory. Never read OpenCode paths as
   an implicit fallback.
3. Prefer native provider configuration. For an OpenAI-compatible custom provider
   such as `myopenai`, inspect its declared `transport`, endpoint surface, model,
   and credential source. `transport: "openai"` selects a wire protocol; it does
   not grant OpenAI OAuth or choose an authentication method.
4. Keep credentials out of repository files. Use `zuno auth login myopenai` for
   interactive setup, or pipe an API key to
   `zuno providers login --provider myopenai` for noninteractive setup. A
   provider-declared environment variable or the protected Zuno credential store
   may also supply the credential according to the provider's supported methods.
5. Validate a provider change with `zuno debug config`,
   `zuno auth list`, and `zuno models myopenai --verbose`. Do not infer support
   from another agent product's configuration or from a catalog-only entry.
6. Preserve project and global scope. State whether a restart is required before
   claiming a disk configuration change is active.
7. Keep process-local extensions distinct from static extension manifests and
   never imply that a Skill owns the runtime lifecycle.

This Skill does not grant tools, permissions, filesystem access, network access,
or environment access. Use only capabilities already exposed by the active
Agent profile and permission policy.
