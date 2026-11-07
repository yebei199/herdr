use std::path::PathBuf;

use crate::app::state::{AppState, FloatKey, PopupPaneState};
use crate::app::{App, Mode};
use crate::layout::PaneId;
use crate::pane::PaneLaunchEnv;
use crate::popup_size::{resolve_popup_geometry, PopupSize};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PopupGeometry {
    pub width: Option<PopupSize>,
    pub height: Option<PopupSize>,
}

impl App {
    pub(crate) fn close_popup_pane(&mut self) -> bool {
        let Some(popup) = self.state.popup_pane.take() else {
            return false;
        };
        self.state
            .direct_attach_resize_locks
            .remove(&popup.terminal_id);
        self.state.terminals.remove(&popup.terminal_id);
        self.shutdown_terminal_runtime(popup.terminal_id);
        self.return_input_from_popup();
        true
    }

    fn return_input_from_popup(&mut self) {
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Shows the active workspace's float for this binding, or hides it when it is up.
    /// The terminal outlives hiding, so a running command is found as it was left.
    pub(crate) fn toggle_float(
        &mut self,
        binding: &crate::config::CustomCommandKeybind,
    ) -> std::io::Result<()> {
        let workspace = self
            .state
            .active
            .and_then(|index| self.state.workspaces.get(index))
            .ok_or_else(|| std::io::Error::other("no active workspace"))?;
        let key = FloatKey {
            workspace_id: workspace.id.clone(),
            command: binding.command.clone(),
        };
        let visible = self
            .state
            .popup_pane
            .as_ref()
            .map(|popup| popup.float.as_ref());
        if visible == Some(None) {
            return Err(std::io::Error::other("popup already open"));
        }
        let was_visible = visible == Some(Some(&key));
        self.hide_visible_float();
        if was_visible {
            return Ok(());
        }
        if let Some(float) = self.state.hidden_floats.remove(&key) {
            self.state.popup_pane = Some(float);
            self.state.mode = Mode::Terminal;
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            return Ok(());
        }
        self.spawn_popup_shell_command(
            &binding.command,
            None,
            self.custom_command_env().0,
            PopupGeometry {
                width: binding.width,
                height: binding.height,
            },
        )?;
        if let Some(popup) = self.state.popup_pane.as_mut() {
            popup.float = Some(key);
        }
        Ok(())
    }

    /// Takes a float off screen; plain popups are left alone.
    pub(crate) fn hide_visible_float(&mut self) -> bool {
        let Some(key) = self.state.popup_pane.as_ref().and_then(|p| p.float.clone()) else {
            return false;
        };
        if let Some(float) = self.state.popup_pane.take() {
            self.state.hidden_floats.insert(key, float);
        }
        self.return_input_from_popup();
        true
    }

    pub(crate) fn forget_hidden_float(&mut self, pane_id: PaneId) -> bool {
        let Some(key) = self
            .state
            .hidden_floats
            .iter()
            .find(|(_, float)| float.pane_id == pane_id)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        if let Some(float) = self.state.hidden_floats.remove(&key) {
            self.state.terminals.remove(&float.terminal_id);
            self.shutdown_terminal_runtime(float.terminal_id);
        }
        true
    }

    pub(crate) fn spawn_popup_shell_command(
        &mut self,
        command: &str,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
    ) -> std::io::Result<()> {
        self.spawn_popup_command(
            cwd,
            extra_env,
            geometry,
            |pane_id, rows, cols, cwd, launch_env, app| {
                TerminalRuntime::spawn_shell_command(
                    pane_id,
                    rows,
                    cols,
                    cwd,
                    command,
                    launch_env,
                    crate::pane::AgentDetection::Disabled,
                    app.state.pane_scrollback_limit_bytes,
                    app.state.host_terminal_theme,
                    app.state.host_terminal_appearance,
                    app.event_tx.clone(),
                    app.render_notify.clone(),
                    app.render_dirty.clone(),
                )
                .map(|runtime| (runtime, None))
            },
        )
    }

    pub(crate) fn spawn_popup_argv_command(
        &mut self,
        argv: &[String],
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
    ) -> std::io::Result<()> {
        self.spawn_popup_command(
            cwd,
            extra_env,
            geometry,
            |pane_id, rows, cols, cwd, launch_env, app| {
                TerminalRuntime::spawn_argv_command(
                    pane_id,
                    rows,
                    cols,
                    cwd,
                    argv,
                    launch_env,
                    crate::pane::AgentDetection::Disabled,
                    app.state.pane_scrollback_limit_bytes,
                    app.state.host_terminal_theme,
                    app.state.host_terminal_appearance,
                    app.event_tx.clone(),
                    app.render_notify.clone(),
                    app.render_dirty.clone(),
                )
                .map(|runtime| (runtime, Some(argv.to_vec())))
            },
        )
    }

    fn spawn_popup_command<F>(
        &mut self,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
        spawn: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(
            PaneId,
            u16,
            u16,
            PathBuf,
            &PaneLaunchEnv,
            &mut App,
        ) -> std::io::Result<(TerminalRuntime, Option<Vec<String>>)>,
    {
        if self.state.popup_pane.is_some() {
            return Err(std::io::Error::other("popup already open"));
        }
        let Some(ws_idx) = self.state.active else {
            return Err(std::io::Error::other("no active workspace"));
        };
        let ws = self
            .state
            .workspaces
            .get(ws_idx)
            .ok_or_else(|| std::io::Error::other("active workspace disappeared"))?;
        let active_tab = ws
            .active_tab()
            .ok_or_else(|| std::io::Error::other("active tab disappeared"))?;
        let focused_pane = ws
            .focused_pane_id()
            .ok_or_else(|| std::io::Error::other("active tab has no focused pane"))?;
        let cwd = cwd.or_else(|| {
            active_tab.cwd_for_pane(focused_pane, &self.state.terminals, &self.terminal_runtimes)
        });
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let launch_env = PaneLaunchEnv::from_extra(extra_env).without_pane_identity();
        let terminal_area = if self.state.view.terminal_area.width >= 4
            && self.state.view.terminal_area.height >= 4
        {
            self.state.view.terminal_area
        } else {
            let (cols, rows) = self.state.headless_size;
            ratatui::layout::Rect::new(0, 0, cols, rows)
        };
        let Some(resolved_geometry) =
            resolve_popup_geometry(geometry.width, geometry.height, terminal_area)
        else {
            return Err(std::io::Error::other("terminal area too small for popup"));
        };
        let rows = resolved_geometry.inner.height;
        let cols = resolved_geometry.inner.width;
        let (runtime, launch_argv) = spawn(pane_id, rows, cols, cwd.clone(), &launch_env, self)?;
        let terminal = match launch_argv {
            Some(argv) => TerminalState::new(terminal_id.clone(), cwd).with_launch_argv(argv),
            None => TerminalState::new(terminal_id.clone(), cwd),
        };
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id,
            terminal_id,
            width: geometry.width,
            height: geometry.height,
            float: None,
        });
        self.state.mode = Mode::Terminal;
        Ok(())
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn install_test_popup_runtime(
        &mut self,
        runtime: TerminalRuntime,
    ) -> (PaneId, TerminalId) {
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/popup")),
        );
        self.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id,
            terminal_id: terminal_id.clone(),
            width: None,
            height: None,
            float: None,
        });
        (pane_id, terminal_id)
    }
}

impl AppState {
    /// A float belongs to its workspace, so it ends with it, however the workspace went away.
    pub(crate) fn queue_orphan_float_shutdowns(&mut self) {
        if self.hidden_floats.is_empty() {
            return;
        }
        let live: std::collections::HashSet<&str> =
            self.workspaces.iter().map(|ws| ws.id.as_str()).collect();
        let orphans: Vec<FloatKey> = self
            .hidden_floats
            .keys()
            .filter(|key| !live.contains(key.workspace_id.as_str()))
            .cloned()
            .collect();
        for key in orphans {
            if let Some(PopupPaneState { terminal_id, .. }) = self.hidden_floats.remove(&key) {
                self.terminals.remove(&terminal_id);
                self.terminal_runtime_shutdowns.push(terminal_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_popup() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("popup")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = TerminalId::alloc();
        app.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/popup")),
        );
        app.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id: PaneId::alloc(),
            terminal_id,
            width: None,
            height: None,
            float: None,
        });
        app
    }

    #[test]
    fn close_popup_uses_terminal_mode_with_active_workspace() {
        let mut app = app_with_popup();
        app.state.mode = Mode::Navigate;

        assert!(app.close_popup_pane());

        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[test]
    fn close_popup_uses_navigate_mode_without_active_workspace() {
        let mut app = app_with_popup();
        app.state.workspaces.clear();
        app.state.active = None;
        app.state.mode = Mode::Navigate;

        assert!(app.close_popup_pane());

        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[test]
    fn close_popup_clears_direct_attach_resize_lock() {
        let mut app = app_with_popup();
        let terminal_id = app.state.popup_pane.as_ref().unwrap().terminal_id.clone();
        app.state
            .direct_attach_resize_locks
            .insert(terminal_id.clone());

        assert!(app.close_popup_pane());

        assert!(!app.state.direct_attach_resize_locks.contains(&terminal_id));
    }

    #[test]
    fn popup_survives_background_workspace_removal() {
        let mut app = app_with_popup();
        app.state.workspaces.clear();
        app.state.active = None;

        app.state.assert_invariants_for_test();

        assert!(app.state.popup_pane.is_some());
    }

    #[test]
    fn popup_close_api_closes_only_active_popup() {
        let mut app = app_with_popup();
        let close = || crate::api::schema::Request {
            id: "close-popup".into(),
            method: crate::api::schema::Method::PopupClose(
                crate::api::schema::EmptyParams::default(),
            ),
        };

        let response = app.handle_api_request(close());
        let response: crate::api::schema::SuccessResponse =
            serde_json::from_str(&response).unwrap();
        assert_eq!(response.result, crate::api::schema::ResponseResult::Ok {});

        let response = app.handle_api_request(close());
        let response: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(response.error.code, "popup_not_open");
    }

    fn float_binding(command: &str) -> crate::config::CustomCommandKeybind {
        crate::config::CustomCommandKeybind {
            bindings: crate::config::ActionKeybinds::prefix("t"),
            label: "alt+t".into(),
            command: command.into(),
            action: crate::config::CustomCommandAction::Float,
            description: None,
            width: None,
            height: None,
        }
    }

    fn app_with_float(command: &str) -> (App, FloatKey) {
        let mut app = app_with_popup();
        let key = FloatKey {
            workspace_id: app.state.workspaces[0].id.clone(),
            command: command.into(),
        };
        app.state.popup_pane.as_mut().unwrap().float = Some(key.clone());
        (app, key)
    }

    #[test]
    fn float_key_hides_without_ending_the_terminal() {
        let (mut app, key) = app_with_float("fish");
        let terminal_id = app.state.popup_pane.as_ref().unwrap().terminal_id.clone();

        app.toggle_float(&float_binding("fish")).unwrap();

        assert!(app.state.popup_pane.is_none());
        assert!(app.state.terminals.contains_key(&terminal_id));
        assert!(app.state.hidden_floats.contains_key(&key));
    }

    #[test]
    fn float_key_brings_back_the_same_terminal() {
        let (mut app, _) = app_with_float("fish");
        let terminal_id = app.state.popup_pane.as_ref().unwrap().terminal_id.clone();
        app.toggle_float(&float_binding("fish")).unwrap();

        app.toggle_float(&float_binding("fish")).unwrap();

        let popup = app.state.popup_pane.as_ref().expect("float visible");
        assert_eq!(popup.terminal_id, terminal_id);
        assert!(app.state.hidden_floats.is_empty());
        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[test]
    fn float_is_bound_to_its_workspace() {
        let (mut app, first_key) = app_with_float("fish");
        app.hide_visible_float();
        let first_terminal = app.state.hidden_floats[&first_key].terminal_id.clone();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("other"));
        app.state.active = Some(1);
        let second_key = FloatKey {
            workspace_id: app.state.workspaces[1].id.clone(),
            command: "fish".into(),
        };
        let second_terminal = TerminalId::alloc();
        app.state.terminals.insert(
            second_terminal.clone(),
            TerminalState::new(second_terminal.clone(), PathBuf::from("/other")),
        );
        app.state.hidden_floats.insert(
            second_key.clone(),
            PopupPaneState {
                pane_id: PaneId::alloc(),
                terminal_id: second_terminal.clone(),
                width: None,
                height: None,
                float: Some(second_key),
            },
        );

        app.toggle_float(&float_binding("fish")).unwrap();

        let popup = app.state.popup_pane.as_ref().expect("float visible");
        assert_eq!(popup.terminal_id, second_terminal);
        assert_eq!(
            app.state.hidden_floats[&first_key].terminal_id,
            first_terminal
        );
    }

    #[test]
    fn float_key_refuses_to_replace_a_plain_popup() {
        let mut app = app_with_popup();

        let error = app.toggle_float(&float_binding("fish")).unwrap_err();

        assert_eq!(error.to_string(), "popup already open");
        assert!(app.state.popup_pane.is_some());
    }

    #[test]
    fn plain_popup_is_not_hideable() {
        let mut app = app_with_popup();

        assert!(!app.hide_visible_float());

        assert!(app.state.popup_pane.is_some());
        assert!(app.state.hidden_floats.is_empty());
    }

    #[test]
    fn hidden_float_ends_with_its_workspace() {
        let (mut app, _) = app_with_float("fish");
        let terminal_id = app.state.popup_pane.as_ref().unwrap().terminal_id.clone();
        app.hide_visible_float();
        app.state.workspaces.clear();

        app.shutdown_detached_terminal_runtimes();

        assert!(app.state.hidden_floats.is_empty());
        assert!(!app.state.terminals.contains_key(&terminal_id));
    }

    #[test]
    fn hidden_float_survives_while_its_workspace_lives() {
        let (mut app, key) = app_with_float("fish");
        app.hide_visible_float();

        app.shutdown_detached_terminal_runtimes();

        assert!(app.state.hidden_floats.contains_key(&key));
    }

    #[test]
    fn exited_hidden_float_is_forgotten() {
        let (mut app, _) = app_with_float("fish");
        let popup = app.state.popup_pane.clone().unwrap();
        app.hide_visible_float();

        assert!(app.forget_hidden_float(popup.pane_id));

        assert!(app.state.hidden_floats.is_empty());
        assert!(!app.state.terminals.contains_key(&popup.terminal_id));
        assert!(!app.forget_hidden_float(popup.pane_id));
    }
}
