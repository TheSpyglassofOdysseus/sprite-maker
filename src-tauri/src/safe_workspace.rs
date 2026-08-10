use crate::{
    error::{CommandError, CommandResult},
    models::Workspace,
    workspace, AppState,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::State;

const WORKSPACE_OWNER_MARKER: &str = ".sprite-studio/workspace-owner";
const WORKSPACE_OWNER_SCHEMA: &str = "sprite-studio-owned-workspace-v2";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceOwner {
    schema: String,
    workspace_id: String,
    canonical_path: String,
}

#[tauri::command(rename = "create_workspace")]
pub fn safe_create_workspace(
    name: String,
    path: String,
    state: State<'_, AppState>,
) -> CommandResult<Workspace> {
    let workspace = workspace::create_workspace(name, path, state)?;
    let root = PathBuf::from(&workspace.path).canonicalize()?;
    write_owner_marker(&root, &workspace.id)?;
    Ok(workspace)
}

#[tauri::command(rename = "delete_workspace")]
pub fn safe_delete_workspace(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let root = workspace::workspace_path(&state, &id)?;
    if root.parent().is_none() || root.components().count() < 3 {
        return Err(CommandError::new(
            "unsafe_delete",
            "Refusing to delete a broad filesystem path",
        ));
    }
    verify_owner_marker(&root, &id)?;
    std::fs::remove_dir_all(&root)?;
    workspace::remove_workspace(id, state)
}

fn write_owner_marker(root: &Path, workspace_id: &str) -> CommandResult<()> {
    let root = root.canonicalize()?;
    let marker = root.join(WORKSPACE_OWNER_MARKER);
    let parent = marker.parent().ok_or_else(|| {
        CommandError::new(
            "workspace_marker_error",
            "Could not resolve the workspace ownership marker directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let owner = WorkspaceOwner {
        schema: WORKSPACE_OWNER_SCHEMA.into(),
        workspace_id: workspace_id.to_string(),
        canonical_path: root.to_string_lossy().into_owned(),
    };
    std::fs::write(
        marker,
        serde_json::to_vec_pretty(&owner)
            .map_err(|error| CommandError::new("serialization_error", error.to_string()))?,
    )?;
    Ok(())
}

fn verify_owner_marker(root: &Path, workspace_id: &str) -> CommandResult<()> {
    let root = root.canonicalize()?;
    let marker = root.join(WORKSPACE_OWNER_MARKER);
    if !marker.is_file() {
        return Err(CommandError::new(
            "unsafe_delete",
            "This folder is not an owned Sprite Studio workspace. Remove it from recents instead of deleting it from disk.",
        ));
    }
    let owner: WorkspaceOwner = serde_json::from_slice(&std::fs::read(marker)?)
        .map_err(|_| CommandError::new("unsafe_delete", "Workspace ownership marker is invalid"))?;
    let canonical = root.to_string_lossy();
    if owner.schema != WORKSPACE_OWNER_SCHEMA
        || owner.workspace_id != workspace_id
        || owner.canonical_path != canonical
    {
        return Err(CommandError::new(
            "unsafe_delete",
            "Workspace ownership marker does not match this registered workspace and path",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{verify_owner_marker, write_owner_marker};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn owner_marker_is_bound_to_workspace_id_and_path() {
        let root = std::env::temp_dir().join(format!("sprite-studio-owner-v2-{}", Uuid::new_v4()));
        let other =
            std::env::temp_dir().join(format!("sprite-studio-owner-v2-other-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("root creates");
        fs::create_dir_all(&other).expect("other creates");
        write_owner_marker(&root, "workspace-a").expect("marker writes");
        assert!(verify_owner_marker(&root, "workspace-a").is_ok());
        assert!(verify_owner_marker(&root, "workspace-b").is_err());
        fs::copy(
            root.join(".sprite-studio/workspace-owner"),
            other.join(".sprite-studio/workspace-owner"),
        )
        .expect_err("copy should fail until destination metadata directory exists");
        fs::create_dir_all(other.join(".sprite-studio")).expect("metadata creates");
        fs::copy(
            root.join(".sprite-studio/workspace-owner"),
            other.join(".sprite-studio/workspace-owner"),
        )
        .expect("marker copies");
        assert!(verify_owner_marker(&other, "workspace-a").is_err());
        fs::remove_dir_all(root).expect("root cleans");
        fs::remove_dir_all(other).expect("other cleans");
    }
}
