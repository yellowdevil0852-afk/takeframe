use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

#[derive(Default)]
struct ExtractionState {
    child: Mutex<Option<CommandChild>>,
    cancelled: AtomicBool,
}

const VIDEO_EXTENSIONS: [&str; 7] = ["mp4", "mov", "mkv", "avi", "webm", "m4v", "flv"];

fn is_video_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Resolves a path into a list of video files: itself if it's a video file,
/// or every video file directly inside it (non-recursive) if it's a directory.
#[tauri::command]
fn resolve_video_paths(path: String) -> Result<Vec<String>, String> {
    let p = std::path::Path::new(&path);
    if p.is_dir() {
        let mut videos: Vec<String> = std::fs::read_dir(p)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|entry| entry.is_file() && is_video_file(entry))
            .map(|entry| entry.to_string_lossy().to_string())
            .collect();
        videos.sort();
        Ok(videos)
    } else if p.is_file() && is_video_file(p) {
        Ok(vec![path])
    } else {
        Ok(vec![])
    }
}

#[derive(Serialize)]
struct VideoInfo {
    duration: f64,
    width: u32,
    height: u32,
    fps: f64,
    codec_name: String,
    format_name: String,
    file_size: u64,
}

fn parse_fraction(s: &str) -> Option<f64> {
    let mut parts = s.split('/');
    let num: f64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some(den_str) => {
            let den: f64 = den_str.parse().ok()?;
            if den == 0.0 {
                None
            } else {
                Some(num / den)
            }
        }
        None => Some(num),
    }
}

#[tauri::command]
async fn get_video_info(app: tauri::AppHandle, path: String) -> Result<VideoInfo, String> {
    let sidecar = app
        .shell()
        .sidecar("ffprobe")
        .map_err(|e| e.to_string())?
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            &path,
        ]);

    let output = sidecar.output().await.map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| e.to_string())?;

    let video_stream = json["streams"]
        .as_array()
        .and_then(|streams| streams.iter().find(|s| s["codec_type"] == "video"))
        .ok_or_else(|| "No video stream found in file".to_string())?;

    let fps = video_stream["r_frame_rate"]
        .as_str()
        .and_then(parse_fraction)
        .unwrap_or(0.0);

    let duration = json["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);

    let file_size = json["format"]["size"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    Ok(VideoInfo {
        duration,
        width: video_stream["width"].as_u64().unwrap_or(0) as u32,
        height: video_stream["height"].as_u64().unwrap_or(0) as u32,
        fps,
        codec_name: video_stream["codec_name"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        format_name: json["format"]["format_name"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        file_size,
    })
}

#[derive(Serialize, Clone)]
struct ExtractionProgress {
    frame: u64,
    out_time_secs: f64,
    done: bool,
}

fn list_frames_in_dir(dir: &str, ext: &str) -> Result<Vec<String>, String> {
    if !std::path::Path::new(dir).exists() {
        return Ok(Vec::new());
    }
    let mut frames: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|x| x.to_string_lossy() == ext)
                .unwrap_or(false)
        })
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    frames.sort();
    Ok(frames)
}

fn sanitize_filename(name: &str, fallback: &str) -> String {
    // `%` is excluded too: prefixes flow into an ffmpeg printf-style output
    // pattern ("{prefix}_%05d.ext"), and a stray `%` in user input could be
    // misread as another format specifier.
    let cleaned: String = name
        .chars()
        .map(|c| if r#"<>:"/\|?*%"#.contains(c) || (c as u32) < 32 { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn video_output_dir(output_dir: &str, input_path: &str) -> std::path::PathBuf {
    let stem = std::path::Path::new(input_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    std::path::Path::new(output_dir).join(sanitize_filename(stem, "video"))
}

/// Frames are always extracted here first so nothing touches the user's chosen
/// output folder until they explicitly export. Keyed by video filename so
/// re-previewing the same video overwrites its own staged frames.
fn staging_dir_for(app: &tauri::AppHandle, input_path: &str) -> Result<std::path::PathBuf, String> {
    let stem = std::path::Path::new(input_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    Ok(cache_dir.join("preview").join(sanitize_filename(stem, "video")))
}

#[tauri::command]
fn count_existing_frames(
    output_dir: String,
    input_path: String,
    format: String,
) -> Result<u64, String> {
    let ext = if format == "jpg" { "jpg" } else { "png" };
    let dir = video_output_dir(&output_dir, &input_path);
    Ok(list_frames_in_dir(&dir.to_string_lossy(), ext)?.len() as u64)
}

#[derive(Serialize)]
struct ExtractionResult {
    output_dir: String,
    ext: String,
    frames: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sprite_path: Option<String>,
}

/// Frames are numbered sequentially with no gaps by ffmpeg during preview, so
/// this same pattern string works both to write them (preview_frames) and to
/// read them back as an image sequence (sprite sheet generation).
fn frame_pattern(dir: &std::path::Path, prefix: &str, ext: &str) -> String {
    format!("{}/{}_%05d.{}", dir.to_string_lossy(), prefix, ext)
}

// A sprite sheet this size (23x23 cells) is already a very large image; past
// this we'd rather skip it than produce a multi-hundred-megapixel file.
const SPRITE_MAX_FRAMES: usize = 512;

// Each cell is downscaled to this before tiling, so the sheet's total pixel
// dimensions stay well under encoder limits even at SPRITE_MAX_FRAMES frames.
const SPRITE_CELL_MAX: u32 = 256;

// Fixed edge softness for the colorkey filter; not user-adjustable, only
// similarity is exposed in the UI.
const CHROMA_KEY_BLEND: f64 = 0.05;

/// `colorkey=0xRRGGBB:similarity:blend,format=rgba`, or None if the color
/// isn't a valid 6-digit hex string (defensive: it should always be one,
/// since the frontend only ever writes hex it read back from a canvas).
fn chroma_key_filter(color: &str, similarity: f64) -> Option<String> {
    let hex = color.trim_start_matches('#');
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        // ffmpeg's colorkey rejects similarity outside [1e-5, 1] — a 0% slider
        // reading maps to exactly 0.0, which it errors out on entirely.
        let clamped = similarity.clamp(0.00001, 1.0);
        Some(format!(
            "colorkey=0x{}:{}:{},format=rgba",
            hex, clamped, CHROMA_KEY_BLEND
        ))
    } else {
        None
    }
}

#[tauri::command]
async fn preview_frames(
    app: tauri::AppHandle,
    state: State<'_, ExtractionState>,
    input_path: String,
    mode: String,
    value: f64,
    format: String,
    prefix: String,
    start_secs: Option<f64>,
    end_secs: Option<f64>,
    max_side: Option<u32>,
) -> Result<ExtractionResult, String> {
    state.cancelled.store(false, Ordering::SeqCst);

    let staging_dir = staging_dir_for(&app, &input_path)?;
    // Wipe any frames from a previous preview run first: ffmpeg's -y only
    // overwrites files it rewrites, so a smaller frame count or a changed
    // prefix would otherwise leave stale files mixed in with the new ones.
    let _ = std::fs::remove_dir_all(&staging_dir);
    std::fs::create_dir_all(&staging_dir).map_err(|e| e.to_string())?;

    let ext = if format == "jpg" { "jpg" } else { "png" };
    let safe_prefix = sanitize_filename(&prefix, "frame");
    let pattern = frame_pattern(&staging_dir, &safe_prefix, ext);

    let sampling_filter = match mode.as_str() {
        "all" => String::new(),
        "seconds" => format!("fps=1/{}", value),
        "frames" => format!(
            "select='not(mod(n\\,{}))',setpts=N/FRAME_RATE/TB",
            value as u64
        ),
        "scene" => format!("select='gt(scene,{})',setpts=N/FRAME_RATE/TB", value),
        _ => return Err(format!("invalid mode: {}", mode)),
    };

    // Longest side capped at `max`, aspect ratio preserved, never upscaled
    // (min(iw,max) is a no-op when the source is already smaller).
    let scale_filter = max_side.map(|max| {
        format!(
            "scale='if(gt(iw,ih),min(iw,{max}),-2)':'if(gt(iw,ih),-2,min(ih,{max}))'",
            max = max
        )
    });

    let vf: String = [Some(sampling_filter), scale_filter]
        .into_iter()
        .flatten()
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>()
        .join(",");

    // -ss/-to as INPUT options (before -i) so both are absolute timestamps
    // in the original file's timeline, not relative to each other.
    let mut args: Vec<String> = vec!["-y".into()];
    if let Some(start) = start_secs {
        args.push("-ss".into());
        args.push(start.to_string());
    }
    if let Some(end) = end_secs {
        args.push("-to".into());
        args.push(end.to_string());
    }
    args.push("-i".into());
    args.push(input_path);
    if !vf.is_empty() {
        args.push("-vf".into());
        args.push(vf);
    }
    if mode == "frames" || mode == "scene" {
        // Newer ffmpeg builds dropped the old global `-vsync` alias entirely
        // (not just deprecated it) in favor of the per-stream `-fps_mode`.
        args.push("-fps_mode".into());
        args.push("vfr".into());
    }
    if format == "jpg" {
        args.push("-q:v".into());
        args.push("2".into());
    }
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push(pattern);

    let (mut rx, child) = app
        .shell()
        .sidecar("ffmpeg")
        .map_err(|e| e.to_string())?
        .args(args)
        .spawn()
        .map_err(|e| e.to_string())?;

    state.child.lock().map_err(|e| e.to_string())?.replace(child);

    let mut last_frame: u64 = 0;
    let mut run_error: Option<String> = None;
    let mut stderr_lines: Vec<String> = Vec::new();

    while let Some(event) = rx.recv().await {
        match event {
            CommandEvent::Stderr(line) => {
                let text = String::from_utf8_lossy(&line).trim().to_string();
                if !text.is_empty() {
                    stderr_lines.push(text);
                }
            }
            CommandEvent::Stdout(line) => {
                let text = String::from_utf8_lossy(&line);
                for kv in text.lines() {
                    if let Some(v) = kv.strip_prefix("frame=") {
                        last_frame = v.trim().parse().unwrap_or(last_frame);
                    } else if let Some(v) = kv.strip_prefix("out_time_ms=") {
                        let out_time_secs = v.trim().parse::<f64>().unwrap_or(0.0) / 1_000_000.0;
                        let _ = app.emit(
                            "extraction-progress",
                            ExtractionProgress {
                                frame: last_frame,
                                out_time_secs,
                                done: false,
                            },
                        );
                    } else if kv.trim() == "progress=end" {
                        let _ = app.emit(
                            "extraction-progress",
                            ExtractionProgress {
                                frame: last_frame,
                                out_time_secs: 0.0,
                                done: true,
                            },
                        );
                    }
                }
            }
            CommandEvent::Error(e) => {
                run_error = Some(e);
                break;
            }
            CommandEvent::Terminated(payload) => {
                if payload.code != Some(0) {
                    // ffmpeg's own diagnostic (e.g. "Unknown encoder", "No such
                    // file") is far more useful than a bare process exit code,
                    // which is often a meaningless OS-level number on Windows.
                    let detail: String = stderr_lines
                        .iter()
                        .rev()
                        .take(5)
                        .rev()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n");
                    run_error = Some(if detail.is_empty() {
                        format!("ffmpeg 執行失敗（結束代碼 {:?}）", payload.code)
                    } else {
                        detail
                    });
                }
                break;
            }
            _ => {}
        }
    }

    state.child.lock().map_err(|e| e.to_string())?.take();

    if let Some(e) = run_error {
        return if state.cancelled.load(Ordering::SeqCst) {
            Err("cancelled".to_string())
        } else {
            Err(e)
        };
    }

    let frames = list_frames_in_dir(&staging_dir.to_string_lossy(), ext)?;

    Ok(ExtractionResult {
        output_dir: staging_dir.to_string_lossy().to_string(),
        ext: ext.to_string(),
        frames,
        sprite_path: None,
    })
}

#[tauri::command]
async fn export_frames(
    app: tauri::AppHandle,
    input_path: String,
    output_dir: String,
    format: String,
    prefix: String,
    sprite_sheet: bool,
    chroma_key_color: Option<String>,
    chroma_key_similarity: Option<f64>,
) -> Result<ExtractionResult, String> {
    let staged_ext = if format == "jpg" { "jpg" } else { "png" };
    let safe_prefix = sanitize_filename(&prefix, "frame");
    let staging_dir = staging_dir_for(&app, &input_path)?;
    let final_dir = video_output_dir(&output_dir, &input_path);
    std::fs::create_dir_all(&final_dir).map_err(|e| e.to_string())?;

    // Extraction (preview_frames) never applies chroma key — it's purely a
    // preview/export-time decision, so you extract once and can still change
    // your mind about background removal afterward without re-extracting.
    let chroma_key = match (chroma_key_color, chroma_key_similarity) {
        (Some(color), Some(similarity)) => chroma_key_filter(&color, similarity),
        _ => None,
    };

    let staged = list_frames_in_dir(&staging_dir.to_string_lossy(), staged_ext)?;

    // Chroma key always writes PNG (needs alpha) regardless of the staged
    // format; otherwise frames are just copied through as staged.
    let (frames, ext): (Vec<String>, &str) = if let Some(filter) = &chroma_key {
        let ext = "png";
        if staged.is_empty() {
            (Vec::new(), ext)
        } else {
            let args: Vec<String> = vec![
                "-y".into(),
                "-i".into(),
                frame_pattern(&staging_dir, &safe_prefix, staged_ext),
                "-vf".into(),
                filter.clone(),
                frame_pattern(&final_dir, &safe_prefix, ext),
            ];
            let output = app
                .shell()
                .sidecar("ffmpeg")
                .map_err(|e| e.to_string())?
                .args(args)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).to_string());
            }
            (list_frames_in_dir(&final_dir.to_string_lossy(), ext)?, ext)
        }
    } else {
        let mut frames: Vec<String> = Vec::with_capacity(staged.len());
        for src in &staged {
            let src_path = std::path::Path::new(src);
            if let Some(name) = src_path.file_name() {
                let dest = final_dir.join(name);
                std::fs::copy(src_path, &dest).map_err(|e| e.to_string())?;
                frames.push(dest.to_string_lossy().to_string());
            }
        }
        (frames, staged_ext)
    };

    let mut sprite_path: Option<String> = None;
    if sprite_sheet && !frames.is_empty() && frames.len() <= SPRITE_MAX_FRAMES {
        let n = frames.len();
        let cols = (n as f64).sqrt().ceil() as u32;
        let rows = ((n as f64) / (cols as f64)).ceil() as u32;
        let dest = final_dir.join(format!("{}_sprite.{}", safe_prefix, ext));

        // Frames are tiled at their original (possibly full source) resolution
        // otherwise, and a full grid of them can trivially exceed PNG/JPEG
        // encoder size limits (e.g. 240 frames of 1080p tiled 16x15 = a
        // 30720x16200 image, which fails to encode). Each cell is capped to a
        // fixed thumbnail size first; SPRITE_CELL_MAX * the largest possible
        // grid side (ceil(sqrt(SPRITE_MAX_FRAMES))) stays well under those
        // limits. Same aspect-preserving, shrink-only formula as preview's
        // max_side scaling, so every cell still comes out the same size.
        // Sourced from final_dir (not staging_dir) so a sprite sheet reflects
        // the actually-exported frames, chroma-keyed or not.
        let cell_scale = format!(
            "scale='if(gt(iw,ih),min(iw,{max}),-2)':'if(gt(iw,ih),-2,min(ih,{max}))'",
            max = SPRITE_CELL_MAX
        );

        let mut args: Vec<String> = vec![
            "-y".into(),
            "-i".into(),
            frame_pattern(&final_dir, &safe_prefix, ext),
            "-vf".into(),
            format!("{},tile={}x{}", cell_scale, cols, rows),
            "-frames:v".into(),
            "1".into(),
        ];
        if ext == "jpg" {
            args.push("-q:v".into());
            args.push("2".into());
        }
        args.push(dest.to_string_lossy().to_string());

        let output = app
            .shell()
            .sidecar("ffmpeg")
            .map_err(|e| e.to_string())?
            .args(args)
            .output()
            .await
            .map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).to_string());
        }
        sprite_path = Some(dest.to_string_lossy().to_string());
    }

    // Staged frames are kept around (not deleted) after export: chroma key
    // is applied at export time, so the user can change their mind about it
    // and re-export without needing to re-run extraction. They're cleaned up
    // when the video is re-previewed (wiped at the start of preview_frames)
    // or removed from the queue (cleanup_staging).

    Ok(ExtractionResult {
        output_dir: final_dir.to_string_lossy().to_string(),
        ext: ext.to_string(),
        frames,
        sprite_path,
    })
}

#[tauri::command]
fn cancel_extraction(state: State<'_, ExtractionState>) -> Result<(), String> {
    state.cancelled.store(true, Ordering::SeqCst);
    if let Some(child) = state.child.lock().map_err(|e| e.to_string())?.take() {
        child.kill().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Removes a video's staged preview frames. Called when a queue item is
/// removed or the queue is cleared, so abandoned previews don't accumulate
/// in the cache directory forever. A no-op if nothing was ever staged.
#[tauri::command]
fn cleanup_staging(app: tauri::AppHandle, input_path: String) -> Result<(), String> {
    let dir = staging_dir_for(&app, &input_path)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Manual per-frame retouch is a standalone save, independent of the
/// preview/export pipeline: it writes one already-edited PNG (from the
/// frontend's canvas) straight to the video's output folder, using the same
/// naming convention batch export would, so it lines up with the rest of
/// that video's exported frames if a full export also runs.
#[tauri::command]
fn save_retouched_frame(
    output_dir: String,
    input_path: String,
    prefix: String,
    frame_number: u32,
    data: Vec<u8>,
) -> Result<String, String> {
    let safe_prefix = sanitize_filename(&prefix, "frame");
    let final_dir = video_output_dir(&output_dir, &input_path);
    std::fs::create_dir_all(&final_dir).map_err(|e| e.to_string())?;
    let dest = final_dir.join(format!("{}_{:05}.png", safe_prefix, frame_number));
    std::fs::write(&dest, &data).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

#[derive(Serialize, Deserialize)]
struct AppSettings {
    output_dir: String,
    mode: String,
    value: f64,
    format: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    scene_threshold: f64,
    #[serde(default)]
    start_secs: Option<f64>,
    #[serde(default)]
    end_secs: Option<f64>,
    #[serde(default)]
    fixed_fps: f64,
    #[serde(default)]
    max_side: Option<u32>,
    #[serde(default = "default_true")]
    auto_clear_preview: bool,
    #[serde(default)]
    sprite_sheet: bool,
    #[serde(default = "default_true")]
    apply_to_all: bool,
    #[serde(default = "default_retouch_brush_size")]
    retouch_brush_size: f64,
}

fn default_true() -> bool {
    true
}

fn default_retouch_brush_size() -> f64 {
    24.0
}

fn settings_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

#[tauri::command]
fn load_settings(app: tauri::AppHandle) -> Result<Option<AppSettings>, String> {
    let path = settings_path(&app)?;
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&data).map(Some).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, settings: AppSettings) -> Result<(), String> {
    let path = settings_path(&app)?;
    let data = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(&path, data).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(ExtractionState::default())
        .invoke_handler(tauri::generate_handler![
            resolve_video_paths,
            get_video_info,
            preview_frames,
            export_frames,
            cancel_extraction,
            cleanup_staging,
            count_existing_frames,
            save_retouched_frame,
            load_settings,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
