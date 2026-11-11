//! The connect work of one MQTT session as an action machine (ADR 023 rung 3).
//!
//! After [`SessionAction::ConnectWork`](super::SessionAction::ConnectWork) a
//! tier has to perform the steps of a [`ConnectWorkPlan`] in order, stop at
//! the first failed subscribe or startup publish, run its closing step, and
//! only then report [`SessionInput::WorkDone`] (or `WorkFailed`) to the
//! [`SessionMachine`](super::SessionMachine). [`ConnectWorkMachine`] holds
//! that order; the tier performs one [`WorkAction`] at a time and feeds back
//! the [`WorkInput`] it produced.
//!
//! # Rung
//!
//! Rung 3, because both ADR 023 decision 2 triggers hold: the same order runs
//! under two execution models — on ESP-IDF the `mqtt-net` runner thread
//! performs one step per loop turn with session inputs (`NetworkDown`,
//! `Disconnected`, `Shutdown`) interleaved between steps, on esp-hal the
//! `serve` future performs one step per await under cancel-safe async — and
//! the steps are esp-mqtt / minimq calls outside `embedded-hal`, so the tiers
//! must share the order without sharing the I/O. The hal adoption is what
//! justifies the machine; with one tier only this would be a rung-1 function.
//! No clock is read here (decision 4): a slow step is the tier's to time.
//!
//! # Lifecycle
//!
//! 1. [`ConnectWorkMachine::new`] with the session's [`SessionToken`], its
//!    [`ConnectWorkPlan`] and the broker's session-present flag.
//! 2. The first [`poll`](ConnectWorkMachine::poll) takes [`WorkInput::Start`]
//!    and answers the first [`WorkAction::Step`], or [`WorkAction::Finish`] when
//!    the plan is empty; a `Report` is never the first answer.
//! 3. Each `Step` is one driver call; the tier feeds [`WorkInput::StepDone`] or
//!    [`WorkInput::StepFailed`]. A failed subscribe or startup publish ends the
//!    work with `Report(WorkFailed)`; a failed connect callback does not.
//! 4. `Finish` is the tier's closing step, emitted exactly once, also for an
//!    empty plan: the ESP-IDF tier sends every runtime subscription still owed
//!    and marks its tracker live, the esp-hal tier drains its SUBACK window.
//! 5. `Report(WorkDone)` or `Report(WorkFailed)` is the terminal action; the
//!    tier feeds that input to the session machine.
//! 6. A tier that sees the session leave `Working(token)` between two actions
//!    feeds [`WorkInput::SessionEnded`] and gets [`WorkAction::Abandon`], also
//!    terminal: nothing is performed and nothing is reported.
//!
//! # Out-of-phase input
//!
//! Any input after the terminal action, a second `Start`, or a `StepDone` /
//! `StepFailed` before the first `Start` is answered with
//! [`WorkAction::Ignored`] and leaves the state unchanged (ADR 023 decision 5).
//! `Ignored` is never a request for I/O; a tier loop that sees it has fallen
//! out of step with the machine (the ESP-IDF runner drops the work and logs an
//! error, the esp-hal `serve` returns).
//!
//! # Finiteness
//!
//! One connect work is finite (ADR 023 decision 6): at most `plan.len() + 2`
//! actions — the plan's steps, `Finish` and the terminal `Report` — or fewer
//! when a step fails or the session ends. The machine itself never waits; the
//! tier bounds each step.
//!
//! # Sync port
//!
//! A sync tier implements [`ConnectPort`] (one driver call per method, ADR 023
//! decision 10) and runs [`perform`] once per action; an async tier maps the
//! actions itself, one await per action.

use super::session::{ConnectStep, ConnectWorkPlan, SessionInput, SessionToken};

/// What the tier reports after performing the previous [`WorkAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkInput {
    /// The first poll of a new machine.
    Start,
    /// The step (or the finish) succeeded; a step with nothing to send counts as done.
    StepDone,
    /// The step (or the finish) failed: a refused SUBSCRIBE, enqueue, or finish send.
    StepFailed,
    /// The session machine left `Working(token)` meanwhile (NetworkDown, Disconnected, Shutdown).
    SessionEnded,
}

/// What the tier performs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkAction {
    /// Perform one plan step with the client (one driver call), then feed
    /// [`WorkInput::StepDone`] or [`WorkInput::StepFailed`].
    Step(ConnectStep),
    /// The tier's closing step before the session is ready: IDF sends every runtime
    /// subscription still owed and marks the tracker live; hal drains the SUBACKs.
    /// Then feed `StepDone` / `StepFailed`. Emitted exactly once, also for an empty plan.
    Finish,
    /// Feed this to the session machine: `WorkDone(token)` or `WorkFailed(token)`. Terminal.
    Report(SessionInput),
    /// The session ended while the work ran: perform nothing, report nothing. Terminal.
    Abandon,
    /// Input after the terminal action, or a second `Start`: the tier loop is out of step
    /// (ADR 023 decisions 5 and 6); state unchanged.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkPhase {
    /// No `Start` yet.
    NotStarted,
    /// This plan step is being performed.
    Step(ConnectStep),
    /// The closing step is being performed.
    Finishing,
    /// Reported or abandoned; every further input is ignored.
    Terminal,
}

/// One session's connect work as a finite action machine (ADR 023 rung 3).
#[derive(Debug, Clone)]
pub struct ConnectWorkMachine {
    token: SessionToken,
    plan: ConnectWorkPlan,
    session_present: bool,
    phase: WorkPhase,
}

impl ConnectWorkMachine {
    /// The work of the session `token`, over `plan`, connected with the
    /// broker's `session_present` flag.
    pub fn new(token: SessionToken, plan: ConnectWorkPlan, session_present: bool) -> Self {
        Self {
            token,
            plan,
            session_present,
            phase: WorkPhase::NotStarted,
        }
    }

    /// The session this work belongs to.
    pub const fn token(&self) -> SessionToken {
        self.token
    }

    /// The broker's session-present flag, for the tier's connect callback.
    pub const fn session_present(&self) -> bool {
        self.session_present
    }

    /// `true` once the work reported or was abandoned.
    pub fn is_terminal(&self) -> bool {
        self.phase == WorkPhase::Terminal
    }

    /// Advances the machine and returns what the tier must do next.
    pub fn poll(&mut self, input: WorkInput) -> WorkAction {
        use WorkInput as I;
        use WorkPhase as P;

        match (self.phase, input) {
            (P::Terminal, _) => WorkAction::Ignored,
            (P::NotStarted, I::Start) => self.advance(),
            (_, I::Start) => WorkAction::Ignored,
            (_, I::SessionEnded) => {
                self.phase = P::Terminal;
                WorkAction::Abandon
            }
            (P::NotStarted, I::StepDone | I::StepFailed) => WorkAction::Ignored,
            (P::Step(_), I::StepDone) => self.advance(),
            // A failing connect callback is logged by the tier, never fatal.
            (P::Step(ConnectStep::RunOnConnect), I::StepFailed) => self.advance(),
            (P::Step(_), I::StepFailed) => self.report(SessionInput::WorkFailed(self.token)),
            (P::Finishing, I::StepDone) => self.report(SessionInput::WorkDone(self.token)),
            (P::Finishing, I::StepFailed) => self.report(SessionInput::WorkFailed(self.token)),
        }
    }

    /// The next plan step, or the closing step once the plan is exhausted.
    fn advance(&mut self) -> WorkAction {
        match self.plan.next() {
            Some(step) => {
                self.phase = WorkPhase::Step(step);
                WorkAction::Step(step)
            }
            None => {
                self.phase = WorkPhase::Finishing;
                WorkAction::Finish
            }
        }
    }

    fn report(&mut self, input: SessionInput) -> WorkAction {
        self.phase = WorkPhase::Terminal;
        WorkAction::Report(input)
    }
}

/// One session's connect work as seen by a sync tier: each method is one driver call.
pub trait ConnectPort {
    /// SUBSCRIBE for the tier's entry `index` (+ its tracker `mark_sent`); a missing entry is `true`.
    fn subscribe(&mut self, index: usize) -> bool;
    /// Publish the startup message; `true` when none is configured.
    fn publish_startup(&mut self) -> bool;
    /// Run the connect callback with the broker's session-present flag; its failure is the tier's to log.
    fn run_on_connect(&mut self, session_present: bool);
    /// The closing step: send what is still owed and go live (IDF), or drain acks (a tier with SUBACK visibility).
    fn finish(&mut self) -> bool;
}

/// Performs one [`WorkAction`] through `port` and returns the input to feed next;
/// `None` for `Report`, `Abandon` and `Ignored`, which ask for no driver call.
pub fn perform<P: ConnectPort>(
    port: &mut P,
    action: WorkAction,
    session_present: bool,
) -> Option<WorkInput> {
    let done = match action {
        WorkAction::Step(ConnectStep::Subscribe(index)) => port.subscribe(index),
        WorkAction::Step(ConnectStep::PublishStartup) => port.publish_startup(),
        WorkAction::Step(ConnectStep::RunOnConnect) => {
            port.run_on_connect(session_present);
            true
        }
        WorkAction::Finish => port.finish(),
        WorkAction::Report(_) | WorkAction::Abandon | WorkAction::Ignored => return None,
    };
    Some(if done {
        WorkInput::StepDone
    } else {
        WorkInput::StepFailed
    })
}

/// A [`ConnectPort`] that records its calls and answers from a script, for
/// host tests of a tier loop (also used by the session tests).
#[cfg(test)]
pub(crate) mod scripted {
    use super::ConnectPort;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    /// One recorded port call.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Call {
        Subscribe(usize),
        PublishStartup,
        RunOnConnect(bool),
        Finish,
    }

    /// Answers each fallible call with the next scripted result (`true` once
    /// the script is used up) and records every call in order.
    #[derive(Debug, Default)]
    pub(crate) struct ScriptedPort {
        pub(crate) results: VecDeque<bool>,
        pub(crate) calls: Vec<Call>,
    }

    impl ScriptedPort {
        /// A port whose every call succeeds.
        pub(crate) fn accepting() -> Self {
            Self::default()
        }

        /// A port answering the fallible calls with `results` in order.
        pub(crate) fn scripted(results: impl IntoIterator<Item = bool>) -> Self {
            Self {
                results: results.into_iter().collect(),
                calls: Vec::new(),
            }
        }

        fn answer(&mut self, call: Call) -> bool {
            self.calls.push(call);
            self.results.pop_front().unwrap_or(true)
        }
    }

    impl ConnectPort for ScriptedPort {
        fn subscribe(&mut self, index: usize) -> bool {
            self.answer(Call::Subscribe(index))
        }

        fn publish_startup(&mut self) -> bool {
            self.answer(Call::PublishStartup)
        }

        fn run_on_connect(&mut self, session_present: bool) {
            self.calls.push(Call::RunOnConnect(session_present));
        }

        fn finish(&mut self) -> bool {
            self.answer(Call::Finish)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scripted::{Call, ScriptedPort};
    use super::*;
    use crate::mqtt::{MqttConnectionState, SessionAction, SessionMachine};
    use alloc::vec::Vec;
    use ConnectStep as S;
    use WorkAction as A;
    use WorkInput as I;

    /// Tokens only come from a session machine; `n` consecutive sessions.
    fn tokens<const N: usize>() -> [SessionToken; N] {
        let mut m = SessionMachine::new();
        m.poll(SessionInput::NetworkUp);
        core::array::from_fn(|i| {
            if i > 0 {
                m.poll(SessionInput::Disconnected);
                m.poll(SessionInput::RetryElapsed);
            }
            match m.poll(SessionInput::Connected {
                session_present: false,
            }) {
                SessionAction::ConnectWork { token, .. } => token,
                other => panic!("expected ConnectWork, got {:?}", other),
            }
        })
    }

    fn token() -> SessionToken {
        tokens::<1>()[0]
    }

    fn full(token: SessionToken) -> ConnectWorkMachine {
        ConnectWorkMachine::new(token, ConnectWorkPlan::new(true, 3, true, true), false)
    }

    /// Polls with `StepDone` after `Start` until the terminal action; the actions seen.
    fn run_all_done(m: &mut ConnectWorkMachine) -> Vec<WorkAction> {
        let mut actions = Vec::new();
        let mut input = I::Start;
        loop {
            let action = m.poll(input);
            actions.push(action);
            match action {
                A::Step(_) | A::Finish => input = I::StepDone,
                A::Report(_) | A::Abandon | A::Ignored => return actions,
            }
        }
    }

    #[test]
    fn a_full_plan_steps_in_order_then_finishes_then_reports_done() {
        let token = token();
        let mut m = full(token);
        assert!(!m.is_terminal());
        assert_eq!(
            run_all_done(&mut m),
            [
                A::Step(S::Subscribe(0)),
                A::Step(S::Subscribe(1)),
                A::Step(S::Subscribe(2)),
                A::Step(S::PublishStartup),
                A::Step(S::RunOnConnect),
                A::Finish,
                A::Report(SessionInput::WorkDone(token)),
            ]
        );
        assert!(m.is_terminal());
    }

    #[test]
    fn an_empty_plan_still_finishes_before_reporting_done() {
        let token = token();
        for plan in [
            ConnectWorkPlan::new(false, 3, false, false),
            ConnectWorkPlan::new(true, 0, false, false),
        ] {
            assert!(plan.is_empty());
            let mut m = ConnectWorkMachine::new(token, plan, true);
            assert_eq!(
                m.poll(I::Start),
                A::Finish,
                "a kept session with nothing to send still goes live"
            );
            assert_eq!(
                m.poll(I::StepDone),
                A::Report(SessionInput::WorkDone(token))
            );
        }
    }

    #[test]
    fn a_kept_session_plan_carries_only_startup_and_on_connect() {
        let token = token();
        let mut m =
            ConnectWorkMachine::new(token, ConnectWorkPlan::new(false, 3, true, true), true);
        assert!(m.session_present());
        assert_eq!(
            run_all_done(&mut m),
            [
                A::Step(S::PublishStartup),
                A::Step(S::RunOnConnect),
                A::Finish,
                A::Report(SessionInput::WorkDone(token)),
            ]
        );
    }

    #[test]
    fn a_failed_subscribe_reports_failed_and_runs_nothing_more() {
        let token = token();
        let mut m = full(token);
        assert_eq!(m.poll(I::Start), A::Step(S::Subscribe(0)));
        assert_eq!(m.poll(I::StepDone), A::Step(S::Subscribe(1)));
        assert_eq!(
            m.poll(I::StepFailed),
            A::Report(SessionInput::WorkFailed(token))
        );
        assert!(m.is_terminal());
        assert_eq!(m.poll(I::StepDone), A::Ignored, "no step after the failure");
    }

    #[test]
    fn a_failed_startup_publish_reports_failed() {
        let token = token();
        let mut m =
            ConnectWorkMachine::new(token, ConnectWorkPlan::new(false, 3, true, true), true);
        assert_eq!(m.poll(I::Start), A::Step(S::PublishStartup));
        assert_eq!(
            m.poll(I::StepFailed),
            A::Report(SessionInput::WorkFailed(token))
        );
        assert!(m.is_terminal());
    }

    #[test]
    fn a_failing_connect_callback_does_not_fail_the_work() {
        let token = token();
        let mut m =
            ConnectWorkMachine::new(token, ConnectWorkPlan::new(false, 0, false, true), false);
        assert_eq!(m.poll(I::Start), A::Step(S::RunOnConnect));
        assert_eq!(
            m.poll(I::StepFailed),
            A::Finish,
            "a failing connect callback is logged, never fatal"
        );
        assert_eq!(
            m.poll(I::StepDone),
            A::Report(SessionInput::WorkDone(token))
        );
    }

    #[test]
    fn a_failed_finish_reports_failed() {
        let token = token();
        let mut m =
            ConnectWorkMachine::new(token, ConnectWorkPlan::new(true, 1, false, false), false);
        assert_eq!(m.poll(I::Start), A::Step(S::Subscribe(0)));
        assert_eq!(m.poll(I::StepDone), A::Finish);
        assert_eq!(
            m.poll(I::StepFailed),
            A::Report(SessionInput::WorkFailed(token))
        );
        assert!(m.is_terminal());
    }

    #[test]
    fn session_ended_between_steps_abandons_without_a_report() {
        let mut m = full(token());
        assert_eq!(m.poll(I::Start), A::Step(S::Subscribe(0)));
        assert_eq!(m.poll(I::StepDone), A::Step(S::Subscribe(1)));
        assert_eq!(m.poll(I::SessionEnded), A::Abandon);
        assert!(m.is_terminal());
        for input in [I::StepDone, I::StepFailed, I::SessionEnded, I::Start] {
            assert_eq!(m.poll(input), A::Ignored, "no report after {:?}", input);
        }
    }

    #[test]
    fn session_ended_before_the_first_step_abandons() {
        let mut m = full(token());
        assert_eq!(m.poll(I::SessionEnded), A::Abandon);
        assert!(m.is_terminal());
        assert_eq!(m.poll(I::Start), A::Ignored);
    }

    #[test]
    fn session_ended_during_finish_abandons() {
        let mut m =
            ConnectWorkMachine::new(token(), ConnectWorkPlan::new(true, 1, false, false), false);
        assert_eq!(m.poll(I::Start), A::Step(S::Subscribe(0)));
        assert_eq!(m.poll(I::StepDone), A::Finish);
        assert_eq!(m.poll(I::SessionEnded), A::Abandon);
        assert!(m.is_terminal());
        assert_eq!(m.poll(I::StepDone), A::Ignored);
    }

    #[test]
    fn input_after_the_terminal_action_is_ignored() {
        let token = token();
        let mut reported = full(token);
        let actions = run_all_done(&mut reported);
        assert_eq!(
            actions.last(),
            Some(&A::Report(SessionInput::WorkDone(token)))
        );
        let mut abandoned = full(token);
        abandoned.poll(I::Start);
        assert_eq!(abandoned.poll(I::SessionEnded), A::Abandon);
        for m in [&mut reported, &mut abandoned] {
            assert!(m.is_terminal());
            for input in [I::Start, I::StepDone, I::StepFailed, I::SessionEnded] {
                assert_eq!(m.poll(input), A::Ignored, "{:?}", input);
                assert!(m.is_terminal(), "state unchanged after {:?}", input);
            }
            assert_eq!(m.poll(I::StepDone), A::Ignored, "Ignored again");
        }
    }

    #[test]
    fn a_second_start_is_ignored() {
        let mut m = full(token());
        assert_eq!(m.poll(I::Start), A::Step(S::Subscribe(0)));
        assert_eq!(m.poll(I::Start), A::Ignored);
        assert_eq!(
            m.poll(I::StepDone),
            A::Step(S::Subscribe(1)),
            "the second Start changed nothing"
        );
        assert_eq!(m.poll(I::StepDone), A::Step(S::Subscribe(2)));
        assert_eq!(m.poll(I::Start), A::Ignored);
        assert_eq!(m.poll(I::StepDone), A::Step(S::PublishStartup));
    }

    #[test]
    fn inputs_outside_their_phase_leave_the_state_unchanged() {
        let mut m = full(token());
        assert_eq!(m.poll(I::StepDone), A::Ignored);
        assert_eq!(m.poll(I::StepFailed), A::Ignored);
        assert!(!m.is_terminal());
        assert_eq!(
            m.poll(I::Start),
            A::Step(S::Subscribe(0)),
            "still not started, the plan is untouched"
        );
    }

    #[test]
    fn the_report_carries_the_work_token() {
        let [first, second] = tokens::<2>();
        assert_ne!(first, second);
        let mut a =
            ConnectWorkMachine::new(first, ConnectWorkPlan::new(true, 1, false, false), false);
        let mut b =
            ConnectWorkMachine::new(second, ConnectWorkPlan::new(true, 1, false, false), true);
        assert_eq!(a.token(), first);
        assert_eq!(b.token(), second);
        assert_eq!(
            run_all_done(&mut a).last(),
            Some(&A::Report(SessionInput::WorkDone(first)))
        );
        b.poll(I::Start);
        assert_eq!(
            b.poll(I::StepFailed),
            A::Report(SessionInput::WorkFailed(second))
        );
    }

    #[test]
    fn the_work_ends_within_plan_len_plus_two_actions() {
        let token = token();
        let plans = [
            ConnectWorkPlan::new(true, 3, true, true),
            ConnectWorkPlan::new(true, 8, false, false),
            ConnectWorkPlan::new(false, 3, true, false),
            ConnectWorkPlan::new(false, 3, false, false),
        ];
        for plan in plans {
            let len = plan.clone().count();
            // Every step succeeds: steps, Finish, Report.
            let mut m = ConnectWorkMachine::new(token, plan.clone(), false);
            assert_eq!(run_all_done(&mut m).len(), len + 2, "plan of {} steps", len);
            // A failure at every position ends it no later.
            for fail_at in 0..=len {
                let mut m = ConnectWorkMachine::new(token, plan.clone(), false);
                let mut actions = 0;
                let mut input = I::Start;
                loop {
                    let action = m.poll(input);
                    actions += 1;
                    assert!(actions <= len + 2, "plan of {} steps did not end", len);
                    match action {
                        A::Step(_) | A::Finish => {
                            input = if actions == fail_at + 1 {
                                I::StepFailed
                            } else {
                                I::StepDone
                            };
                        }
                        A::Report(_) => break,
                        other => panic!("unexpected {:?}", other),
                    }
                }
                assert!(m.is_terminal());
            }
        }
    }

    #[test]
    fn perform_maps_each_action_to_exactly_one_port_call() {
        let token = token();
        let mut m = ConnectWorkMachine::new(token, ConnectWorkPlan::new(true, 2, true, true), true);
        let mut port = ScriptedPort::accepting();
        let mut input = I::Start;
        let report = loop {
            let action = m.poll(input);
            let before = port.calls.len();
            match perform(&mut port, action, m.session_present()) {
                Some(next) => {
                    assert_eq!(port.calls.len(), before + 1, "{:?} is one call", action);
                    input = next;
                }
                None => break action,
            }
        };
        assert_eq!(report, A::Report(SessionInput::WorkDone(token)));
        assert_eq!(
            port.calls,
            [
                Call::Subscribe(0),
                Call::Subscribe(1),
                Call::PublishStartup,
                Call::RunOnConnect(true),
                Call::Finish,
            ]
        );
    }

    #[test]
    fn perform_turns_a_refused_call_into_step_failed() {
        let mut port = ScriptedPort::scripted([false, true, false, false]);
        assert_eq!(
            perform(&mut port, A::Step(S::Subscribe(4)), false),
            Some(I::StepFailed)
        );
        assert_eq!(
            perform(&mut port, A::Step(S::Subscribe(5)), false),
            Some(I::StepDone)
        );
        assert_eq!(
            perform(&mut port, A::Step(S::PublishStartup), false),
            Some(I::StepFailed)
        );
        assert_eq!(perform(&mut port, A::Finish, false), Some(I::StepFailed));
        assert_eq!(
            port.calls,
            [
                Call::Subscribe(4),
                Call::Subscribe(5),
                Call::PublishStartup,
                Call::Finish
            ]
        );
    }

    #[test]
    fn perform_never_fails_the_work_on_a_connect_callback() {
        // The port cannot refuse `run_on_connect`; a scripted `false` is left
        // for the next fallible call.
        let mut port = ScriptedPort::scripted([false]);
        assert_eq!(
            perform(&mut port, A::Step(S::RunOnConnect), true),
            Some(I::StepDone)
        );
        assert_eq!(port.calls, [Call::RunOnConnect(true)]);
        assert_eq!(port.results.len(), 1, "no result consumed");
    }

    #[test]
    fn perform_answers_nothing_for_a_terminal_action() {
        let token = token();
        let mut port = ScriptedPort::accepting();
        for action in [
            A::Report(SessionInput::WorkDone(token)),
            A::Report(SessionInput::WorkFailed(token)),
            A::Abandon,
            A::Ignored,
        ] {
            assert_eq!(perform(&mut port, action, false), None, "{:?}", action);
        }
        assert!(port.calls.is_empty(), "no port call for a terminal action");
    }

    // ── Scripted runner turn over both machines ──────────────────────────────

    /// Subscriptions the modelled tier has configured.
    const SUBSCRIPTIONS: usize = 2;

    /// A connect work in progress: the machine plus the input to feed it next.
    struct PendingWork {
        machine: ConnectWorkMachine,
        next: WorkInput,
    }

    /// A model of the ESP-IDF runner's turn: session inputs are drained
    /// between work actions, each work action is performed through the port,
    /// and the session is probed before every action.
    struct Model {
        session: SessionMachine,
        work: Option<PendingWork>,
        port: ScriptedPort,
        /// Every session action, in order.
        session_actions: Vec<SessionAction>,
        /// Every work action, in order.
        work_actions: Vec<WorkAction>,
        polls: usize,
    }

    impl Model {
        fn new() -> Self {
            Self {
                session: SessionMachine::new(),
                work: None,
                port: ScriptedPort::accepting(),
                session_actions: Vec::new(),
                work_actions: Vec::new(),
                polls: 0,
            }
        }

        /// A session input handed to the runner by the event loop (or by itself).
        fn feed(&mut self, input: SessionInput) -> SessionAction {
            self.polls += 1;
            let action = self.session.poll(input);
            self.session_actions.push(action);
            if let SessionAction::ConnectWork {
                token,
                subscribe,
                session_present,
            } = action
            {
                let plan = ConnectWorkPlan::new(subscribe, SUBSCRIPTIONS, true, true);
                self.work = Some(PendingWork {
                    machine: ConnectWorkMachine::new(token, plan, session_present),
                    next: I::Start,
                });
            }
            action
        }

        /// Exactly one work action, as the runner's `work_turn`.
        fn work_turn(&mut self) -> Option<WorkAction> {
            let mut work = self.work.take()?;
            let token = work.machine.token();
            let input = if self.session.is_current(token) {
                work.next
            } else {
                I::SessionEnded
            };
            self.polls += 1;
            let action = work.machine.poll(input);
            self.work_actions.push(action);
            match action {
                A::Step(_) | A::Finish => {
                    work.next = perform(&mut self.port, action, work.machine.session_present())
                        .expect("a step asks for a port call");
                    self.work = Some(work);
                }
                A::Report(report) => {
                    self.feed(report);
                }
                A::Abandon | A::Ignored => {}
            }
            Some(action)
        }

        /// Turns until no work is pending; the bound is the decision-6 one.
        fn run_work(&mut self) {
            let mut turns = 0;
            while self.work.is_some() {
                turns += 1;
                assert!(turns <= SUBSCRIPTIONS + 2 + 2, "the work did not end");
                self.work_turn();
            }
        }

        fn connected(&mut self, session_present: bool) -> (SessionToken, bool) {
            match self.feed(SessionInput::Connected { session_present }) {
                SessionAction::ConnectWork {
                    token, subscribe, ..
                } => (token, subscribe),
                other => panic!("expected ConnectWork, got {:?}", other),
            }
        }

        fn reached_ready(&self) -> bool {
            self.session_actions.contains(&SessionAction::Ready)
        }
    }

    const FULL_PLAN: [Call; 5] = [
        Call::Subscribe(0),
        Call::Subscribe(1),
        Call::PublishStartup,
        Call::RunOnConnect(false),
        Call::Finish,
    ];

    #[test]
    fn a_full_connect_work_drives_the_port_in_plan_order_and_reaches_ready() {
        let mut model = Model::new();
        assert_eq!(model.feed(SessionInput::NetworkUp), SessionAction::Connect);
        model.connected(false);
        model.run_work();
        assert_eq!(model.port.calls, FULL_PLAN);
        assert_eq!(model.session_actions.last(), Some(&SessionAction::Ready));
        assert!(model.session.can_publish());
        assert_eq!(model.session.connect_count(), 1);
        assert_eq!(model.work_actions.len(), FULL_PLAN.len() + 1);
    }

    #[test]
    fn a_network_drop_between_connect_steps_closes_before_the_next_step_and_never_reports_ready() {
        let mut model = Model::new();
        model.feed(SessionInput::NetworkUp);
        model.connected(false);
        assert_eq!(model.work_turn(), Some(A::Step(S::Subscribe(0))));
        // Drained before the next step: the runner acts on it first.
        assert_eq!(model.feed(SessionInput::NetworkDown), SessionAction::Close);
        assert_eq!(model.work_turn(), Some(A::Abandon));
        assert!(model.work.is_none());
        assert_eq!(model.work_actions, [A::Step(S::Subscribe(0)), A::Abandon]);
        assert_eq!(model.port.calls, [Call::Subscribe(0)]);
        assert!(
            !model.port.calls.contains(&Call::Finish),
            "finish never runs for a session that ended"
        );
        assert!(!model.reached_ready());
        assert_eq!(model.feed(SessionInput::Disconnected), SessionAction::None);
        assert_eq!(model.session.state(), MqttConnectionState::Disconnected);
        assert_eq!(model.feed(SessionInput::NetworkUp), SessionAction::Connect);
        let (_, subscribe) = model.connected(true);
        assert!(
            subscribe,
            "the kept session's subscriptions were incomplete"
        );
    }

    #[test]
    fn a_broker_drop_between_connect_steps_waits_then_reconnects_and_subscribes_again() {
        let mut model = Model::new();
        model.feed(SessionInput::NetworkUp);
        model.connected(false);
        assert_eq!(model.work_turn(), Some(A::Step(S::Subscribe(0))));
        assert!(matches!(
            model.feed(SessionInput::Disconnected),
            SessionAction::Wait { .. }
        ));
        assert_eq!(model.work_turn(), Some(A::Abandon));
        assert!(!model.reached_ready());
        assert_eq!(
            model.feed(SessionInput::RetryElapsed),
            SessionAction::Connect
        );
        let (_, subscribe) = model.connected(true);
        assert!(subscribe);
        model.run_work();
        assert!(model.session.can_publish());
        assert_eq!(model.session.connect_count(), 1);
        assert_eq!(
            model.port.calls,
            [
                Call::Subscribe(0),
                Call::Subscribe(0),
                Call::Subscribe(1),
                Call::PublishStartup,
                Call::RunOnConnect(true),
                Call::Finish,
            ],
            "the new (kept) session redoes the whole plan"
        );
    }

    #[test]
    fn shutdown_between_connect_steps_closes_and_abandons() {
        let mut model = Model::new();
        model.feed(SessionInput::NetworkUp);
        let (token, _) = model.connected(false);
        assert_eq!(model.work_turn(), Some(A::Step(S::Subscribe(0))));
        assert_eq!(model.feed(SessionInput::Shutdown), SessionAction::Close);
        assert_eq!(model.work_turn(), Some(A::Abandon));
        assert!(model.work.is_none());
        assert_eq!(model.port.calls, [Call::Subscribe(0)]);
        assert!(!model.reached_ready());
        for input in [
            SessionInput::Disconnected,
            SessionInput::NetworkUp,
            SessionInput::RetryElapsed,
            SessionInput::WorkDone(token),
            SessionInput::Connected {
                session_present: false,
            },
        ] {
            assert_eq!(model.feed(input), SessionAction::Ignored, "{:?}", input);
        }
        assert_eq!(model.session.state(), MqttConnectionState::ShuttingDown);
    }

    #[test]
    fn a_late_work_done_from_an_abandoned_work_is_ignored_by_the_session() {
        let mut model = Model::new();
        model.feed(SessionInput::NetworkUp);
        let (old, _) = model.connected(false);
        model.work_turn();
        model.feed(SessionInput::NetworkDown);
        assert_eq!(model.work_turn(), Some(A::Abandon));
        // Defence in depth: even a report that slipped through is stale.
        assert_eq!(
            model.feed(SessionInput::WorkDone(old)),
            SessionAction::Ignored
        );
        assert!(!model.session.can_publish());
        model.feed(SessionInput::Disconnected);
        model.feed(SessionInput::NetworkUp);
        let (current, _) = model.connected(true);
        assert_ne!(old, current);
        assert_eq!(
            model.feed(SessionInput::WorkDone(old)),
            SessionAction::Ignored
        );
        assert_eq!(
            model.feed(SessionInput::WorkFailed(old)),
            SessionAction::Ignored
        );
        assert!(
            model.session.is_current(current),
            "the stale input changed nothing"
        );
        model.run_work();
        assert!(model.session.can_publish());
        assert_eq!(model.session.connect_count(), 1);
    }

    #[test]
    fn connect_work_with_interleaved_inputs_settles_within_a_bounded_number_of_polls() {
        // One session input lands after turn `at` of the work; the loop must
        // end the work within the decision-6 bound and leave the session settled.
        const MAX_POLLS: usize = 16;
        let interleaved = [
            None,
            Some(SessionInput::NetworkDown),
            Some(SessionInput::Disconnected),
            Some(SessionInput::Shutdown),
            Some(SessionInput::NetworkUp),
            Some(SessionInput::AttemptStarted),
            Some(SessionInput::RetryElapsed),
        ];
        for input in interleaved {
            for at in 0..=FULL_PLAN.len() + 1 {
                let mut model = Model::new();
                model.feed(SessionInput::NetworkUp);
                model.connected(false);
                let mut turns = 0;
                while model.work.is_some() {
                    if turns == at {
                        if let Some(input) = input {
                            if model.feed(input) == SessionAction::Close {
                                model.feed(SessionInput::Disconnected);
                            }
                        }
                    }
                    turns += 1;
                    assert!(
                        turns <= FULL_PLAN.len() + 1,
                        "{:?} at {} did not end the work",
                        input,
                        at
                    );
                    model.work_turn();
                }
                assert!(
                    model.polls <= MAX_POLLS,
                    "{:?} at {} took {} polls",
                    input,
                    at,
                    model.polls
                );
                assert!(
                    !model.work_actions.contains(&A::Ignored),
                    "{:?} at {} fell out of step",
                    input,
                    at
                );
                let state = model.session.state();
                let settled = model.session.can_publish()
                    || state == MqttConnectionState::Disconnected
                    || state == MqttConnectionState::ShuttingDown;
                assert!(settled, "{:?} at {} ended in {:?}", input, at, state);
                if model.reached_ready() {
                    assert_eq!(model.port.calls, FULL_PLAN, "{:?} at {}", input, at);
                } else {
                    // Nothing is performed after the session ended: only the
                    // steps of the turns before the input happened.
                    assert_eq!(
                        model.port.calls,
                        &FULL_PLAN[..at.min(FULL_PLAN.len())],
                        "{:?} at {}: a step ran for a session that ended",
                        input,
                        at
                    );
                }
            }
        }
    }
}
