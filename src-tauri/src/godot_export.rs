use crate::{
    animations::load_animation_by_id,
    assets::get_asset,
    error::{CommandError, CommandResult},
    workspace::workspace_path,
    AppState,
};
use image::{GenericImage, RgbaImage};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::State;

const MAX_GODOT_SHEET_EDGE: u32 = 16384;
const MAX_GODOT_SHEET_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GodotExportResult {
    pub png_path: String,
    pub sprite_frames_path: String,
    pub metadata_path: String,
    pub godot_texture_path: String,
    pub width: u32,
    pub height: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub frame_count: u32,
}

#[tauri::command]
pub fn export_godot_animation(
    id: String,
    destination: Option<String>,
    state: State<'_, AppState>,
) -> CommandResult<GodotExportResult> {
    let animation = load_animation_by_id(&state, &id)?;
    if animation.frames.is_empty() {
        return Err(CommandError::new(
            "empty_animation",
            "Add at least one frame before exporting to Godot",
        ));
    }
    let root = workspace_path(&state, &animation.workspace_id)?;
    let assets_root = root.join("assets").canonicalize()?;
    let mut decoded = Vec::with_capacity(animation.frames.len());
    let mut frame_width = 0_u32;
    let mut frame_height = 0_u32;

    for frame in &animation.frames {
        let asset = get_asset(&state, &frame.asset_id)?;
        if asset.workspace_id != animation.workspace_id {
            return Err(CommandError::new(
                "invalid_animation_frame",
                "Every exported frame must belong to the same project",
            ));
        }
        let path = PathBuf::from(&asset.path).canonicalize()?;
        if !path.starts_with(&assets_root) {
            return Err(CommandError::new(
                "asset_outside_workspace",
                "Animation frame resolves outside the project asset tree",
            ));
        }
        let image = image::open(&path)?.to_rgba8();
        frame_width = frame_width.max(image.width());
        frame_height = frame_height.max(image.height());
        decoded.push((asset, image));
    }

    let frame_count = decoded.len() as u32;
    let sheet_width = frame_width.checked_mul(frame_count).ok_or_else(|| {
        CommandError::new(
            "export_too_large",
            "Godot spritesheet dimensions overflowed",
        )
    })?;
    let pixels = u64::from(sheet_width).saturating_mul(u64::from(frame_height));
    if sheet_width > MAX_GODOT_SHEET_EDGE
        || frame_height > MAX_GODOT_SHEET_EDGE
        || pixels > MAX_GODOT_SHEET_PIXELS
    {
        return Err(CommandError::new(
            "export_too_large",
            "Godot spritesheet exceeds the safe export size",
        ));
    }

    let mut sheet = RgbaImage::new(sheet_width, frame_height);
    for (index, (_, frame)) in decoded.iter().enumerate() {
        sheet.copy_from(frame, index as u32 * frame_width, 0)?;
    }

    let slug = slugify(&animation.name);
    let explicit_destination = destination.is_some();
    let output_directory = if let Some(path) = destination {
        PathBuf::from(path)
    } else {
        root.join("exports").join("godot").join(&slug)
    };
    std::fs::create_dir_all(&output_directory)?;
    let output_directory = output_directory.canonicalize()?;
    let png_path = output_directory.join(format!("{slug}.png"));
    let sprite_frames_path = output_directory.join(format!("{slug}_sprite_frames.tres"));
    let metadata_path = output_directory.join(format!("{slug}.godot.json"));

    let godot_texture_path = godot_texture_path(
        &output_directory,
        &png_path,
        explicit_destination,
    )?;
    sheet.save(&png_path)?;
    let tres = sprite_frames_resource(
        &animation.name,
        animation.fps,
        animation.looping,
        frame_width,
        frame_height,
        &animation.frames,
        &godot_texture_path,
    );
    std::fs::write(&sprite_frames_path, tres)?;

    let frames: Vec<_> = decoded
        .iter()
        .enumerate()
        .map(|(index, (asset, _))| {
            let duration_ms = animation.frames[index]
                .duration_ms
                .unwrap_or_else(|| (1000.0 / animation.fps).round() as u32)
                .max(1);
            serde_json::json!({
                "index": index,
                "assetId": asset.id,
                "source": asset.relative_path,
                "x": index as u32 * frame_width,
                "y": 0,
                "width": frame_width,
                "height": frame_height,
                "durationMs": duration_ms,
                "durationFactor": duration_factor(duration_ms, animation.fps),
                "pivot": { "x": 0.5, "y": 1.0 }
            })
        })
        .collect();
    let metadata = serde_json::json!({
        "schemaVersion": "sprite-studio-godot-v1",
        "engine": "Godot 4",
        "name": animation.name,
        "animation": slug,
        "image": png_path.file_name().and_then(|value| value.to_str()).unwrap_or("spritesheet.png"),
        "spriteFrames": sprite_frames_path.file_name().and_then(|value| value.to_str()).unwrap_or("sprite_frames.tres"),
        "godotTexturePath": godot_texture_path,
        "frameWidth": frame_width,
        "frameHeight": frame_height,
        "frameCount": frame_count,
        "fps": animation.fps,
        "loop": animation.looping,
        "frames": frames
    });
    std::fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata)
            .map_err(|error| CommandError::new("serialization_error", error.to_string()))?,
    )?;

    Ok(GodotExportResult {
        png_path: png_path.to_string_lossy().into_owned(),
        sprite_frames_path: sprite_frames_path.to_string_lossy().into_owned(),
        metadata_path: metadata_path.to_string_lossy().into_owned(),
        godot_texture_path,
        width: sheet_width,
        height: frame_height,
        frame_width,
        frame_height,
        frame_count,
    })
}

fn slugify(value: &str) -> String {
    let slug: String = value
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
    if slug.is_empty() {
        "animation".into()
    } else {
        slug.into()
    }
}

fn find_godot_project_root(start: &Path) -> Option<PathBuf> {
    let mut cursor = Some(start);
    while let Some(directory) = cursor {
        if directory.join("project.godot").is_file() {
            return Some(directory.to_path_buf());
        }
        cursor = directory.parent();
    }
    None
}

fn godot_texture_path(
    output_directory: &Path,
    png_path: &Path,
    require_project_root: bool,
) -> CommandResult<String> {
    if let Some(project_root) = find_godot_project_root(output_directory) {
        let relative = png_path.strip_prefix(&project_root).map_err(|_| {
            CommandError::new(
                "godot_export_path_error",
                "Could not resolve the exported texture relative to the Godot project",
            )
        })?;
        return Ok(format!(
            "res://{}",
            relative.to_string_lossy().replace('\\', "/")
        ));
    }
    if require_project_root {
        return Err(CommandError::new(
            "godot_project_not_found",
            "Choose a folder inside a Godot project containing project.godot",
        ));
    }
    Ok(format!(
        "res://{}",
        png_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("spritesheet.png")
    ))
}

fn duration_factor(duration_ms: u32, fps: f64) -> f64 {
    ((duration_ms as f64 * fps / 1000.0) * 1000.0).round() / 1000.0
}

fn escape_godot_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn sprite_frames_resource(
    name: &str,
    fps: f64,
    looping: bool,
    frame_width: u32,
    frame_height: u32,
    frames: &[crate::models::AnimationFrame],
    texture_path: &str,
) -> String {
    let mut output = format!(
        "[gd_resource type=\"SpriteFrames\" load_steps={} format=3]\n\n[ext_resource type=\"Texture2D\" path=\"{}\" id=\"1_sheet\"]\n\n",
        frames.len() + 2,
        escape_godot_string(texture_path)
    );
    for index in 0..frames.len() {
        output.push_str(&format!(
            "[sub_resource type=\"AtlasTexture\" id=\"AtlasTexture_{index}\"]\natlas = ExtResource(\"1_sheet\")\nregion = Rect2({}, 0, {}, {})\nfilter_clip = true\n\n",
            index as u32 * frame_width,
            frame_width,
            frame_height
        ));
    }
    output.push_str("[resource]\nanimations = [{\n\"frames\": [");
    for (index, frame) in frames.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        let duration_ms = frame
            .duration_ms
            .unwrap_or_else(|| (1000.0 / fps).round() as u32)
            .max(1);
        output.push_str(&format!(
            "{{\n\"duration\": {:.3},\n\"texture\": SubResource(\"AtlasTexture_{index}\")\n}}",
            duration_factor(duration_ms, fps)
        ));
    }
    output.push_str(&format!(
        "],\n\"loop\": {},\n\"name\": &\"{}\",\n\"speed\": {:.3}\n}}]\n",
        if looping { "true" } else { "false" },
        escape_godot_string(name),
        fps
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::{duration_factor, godot_texture_path, sprite_frames_resource};
    use crate::models::AnimationFrame;
    use image::{Rgba, RgbaImage};
    use std::{fs, process::Command};
    use uuid::Uuid;

    #[test]
    fn converts_frame_milliseconds_to_godot_duration_factor() {
        assert!((duration_factor(125, 8.0) - 1.0).abs() < f64::EPSILON);
        assert!((duration_factor(250, 8.0) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn detects_res_path_inside_godot_project() {
        let root = std::env::temp_dir().join(format!("godot-export-{}", Uuid::new_v4()));
        let output = root.join("art/hero");
        fs::create_dir_all(&output).expect("output should exist");
        fs::write(root.join("project.godot"), "[application]").expect("project marker writes");
        let png = output.join("hero.png");
        fs::write(&png, []).expect("png placeholder writes");
        assert_eq!(
            godot_texture_path(&output, &png, true).expect("Godot path should resolve"),
            "res://art/hero/hero.png"
        );
        fs::remove_dir_all(root).expect("fixture cleans");
    }

    #[test]
    fn explicit_export_rejects_non_godot_destination() {
        let root = std::env::temp_dir().join(format!("godot-export-invalid-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("output should exist");
        let png = root.join("hero.png");
        let error = godot_texture_path(&root, &png, true)
            .expect_err("explicit export outside a Godot project must fail");
        assert_eq!(error.code, "godot_project_not_found");
        fs::remove_dir_all(root).expect("fixture cleans");
    }

    #[test]
    fn emits_sprite_frames_resource() {
        let frames = vec![
            AnimationFrame {
                asset_id: "a".into(),
                duration_ms: Some(125),
            },
            AnimationFrame {
                asset_id: "b".into(),
                duration_ms: Some(250),
            },
        ];
        let resource = sprite_frames_resource("run", 8.0, true, 64, 64, &frames, "res://hero.png");
        assert!(resource.contains("type=\"SpriteFrames\""));
        assert!(resource.contains("AtlasTexture_1"));
        assert!(resource.contains("\"duration\": 2.000"));
        assert!(resource.contains("\"loop\": true"));
    }

    #[test]
    #[ignore = "requires GODOT_BIN pointing to a Godot 4 executable"]
    fn godot_engine_loads_emitted_spriteframes_resource() {
        let godot = std::env::var("GODOT_BIN").expect("GODOT_BIN must be set");
        let root = std::env::temp_dir().join(format!("godot-engine-smoke-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("project directory should exist");
        fs::write(
            root.join("project.godot"),
            "[application]\nconfig/name=\"Sprite Studio Godot Export Smoke\"\n[rendering]\nrenderer/rendering_method=\"gl_compatibility\"\n",
        )
        .expect("project.godot writes");

        RgbaImage::from_pixel(32, 16, Rgba([255, 255, 255, 255]))
            .save(root.join("hero.png"))
            .expect("fixture spritesheet saves");
        let frames = vec![
            AnimationFrame {
                asset_id: "a".into(),
                duration_ms: Some(125),
            },
            AnimationFrame {
                asset_id: "b".into(),
                duration_ms: Some(250),
            },
        ];
        fs::write(
            root.join("hero_sprite_frames.tres"),
            sprite_frames_resource("run", 8.0, true, 16, 16, &frames, "res://hero.png"),
        )
        .expect("SpriteFrames resource writes");
        fs::write(
            root.join("validate.gd"),
            r#"extends SceneTree

func _initialize():
    var frames = load("res://hero_sprite_frames.tres")
    if frames == null or not frames is SpriteFrames:
        push_error("SpriteFrames resource failed to load")
        quit(10)
        return
    if not frames.has_animation(&"run"):
        push_error("run animation missing")
        quit(11)
        return
    if frames.get_frame_count(&"run") != 2:
        push_error("frame count mismatch")
        quit(12)
        return
    if abs(frames.get_animation_speed(&"run") - 8.0) > 0.001:
        push_error("FPS mismatch")
        quit(13)
        return
    if abs(frames.get_frame_duration(&"run", 1) - 2.0) > 0.001:
        push_error("duration factor mismatch")
        quit(14)
        return
    quit(0)
"#,
        )
        .expect("validation script writes");

        let import = Command::new(&godot)
            .args(["--headless", "--editor", "--path"])
            .arg(&root)
            .arg("--quit")
            .status()
            .expect("Godot import process should start");
        assert!(import.success(), "Godot project import failed: {import}");

        let validation = Command::new(&godot)
            .args(["--headless", "--path"])
            .arg(&root)
            .args(["--script", "validate.gd"])
            .status()
            .expect("Godot validation process should start");
        assert!(
            validation.success(),
            "Godot rejected the emitted SpriteFrames resource: {validation}"
        );
        fs::remove_dir_all(root).expect("fixture cleans");
    }
}
