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
    sheet.save(&png_path)?;

    let godot_texture_path = godot_texture_path(&output_directory, &png_path);
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

fn godot_texture_path(output_directory: &Path, png_path: &Path) -> String {
    let mut cursor = Some(output_directory);
    while let Some(directory) = cursor {
        if directory.join("project.godot").is_file() {
            if let Ok(relative) = png_path.strip_prefix(directory) {
                return format!("res://{}", relative.to_string_lossy().replace('\\', "/"));
            }
        }
        cursor = directory.parent();
    }
    format!(
        "res://{}",
        png_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("spritesheet.png")
    )
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
    use std::fs;
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
        assert_eq!(godot_texture_path(&output, &png), "res://art/hero/hero.png");
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
}
