use crate::{
    error::{CommandError, CommandResult},
    models::Workspace,
    AppState,
};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::State;
use uuid::Uuid;

const WORKSPACE_MARKER_VERSION: u32 = 1;
const WORKSPACE_MARKER_FILE: &str = "workspace.json";

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceMarker {
    schema_version: u32,
    workspace_id: String,
    canonical_path: String,
    deletion_allowed: bool,
}

fn row_to_workspace(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        created_at: row.get(3)?,
        last_opened_at: row.get(4)?,
    })
}

fn workspace_marker_path(path: &Path) -> PathBuf {
    path.join(".sprite-studio").join(WORKSPACE_MARKER_FILE)
}

fn read_workspace_marker(path: &Path) -> CommandResult<WorkspaceMarker> {
    let marker_path = workspace_marker_path(path);
    let bytes = std::fs::read(&marker_path).map_err(|_| {
        CommandError::new(
            "workspace_marker_missing",
            "This directory is not marked as a Sprite Studio-managed workspace",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        CommandError::new(
            "workspace_marker_invalid",
            "The Sprite Studio workspace ownership marker is invalid",
        )
    })
}

fn write_workspace_marker(
    path: &Path,
    workspace_id: &str,
    deletion_allowed: bool,
) -> CommandResult<()> {
    let canonical = path.canonicalize()?;
    let canonical_path = canonical.to_string_lossy().into_owned();
    let marker_path = workspace_marker_path(&canonical);
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing_allows_delete = std::fs::read(&marker_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<WorkspaceMarker>(&bytes).ok())
        .is_some_and(|marker| {
            marker.schema_version == WORKSPACE_MARKER_VERSION
                && marker.workspace_id == workspace_id
                && marker.canonical_path == canonical_path
                && marker.deletion_allowed
        });

    let marker = WorkspaceMarker {
        schema_version: WORKSPACE_MARKER_VERSION,
        workspace_id: workspace_id.to_string(),
        canonical_path,
        deletion_allowed: deletion_allowed || existing_allows_delete,
    };
    std::fs::write(
        marker_path,
        serde_json::to_vec_pretty(&marker)
            .map_err(|error| CommandError::new("serialization_error", error.to_string()))?,
    )?;
    Ok(())
}

fn validate_workspace_delete(path: &Path, workspace_id: &str) -> CommandResult<()> {
    let canonical = path.canonicalize()?;
    if canonical.parent().is_none() || canonical.components().count() < 3 {
        return Err(CommandError::new(
            "unsafe_delete",
            "Refusing to delete a broad filesystem path",
        ));
    }

    let marker = read_workspace_marker(&canonical)?;
    let canonical_path = canonical.to_string_lossy();
    if marker.schema_version != WORKSPACE_MARKER_VERSION
        || marker.workspace_id != workspace_id
        || marker.canonical_path != canonical_path
    {
        return Err(CommandError::new(
            "workspace_marker_mismatch",
            "Workspace ownership marker does not match this registered directory",
        ));
    }
    if !marker.deletion_allowed {
        return Err(CommandError::new(
            "workspace_delete_not_managed",
            "This directory was opened as an existing folder, so Sprite Studio will not recursively delete it. Remove it from the workspace list instead, or delete it manually after reviewing its contents.",
        ));
    }
    Ok(())
}

fn initialize_workspace(path: &Path) -> CommandResult<()> {
    for folder in [
        "assets/characters",
        "assets/creatures",
        "assets/terrain",
        "assets/props",
        "assets/effects",
        "animations",
        "exports",
        "worktrees",
    ] {
        std::fs::create_dir_all(path.join(folder))?;
    }
    let metadata = path.join(".sprite-studio");
    std::fs::create_dir_all(&metadata)?;
    let gitignore = metadata.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(gitignore, "thumbnails/\n")?;
    }
    let sprite_tool = metadata.join("sprite_tool.py");
    let bundled_tool = include_str!("../resources/sprite_tool.py");
    if !sprite_tool.exists() || std::fs::read_to_string(&sprite_tool)? != bundled_tool {
        std::fs::write(sprite_tool, bundled_tool)?;
    }
    let rig_tool = metadata.join("sprite_rig.py");
    let bundled_rig_tool = include_str!("../resources/sprite_rig.py");
    if !rig_tool.exists() || std::fs::read_to_string(&rig_tool)? != bundled_rig_tool {
        std::fs::write(rig_tool, bundled_rig_tool)?;
    }
    Ok(())
}

fn normalized_path(path: &str, create: bool) -> CommandResult<PathBuf> {
    let candidate = PathBuf::from(path);
    if create {
        std::fs::create_dir_all(&candidate)?;
    }
    if !candidate.is_dir() {
        return Err(CommandError::new(
            "workspace_missing",
            format!("Workspace directory does not exist: {path}"),
        ));
    }
    candidate.canonicalize().map_err(CommandError::from)
}

pub fn workspace_path(state: &AppState, workspace_id: &str) -> CommandResult<PathBuf> {
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let path: Option<String> = connection
        .query_row(
            "SELECT path FROM projects WHERE id = ?1",
            [workspace_id],
            |row| row.get(0),
        )
        .optional()?;
    let path = path.ok_or_else(|| {
        CommandError::new("workspace_not_found", "Workspace is no longer registered")
    })?;
    let path = normalized_path(&path, false)?;
    initialize_workspace(&path)?;
    Ok(path)
}

#[tauri::command]
pub fn list_workspaces(state: State<'_, AppState>) -> CommandResult<Vec<Workspace>> {
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let mut statement = connection.prepare("SELECT id, name, path, created_at, last_opened_at FROM projects ORDER BY last_opened_at DESC")?;
    let rows = statement.query_map([], row_to_workspace)?;
    Ok(rows.filter_map(Result::ok).collect())
}

#[tauri::command]
pub fn create_workspace(
    name: String,
    path: String,
    state: State<'_, AppState>,
) -> CommandResult<Workspace> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::new(
            "invalid_name",
            "Workspace name cannot be empty",
        ));
    }
    let candidate = PathBuf::from(&path);
    let existed_before = candidate.exists();
    let path = normalized_path(&path, true)?;
    initialize_workspace(&path)?;
    register_workspace(name, &path, &state, !existed_before)
}

#[tauri::command]
pub fn open_workspace(path: String, state: State<'_, AppState>) -> CommandResult<Workspace> {
    let path = normalized_path(&path, false)?;
    initialize_workspace(&path)?;
    let fallback_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Untitled workspace");
    register_workspace(fallback_name, &path, &state, false)
}

fn register_workspace(
    name: &str,
    path: &Path,
    state: &AppState,
    deletion_allowed: bool,
) -> CommandResult<Workspace> {
    let now = Utc::now().to_rfc3339();
    let path_string = path.to_string_lossy().into_owned();
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let existing: Option<Workspace> = connection
        .query_row(
            "SELECT id, name, path, created_at, last_opened_at FROM projects WHERE path = ?1",
            [&path_string],
            row_to_workspace,
        )
        .optional()?;
    if let Some(mut workspace) = existing {
        connection.execute(
            "UPDATE projects SET last_opened_at = ?1 WHERE id = ?2",
            params![now, workspace.id],
        )?;
        ensure_default_worktree(&connection, &workspace.id, &now)?;
        write_workspace_marker(path, &workspace.id, deletion_allowed)?;
        workspace.last_opened_at = now;
        return Ok(workspace);
    }
    let workspace = Workspace {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        path: path_string,
        created_at: now.clone(),
        last_opened_at: now,
    };
    connection.execute(
        "INSERT INTO projects(id, name, path, created_at, last_opened_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![workspace.id, workspace.name, workspace.path, workspace.created_at, workspace.last_opened_at],
    )?;
    ensure_default_worktree(&connection, &workspace.id, &workspace.created_at)?;
    write_workspace_marker(path, &workspace.id, deletion_allowed)?;
    Ok(workspace)
}

fn ensure_default_worktree(
    connection: &rusqlite::Connection,
    project_id: &str,
    timestamp: &str,
) -> CommandResult<()> {
    connection.execute(
        "INSERT OR IGNORE INTO worktrees(id, project_id, name, slug, kind, description, created_at, updated_at) VALUES (?1, ?2, 'General', 'general', 'general', 'Project-level assets and conversations', ?3, ?3)",
        params![Uuid::new_v4().to_string(), project_id, timestamp],
    )?;
    Ok(())
}

#[tauri::command]
pub fn touch_workspace(id: String, state: State<'_, AppState>) -> CommandResult<Workspace> {
    let now = Utc::now().to_rfc3339();
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    connection.execute(
        "UPDATE projects SET last_opened_at = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    let workspace = connection
        .query_row(
            "SELECT id, name, path, created_at, last_opened_at FROM projects WHERE id = ?1",
            [&id],
            row_to_workspace,
        )
        .optional()?
        .ok_or_else(|| {
            CommandError::new("workspace_not_found", "Workspace is no longer registered")
        })?;
    if !Path::new(&workspace.path).is_dir() {
        return Err(CommandError::new(
            "workspace_missing",
            "The workspace directory was moved or deleted",
        ));
    }
    Ok(workspace)
}

#[tauri::command]
pub fn rename_workspace(id: String, name: String, state: State<'_, AppState>) -> CommandResult<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::new(
            "invalid_name",
            "Workspace name cannot be empty",
        ));
    }
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let changed = connection.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2",
        params![name, id],
    )?;
    if changed == 0 {
        return Err(CommandError::new(
            "workspace_not_found",
            "Workspace is no longer registered",
        ));
    }
    Ok(())
}

#[tauri::command]
pub fn remove_workspace(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    connection.execute("DELETE FROM projects WHERE id = ?1", [id])?;
    Ok(())
}

#[tauri::command]
pub fn delete_workspace(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let path = workspace_path(&state, &id)?;
    validate_workspace_delete(&path, &id)?;
    std::fs::remove_dir_all(&path)?;
    remove_workspace(id, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database;
    use std::{collections::HashMap, sync::Mutex};

    fn fixture() -> (PathBuf, AppState) {
        let root =
            std::env::temp_dir().join(format!("sprite-studio-workspace-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temporary directory should be created");
        let connection = database::open(&root.join("app.sqlite3")).expect("database should open");
        (
            root,
            AppState {
                db: Mutex::new(connection),
                cancellers: Mutex::new(HashMap::new()),
            },
        )
    }

    #[test]
    fn missing_workspace_returns_a_typed_error() {
        let missing =
            std::env::temp_dir().join(format!("missing-sprite-workspace-{}", Uuid::new_v4()));
        let error = normalized_path(missing.to_string_lossy().as_ref(), false)
            .expect_err("missing directory must fail");
        assert_eq!(error.code, "workspace_missing");
    }

    #[test]
    fn duplicate_workspace_open_reuses_the_registration() {
        let (root, state) = fixture();
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("project should exist");
        let first =
            register_workspace("Test", &project, &state, false).expect("workspace should register");
        let second = register_workspace("Test again", &project, &state, false)
            .expect("duplicate open should be graceful");
        assert_eq!(first.id, second.id);
        let count: i64 = state
            .db
            .lock()
            .expect("database lock")
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .expect("count should work");
        assert_eq!(count, 1);
        let worktree_count: i64 = state
            .db
            .lock()
            .expect("database lock")
            .query_row(
                "SELECT COUNT(*) FROM worktrees WHERE project_id = ?1",
                [&first.id],
                |row| row.get(0),
            )
            .expect("default worktree count should work");
        assert_eq!(worktree_count, 1);
        drop(state);
        std::fs::remove_dir_all(root).expect("temporary fixture should be removable");
    }

    #[test]
    fn managed_workspace_marker_allows_exact_registered_delete_target() {
        let root =
            std::env::temp_dir().join(format!("sprite-studio-marker-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temporary directory should be created");
        initialize_workspace(&root).expect("workspace should initialize");
        let id = Uuid::new_v4().to_string();
        write_workspace_marker(&root, &id, true).expect("marker should write");
        validate_workspace_delete(&root, &id).expect("managed marker should validate");
        std::fs::remove_dir_all(root).expect("temporary fixture should be removable");
    }

    #[test]
    fn opened_existing_workspace_is_not_recursively_deletable() {
        let root =
            std::env::temp_dir().join(format!("sprite-studio-opened-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temporary directory should be created");
        initialize_workspace(&root).expect("workspace should initialize");
        let id = Uuid::new_v4().to_string();
        write_workspace_marker(&root, &id, false).expect("marker should write");
        let error = validate_workspace_delete(&root, &id)
            .expect_err("opened folders must not be recursively deletable");
        assert_eq!(error.code, "workspace_delete_not_managed");
        std::fs::remove_dir_all(root).expect("temporary fixture should be removable");
    }

    #[test]
    fn mismatched_workspace_marker_blocks_delete() {
        let root =
            std::env::temp_dir().join(format!("sprite-studio-mismatch-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temporary directory should be created");
        initialize_workspace(&root).expect("workspace should initialize");
        write_workspace_marker(&root, "workspace-a", true).expect("marker should write");
        let error = validate_workspace_delete(&root, "workspace-b")
            .expect_err("mismatched marker must block delete");
        assert_eq!(error.code, "workspace_marker_mismatch");
        std::fs::remove_dir_all(root).expect("temporary fixture should be removable");
    }

    #[test]
    fn initialization_installs_the_codex_sprite_renderers() {
        let root =
            std::env::temp_dir().join(format!("sprite-studio-renderer-test-{}", Uuid::new_v4()));
        initialize_workspace(&root).expect("workspace should initialize");
        let tool = root.join(".sprite-studio/sprite_tool.py");
        assert!(tool.is_file());
        assert!(std::fs::read_to_string(tool)
            .expect("tool should be readable")
            .contains("Small dependency-free pixel sprite renderer"));
        let rig_tool = root.join(".sprite-studio/sprite_rig.py");
        assert!(rig_tool.is_file());
        assert!(std::fs::read_to_string(rig_tool)
            .expect("rig tool should be readable")
            .contains("layered pixel-rig renderer"));
        assert!(root.join("assets/characters").is_dir());
        assert!(root.join("assets/creatures").is_dir());
        std::fs::remove_dir_all(root).expect("temporary fixture should be removable");
    }
}
