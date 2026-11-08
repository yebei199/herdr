use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use super::*;

/// Lines one token row may wrap onto before the rest is elided.
const AGENT_ROW_MAX_LINES: usize = 2;

pub(super) struct AgentRow {
    pub(super) pane_id: String,
    pub(super) status: crate::api::schema::AgentStatus,
    pub(super) focused: bool,
    pub(super) workspace_id: String,
    pub(super) workspace_label: String,
    pub(super) workspace_focused: bool,
    pub(super) rows: Vec<Vec<crate::ui::ResolvedToken>>,
}

/// Rows the agent list can show; implemented by local and endpoint-qualified agent rows.
pub(super) trait AgentListRow {
    fn agent(&self) -> &AgentRow;
}

impl AgentListRow for AgentRow {
    fn agent(&self) -> &AgentRow {
        self
    }
}

pub(super) enum AgentListItem<T> {
    Group(AgentGroup),
    Agent(T),
}

/// Workspace heading above that workspace's agents in the grouped (`spaces`) panel.
pub(super) struct AgentGroup {
    pub(super) label: String,
    pub(super) endpoint_id: Option<ClientEndpointId>,
    pub(super) workspace_id: String,
    pub(super) focused: bool,
}

pub(super) struct AgentGroupHit {
    pub(super) rect: Rect,
    pub(super) endpoint_id: Option<ClientEndpointId>,
    pub(super) workspace_id: String,
}

fn agents_grouped(config: &ClientShellConfig) -> bool {
    config.agent_panel_sort == crate::config::AgentPanelSortConfig::Spaces
}

/// Inserts a workspace heading before each run of agents sharing one, when the panel is grouped.
pub(super) fn agent_list_items<T>(
    rows: Vec<T>,
    config: &ClientShellConfig,
    group_of: impl Fn(&T) -> AgentGroup,
) -> Vec<AgentListItem<T>> {
    if !agents_grouped(config) {
        return rows.into_iter().map(AgentListItem::Agent).collect();
    }
    let mut items = Vec::with_capacity(rows.len());
    let mut current: Option<(Option<ClientEndpointId>, String)> = None;
    for row in rows {
        let group = group_of(&row);
        let key = (group.endpoint_id.clone(), group.workspace_id.clone());
        if current.as_ref() != Some(&key) {
            items.push(AgentListItem::Group(group));
            current = Some(key);
        }
        items.push(AgentListItem::Agent(row));
    }
    items
}

fn local_agent_group(row: &AgentRow) -> AgentGroup {
    AgentGroup {
        label: row.workspace_label.clone(),
        endpoint_id: None,
        workspace_id: row.workspace_id.clone(),
        focused: row.workspace_focused,
    }
}

pub(super) fn local_agent_items(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
) -> Vec<AgentListItem<AgentRow>> {
    agent_list_items(
        agent_rows(snapshot, config, None),
        config,
        local_agent_group,
    )
}

pub(super) fn ordered_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    if snapshot.agent_view_label.is_some() {
        return snapshot
            .agent_order
            .iter()
            .filter(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == pane_id.as_str())
            })
            .cloned()
            .collect();
    }
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    if sort == crate::config::AgentPanelSortConfig::Priority {
        agents.sort_by_key(|agent| {
            (
                std::cmp::Reverse(status_priority(agent.agent_status)),
                std::cmp::Reverse(agent.state_change_seq),
            )
        });
    }
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

pub(super) fn render_agent_panel(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !render_agent_panel_header(
        buffer,
        area,
        snapshot.agent_view_label.as_deref(),
        config,
        hits,
    ) {
        return;
    }

    let items = local_agent_items(snapshot, config);
    render_agent_list(
        buffer,
        area,
        &items,
        snapshot
            .agent_view_label
            .as_ref()
            .map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        |_, rect, row, hits| hits.agents.push((rect, row.pane_id.clone())),
    );
}

pub(super) fn render_agent_panel_header(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) -> bool {
    if area.height == 0 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(config.palette.surface_dim),
    );
    if area.height < 2 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " agents",
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let sort_label = agent_view_label.unwrap_or(match config.agent_panel_sort {
        crate::config::AgentPanelSortConfig::Spaces => "grouped",
        crate::config::AgentPanelSortConfig::Priority => "priority",
    });
    let sort_width = display_width(sort_label).min(area.width as usize) as u16;
    let sort_rect = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );
    hits.agent_sort_toggle = if config.mouse_capture && agent_view_label.is_none() {
        sort_rect
    } else {
        Rect::default()
    };
    put_text(
        buffer,
        sort_rect.x,
        sort_rect.y,
        sort_rect.width,
        sort_label,
        Style::default()
            .fg(if agent_view_label.is_some() {
                config.palette.accent
            } else {
                config.palette.overlay0
            })
            .add_modifier(Modifier::BOLD),
    );
    true
}

/// Agent list measured at one width: every item's lines, the gaps after them and the
/// scroll metrics they produce. Rendering and reveal both read heights from here.
struct AgentListLayout {
    lines: Vec<Vec<Line<'static>>>,
    heights: Vec<u16>,
    gaps: Vec<u16>,
    metrics: crate::pane::ScrollMetrics,
    content_width: u16,
}

fn layout_agent_list<T: AgentListRow>(
    items: &[AgentListItem<T>],
    body: Rect,
    requested_start: usize,
    config: &ClientShellConfig,
) -> AgentListLayout {
    let gaps = items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            AgentListItem::Agent(_) if index + 1 < items.len() => config.agents.row_gap,
            _ => 0,
        })
        .collect::<Vec<_>>();
    let measure = |width: u16| {
        let lines = items
            .iter()
            .map(|item| match item {
                AgentListItem::Group(group) => vec![agent_group_line(group, width, config)],
                AgentListItem::Agent(row) => agent_lines(row.agent(), width, config),
            })
            .collect::<Vec<_>>();
        let heights = lines
            .iter()
            .map(|lines| lines.len().clamp(1, u16::MAX as usize) as u16)
            .collect::<Vec<_>>();
        let metrics =
            super::scroll::list_scroll_metrics(&heights, &gaps, body.height, requested_start);
        (lines, heights, metrics)
    };
    let (mut lines, mut heights, mut metrics) = measure(body.width);
    let mut content_width = body.width;
    // The scrollbar takes a column, which can only add wrapped lines, so it stays needed.
    if metrics.max_offset_from_bottom > 0 && body.width > 1 {
        content_width = body.width - 1;
        (lines, heights, metrics) = measure(content_width);
    }
    AgentListLayout {
        lines,
        heights,
        gaps,
        metrics,
        content_width,
    }
}

/// Scroll start that keeps item `target` visible, pulling in its workspace heading when the
/// agent is the first of its group and both fit.
pub(super) fn agent_scroll_to_reveal<T: AgentListRow>(
    items: &[AgentListItem<T>],
    target: usize,
    body: Rect,
    scroll: usize,
    config: &ClientShellConfig,
) -> usize {
    if body.is_empty() || target >= items.len() {
        return scroll;
    }
    let layout = layout_agent_list(items, body, scroll, config);
    let start = super::scroll::list_scroll_start_to_reveal(
        &layout.heights,
        &layout.gaps,
        body.height,
        scroll,
        target,
    );
    let heading = target.checked_sub(1).filter(|heading| {
        matches!(items[*heading], AgentListItem::Group(_))
            && layout.heights[*heading] + layout.gaps[*heading] + layout.heights[target]
                <= body.height
    });
    match heading {
        Some(heading) if start == target => heading,
        _ => start,
    }
}

pub(super) fn render_agent_list<T: AgentListRow>(
    buffer: &mut Buffer,
    area: Rect,
    items: &[AgentListItem<T>],
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    mut on_agent: impl FnMut(&mut Buffer, Rect, &T, &mut ShellHitMap),
) {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    hits.agent_body = body;
    if body.is_empty() || items.is_empty() {
        *agent_scroll = 0;
        if let Some(message) = empty_message.filter(|_| !body.is_empty()) {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                message,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        return;
    }

    let layout = layout_agent_list(items, body, *agent_scroll, config);
    let metrics = layout.metrics;
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let mut y = body.y;
    for (index, item) in items.iter().enumerate().skip(*agent_scroll) {
        let height = layout.heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, layout.content_width, height);
        match item {
            AgentListItem::Group(group) => {
                paint_lines(buffer, rect, &layout.lines[index], Style::default());
                hits.agent_groups.push(AgentGroupHit {
                    rect,
                    endpoint_id: group.endpoint_id.clone(),
                    workspace_id: group.workspace_id.clone(),
                });
            }
            AgentListItem::Agent(row) => {
                let style = if row.agent().focused {
                    Style::default().bg(config.palette.active_row_bg)
                } else {
                    Style::default()
                };
                paint_lines(buffer, rect, &layout.lines[index], style);
                on_agent(buffer, rect, row, hits);
            }
        }
        y = y.saturating_add(height).saturating_add(layout.gaps[index]);
    }

    if layout.content_width < body.width {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.agent_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

fn paint_lines(buffer: &mut Buffer, rect: Rect, lines: &[Line<'static>], style: Style) {
    for (offset, line) in lines.iter().take(rect.height as usize).enumerate() {
        Paragraph::new(line.clone()).style(style).render(
            Rect::new(rect.x, rect.y + offset as u16, rect.width, 1),
            buffer,
        );
    }
}

fn agent_group_line(group: &AgentGroup, width: u16, config: &ClientShellConfig) -> Line<'static> {
    let style = Style::default()
        .fg(if group.focused {
            config.palette.text
        } else {
            config.palette.subtext0
        })
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            crate::ui::truncate_end(&group.label, width.saturating_sub(1) as usize),
            style,
        ),
    ])
}

pub(super) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Vec<AgentRow> {
    ordered_agent_pane_ids(snapshot, config.agent_panel_sort)
        .into_iter()
        .filter_map(|pane_id| agent_row(snapshot, &pane_id, config, machine))
        .collect()
}

pub(super) fn agent_row(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Option<AgentRow> {
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
    let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == agent.pane_id);
    let tab_count = snapshot
        .tabs
        .iter()
        .filter(|candidate| candidate.workspace_id == agent.workspace_id)
        .count();
    let tab_label = tab
        .filter(|tab| tab_count > 1 || tab.custom_label)
        .map(|tab| tab.label.as_str());
    let agent_label = agent
        .display_agent
        .as_deref()
        .or(agent.name.as_deref())
        .or(agent.agent.as_deref())
        .or(agent.title.as_deref());
    let labels = agent
        .state_labels
        .iter()
        .cloned()
        .collect::<HashMap<_, _>>();
    let tokens = agent.tokens.iter().cloned().collect::<HashMap<_, _>>();
    let state_text = labels
        .get(status_text(agent.agent_status))
        .map(String::as_str)
        .unwrap_or_else(|| sidebar_status_text(agent.agent_status));
    let canonical_agent = agent
        .agent
        .as_deref()
        .and_then(crate::detect::parse_agent_label);
    let rows = crate::ui::sidebar_agent_rows(
        &config.agents,
        crate::ui::AgentTokenContext {
            machine,
            workspace: &workspace.label,
            tab: tab_label,
            pane: agent
                .title
                .as_deref()
                .or_else(|| pane.and_then(|pane| pane.label.as_deref())),
            agent_label,
            terminal_title: agent.terminal_title.as_deref(),
            terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
            canonical_agent,
            tokens: &tokens,
        },
        state_text,
    );
    Some(AgentRow {
        pane_id: agent.pane_id.clone(),
        status: agent.agent_status,
        focused: agent.focused,
        workspace_id: workspace.workspace_id.clone(),
        workspace_label: workspace.label.clone(),
        workspace_focused: snapshot.focused_workspace_id.as_deref()
            == Some(workspace.workspace_id.as_str()),
        rows,
    })
}

/// The agent's token rows laid out at `width`: text rows wrap onto at most
/// `AGENT_ROW_MAX_LINES` lines, and grouped agents sit one level under their heading.
fn agent_lines(row: &AgentRow, width: u16, config: &ClientShellConfig) -> Vec<Line<'static>> {
    let palette = &config.palette;
    let name_style = Style::default()
        .fg(if row.focused {
            palette.text
        } else {
            palette.subtext0
        })
        .add_modifier(Modifier::BOLD);
    let status_style = Style::default().fg(status_color(row.status, palette));
    let secondary = Style::default().fg(palette.overlay0);
    let icon = (
        status_icon(row.status, config.status_indicators),
        status_style,
    );
    let fallback = [vec![crate::ui::ResolvedToken {
        kind: crate::ui::ResolvedTokenKind::StateIcon,
        style: Default::default(),
    }]];
    let rows = if row.rows.is_empty() {
        &fallback[..]
    } else {
        &row.rows[..]
    };
    let nesting = if agents_grouped(config) { 2 } else { 0 };
    let mut lines = Vec::new();
    for (index, tokens) in rows.iter().enumerate() {
        let indent = nesting + if index == 0 { 1 } else { 3 };
        let available = width.saturating_sub(indent) as usize;
        let spans = |max_width| {
            crate::ui::resolved_token_spans(
                tokens,
                icon,
                status_style,
                name_style,
                secondary,
                secondary,
                palette,
                max_width,
            )
        };
        let wrapped =
            if available >= 2 && tokens.iter().any(|token| token.kind.text_value().is_some()) {
                crate::ui::wrap_token_spans(
                    tokens,
                    &spans(usize::MAX),
                    icon.0,
                    available,
                    AGENT_ROW_MAX_LINES,
                )
            } else {
                vec![spans(available)]
            };
        lines.extend(wrapped.into_iter().map(|spans| {
            Line::from(
                std::iter::once(Span::raw(" ".repeat(indent as usize)))
                    .chain(spans)
                    .collect::<Vec<_>>(),
            )
        }));
    }
    lines
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    for (offset, character) in text.chars().take(width as usize).enumerate() {
        if let Some(cell) = buffer.cell_mut((x + offset as u16, y)) {
            cell.set_char(character).set_style(style);
        }
    }
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn sidebar_status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Working => "working",
        AgentStatus::Idle | AgentStatus::Unknown => "idle",
    }
}
