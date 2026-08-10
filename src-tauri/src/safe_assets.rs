use crate::{
    assets,
    error::{CommandError, CommandResult},
    models::{Asset, ExportResult, GenerationManifest},
    workspace::workspace_path,
    AppState,
};
use image::ImageReader;
use rusqlite::OptionalExtension;
use std::path::{Component, Path, PathBuf};
use tauri::{Manager, State};

const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_GENERATION_FILES: usize = 64;

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

fn safe_category(category: &str) -> CommandResult<String> {
    let value = category.trim().to_ascii_lowercase();
    if !matches!(
        value.as_str(),
        "characters" | "creatures" | "terrain" | "props" | "effects"
    ) {
        return Err(CommandError::new(
            "invalid_category",
            "Choose characters, creatures, terrain, props, or effects",
        ));
    }
    Ok(value)
}

fn canonical_assets_root(root: &Path) -> CommandResult<PathBuf> {
    let root = root.canonicalize()?;
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets)?;
    let assets = assets.canonicalize()?;
    if !assets.starts_with(&root) {
        return Err(CommandError::new(
            "asset_root_outside_workspace",
            "The assets directory resolves outside this workspace",
        ));
    }
    Ok(assets)
}

fn canonical_asset_path(root: &Path, path: &Path) -> CommandResult<PathBuf> {
    let assets_root = canonical_assets_root(root)?;
    let candidate = path.canonicalize()?;
    if !candidate.starts_with(&assets_root) {
        return Err(CommandError::new(
            "asset_outside_workspace",
            "Asset path resolves outside this workspace",
        ));
    }
    Ok(candidate)
}

fn validate_image(path: &Path) -> CommandResult<(u32, u32, u64)> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(CommandError::new(
            "asset_missing",
            "Image is not a regular file",
        ));
    }
    if metadata.len() > MAX_IMAGE_FILE_BYTES {
        return Err(CommandError::new(
            "image_too_large",
            format!(
                "Image file is {} bytes; the maximum supported import is {} bytes",
                metadata.len(),
                MAX_IMAGE_FILE_BYTES
            ),
        ));
    }
    let (width, height) = image::image_dimensions(path)?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0
        || height == 0
        || width > MAX_IMAGE_EDGE
        || height > MAX_IMAGE_EDGE
        || pixels > MAX_IMAGE_PIXELS
    {
        return Err(CommandError::new(
            "image_too_large",
            format!("Image dimensions {width}x{height} exceed the safe import limit"),
        ));
    }
    ImageReader::open(path)?.with_guessed_format()?.decode()?;
    Ok((width, height, metadata.len()))
}

fn image_paths(directory: &Path, output: &mut Vec<PathBuf>) -> CommandResult<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            image_paths(&path, output)?;
        } else if file_type.is_file() && valid_extension(&path) {
            output.push(path);
        }
    }
    Ok(())
}

fn safe_manifest_relative(value: &str) -> CommandResult<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CommandError::new(
            "invalid_generation",
            "Generation manifest contains an unsafe file path",
        ));
    }
    let mut components = path.components();
    if components.next().and_then(|value| value.as_os_str().to_str()) != Some("assets") {
        return Err(CommandError::new(
            "invalid_generation",
            "Generated files must be written below assets/",
        ));
    }
    let category = components
        .next()
        .and_then(|value| value.as_os_str().to_str())
        .unwrap_or("");
    if !matches!(
        category,
        "characters" | "creatures" | "terrain" | "props" | "effects"
    ) {
        return Err(CommandError::new(
            "invalid_generation",
            "Generated files must use a supported asset category",
        ));
    }
    Ok(path.to_path_buf())
}

#[tauri::command]
pub fn scan_assets(
    workspace_id: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Vec<Asset>> {
    let root = workspace_path(&state, &workspace_id)?;
    let assets_root = canonical_assets_root(&root)?;
    let mut paths = Vec::new();
    image_paths(&assets_root, &mut paths)?;
    let mut found = Vec::new();

    for path in paths {
        let path = match canonical_asset_path(&root, &path) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if validate_image(&path).is_err() {
            continue;
        }
        let existing_id = {
            let connection = state
                .db
                .lock()
                .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
            connection
                .query_row(
                    "SELECT id FROM assets WHERE path = ?1",
                    [path.to_string_lossy().as_ref()],
                    |row| row.get(0),
                )
                .optional()?
        };
        let asset = assets::inspect(&workspace_id, &root, &path, existing_id)?;
        app.asset_protocol_scope()
            .allow_file(&asset.path)
            .map_err(|error| CommandError::new("asset_scope_error", error.to_string()))?;
        assets::upsert(&state, &asset, "scanned")?;
        found.push(asset);
    }

    {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        let mut statement =
            connection.prepare("SELECT id, path FROM assets WHERE workspace_id = ?1")?;
        let registered = statement.query_map([&workspace_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for entry in registered.filter_map(Result::ok) {
            let valid = canonical_asset_path(&root, Path::new(&entry.1))
                .and_then(|path| validate_image(&path).map(|_| ()))
                .is_ok();
            if !valid {
                connection.execute("DELETE FROM assets WHERE id = ?1", [entry.0])?;
            }
        }
    }

    found.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    Ok(found)
}

#[tauri::command]
pub fn import_asset(
    workspace_id: String,
    source_path: String,
    category: String,
    state: State<'_, AppState>,
) -> CommandResult<Asset> {
    let root = workspace_path(&state, &workspace_id)?;
    canonical_assets_root(&root)?;
    let source = PathBuf::from(source_path);
    if std::fs::symlink_metadata(&source)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(CommandError::new(
            "unsafe_asset_source",
            "Import symbolic links by selecting their real image file instead",
        ));
    }
    if !source.is_file() || !valid_extension(&source) {
        return Err(CommandError::new(
            "asset_missing",
            "Select a supported image file",
        ));
    }
    validate_image(&source)?;
    let category = safe_category(&category)?;
    let directory = root.join("assets").join(&category);
    std::fs::create_dir_all(&directory)?;
    let directory = directory.canonicalize()?;
    let assets_root = canonical_assets_root(&root)?;
    if !directory.starts_with(&assets_root) {
        return Err(CommandError::new(
            "asset_outside_workspace",
            "Asset category directory resolves outside this workspace",
        ));
    }
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("asset");
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png");
    let mut destination = directory.join(format!("{stem}.{extension}"));
    let mut version = 2_u32;
    while destination.exists() || std::fs::symlink_metadata(&destination).is_ok() {
        destination = directory.join(format!("{stem}-{version}.{extension}"));
        version = version.saturating_add(1);
    }
    std::fs::copy(&source, &destination)?;
    let destination = canonical_asset_path(&root, &destination)?;
    let asset = assets::inspect(&workspace_id, &root, &destination, None)?;
    assets::upsert(&state, &asset, "imported")?;
    Ok(asset)
}

#[tauri::command]
pub fn rename_asset(id: String, name: String, state: State<'_, AppState>) -> CommandResult<Asset> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err(CommandError::new(
            "invalid_name",
            "Enter a valid filename without path separators",
        ));
    }
    let (workspace_id, old_path): (String, String) = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        connection
            .query_row(
                "SELECT workspace_id, path FROM assets WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| CommandError::new("asset_not_found", "Asset no longer exists"))?
    };
    let root = workspace_path(&state, &workspace_id)?;
    let old_path = canonical_asset_path(&root, Path::new(&old_path))?;
    let extension = old_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png");
    let new_path = old_path.with_file_name(format!("{name}.{extension}"));
    if new_path.exists() || std::fs::symlink_metadata(&new_path).is_ok() {
        return Err(CommandError::new(
            "asset_exists",
            "An asset with that name already exists",
        ));
    }
    std::fs::rename(&old_path, &new_path)?;
    let new_path = canonical_asset_path(&root, &new_path)?;
    let asset = assets::inspect(&workspace_id, &root, &new_path, Some(id))?;
    assets::upsert(&state, &asset, "renamed")?;
    Ok(asset)
}

#[tauri::command]
pub fn delete_asset(id: String, state: State<'_, AppState>) -> CommandResult<()> {
    let (workspace_id, path): (String, String) = {
        let connection = state
            .db
            .lock()
            .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
        connection
            .query_row(
                "SELECT workspace_id, path FROM assets WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| CommandError::new("asset_not_found", "Asset no longer exists"))?
    };
    let root = workspace_path(&state, &workspace_id)?;
    let safe_path = canonical_asset_path(&root, Path::new(&path))?;
    if safe_path.is_file() {
        std::fs::remove_file(safe_path)?;
    }
    let connection = state
        .db
        .lock()
        .map_err(|_| CommandError::new("database_locked", "Database lock was poisoned"))?;
    connection.execute("DELETE FROM assets WHERE id = ?1", [id])?;
    Ok(())
}

#[tauri::command]
pub fn export_asset(id: String, state: State<'_, AppState>) -> CommandResult<ExportResult> {
    let asset = assets::get_asset(&state, &id)?;
    let root = workspace_path(&state, &asset.workspace_id)?;
    let source = canonical_asset_path(&root, Path::new(&asset.path))?;
    validate_image(&source)?;
    let output_directory = root.join("exports");
    std::fs::create_dir_all(&output_directory)?;
    let slug: String = asset
        .name
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() {
                value.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png");
    let image_path = output_directory.join(format!("{slug}.{extension}"));
    let metadata_path = output_directory.join(format!("{slug}.json"));
    std::fs::copy(&source, &image_path)?;
    let metadata = serde_json::json!({
        "name": asset.name,
        "image": image_path.file_name().and_then(|value| value.to_str()).unwrap_or("sprite.png"),
        "source": asset.relative_path,
        "category": asset.category,
        "width": asset.width,
        "height": asset.height,
        "frameCount": 1
    });
    std::fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata)
            .map_err(|error| CommandError::new("serialization_error", error.to_string()))?,
    )?;
    Ok(ExportResult {
        png_path: image_path.to_string_lossy().into_owned(),
        metadata_path: metadata_path.to_string_lossy().into_owned(),
        width: asset.width,
        height: asset.height,
    })
}

#[tauri::command]
pub fn get_generation_manifest(
    workspace_id: String,
    state: State<'_, AppState>,
) -> CommandResult<Option<GenerationManifest>> {
    let root = workspace_path(&state, &workspace_id)?;
    let path = root.join(".sprite-studio/last-generation.json");
    if !path.is_file() {
        return Ok(None);
    }
    let manifest: GenerationManifest = serde_json::from_str(&std::fs::read_to_string(path)?)
        .map_err(|error| CommandError::new("invalid_generation", error.to_string()))?;
    if manifest.files.is_empty() || manifest.files.len() > MAX_GENERATION_FILES {
        return Err(CommandError::new(
            "invalid_generation",
            "Generation manifest contains an invalid number of files",
        ));
    }
    for relative in &manifest.files {
        let relative = safe_manifest_relative(relative)?;
        let candidate = canonical_asset_path(&root, &root.join(relative))?;
        validate_image(&candidate)?;
    }
    Ok(Some(manifest))
}

#[cfg(test)]
mod tests {
    use super::{canonical_assets_root, safe_manifest_relative};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn manifest_paths_reject_escape() {
        assert!(safe_manifest_relative("../outside.png").is_err());
        assert!(safe_manifest_relative("exports/frame.png").is_err());
        assert!(safe_manifest_relative("assets/characters/frame.png").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_assets_root_cannot_escape_workspace() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("asset-root-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("asset-outside-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("root creates");
        fs::create_dir_all(&outside).expect("outside creates");
        symlink(&outside, root.join("assets")).expect("assets link creates");
        assert!(canonical_assets_root(&root).is_err());
        fs::remove_file(root.join("assets")).expect("link removes");
        fs::remove_dir_all(root).expect("root cleans");
        fs::remove_dir_all(outside).expect("outside cleans");
    }
}
