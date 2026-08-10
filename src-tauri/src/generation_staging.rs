use crate::{
    error::{CommandError, CommandResult},
    models::GenerationManifest,
};
use image::ImageReader;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const MAX_STAGE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GENERATED_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GENERATED_EDGE: u32 = 512;
const MAX_GENERATED_PIXELS: u64 = 512 * 512;
const MAX_GENERATED_FILES: usize = 64;

pub struct GenerationStage {
    real_root: PathBuf,
    stage_root: PathBuf,
}

impl GenerationStage {
    pub fn prepare(
        real_root: &Path,
        workspace_id: &str,
        conversation_id: &str,
        request_id: &str,
    ) -> CommandResult<Self> {
        let real_root = real_root.canonicalize()?;
        let base = stage_base(workspace_id, conversation_id)?;
        let stage_root = base.join("current");
        if stage_root.exists() {
            let archive = base.join("failed-or-interrupted");
            fs::create_dir_all(&archive)?;
            let archived = archive.join(format!("previous-before-{}", safe_id(request_id)?));
            if archived.exists() {
                fs::remove_dir_all(&archived)?;
            }
            fs::rename(&stage_root, archived)?;
        }
        fs::create_dir_all(&stage_root)?;

        let mut copied_bytes = 0_u64;
        for relative in ["assets", "animations", "worktrees", ".sprite-studio"] {
            let source = real_root.join(relative);
            if source.is_dir() {
                copy_tree(&source, &stage_root.join(relative), &mut copied_bytes)?;
            }
        }
        for relative in [
            "assets/characters",
            "assets/creatures",
            "assets/terrain",
            "assets/props",
            "assets/effects",
            "animations",
            "worktrees",
            ".sprite-studio",
        ] {
            fs::create_dir_all(stage_root.join(relative))?;
        }
        let stale_manifest = stage_root.join(".sprite-studio/last-generation.json");
        if stale_manifest.is_file() {
            fs::remove_file(stale_manifest)?;
        }

        Ok(Self {
            real_root,
            stage_root,
        })
    }

    pub fn path(&self) -> &Path {
        &self.stage_root
    }

    pub fn rewrite_prompt(&self, prompt: &str) -> String {
        prompt.replace(
            self.real_root.to_string_lossy().as_ref(),
            self.stage_root.to_string_lossy().as_ref(),
        )
    }

    pub fn promote_if_present(&self) -> CommandResult<Option<GenerationManifest>> {
        let manifest_path = self.stage_root.join(".sprite-studio/last-generation.json");
        if !manifest_path.is_file() {
            return Ok(None);
        }
        let mut manifest: GenerationManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
            .map_err(|error| CommandError::new("invalid_generation", error.to_string()))?;
        if manifest.files.is_empty() || manifest.files.len() > MAX_GENERATED_FILES {
            return Err(CommandError::new(
                "invalid_generation",
                "Generation manifest contains an invalid number of files",
            ));
        }

        let staged_assets = self.stage_root.join("assets").canonicalize()?;
        let real_assets = self.real_root.join("assets").canonicalize()?;
        let mut plan = Vec::with_capacity(manifest.files.len());

        for relative in &manifest.files {
            let relative_path = validated_manifest_relative(relative)?;
            let staged = self.stage_root.join(&relative_path).canonicalize()?;
            if !staged.starts_with(&staged_assets) || !staged.is_file() {
                return Err(CommandError::new(
                    "invalid_generation",
                    "Generated file resolves outside the staging asset tree",
                ));
            }
            validate_generated_image(&staged)?;
            let requested = self.real_root.join(&relative_path);
            let destination = unique_destination(&real_assets, &requested)?;
            plan.push((staged, destination));
        }

        let mut copied = Vec::new();
        let copy_result: CommandResult<()> = (|| {
            for (staged, destination) in &plan {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(staged, destination)?;
                let canonical = destination.canonicalize()?;
                if !canonical.starts_with(&real_assets) {
                    return Err(CommandError::new(
                        "invalid_generation",
                        "Promoted file escaped the real workspace asset tree",
                    ));
                }
                copied.push(canonical);
            }
            Ok(())
        })();
        if let Err(error) = copy_result {
            for path in copied {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }

        let promoted = copied
            .iter()
            .map(|destination| {
                destination
                    .strip_prefix(&self.real_root)
                    .map(|value| value.to_string_lossy().replace('\\', "/"))
                    .map_err(|_| {
                        CommandError::new(
                            "invalid_generation",
                            "Promoted file is outside the real workspace",
                        )
                    })
            })
            .collect::<CommandResult<Vec<_>>>()?;

        manifest.files = promoted;
        let real_metadata = self.real_root.join(".sprite-studio");
        fs::create_dir_all(&real_metadata)?;
        fs::write(
            real_metadata.join("last-generation.json"),
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| CommandError::new("serialization_error", error.to_string()))?,
        )?;
        Ok(Some(manifest))
    }

    pub fn cleanup(self) {
        let _ = fs::remove_dir_all(self.stage_root);
    }
}

fn safe_id(value: &str) -> CommandResult<String> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(CommandError::new(
            "invalid_stage_id",
            "Generation staging identifier is invalid",
        ));
    }
    Ok(value.to_string())
}

fn stage_base(workspace_id: &str, conversation_id: &str) -> CommandResult<PathBuf> {
    Ok(std::env::temp_dir()
        .join("sprite-studio-generation")
        .join(safe_id(workspace_id)?)
        .join(safe_id(conversation_id)?))
}

fn copy_tree(source: &Path, destination: &Path, copied_bytes: &mut u64) -> CommandResult<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path, copied_bytes)?;
        } else if file_type.is_file() {
            let bytes = entry.metadata()?.len();
            *copied_bytes = copied_bytes.checked_add(bytes).ok_or_else(|| {
                CommandError::new("generation_stage_too_large", "Generation staging size overflowed")
            })?;
            if *copied_bytes > MAX_STAGE_BYTES {
                return Err(CommandError::new(
                    "generation_stage_too_large",
                    "Project assets exceed the 1 GiB generation staging limit",
                ));
            }
            fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn validated_manifest_relative(value: &str) -> CommandResult<PathBuf> {
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
            "Generated files must be written under assets/",
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
    if path.extension().and_then(|value| value.to_str()) != Some("png") {
        return Err(CommandError::new(
            "invalid_generation",
            "Generated sprite files must be PNG images",
        ));
    }
    Ok(path.to_path_buf())
}

fn validate_generated_image(path: &Path) -> CommandResult<()> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_GENERATED_FILE_BYTES {
        return Err(CommandError::new(
            "invalid_generation",
            "Generated sprite exceeds the safe file-size limit",
        ));
    }
    let (width, height) = image::image_dimensions(path)?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if !(8..=MAX_GENERATED_EDGE).contains(&width)
        || !(8..=MAX_GENERATED_EDGE).contains(&height)
        || pixels > MAX_GENERATED_PIXELS
    {
        return Err(CommandError::new(
            "invalid_generation",
            format!("Generated sprite dimensions {width}x{height} are outside the safe profile"),
        ));
    }
    ImageReader::open(path)?.with_guessed_format()?.decode()?;
    Ok(())
}

fn unique_destination(assets_root: &Path, requested: &Path) -> CommandResult<PathBuf> {
    let parent = requested.parent().ok_or_else(|| {
        CommandError::new(
            "invalid_generation",
            "Generated file does not have a destination directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(assets_root) {
        return Err(CommandError::new(
            "invalid_generation",
            "Generated destination resolves outside the real asset tree",
        ));
    }
    if !requested.exists() && fs::symlink_metadata(requested).is_err() {
        return Ok(requested.to_path_buf());
    }
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("sprite");
    for index in 2..10_000_u32 {
        let candidate = parent.join(format!("{stem}-{index}.png"));
        if !candidate.exists() && fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(CommandError::new(
        "generation_name_exhausted",
        "Could not allocate a safe destination for generated output",
    ))
}

#[cfg(test)]
mod tests {
    use super::{stage_base, validated_manifest_relative, GenerationStage};
    use crate::models::GenerationManifest;
    use image::{Rgba, RgbaImage};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn manifest_paths_reject_parent_escape_and_non_assets() {
        assert!(validated_manifest_relative("../outside.png").is_err());
        assert!(validated_manifest_relative("exports/frame.png").is_err());
        assert!(validated_manifest_relative("assets/characters/frame.png").is_ok());
    }

    #[test]
    fn conversation_stage_uses_a_stable_current_path() {
        let first = stage_base("workspace-a", "conversation-a")
            .expect("stage base")
            .join("current");
        let second = stage_base("workspace-a", "conversation-a")
            .expect("stage base")
            .join("current");
        assert_eq!(first, second);
    }

    #[test]
    fn staged_generation_promotes_without_overwriting_existing_asset() {
        let root = std::env::temp_dir().join(format!("stage-promote-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("assets/characters")).expect("asset directory should exist");
        fs::create_dir_all(root.join(".sprite-studio")).expect("metadata directory should exist");
        let existing = root.join("assets/characters/hero.png");
        RgbaImage::from_pixel(16, 16, Rgba([1, 2, 3, 255]))
            .save(&existing)
            .expect("existing image saves");

        let stage = GenerationStage::prepare(
            &root,
            "workspace-test",
            "conversation-test",
            "request-test",
        )
        .expect("stage should prepare");
        let staged = stage.path().join("assets/characters/hero.png");
        RgbaImage::from_pixel(16, 16, Rgba([4, 5, 6, 255]))
            .save(&staged)
            .expect("staged image saves");
        let manifest = GenerationManifest {
            name: "hero".into(),
            category: "characters".into(),
            fps: 1.0,
            files: vec!["assets/characters/hero.png".into()],
            generated_at: "2026-08-09T00:00:00Z".into(),
        };
        fs::write(
            stage.path().join(".sprite-studio/last-generation.json"),
            serde_json::to_vec_pretty(&manifest).expect("manifest serializes"),
        )
        .expect("manifest writes");

        let promoted = stage
            .promote_if_present()
            .expect("generation should promote")
            .expect("manifest should be present");
        assert_eq!(promoted.files, vec!["assets/characters/hero-2.png"]);
        assert!(root.join("assets/characters/hero-2.png").is_file());
        let original = image::open(existing).expect("original reads").to_rgba8();
        assert_eq!(original.get_pixel(0, 0).0, [1, 2, 3, 255]);
        stage.cleanup();
        fs::remove_dir_all(root).expect("fixture should clean up");
    }
}
