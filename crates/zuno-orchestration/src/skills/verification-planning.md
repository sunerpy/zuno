# Verification Planning

Use this Skill to define evidence before implementation or release work begins.

1. Identify the behavior being changed, its failure modes, and the smallest
   observable acceptance surface.
2. Choose targeted unit, integration, protocol, TUI, or end-to-end checks in
   proportion to risk.
3. Name exact commands, fixtures, expected outputs, and cleanup requirements.
4. Distinguish static inspection, successful build, automated behavior, and real
   runtime acceptance. One does not prove the others.
5. A check is passed only when its complete exit status and decisive output were
   observed. Record blocked checks and their reason explicitly.

This Skill does not grant tools, permissions, filesystem access, network access,
or environment access. Use only capabilities already exposed by the active
Agent profile and permission policy.
