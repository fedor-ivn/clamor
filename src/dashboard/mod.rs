mod input;
pub(crate) mod keys;
pub(crate) mod render;
pub mod shortcuts;

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    EventStream,
};
use ratatui::crossterm::execute;
use ratatui::Terminal;

use crate::agent::{generate_id, next_color_index, Agent, AgentState};
use crate::client::DaemonClient;
use crate::config::{resolve_path, ClamorConfig};
use crate::daemon;
use crate::pane::{self, PaneView};
use crate::protocol::DaemonMessage;
use crate::state::{
    cycle_backend_for_folder, selected_backend_for_folder, with_state, ClamorState,
    PromptHistoryEntry,
};
use crate::watcher::StateSource;

use input::{DashboardAction, FolderPickReason, InputMode, PromptEdit, PromptField};

fn apply_edit(s: &mut String, edit: &PromptEdit) {
    match edit {
        PromptEdit::Char(c) => s.push(*c),
        PromptEdit::Paste(text) => s.push_str(text),
        PromptEdit::Backspace => {
            s.pop();
        }
        PromptEdit::DeleteWord => {
            let trimmed = s.trim_end_matches(|c: char| !c.is_alphanumeric());
            let len_after_spaces = trimmed.len();
            let word_trimmed = trimmed.trim_end_matches(|c: char| c.is_alphanumeric());
            let len_after_word = word_trimmed.len();
            if len_after_spaces == s.len() && len_after_word == len_after_spaces {
                s.pop();
            } else {
                s.truncate(len_after_word);
            }
        }
        PromptEdit::DeleteLine => s.clear(),
        PromptEdit::HistoryPrev | PromptEdit::HistoryNext => {}
    }
}

enum AppMode {
    Dashboard,
    Terminal { agent_id: String },
}

fn ensure_daemon() -> Result<()> {
    if !daemon::is_daemon_running() {
        daemon::start_daemon_background()?;
    }
    Ok(())
}

async fn reconcile_state(
    config: &ClamorConfig,
    client: &mut DaemonClient,
    pty_rows: u16,
    pty_cols: u16,
) -> Result<()> {
    let daemon_agents = client.list_agents().await?;
    let daemon_ids: std::collections::HashSet<String> =
        daemon_agents.iter().map(|a| a.id.clone()).collect();

    let state = ClamorState::load()?;
    type ResumeEntry = (String, String, Vec<String>, Vec<(String, String)>);
    let mut to_resume: Vec<ResumeEntry> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();

    for (id, agent) in &state.agents {
        if !daemon_ids.contains(id) {
            match reconcile_resume_action(config, id, agent) {
                ResumeReconcileAction::Resume { cwd, cmd, env } => {
                    to_resume.push((id.clone(), cwd, cmd, env));
                }
                ResumeReconcileAction::Remove => {
                    to_remove.push(id.clone());
                }
            }
        }
    }

    // Resume agents with session IDs
    for (id, cwd, cmd, env) in &to_resume {
        let _ = client
            .spawn_agent(id, cwd, cmd, env, pty_rows, pty_cols)
            .await;
    }

    // Update state: mark resumed agents as Working, remove non-resumable ones
    if !to_resume.is_empty() || !to_remove.is_empty() {
        with_state(|state| {
            for (id, _, _, _) in &to_resume {
                if let Some(agent) = state.agents.get_mut(id) {
                    agent.state = AgentState::Input;
                    agent.last_activity_at = chrono::Utc::now();
                }
            }
            for id in &to_remove {
                state.agents.remove(id);
            }
        })?;
    }

    Ok(())
}

enum ResumeReconcileAction {
    Resume {
        cwd: String,
        cmd: Vec<String>,
        env: Vec<(String, String)>,
    },
    Remove,
}

fn reconcile_resume_action(
    config: &ClamorConfig,
    agent_id: &str,
    agent: &Agent,
) -> ResumeReconcileAction {
    let folder_path = config
        .folder_path(&agent.folder_id)
        .unwrap_or(agent.cwd.as_str());
    match crate::spawn::resolve_resume_launch(
        config,
        &agent.backend_id,
        &agent.folder_id,
        folder_path,
        &agent.cwd,
        &agent.title,
        agent.resume_token.as_deref(),
    ) {
        Ok(launch) => {
            let mut env = launch.env;
            env.push(("CLAMOR_AGENT_ID".to_string(), agent_id.to_string()));
            ResumeReconcileAction::Resume {
                cwd: agent.cwd.clone(),
                cmd: launch.cmd,
                env,
            }
        }
        Err(_) => ResumeReconcileAction::Remove,
    }
}

pub async fn run(config: &ClamorConfig, attach_to: Option<String>) -> Result<()> {
    ensure_daemon()?;
    with_state(|state| crate::state::reconcile_folder_backend_selections(config, state))?;
    let mut client = DaemonClient::connect().await?;

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    reconcile_state(config, &mut client, term_rows, term_cols).await?;

    let state_source = StateSource::new(config);

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;

    let has_keyboard_enhancement =
        crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if has_keyboard_enhancement {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }

    let result = main_loop(&mut terminal, config, &mut client, attach_to, &state_source).await;

    if has_keyboard_enhancement {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)?;
    }
    execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture)?;
    restore_terminal(&mut terminal)?;

    result
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original(info);
    }));
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    io::stdout()
        .execute(EnterAlternateScreen)
        .context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend).context("Failed to create terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;
    terminal
        .backend_mut()
        .execute(LeaveAlternateScreen)
        .context("Failed to leave alternate screen")?;
    terminal.show_cursor().context("Failed to show cursor")?;
    Ok(())
}

async fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &ClamorConfig,
    client: &mut DaemonClient,
    attach_to: Option<String>,
    state_source: &StateSource,
) -> Result<()> {
    let mut input_mode = InputMode::Normal;
    let mut killed_at: HashMap<String, Instant> = HashMap::new();
    let kill_linger = Duration::from_secs(3);
    let mut pane_views: HashMap<String, PaneView> = HashMap::new();
    let mut last_agent_id: Option<String> = None;
    let mut prompt_draft: Option<(String, String, String, String)> = None;
    let mut selected_index: Option<usize> = None;
    let mut history_index: Option<usize> = None;
    let mut history_stash: Option<(String, String)> = None;
    let mut filter_query = String::new();
    let mut selected_agents: HashSet<String> = HashSet::new();
    let mut daemon_connected = true;
    let mut pending_g = false;
    let mut flash: Option<(String, Instant)> = None;

    let mut mode = if let Some(ref agent_id) = attach_to {
        let state = state_source.get();
        if !state.agents.contains_key(agent_id) {
            AppMode::Dashboard
        } else {
            let (term_cols, term_rows) = crossterm::terminal::size()?;
            let content_rows = term_rows.saturating_sub(1);
            let _ = client.resize(agent_id, content_rows, term_cols).await;
            match client.subscribe(agent_id).await {
                Ok(catch_up) => {
                    let pv = if catch_up.is_empty() {
                        PaneView::new(content_rows, term_cols)
                    } else {
                        PaneView::from_catch_up(content_rows, term_cols, &catch_up)
                    };
                    pane_views.insert(agent_id.clone(), pv);
                    AppMode::Terminal {
                        agent_id: agent_id.clone(),
                    }
                }
                Err(_) => AppMode::Dashboard,
            }
        }
    } else {
        AppMode::Dashboard
    };

    let sorted_folders: Vec<(String, String)> = config.ordered_folders();

    let mut event_stream = EventStream::new();
    let mut frame_interval = tokio::time::interval(Duration::from_millis(16));
    let mut needs_render = true;

    loop {
        tokio::select! {
            msg_result = client.recv() => {
                match msg_result {
                    Ok(msg) => {
                        daemon_connected = true;
                        match msg {
                            DaemonMessage::Output { id, data } => {
                                if let Some(pv) = pane_views.get_mut(&id) {
                                    pv.process_output(&data);
                                }
                            }
                            DaemonMessage::Exited { id } => {
                                let _ = with_state(|state| {
                                    if let Some(agent) = state.agents.get_mut(&id) {
                                        agent.state = AgentState::Done;
                                    }
                                });
                                state_source.invalidate();
                            }
                            DaemonMessage::CatchUp { id, data } => {
                                if let Some(pv) = pane_views.get_mut(&id) {
                                    pv.process_output(&data);
                                }
                            }
                            DaemonMessage::Heartbeat => {
                                let _ = client.pong().await;
                            }
                            _ => {}
                        }
                        needs_render = true;
                    }
                    Err(_) => {
                        daemon_connected = false;
                        if let Ok(new_client) = DaemonClient::connect().await {
                            *client = new_client;
                            daemon_connected = true;
                        }
                    }
                }
            }

            event_result = event_stream.next() => {
                if let Some(Ok(ev)) = event_result {
                    let action = match mode {
                        AppMode::Dashboard => {
                            handle_dashboard_event(
                                &ev,
                                terminal,
                                client,
                                config,
                                &mut input_mode,
                                &mut killed_at,
                                &sorted_folders,
                                &last_agent_id,
                                state_source,
                                &mut prompt_draft,
                                &mut selected_index,
                                &mut filter_query,
                                &mut history_index,
                                &mut history_stash,
                                &mut selected_agents,
                                &mut pending_g,
                                &mut flash,
                            ).await?
                        }
                        AppMode::Terminal { ref agent_id } => {
                            handle_terminal_event(
                                &ev,
                                terminal,
                                client,
                                agent_id,
                                &mut pane_views,
                            ).await?
                        }
                    };

                    match action {
                        LoopAction::Quit => break,
                        LoopAction::SwitchToTerminal(agent_id) => {
                            let (term_cols, term_rows) = crossterm::terminal::size()?;
                            let content_rows = term_rows.saturating_sub(1);

                            let has_existing = pane_views.contains_key(&agent_id);

                            // Use buffered variants so in-flight Output messages
                            // aren't silently discarded (causes parser state drift)
                            let resize_msgs = client
                                .resize_buffered(&agent_id, content_rows, term_cols)
                                .await
                                .unwrap_or_default();

                            // On re-attach, rebuild daemon parser to fix rendering drift.
                    // On first attach, just subscribe normally.
                    let result = if has_existing {
                        client.refresh_parser_buffered(&agent_id).await
                    } else {
                        client.subscribe_buffered(&agent_id).await
                    };

                    match result {
                                Ok(result) => {
                                    let pv = if result.catch_up.is_empty() {
                                        PaneView::new(content_rows, term_cols)
                                    } else {
                                        PaneView::from_catch_up(
                                            content_rows,
                                            term_cols,
                                            &result.catch_up,
                                        )
                                    };
                                    pane_views.insert(agent_id.clone(), pv);
                                    for msg in resize_msgs.into_iter().chain(result.buffered) {
                                        apply_daemon_message(
                                            &msg,
                                            &mut pane_views,
                                            state_source,
                                        );
                                    }
                                }
                                Err(_) => continue,
                            }

                            terminal.clear()?;
                            mode = AppMode::Terminal { agent_id };
                        }
                        LoopAction::SwitchToDashboard => {
                            if let AppMode::Terminal { ref agent_id } = mode {
                                last_agent_id = Some(agent_id.clone());
                            }
                            mode = AppMode::Dashboard;
                            input_mode = InputMode::Normal;
                        }
                        LoopAction::JumpInput(forward) => {
                            let state = state_source.get();
                            let agent_refs: HashMap<String, &Agent> = state
                                .agents
                                .iter()
                                .map(|(id, a)| (id.clone(), a))
                                .collect();
                            let ordered =
                                render::ordered_agent_ids(config, &agent_refs, &filter_query);
                            let current = match mode {
                                AppMode::Terminal { ref agent_id } => Some(agent_id.as_str()),
                                AppMode::Dashboard => selected_index
                                    .and_then(|i| ordered.get(i))
                                    .map(|s| s.as_str()),
                            };
                            match pick_jump_target(&ordered, &agent_refs, current, forward) {
                                Some(target) => match mode {
                                    AppMode::Dashboard => {
                                        if let Some(pos) =
                                            ordered.iter().position(|id| *id == target)
                                        {
                                            selected_index = Some(pos);
                                        }
                                    }
                                    AppMode::Terminal { .. } => {
                                        if Some(target.as_str()) != current {
                                            // Reuse the existing switch pipeline to resize/subscribe.
                                            // Goto top of the loop body via a synthesized action.
                                            // Easiest: recursively handle as SwitchToTerminal.
                                            // We can't call it directly; set last_agent_id and
                                            // push the switch through the normal flow.
                                            // Instead, duplicate the SwitchToTerminal logic:
                                            let (term_cols, term_rows) =
                                                crossterm::terminal::size()?;
                                            let content_rows = term_rows.saturating_sub(1);
                                            let has_existing = pane_views.contains_key(&target);
                                            let resize_msgs = client
                                                .resize_buffered(
                                                    &target,
                                                    content_rows,
                                                    term_cols,
                                                )
                                                .await
                                                .unwrap_or_default();
                                            let result = if has_existing {
                                                client.refresh_parser_buffered(&target).await
                                            } else {
                                                client.subscribe_buffered(&target).await
                                            };
                                            if let Ok(result) = result {
                                                let pv = if result.catch_up.is_empty() {
                                                    PaneView::new(content_rows, term_cols)
                                                } else {
                                                    PaneView::from_catch_up(
                                                        content_rows,
                                                        term_cols,
                                                        &result.catch_up,
                                                    )
                                                };
                                                pane_views.insert(target.clone(), pv);
                                                for msg in resize_msgs
                                                    .into_iter()
                                                    .chain(result.buffered)
                                                {
                                                    apply_daemon_message(
                                                        &msg,
                                                        &mut pane_views,
                                                        state_source,
                                                    );
                                                }
                                                terminal.clear()?;
                                                mode = AppMode::Terminal { agent_id: target };
                                            }
                                        }
                                    }
                                },
                                None => {
                                    set_flash(&mut flash, "no agents needing input");
                                }
                            }
                        }
                        LoopAction::Continue => {}
                    }
                    needs_render = true;
                }
            }

            _ = frame_interval.tick() => {
                // Expire killed agents
                let expired: Vec<String> = killed_at
                    .iter()
                    .filter(|(_, t)| t.elapsed() > kill_linger)
                    .map(|(id, _)| id.clone())
                    .collect();
                if !expired.is_empty() {
                    let _ = with_state(|state| {
                        for id in &expired {
                            state.agents.remove(id);
                        }
                    });
                    state_source.invalidate();
                    for id in &expired {
                        killed_at.remove(id);
                    }
                    needs_render = true;
                }

                if flash
                    .as_ref()
                    .is_some_and(|(_, until)| *until <= Instant::now())
                {
                    flash = None;
                    needs_render = true;
                }

                if needs_render {
                    let flash_text = active_flash(&flash).map(|s| s.to_string());
                    match mode {
                        AppMode::Dashboard => {
                            render_dashboard(
                                terminal,
                                config,
                                &input_mode,
                                &killed_at,
                                &sorted_folders,
                                state_source,
                                selected_index,
                                &filter_query,
                                &selected_agents,
                                daemon_connected,
                                flash_text.as_deref(),
                            )?;
                        }
                        AppMode::Terminal { ref agent_id } => {
                            let state = state_source.get();
                            if !state.agents.contains_key(agent_id) {
                                let _ = client.unsubscribe(agent_id).await;
                                pane_views.remove(agent_id);
                                last_agent_id = Some(agent_id.clone());
                                mode = AppMode::Dashboard;
                                input_mode = InputMode::Normal;
                                needs_render = true;
                                continue;
                            }
                            render_terminal_view(
                                terminal,
                                agent_id,
                                &mut pane_views,
                                state_source,
                                flash_text.as_deref(),
                            )?;
                        }
                    }
                    if flash.is_some() {
                        // Re-render next tick so the flash can expire visually.
                        needs_render = true;
                    } else {
                        needs_render = false;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Apply a buffered daemon message (Output/Exited) to the appropriate pane view.
/// Used to replay messages that were in-flight during resize/subscribe calls.
fn apply_daemon_message(
    msg: &DaemonMessage,
    pane_views: &mut HashMap<String, PaneView>,
    state_source: &StateSource,
) {
    match msg {
        DaemonMessage::Output { id, data } => {
            if let Some(pv) = pane_views.get_mut(id) {
                pv.process_output(data);
            }
        }
        DaemonMessage::Exited { id } => {
            let _ = with_state(|state| {
                if let Some(agent) = state.agents.get_mut(id) {
                    agent.state = AgentState::Done;
                }
            });
            state_source.invalidate();
        }
        _ => {}
    }
}

enum LoopAction {
    Continue,
    Quit,
    SwitchToTerminal(String),
    SwitchToDashboard,
    JumpInput(bool),
}

/// Pick the next/prev agent in `Input` state relative to `from_id`,
/// cycling through `ordered_ids` (dashboard display order). Returns `None`
/// if no agent is currently in `Input`.
fn pick_jump_target(
    ordered_ids: &[String],
    agents: &HashMap<String, &Agent>,
    from_id: Option<&str>,
    forward: bool,
) -> Option<String> {
    if ordered_ids.is_empty() {
        return None;
    }

    let candidates: Vec<&String> = ordered_ids
        .iter()
        .filter(|id| {
            agents
                .get(id.as_str())
                .is_some_and(|a| a.state == AgentState::Input)
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let from_pos = from_id
        .and_then(|id| ordered_ids.iter().position(|o| o == id))
        .unwrap_or(0);

    if forward {
        for id in candidates.iter() {
            let pos = ordered_ids.iter().position(|o| o == *id).unwrap();
            if pos > from_pos {
                return Some((*id).clone());
            }
        }
        candidates.first().map(|s| (*s).clone())
    } else {
        for id in candidates.iter().rev() {
            let pos = ordered_ids.iter().position(|o| o == *id).unwrap();
            if pos < from_pos {
                return Some((*id).clone());
            }
        }
        candidates.last().map(|s| (*s).clone())
    }
}

/// Assign keys from `keys::key_pool()` to `ordered_ids` in order, returning
/// `(agent_id, key)` pairs. Stops when the pool is exhausted; any remaining
/// agents get no key.
fn rebind_assignments(ordered_ids: &[String]) -> Vec<(String, char)> {
    keys::key_pool()
        .iter()
        .copied()
        .zip(ordered_ids.iter().cloned())
        .map(|(key, id)| (id, key))
        .collect()
}

const FLASH_TTL: Duration = Duration::from_millis(2000);

fn set_flash(flash: &mut Option<(String, Instant)>, msg: &str) {
    *flash = Some((msg.to_string(), Instant::now() + FLASH_TTL));
}

fn active_flash(flash: &Option<(String, Instant)>) -> Option<&str> {
    flash
        .as_ref()
        .filter(|(_, until)| *until > Instant::now())
        .map(|(msg, _)| msg.as_str())
}

#[allow(clippy::too_many_arguments)]
fn render_dashboard(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &ClamorConfig,
    input_mode: &InputMode,
    killed_at: &HashMap<String, Instant>,
    sorted_folders: &[(String, String)],
    state_source: &StateSource,
    selected_index: Option<usize>,
    filter_query: &str,
    selected_agents: &HashSet<String>,
    daemon_connected: bool,
    flash: Option<&str>,
) -> Result<()> {
    let state = state_source.get();
    let killed_ids: Vec<String> = killed_at.keys().cloned().collect();
    let folder_backend_labels: HashMap<String, String> = config
        .folders
        .keys()
        .filter_map(|folder_id| {
            folder_backend_label(config, &state, folder_id).map(|label| (folder_id.clone(), label))
        })
        .collect();

    let agent_refs: HashMap<String, &Agent> =
        state.agents.iter().map(|(id, a)| (id.clone(), a)).collect();

    let overlay = build_overlay(
        config,
        &state,
        input_mode,
        sorted_folders,
        filter_query,
        selected_agents,
    );

    terminal.draw(|frame| {
        render::render(
            frame,
            config,
            &folder_backend_labels,
            &agent_refs,
            &killed_ids,
            &overlay,
            selected_index,
            filter_query,
            selected_agents,
            daemon_connected,
            flash,
        );
    })?;

    Ok(())
}

fn folder_backend_label(
    config: &ClamorConfig,
    state: &ClamorState,
    folder_id: &str,
) -> Option<String> {
    selected_backend_for_folder(config, state, folder_id)
        .map(|backend_id| config.backend_display_name(&backend_id).to_string())
}

fn build_overlay<'a>(
    config: &ClamorConfig,
    state: &ClamorState,
    input_mode: &'a InputMode,
    sorted_folders: &'a [(String, String)],
    filter_query: &'a str,
    selected_agents: &HashSet<String>,
) -> render::Overlay<'a> {
    match input_mode {
        InputMode::Normal => {
            if !filter_query.is_empty() {
                render::Overlay::FilterActive {
                    query: filter_query,
                }
            } else {
                render::Overlay::None
            }
        }
        InputMode::WaitingKill => render::Overlay::PendingKill,
        InputMode::WaitingReload => render::Overlay::PendingReload,
        InputMode::ConfirmReload {
            agent_id, title, ..
        } => render::Overlay::ConfirmReload {
            agent_id,
            description: title,
        },
        InputMode::PickingFolder { .. } => render::Overlay::FolderPicker {
            folders: sorted_folders,
        },
        InputMode::TypingPrompt {
            folder_name,
            title,
            description,
            active_field,
            ..
        } => {
            let selected_id = selected_backend_for_folder(config, state, folder_name);
            let backends: Vec<(String, String, bool)> = config
                .folder_backends(folder_name)
                .unwrap_or(&[])
                .iter()
                .filter(|id| config.backends.contains_key(id.as_str()))
                .map(|id| {
                    let label = config.backend_display_name(id).to_string();
                    let selected = selected_id.as_deref() == Some(id.as_str());
                    (id.clone(), label, selected)
                })
                .collect();
            render::Overlay::PromptInput {
                folder_name,
                backends,
                title,
                description,
                active_field,
            }
        }
        InputMode::TypingAdopt { input, .. } => render::Overlay::AdoptInput { input },
        InputMode::ConfirmEmptySpawn { .. } => render::Overlay::ConfirmEmptySpawn,
        InputMode::ConfirmKill {
            agent_id, title, ..
        } => render::Overlay::ConfirmKill {
            agent_id,
            description: title,
        },
        InputMode::ConfirmBatchKill => render::Overlay::ConfirmBatchKill {
            count: selected_agents.len(),
        },
        InputMode::ConfirmRebind => render::Overlay::ConfirmRebind {
            count: state.agents.len(),
        },
        InputMode::ReloadUnavailable { ref reason } => {
            render::Overlay::ReloadUnavailable { reason }
        }
        InputMode::ActionFailed { ref reason } => render::Overlay::ActionFailed { reason },
        InputMode::QuitHint => render::Overlay::QuitHint,
        InputMode::WaitingEdit => render::Overlay::PendingEdit,
        InputMode::EditingDescription { input, .. } => render::Overlay::EditInput { input },
        InputMode::Filtering { query } => render::Overlay::FilterInput { query },
        InputMode::Help {
            scroll,
            filter,
            filtering,
        } => render::Overlay::Help {
            scroll: *scroll,
            filter,
            filtering: *filtering,
        },
    }
}

fn render_terminal_view(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent_id: &str,
    pane_views: &mut HashMap<String, PaneView>,
    state_source: &StateSource,
    flash: Option<&str>,
) -> Result<()> {
    let state = state_source.get();
    let agent = match state.agents.get(agent_id) {
        Some(a) => a,
        None => return Ok(()),
    };

    if let Some(pv) = pane_views.get_mut(agent_id) {
        let sel = pv.selection.clone();
        let scroll_offset = pv.scroll_offset;
        let has_pending = pv.has_pending_output();
        let copy_cursor = pv
            .copy_mode
            .as_ref()
            .map(|cm| (cm.cursor_col, cm.cursor_row));
        let scroll_info = if scroll_offset > 0 {
            let scrollback_total = pv.scrollback_len();
            Some((scroll_offset, scrollback_total))
        } else {
            None
        };
        let screen = pv.scrolled_screen();
        terminal.draw(|frame| {
            render::render_terminal(
                frame,
                screen,
                agent,
                &sel,
                scroll_info,
                has_pending,
                copy_cursor,
                flash,
            );
        })?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_dashboard_event(
    ev: &Event,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &mut DaemonClient,
    config: &ClamorConfig,
    input_mode: &mut InputMode,
    killed_at: &mut HashMap<String, Instant>,
    sorted_folders: &[(String, String)],
    last_agent_id: &Option<String>,
    state_source: &StateSource,
    prompt_draft: &mut Option<(String, String, String, String)>,
    selected_index: &mut Option<usize>,
    filter_query: &mut String,
    history_index: &mut Option<usize>,
    history_stash: &mut Option<(String, String)>,
    selected_agents: &mut HashSet<String>,
    pending_g: &mut bool,
    _flash: &mut Option<(String, Instant)>,
) -> Result<LoopAction> {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let pty_rows = term_rows.saturating_sub(1);
    let pty_cols = term_cols;

    let state = state_source.get();

    let mut key_map: HashMap<char, String> = HashMap::new();
    for (id, agent) in &state.agents {
        if let Some(k) = agent.key {
            key_map.insert(k, id.clone());
        }
    }

    // Build agent refs for ordered_agent_ids and clamp selection
    let agent_refs: HashMap<String, &Agent> =
        state.agents.iter().map(|(id, a)| (id.clone(), a)).collect();
    {
        let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
        if let Some(idx) = *selected_index {
            if agent_ids.is_empty() {
                *selected_index = None;
            } else if idx >= agent_ids.len() {
                *selected_index = Some(agent_ids.len() - 1);
            }
        }
    }

    if let Event::Paste(text) = ev {
        let action = match input_mode {
            InputMode::TypingPrompt { .. } => {
                DashboardAction::PromptInput(PromptEdit::Paste(text.clone()))
            }
            InputMode::TypingAdopt { .. } => {
                DashboardAction::AdoptInput(PromptEdit::Paste(text.clone()))
            }
            InputMode::EditingDescription { .. } => {
                DashboardAction::EditInput(PromptEdit::Paste(text.clone()))
            }
            InputMode::Filtering { .. } => {
                DashboardAction::FilterInput(PromptEdit::Paste(text.clone()))
            }
            _ => DashboardAction::Refresh,
        };
        match action {
            DashboardAction::PromptInput(edit) => {
                *history_index = None;
                *history_stash = None;
                if let InputMode::TypingPrompt {
                    ref mut title,
                    ref mut description,
                    ref active_field,
                    ..
                } = input_mode
                {
                    let target = match active_field {
                        PromptField::Title => Some(title),
                        PromptField::Description => Some(description),
                        PromptField::Backend => None,
                    };
                    if let Some(target) = target {
                        apply_edit(target, &edit);
                    }
                }
            }
            DashboardAction::AdoptInput(edit) => {
                if let InputMode::TypingAdopt { ref mut input, .. } = input_mode {
                    apply_edit(input, &edit);
                }
            }
            DashboardAction::EditInput(edit) => {
                if let InputMode::EditingDescription { ref mut input, .. } = input_mode {
                    apply_edit(input, &edit);
                }
            }
            DashboardAction::FilterInput(edit) => {
                if let InputMode::Filtering { ref mut query } = input_mode {
                    apply_edit(query, &edit);
                    *filter_query = query.clone();
                    *selected_index = Some(0);
                    selected_agents.clear();
                }
            }
            _ => {}
        }
        return Ok(LoopAction::Continue);
    }

    if let Event::Key(key_event) = ev {
        if key_event.kind != KeyEventKind::Press {
            return Ok(LoopAction::Continue);
        }

        // Ctrl+F: toggle back to last attached agent
        if matches!(input_mode, InputMode::Normal)
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
            && key_event.code == KeyCode::Char('f')
        {
            if let Some(ref id) = last_agent_id {
                if state.agents.contains_key(id) {
                    return Ok(LoopAction::SwitchToTerminal(id.clone()));
                }
            }
        }

        // Esc in Normal mode clears active filter and batch selection
        if matches!(input_mode, InputMode::Normal)
            && key_event.code == KeyCode::Esc
            && (!filter_query.is_empty() || !selected_agents.is_empty())
        {
            filter_query.clear();
            *selected_index = None;
            selected_agents.clear();
            return Ok(LoopAction::Continue);
        }

        let action = input::handle_input(*key_event, &key_map, input_mode);

        // Handle gg (pending g -> g = SelectFirst)
        let action = if *pending_g {
            *pending_g = false;
            match action {
                DashboardAction::PendingG => DashboardAction::SelectFirst,
                other => other, // not `g`, process the key normally
            }
        } else {
            match action {
                DashboardAction::PendingG => {
                    *pending_g = true;
                    return Ok(LoopAction::Continue);
                }
                other => other,
            }
        };

        match action {
            DashboardAction::Quit => return Ok(LoopAction::Quit),

            DashboardAction::JumpInputNext => return Ok(LoopAction::JumpInput(true)),
            DashboardAction::JumpInputPrev => return Ok(LoopAction::JumpInput(false)),

            DashboardAction::RebindStart => {
                if !state.agents.is_empty() {
                    *input_mode = InputMode::ConfirmRebind;
                }
            }

            DashboardAction::Attach(ref agent_id) => {
                *input_mode = InputMode::Normal;
                if state.agents.contains_key(agent_id) {
                    return Ok(LoopAction::SwitchToTerminal(agent_id.clone()));
                }
            }

            DashboardAction::SpawnInline => {
                *history_index = None;
                *history_stash = None;
                if sorted_folders.len() == 1 {
                    let (name, path) = &sorted_folders[0];
                    if let Some((draft_folder_name, draft_folder_path, draft_title, draft_desc)) =
                        prompt_draft.take()
                    {
                        if draft_folder_name == *name {
                            *input_mode = InputMode::TypingPrompt {
                                folder_name: draft_folder_name,
                                folder_path: draft_folder_path,
                                title: draft_title,
                                description: draft_desc,
                                active_field: PromptField::Title,
                            };
                        } else {
                            *input_mode = InputMode::TypingPrompt {
                                folder_name: name.clone(),
                                folder_path: path.clone(),
                                title: String::new(),
                                description: String::new(),
                                active_field: PromptField::Title,
                            };
                        }
                    } else {
                        *input_mode = InputMode::TypingPrompt {
                            folder_name: name.clone(),
                            folder_path: path.clone(),
                            title: String::new(),
                            description: String::new(),
                            active_field: PromptField::Title,
                        };
                    }
                } else if sorted_folders.is_empty() {
                    *input_mode = InputMode::Normal;
                } else {
                    *input_mode = InputMode::PickingFolder {
                        folder_count: sorted_folders.len(),
                        reason: FolderPickReason::SpawnInline,
                    };
                }
            }

            DashboardAction::SpawnEditor => {
                if sorted_folders.len() == 1 {
                    let (name, path) = &sorted_folders[0];
                    let folder_name_owned = name.clone();
                    let folder_path_owned = path.clone();
                    let mut editor_result: Option<(String, String)> = None;
                    tokio::task::block_in_place(|| {
                        suspend_tui(terminal, || {
                            if let Ok(result) = crate::spawn::read_task_from_editor() {
                                editor_result = Some(result);
                            }
                        })
                    })?;
                    *client = DaemonClient::connect().await?;
                    match editor_result {
                        Some((title, prompt)) => {
                            let state = state_source.get();
                            let ctx = SpawnContext {
                                config,
                                current_state: &state,
                                state_source,
                                pty_rows,
                                pty_cols,
                            };
                            spawn_inline(
                                client,
                                &SpawnParams {
                                    folder_name: &folder_name_owned,
                                    folder_path: &folder_path_owned,
                                    title: &title,
                                    prompt: Some(&prompt),
                                },
                                &ctx,
                            )
                            .await?;
                        }
                        None => {
                            *input_mode = InputMode::ConfirmEmptySpawn {
                                folder_name: folder_name_owned,
                                folder_path: folder_path_owned,
                            };
                        }
                    }
                } else if sorted_folders.is_empty() {
                    *input_mode = InputMode::Normal;
                } else {
                    *input_mode = InputMode::PickingFolder {
                        folder_count: sorted_folders.len(),
                        reason: FolderPickReason::SpawnEditor,
                    };
                }
            }

            DashboardAction::FolderPicked(idx) => {
                *history_index = None;
                *history_stash = None;
                let reason = match input_mode {
                    InputMode::PickingFolder { reason, .. } => match reason {
                        FolderPickReason::SpawnInline => FolderPickReason::SpawnInline,
                        FolderPickReason::SpawnEditor => FolderPickReason::SpawnEditor,
                        FolderPickReason::Adopt => FolderPickReason::Adopt,
                    },
                    _ => FolderPickReason::SpawnInline,
                };
                if let Some((name, path)) = sorted_folders.get(idx) {
                    match reason {
                        FolderPickReason::SpawnEditor => {
                            let folder_name_owned = name.clone();
                            let folder_path_owned = path.clone();
                            let mut editor_result: Option<(String, String)> = None;
                            tokio::task::block_in_place(|| {
                                suspend_tui(
                                    terminal,
                                    || match crate::spawn::read_task_from_editor() {
                                        Ok(result) => editor_result = Some(result),
                                        Err(e) => {
                                            eprintln!("Error: {e}");
                                            std::thread::sleep(Duration::from_secs(1));
                                        }
                                    },
                                )
                            })?;
                            *client = DaemonClient::connect().await?;
                            match editor_result {
                                Some((title, prompt)) => {
                                    let state = state_source.get();
                                    let ctx = SpawnContext {
                                        config,
                                        current_state: &state,
                                        state_source,
                                        pty_rows,
                                        pty_cols,
                                    };
                                    spawn_inline(
                                        client,
                                        &SpawnParams {
                                            folder_name: &folder_name_owned,
                                            folder_path: &folder_path_owned,
                                            title: &title,
                                            prompt: Some(&prompt),
                                        },
                                        &ctx,
                                    )
                                    .await?;
                                    *input_mode = InputMode::Normal;
                                }
                                None => {
                                    *input_mode = InputMode::ConfirmEmptySpawn {
                                        folder_name: folder_name_owned,
                                        folder_path: folder_path_owned,
                                    };
                                }
                            }
                        }
                        FolderPickReason::SpawnInline => {
                            if let Some((
                                draft_folder_name,
                                draft_folder_path,
                                draft_title,
                                draft_desc,
                            )) = prompt_draft.take()
                            {
                                if draft_folder_name == *name {
                                    *input_mode = InputMode::TypingPrompt {
                                        folder_name: draft_folder_name,
                                        folder_path: draft_folder_path,
                                        title: draft_title,
                                        description: draft_desc,
                                        active_field: PromptField::Title,
                                    };
                                } else {
                                    *input_mode = InputMode::TypingPrompt {
                                        folder_name: name.clone(),
                                        folder_path: path.clone(),
                                        title: String::new(),
                                        description: String::new(),
                                        active_field: PromptField::Title,
                                    };
                                }
                            } else {
                                *input_mode = InputMode::TypingPrompt {
                                    folder_name: name.clone(),
                                    folder_path: path.clone(),
                                    title: String::new(),
                                    description: String::new(),
                                    active_field: PromptField::Title,
                                };
                            }
                        }
                        FolderPickReason::Adopt => {
                            *input_mode = InputMode::TypingAdopt {
                                input: String::new(),
                                folder_name: name.clone(),
                                folder_path: path.clone(),
                            };
                        }
                    }
                } else {
                    *input_mode = InputMode::Normal;
                }
            }

            DashboardAction::PromptCycleField { reverse } => {
                if let InputMode::TypingPrompt {
                    ref mut active_field,
                    ref folder_name,
                    ..
                } = input_mode
                {
                    let backend_count = config
                        .folder_backends(folder_name)
                        .map(|b| {
                            b.iter()
                                .filter(|id| config.backends.contains_key(id.as_str()))
                                .count()
                        })
                        .unwrap_or(1);

                    let next = if reverse {
                        match active_field {
                            PromptField::Title => {
                                if backend_count > 1 {
                                    PromptField::Backend
                                } else {
                                    PromptField::Description
                                }
                            }
                            PromptField::Description => PromptField::Title,
                            PromptField::Backend => PromptField::Description,
                        }
                    } else {
                        match active_field {
                            PromptField::Title => PromptField::Description,
                            PromptField::Description => {
                                if backend_count > 1 {
                                    PromptField::Backend
                                } else {
                                    PromptField::Title
                                }
                            }
                            PromptField::Backend => PromptField::Title,
                        }
                    };
                    *active_field = next;
                }
            }

            DashboardAction::PromptCycleBackend { reverse } => {
                if let InputMode::TypingPrompt { folder_name, .. } = input_mode {
                    let changed = with_state(|state| {
                        cycle_backend_for_folder(config, state, folder_name, reverse)
                    })?;
                    if changed.is_some() {
                        state_source.invalidate();
                    }
                }
            }

            DashboardAction::PromptInput(edit) => {
                if let InputMode::TypingPrompt {
                    ref mut title,
                    ref mut description,
                    ref active_field,
                    ..
                } = input_mode
                {
                    match edit {
                        PromptEdit::HistoryPrev => {
                            let state = state_source.get();
                            let history = &state.prompt_history;
                            if !history.is_empty() {
                                if history_index.is_none() {
                                    *history_stash = Some((title.clone(), description.clone()));
                                }
                                let new_idx = match *history_index {
                                    None => 0,
                                    Some(i) => (i + 1).min(history.len() - 1),
                                };
                                *history_index = Some(new_idx);
                                if let Some(entry) = history.get(new_idx) {
                                    *title = entry.title.clone();
                                    *description = entry.description.clone();
                                }
                            }
                        }
                        PromptEdit::HistoryNext => {
                            if let Some(idx) = *history_index {
                                if idx == 0 {
                                    *history_index = None;
                                    if let Some((stashed_title, stashed_desc)) =
                                        history_stash.take()
                                    {
                                        *title = stashed_title;
                                        *description = stashed_desc;
                                    }
                                } else {
                                    let new_idx = idx - 1;
                                    *history_index = Some(new_idx);
                                    let state = state_source.get();
                                    if let Some(entry) = state.prompt_history.get(new_idx) {
                                        *title = entry.title.clone();
                                        *description = entry.description.clone();
                                    }
                                }
                            }
                        }
                        _ => {
                            *history_index = None;
                            *history_stash = None;
                            let target = match active_field {
                                PromptField::Title => Some(title),
                                PromptField::Description => Some(description),
                                PromptField::Backend => None,
                            };
                            if let Some(target) = target {
                                apply_edit(target, &edit);
                            }
                        }
                    }
                }
            }

            DashboardAction::PromptSubmitted => {
                let mut submitted = false;
                if let InputMode::TypingPrompt {
                    folder_name,
                    folder_path,
                    title,
                    description,
                    active_field,
                } = input_mode
                {
                    let title_trimmed = title.trim().to_string();
                    if title_trimmed.is_empty() {
                        *active_field = PromptField::Title;
                    } else {
                        let desc_trimmed = description.trim().to_string();
                        let effective_prompt = if desc_trimmed.is_empty() {
                            None
                        } else {
                            Some(format!("{title_trimmed}\n\n{desc_trimmed}"))
                        };
                        let ctx = SpawnContext {
                            config,
                            current_state: &state,
                            state_source,
                            pty_rows,
                            pty_cols,
                        };
                        let spawn_result = spawn_inline(
                            client,
                            &SpawnParams {
                                folder_name,
                                folder_path,
                                title: &title_trimmed,
                                prompt: effective_prompt.as_deref(),
                            },
                            &ctx,
                        )
                        .await;
                        match spawn_result {
                            Ok(()) => {
                                if !title_trimmed.is_empty() {
                                    let entry = PromptHistoryEntry {
                                        title: title_trimmed,
                                        description: desc_trimmed,
                                    };
                                    let _ = with_state(|state| {
                                        state.prompt_history.retain(|e| e != &entry);
                                        state.prompt_history.insert(0, entry);
                                        state.prompt_history.truncate(50);
                                    });
                                }
                                submitted = true;
                            }
                            Err(e) => {
                                *prompt_draft = None;
                                *history_index = None;
                                *history_stash = None;
                                *input_mode = InputMode::ActionFailed {
                                    reason: format!("{e:#}"),
                                };
                            }
                        }
                    }
                }
                if submitted {
                    *prompt_draft = None;
                    *history_index = None;
                    *history_stash = None;
                    *input_mode = InputMode::Normal;
                }
            }

            DashboardAction::ConfirmYes => {
                let mut failure_reason: Option<String> = None;
                if let InputMode::ConfirmReload { agent_id, .. } = &*input_mode {
                    let id = agent_id.clone();
                    let _ = client.kill_agent(&id).await;
                    if let Some(agent) = state.agents.get(&id) {
                        match reconcile_resume_action(config, &id, agent) {
                            ResumeReconcileAction::Resume { cwd, cmd, env } => {
                                let _ = client
                                    .spawn_agent(&id, &cwd, &cmd, &env, pty_rows, pty_cols)
                                    .await;
                                let _ = with_state(|state| {
                                    if let Some(a) = state.agents.get_mut(&id) {
                                        a.state = AgentState::Input;
                                        a.last_activity_at = chrono::Utc::now();
                                    }
                                });
                                state_source.invalidate();
                            }
                            ResumeReconcileAction::Remove => {
                                killed_at.insert(id, Instant::now());
                            }
                        }
                    }
                } else if let InputMode::ConfirmKill { agent_id, .. } = &*input_mode {
                    let id = agent_id.clone();
                    let _ = client.kill_agent(&id).await;
                    selected_agents.remove(&id);
                    killed_at.insert(id, Instant::now());
                } else if let InputMode::ConfirmBatchKill = &*input_mode {
                    for agent_id in selected_agents.drain() {
                        let _ = client.kill_agent(&agent_id).await;
                        killed_at.insert(agent_id, Instant::now());
                    }
                } else if let InputMode::ConfirmRebind = &*input_mode {
                    let agent_refs: HashMap<String, &Agent> =
                        state.agents.iter().map(|(id, a)| (id.clone(), a)).collect();
                    let ordered = render::ordered_agent_ids(config, &agent_refs, filter_query);
                    let assignments = rebind_assignments(&ordered);
                    let _ = with_state(|state| {
                        for agent in state.agents.values_mut() {
                            agent.key = None;
                        }
                        for (id, key) in &assignments {
                            if let Some(agent) = state.agents.get_mut(id) {
                                agent.key = Some(*key);
                            }
                        }
                    });
                    state_source.invalidate();
                } else if let InputMode::ConfirmEmptySpawn {
                    folder_name,
                    folder_path,
                } = input_mode
                {
                    let ctx = SpawnContext {
                        config,
                        current_state: &state,
                        state_source,
                        pty_rows,
                        pty_cols,
                    };
                    if let Err(e) = spawn_inline(
                        client,
                        &SpawnParams {
                            folder_name,
                            folder_path,
                            title: "interactive",
                            prompt: None,
                        },
                        &ctx,
                    )
                    .await
                    {
                        failure_reason = Some(format!("{e:#}"));
                    }
                }
                *input_mode = match failure_reason {
                    Some(reason) => InputMode::ActionFailed { reason },
                    None => InputMode::Normal,
                };
            }

            DashboardAction::PendingEdit => {
                *input_mode = InputMode::WaitingEdit;
            }

            DashboardAction::EditAgent(agent_id) => {
                let current_title = state
                    .agents
                    .get(&agent_id)
                    .map(|a| a.title.clone())
                    .unwrap_or_default();
                *input_mode = InputMode::EditingDescription {
                    agent_id,
                    input: current_title,
                };
            }

            DashboardAction::EditInput(edit) => {
                if let InputMode::EditingDescription { ref mut input, .. } = input_mode {
                    apply_edit(input, &edit);
                }
            }

            DashboardAction::EditSubmitted => {
                let mut submitted = false;
                if let InputMode::EditingDescription { agent_id, input } = input_mode {
                    let new_title = input.trim().to_string();
                    if !new_title.is_empty() {
                        let id = agent_id.clone();
                        let _ = with_state(|state| {
                            if let Some(agent) = state.agents.get_mut(&id) {
                                agent.title = new_title;
                            }
                        });
                        state_source.invalidate();
                        submitted = true;
                    }
                }
                if submitted {
                    *input_mode = InputMode::Normal;
                }
            }

            DashboardAction::ToggleSelect => {
                if let Some(idx) = *selected_index {
                    let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                    if let Some(agent_id) = agent_ids.get(idx) {
                        if selected_agents.contains(agent_id) {
                            selected_agents.remove(agent_id);
                        } else {
                            selected_agents.insert(agent_id.clone());
                        }
                    }
                }
            }

            DashboardAction::ToggleSelectAll => {
                let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                if selected_agents.len() == agent_ids.len() && !agent_ids.is_empty() {
                    selected_agents.clear();
                } else {
                    *selected_agents = agent_ids.into_iter().collect();
                }
            }

            DashboardAction::PendingKill => {
                if !selected_agents.is_empty() {
                    *input_mode = InputMode::ConfirmBatchKill;
                } else {
                    *input_mode = InputMode::WaitingKill;
                }
            }

            DashboardAction::KillAgent(agent_id) => {
                let title = state
                    .agents
                    .get(&agent_id)
                    .map(|a| a.title.clone())
                    .unwrap_or_default();
                *input_mode = InputMode::ConfirmKill { agent_id, title };
            }

            DashboardAction::PendingReload => {
                *input_mode = InputMode::WaitingReload;
            }

            DashboardAction::ReloadAgent(agent_id) => {
                if let Some(agent) = state.agents.get(&agent_id) {
                    if !crate::spawn::backend_supports_resume(config, &agent.backend_id) {
                        let name = config.backend_display_name(&agent.backend_id);
                        *input_mode = InputMode::ReloadUnavailable {
                            reason: format!("{name} backend does not support resume"),
                        };
                    } else {
                        *input_mode = InputMode::ConfirmReload {
                            agent_id,
                            title: agent.title.clone(),
                        };
                    }
                }
            }

            DashboardAction::AdoptStart => {
                if sorted_folders.len() == 1 {
                    let (name, path) = &sorted_folders[0];
                    *input_mode = InputMode::TypingAdopt {
                        input: String::new(),
                        folder_name: name.clone(),
                        folder_path: path.clone(),
                    };
                } else if sorted_folders.is_empty() {
                    *input_mode = InputMode::Normal;
                } else {
                    *input_mode = InputMode::PickingFolder {
                        folder_count: sorted_folders.len(),
                        reason: FolderPickReason::Adopt,
                    };
                }
            }

            DashboardAction::AdoptInput(edit) => {
                if let InputMode::TypingAdopt { ref mut input, .. } = input_mode {
                    apply_edit(input, &edit);
                }
            }

            DashboardAction::AdoptSubmitted => {
                let mut failure_reason: Option<String> = None;
                if let InputMode::TypingAdopt {
                    input,
                    folder_name,
                    folder_path,
                } = input_mode
                {
                    let session_id = input.trim().to_string();
                    if !session_id.is_empty() {
                        let ctx = SpawnContext {
                            config,
                            current_state: &state,
                            state_source,
                            pty_rows,
                            pty_cols,
                        };
                        if let Err(e) =
                            adopt_inline(client, &session_id, folder_name, folder_path, &ctx).await
                        {
                            failure_reason = Some(format!("{e:#}"));
                        }
                    }
                }
                *input_mode = match failure_reason {
                    Some(reason) => InputMode::ActionFailed { reason },
                    None => InputMode::Normal,
                };
            }

            DashboardAction::SelectNext => {
                let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                if agent_ids.is_empty() {
                    *selected_index = None;
                } else {
                    *selected_index = Some(match *selected_index {
                        None => 0,
                        Some(i) => (i + 1).min(agent_ids.len() - 1),
                    });
                }
            }

            DashboardAction::SelectPrev => {
                let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                if agent_ids.is_empty() {
                    *selected_index = None;
                } else {
                    *selected_index = Some(match *selected_index {
                        None => agent_ids.len() - 1,
                        Some(i) => i.saturating_sub(1),
                    });
                }
            }

            DashboardAction::AttachSelected => {
                if let Some(idx) = *selected_index {
                    let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                    if let Some(agent_id) = agent_ids.get(idx) {
                        if state.agents.contains_key(agent_id) {
                            return Ok(LoopAction::SwitchToTerminal(agent_id.clone()));
                        }
                    }
                }
            }

            DashboardAction::SelectFirst => {
                let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                if agent_ids.is_empty() {
                    *selected_index = None;
                } else {
                    *selected_index = Some(0);
                }
            }

            DashboardAction::SelectLast => {
                let agent_ids = render::ordered_agent_ids(config, &agent_refs, filter_query);
                if agent_ids.is_empty() {
                    *selected_index = None;
                } else {
                    *selected_index = Some(agent_ids.len() - 1);
                }
            }

            DashboardAction::StartFilter => {
                *input_mode = InputMode::Filtering {
                    query: filter_query.clone(),
                };
            }

            DashboardAction::FilterInput(edit) => {
                if let InputMode::Filtering { ref mut query } = input_mode {
                    apply_edit(query, &edit);
                    *filter_query = query.clone();
                    *selected_index = Some(0);
                    selected_agents.clear();
                }
            }

            DashboardAction::FilterAccept => {
                *selected_index = Some(0);
                *input_mode = InputMode::Normal;
            }

            DashboardAction::Cancel => {
                *history_index = None;
                *history_stash = None;
                if let InputMode::Filtering { .. } = &*input_mode {
                    filter_query.clear();
                    *selected_index = None;
                    selected_agents.clear();
                } else if let InputMode::TypingPrompt {
                    folder_name,
                    folder_path,
                    title,
                    description,
                    ..
                } = &*input_mode
                {
                    if !title.is_empty() || !description.is_empty() {
                        *prompt_draft = Some((
                            folder_name.clone(),
                            folder_path.clone(),
                            title.clone(),
                            description.clone(),
                        ));
                    }
                }
                *input_mode = InputMode::Normal;
            }

            DashboardAction::ShowHelp => {
                *input_mode = InputMode::Help {
                    scroll: 0,
                    filter: String::new(),
                    filtering: false,
                };
            }

            DashboardAction::ShowQuitHint => {
                *input_mode = InputMode::QuitHint;
            }

            DashboardAction::ClearSelection => {
                *selected_index = None;
            }

            DashboardAction::HelpScroll(delta) => {
                if let InputMode::Help {
                    ref mut scroll,
                    ref filter,
                    ..
                } = input_mode
                {
                    let total = shortcuts::help_line_count(filter);
                    if delta == i32::MIN {
                        *scroll = 0;
                    } else if delta == i32::MAX {
                        *scroll = total.saturating_sub(1);
                    } else if delta < 0 {
                        *scroll = scroll.saturating_sub((-delta) as usize);
                    } else {
                        *scroll = (*scroll + delta as usize).min(total.saturating_sub(1));
                    }
                }
            }

            DashboardAction::HelpStartFilter => {
                if let InputMode::Help {
                    ref mut filtering, ..
                } = input_mode
                {
                    *filtering = true;
                }
            }

            DashboardAction::HelpFilterInput(edit) => {
                if let InputMode::Help {
                    ref mut filter,
                    ref mut scroll,
                    ..
                } = input_mode
                {
                    apply_edit(filter, &edit);
                    *scroll = 0;
                }
            }

            DashboardAction::HelpFilterAccept => {
                if let InputMode::Help {
                    ref mut filtering, ..
                } = input_mode
                {
                    *filtering = false;
                }
            }

            DashboardAction::Refresh | DashboardAction::PendingG => {}
        }
    }

    Ok(LoopAction::Continue)
}

/// Handle keyboard input while in copy mode.
fn handle_copy_mode_key(
    key_event: &crossterm::event::KeyEvent,
    _client: &mut DaemonClient,
    agent_id: &str,
    pane_views: &mut HashMap<String, PaneView>,
    visible_rows: u16,
    visible_cols: u16,
) -> Result<LoopAction> {
    let pv = match pane_views.get_mut(agent_id) {
        Some(pv) => pv,
        None => return Ok(LoopAction::Continue),
    };

    let ctrl = key_event.modifiers.contains(KeyModifiers::CONTROL);

    // Handle pending `g` state (waiting for second `g` to make `gg`)
    let was_pending_g = pv.copy_mode.as_ref().is_some_and(|cm| cm.pending_g);
    if was_pending_g {
        if let Some(cm) = pv.copy_mode.as_mut() {
            cm.pending_g = false;
        }
        if key_event.code == KeyCode::Char('g') {
            pv.copy_jump_edge(true, visible_rows);
            return Ok(LoopAction::Continue);
        }
        // Not `g` — fall through to handle this key normally
    }

    match key_event.code {
        // Exit copy mode
        KeyCode::Char('q') | KeyCode::Esc => pv.exit_copy_mode(),

        // Ctrl+F -> exit copy mode + detach
        KeyCode::Char('f') if ctrl => {
            pv.exit_copy_mode();
            return Ok(LoopAction::SwitchToDashboard);
        }

        // Ctrl+J -> exit copy mode (snap to bottom)
        KeyCode::Char('j') if ctrl => pv.exit_copy_mode(),

        // Movement
        KeyCode::Char('h') | KeyCode::Left => pv.copy_move(-1, 0, visible_rows, visible_cols),
        KeyCode::Char('j') | KeyCode::Down => pv.copy_move(0, 1, visible_rows, visible_cols),
        KeyCode::Char('k') | KeyCode::Up => pv.copy_move(0, -1, visible_rows, visible_cols),
        KeyCode::Char('l') | KeyCode::Right => pv.copy_move(1, 0, visible_rows, visible_cols),

        // Line jumps
        KeyCode::Char('0') | KeyCode::Home => pv.copy_line_jump(false, visible_cols),
        KeyCode::Char('$') | KeyCode::End => pv.copy_line_jump(true, visible_cols),

        // Page up/down
        KeyCode::Char('u') if ctrl => pv.copy_page(true, visible_rows),
        KeyCode::Char('d') if ctrl => pv.copy_page(false, visible_rows),

        // gg = top, G = bottom
        KeyCode::Char('g') => {
            if let Some(cm) = pv.copy_mode.as_mut() {
                cm.pending_g = true;
            }
        }
        KeyCode::Char('G') => pv.copy_jump_edge(false, visible_rows),

        // Toggle selection
        KeyCode::Char('v') => pv.copy_toggle_selection(visible_cols),
        KeyCode::Char('V') => pv.copy_toggle_line_selection(visible_cols),

        // Yank
        KeyCode::Char('y') => {
            pv.copy_yank(visible_cols);
            pv.exit_copy_mode();
        }

        _ => {}
    }

    Ok(LoopAction::Continue)
}

async fn handle_terminal_event(
    ev: &Event,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &mut DaemonClient,
    agent_id: &str,
    pane_views: &mut HashMap<String, PaneView>,
) -> Result<LoopAction> {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let content_rows = term_rows.saturating_sub(1);

    match ev {
        Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
            // Copy mode intercepts all keys when active
            let in_copy_mode = pane_views
                .get(agent_id)
                .is_some_and(|pv| pv.copy_mode.is_some());

            if in_copy_mode {
                return handle_copy_mode_key(
                    key_event,
                    client,
                    agent_id,
                    pane_views,
                    content_rows,
                    term_cols,
                );
            }

            // Ctrl+F -> back to dashboard (stay subscribed so pane
            // keeps receiving live output while on the dashboard)
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('f')
            {
                return Ok(LoopAction::SwitchToDashboard);
            }

            // Ctrl+G / Ctrl+Shift+G -> jump to next/prev agent in Input state.
            // Shifted form arrives as KeyCode::Char('G') on most terminals; on
            // terminals with disambiguating escape codes the SHIFT modifier may
            // also be set, so accept either signal.
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key_event.code, KeyCode::Char('g') | KeyCode::Char('G'))
            {
                let shifted = key_event.modifiers.contains(KeyModifiers::SHIFT)
                    || matches!(key_event.code, KeyCode::Char('G'));
                return Ok(LoopAction::JumpInput(!shifted));
            }

            // Ctrl+C -> send SIGINT to agent
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('c')
            {
                let _ = client.send_sigint(agent_id).await;
                let id = agent_id.to_owned();
                let _ = with_state(|state| {
                    if let Some(agent) = state.agents.get_mut(&id) {
                        agent.state = AgentState::Input;
                    }
                });
                return Ok(LoopAction::Continue);
            }

            // Ctrl+J -> snap to bottom (live view) without forwarding to PTY
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('j')
            {
                if let Some(pv) = pane_views.get_mut(agent_id) {
                    pv.snap_to_bottom();
                }
                return Ok(LoopAction::Continue);
            }

            // Ctrl+S -> enter copy mode
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('s')
            {
                if let Some(pv) = pane_views.get_mut(agent_id) {
                    pv.enter_copy_mode(content_rows, term_cols);
                }
                return Ok(LoopAction::Continue);
            }

            if let Some(pv) = pane_views.get_mut(agent_id) {
                pv.clear_selection();
                pv.snap_to_bottom();
            }

            // Esc is forwarded to the PTY. The old path also pre-emptively
            // flipped agent.state Working→Input on the guess that Esc
            // interrupts Claude Code, but that races with hook events and
            // caused visible "stuck Input" when Esc was absorbed by other TUI
            // contexts (menus, vim-mode, etc.). If Esc truly interrupts,
            // Claude will fire Stop which will correctly land on Done.

            if let Some(bytes) = pane::encode_key(*key_event) {
                let _ = client.send_input(agent_id, &bytes).await;
            }
        }

        Event::Mouse(mouse_event) => {
            let (term_cols, term_rows) = crossterm::terminal::size()?;
            let content_rows = term_rows.saturating_sub(1);
            let pane_area = ratatui::layout::Rect::new(0, 1, term_cols, content_rows);

            let mouse_mode = pane_views
                .get(agent_id)
                .is_some_and(|pv| pv.mouse_mode_active());

            if mouse_mode {
                if let Some(bytes) = pane::encode_mouse_for_pane(*mouse_event, pane_area) {
                    let _ = client.send_input(agent_id, &bytes).await;
                }
            } else {
                use crossterm::event::{MouseButton, MouseEventKind};
                match mouse_event.kind {
                    MouseEventKind::ScrollUp => {
                        if let Some(pv) = pane_views.get_mut(agent_id) {
                            if pv.alternate_screen() {
                                let _ = client.send_input(agent_id, b"\x1b[A\x1b[A\x1b[A").await;
                            } else {
                                pv.scroll_up(3);
                            }
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if let Some(pv) = pane_views.get_mut(agent_id) {
                            if pv.alternate_screen() {
                                let _ = client.send_input(agent_id, b"\x1b[B\x1b[B\x1b[B").await;
                            } else {
                                pv.scroll_down(3);
                            }
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(pv) = pane_views.get_mut(agent_id) {
                            if let Some(ref sel) = pv.selection {
                                if !sel.active && sel.start != sel.end {
                                    let sel = sel.clone();
                                    let screen = pv.scrolled_screen();
                                    let text =
                                        pane::extract_selected_text(screen, &sel, pane_area.width);
                                    if !text.is_empty() {
                                        pane::copy_to_clipboard(&text);
                                    }
                                }
                            }

                            let col = mouse_event.column.saturating_sub(pane_area.x);
                            let row = mouse_event.row.saturating_sub(pane_area.y);
                            if col < pane_area.width && row < pane_area.height {
                                pv.selection = Some(pane::Selection {
                                    start: (col, row),
                                    end: (col, row),
                                    active: true,
                                });
                            }
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(pv) = pane_views.get_mut(agent_id) {
                            if pv.selection.as_ref().is_some_and(|s| s.active) {
                                let col = mouse_event
                                    .column
                                    .saturating_sub(pane_area.x)
                                    .min(pane_area.width.saturating_sub(1));
                                let row = mouse_event
                                    .row
                                    .saturating_sub(pane_area.y)
                                    .min(pane_area.height.saturating_sub(1));

                                if let Some(ref mut sel) = pv.selection {
                                    sel.end = (col, row);
                                }

                                if !pv.alternate_screen() {
                                    let scroll_zone = 3u16;
                                    if mouse_event.row < pane_area.y.saturating_add(scroll_zone) {
                                        let distance = pane_area
                                            .y
                                            .saturating_add(scroll_zone)
                                            .saturating_sub(mouse_event.row);
                                        let speed = (distance as usize).clamp(1, 5);
                                        let old = pv.scroll_offset;
                                        pv.scroll_up(speed);
                                        let delta = (pv.scroll_offset - old) as u16;
                                        if delta > 0 {
                                            if let Some(ref mut sel) = pv.selection {
                                                sel.start.1 = sel
                                                    .start
                                                    .1
                                                    .saturating_add(delta)
                                                    .min(pane_area.height.saturating_sub(1));
                                            }
                                        }
                                    } else if mouse_event.row
                                        >= pane_area.y.saturating_add(
                                            pane_area.height.saturating_sub(scroll_zone),
                                        )
                                    {
                                        let edge_start = pane_area.y.saturating_add(
                                            pane_area.height.saturating_sub(scroll_zone),
                                        );
                                        let distance = mouse_event.row.saturating_sub(edge_start);
                                        let speed = (distance as usize + 1).clamp(1, 5);
                                        let old = pv.scroll_offset;
                                        pv.scroll_down(speed);
                                        let delta = (old - pv.scroll_offset) as u16;
                                        if delta > 0 {
                                            if let Some(ref mut sel) = pv.selection {
                                                sel.start.1 = sel.start.1.saturating_sub(delta);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(pv) = pane_views.get_mut(agent_id) {
                            let should_copy = pv
                                .selection
                                .as_ref()
                                .is_some_and(|s| s.active && s.start != s.end);
                            if should_copy {
                                let sel = pv.selection.clone().unwrap();
                                let screen = pv.scrolled_screen();
                                let text =
                                    pane::extract_selected_text(screen, &sel, pane_area.width);
                                if !text.is_empty() {
                                    pane::copy_to_clipboard(&text);
                                }
                            }
                            pv.selection = None;
                        }
                    }
                    _ => {}
                }
            }
        }

        Event::Paste(text) => {
            if let Some(pv) = pane_views.get_mut(agent_id) {
                pv.clear_selection();
                pv.snap_to_bottom();
            }

            let use_bracket = pane_views
                .get(agent_id)
                .is_some_and(|pv| pv.parser.screen().bracketed_paste());

            let data = if use_bracket {
                let mut buf = Vec::with_capacity(text.len() + 14);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(text.as_bytes());
                buf.extend_from_slice(b"\x1b[201~");
                buf
            } else {
                text.as_bytes().to_vec()
            };
            let _ = client.send_input(agent_id, &data).await;

            terminal.clear()?;
        }

        Event::Resize(cols, rows) => {
            let content_rows = rows.saturating_sub(1);
            if let Some(pv) = pane_views.get_mut(agent_id) {
                pv.resize(content_rows, *cols);
            }
            let _ = client.resize(agent_id, content_rows, *cols).await;
        }

        _ => {}
    }

    Ok(LoopAction::Continue)
}

struct SpawnParams<'a> {
    folder_name: &'a str,
    folder_path: &'a str,
    title: &'a str,
    prompt: Option<&'a str>,
}

struct SpawnContext<'a> {
    config: &'a ClamorConfig,
    current_state: &'a ClamorState,
    state_source: &'a StateSource,
    pty_rows: u16,
    pty_cols: u16,
}

async fn spawn_inline(
    client: &mut DaemonClient,
    params: &SpawnParams<'_>,
    ctx: &SpawnContext<'_>,
) -> Result<()> {
    let cwd = resolve_path(params.folder_path);
    let cwd_str = cwd.to_string_lossy().to_string();

    let launch = crate::spawn::resolve_spawn_launch(
        ctx.config,
        ctx.current_state,
        params.folder_name,
        params.folder_path,
        &cwd_str,
        params.title,
        params.prompt,
    )?;

    let existing_ids: std::collections::HashSet<String> =
        ctx.current_state.agents.keys().cloned().collect();
    let id = generate_id(&existing_ids);
    let now = Utc::now();

    let existing: Vec<&Agent> = ctx.current_state.agents.values().collect();
    let key = keys::next_available_key(&existing);
    let color_index = next_color_index(&existing);

    let initial_state = if params.prompt.is_some() {
        AgentState::Working
    } else {
        AgentState::Input
    };

    let agent = Agent {
        id: id.clone(),
        title: launch.title.clone(),
        folder_id: params.folder_name.to_string(),
        backend_id: launch.backend_id.clone(),
        cwd: cwd_str.clone(),
        initial_prompt: params.prompt.map(|s| s.to_string()),
        state: initial_state,
        started_at: now,
        last_activity_at: now,
        last_tool: None,
        resume_token: None,
        metadata: HashMap::new(),
        key,
        color_index,
    };

    let mut env = launch.env.clone();
    env.push(("CLAMOR_AGENT_ID".to_string(), id.clone()));

    client
        .spawn_agent(&id, &cwd_str, &launch.cmd, &env, ctx.pty_rows, ctx.pty_cols)
        .await?;

    with_state(|state| {
        state.agents.insert(id.clone(), agent);
    })?;
    ctx.state_source.invalidate();

    Ok(())
}

async fn adopt_inline(
    client: &mut DaemonClient,
    session_id: &str,
    folder_name: &str,
    folder_path: &str,
    ctx: &SpawnContext<'_>,
) -> Result<()> {
    let cwd = resolve_path(folder_path);
    let cwd_str = cwd.to_string_lossy().to_string();

    let backend_id = crate::spawn::select_adopt_backend(ctx.config, folder_name)?;
    let launch = crate::spawn::resolve_resume_launch(
        ctx.config,
        &backend_id,
        folder_name,
        folder_path,
        &cwd_str,
        &format!("adopted: {session_id}"),
        Some(session_id),
    )?;

    let existing_ids: std::collections::HashSet<String> =
        ctx.current_state.agents.keys().cloned().collect();
    let id = generate_id(&existing_ids);
    let now = Utc::now();

    let existing: Vec<&Agent> = ctx.current_state.agents.values().collect();
    let key = keys::next_available_key(&existing);
    let color_index = next_color_index(&existing);

    let agent = Agent {
        id: id.clone(),
        title: launch.title.clone(),
        folder_id: folder_name.to_string(),
        backend_id: launch.backend_id.clone(),
        cwd: cwd_str.clone(),
        initial_prompt: None,
        state: AgentState::Input,
        started_at: now,
        last_activity_at: now,
        last_tool: None,
        resume_token: Some(session_id.to_string()),
        metadata: HashMap::new(),
        key,
        color_index,
    };

    let mut env = launch.env.clone();
    env.push(("CLAMOR_AGENT_ID".to_string(), id.clone()));

    client
        .spawn_agent(&id, &cwd_str, &launch.cmd, &env, ctx.pty_rows, ctx.pty_cols)
        .await?;

    with_state(|state| {
        state.agents.insert(id.clone(), agent);
    })?;
    ctx.state_source.invalidate();

    Ok(())
}

fn suspend_tui<F>(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, f: F) -> Result<()>
where
    F: FnOnce(),
{
    execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    terminal::disable_raw_mode().context("Failed to disable raw mode for suspend")?;
    terminal
        .backend_mut()
        .execute(LeaveAlternateScreen)
        .context("Failed to leave alternate screen for suspend")?;

    f();

    io::stdout()
        .execute(EnterAlternateScreen)
        .context("Failed to re-enter alternate screen")?;
    terminal::enable_raw_mode().context("Failed to re-enable raw mode")?;
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    terminal.clear().context("Failed to clear terminal")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_agent(backend_id: &str, resume_token: Option<&str>) -> Agent {
        Agent {
            id: "abc123".to_string(),
            title: "task".to_string(),
            folder_id: "work".to_string(),
            backend_id: backend_id.to_string(),
            cwd: "/tmp/work".to_string(),
            initial_prompt: None,
            state: AgentState::Input,
            started_at: Utc::now(),
            last_activity_at: Utc::now(),
            last_tool: None,
            resume_token: resume_token.map(ToString::to_string),
            metadata: HashMap::new(),
            key: None,
            color_index: 0,
        }
    }

    fn jump_agent(id: &str, state: AgentState) -> (String, Agent) {
        let mut a = test_agent("claude-code", None);
        a.id = id.to_string();
        a.state = state;
        (id.to_string(), a)
    }

    #[test]
    fn jump_picks_next_input_agent_in_display_order() {
        let pairs = [
            jump_agent("a", AgentState::Working),
            jump_agent("b", AgentState::Input),
            jump_agent("c", AgentState::Working),
            jump_agent("d", AgentState::Input),
        ];
        let ordered: Vec<String> = pairs.iter().map(|(id, _)| id.clone()).collect();
        let agents: HashMap<String, &Agent> = pairs.iter().map(|(id, a)| (id.clone(), a)).collect();

        assert_eq!(
            pick_jump_target(&ordered, &agents, Some("a"), true).as_deref(),
            Some("b")
        );
        assert_eq!(
            pick_jump_target(&ordered, &agents, Some("b"), true).as_deref(),
            Some("d")
        );
    }

    #[test]
    fn jump_wraps_around_in_both_directions() {
        let pairs = [
            jump_agent("a", AgentState::Input),
            jump_agent("b", AgentState::Working),
            jump_agent("c", AgentState::Input),
        ];
        let ordered: Vec<String> = pairs.iter().map(|(id, _)| id.clone()).collect();
        let agents: HashMap<String, &Agent> = pairs.iter().map(|(id, a)| (id.clone(), a)).collect();

        assert_eq!(
            pick_jump_target(&ordered, &agents, Some("c"), true).as_deref(),
            Some("a")
        );
        assert_eq!(
            pick_jump_target(&ordered, &agents, Some("a"), false).as_deref(),
            Some("c")
        );
    }

    #[test]
    fn rebind_assigns_keys_in_pool_order_truncating_overflow() {
        let pool_len = keys::key_pool().len();
        let ordered: Vec<String> = (0..pool_len + 3).map(|i| format!("agent-{i}")).collect();
        let assignments = rebind_assignments(&ordered);

        assert_eq!(assignments.len(), pool_len);
        let expected_first: Vec<(String, char)> = keys::key_pool()
            .iter()
            .copied()
            .zip(ordered.iter().cloned())
            .map(|(k, id)| (id, k))
            .collect();
        assert_eq!(assignments, expected_first);
    }

    #[test]
    fn jump_returns_none_when_no_agent_needs_input() {
        let pairs = [
            jump_agent("a", AgentState::Working),
            jump_agent("b", AgentState::Done),
        ];
        let ordered: Vec<String> = pairs.iter().map(|(id, _)| id.clone()).collect();
        let agents: HashMap<String, &Agent> = pairs.iter().map(|(id, a)| (id.clone(), a)).collect();

        assert!(pick_jump_target(&ordered, &agents, Some("a"), true).is_none());
        assert!(pick_jump_target(&ordered, &agents, None, false).is_none());
    }

    #[test]
    fn reconcile_removes_agent_when_backend_cannot_resume() {
        let config: ClamorConfig = serde_yaml::from_str(
            r#"
backends:
  open-code:
    display_name: OpenCode
    spawn:
      cmd: [opencode, run, --prompt, "{{prompt}}"]
folders:
  work:
    path: ~/work
    backends: [open-code]
"#,
        )
        .unwrap();

        let agent = test_agent("open-code", Some("sess-1"));
        assert!(matches!(
            reconcile_resume_action(&config, &agent.id, &agent),
            ResumeReconcileAction::Remove
        ));
    }

    #[test]
    fn reconcile_removes_agent_when_backend_definition_is_missing() {
        let config: ClamorConfig = serde_yaml::from_str(
            r#"
folders:
  work:
    path: ~/work
    backends: [claude-code]
"#,
        )
        .unwrap();
        let agent = test_agent("missing-backend", Some("sess-1"));

        assert!(matches!(
            reconcile_resume_action(&config, &agent.id, &agent),
            ResumeReconcileAction::Remove
        ));
    }

    #[test]
    fn reconcile_removes_agent_when_resume_template_is_invalid() {
        let config: ClamorConfig = serde_yaml::from_str(
            r#"
backends:
  claude-code:
    display_name: Claude
    spawn:
      cmd: [claude, "{{prompt}}"]
    resume:
      cmd: [claude, --resume, "{{missing}}"]
    capabilities:
      resume: true
folders:
  work:
    path: ~/work
    backends: [claude-code]
"#,
        )
        .unwrap();
        let agent = test_agent("claude-code", Some("sess-1"));

        assert!(matches!(
            reconcile_resume_action(&config, &agent.id, &agent),
            ResumeReconcileAction::Remove
        ));
    }

    #[test]
    fn reconcile_resumes_tokenless_backend() {
        let config: ClamorConfig = serde_yaml::from_str(
            r#"
backends:
  pi:
    display_name: Pi
    spawn:
      cmd: [pi, "{{prompt}}"]
    resume:
      cmd: [pi, --continue]
    capabilities:
      resume: true
folders:
  work:
    path: ~/work
    backends: [pi]
"#,
        )
        .unwrap();

        // No resume token — pi uses --continue instead
        let agent = test_agent("pi", None);
        match reconcile_resume_action(&config, &agent.id, &agent) {
            ResumeReconcileAction::Resume { cmd, .. } => {
                assert_eq!(cmd, vec!["pi", "--continue"]);
            }
            ResumeReconcileAction::Remove => panic!("expected Resume, got Remove"),
        }
    }
}
