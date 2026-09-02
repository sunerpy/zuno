# Bedrock Model Capability Review

Use this Skill before editing a provider model catalog or enabling a provider
feature for an Amazon Bedrock model.

1. Treat every capability as specific to one model id in one region. Tool use,
   streaming, structured output, reasoning, prompt caching, and context limits
   differ between sibling models and between regions of the same model.
2. Do not use a sibling model's documentation, a model family page, a changelog,
   a prior session's memory, or a plausible default as evidence. A sibling
   model's documentation is not evidence about this model.
3. Accept exactly two states as evidence: a vendor document that names this
   exact model id and region, cited by URL or title, or a real probe request
   against this model in this region whose response you observed and whose
   receipt id the tool result printed.
4. Record the claim with `capability_claim` before writing the configuration:
   `documented` with the citation in `sources`, or `probed` with
   `probeReceiptId`. When neither exists, record `inferred` or `unknown` and
   say so; a goal that changes the workspace cannot complete while such a claim
   stands.
5. A write to the workspace retires an earlier probe. Probe again after the
   last change and record the claim again before completing.
6. Report the recorded state with the change. Never describe an inferred
   capability as supported.
