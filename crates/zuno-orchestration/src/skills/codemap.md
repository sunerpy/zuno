# Code Map

Use this Skill to build a small structural map of an indexed codebase before
planning or editing.

1. Check whether the repository has a current CodeGraph index. Prefer CodeGraph
   symbol, caller, callee, and impact queries for indexed source.
2. Use native read, glob, and grep only for unindexed files, configuration,
   Markdown, generated data, or a stale-index exception.
3. Return the entry points, core types, call path, persistence boundary, tests,
   and likely blast radius relevant to the request.
4. Separate observed source facts from conclusions. Include exact file paths and
   symbol names.
5. Do not edit files, regenerate the index, or rewrite instruction files unless
   the user separately requests that work.
