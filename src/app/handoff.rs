//! Transfer auxiliary terminals without adding ephemeral UI state to disk sessions.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::state::PopupPaneState;
use super::App;
use crate::handoff_runtime::ImportedHandoffRuntime;
use crate::layout::PaneId;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

/// The placement and launch metadata needed to reattach a running auxiliary PTY.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct HandoffPopup {
    pub state: PopupPaneState,
    pub visible: bool,
    pub cwd: PathBuf,
    pub launch_argv: Option<Vec<String>>,
}

/// Split auxiliary descriptors before the ordinary layout's strict import pass.
pub(crate) fn take_popup_imports(
    popups: Vec<HandoffPopup>,
    imports: &mut HashMap<u32, ImportedHandoffRuntime>,
) -> io::Result<Vec<(HandoffPopup, ImportedHandoffRuntime)>> {
    popups
        .into_iter()
        .map(|popup| {
            let import = imports
                .remove(&popup.state.pane_id.raw())
                .ok_or_else(|| io::Error::other("missing auxiliary terminal runtime"))?;
            Ok((popup, import))
        })
        .collect()
}

impl App {
    /// Capture visible popups and hidden floats before disconnecting any client.
    pub(crate) fn capture_handoff_popups(&self) -> io::Result<Vec<HandoffPopup>> {
        self.state
            .popup_pane
            .iter()
            .map(|popup| (popup, true))
            .chain(
                self.state
                    .hidden_floats
                    .values()
                    .map(|popup| (popup, false)),
            )
            .map(|(popup, visible)| {
                let terminal = self
                    .state
                    .terminals
                    .get(&popup.terminal_id)
                    .ok_or_else(|| io::Error::other("auxiliary terminal state missing"))?;
                if self.terminal_runtimes.get(&popup.terminal_id).is_none() {
                    return Err(io::Error::other("auxiliary terminal runtime missing"));
                }
                Ok(HandoffPopup {
                    state: popup.clone(),
                    visible,
                    cwd: terminal.cwd.clone(),
                    launch_argv: terminal.launch_argv.clone(),
                })
            })
            .collect()
    }

    /// Restore all auxiliary PTYs with ownership deferred until the whole handoff commits.
    pub(crate) fn restore_handoff_popups(
        &mut self,
        imports: Vec<(HandoffPopup, ImportedHandoffRuntime)>,
    ) -> io::Result<()> {
        let mut keys = HashSet::new();
        for (saved, mut import) in imports {
            if saved.visible && self.state.popup_pane.is_some() {
                return Err(io::Error::other("multiple visible auxiliary terminals"));
            }
            match saved.state.float.as_ref() {
                Some(key)
                    if key.tab_is_live(&self.state.workspaces) && keys.insert(key.clone()) => {}
                None if saved.visible => {}
                _ => return Err(io::Error::other("invalid auxiliary terminal tab binding")),
            }
            let pane_id = PaneId::alloc();
            let terminal_id = TerminalId::alloc();
            let agent_state = import.state.agent_state.take();
            import.state = import.state.with_pane_id(pane_id);
            let runtime = TerminalRuntime::from_handoff_fd(
                import,
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.state.host_terminal_appearance,
                self.event_tx.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            )?;
            let mut terminal = TerminalState::new(terminal_id.clone(), saved.cwd);
            terminal.launch_argv = saved.launch_argv;
            if let Some(agent_state) = agent_state {
                terminal.restore_handoff_agent_state(agent_state);
            }
            self.state.terminals.insert(terminal_id.clone(), terminal);
            self.terminal_runtimes.insert(terminal_id.clone(), runtime);
            let popup = PopupPaneState {
                pane_id,
                terminal_id,
                ..saved.state
            };
            if saved.visible {
                self.state.popup_pane = Some(popup);
            } else if let Some(key) = &popup.float {
                self.state.hidden_floats.insert(key.clone(), popup);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{App, ImportedHandoffRuntime};
    use crate::app::popup::PopupGeometry;
    use std::time::{Duration, Instant};

    fn test_app() -> App {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("handoff")];
        app.state.active = Some(0);
        app
    }

    async fn wait_output(path: &std::path::Path, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(path).unwrap_or_default() != expected {
            assert!(
                Instant::now() < deadline,
                "popup did not process terminal input"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auxiliary_handoff_keeps_shell_memory_input_and_detection_policy() {
        let directory = std::env::temp_dir().join(format!(
            "herdr-popup-{}-{}",
            std::process::id(),
            crate::layout::PaneId::alloc().raw()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let output = directory.join("received");
        let mut source = test_app();
        source.spawn_popup_argv_command(
            &[
                "bash".into(), "-c".into(),
                "secret=retained; while IFS= read -r line; do printf '%s:%s\\n' \"$secret\" \"$line\" > \"$1\"; done".into(),
                "handoff-test".into(), output.to_string_lossy().into_owned(),
            ],
            Some(directory.clone()), Vec::new(), PopupGeometry::default(),
        ).unwrap();
        let saved = source.capture_handoff_popups().unwrap().remove(0);
        let runtime = source
            .terminal_runtimes
            .get(&saved.state.terminal_id)
            .unwrap();
        runtime
            .try_send_bytes(bytes::Bytes::from_static(b"before\r"))
            .unwrap();
        wait_output(&output, "retained:before\n").await;
        let pid = runtime.child_pid();
        runtime
            .pause_handoff_reader(Duration::from_secs(2))
            .unwrap();
        let state = runtime.handoff_runtime_state(saved.state.pane_id.raw());
        assert!(state.agent_detection_disabled);
        let import = ImportedHandoffRuntime {
            master_fd: runtime.duplicate_handoff_fd().unwrap(),
            state,
        };
        let mut replacement = test_app();
        replacement
            .restore_handoff_popups(vec![(saved.clone(), import)])
            .unwrap();
        source
            .terminal_runtimes
            .remove(&saved.state.terminal_id)
            .unwrap()
            .preserve_for_handoff();
        replacement.assume_handoff_ownership();
        replacement.unpause_handoff_readers();
        let popup = replacement.state.popup_pane.as_ref().unwrap();
        assert_ne!(popup.pane_id, saved.state.pane_id);
        let runtime = replacement
            .terminal_runtimes
            .get(&popup.terminal_id)
            .unwrap();
        assert_eq!(runtime.child_pid(), pid);
        assert!(
            runtime
                .handoff_runtime_state(popup.pane_id.raw())
                .agent_detection_disabled
        );
        runtime
            .try_send_bytes(bytes::Bytes::from_static(b"after\r"))
            .unwrap();
        wait_output(&output, "retained:after\n").await;
        assert!(replacement.close_popup_pane());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
