//! The typed public facts are authoritative. Standard ACP updates provide a
//! compatible view; extension clients retain replacement/removal semantics.
use super::*;
use zuno_types::activity::*;

pub(super) async fn publish(
    client: &ClientConnection,
    session: &SessionId,
    frame: &CommittedFrame,
    snapshot: bool,
) -> Result<(), RpcError> {
    if !snapshot {
        client
            .notify(
                "_zuno/activity",
                serde_json::to_value(frame).map_err(|_| invalid_rpc())?,
            )
            .await?;
    }
    let CommittedEvent::Upsert { position, record } = &frame.event else {
        return Ok(());
    };
    // Message text parts are immutable once inserted; a metadata-only message
    // update must not duplicate the user's already rendered text.
    let first = snapshot || *position == frame.sequence;
    let mut update = match &record.item {
        SessionItem::Message { role, content, .. } if first && !content.is_empty() => {
            let text = content.iter().map(text).collect::<Vec<_>>().join("\n");
            json!({"sessionUpdate":if *role==MessageRole::User{"user_message_chunk"}else{"agent_message_chunk"},
                "content":{"type":"text","text":text}})
        }
        SessionItem::Thinking { text, .. } if first => {
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":text}})
        }
        SessionItem::Invocation { invocation } => {
            let status = match invocation.state {
                InvocationState::Queued | InvocationState::Waiting => "pending",
                InvocationState::Running => "in_progress",
                InvocationState::Succeeded => "completed",
                InvocationState::Failed
                | InvocationState::Denied
                | InvocationState::Cancelled
                | InvocationState::Uncertain => "failed",
            };
            let kind = match invocation.presentation.action {
                InvocationAction::FileRead | InvocationAction::MemoryRead => "read",
                InvocationAction::FileList
                | InvocationAction::FileSearch
                | InvocationAction::WebSearch => "search",
                InvocationAction::FileEdit | InvocationAction::MemoryWrite => "edit",
                InvocationAction::Process => "execute",
                InvocationAction::WebFetch => "fetch",
                _ => "other",
            };
            json!({"sessionUpdate":if first{"tool_call"}else{"tool_call_update"},
                "toolCallId":invocation.id,"title":invocation.name,"kind":kind,"status":status,
                "rawInput":invocation.input,
                "content":invocation.content.iter().map(|c|json!({"type":"content","content":{"type":"text","text":text(c)}})).collect::<Vec<_>>()})
        }
        SessionItem::Plan { steps } => {
            json!({"sessionUpdate":"plan","entries":steps.iter().filter(|step|step.status!=zuno_types::PlanStepStatus::Superseded).map(|step|json!({
            "content":step.text,"priority":"medium","status":match step.status {
                zuno_types::PlanStepStatus::Pending=>"pending",
                zuno_types::PlanStepStatus::InProgress=>"in_progress",
                zuno_types::PlanStepStatus::Completed|zuno_types::PlanStepStatus::Superseded=>"completed",
            }})).collect::<Vec<_>>()})
        }
        _ => return Ok(()),
    };
    update["_meta"] =
        json!({"zuno":{"activityId":record.id,"sequence":frame.sequence,"actions":record.actions}});
    client.session_update(session.as_str(), update).await
}
pub(super) fn text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text, .. } | ContentBlock::Terminal { text, .. } => text.clone(),
        ContentBlock::Code { code, .. } => code.clone(),
        ContentBlock::Diff { diff, .. } => diff.clone(),
        ContentBlock::Structured { value } => value.to_string(),
        ContentBlock::Image { resource, alt } => format!("{alt} [{}]", resource.name),
        ContentBlock::Resource { resource } => resource.name.clone(),
    }
}
