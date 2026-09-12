//! Bind a screen's Stop to the native target present at event dispatch.
//!
//! SessionScreen's existing sink carries the user's reason, not an execution
//! identity. Keep that sink inside this component: capture before dispatch and
//! drain before returning, so a raw cancellation never waits for another event
//! or an async worker to decide which turn the user meant. Firing the native
//! signal is synchronous; provider/tool shutdown remains owned by the driver.

use tokio::sync::mpsc;
use zuno_engine::interrupt::HardInterruptRequest;
use zuno_engine::status::SessionControl;
use zuno_tui::app::{AppEvent, Component, EventResult};
use zuno_tui::crossterm::event::KeyEvent;
use zuno_tui::keybind::{ActionComponent, Definition, PendingPrefix};
use zuno_tui::ratatui::Frame;
use zuno_tui::ratatui::layout::Rect;

/// Mounted outside the permission/dialog tree, inside the key dispatcher.
pub(super) struct CancellationBridge {
    inner: Box<dyn ActionComponent>,
    control: SessionControl,
    requests: mpsc::Receiver<HardInterruptRequest>,
}

impl CancellationBridge {
    pub(super) fn new(
        inner: Box<dyn ActionComponent>,
        control: SessionControl,
        requests: mpsc::Receiver<HardInterruptRequest>,
    ) -> Self {
        Self {
            inner,
            control,
            requests,
        }
    }

    fn dispatch(
        &mut self,
        handle: impl FnOnce(&mut dyn ActionComponent) -> EventResult,
    ) -> EventResult {
        // Only requests emitted by this dispatch have an identifiable target.
        // Never reuse an orphaned request left by an earlier event or mount.
        while self.requests.try_recv().is_ok() {
            tracing::warn!(
                target: "zuno::tui::cancellation",
                session_id = %self.control.session_id(),
                "discarded a cancellation without an event target"
            );
        }
        let target = self.control.cancel_target();
        let result = handle(self.inner.as_mut());
        while let Ok(request) = self.requests.try_recv() {
            let applied = target
                .as_ref()
                .is_some_and(|target| self.control.abort_target(target, request));
            tracing::info!(
                target: "zuno::tui::cancellation",
                session_id = %self.control.session_id(),
                applied,
                "TUI cancellation checked against its event target"
            );
        }
        result
    }
}

impl Component for CancellationBridge {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inner.render(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        self.dispatch(|inner| inner.handle_event(event))
    }

    fn alternate_scroll(&self) -> bool {
        self.inner.alternate_scroll()
    }
}

impl ActionComponent for CancellationBridge {
    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult {
        self.dispatch(|inner| inner.handle_action(action, event))
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        self.inner.focused_scopes()
    }

    fn pending_changed(&mut self, pending: &PendingPrefix) -> EventResult {
        self.dispatch(|inner| inner.pending_changed(pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use zuno_engine::interrupt::{HardInterruptReason, HardInterruptSource};
    use zuno_engine::r#loop::TurnEvent;
    use zuno_engine::status::SessionRunRegistry;
    use zuno_tui::app::{TerminalEvent, terminal_event_channel};
    use zuno_tui::crossterm::event::{KeyCode, KeyModifiers};
    use zuno_tui::views::ViewContext;
    use zuno_tui::views::session::SessionScreen;

    fn stop() -> HardInterruptRequest {
        HardInterruptRequest::new(HardInterruptSource::Tui, HardInterruptReason::UserCancel)
    }

    struct StopEmitter {
        requests: mpsc::Sender<HardInterruptRequest>,
        during_dispatch: Option<Box<dyn FnOnce() + Send>>,
    }

    impl Component for StopEmitter {
        fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {}

        fn handle_event(&mut self, event: &AppEvent) -> EventResult {
            if matches!(event, AppEvent::Terminal(TerminalEvent::Wake)) {
                return EventResult::IGNORED;
            }
            if let Some(handoff) = self.during_dispatch.take() {
                handoff();
            }
            self.requests.try_send(stop()).expect("emit Stop");
            EventResult::REDRAW
        }
    }

    impl ActionComponent for StopEmitter {
        fn handle_action(
            &mut self,
            _action: &'static Definition,
            _event: &KeyEvent,
        ) -> EventResult {
            self.handle_event(&AppEvent::Terminal(TerminalEvent::Shutdown))
        }
    }

    fn press_stop(bridge: &mut CancellationBridge) {
        bridge.handle_action(
            zuno_tui::keybind::definition("session_interrupt").expect("Stop action"),
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
    }

    #[test]
    fn tui_stop_captured_for_t1_cannot_cancel_t2_during_dispatch() {
        let registry = SessionRunRegistry::new();
        let first = registry.begin_turn("s").expect("T1 lease");
        let first_id = first.mark_turn_started("t1").expect("T1 identity");
        let second = Arc::new(Mutex::new(None));
        let handoff = Arc::clone(&second);
        let runs = registry.clone();
        let (requests, receiver) = mpsc::channel(1);
        let emitter = StopEmitter {
            requests,
            during_dispatch: Some(Box::new(move || {
                drop(first_id);
                drop(first);
                let next = runs.begin_turn("s").expect("T2 lease");
                let identity = next.mark_turn_started("t2").expect("T2 identity");
                *handoff.lock().expect("handoff") = Some((next, identity));
            })),
        };
        let mut bridge =
            CancellationBridge::new(Box::new(emitter), registry.control("s"), receiver);
        press_stop(&mut bridge);
        assert!(
            !second
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .interrupt_signal()
                .is_set()
        );
        // A later non-Stop event cannot replay the stale cancellation.
        bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
        assert!(
            !second
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .interrupt_signal()
                .is_set()
        );
        // A new Stop observes T2 independently, not the old T1 target.
        press_stop(&mut bridge);
        assert_eq!(
            second
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .interrupt_request(),
            Some(stop())
        );
    }

    #[test]
    fn tui_stop_captured_for_an_input_cannot_follow_a_new_input_on_the_same_lease() {
        let registry = SessionRunRegistry::new();
        let lease = Arc::new(registry.begin_turn("s").expect("lease"));
        let first = lease.mark_input_started("input1").expect("input1");
        let next_input = Arc::new(Mutex::new(None));
        let next_binding = Arc::clone(&next_input);
        let shared_lease = Arc::clone(&lease);
        let (requests, receiver) = mpsc::channel(1);
        let emitter = StopEmitter {
            requests,
            during_dispatch: Some(Box::new(move || {
                drop(first);
                *next_binding.lock().unwrap() = shared_lease.mark_input_started("input2");
            })),
        };
        let mut bridge =
            CancellationBridge::new(Box::new(emitter), registry.control("s"), receiver);
        press_stop(&mut bridge);
        assert!(!lease.interrupt_signal().is_set());
        press_stop(&mut bridge);
        assert_eq!(lease.interrupt_request(), Some(stop()));
    }

    #[test]
    fn queued_t1_stop_without_an_event_target_is_not_reinterpreted_as_t2_stop() {
        let registry = SessionRunRegistry::new();
        let first = registry.begin_turn("s").expect("T1");
        let (requests, receiver) = mpsc::channel(1);
        requests.try_send(stop()).expect("queue T1 Stop");
        drop(first);
        let second = registry.begin_turn("s").expect("T2");
        let mut bridge = CancellationBridge::new(
            Box::new(StopEmitter {
                requests,
                during_dispatch: None,
            }),
            registry.control("s"),
            receiver,
        );
        bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
        assert!(!second.interrupt_signal().is_set());
        assert!(bridge.requests.is_empty());
    }

    #[test]
    fn idle_stop_never_arms_the_next_turn() {
        let registry = SessionRunRegistry::new();
        let (requests, receiver) = mpsc::channel(1);
        let mut bridge = CancellationBridge::new(
            Box::new(StopEmitter {
                requests,
                during_dispatch: None,
            }),
            registry.control("s"),
            receiver,
        );
        press_stop(&mut bridge);
        let next = registry.begin_turn("s").expect("new turn");
        bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
        assert!(!next.interrupt_signal().is_set());
    }

    #[test]
    fn real_session_screen_stop_confirmation_uses_the_native_target() {
        let registry = SessionRunRegistry::new();
        let lease = registry.begin_turn("s").expect("lease");
        let _identity = lease.mark_turn_started("t1").expect("turn identity");
        let (terminal, _events) = terminal_event_channel();
        let (requests, receiver) = mpsc::channel(1);
        let screen =
            SessionScreen::new(ViewContext::defaults(), terminal).with_cancel_sink(requests);
        let mut bridge = CancellationBridge::new(Box::new(screen), registry.control("s"), receiver);
        bridge.handle_event(&AppEvent::Engine(TurnEvent::TurnStarted {
            session_id: "s".to_owned(),
            turn_id: "t1".to_owned(),
        }));
        press_stop(&mut bridge);
        assert!(
            !lease.interrupt_signal().is_set(),
            "first Escape only confirms intent"
        );
        press_stop(&mut bridge);
        assert_eq!(lease.interrupt_request(), Some(stop()));
        assert!(bridge.requests.is_empty());
    }
}
