# Verification Planning

Use this Skill to define evidence before implementation or release work begins.

1. Identify the behavior being changed, its failure modes, and the smallest
   observable acceptance surface.
2. Choose unit, integration, protocol, client, recovery, or end-to-end checks in
   proportion to the realistic risk.
3. Name exact commands, fixtures, decisive expected observations, cleanup, and
   any external prerequisites.
4. Separate static inspection, compilation, automated behavior, and real runtime
   acceptance; state which layer each check can and cannot prove.
5. Return an acceptance plan, not a claim that unexecuted checks passed.
