//! One native, tool-disabled discussion turn. It does not enter Work, run
//! preludes, load requested skills, reconcile Plans, or restart a paused Goal.

use super::*;

struct DiscussionBudget {
    control: zuno_session_control::SessionControlService,
    admission: zuno_session_control::DiscussionAdmission,
    inner: zuno_goal::GoalBudgetPolicy,
}

#[async_trait]
impl zuno_engine::budget::TurnBudgetPolicy for DiscussionBudget {
    async fn before_request(
        &self,
        snapshot: &zuno_engine::budget::TurnUsageSnapshot<'_>,
    ) -> Result<zuno_engine::budget::BudgetDecision, zuno_engine::budget::BudgetPolicyError> {
        let allowed = self
            .control
            .discussion_is_current(&self.admission, snapshot.turn_id)
            .map_err(|error| match error {
                zuno_session_control::SessionControlError::Database(error) => {
                    zuno_engine::budget::BudgetPolicyError::Database(error)
                }
                zuno_session_control::SessionControlError::Goal(zuno_goal::GoalError::Db(
                    error,
                )) => zuno_engine::budget::BudgetPolicyError::Database(error),
                error => zuno_engine::budget::BudgetPolicyError::Permanent(error.to_string()),
            })?;
        if !allowed {
            return Ok(
                zuno_engine::budget::BudgetDecision::stop_uncertain_side_effect(
                    "The discussion's native authorization changed; no further model request is allowed.",
                ),
            );
        }
        self.inner.before_request(snapshot).await
    }

    async fn after_response(
        &self,
        snapshot: &zuno_engine::budget::TurnUsageSnapshot<'_>,
    ) -> Result<zuno_engine::budget::BudgetDecision, zuno_engine::budget::BudgetPolicyError> {
        self.inner.after_response(snapshot).await
    }
}

impl TurnHost {
    pub(crate) fn pending_discussion_input(&self) -> Result<Option<String>, String> {
        self.session_control
            .pending_discussion(&self.session_id)
            .map(|candidate| candidate.map(|candidate| candidate.input_id))
            .map_err(to_string)
    }

    /// Also used on cold ACP load for a previously saved, never-applied user
    /// question. The native claim is exact and atomic; no text is re-admitted.
    pub(crate) async fn drive_pending_discussion_with_guard(
        &mut self,
        guard: &SessionRunGuard,
        events: TurnEventSender,
    ) -> Result<bool, String> {
        let Some(admission) = self
            .session_control
            .pending_discussion(&self.session_id)
            .map_err(to_string)?
        else {
            return Ok(false);
        };
        let Some(_active_input) = guard.mark_input_started(&admission.input_id) else {
            return Ok(false);
        };
        let turn_id = Uuid::now_v7().simple().to_string();
        if !self
            .session_control
            .claim_discussion(&admission, &turn_id, zuno_db::message::now_millis())
            .map_err(to_string)?
        {
            return Ok(false);
        }
        self.last_turn_completed = false;
        let receipts = input_receipts::ReceiptCycle::from_bound_turn(
            self.database.clone(),
            &self.session_id,
            &turn_id,
        );
        let result = async {
            events.publish(TurnEvent::Notice {
                audience: NoticeAudience::User,
                severity: NoticeSeverity::Info,
                code: "discussion.tools_disabled".to_owned(),
                detail: "Discussion only: all tools are disabled. The previous Goal and uncertain effects remain paused; this answer does not resume or verify them.".to_owned(),
            }).await.map_err(TurnFailure::event_consumer)?;
            let mut resolver = self.resolver.clone();
            resolver.append_prompt_section(
                "runtime.discussion",
                format!("native:discussion/{}@{}", admission.input_id, admission.execution_revision),
                "Answer only the latest user's question as text, using the recorded context. ALL tool execution is disabled by the host. Do not continue the earlier task, resume its Goal, claim to have inspected external state, or claim to have changed files or designs. You may explain and propose changes. If executing the request requires tools, explain that limitation and the outstanding uncertainty. Prior work remains paused.".to_owned(),
            ).map_err(TurnFailure::host)?;
            let context = TurnContext::new(
                &mut self.connection, &self.providers, &resolver, &self.dispatcher,
                guard.interrupt_signal(),
            )
            .with_tools_disabled()
            .with_attachments(self.attachments.clone())
            .with_run_registry(self.runs.clone())
            .with_budget_policy(Arc::new(DiscussionBudget {
                control: self.session_control.clone(),
                admission: admission.clone(),
                inner: zuno_goal::GoalBudgetPolicy::new(self.goal_store.clone())
                    .with_allowance(self.turn_allowance),
            }));
            let request = RunTurnRequest::new(
                self.session_id.clone(), turn_id, DynamicContext::default(),
            ).with_start(TurnStart::UserMessage).with_context_limit(self.window.context);
            self.driver.drive(request, context, events).await
                .map(Some).map_err(TurnFailure::Engine)
        }.await;
        receipts
            .finish(&result, self.credential.as_deref())
            .map_err(|error| error.rendered(self.credential.as_deref()))?;
        // Deliberately no goal usage mutation, learning extraction, Plan
        // reconciliation, automatic wake, or persistent-gate rewrite here.
        result
            .map(|_| true)
            .map_err(|error| error.rendered(self.credential.as_deref()))
    }
}
