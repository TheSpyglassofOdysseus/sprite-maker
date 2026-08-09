use crate::{
    conversations::{add_message, get_conversation, set_provider_session, update_message},
    error::{CommandError, CommandResult},
    generation_staging::GenerationStage,
    models::{GenerationOptions, ProviderEvent, ProviderRequestOptions, ProviderStatus},
    providers,
    safe_references,
    sprite_harness::studio_prompt,
    workspace::workspace_path,
    AppState,
};
use std::{
    path::PathBuf,
    process::Stdio,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::oneshot,
};
use uuid::Uuid;

#[tauri::command]
pub fn detect_providers() -> Vec<ProviderStatus> {
    let mut values = providers::detect_providers();
    if let Some(codex) = values.iter_mut().find(|provider| provider.id == "codex") {
        if codex.installed {
            codex.detail = "Installed and available through isolated generation staging".into();
        }
    }
    values
}

fn codex_executable() -> CommandResult<PathBuf> {
    providers::detect_providers()
        .into_iter()
        .find(|provider| provider.id == "codex" && provider.installed)
        .and_then(|provider| provider.executable.map(PathBuf::from))
        .ok_or_else(|| {
            CommandError::new(
                "provider_unavailable",
                "Codex CLI was not found. Install and authenticate it, then retry detection.",
            )
        })
}

fn codex_reference_limit() -> usize {
    providers::detect_providers()
        .into_iter()
        .find(|provider| provider.id == "codex")
        .map(|provider| provider.capabilities.maximum_reference_images as usize)
        .unwrap_or(5)
}

fn emit(
    app: &AppHandle,
    request_id: &str,
    conversation_id: &str,
    event_type: &str,
    content: impl Into<String>,
) {
    let _ = app.emit(
        "provider-event",
        ProviderEvent {
            request_id: request_id.to_string(),
            conversation_id: conversation_id.to_string(),
            event_type: event_type.to_string(),
            content: content.into(),
        },
    );
}

fn parse_codex_line(line: &str) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return (Some(line.to_string()), None, None);
    };
    let event_type = value
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("activity");
    if event_type == "thread.started" {
        return (
            None,
            Some(format!(
                "Session {} started",
                value
                    .get("thread_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
            )),
            value
                .get("thread_id")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        );
    }
    if event_type == "item.completed" || event_type == "item.updated" {
        if let Some(item) = value.get("item") {
            let item_type = item
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("activity");
            if item_type == "agent_message" {
                return (
                    item.get("text")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    None,
                    None,
                );
            }
            let label = item
                .get("command")
                .and_then(|value| value.as_str())
                .or_else(|| item.get("name").and_then(|value| value.as_str()))
                .unwrap_or(item_type);
            return (
                None,
                Some(format!("{}: {}", item_type.replace('_', " "), label)),
                None,
            );
        }
    }
    if event_type == "turn.failed" || event_type == "error" {
        let message = value
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or(line);
        return (None, Some(message.to_string()), None);
    }
    (None, None, None)
}

#[tauri::command]
pub fn start_provider_message(
    conversation_id: String,
    prompt: String,
    context: Option<String>,
    options: Option<ProviderRequestOptions>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(CommandError::new(
            "empty_prompt",
            "Write a message before sending",
        ));
    }
    let conversation = get_conversation(&state, &conversation_id)?;
    if conversation.provider != "codex" {
        return Err(CommandError::new(
            "provider_unsupported",
            "This provider adapter is not available yet",
        ));
    }
    let executable = codex_executable()?;
    let options = options.unwrap_or_default();
    validate_provider_options(&options)?;
    let reference_context = safe_references::prompt_context(
        &state,
        &conversation_id,
        &options.reference_ids,
        codex_reference_limit(),
    )?;
    let combined_context = [context.as_deref().unwrap_or(""), reference_context.as_str()]
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    workspace_path(&state, &conversation.workspace_id)?;
    add_message(
        &state,
        &conversation_id,
        "user",
        "text",
        &prompt,
        "completed",
    )?;
    let assistant = add_message(&state, &conversation_id, "assistant", "text", "", "running")?;
    let request_id = Uuid::new_v4().to_string();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    state
        .cancellers
        .lock()
        .map_err(|_| {
            CommandError::new("process_error", "Provider process registry is unavailable")
        })?
        .insert(request_id.clone(), cancel_tx);

    let run = ProviderRun {
        request_id: request_id.clone(),
        conversation_id,
        workspace_id: conversation.workspace_id,
        session_id: conversation.provider_session_id,
        assistant_id: assistant.id,
        prompt: studio_prompt(
            &prompt,
            (!combined_context.is_empty()).then_some(combined_context.as_str()),
            options.generation.as_ref(),
            options.command.as_deref(),
        ),
        model: options.model,
        reasoning_effort: options.reasoning_effort,
        executable,
    };
    tauri::async_runtime::spawn(async move {
        run_codex(app, run, cancel_rx).await;
    });
    Ok(request_id)
}

struct ProviderRun {
    request_id: String,
    conversation_id: String,
    workspace_id: String,
    session_id: Option<String>,
    assistant_id: String,
    prompt: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    executable: PathBuf,
}

async fn run_codex(app: AppHandle, run: ProviderRun, mut cancel_rx: oneshot::Receiver<()>) {
    let ProviderRun {
        request_id,
        conversation_id,
        workspace_id,
        session_id,
        assistant_id,
        prompt,
        model,
        reasoning_effort,
        executable,
    } = run;
    let state = app.state::<AppState>();
    emit(
        &app,
        &request_id,
        &conversation_id,
        "started",
        "Codex is working in an isolated staging workspace",
    );
    let real_workspace = match workspace_path(&state, &workspace_id) {
        Ok(path) => path,
        Err(error) => {
            finish_failed(&app, &state, &request_id, &conversation_id, &assistant_id, error.message);
            return;
        }
    };
    let stage = match GenerationStage::prepare(
        &real_workspace,
        &workspace_id,
        &conversation_id,
        &request_id,
    ) {
        Ok(stage) => stage,
        Err(error) => {
            finish_failed(&app, &state, &request_id, &conversation_id, &assistant_id, error.message);
            return;
        }
    };
    let staged_prompt = stage.rewrite_prompt(&prompt);
    let arguments = codex_arguments(
        session_id.as_deref(),
        model.as_deref(),
        reasoning_effort.as_deref(),
    );
    let mut child = match Command::new(executable)
        .args(&arguments)
        .current_dir(stage.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            finish_failed(
                &app,
                &state,
                &request_id,
                &conversation_id,
                &assistant_id,
                format!("Could not start Codex CLI: {error}"),
            );
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(staged_prompt.as_bytes()).await {
            finish_failed(
                &app,
                &state,
                &request_id,
                &conversation_id,
                &assistant_id,
                format!("Could not send the prompt to Codex: {error}"),
            );
            return;
        }
    }
    let stderr = child.stderr.take();
    let stderr_task = tauri::async_runtime::spawn(async move {
        let mut output = String::new();
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&line);
            }
        }
        output
    });
    let mut response = String::new();
    let mut cancelled = false;
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                _ = &mut cancel_rx => {
                    cancelled = true;
                    let _ = child.kill().await;
                    break;
                }
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        let (text, activity, provider_session_id) = parse_codex_line(&line);
                        if let Some(text) = text {
                            if !response.is_empty() && !response.ends_with('\n') {
                                response.push('\n');
                            }
                            response.push_str(&text);
                            emit(&app, &request_id, &conversation_id, "content", text);
                        }
                        if let Some(activity) = activity {
                            emit(&app, &request_id, &conversation_id, "activity", activity);
                        }
                        if let Some(provider_session_id) = provider_session_id {
                            let _ = set_provider_session(&state, &conversation_id, &provider_session_id);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        emit(
                            &app,
                            &request_id,
                            &conversation_id,
                            "activity",
                            format!("Output stream error: {error}"),
                        );
                        break;
                    }
                }
            }
        }
    }
    let status = child.wait().await;
    let stderr_output = stderr_task.await.unwrap_or_default();
    remove_canceller(&state, &request_id);

    if cancelled {
        let message = if response.is_empty() { "Request cancelled" } else { &response };
        let _ = update_message(&state, &assistant_id, message, "cancelled");
        emit(&app, &request_id, &conversation_id, "cancelled", "Request cancelled");
        return;
    }

    match status {
        Ok(exit) if exit.success() => {
            match stage.promote_if_present() {
                Ok(Some(manifest)) => {
                    emit(
                        &app,
                        &request_id,
                        &conversation_id,
                        "activity",
                        format!("Validated and promoted {} generated file(s)", manifest.files.len()),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    finish_failed(
                        &app,
                        &state,
                        &request_id,
                        &conversation_id,
                        &assistant_id,
                        format!("Generation output was rejected: {}", error.message),
                    );
                    return;
                }
            }
            if response.trim().is_empty() {
                response = "Codex completed without returning a text response.".into();
            }
            let _ = update_message(&state, &assistant_id, &response, "completed");
            emit(&app, &request_id, &conversation_id, "completed", response);
            stage.cleanup();
        }
        Ok(exit) => {
            let message = if stderr_output.trim().is_empty() {
                format!("Codex exited with status {exit}")
            } else {
                stderr_output
            };
            finish_failed(&app, &state, &request_id, &conversation_id, &assistant_id, message);
        }
        Err(error) => {
            finish_failed(
                &app,
                &state,
                &request_id,
                &conversation_id,
                &assistant_id,
                format!("Could not observe the Codex process: {error}"),
            );
        }
    }
}

fn finish_failed(
    app: &AppHandle,
    state: &State<'_, AppState>,
    request_id: &str,
    conversation_id: &str,
    assistant_id: &str,
    message: String,
) {
    remove_canceller(state, request_id);
    let _ = update_message(state, assistant_id, &message, "failed");
    emit(app, request_id, conversation_id, "failed", message);
}

fn remove_canceller(state: &State<'_, AppState>, request_id: &str) {
    if let Ok(mut values) = state.cancellers.lock() {
        values.remove(request_id);
    }
}

fn codex_arguments(
    session_id: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Vec<String> {
    let mut arguments = if session_id.is_some() {
        vec![
            "exec".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "resume".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
        ]
    } else {
        vec![
            "exec".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
        ]
    };
    if let Some(model) = model {
        arguments.extend(["--model".into(), model.into()]);
    }
    if let Some(reasoning_effort) = reasoning_effort {
        arguments.extend([
            "--config".into(),
            format!("model_reasoning_effort=\"{reasoning_effort}\""),
        ]);
    }
    if let Some(session_id) = session_id {
        arguments.push(session_id.into());
    }
    arguments.push("-".into());
    arguments
}

fn validate_provider_options(options: &ProviderRequestOptions) -> CommandResult<()> {
    if let Some(model) = options.model.as_deref() {
        if model.is_empty()
            || model.len() > 96
            || !model
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        {
            return Err(CommandError::new(
                "invalid_provider_model",
                "Choose a model reported by the selected provider",
            ));
        }
    }
    if let Some(effort) = options.reasoning_effort.as_deref() {
        if !matches!(
            effort,
            "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        ) {
            return Err(CommandError::new(
                "invalid_reasoning_effort",
                "Choose a reasoning level reported by the selected model",
            ));
        }
    }
    if let Some(command) = options.command.as_deref() {
        if !matches!(command, "animate" | "sprite" | "character" | "effect") {
            return Err(CommandError::new(
                "invalid_slash_command",
                "Choose a supported Sprite Studio slash command",
            ));
        }
    }
    if let Some(generation) = options.generation.as_ref() {
        validate_generation_options(generation)?;
    }
    Ok(())
}

fn validate_generation_options(generation: &GenerationOptions) -> CommandResult<()> {
    if !matches!(generation.quality.as_str(), "low" | "mid" | "high" | "custom")
        || !(8..=512).contains(&generation.width)
        || !(8..=512).contains(&generation.height)
        || !(1..=32).contains(&generation.frames)
        || !(1..=60).contains(&generation.fps)
        || !matches!(generation.frame_mode.as_str(), "fixed" | "auto")
        || !(1..=32).contains(&generation.min_frames)
        || !(1..=32).contains(&generation.max_frames)
        || generation.min_frames > generation.max_frames
    {
        return Err(CommandError::new(
            "invalid_generation_profile",
            "Use an 8–512 px canvas, Fixed or Auto frames within 1–32, and 1–60 FPS",
        ));
    }
    Ok(())
}

#[tauri::command]
pub fn cancel_provider_request(
    request_id: String,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    providers::cancel_provider_request(request_id, state)
}

#[cfg(test)]
mod tests {
    use super::{codex_arguments, parse_codex_line, validate_provider_options};
    use crate::models::{GenerationOptions, ProviderRequestOptions};

    #[test]
    fn parses_streamed_agent_messages() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"Created the sprite metadata."}}"#;
        let (content, activity, _) = parse_codex_line(line);
        assert_eq!(content.as_deref(), Some("Created the sprite metadata."));
        assert!(activity.is_none());
    }

    #[test]
    fn keeps_workspace_write_sandbox_but_only_inside_staging() {
        assert_eq!(
            codex_arguments(Some("session-123"), Some("gpt-5.6-terra"), Some("high")),
            [
                "exec",
                "--sandbox",
                "workspace-write",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "--model",
                "gpt-5.6-terra",
                "--config",
                "model_reasoning_effort=\"high\"",
                "session-123",
                "-"
            ]
        );
    }

    #[test]
    fn validates_chat_generation_and_provider_modes() {
        let options = ProviderRequestOptions {
            model: Some("gpt-5.6-sol".into()),
            reasoning_effort: Some("xhigh".into()),
            command: Some("animate".into()),
            generation: Some(GenerationOptions {
                quality: "high".into(),
                width: 128,
                height: 128,
                frames: 8,
                fps: 12,
                frame_mode: "auto".into(),
                min_frames: 4,
                max_frames: 12,
                allow_interpolation: false,
                allow_auto_adjust: true,
            }),
            reference_ids: Vec::new(),
        };
        assert!(validate_provider_options(&options).is_ok());
    }
}
