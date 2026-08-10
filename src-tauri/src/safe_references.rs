use crate::{
    error::{CommandError, CommandResult},
    models::ReferenceImage,
    AppState,
};
use chrono::Utc;
use image::ImageReader;
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

fn valid_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

fn validate_image(path: &Path) -> CommandResult<(u32, u32, u64)> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_REFERENCE_FILE_BYTES {
        return Err(CommandError::new(
            "reference_too_large",
            "Reference image exceeds the safe file-size limit",
        ));
    }
    let (width, height) = image::image_dimensions(path)?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0
        || height == 0
        || width > MAX_REFERENCE_EDGE
        || height > MAX_REFERENCE_EDGE
        || pixels > MAX_REFERENCE_PIXELS
    {
        return Err(CommandError::new(
            "reference_too_large",
            format!("Reference dimensions {width}x{height} exceed the safe import limit"),
        ));
    }
    ImageReader::open(path)?.with_guessed_format()?.decode()?;
    Ok((width, height, metadata.len()))
}

fn canonical_reference_path(
    project_root: &Path,
    worktree_slug: &str,
    path: &Path,
) -> CommandResult<PathBuf> {
    let root = project_root.canonicalize()?;
    let directory = root
        .join("worktrees")
        .join(worktree_slug)
        .join("references");
    std::fs::create_dir_all(&directory)?;
    let directory = directory.canonicalize()?;
    let candidate = path.canonicalize()?;
    if !candidate.starts_with(&directory) {
        return Err(CommandError::new(
            "reference_outside_workspace",
            "Reference path resolves outside its worktree reference directory",
        ));
    }
    Ok(candidate)
}

fn portable_file_name(path: &Path) -> String {
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
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    format!(
        "{}-{}.{}",
        &Uuid::new_v4().simple().to_string()[..8],
        if slug.is_empty() { "reference" } else { slug },
        extension
    )
}

fn file_hash(path: &Path) -> CommandResult<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn worktree_location(
    state: &AppState,
    worktree_id: &str,
) -> CommandResult<(String, PathBuf, String)> {
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let (project_id, project_path, worktree_slug): (String, String, String) = connection
        .query_row(
            r#"SELECT p.id, p.path, w.slug
               FROM worktrees w JOIN projects p ON p.id = w.project_id
               WHERE w.id = ?1"#,
            [worktree_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| CommandError::new("worktree_not_found", "The worktree no longer exists"))?;
    let project_root = PathBuf::from(project_path).canonicalize()?;
    Ok((project_id, project_root, worktree_slug))
}

#[tauri::command]
pub fn list_reference_images(
    worktree_id: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Vec<ReferenceImage>> {
    let (_, project_root, slug) = worktree_location(&state, &worktree_id)?;
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    let mut statement = connection.prepare(&format!(
        "{} WHERE worktree_id = ?1 ORDER BY updated_at DESC",
        select_reference()
    ))?;
    let rows = statement.query_map([&worktree_id], reference_row)?;
    let mut references = Vec::new();
    for reference in rows.filter_map(Result::ok) {
        let path = match canonical_reference_path(&project_root, &slug, Path::new(&reference.path)) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if validate_image(&path).is_err() {
            continue;
        }
        app.asset_protocol_scope()
            .allow_file(&path)
            .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
        references.push(reference);
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
    let source = PathBuf::from(source_path);
    if std::fs::symlink_metadata(&source)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(CommandError::new(
            "unsafe_reference_source",
            "Select the real reference image rather than a symbolic link",
        ));
    }
    if !source.is_file() || !valid_extension(&source) {
        return Err(CommandError::new(
            "reference_not_found",
            "Select a supported reference image",
        ));
    }
    let (width, height, _) = validate_image(&source)?;
    let (project_id, project_root, worktree_slug) =
        worktree_location(&state, &worktree_id)?;
    let reference_directory = project_root
        .join("worktrees")
        .join(&worktree_slug)
        .join("references");
    std::fs::create_dir_all(&reference_directory)?;
    let reference_directory = reference_directory.canonicalize()?;
    let destination = reference_directory.join(portable_file_name(&source));
    std::fs::copy(&source, &destination)?;
    let destination = canonical_reference_path(&project_root, &worktree_slug, &destination)?;
    validate_image(&destination)?;
    app.asset_protocol_scope()
        .allow_file(&destination)
        .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
    let metadata = std::fs::metadata(&destination)?;
    let relative_path = destination
        .strip_prefix(&project_root)
        .map_err(|_| {
            CommandError::new(
                "reference_outside_workspace",
                "Reference escaped project root",
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let format = destination
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
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
        format,
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
pub fn delete_reference_image(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let value: Option<(String, String, String)> = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        connection
            .query_row(
                r#"SELECT r.path, p.path, w.slug
                   FROM reference_images r
                   JOIN projects p ON p.id = r.project_id
                   JOIN worktrees w ON w.id = r.worktree_id
                   WHERE r.id=?1"#,
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
    };
    let (path, project_path, slug) = value.ok_or_else(|| {
        CommandError::new(
            "reference_not_found",
            "The reference image no longer exists",
        )
    })?;
    let project_root = PathBuf::from(project_path).canonicalize()?;
    let safe_path = canonical_reference_path(&project_root, &slug, Path::new(&path))?;
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
        let value: Option<(String, String, String, Option<String>, String, String)> = connection
            .query_row(
                r#"SELECT r.name, r.path, r.category, r.notes, p.path, w.slug
                   FROM reference_images r
                   JOIN conversations c ON c.workspace_id = r.project_id
                   JOIN projects p ON p.id = r.project_id
                   JOIN worktrees w ON w.id = r.worktree_id
                   WHERE c.id=?1 AND r.id=?2"#,
                params![conversation_id, id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let (name, path, category, notes, project_path, slug) = value.ok_or_else(|| {
            CommandError::new(
                "invalid_conversation_reference",
                "A selected reference is unavailable in this project",
            )
        })?;
        let project_root = PathBuf::from(project_path).canonicalize()?;
        let safe_path = canonical_reference_path(&project_root, &slug, Path::new(&path))?;
        validate_image(&safe_path)?;
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
            "ACTIVE REFERENCE IMAGES\nInspect these validated local image files before generation and preserve the requested identity/style/pose roles.\n{}",
            lines.join("\n")
        )
    })
}
