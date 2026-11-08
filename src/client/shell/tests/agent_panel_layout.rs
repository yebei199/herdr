use super::*;

const LONG_CJK: &str = "侧边栏长名字折成两行显示";
const OVERFLOW: &str = "给侧边栏的长会话名做折行并且第二行也放不下还要更长一些";

fn agent_in(pane_id: &str, workspace_id: &str, title: &str) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: format!("tab_{workspace_id}"),
        name: Some("codex".into()),
        display_agent: None,
        agent: Some("codex".into()),
        title: Some(title.into()),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

/// Two workspaces whose agents mix short, wrapping and overflowing pane names.
fn state_with(
    agents: Vec<ClientShellAgent>,
    sort: crate::config::AgentPanelSortConfig,
) -> ClientShellState {
    let mut projected = snapshot();
    let mut second = projected.workspaces[0].clone();
    second.workspace_id = "ws_2".into();
    second.label = "nixos".into();
    second.focused = false;
    second.active_tab_id = "tab_ws_2".into();
    projected.workspaces.push(second);
    projected.tabs = ["ws_1", "ws_2"]
        .into_iter()
        .map(|workspace_id| ClientShellTab {
            tab_id: format!("tab_{workspace_id}"),
            workspace_id: workspace_id.into(),
            ..projected.tabs[0].clone()
        })
        .collect();
    projected.agents = agents;
    let mut config = Config::default();
    config.ui.agent_panel_sort = sort;
    config.ui.sidebar.agents.rows = vec![
        vec![
            crate::config::AgentSidebarToken::StateIcon,
            crate::config::AgentSidebarToken::Pane,
        ],
        vec![crate::config::AgentSidebarToken::Agent],
    ];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

fn grouped(agents: Vec<ClientShellAgent>) -> ClientShellState {
    state_with(agents, crate::config::AgentPanelSortConfig::Spaces)
}

fn row_text(buffer: &Buffer, rect: Rect, row: u16) -> String {
    (rect.x..rect.right())
        .map(|x| buffer[(x, rect.y + row)].symbol())
        .collect()
}

fn agent_rect(state: &ClientShellState, pane_id: &str) -> Rect {
    state
        .hits
        .agents
        .iter()
        .find(|(_, pane)| pane == pane_id)
        .unwrap_or_else(|| panic!("{pane_id} is visible"))
        .0
}

fn click(state: &mut ClientShellState, x: u16, y: u16) -> crate::api::schema::Method {
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    })]);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("click should reach the endpoint API: {:?}", outcome.actions);
    };
    request.method.clone()
}

#[test]
fn grouped_panel_heads_each_workspace_and_wraps_long_names() {
    let mut state = grouped(vec![
        agent_in("pane_1", "ws_1", LONG_CJK),
        agent_in("pane_2", "ws_1", "short"),
        agent_in("pane_3", "ws_2", OVERFLOW),
    ]);
    let buffer = state.compose(106, 30).unwrap().to_ratatui_buffer().unwrap();

    let groups = state
        .hits
        .agent_groups
        .iter()
        .map(|hit| (hit.workspace_id.clone(), hit.rect))
        .collect::<Vec<_>>();
    assert_eq!(
        groups.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["ws_1", "ws_2"]
    );
    assert!(row_text(&buffer, groups[0].1, 0).starts_with(" client-shell"));
    assert!(row_text(&buffer, groups[1].1, 0).starts_with(" nixos"));

    // Wrapped name: two lines hanging under the name, then the agent row; nothing lost.
    let wrapped = agent_rect(&state, "pane_1");
    assert_eq!(wrapped.y, groups[0].1.bottom());
    assert_eq!(wrapped.height, 3);
    let first = row_text(&buffer, wrapped, 0);
    let second = row_text(&buffer, wrapped, 1);
    assert!(first.starts_with("   "), "nested one level: {first:?}");
    assert!(
        second.starts_with("     "),
        "hangs under the name: {second:?}"
    );
    let joined = format!("{first}{second}")
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .skip(1)
        .collect::<String>();
    assert_eq!(joined, LONG_CJK);
    assert!(row_text(&buffer, wrapped, 2)
        .trim_start()
        .starts_with("codex"));

    // Short names keep one line per configured row.
    assert_eq!(agent_rect(&state, "pane_2").height, 2);

    // Overflow: still two lines, the second ends in an ellipsis.
    let overflow = agent_rect(&state, "pane_3");
    assert_eq!(overflow.y, groups[1].1.bottom());
    assert_eq!(overflow.height, 3);
    assert!(row_text(&buffer, overflow, 1).trim_end().ends_with('…'));

    assert!(matches!(
        click(&mut state, groups[1].1.x + 1, groups[1].1.y),
        crate::api::schema::Method::WorkspaceFocus(target) if target.workspace_id == "ws_2"
    ));
}

#[test]
fn priority_panel_stays_flat() {
    let mut state = state_with(
        vec![
            agent_in("pane_1", "ws_1", "one"),
            agent_in("pane_2", "ws_2", "two"),
        ],
        crate::config::AgentPanelSortConfig::Priority,
    );
    let buffer = state.compose(106, 30).unwrap().to_ratatui_buffer().unwrap();
    assert!(state.hits.agent_groups.is_empty());
    let first = agent_rect(&state, "pane_1");
    assert_eq!(first.y, state.hits.agent_body.y);
    assert!(
        row_text(&buffer, first, 0).starts_with(" ")
            && !row_text(&buffer, first, 0).starts_with("  ")
    );
    assert_eq!(agent_rect(&state, "pane_2").y, first.bottom());
}

/// Every visible item is laid out at its wrapped height, back to back inside the body,
/// and a click on any row of an entry focuses that entry.
fn assert_hits_follow_real_heights(state: &mut ClientShellState) {
    let body = state.hits.agent_body;
    let mut rects = state
        .hits
        .agents
        .iter()
        .map(|(rect, pane)| (*rect, Some(pane.clone())))
        .chain(state.hits.agent_groups.iter().map(|hit| (hit.rect, None)))
        .collect::<Vec<_>>();
    rects.sort_by_key(|(rect, _)| rect.y);
    assert_eq!(rects[0].0.y, body.y);
    for pair in rects.windows(2) {
        assert_eq!(pair[1].0.y, pair[0].0.bottom(), "{rects:?}");
    }
    assert!(rects.last().unwrap().0.bottom() <= body.bottom());
    for (rect, pane) in rects {
        let Some(pane) = pane else { continue };
        let method = click(state, rect.x + 1, rect.bottom() - 1);
        assert!(
            matches!(&method, crate::api::schema::Method::PaneFocus(target) if target.pane_id == pane),
            "{pane}: {method:?}"
        );
    }
}

#[test]
fn variable_height_entries_scroll_and_hit_by_real_height() {
    let agents = (0..6)
        .map(|index| {
            let workspace = if index < 3 { "ws_1" } else { "ws_2" };
            let title = if index % 2 == 0 { LONG_CJK } else { "short" };
            agent_in(&format!("pane_{index}"), workspace, title)
        })
        .collect::<Vec<_>>();
    let mut state = grouped(agents);
    state.compose(106, 24).unwrap();
    let metrics = state.hits.agent_scroll_metrics.expect("agent metrics");
    assert!(metrics.max_offset_from_bottom > 0, "list overflows");
    assert!(!state.hits.agent_scrollbar.is_empty());
    assert_hits_follow_real_heights(&mut state);

    // Scrolled to the end, the last entry is fully visible and still hit-tested exactly.
    state.agent_scroll = usize::MAX;
    state.compose(106, 24).unwrap();
    assert_eq!(
        state.agent_scroll, state.hits.agent_max_scroll,
        "scroll clamps to the last start that fits"
    );
    assert_eq!(agent_rect(&state, "pane_5").height, 2);
    assert_hits_follow_real_heights(&mut state);

    // Navigating to an agent above the viewport brings its workspace heading along.
    let mut outcome = ClientShellInput::default();
    state.agent_scroll = usize::MAX;
    state.compose(106, 24).unwrap();
    assert!(!state.hits.agents.iter().any(|(_, pane)| pane == "pane_0"));
    if let Some(snapshot) = state.snapshot.as_mut() {
        snapshot.focused_pane_id = Some("pane_5".into());
    }
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextAgent),
        &mut outcome,
    );
    state.compose(106, 24).unwrap();
    assert_eq!(state.agent_scroll, 0);
    assert_eq!(
        agent_rect(&state, "pane_0").y,
        state.hits.agent_groups[0].rect.bottom()
    );
}
