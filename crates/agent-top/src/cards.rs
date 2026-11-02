//! The card view: one card per agent, packed into columns, each with its recent activity and what it is doing now.

use crate::app::{App, is_active};
use crate::format::{age, cost, cpu_cell, duration_ms, mem_cell, session_name, short_model, tokens_cell, tool_calls, truncate};
use crate::theme::Theme;
use agent_top_core::{Agent, AgentState, SpanKind, ToolSpan};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use std::time::{Duration, SystemTime};

const MIN_CARD_W: u16 = 58;
const TILE_W: usize = 32;
const TILE_H: u16 = 5;
const MAX_TILE_ROWS: usize = 3;
const CHIP_LINES: usize = 2;
pub const ACTIVITY_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_CHILD_LINES: usize = 6;
const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// A card's content: the agent's own lines, then a group of tiles per workflow while any subagent is live.
struct Plan<'a> {
    row: usize,
    head: Vec<Line<'static>>,
    groups: Vec<GroupPlan<'a>>,
}

struct GroupPlan<'a> {
    top: Vec<Line<'static>>,
    tiles: Vec<&'a Agent>,
    /// Tiles carry the agent type for loose subagents; a workflow's phase strip already names it.
    tile_roles: bool,
    chips: Vec<Line<'static>>,
}

impl Plan<'_> {
    fn wide(&self) -> bool {
        !self.groups.is_empty()
    }

    fn height(&self, inner: usize) -> u16 {
        let per_row = tiles_per_row(inner);
        let groups: usize =
            self.groups.iter().map(|g| 1 + g.top.len() + g.tiles.len().div_ceil(per_row) * usize::from(TILE_H) + g.chips.len()).sum();
        (self.head.len() + groups + 2) as u16
    }
}

fn tiles_per_row(inner: usize) -> usize {
    (inner / TILE_W).max(1)
}

struct Placed<'a> {
    plan: Plan<'a>,
    col: usize,
    y: u16,
    height: u16,
}

pub fn draw_cards(f: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    let roots = app.card_roots();
    if roots.is_empty() {
        let hidden = app.hidden_cards();
        let hint = if hidden > 0 { format!("  ·  x shows {hidden} inactive") } else { String::new() };
        let msg = Paragraph::new(vec![
            Line::styled("no agents running", Style::default().fg(theme.dim).bold()),
            Line::styled(format!("cards appear here as agents start{hint}"), Style::default().fg(theme.dim)),
        ])
        .alignment(Alignment::Center);
        f.render_widget(msg, Rect { y: area.y + area.height / 3, height: 2.min(area.height), ..area });
        return;
    }
    let cols = usize::from((area.width / MIN_CARD_W).max(1));
    let col_w = area.width / cols as u16;
    let (narrow, wide) = (usize::from(col_w.saturating_sub(4)), usize::from(area.width.saturating_sub(4)));
    let now = app.snapshot.taken_at;
    let selected = app.root_of(app.selected);

    let mut scroll = app.card_scroll;
    let app_mut = app;
    let app: &App = &*app_mut;
    let (placed, bottom) = {
        let plans: Vec<Plan> = roots.iter().map(|&row| plan(app, row, narrow, wide, now, theme)).collect();
        // Cards with live subagents span the width, above the rest.
        let (wides, narrows): (Vec<Plan>, Vec<Plan>) = plans.into_iter().partition(Plan::wide);
        let mut placed = Vec::new();
        let mut y = 0;
        for plan in wides {
            let height = plan.height(wide);
            placed.push(Placed { plan, col: usize::MAX, y, height });
            y += height;
        }
        let mut heights = vec![y; cols];
        for plan in narrows {
            let height = plan.height(narrow);
            let col = (0..cols).min_by_key(|&c| heights[c]).unwrap_or(0);
            placed.push(Placed { plan, col, y: heights[col], height });
            heights[col] += height;
        }
        (placed, heights.iter().max().copied().unwrap_or(y))
    };

    if let Some(p) = placed.iter().find(|p| p.plan.row == selected) {
        let h = p.height.min(area.height);
        if p.y < scroll {
            scroll = p.y;
        } else if p.y + h > scroll + area.height {
            scroll = p.y + h - area.height;
        }
    }
    scroll = scroll.min(bottom.saturating_sub(area.height));
    let top = scroll;

    let mut hidden = 0;
    for p in &placed {
        if p.y < top || p.y >= top + area.height {
            hidden += 1;
            continue;
        }
        let y = p.y - top;
        if y + p.height > area.height {
            hidden += 1;
        }
        let (x, width) = match p.col {
            usize::MAX => (0, area.width),
            c if c + 1 == cols => (c as u16 * col_w, area.width - c as u16 * col_w),
            c => (c as u16 * col_w, col_w),
        };
        let rect = Rect { x: area.x + x, y: area.y + y, width, height: p.height.min(area.height - y) };
        render_card(f, app, &p.plan, rect, p.plan.row == selected, now, theme);
    }
    if hidden > 0 {
        let label = format!(" ↓ {hidden} more · j/k ");
        let w = label.chars().count() as u16;
        if w < area.width {
            let rect = Rect { x: area.right() - w - 1, y: area.bottom() - 1, width: w, height: 1 };
            f.render_widget(Paragraph::new(Span::styled(label, theme.badge_style(theme.accent))), rect);
        }
    }
    app_mut.card_scroll = scroll;
}

fn plan<'a>(app: &'a App, row: usize, narrow: usize, wide: usize, now: SystemTime, theme: &Theme) -> Plan<'a> {
    let a = &app.rows[row];
    let children: Vec<(&Agent, usize)> =
        (row + 1..app.rows.len()).take_while(|&i| app.row_depths[i] > 0).map(|i| (&app.rows[i], app.row_depths[i])).collect();
    let live = children.iter().any(|(c, _)| is_active(c));
    let width = if live { wide } else { narrow };
    // Children whose usage is not already folded into the card's agent have spans of their own.
    let spans = a.spans.iter().chain(children.iter().filter(|(c, _)| !c.counted_in_parent).flat_map(|(c, _)| c.spans.iter()));
    let mut head = vec![activity_line(spans, open_until(a, now), now, width, theme), doing_line(a, now, false, width, theme)];
    head.extend(recent_line(a, width, theme));
    head.push(stats_line(a, theme));
    if !live {
        head.extend(child_lines(&children, now, width, theme));
        return Plan { row, head, groups: Vec::new() };
    }
    let groups = groups(&children).into_iter().map(|g| group_plan(g, width, theme)).collect();
    Plan { row, head, groups }
}

/// Subagents grouped under the workflow (or other child) that spawned them; loose ones form a group of their own.
struct Group<'a> {
    head: Option<&'a Agent>,
    members: Vec<&'a Agent>,
}

fn groups<'a>(children: &[(&'a Agent, usize)]) -> Vec<Group<'a>> {
    let mut out = Vec::new();
    let mut loose = Vec::new();
    let mut i = 0;
    while i < children.len() {
        let (c, depth) = children[i];
        let below: Vec<&Agent> = children[i + 1..].iter().take_while(|&&(_, d)| d > depth).map(|&(a, _)| a).collect();
        i += 1 + below.len();
        if below.is_empty() {
            loose.push(c);
        } else {
            out.push(Group { head: Some(c), members: below });
        }
    }
    if !loose.is_empty() {
        out.push(Group { head: None, members: loose });
    }
    out
}

fn is_workflow(a: &Agent) -> bool {
    a.subagent.as_ref().and_then(|i| i.role.as_deref()) == Some("workflow")
}

fn group_plan<'a>(g: Group<'a>, width: usize, theme: &Theme) -> GroupPlan<'a> {
    let (live, done): (Vec<&Agent>, Vec<&Agent>) = g.members.iter().partition(|a| is_active(a));
    let workflow = g.head.is_some_and(is_workflow);
    let sep = || Span::styled("  ", Style::default());
    let mut header = vec![Span::styled("◆ ", Style::default().fg(theme.blue))];
    match g.head {
        Some(h) => {
            let name = h.subagent.as_ref().and_then(|i| i.nickname.clone()).unwrap_or_else(|| session_name(h));
            header.push(Span::styled(truncate(&name, width / 2), Style::default().fg(theme.blue).bold()));
            if workflow {
                header.extend([sep(), Span::styled("workflow", Style::default().fg(theme.dim))]);
            }
        }
        None => header.push(Span::styled("subagents", Style::default().fg(theme.blue).bold())),
    }
    if !live.is_empty() {
        header.extend([sep(), Span::styled(format!("● {} running", live.len()), Style::default().fg(theme.green))]);
    }
    if !done.is_empty() {
        header.extend([sep(), Span::styled(format!("✓ {} done", done.len()), Style::default().fg(theme.dim))]);
    }
    if let Some(h) = g.head {
        header.extend([sep(), Span::styled(cost(h), Style::default().fg(theme.mauve))]);
        header.extend([sep(), Span::styled(age(h.age_secs), Style::default().fg(theme.dim))]);
    }
    let mut top = vec![Line::from(header)];
    if workflow {
        top.extend(phase_strip(&g.members, theme));
    }
    let cap = tiles_per_row(width) * MAX_TILE_ROWS;
    let mut chips = Vec::new();
    let mut tiles = live;
    if tiles.len() > cap {
        let more = tiles.len() - (cap - 1);
        tiles.truncate(cap - 1);
        chips.push(Line::styled(format!("● {more} more running"), Style::default().fg(theme.green)));
    }
    chips.extend(chip_lines(&done, width, theme));
    GroupPlan { top, tiles, tile_roles: !workflow, chips }
}

/// A workflow's phases in the order they started, each with how many of its agents run and are done.
fn phase_strip(members: &[&Agent], theme: &Theme) -> Option<Line<'static>> {
    let mut phases: Vec<(String, Option<SystemTime>, usize, usize)> = Vec::new();
    for a in members {
        let Some(role) = a.subagent.as_ref().and_then(|i| i.role.clone()) else { continue };
        let start = a.spans.first().map(|s| s.started_at);
        let live = is_active(a);
        match phases.iter_mut().find(|p| p.0 == role) {
            Some(p) => {
                p.1 = match (p.1, start) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (x, y) => x.or(y),
                };
                if live { p.2 += 1 } else { p.3 += 1 }
            }
            None => phases.push((role, start, usize::from(live), usize::from(!live))),
        }
    }
    if phases.is_empty() {
        return None;
    }
    phases.sort_by_key(|p| (p.1.is_none(), p.1));
    let mut spans = Vec::new();
    for (i, (name, _, running, done)) in phases.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ━━▶ ", Style::default().fg(theme.border)));
        }
        if running > 0 {
            spans.push(Span::styled(format!("● {name} {running}"), Style::default().fg(theme.green).bold()));
            if done > 0 {
                spans.push(Span::styled(format!(" ✓{done}"), Style::default().fg(theme.dim)));
            }
        } else {
            spans.push(Span::styled(format!("✓ {name} {done}"), Style::default().fg(theme.dim)));
        }
    }
    Some(Line::from(spans))
}

fn label(a: &Agent) -> String {
    a.subagent.as_ref().and_then(|i| i.nickname.clone()).unwrap_or_else(|| a.name.clone())
}

/// Finished subagents as small chips, wrapped to `CHIP_LINES` lines.
fn chip_lines(done: &[&Agent], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (i, a) in done.iter().enumerate() {
        let name = truncate(&label(a), 28);
        let price = cost(a);
        let w = name.chars().count() + price.chars().count() + 5;
        let last_line = lines.len() + 1 == CHIP_LINES;
        if used + w + if last_line { 10 } else { 0 } > width && !cur.is_empty() {
            if last_line {
                cur.push(Span::styled(format!("+{} more", done.len() - i), Style::default().fg(theme.dim)));
                lines.push(Line::from(std::mem::take(&mut cur)));
                return lines;
            }
            lines.push(Line::from(std::mem::take(&mut cur)));
            used = 0;
        }
        let (glyph, colour) = if a.state == AgentState::Stopped { ("✓ ", theme.green) } else { ("◌ ", theme.yellow) };
        cur.extend([
            Span::styled(glyph, Style::default().fg(colour)),
            Span::styled(name, Style::default().fg(theme.dim)),
            Span::styled(format!(" {price}   "), Style::default().fg(theme.dim)),
        ]);
        used += w;
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

fn render_card(f: &mut Frame, app: &App, p: &Plan, rect: Rect, selected: bool, now: SystemTime, theme: &Theme) {
    let block = card_block(app, &app.rows[p.row], selected, usize::from(rect.width), theme);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let bottom = inner.bottom();
    let mut y = inner.y;
    let put = |f: &mut Frame, lines: &[Line<'static>], y: &mut u16| {
        let h = (lines.len() as u16).min(bottom.saturating_sub(*y));
        if h > 0 {
            f.render_widget(Paragraph::new(lines.to_vec()), Rect { y: *y, height: h, ..inner });
        }
        *y += lines.len() as u16;
    };
    put(f, &p.head, &mut y);
    let per_row = tiles_per_row(usize::from(inner.width));
    let tile_w = inner.width / per_row as u16;
    for g in &p.groups {
        y += 1;
        put(f, &g.top, &mut y);
        for (i, a) in g.tiles.iter().enumerate() {
            let ty = y + (i / per_row) as u16 * TILE_H;
            if ty + TILE_H <= bottom {
                let r = Rect { x: inner.x + (i % per_row) as u16 * tile_w, y: ty, width: tile_w, height: TILE_H };
                render_tile(f, app, a, r, g.tile_roles, now, theme);
            }
        }
        y += g.tiles.len().div_ceil(per_row) as u16 * TILE_H;
        put(f, &g.chips, &mut y);
    }
}

/// One subagent: its own activity, what it is doing, and what it has cost.
fn render_tile(f: &mut Frame, app: &App, a: &Agent, rect: Rect, role: bool, now: SystemTime, theme: &Theme) {
    let border = if app.is_new(a) {
        theme.mauve
    } else {
        match a.state {
            AgentState::Running => theme.green,
            AgentState::Idle => theme.yellow,
            AgentState::Stopped => theme.track,
        }
    };
    let w = usize::from(rect.width.saturating_sub(2));
    let kind = a.subagent.as_ref().and_then(|i| i.role.clone()).filter(|_| role).map(|r| format!(" {r} ")).unwrap_or_default();
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(state_dot(a.state), Style::default().fg(state_colour(a.state, theme))),
        Span::raw(" "),
        Span::styled(truncate(&label(a), w.saturating_sub(kind.chars().count() + 5).max(6)), Style::default().fg(theme.text).bold()),
        Span::raw(" "),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(title)
        .title(Line::styled(kind, Style::default().fg(theme.dim)).right_aligned());
    let sep = || Span::styled(" · ", Style::default().fg(theme.dim));
    let lines = vec![
        activity_line(a.spans.iter(), open_until(a, now), now, w, theme),
        doing_line(a, now, true, w, theme),
        Line::from(vec![
            Span::styled(format!("{} tok", tokens_cell(a)), Style::default().fg(theme.text)),
            sep(),
            Span::styled(cost(a), Style::default().fg(theme.mauve)),
            sep(),
            Span::styled(format!("{} tools", tool_calls(a)), Style::default().fg(theme.dim)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn card_block(app: &App, a: &Agent, selected: bool, width: usize, theme: &Theme) -> Block<'static> {
    let new = app.is_new(a);
    let border = if selected {
        theme.accent
    } else if new {
        theme.mauve
    } else {
        match a.state {
            AgentState::Running => theme.green,
            AgentState::Idle => theme.border,
            AgentState::Stopped => theme.track,
        }
    };
    let right = format!(" {} · {} ", a.harness.label(), short_model(a.model.as_deref()));
    let tag = if new {
        Some(("new", theme.mauve))
    } else if a.state == AgentState::Stopped {
        Some(("finished", theme.dim))
    } else {
        None
    };
    let room = width.saturating_sub(right.chars().count() + tag.map_or(0, |(t, _)| t.len() + 3) + 8);
    let name_style = if a.state == AgentState::Stopped { Style::default().fg(theme.dim) } else { Style::default().fg(theme.text).bold() };
    let mut title = vec![
        Span::raw(" "),
        Span::styled(state_dot(a.state), Style::default().fg(state_colour(a.state, theme))),
        Span::raw(" "),
        Span::styled(truncate(&session_name(a), room.max(8)), name_style),
        Span::raw(" "),
    ];
    if let Some((t, colour)) = tag {
        title.push(Span::styled(format!(" {t} "), theme.badge_style(colour)));
        title.push(Span::raw(" "));
    }
    Block::bordered()
        .border_type(if selected { BorderType::Thick } else { BorderType::Rounded })
        .border_style(Style::default().fg(border))
        .title(Line::from(title))
        .title(Line::styled(right, Style::default().fg(theme.dim)).right_aligned())
        .padding(Padding::horizontal(1))
}

/// An agent that is not running has nothing in flight, whatever its last unanswered call says.
fn open_until(a: &Agent, now: SystemTime) -> SystemTime {
    match (a.state, a.idle_secs) {
        (AgentState::Running, _) | (_, None) => now,
        (_, Some(idle)) => now - Duration::from_secs(idle),
    }
}

/// Share of each slice of the last `ACTIVITY_WINDOW` spent in a tool call or inference; above 1 when they overlap.
pub fn activity<'a>(spans: impl Iterator<Item = &'a ToolSpan>, open_until: SystemTime, now: SystemTime, n: usize) -> Vec<(f64, bool)> {
    let window = ACTIVITY_WINDOW.as_secs_f64() * 1000.0;
    let bucket = window / n.max(1) as f64;
    let start = now - ACTIVITY_WINDOW;
    let at = |t: SystemTime| match t.duration_since(start) {
        Ok(d) => d.as_secs_f64() * 1000.0,
        Err(e) => -e.duration().as_secs_f64() * 1000.0,
    };
    let mut out = vec![(0.0, false); n];
    for sp in spans.filter(|s| matches!(s.kind, SpanKind::Tool | SpanKind::Inference)) {
        let s = at(sp.started_at);
        let e = match sp.duration_ms {
            Some(d) => s + d as f64,
            None => at(open_until),
        };
        let (s, e) = (s.max(0.0), e.min(window));
        if e <= s {
            continue;
        }
        let last = ((e / bucket).ceil() as usize).min(n);
        for (i, slot) in out.iter_mut().enumerate().take(last).skip((s / bucket) as usize) {
            let b0 = i as f64 * bucket;
            slot.0 += (e.min(b0 + bucket) - s.max(b0)).max(0.0) / bucket;
        }
        if sp.error && last > 0 {
            out[last - 1].1 = true;
        }
    }
    out
}

fn activity_line<'a>(
    spans: impl Iterator<Item = &'a ToolSpan>,
    open_until: SystemTime,
    now: SystemTime,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let cells = activity(spans, open_until, now, width);
    Line::from(
        cells
            .into_iter()
            .map(|(load, error)| {
                if load <= 0.0 {
                    return Span::styled(BARS[0], Style::default().fg(theme.track));
                }
                let level = ((load.min(1.0) * 8.0).ceil() as usize).clamp(1, 8) - 1;
                // Height is how busy a slice was; colour warms as more agents work at once.
                let colour = if error { theme.red } else { theme.ramp_ok.at(((load - 1.0).max(0.0) / 3.0).min(1.0)) };
                Span::styled(BARS[level], Style::default().fg(colour))
            })
            .collect::<Vec<_>>(),
    )
}

fn pretty_tool(name: &str) -> String {
    match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((server, tool)) => format!("{server} › {tool}"),
        None => name.to_string(),
    }
}

/// The newest open span of `kind`, preferring the agent's own thread over its subagents'.
fn open_span(a: &Agent, kind: SpanKind) -> Option<&ToolSpan> {
    let open = || a.spans.iter().rev().filter(move |s| s.kind == kind && s.is_open());
    open().find(|s| !s.sidechain).or_else(|| open().next())
}

fn doing(a: &Agent, now: SystemTime, child: bool, theme: &Theme) -> (&'static str, String, Color) {
    let since = |t: SystemTime| age(now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0));
    let idle = a.idle_secs.map(age);
    match a.state {
        AgentState::Stopped => ("✓", idle.map_or("finished".into(), |i| format!("finished {i} ago")), theme.dim),
        AgentState::Idle if child => ("◌", idle.map_or("quiet".into(), |i| format!("quiet for {i}")), theme.yellow),
        AgentState::Idle => ("◌", idle.map_or("waiting for input".into(), |i| format!("waiting for input · {i}")), theme.yellow),
        AgentState::Running => {
            if let Some(t) = open_span(a, SpanKind::Tool) {
                ("⚙", format!("{} · {}", pretty_tool(&t.name), since(t.started_at)), theme.peach)
            } else if let Some(t) = open_span(a, SpanKind::Inference) {
                ("✦", format!("thinking · {}", since(t.started_at)), theme.mauve)
            } else {
                ("●", "working".into(), theme.green)
            }
        }
    }
}

fn doing_line(a: &Agent, now: SystemTime, child: bool, width: usize, theme: &Theme) -> Line<'static> {
    let (icon, text, colour) = doing(a, now, child, theme);
    Line::from(vec![
        Span::styled(format!("{icon} "), Style::default().fg(colour)),
        Span::styled(truncate(&text, width.saturating_sub(2)), Style::default().fg(colour).add_modifier(Modifier::BOLD)),
    ])
}

/// The agent's own latest finished tool calls, newest first, as many as fit.
fn recent_line(a: &Agent, width: usize, theme: &Theme) -> Option<Line<'static>> {
    let done = |own: bool| a.spans.iter().rev().filter(move |s| s.kind == SpanKind::Tool && !s.is_open() && (!own || !s.sidechain));
    let own = done(true).next().is_some();
    let mut spans = vec![Span::styled("recent ", Style::default().fg(theme.dim))];
    let mut used = 7;
    for s in done(own) {
        let name = truncate(&pretty_tool(&s.name), 24);
        let took = s.duration_ms.map(duration_ms).unwrap_or_default();
        let w = name.chars().count() + took.chars().count() + 5;
        if used + w > width {
            break;
        }
        used += w;
        spans.push(Span::styled(name, Style::default().fg(theme.text)));
        spans.push(Span::styled(format!(" {took} "), Style::default().fg(theme.dim)));
        spans.push(if s.error {
            Span::styled("✗", Style::default().fg(theme.red))
        } else {
            Span::styled("✓", Style::default().fg(theme.green))
        });
        spans.push(Span::raw("  "));
    }
    (spans.len() > 1).then(|| Line::from(spans))
}

fn stats_line(a: &Agent, theme: &Theme) -> Line<'static> {
    let sep = || Span::styled(" · ", Style::default().fg(theme.dim));
    let mut spans = vec![
        Span::styled(format!("{} tok", tokens_cell(a)), Style::default().fg(theme.text)),
        sep(),
        Span::styled(cost(a), Style::default().fg(theme.mauve)),
        sep(),
        Span::styled(format!("{} tools", tool_calls(a)), Style::default().fg(theme.text)),
    ];
    if a.pid.is_some() && !a.shares_process {
        spans.extend([sep(), Span::styled(format!("cpu {}%", cpu_cell(a)), Style::default().fg(theme.dim))]);
        spans.extend([sep(), Span::styled(mem_cell(a), Style::default().fg(theme.dim))]);
    }
    spans.extend([sep(), Span::styled(age(a.age_secs), Style::default().fg(theme.dim))]);
    Line::from(spans)
}

/// Workflows with their running and done counts, each running subagent's current work, and the finished count.
fn child_lines(children: &[(&Agent, usize)], now: SystemTime, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut finished = 0;
    for (i, &(c, depth)) in children.iter().enumerate() {
        let indent = "  ".repeat(depth.saturating_sub(1));
        let below: Vec<&Agent> = children[i + 1..].iter().take_while(|&&(_, d)| d > depth).map(|&(a, _)| a).collect();
        if !below.is_empty() {
            let running = below.iter().filter(|a| is_active(a)).count();
            let mut tail = Vec::new();
            if running > 0 {
                tail.push(Span::styled(format!("  {running} running"), Style::default().fg(theme.green)));
            }
            if below.len() > running {
                tail.push(Span::styled(format!("  {} done", below.len() - running), Style::default().fg(theme.dim)));
            }
            tail.push(Span::styled(format!("  {}", cost(c)), Style::default().fg(theme.mauve)));
            let room = width.saturating_sub(indent.len() + 4 + tail.iter().map(|t| t.content.chars().count()).sum::<usize>());
            let mut spans = vec![
                Span::styled(format!("{indent}↳ "), Style::default().fg(theme.dim)),
                Span::styled(format!("{} ", state_dot(c.state)), Style::default().fg(state_colour(c.state, theme))),
                Span::styled(truncate(&session_name(c), room.max(8)), Style::default().fg(theme.blue).bold()),
            ];
            spans.extend(tail);
            out.push(Line::from(spans));
        } else if is_active(c) {
            let name = truncate(&session_name(c), (width / 2).max(12));
            let (icon, text, colour) = doing(c, now, true, theme);
            let room = width.saturating_sub(indent.len() + name.chars().count() + 7);
            out.push(Line::from(vec![
                Span::styled(format!("{indent}↳ "), Style::default().fg(theme.dim)),
                Span::styled(format!("{} ", state_dot(c.state)), Style::default().fg(state_colour(c.state, theme))),
                Span::styled(name, Style::default().fg(theme.text)),
                Span::styled(format!("  {icon} "), Style::default().fg(colour)),
                Span::styled(truncate(&text, room), Style::default().fg(colour)),
            ]));
        } else if depth == 1 {
            finished += 1;
        }
    }
    if out.len() > MAX_CHILD_LINES {
        let more = out.len() - (MAX_CHILD_LINES - 1);
        out.truncate(MAX_CHILD_LINES - 1);
        out.push(Line::styled(format!("  + {more} more"), Style::default().fg(theme.dim)));
    }
    if finished > 0 {
        let noun = if finished == 1 { "subagent" } else { "subagents" };
        out.push(Line::styled(format!("↳ ✓ {finished} {noun} finished"), Style::default().fg(theme.dim)));
    }
    out
}

fn state_dot(s: AgentState) -> &'static str {
    match s {
        AgentState::Running => "●",
        AgentState::Idle => "◌",
        AgentState::Stopped => "✓",
    }
}

fn state_colour(s: AgentState, theme: &Theme) -> Color {
    match s {
        AgentState::Running => theme.green,
        AgentState::Idle => theme.yellow,
        AgentState::Stopped => theme.dim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_s: u64, dur_ms: Option<u64>, kind: SpanKind, error: bool) -> ToolSpan {
        ToolSpan {
            id: format!("{start_s}"),
            name: "Bash".into(),
            started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(start_s),
            duration_ms: dur_ms,
            sidechain: false,
            error,
            kind,
        }
    }

    #[test]
    fn activity_is_the_busy_share_of_each_slice() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        // Ten 60-second slices from 400s to 1000s.
        let spans = [
            span(700, Some(300_000), SpanKind::Tool, false),
            span(700, Some(30_000), SpanKind::Inference, true),
            span(400, None, SpanKind::Turn, false),
        ];
        let cells = activity(spans.iter(), now, now, 10);
        assert!(cells[..5].iter().all(|&(load, _)| load == 0.0), "{cells:?}");
        assert!((cells[5].0 - 1.5).abs() < 1e-9, "two calls overlap for half a slice: {cells:?}");
        assert!(cells[5].1, "the failed call marks the slice it ended in");
        assert!(cells[6..].iter().all(|&(load, error)| (load - 1.0).abs() < 1e-9 && !error), "{cells:?}");

        let open = [span(880, None, SpanKind::Tool, false)];
        let cells = activity(open.iter(), now - Duration::from_secs(60), now, 10);
        assert_eq!(cells.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn mcp_tools_read_as_server_and_tool() {
        assert_eq!(pretty_tool("mcp__t3-code__delegate_task"), "t3-code › delegate_task");
        assert_eq!(pretty_tool("Bash"), "Bash");
    }
}
