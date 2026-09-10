These are exact Amazon EventStream response bytes captured on 2026-09-10
from small synthetic requests to `us.anthropic.claude-opus-5` in `us-east-2`.
They contain no credentials or user content.

The model filtered the responses. The usage is nevertheless billable and must
survive decoding, failed-turn accounting, and context projection.

| Fixture | Input | Cache read | Cache write | Output | Full total |
| --- | ---: | ---: | ---: | ---: | ---: |
| converse-cold | 10 | 0 | 4209 | 1 | 4220 |
| converse-warm | 10 | 4209 | 0 | 1 | 4220 |
| invoke-cold | 10 | 0 | 4210 | 1 | 4221 |
| invoke-warm | 10 | 4210 | 0 | 1 | 4221 |

Converse reports disjoint prompt buckets. Invoke reports usage in both
`message_start` and `message_delta`; these are snapshots of one request, not
independent charges. An explicit `output_tokens_details.thinking_tokens: 0`
is distinct from an absent breakdown.

The `fable-*` fixtures use the same API paths with
`us.anthropic.claude-fable-5-1` and a synthetic garden notebook. All four return
normal text and `end_turn`.

| Fixture | Input | Cache read | Cache write | Output | Full total |
| --- | ---: | ---: | ---: | ---: | ---: |
| fable-converse-cold | 22 | 0 | 4987 | 38 | 5047 |
| fable-converse-warm | 22 | 4987 | 0 | 48 | 5057 |
| fable-invoke-cold | 22 | 0 | 4988 | 39 | 5049 |
| fable-invoke-warm | 22 | 4988 | 0 | 36 | 5046 |
