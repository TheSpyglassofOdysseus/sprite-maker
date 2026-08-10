use crate::{
    error::{CommandError, CommandResult},
    models::SpriteSheet,
    workspace::workspace_path,
    AppState,
};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};
use tauri::{Manager, State};

fn sheet_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SpriteSheet> {
    Ok(SpriteSheet {
        id: row.get(0)?,
        project_id: row.get(1)?,
        worktree_id: row.get(2)?,
        animation_id: row.get(3)?,
        name: row.get(4)?,
        layout: row.get(5)?,
        frame_width: row.get(6)?,
        frame_height: row.get(7)?,
        padding: row.get(8)?,
        spacing: row.get(9)?,
        rows: row.get(10)?,
        columns: row.get(11)?,
        scale: row.get(12)?,
        transparent: row.get(13)?,
        alignment: row.get(14)?,
        pivot_x: row.get(15)?,
        pivot_y: row.get(16)?,
        png_path: row.get(17)?,
        metadata_path: row.get(18)?,
        width: row.get(19)?,
        height: row.get(20)?,
        frame_count: row.get(21)?,
        created_at: row.get(22)?,
        updated_at: row.get(23)?,
    })
}

fn select_sheet() -> &'static str {
    r#"SELECT id, project_id, worktree_id, animation_id, name, layout,
              frame_width, frame_height, padding, spacing, rows, columns, scale,
              transparent, alignment, pivot_x, pivot_y, png_path, metadata_path,
              width, height, frame_count, created_at, updated_at
       FROM sprite_sheets"#
}

fn sheet_root(root: &Path) -> CommandResult<PathBuf> {
    let directory = root.join("exports").join("sprite-sheets");
    std::fs::create_dir_all(&directory)?;
    directory.canonicalize().map_err(Into::into)
}

fn validated_sheet_path(root: &Path, path: &str) -> CommandResult<Option<PathBuf>> {
    if path.trim().is_empty() {
        return Ok(None);
    }
    let candidate = PathBuf::from(path);
    if !candidate.exists() {
        return Ok(None);
    }
    let expected = sheet_root(root)?;
    let canonical = candidate.canonicalize()?;
    if !canonical.starts_with(&expected) {
        return Err(CommandError::new(
            "unsafe_sheet_path",
            "Stored sprite-sheet path resolves outside this project's exports directory",
        ));
    }
    Ok(Some(canonical))
}

#[tauri::command]
pub fn list_sprite_sheets_safe(
    project_id: String,
    worktree_id: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Vec<SpriteSheet>> {
    let root = workspace_path(&state, &project_id)?;
    let sheets: Vec<SpriteSheet> = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        if let Some(worktree_id) = worktree_id {
            let mut statement = connection.prepare(&format!(
                "{} WHERE project_id=?1 AND worktree_id=?2 ORDER BY updated_at DESC",
                select_sheet()
            ))?;
            let rows = statement.query_map(params![project_id, worktree_id], sheet_row)?;
            rows.filter_map(Result::ok).collect()
        } else {
            let mut statement = connection.prepare(&format!(
                "{} WHERE project_id=?1 ORDER BY updated_at DESC",
                select_sheet()
            ))?;
            let rows = statement.query_map([project_id], sheet_row)?;
            rows.filter_map(Result::ok).collect()
        }
    };

    for sheet in &sheets {
        if let Some(path) = validated_sheet_path(&root, &sheet.png_path)? {
            app.asset_protocol_scope()
                .allow_file(&path)
                .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
        }
        if let Some(path) = validated_sheet_path(&root, &sheet.metadata_path)? {
            app.asset_protocol_scope()
                .allow_file(&path)
                .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
        }
    }
    Ok(sheets)
}

#[tauri::command]
pub fn delete_sprite_sheet_safe(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let stored: Option<(String, String, String)> = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        connection
            .query_row(
                "SELECT project_id,png_path,metadata_path FROM sprite_sheets WHERE id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
    };
    let (project_id, png_path, metadata_path) = stored.ok_or_else(|| {
        CommandError::new(
            "sprite_sheet_not_found",
            "The sprite sheet no longer exists",
        )
    })?;
    let root = workspace_path(&state, &project_id)?;
    let png = validated_sheet_path(&root, &png_path)?;
    let metadata = validated_sheet_path(&root, &metadata_path)?;

    if let Some(path) = png {
        std::fs::remove_file(path)?;
    }
    if let Some(path) = metadata {
        std::fs::remove_file(path)?;
    }

    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    connection.execute("DELETE FROM sprite_sheets WHERE id=?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validated_sheet_path;
    use uuid::Uuid;

    #[test]
    fn sheet_path_must_resolve_under_managed_exports() {
        let root = std::env::temp_dir().join(format!("sprite-sheet-safe-{}", Uuid::new_v4()));
        let sheets = root.join("exports/sprite-sheets");
        std::fs::create_dir_all(&sheets).expect("sheet directory should exist");
        let file = sheets.join("safe.png");
        std::fs::write(&file, b"test").expect("fixture should write");
        assert_eq!(
            validated_sheet_path(&root, file.to_string_lossy().as_ref())
                .expect("managed path should validate"),
            Some(file.canonicalize().expect("fixture canonicalizes"))
        );
        std::fs::remove_dir_all(root).expect("fixture should clean up");
    }

    #[cfg(unix)]
    #[test]
    fn sheet_symlink_cannot_escape_managed_exports() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("sprite-sheet-link-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("sprite-sheet-out-{}", Uuid::new_v4()));
        let sheets = root.join("exports/sprite-sheets");
        std::fs::create_dir_all(&sheets).expect("sheet directory should exist");
        std::fs::create_dir_all(&outside).expect("outside directory should exist");
        let outside_file = outside.join("outside.png");
        std::fs::write(&outside_file, b"outside").expect("outside fixture should write");
        let link = sheets.join("linked.png");
        symlink(&outside_file, &link).expect("symlink should create");
        assert!(validated_sheet_path(&root, link.to_string_lossy().as_ref()).is_err());
        std::fs::remove_dir_all(root).expect("fixture should clean up");
        std::fs::remove_dir_all(outside).expect("outside fixture should clean up");
    }
}
