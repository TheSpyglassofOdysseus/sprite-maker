use crate::{
    error::{CommandError, CommandResult},
    models::ReferenceImage,
    AppState,
};
use chrono::Utc;
use image::GenericImageView;
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};
use tauri::{Manager, State};
use uuid::Uuid;

const MAX_REFERENCE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REFERENCE_EDGE: u32 = 8192;
const MAX_REFERENCE_PIXELS: u64 = 32 * 1024 * 1024;

const REFERENCE_CATEGORIES: &[&str] = &[
    "character_appearance",
    "clothing",
    "face",
    "weapon",
    "pose",
    "art_style",
    "environment",
    "palette",
    "animation",
    "vfx",
    "anatomy",
    "lighting",
    "other",
];

fn reference_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReferenceImage> {
    Ok(ReferenceImage {
        id: row.get(0)?,
        project_id: row.get(1)?,
        worktree_id: row.get(2)?,
        name: row.get(3)?,
        path: row.get(4)?,
        relative_path: row.get(5)?,
        category: row.get(6)?,
        notes: row.get(7)?,
        format: row.get(8)?,
        width: row.get(9)?,
        height: row.get(10)?,
        file_size: row.get(11)?,
        content_hash: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn select_reference() -> &'static str {
    "SELECT id, project_id, worktree_id, name, path, relative_path, category, notes, format, width, height, file_size, content_hash, created_at, updated_at FROM reference_images"
}

fn validate_category(category: &str) -> CommandResult<()> {
    if REFERENCE_CATEGORIES.contains(&category) {
        Ok(())
    } else {
        Err(CommandError::new(
            "invalid_reference_category",
            "Choose a supported reference category",
        ))
    }
}

fn validated_extension(path: &Path) -> CommandResult<String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        Ok(extension)
    } else {
        Err(CommandError::new(
            "invalid_reference_format",
            "Reference images must use PNG, JPEG, GIF, or WebP",
        ))
    }
}

fn validate_reference_constraints(path: &Path) -> CommandResult<(u32, u32, u64)> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_REFERENCE_FILE_BYTES {
        return Err(CommandError::new(
            "reference_too_large",
            format!(
                "Reference file is {} bytes; the maximum supported import is {} bytes",
                metadata.len(),
                MAX_REFERENCE_FILE_BYTES
            ),
        ));
    }
    let (width, height) = image::image_dimensions(path)
        .map_err(|error| CommandError::new("invalid_reference_image", error.to_string()))?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width > MAX_REFERENCE_EDGE || height > MAX_REFERENCE_EDGE || pixels > MAX_REFERENCE_PIXELS {
        return Err(CommandError::new(
            "reference_too_large",
            format!("Reference dimensions {width}x{height} exceed the safe import limit"),
        ));
    }
    Ok((width, height, metadata.len()))
}

fn canonical_reference_directory(project_root: &Path, path: &Path) -> CommandResult<PathBuf> {
    let project_root = project_root.canonicalize()?;
    let worktrees_root = project_root.join("worktrees").canonicalize()?;
    let candidate = path.canonicalize()?;
    if !candidate.starts_with(&worktrees_root) {
        return Err(CommandError::new(
            "reference_outside_workspace",
            "Reference directory resolves outside this workspace",
        ));
    }
    let relative = candidate.strip_prefix(&worktrees_root).map_err(|_| {
        CommandError::new(
            "reference_outside_workspace",
            "Reference directory is outside the worktree root",
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() < 2 || components[1].as_os_str().to_str() != Some("references") {
        return Err(CommandError::new(
            "reference_outside_workspace",
            "Reference directory must be inside a worktree references folder",
        ));
    }
    Ok(candidate)
}

fn canonical_reference_path(project_root: &Path, path: &Path) -> CommandResult<PathBuf> {
    let candidate = path.canonicalize()?;
    let parent = candidate.parent().ok_or_else(|| {
        CommandError::new(
            "reference_outside_workspace",
            "Reference path has no containing directory",
        )
    })?;
    canonical_reference_directory(project_root, parent)?;
    Ok(candidate)
}

fn portable_file_name(path: &Path, extension: &str) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("reference");
    let slug: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    format!(
        "{}-{}.{}",
        &Uuid::new_v4().simple().to_string()[..8],
        if slug.is_empty() { "reference" } else { slug },
        extension
    )
}

fn file_hash(path: &Path) -> CommandResult<String> {
    let bytes = std::fs::read(path)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[tauri::command]
pub fn list_reference_images(
    worktree_id: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Vec<ReferenceImage>> {
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let project_path: String = connection
        .query_row(
            r#"SELECT p.path
               FROM worktrees w JOIN projects p ON p.id=w.project_id
               WHERE w.id=?1"#,
            [&worktree_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| CommandError::new("worktree_not_found", "The worktree no longer exists"))?;
    let mut statement = connection.prepare(&format!(
        "{} WHERE worktree_id = ?1 ORDER BY updated_at DESC",
        select_reference()
    ))?;
    let rows = statement.query_map([worktree_id], reference_row)?;
    let references: Vec<_> = rows.filter_map(Result::ok).collect();
    drop(statement);
    drop(connection);
    let project_root = PathBuf::from(project_path);
    for reference in &references {
        let safe_path = canonical_reference_path(&project_root, Path::new(&reference.path))?;
        validate_reference_constraints(&safe_path)?;
        app.asset_protocol_scope()
            .allow_file(&safe_path)
            .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
    }
    Ok(references)
}

#[tauri::command]
pub fn import_reference_image(
    worktree_id: String,
    source_path: String,
    category: String,
    notes: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<ReferenceImage> {
    validate_category(&category)?;
    let source = PathBuf::from(&source_path);
    if !source.is_file() {
        return Err(CommandError::new(
            "reference_not_found",
            "The selected reference image no longer exists",
        ));
    }
    let extension = validated_extension(&source)?;
    let (width, height, _) = validate_reference_constraints(&source)?;
    image::open(&source)
        .map_err(|error| CommandError::new("invalid_reference_image", error.to_string()))?;
    let (project_id, project_path, worktree_slug): (String, String, String) = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        connection
            .query_row(
                r#"SELECT p.id, p.path, w.slug
                   FROM worktrees w JOIN projects p ON p.id = w.project_id
                   WHERE w.id = ?1"#,
                [&worktree_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| {
                CommandError::new("worktree_not_found", "The worktree no longer exists")
            })?
    };
    let project_root = PathBuf::from(&project_path).canonicalize()?;
    let reference_directory = project_root
        .join("worktrees")
        .join(worktree_slug)
        .join("references");
    std::fs::create_dir_all(&reference_directory)?;
    let reference_directory = canonical_reference_directory(&project_root, &reference_directory)?;
    let destination = reference_directory.join(portable_file_name(&source, &extension));
    std::fs::copy(&source, &destination)?;
    let destination = canonical_reference_path(&project_root, &destination)?;
    app.asset_protocol_scope()
        .allow_file(&destination)
        .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
    let metadata = std::fs::metadata(&destination)?;
    let relative_path = destination
        .strip_prefix(&project_root)
        .map_err(|_| {
            CommandError::new(
                "reference_outside_workspace",
                "Reference path is outside the project root",
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let now = Utc::now().to_rfc3339();
    let reference = ReferenceImage {
        id: Uuid::new_v4().to_string(),
        project_id,
        worktree_id,
        name: source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("Reference")
            .to_string(),
        path: destination.to_string_lossy().into_owned(),
        relative_path,
        category,
        notes: notes.filter(|value| !value.trim().is_empty()),
        format: extension,
        width,
        height,
        file_size: metadata.len(),
        content_hash: file_hash(&destination)?,
        created_at: now.clone(),
        updated_at: now,
    };
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    connection.execute(
        r#"INSERT INTO reference_images(
            id, project_id, worktree_id, name, path, relative_path, category, notes,
            format, width, height, file_size, content_hash, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"#,
        params![
            reference.id,
            reference.project_id,
            reference.worktree_id,
            reference.name,
            reference.path,
            reference.relative_path,
            reference.category,
            reference.notes,
            reference.format,
            reference.width,
            reference.height,
            reference.file_size,
            reference.content_hash,
            reference.created_at,
            reference.updated_at
        ],
    )?;
    Ok(reference)
}

#[tauri::command]
pub fn update_reference_image(
    id: String,
    name: String,
    category: String,
    notes: Option<String>,
    state: State<'_, AppState>,
) -> CommandResult<ReferenceImage> {
    validate_category(&category)?;
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::new(
            "invalid_reference_name",
            "Reference name cannot be empty",
        ));
    }
    let now = Utc::now().to_rfc3339();
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let changed = connection.execute(
        "UPDATE reference_images SET name=?2, category=?3, notes=?4, updated_at=?5 WHERE id=?1",
        params![
            id,
            name,
            category,
            notes.filter(|value| !value.trim().is_empty()),
            now
        ],
    )?;
    if changed == 0 {
        return Err(CommandError::new(
            "reference_not_found",
            "The reference image no longer exists",
        ));
    }
    connection
        .query_row(
            &format!("{} WHERE id = ?1", select_reference()),
            [id],
            reference_row,
        )
        .map_err(Into::into)
}

#[tauri::command]
pub fn delete_reference_image(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let value: Option<(String, String)> = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        connection
            .query_row(
                r#"SELECT p.path, r.path
                   FROM reference_images r JOIN projects p ON p.id=r.project_id
                   WHERE r.id=?1"#,
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
    };
    let (project_path, path) = value.ok_or_else(|| {
        CommandError::new(
            "reference_not_found",
            "The reference image no longer exists",
        )
    })?;
    let safe_path = canonical_reference_path(Path::new(&project_path), Path::new(&path))?;
    if safe_path.is_file() {
        std::fs::remove_file(&safe_path)?;
    }
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    connection.execute("DELETE FROM reference_images WHERE id=?1", [&id])?;
    Ok(())
}

#[tauri::command]
pub fn set_conversation_reference(
    conversation_id: String,
    reference_id: String,
    active: bool,
    strength: Option<f64>,
    state: State<'_, AppState>,
) -> CommandResult<()> {
    let strength = strength.unwrap_or(1.0).clamp(0.0, 2.0);
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let compatible: bool = connection.query_row(
        r#"SELECT EXISTS(
             SELECT 1 FROM conversations c
             JOIN reference_images r ON r.project_id = c.workspace_id
             WHERE c.id=?1 AND r.id=?2
           )"#,
        params![conversation_id, reference_id],
        |row| row.get(0),
    )?;
    if !compatible {
        return Err(CommandError::new(
            "invalid_conversation_reference",
            "The reference and conversation must belong to the same project",
        ));
    }
    if active {
        connection.execute(
            r#"INSERT INTO conversation_references(conversation_id, reference_id, active, strength, created_at)
               VALUES (?1, ?2, 1, ?3, ?4)
               ON CONFLICT(conversation_id, reference_id) DO UPDATE SET active=1, strength=excluded.strength"#,
            params![conversation_id, reference_id, strength, Utc::now().to_rfc3339()],
        )?;
    } else {
        connection.execute(
            "DELETE FROM conversation_references WHERE conversation_id=?1 AND reference_id=?2",
            params![conversation_id, reference_id],
        )?;
    }
    Ok(())
}

#[tauri::command]
pub fn list_conversation_reference_ids(
    conversation_id: String,
    state: State<'_, AppState>,
) -> CommandResult<Vec<String>> {
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let mut statement = connection.prepare(
        "SELECT reference_id FROM conversation_references WHERE conversation_id=?1 AND active=1 ORDER BY created_at",
    )?;
    let rows = statement.query_map([conversation_id], |row| row.get(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn prompt_context(
    state: &State<'_, AppState>,
    conversation_id: &str,
    reference_ids: &[String],
    maximum: usize,
) -> CommandResult<String> {
    if reference_ids.len() > maximum {
        return Err(CommandError::new(
            "too_many_references",
            format!("The selected provider supports at most {maximum} reference images"),
        ));
    }
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let mut lines = Vec::new();
    for id in reference_ids {
        let value: Option<(String, String, String, Option<String>, String)> = connection
            .query_row(
                r#"SELECT r.name, r.path, r.category, r.notes, p.path
                   FROM reference_images r
                   JOIN conversations c ON c.workspace_id = r.project_id
                   JOIN projects p ON p.id = r.project_id
                   WHERE c.id=?1 AND r.id=?2"#,
                params![conversation_id, id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let (name, path, category, notes, project_path) = value.ok_or_else(|| {
            CommandError::new(
                "invalid_conversation_reference",
                "A selected reference is unavailable in this project",
            )
        })?;
        let safe_path = canonical_reference_path(Path::new(&project_path), Path::new(&path))?;
        validate_reference_constraints(&safe_path)?;
        lines.push(format!(
            "- {name} [{category}]: {}{}",
            safe_path.to_string_lossy(),
            notes
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" — {value}"))
                .unwrap_or_default()
        ));
    }
    Ok(if lines.is_empty() {
        String::new()
    } else {
        format!(
            "ACTIVE REFERENCE IMAGES\nInspect these local image files before generation and preserve the requested identity/style/pose roles. Do not copy protected characters or brands.\n{}",
            lines.join("\n")
        )
    })
}
