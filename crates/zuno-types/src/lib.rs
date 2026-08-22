//! Wire and domain types shared across the workspace (sessions, messages, parts, tool payloads).

/// One class of routine work a client may compact in its main timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityKind {
    Command,
    Read,
    Search,
    Delegation,
    Image,
    Tool,
}

/// Frontend-neutral counts for one model step's routine activity.
///
/// Clients decide how to render these counts. The projection deliberately carries no
/// terminal glyphs, key names, or localized copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityProjection {
    pub commands: usize,
    pub reads: usize,
    pub searches: usize,
    pub delegations: usize,
    pub images: usize,
    pub tools: usize,
}

impl ActivityProjection {
    pub fn record(&mut self, kind: ActivityKind) {
        let slot = match kind {
            ActivityKind::Command => &mut self.commands,
            ActivityKind::Read => &mut self.reads,
            ActivityKind::Search => &mut self.searches,
            ActivityKind::Delegation => &mut self.delegations,
            ActivityKind::Image => &mut self.images,
            ActivityKind::Tool => &mut self.tools,
        };
        *slot = slot.saturating_add(1);
    }

    #[must_use]
    pub const fn total(self) -> usize {
        self.commands
            .saturating_add(self.reads)
            .saturating_add(self.searches)
            .saturating_add(self.delegations)
            .saturating_add(self.images)
            .saturating_add(self.tools)
    }
}
