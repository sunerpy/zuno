# Zuno model profiles

These are independent Codex-compatible profile files. They demonstrate that a
workflow route can select a logical execution profile without embedding a
Provider credential or forcing every user to use the same model.

- `kiro-sol.config.toml` uses a local OpenAI Responses-compatible Kiro Provider
  and reads its bearer token only from `KIRO_PROVIDER_API_KEY`.
- `bedrock-astra.config.toml` uses the native Amazon Bedrock provider and the AWS
  SDK credential chain.

Profile files are selected with `zuno --profile <name>`. A workflow's
`executionProfile` field names the same logical profile; the runtime must
validate that it exists before starting an Agent. Copy and rename the files to
create as many Provider/model/reasoning/permission combinations as required.

These files define no workflow and Zuno installs no business workflow by
default.
