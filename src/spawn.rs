use std::io::{self, Write};

use anyhow::{bail, Context};
use chrono::Utc;

use crate::agent::{generate_id, next_color_index, Agent, AgentState};
use crate::client::DaemonClient;
use crate::config::{resolve_path, BackendCommandConfig, ClamorConfig};
use crate::daemon;
use crate::dashboard::keys;
use crate::picker;
use crate::state::{selected_backend_for_folder, with_state, ClamorState};

fn ensure_daemon() -> anyhow::Result<()> {
    if !daemon::is_daemon_running() {
        daemon::start_daemon_background()?;
    }
    Ok(())
}

pub fn is_debug_mode() -> bool {
    std::env::var("CLAMOR_DEBUG").is_ok()
}

fn build_debug_spawn_cmd(prompt: Option<&str>) -> Vec<String> {
    if is_debug_mode() {
        let exe = std::env::current_exe().unwrap_or_else(|_| "clamor".into());
        let desc = prompt.unwrap_or("interactive");
        return vec![
            exe.to_string_lossy().to_string(),
            "mock-agent".to_string(),
            "--description".to_string(),
            desc.to_string(),
        ];
    }

    Vec::new()
}

fn build_debug_resume_cmd(resume_token: &str) -> Vec<String> {
    if is_debug_mode() {
        let exe = std::env::current_exe().unwrap_or_else(|_| "clamor".into());
        return vec![
            exe.to_string_lossy().to_string(),
            "mock-agent".to_string(),
            "--description".to_string(),
            format!("resumed: {resume_token}"),
        ];
    }

    Vec::new()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateContext<'a> {
    pub prompt: Option<&'a str>,
    pub title: Option<&'a str>,
    pub folder_id: &'a str,
    pub folder_path: &'a str,
    pub cwd: &'a str,
    pub backend_id: &'a str,
    pub session_id: Option<&'a str>,
    pub resume_token: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub backend_id: String,
    pub title: String,
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn lookup_template_var<'a>(
    ctx: &'a TemplateContext<'a>,
    name: &str,
) -> anyhow::Result<Option<&'a str>> {
    match name {
        "prompt" => Ok(ctx.prompt),
        "title" => Ok(ctx.title),
        "folder_id" => Ok(Some(ctx.folder_id)),
        "folder_path" => Ok(Some(ctx.folder_path)),
        "cwd" => Ok(Some(ctx.cwd)),
        "backend_id" => Ok(Some(ctx.backend_id)),
        "session_id" => Ok(ctx.session_id.or(ctx.resume_token)),
        "resume_token" => Ok(ctx.resume_token.or(ctx.session_id)),
        _ => bail!("Unknown template variable '{{{{{name}}}}}'"),
    }
}

pub fn render_template(
    template: &str,
    ctx: &TemplateContext<'_>,
) -> anyhow::Result<Option<String>> {
    let trimmed = template.trim();
    if trimmed.starts_with("{{") && trimmed.ends_with("}}") {
        let inner = trimmed[2..trimmed.len() - 2].trim();
        if trimmed.len() == inner.len() + 4 {
            return lookup_template_var(ctx, inner).map(|value| value.map(ToString::to_string));
        }
    }

    let mut rendered = String::with_capacity(template.len());
    let mut cursor = 0;

    while let Some(open_offset) = template[cursor..].find("{{") {
        let open = cursor + open_offset;
        rendered.push_str(&template[cursor..open]);

        let close = template[open + 2..]
            .find("}}")
            .map(|offset| open + 2 + offset)
            .with_context(|| format!("Unclosed template variable in '{template}'"))?;

        let name = template[open + 2..close].trim();
        let value = lookup_template_var(ctx, name)?
            .with_context(|| format!("Template variable '{{{{{name}}}}}' requires a value"))?;
        rendered.push_str(value);
        cursor = close + 2;
    }

    rendered.push_str(&template[cursor..]);
    Ok(Some(rendered))
}

type RenderedCommand = (Vec<String>, Vec<(String, String)>, Option<String>);

fn render_command_config(
    command: &BackendCommandConfig,
    ctx: &TemplateContext<'_>,
) -> anyhow::Result<RenderedCommand> {
    let mut cmd = Vec::with_capacity(command.cmd.len());
    for arg in &command.cmd {
        if let Some(rendered) = render_template(arg, ctx)? {
            cmd.push(rendered);
        }
    }

    let mut env = Vec::with_capacity(command.env.len());
    for (key, value) in &command.env {
        if let Some(rendered) = render_template(value, ctx)? {
            env.push((key.clone(), rendered));
        }
    }
    env.sort_by(|a, b| a.0.cmp(&b.0));

    let title = match &command.title_template {
        Some(template) => render_template(template, ctx)?,
        None => None,
    };

    Ok((cmd, env, title))
}

fn selected_backend_id(
    config: &ClamorConfig,
    state: &ClamorState,
    folder_id: &str,
) -> anyhow::Result<String> {
    selected_backend_for_folder(config, state, folder_id)
        .with_context(|| format!("Folder '{folder_id}' has no valid backend selection"))
}

pub fn select_adopt_backend(config: &ClamorConfig, folder_id: &str) -> anyhow::Result<String> {
    let allowed = config
        .folder_backends(folder_id)
        .with_context(|| format!("Unknown folder '{folder_id}'"))?;

    allowed
        .iter()
        .find(|backend_id| backend_supports_resume(config, backend_id))
        .cloned()
        .with_context(|| {
            format!("Folder '{folder_id}' has no resumable backend available for adopt/resume")
        })
}

pub fn backend_supports_resume(config: &ClamorConfig, backend_id: &str) -> bool {
    config
        .backends
        .get(backend_id)
        .is_some_and(|backend| backend.capabilities.resume && backend.resume.is_some())
}

/// Backend can resume without a token (e.g. pi's `--continue` picks up the last session in CWD).
fn backend_supports_tokenless_resume(config: &ClamorConfig, backend_id: &str) -> bool {
    config
        .backends
        .get(backend_id)
        .and_then(|b| b.resume.as_ref())
        .is_some_and(|resume| {
            !resume
                .cmd
                .iter()
                .any(|arg| arg.contains("{{resume_token}}") || arg.contains("{{session_id}}"))
        })
}

pub fn resolve_spawn_launch(
    config: &ClamorConfig,
    state: &ClamorState,
    folder_id: &str,
    folder_path: &str,
    cwd: &str,
    title: &str,
    prompt: Option<&str>,
) -> anyhow::Result<ResolvedLaunch> {
    let backend_id = selected_backend_id(config, state, folder_id)?;

    if is_debug_mode() {
        return Ok(ResolvedLaunch {
            backend_id,
            title: title.to_string(),
            cmd: build_debug_spawn_cmd(prompt),
            env: Vec::new(),
        });
    }

    let backend = config
        .backends
        .get(&backend_id)
        .with_context(|| format!("Unknown backend '{backend_id}'"))?;
    let ctx = TemplateContext {
        prompt,
        title: Some(title),
        folder_id,
        folder_path,
        cwd,
        backend_id: &backend_id,
        session_id: None,
        resume_token: None,
    };
    let (cmd, env, rendered_title) = render_command_config(&backend.spawn, &ctx)?;
    if cmd.is_empty() {
        bail!("Backend '{backend_id}' has an empty spawn command");
    }

    Ok(ResolvedLaunch {
        backend_id,
        title: rendered_title.unwrap_or_else(|| title.to_string()),
        cmd,
        env,
    })
}

pub fn resolve_resume_launch(
    config: &ClamorConfig,
    backend_id: &str,
    folder_id: &str,
    folder_path: &str,
    cwd: &str,
    title: &str,
    resume_token: Option<&str>,
) -> anyhow::Result<ResolvedLaunch> {
    if is_debug_mode() {
        let token = resume_token.unwrap_or("tokenless");
        return Ok(ResolvedLaunch {
            backend_id: backend_id.to_string(),
            title: title.to_string(),
            cmd: build_debug_resume_cmd(token),
            env: Vec::new(),
        });
    }

    let backend = config
        .backends
        .get(backend_id)
        .with_context(|| format!("Unknown backend '{backend_id}'"))?;

    if !backend.capabilities.resume {
        bail!("Backend '{backend_id}' does not support resume");
    }

    let resume = backend
        .resume
        .as_ref()
        .with_context(|| format!("Backend '{backend_id}' is missing a resume command"))?;
    let token = resume_token.filter(|t| !t.is_empty());
    let needs_token = resume
        .cmd
        .iter()
        .any(|arg| arg.contains("{{resume_token}}") || arg.contains("{{session_id}}"));
    if needs_token && token.is_none() {
        bail!("Backend '{backend_id}' resume requires a session token but none is stored for this agent");
    }
    let ctx = TemplateContext {
        prompt: None,
        title: Some(title),
        folder_id,
        folder_path,
        cwd,
        backend_id,
        session_id: token,
        resume_token: token,
    };
    let (cmd, env, rendered_title) = render_command_config(resume, &ctx)?;
    if cmd.is_empty() {
        bail!("Backend '{backend_id}' has an empty resume command");
    }

    Ok(ResolvedLaunch {
        backend_id: backend_id.to_string(),
        title: rendered_title.unwrap_or_else(|| title.to_string()),
        cmd,
        env,
    })
}

pub async fn spawn_agent(
    description: Option<String>,
    folder_override: Option<String>,
    force_editor: bool,
) -> anyhow::Result<()> {
    ensure_daemon()?;
    let config = ClamorConfig::load()?;

    if config.folders.is_empty() {
        bail!(
            "No folders configured. Run `clamor config` to edit folders or `clamor config print-example` for a starter template."
        );
    }

    let (folder_name, folder_path) = match folder_override {
        Some(ref name) => {
            let path = config
                .folder_path(name)
                .with_context(|| format!("Unknown folder: {name}"))?;
            (name.clone(), path.to_string())
        }
        None => tokio::task::block_in_place(|| select_folder(&config))?,
    };

    let cwd = resolve_path(&folder_path);
    let cwd_str = cwd.to_string_lossy().to_string();

    let state = ClamorState::load()?;

    let (title, prompt) = match description {
        Some(d) => (d.clone(), Some(d)),
        None if force_editor => {
            let (t, p) = tokio::task::block_in_place(read_task_from_editor)?;
            (t, Some(p))
        }
        None => tokio::task::block_in_place(read_task_description)?,
    };

    let launch = resolve_spawn_launch(
        &config,
        &state,
        &folder_name,
        &folder_path,
        &cwd_str,
        &title,
        prompt.as_deref(),
    )?;

    let existing_ids: std::collections::HashSet<String> = state.agents.keys().cloned().collect();
    let id = generate_id(&existing_ids);
    let now = Utc::now();

    let existing: Vec<&Agent> = state.agents.values().collect();
    let key = keys::next_available_key(&existing);
    let color_index = next_color_index(&existing);

    let initial_state = if prompt.is_some() {
        AgentState::Working
    } else {
        AgentState::Input
    };

    let agent = Agent {
        id: id.clone(),
        title: launch.title.clone(),
        folder_id: folder_name,
        backend_id: launch.backend_id.clone(),
        cwd: cwd_str.clone(),
        initial_prompt: prompt.clone(),
        state: initial_state,
        started_at: now,
        last_activity_at: now,
        last_tool: None,
        resume_token: None,
        metadata: std::collections::HashMap::new(),
        key,
        color_index,
    };

    with_state(|state| {
        state.agents.insert(id.clone(), agent);
    })?;

    let mut env = launch.env.clone();
    env.push(("CLAMOR_AGENT_ID".to_string(), id.clone()));
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut client = DaemonClient::connect().await?;
    client
        .spawn_agent(&id, &cwd_str, &launch.cmd, &env, term_rows, term_cols)
        .await?;

    println!("Spawned agent {id}: {}", launch.title);

    Ok(())
}

pub async fn adopt_session(
    session_id: &str,
    description: Option<String>,
    folder_override: Option<String>,
) -> anyhow::Result<()> {
    ensure_daemon()?;
    let config = ClamorConfig::load()?;

    if config.folders.is_empty() {
        bail!(
            "No folders configured. Run `clamor config` to edit folders or `clamor config print-example` for a starter template."
        );
    }

    let (folder_name, folder_path) = match folder_override {
        Some(ref name) => {
            let path = config
                .folder_path(name)
                .with_context(|| format!("Unknown folder: {name}"))?;
            (name.clone(), path.to_string())
        }
        None => tokio::task::block_in_place(|| select_folder(&config))?,
    };

    let cwd = resolve_path(&folder_path);
    let cwd_str = cwd.to_string_lossy().to_string();

    let state = ClamorState::load()?;

    let title = match description {
        Some(d) => d,
        None => tokio::task::block_in_place(|| {
            print!("Title: ");
            io::stdout().flush()?;
            let input = read_line()?.trim().to_string();
            if input.is_empty() {
                bail!("Empty title, aborting.");
            }
            Ok(input)
        })?,
    };

    let backend_id = select_adopt_backend(&config, &folder_name)?;
    let launch = resolve_resume_launch(
        &config,
        &backend_id,
        &folder_name,
        &folder_path,
        &cwd_str,
        &title,
        Some(session_id),
    )?;

    let existing_ids: std::collections::HashSet<String> = state.agents.keys().cloned().collect();
    let id = generate_id(&existing_ids);
    let now = Utc::now();

    let existing: Vec<&Agent> = state.agents.values().collect();
    let key = keys::next_available_key(&existing);
    let color_index = next_color_index(&existing);

    let agent = Agent {
        id: id.clone(),
        title: launch.title.clone(),
        folder_id: folder_name,
        backend_id: launch.backend_id.clone(),
        cwd: cwd_str.clone(),
        initial_prompt: None,
        state: AgentState::Input,
        started_at: now,
        last_activity_at: now,
        last_tool: None,
        resume_token: Some(session_id.to_string()),
        metadata: std::collections::HashMap::new(),
        key,
        color_index,
    };

    with_state(|state| {
        state.agents.insert(id.clone(), agent);
    })?;

    let mut env = launch.env.clone();
    env.push(("CLAMOR_AGENT_ID".to_string(), id.clone()));
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut client = DaemonClient::connect().await?;
    client
        .spawn_agent(&id, &cwd_str, &launch.cmd, &env, term_rows, term_cols)
        .await?;

    println!(
        "Adopted session {session_id} as agent {id}: {}",
        launch.title
    );

    Ok(())
}

pub async fn pre_upgrade() -> anyhow::Result<bool> {
    if !daemon::is_daemon_running() {
        return Ok(true);
    }

    let config = ClamorConfig::load()?;
    let state = ClamorState::load()?;
    let total = state.agents.len();

    if total > 0 {
        let resumable: Vec<&Agent> = state
            .agents
            .values()
            .filter(|a| {
                backend_supports_resume(&config, &a.backend_id)
                    && (a.resume_token.is_some()
                        || backend_supports_tokenless_resume(&config, &a.backend_id))
            })
            .collect();
        let lost: Vec<&Agent> = state
            .agents
            .values()
            .filter(|a| {
                !backend_supports_resume(&config, &a.backend_id)
                    || (a.resume_token.is_none()
                        && !backend_supports_tokenless_resume(&config, &a.backend_id))
            })
            .collect();

        println!();
        if lost.is_empty() {
            println!("{total} session(s) — all will auto-resume after upgrade.");
        } else if resumable.is_empty() {
            println!("{total} session(s) will be lost (no usable resume support/token):");
            for a in &lost {
                println!("    {} {}", a.id, a.title);
            }
        } else {
            println!(
                "{} of {total} session(s) will auto-resume after upgrade.",
                resumable.len()
            );
            println!();
            println!(
                "{} will be lost (no usable resume support/token):",
                lost.len()
            );
            for a in &lost {
                println!("    {} {}", a.id, a.title);
            }
        }
        println!();
    }

    let confirmed = tokio::task::block_in_place(|| {
        print!("Proceed? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok::<bool, anyhow::Error>(answer.trim().eq_ignore_ascii_case("y"))
    })?;

    if !confirmed {
        println!("Skipping. Rebuild and restart the daemon later.");
        return Ok(false);
    }

    let mut client = DaemonClient::connect().await?;
    client.shutdown().await?;
    println!("Daemon stopped.");

    Ok(true)
}

pub async fn resume_agents() -> anyhow::Result<()> {
    let config = ClamorConfig::load()?;
    let state = ClamorState::load()?;

    let resumable: Vec<&Agent> = state
        .agents
        .values()
        .filter(|a| {
            backend_supports_resume(&config, &a.backend_id)
                && (a.resume_token.is_some()
                    || backend_supports_tokenless_resume(&config, &a.backend_id))
        })
        .collect();

    if resumable.is_empty() {
        println!("No agents to resume.");
        return Ok(());
    }

    ensure_daemon()?;

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut client = DaemonClient::connect().await?;
    let mut count = 0;
    let mut resumed_ids = Vec::new();

    for agent in &resumable {
        let folder_path = config
            .folder_path(&agent.folder_id)
            .unwrap_or(agent.cwd.as_str());
        let launch = match resolve_resume_launch(
            &config,
            &agent.backend_id,
            &agent.folder_id,
            folder_path,
            &agent.cwd,
            &agent.title,
            agent.resume_token.as_deref(),
        ) {
            Ok(launch) => launch,
            Err(e) => {
                eprintln!("  Skipped {}: {e:#}", agent.id);
                continue;
            }
        };
        let mut env = launch.env.clone();
        env.push(("CLAMOR_AGENT_ID".to_string(), agent.id.clone()));

        match client
            .spawn_agent(
                &agent.id,
                &agent.cwd,
                &launch.cmd,
                &env,
                term_rows,
                term_cols,
            )
            .await
        {
            Ok(()) => {
                count += 1;
                resumed_ids.push(agent.id.clone());
                println!("  Resumed {}: {}", agent.id, agent.title);
            }
            Err(e) => {
                eprintln!("  Failed to resume {}: {e:#}", agent.id);
            }
        }
    }

    with_state(|state| {
        for agent_id in &resumed_ids {
            if let Some(a) = state.agents.get_mut(agent_id) {
                a.state = AgentState::Input;
                a.last_activity_at = chrono::Utc::now();
            }
        }
    })?;

    println!("Resumed {count}/{} agent(s).", resumable.len());

    Ok(())
}

pub async fn kill_agent(agent_ref: &str) -> anyhow::Result<()> {
    ensure_daemon()?;
    let state = ClamorState::load()?;

    let agent = resolve_agent(&state, agent_ref)?.clone();

    let mut client = DaemonClient::connect().await?;
    let _ = client.kill_agent(&agent.id).await;

    with_state(|state| {
        state.agents.remove(&agent.id);
    })?;

    println!("Killed agent {}: {}", agent.id, agent.title);

    Ok(())
}

pub async fn kill_all_agents() -> anyhow::Result<()> {
    ensure_daemon()?;
    let state = ClamorState::load()?;

    let count = state.agents.len();
    let mut client = DaemonClient::connect().await?;
    for agent in state.agents.values() {
        let _ = client.kill_agent(&agent.id).await;
    }

    with_state(|state| {
        state.agents.clear();
    })?;

    println!("Killed {count} agent(s).");

    Ok(())
}

pub fn clean_agents() -> anyhow::Result<()> {
    let removed = with_state(|state| {
        let done_ids: Vec<String> = state
            .agents
            .iter()
            .filter(|(_, a)| a.state == AgentState::Done)
            .map(|(id, _)| id.clone())
            .collect();

        let count = done_ids.len();
        for id in &done_ids {
            state.agents.remove(id);
        }
        count
    })?;

    println!("Removed {removed} finished agent(s).");

    Ok(())
}

pub fn list_agents() -> anyhow::Result<()> {
    let state = ClamorState::load()?;

    if state.agents.is_empty() {
        println!("No agents.");
        return Ok(());
    }

    let mut agents: Vec<&Agent> = state.agents.values().collect();
    agents.sort_by_key(|a| a.started_at);

    let id_w = 6;
    let state_w = 6;
    let desc_w = 40;
    let folder_w = agents
        .iter()
        .map(|a| a.folder_id.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:<id_w$}  {:<state_w$}  {:<desc_w$}  {:<folder_w$}  {:>5}",
        "ID", "STATE", "TITLE", "FOLDER", "TIME",
    );

    for agent in &agents {
        let state_str = match agent.state {
            AgentState::Working => "work",
            AgentState::Input => "input",
            AgentState::Done => "done",
        };
        let desc = truncate(&agent.title, desc_w);
        let time = format_duration(agent.started_at);

        println!(
            "{:<id_w$}  {:<state_w$}  {:<desc_w$}  {:<folder_w$}  {:>5}",
            agent.id, state_str, desc, agent.folder_id, time,
        );
    }

    Ok(())
}

pub fn resolve_agent<'a>(state: &'a ClamorState, agent_ref: &str) -> anyhow::Result<&'a Agent> {
    let matches: Vec<&Agent> = state
        .agents
        .values()
        .filter(|a| a.id.starts_with(agent_ref))
        .collect();

    match matches.len() {
        0 => bail!("no agent matching '{agent_ref}'"),
        1 => Ok(matches[0]),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|a| a.id.as_str()).collect();
            bail!(
                "ambiguous prefix '{agent_ref}' — matches: {}",
                ids.join(", ")
            )
        }
    }
}

pub async fn edit_agent(agent_ref: &str, description: Option<String>) -> anyhow::Result<()> {
    let state = ClamorState::load()?;

    let agent_id = resolve_agent(&state, agent_ref)?.id.clone();

    let new_title = match description {
        Some(d) => d,
        None => tokio::task::block_in_place(|| {
            print!("Title: ");
            io::stdout().flush()?;
            Ok::<String, anyhow::Error>(read_line()?.trim().to_string())
        })?,
    };

    if new_title.is_empty() {
        bail!("Empty title, aborting.");
    }

    with_state(|state| {
        if let Some(agent) = state.agents.get_mut(&agent_id) {
            agent.title = new_title.clone();
        }
    })?;

    println!("Updated title for {agent_id}: {new_title}");
    Ok(())
}

pub fn open_config() -> anyhow::Result<()> {
    let config_path = crate::config::config_path_for_editing()?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor)
        .arg(&config_path)
        .status()
        .with_context(|| format!("Failed to open {editor}"))?;

    if !status.success() {
        bail!("Editor exited with non-zero status");
    }

    Ok(())
}

use crate::dashboard::render::format_duration;

// ── Helpers ────────────────────────────────────────────────────────

fn select_folder(config: &ClamorConfig) -> anyhow::Result<(String, String)> {
    let folders = config.ordered_folders();

    let options: Vec<String> = folders.iter().map(|(name, _)| name.clone()).collect();

    let idx = picker::pick("Where?", &options)?.context("Aborted.")?;

    let (name, path) = &folders[idx];
    Ok((name.clone(), path.clone()))
}

fn read_task_description() -> anyhow::Result<(String, Option<String>)> {
    print!("Title: ");
    io::stdout().flush()?;

    let input = read_line()?.trim().to_string();

    if !input.is_empty() {
        return Ok((input.clone(), Some(input)));
    }

    let (title, prompt) = read_task_from_editor()?;
    Ok((title, Some(prompt)))
}

pub fn read_task_from_editor() -> anyhow::Result<(String, String)> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let tmp = std::env::temp_dir().join(format!(
        "clamor-task-{}.md",
        generate_id(&std::collections::HashSet::new())
    ));

    std::fs::write(&tmp, "")?;

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .with_context(|| format!("Failed to open {editor}"))?;

    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("Editor exited with non-zero status");
    }

    let content = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);

    let content = content.trim().to_string();
    if content.is_empty() {
        bail!("Empty task description, aborting.");
    }

    let description = content.lines().next().unwrap_or("").to_string();

    Ok((description, content))
}

fn read_line() -> anyhow::Result<String> {
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf)
}

fn truncate(s: &str, max_len: usize) -> String {
    if max_len <= 3 {
        return s.chars().take(max_len).collect();
    }
    if s.len() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn test_agent(backend_id: &str, resume_token: Option<&str>) -> Agent {
        Agent {
            id: "existing".to_string(),
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

    fn test_config() -> ClamorConfig {
        serde_yaml::from_str(
            r#"
backends:
  claude-code:
    display_name: Claude
    spawn:
      cmd: [claude, "{{prompt}}"]
      title_template: "{{title}}"
    resume:
      cmd: [claude, --resume, "{{resume_token}}"]
      title_template: "{{title}}"
    capabilities:
      resume: true
  open-code:
    display_name: OpenCode
    spawn:
      cmd: [opencode, run, --prompt, "{{prompt}}"]
      title_template: "{{title}}"
  pi:
    display_name: Pi
    spawn:
      cmd: [pi, "{{prompt}}"]
      title_template: "{{title}}"
    resume:
      cmd: [pi, --continue]
      title_template: "{{title}}"
    capabilities:
      resume: true
folders:
  work:
    path: ~/work
    backends: [claude-code, open-code, pi]
"#,
        )
        .unwrap()
    }

    #[test]
    fn renders_optional_prompt_as_missing_argument() {
        let config = test_config();
        let state = ClamorState::default();

        let launch =
            resolve_spawn_launch(&config, &state, "work", "~/work", "/tmp/work", "task", None)
                .unwrap();

        assert_eq!(launch.backend_id, "claude-code");
        assert_eq!(launch.cmd, vec!["claude"]);
        assert_eq!(launch.title, "task");
    }

    #[test]
    fn fails_when_required_template_value_is_missing() {
        let ctx = TemplateContext {
            prompt: None,
            title: Some("task"),
            folder_id: "work",
            folder_path: "~/work",
            cwd: "/tmp/work",
            backend_id: "claude-code",
            session_id: None,
            resume_token: None,
        };

        let err = render_template("resume {{resume_token}}", &ctx).unwrap_err();
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn pi_resumes_with_continue_flag_without_token() {
        let config = test_config();

        let launch =
            resolve_resume_launch(&config, "pi", "work", "~/work", "/tmp/work", "task", None)
                .unwrap();

        assert_eq!(launch.cmd, vec!["pi", "--continue"]);
        assert_eq!(launch.title, "task");
    }

    #[test]
    fn preserves_claude_resume_parity() {
        let config = test_config();

        let launch = resolve_resume_launch(
            &config,
            "claude-code",
            "work",
            "~/work",
            "/tmp/work",
            "task",
            Some("sess-1"),
        )
        .unwrap();

        assert_eq!(launch.cmd, vec!["claude", "--resume", "sess-1"]);
        assert_eq!(launch.title, "task");
    }

    #[test]
    fn resolves_non_claude_spawn_commands() {
        let config = test_config();
        let mut state = ClamorState::default();
        state.folder_state.insert(
            "work".to_string(),
            crate::state::FolderState {
                selected_backend: Some("open-code".to_string()),
            },
        );

        let launch = resolve_spawn_launch(
            &config,
            &state,
            "work",
            "~/work",
            "/tmp/work",
            "task",
            Some("implement it"),
        )
        .unwrap();

        assert_eq!(launch.backend_id, "open-code");
        assert_eq!(
            launch.cmd,
            vec!["opencode", "run", "--prompt", "implement it"]
        );
    }

    #[test]
    fn rejects_resume_for_non_resumable_backend() {
        let config = test_config();

        let err = resolve_resume_launch(
            &config,
            "open-code",
            "work",
            "~/work",
            "/tmp/work",
            "task",
            Some("sess-1"),
        )
        .unwrap_err();

        assert!(err.to_string().contains("does not support resume"));
    }

    #[test]
    fn adopt_prefers_claude_even_when_folder_selection_differs() {
        let config = test_config();
        let mut state = ClamorState::default();
        state.folder_state.insert(
            "work".to_string(),
            crate::state::FolderState {
                selected_backend: Some("open-code".to_string()),
            },
        );

        assert_eq!(
            selected_backend_id(&config, &state, "work").unwrap(),
            "open-code"
        );
        assert_eq!(
            select_adopt_backend(&config, "work").unwrap(),
            "claude-code"
        );
    }

    #[test]
    fn adopt_uses_first_resumable_backend_when_claude_unavailable() {
        let config: ClamorConfig = serde_yaml::from_str(
            r#"
backends:
  open-code:
    display_name: OpenCode
    spawn:
      cmd: [opencode, run, --prompt, "{{prompt}}"]
  pi:
    display_name: Pi
    spawn:
      cmd: [pi, "{{prompt}}"]
    resume:
      cmd: [pi, resume, "{{resume_token}}"]
    capabilities:
      resume: true
folders:
  work:
    path: ~/work
    backends: [open-code, pi]
"#,
        )
        .unwrap();

        assert_eq!(select_adopt_backend(&config, "work").unwrap(), "pi");
    }

    #[test]
    fn changing_folder_selection_only_affects_future_spawns() {
        let config = test_config();
        let mut state = ClamorState::default();
        state.agents.insert(
            "existing".to_string(),
            test_agent("claude-code", Some("sess-1")),
        );

        let first_launch = resolve_spawn_launch(
            &config,
            &state,
            "work",
            "~/work",
            "/tmp/work",
            "before switch",
            Some("ship it"),
        )
        .unwrap();
        assert_eq!(first_launch.backend_id, "claude-code");

        state.folder_state.insert(
            "work".to_string(),
            crate::state::FolderState {
                selected_backend: Some("open-code".to_string()),
            },
        );

        let second_launch = resolve_spawn_launch(
            &config,
            &state,
            "work",
            "~/work",
            "/tmp/work",
            "after switch",
            Some("ship it"),
        )
        .unwrap();

        assert_eq!(state.agents["existing"].backend_id, "claude-code");
        assert_eq!(second_launch.backend_id, "open-code");
    }

    #[test]
    fn adopt_fails_when_folder_has_no_resumable_backend() {
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

        let err = select_adopt_backend(&config, "work").unwrap_err();
        assert!(err.to_string().contains("no resumable backend available"));
    }
}
