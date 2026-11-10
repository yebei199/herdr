//! Host declarations: a process in the pane's foreground job (an editor that
//! runs the agent in its own embedded terminal) states which agent the pane
//! stands for. Herdr cannot see that agent's process, so the declaration takes
//! the place of the foreground process probe while the host lives.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use tokio::sync::Notify;

use super::{AgentProcessLiveness, ForegroundShellAgentAction};
use crate::detect::{Agent, AgentDetection, AgentState};

/// A host process, told apart from a later process that reuses its pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostProcess {
    pub pid: u32,
    pub start_token: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostAgentClaim {
    pub host: HostProcess,
    /// `None` once the host's agent has exited.
    pub agent: Option<Agent>,
    /// The pane shows the host, not the agent: only the title is evidence.
    pub hidden: bool,
    /// The host knows the agent waits for the user.
    pub waiting: bool,
    pub seq: Option<u64>,
}

/// The pane's current claim, shared by the API and the detection task.
#[derive(Default)]
pub(crate) struct HostAgentCell {
    claim: Mutex<Option<HostAgentClaim>>,
    revision: AtomicU64,
    wake: Notify,
}

impl HostAgentCell {
    pub(crate) fn with_claim(claim: Option<HostAgentClaim>) -> Self {
        Self {
            claim: Mutex::new(claim),
            ..Self::default()
        }
    }

    pub(crate) fn get(&self) -> Option<HostAgentClaim> {
        self.claim.lock().ok().and_then(|claim| *claim)
    }

    /// Replaces the claim and wakes detection. A claim from the same host with
    /// a sequence not above the current one is stale and ignored.
    pub(crate) fn declare(&self, claim: HostAgentClaim) -> bool {
        let Ok(mut current) = self.claim.lock() else {
            return false;
        };
        let stale = current.is_some_and(|current| {
            current.host == claim.host
                && claim
                    .seq
                    .zip(current.seq)
                    .is_some_and(|(incoming, current)| incoming <= current)
        });
        if stale {
            return false;
        }
        *current = Some(claim);
        drop(current);
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_one();
        true
    }

    /// Drops the claim of a host that is gone, unless a newer host replaced it.
    fn retire(&self, host: HostProcess) {
        let Ok(mut current) = self.claim.lock() else {
            return;
        };
        if current.is_some_and(|claim| claim.host == host) {
            *current = None;
            drop(current);
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(super) async fn changed(&self) {
        self.wake.notified().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostPresence {
    Foreground,
    /// Alive in the pane but not in front, e.g. stopped with Ctrl-Z.
    Background,
    Gone,
}

pub(super) fn host_presence(
    host_process_group: Option<u32>,
    foreground_process_group: Option<u32>,
) -> HostPresence {
    match host_process_group {
        None => HostPresence::Gone,
        Some(group) if Some(group) == foreground_process_group => HostPresence::Foreground,
        Some(_) => HostPresence::Background,
    }
}

/// What a claim means for the pane's agent identity on one detection tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostAgentStep {
    /// Apply this action as if the probe had seen `agent` in front.
    Decide {
        action: ForegroundShellAgentAction,
        agent: Option<Agent>,
    },
    /// The host is suspended with its agent: keep the identity, ignore the screen.
    Hold,
    /// The claim says nothing about the foreground: probe it as usual.
    Probe,
}

/// `bound` is the host whose claim produced the current agent. An agent from
/// another host, or a different agent, exits before the claimed one starts.
pub(super) fn host_agent_step(
    claim: &HostAgentClaim,
    presence: HostPresence,
    bound: Option<HostProcess>,
    previous: Option<Agent>,
    exit_reported: bool,
) -> HostAgentStep {
    let claimed = match presence {
        HostPresence::Foreground => claim.agent,
        HostPresence::Background if previous.is_some() => return HostAgentStep::Hold,
        HostPresence::Background => return HostAgentStep::Probe,
        HostPresence::Gone => None,
    };
    let action = match previous {
        None => ForegroundShellAgentAction::ObserveProbe,
        Some(previous) if exit_reported => {
            if claimed == Some(previous) {
                ForegroundShellAgentAction::ReportReplacementProcess
            } else if claimed.is_none() {
                ForegroundShellAgentAction::ClearAgent
            } else {
                ForegroundShellAgentAction::ObserveProbe
            }
        }
        Some(previous)
            if claimed == Some(previous) && bound.is_none_or(|bound| bound == claim.host) =>
        {
            ForegroundShellAgentAction::ObserveProbe
        }
        Some(_) => ForegroundShellAgentAction::ReportProcessExit,
    };
    HostAgentStep::Decide {
        action,
        agent: claimed,
    }
}

/// Screen evidence for one detection read. A process exit publishes its idle
/// whatever the claim says, so the completion it carries is never lost.
pub(super) fn screen_detection(
    claim: Option<&HostAgentClaim>,
    agent: Option<Agent>,
    content: &str,
    osc_title: &str,
    osc_progress: &str,
    process_exited: bool,
) -> Option<AgentDetection> {
    match claim {
        Some(claim) if !process_exited => {
            host_screen_detection(claim, agent, content, osc_title, osc_progress)
        }
        _ => super::detection_update_for_publish_with_osc(
            agent,
            content,
            osc_title,
            osc_progress,
            process_exited,
        ),
    }
}

/// Screen evidence under a claim that holds the current agent. Waiting is
/// blocked; hidden reads only the title, and its idle still goes through the
/// working-to-idle confirmation because the host may be about to report waiting.
fn host_screen_detection(
    claim: &HostAgentClaim,
    agent: Option<Agent>,
    content: &str,
    osc_title: &str,
    osc_progress: &str,
) -> Option<AgentDetection> {
    if claim.waiting {
        return Some(AgentDetection {
            state: AgentState::Blocked,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: true,
            visible_working: false,
        });
    }
    if !claim.hidden {
        return super::detection_update_for_publish_with_osc(
            agent,
            content,
            osc_title,
            osc_progress,
            false,
        );
    }
    hidden_title_detection(crate::detect::detect_title_only(
        agent?,
        osc_title,
        osc_progress,
    ))
}

fn hidden_title_detection(detection: AgentDetection) -> Option<AgentDetection> {
    (!detection.skip_state_update).then_some(AgentDetection {
        visible_idle: false,
        ..detection
    })
}

/// How long the screen is ignored after a hidden agent is shown again: the
/// host's repaint can reach the pane after its declaration does.
const REVEAL_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Per-task view of the pane's claim.
#[derive(Default)]
pub(super) struct HostAgentTracker {
    bound: Option<HostProcess>,
    seen_revision: u64,
    last_hidden: bool,
    reveal_settle_until: Option<std::time::Instant>,
}

pub(super) struct HostAgentTick {
    pub claim: HostAgentClaim,
    pub presence: HostPresence,
    /// The claim changed since the previous tick.
    pub changed: bool,
}

impl HostAgentTick {
    /// The claim governs screen evidence only while its host is in front and
    /// the pane's current agent is the claimed one.
    pub(super) fn screen_claim(&self, agent: Option<Agent>) -> Option<&HostAgentClaim> {
        (self.presence == HostPresence::Foreground && agent.is_some() && self.claim.agent == agent)
            .then_some(&self.claim)
    }
}

impl HostAgentTracker {
    pub(super) fn observe(
        &mut self,
        cell: &HostAgentCell,
        shell_pid: u32,
        foreground_process_group: Option<u32>,
        processes: &impl AgentProcessLiveness,
        now: std::time::Instant,
    ) -> Option<HostAgentTick> {
        let revision = cell.revision();
        let changed = revision != self.seen_revision;
        self.seen_revision = revision;
        let Some(claim) = cell.get() else {
            self.last_hidden = false;
            return None;
        };
        if self.last_hidden && !claim.hidden && claim.agent.is_some() {
            self.reveal_settle_until = Some(now + REVEAL_SETTLE);
        }
        self.last_hidden = claim.hidden;
        let host_group = (shell_pid > 0)
            .then(|| {
                processes.live_process_group(shell_pid, claim.host.pid, claim.host.start_token)
            })
            .flatten();
        Some(HostAgentTick {
            claim,
            presence: host_presence(host_group, foreground_process_group),
            changed,
        })
    }

    pub(super) fn bound(&self) -> Option<HostProcess> {
        self.bound
    }

    /// True while a just-revealed agent's screen may still show the host.
    pub(super) fn screen_settling(&mut self, now: std::time::Instant) -> bool {
        if self.reveal_settle_until.is_some_and(|until| now < until) {
            return true;
        }
        self.reveal_settle_until = None;
        false
    }

    /// Records the outcome of a decided step. Returns true once a gone host's
    /// claim has been retired and the pane is back to process probing.
    pub(super) fn settle(
        &mut self,
        cell: &HostAgentCell,
        tick: &HostAgentTick,
        action: ForegroundShellAgentAction,
        current_agent: Option<Agent>,
    ) -> bool {
        if action != ForegroundShellAgentAction::ReportProcessExit {
            self.bound = current_agent.map(|_| tick.claim.host);
        }
        if tick.presence != HostPresence::Gone || current_agent.is_some() {
            return false;
        }
        cell.retire(tick.claim.host);
        self.bound = None;
        self.seen_revision = cell.revision();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: HostProcess = HostProcess {
        pid: 40,
        start_token: 7,
    };
    const OTHER_HOST: HostProcess = HostProcess {
        pid: 41,
        start_token: 9,
    };

    fn claim(agent: Option<Agent>) -> HostAgentClaim {
        HostAgentClaim {
            host: HOST,
            agent,
            hidden: false,
            waiting: false,
            seq: None,
        }
    }

    fn decide(action: ForegroundShellAgentAction, agent: Option<Agent>) -> HostAgentStep {
        HostAgentStep::Decide { action, agent }
    }

    struct FakeProcesses(Option<u32>);

    impl AgentProcessLiveness for FakeProcesses {
        fn start_token(&self, _pid: u32) -> Option<u64> {
            None
        }

        fn live_process_group(&self, _shell_pid: u32, _pid: u32, _token: u64) -> Option<u32> {
            self.0
        }
    }

    #[test]
    fn tracker_reports_changes_presence_and_reveal_settling() {
        let cell = HostAgentCell::default();
        let mut tracker = HostAgentTracker::default();
        let now = std::time::Instant::now();
        let front = FakeProcesses(Some(7));
        assert!(tracker.observe(&cell, 1, Some(7), &front, now).is_none());

        assert!(cell.declare(HostAgentClaim {
            hidden: true,
            ..claim(Some(Agent::Pi))
        }));
        let tick = tracker.observe(&cell, 1, Some(7), &front, now).unwrap();
        assert!(tick.changed);
        assert_eq!(tick.presence, HostPresence::Foreground);
        assert!(
            !tracker
                .observe(&cell, 1, Some(7), &front, now)
                .unwrap()
                .changed
        );
        assert!(!tracker.screen_settling(now));

        assert!(cell.declare(claim(Some(Agent::Pi))));
        tracker.observe(&cell, 1, Some(7), &front, now).unwrap();
        assert!(
            tracker.screen_settling(now),
            "a reveal waits for the repaint"
        );
        assert!(!tracker.screen_settling(now + REVEAL_SETTLE));

        let suspended = tracker.observe(&cell, 1, Some(8), &front, now).unwrap();
        assert_eq!(suspended.presence, HostPresence::Background);
        let gone = tracker
            .observe(&cell, 1, Some(7), &FakeProcesses(None), now)
            .unwrap();
        assert_eq!(gone.presence, HostPresence::Gone);
        assert!(tracker.settle(&cell, &gone, ForegroundShellAgentAction::ClearAgent, None));
        assert!(cell.get().is_none(), "a gone host's claim is retired");
    }

    #[test]
    fn host_presence_follows_the_host_process_group() {
        assert_eq!(host_presence(Some(5), Some(5)), HostPresence::Foreground);
        assert_eq!(host_presence(Some(5), Some(6)), HostPresence::Background);
        assert_eq!(host_presence(Some(5), None), HostPresence::Background);
        assert_eq!(host_presence(None, Some(5)), HostPresence::Gone);
    }

    #[test]
    fn claim_acquires_and_keeps_its_agent() {
        let codex = claim(Some(Agent::Codex));
        assert_eq!(
            host_agent_step(&codex, HostPresence::Foreground, None, None, false),
            decide(ForegroundShellAgentAction::ObserveProbe, Some(Agent::Codex))
        );
        assert_eq!(
            host_agent_step(
                &codex,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                false
            ),
            decide(ForegroundShellAgentAction::ObserveProbe, Some(Agent::Codex))
        );
    }

    #[test]
    fn none_reports_a_process_exit_and_then_clears() {
        let none = claim(None);
        assert_eq!(
            host_agent_step(
                &none,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                false
            ),
            decide(ForegroundShellAgentAction::ReportProcessExit, None)
        );
        assert_eq!(
            host_agent_step(
                &none,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                true
            ),
            decide(ForegroundShellAgentAction::ClearAgent, None)
        );
        assert_eq!(
            host_agent_step(&none, HostPresence::Foreground, None, None, false),
            decide(ForegroundShellAgentAction::ObserveProbe, None)
        );
    }

    #[test]
    fn switching_agents_reports_the_previous_exit_first() {
        let pi = claim(Some(Agent::Pi));
        assert_eq!(
            host_agent_step(
                &pi,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                false
            ),
            decide(
                ForegroundShellAgentAction::ReportProcessExit,
                Some(Agent::Pi)
            )
        );
        assert_eq!(
            host_agent_step(
                &pi,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                true
            ),
            decide(ForegroundShellAgentAction::ObserveProbe, Some(Agent::Pi))
        );
    }

    #[test]
    fn a_new_host_with_the_same_agent_is_a_new_agent() {
        let codex = HostAgentClaim {
            host: OTHER_HOST,
            ..claim(Some(Agent::Codex))
        };
        assert_eq!(
            host_agent_step(
                &codex,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                false
            ),
            decide(
                ForegroundShellAgentAction::ReportProcessExit,
                Some(Agent::Codex)
            )
        );
        assert_eq!(
            host_agent_step(
                &codex,
                HostPresence::Foreground,
                Some(HOST),
                Some(Agent::Codex),
                true
            ),
            decide(
                ForegroundShellAgentAction::ReportReplacementProcess,
                Some(Agent::Codex)
            )
        );
    }

    #[test]
    fn an_agent_found_by_probe_is_adopted_by_a_matching_claim() {
        assert_eq!(
            host_agent_step(
                &claim(Some(Agent::Codex)),
                HostPresence::Foreground,
                None,
                Some(Agent::Codex),
                false
            ),
            decide(ForegroundShellAgentAction::ObserveProbe, Some(Agent::Codex))
        );
    }

    #[test]
    fn gone_host_exits_its_agent_and_suspended_host_holds_it() {
        let codex = claim(Some(Agent::Codex));
        assert_eq!(
            host_agent_step(
                &codex,
                HostPresence::Gone,
                Some(HOST),
                Some(Agent::Codex),
                false
            ),
            decide(ForegroundShellAgentAction::ReportProcessExit, None)
        );
        assert_eq!(
            host_agent_step(
                &codex,
                HostPresence::Background,
                Some(HOST),
                Some(Agent::Codex),
                false
            ),
            HostAgentStep::Hold
        );
        assert_eq!(
            host_agent_step(&codex, HostPresence::Background, None, None, false),
            HostAgentStep::Probe
        );
    }

    #[test]
    fn declare_ignores_stale_sequences_from_the_same_host() {
        let cell = HostAgentCell::default();
        let first = HostAgentClaim {
            seq: Some(5),
            ..claim(Some(Agent::Pi))
        };
        assert!(cell.declare(first));
        assert!(!cell.declare(HostAgentClaim {
            seq: Some(5),
            agent: None,
            ..first
        }));
        assert!(cell.declare(HostAgentClaim {
            host: OTHER_HOST,
            seq: Some(1),
            ..first
        }));
        assert_eq!(cell.get().map(|claim| claim.host), Some(OTHER_HOST));
        cell.retire(HOST);
        assert!(
            cell.get().is_some(),
            "a replaced host cannot retire the claim"
        );
        cell.retire(OTHER_HOST);
        assert!(cell.get().is_none());
    }

    #[test]
    fn waiting_is_blocked_and_hidden_reads_only_the_title_without_visible_idle() {
        let waiting = HostAgentClaim {
            hidden: true,
            waiting: true,
            ..claim(Some(Agent::Claude))
        };
        let blocked = host_screen_detection(&waiting, Some(Agent::Claude), "", "", "")
            .expect("waiting publishes");
        assert_eq!(blocked.state, AgentState::Blocked);
        assert!(blocked.visible_blocker);

        let hidden = HostAgentClaim {
            waiting: false,
            ..waiting
        };
        let detection = host_screen_detection(
            &hidden,
            Some(Agent::Claude),
            "screen text",
            "unmatched title",
            "",
        );
        assert_eq!(
            detection.map(|detection| detection.state),
            Some(AgentState::Unknown)
        );
        assert!(host_screen_detection(&hidden, None, "", "", "").is_none());
    }

    #[test]
    fn hidden_title_idle_waits_for_the_working_to_idle_confirmation() {
        let title_idle = AgentDetection {
            state: AgentState::Idle,
            skip_state_update: false,
            visible_idle: true,
            visible_blocker: false,
            visible_working: false,
        };
        let detection = hidden_title_detection(title_idle).expect("idle publishes");
        assert!(!detection.visible_idle);
        let mut pending_idle = super::super::PendingIdleConfirmation::default();
        let decision = super::super::decide_screen_detection_publish(
            super::super::agent_detection::ScreenDetectionPublishInput {
                current_state: AgentState::Working,
                last_visible_idle: false,
                last_visible_blocker: false,
                last_visible_working: true,
                last_visible_signal_refresh: None,
                screen_detection: detection,
                process_exited: false,
                agent_changed: false,
                now: std::time::Instant::now(),
            },
            &mut pending_idle,
        );
        assert_eq!(decision, super::super::DetectionPublishDecision::NoPublish);
        assert!(pending_idle.active());
        assert!(hidden_title_detection(AgentDetection {
            skip_state_update: true,
            ..title_idle
        })
        .is_none());
    }

    #[test]
    fn process_exit_publishes_idle_through_a_hidden_or_waiting_claim() {
        for claim in [
            HostAgentClaim {
                hidden: true,
                ..claim(Some(Agent::Pi))
            },
            HostAgentClaim {
                hidden: true,
                waiting: true,
                ..claim(Some(Agent::Pi))
            },
        ] {
            let detection = screen_detection(Some(&claim), Some(Agent::Pi), "", "", "", true)
                .expect("exit publishes");
            assert_eq!(detection.state, AgentState::Idle);
            assert!(detection.visible_idle);
        }
    }

    #[test]
    fn screen_claim_applies_only_to_the_claimed_agent_in_front() {
        let tick = |presence| HostAgentTick {
            claim: HostAgentClaim {
                hidden: true,
                ..claim(Some(Agent::Pi))
            },
            presence,
            changed: false,
        };
        assert!(tick(HostPresence::Foreground)
            .screen_claim(Some(Agent::Pi))
            .is_some());
        assert!(tick(HostPresence::Foreground)
            .screen_claim(Some(Agent::Codex))
            .is_none());
        assert!(tick(HostPresence::Foreground).screen_claim(None).is_none());
        assert!(tick(HostPresence::Gone)
            .screen_claim(Some(Agent::Pi))
            .is_none());
    }
}
