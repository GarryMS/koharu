//! Text style preset routes.
//!
//! Presets live in the app data directory under `font-presets/*.json`.

use std::fs;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use camino::{Utf8Path, Utf8PathBuf};
use koharu_core::TextStyle;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

const PRESETS_DIR: &str = "font-presets";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default()
        .routes(routes!(list_font_presets))
        .routes(routes!(create_font_preset))
        .routes(routes!(delete_font_preset))
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FontPreset {
    pub id: String,
    pub name: String,
    pub style: TextStyle,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateFontPresetRequest {
    pub name: String,
    pub style: TextStyle,
}

#[utoipa::path(get, path = "/font-presets", responses((status = 200, body = Vec<FontPreset>)))]
async fn list_font_presets(State(app): State<AppState>) -> ApiResult<Json<Vec<FontPreset>>> {
    let dir = presets_dir(&app.config.load().data.path);
    let presets = read_presets(&dir).map_err(ApiError::internal)?;
    Ok(Json(presets))
}

#[utoipa::path(
    post,
    path = "/font-presets",
    request_body = CreateFontPresetRequest,
    responses((status = 200, body = FontPreset))
)]
async fn create_font_preset(
    State(app): State<AppState>,
    Json(req): Json<CreateFontPresetRequest>,
) -> ApiResult<Json<FontPreset>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("preset name cannot be empty"));
    }

    let dir = presets_dir(&app.config.load().data.path);
    fs::create_dir_all(dir.as_std_path())
        .map_err(|e| ApiError::internal(anyhow::anyhow!("create {dir}: {e}")))?;

    let preset = FontPreset {
        id: unique_preset_id(name),
        name: name.to_string(),
        style: req.style,
    };
    let path = preset_path(&dir, &preset.id)?;
    let json = serde_json::to_vec_pretty(&preset).map_err(anyhow::Error::new)?;
    fs::write(path.as_std_path(), json)
        .map_err(|e| ApiError::internal(anyhow::anyhow!("write {path}: {e}")))?;

    Ok(Json(preset))
}

#[utoipa::path(
    delete,
    path = "/font-presets/{id}",
    params(("id" = String, Path, description = "Preset id")),
    responses((status = 204))
)]
async fn delete_font_preset(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let dir = presets_dir(&app.config.load().data.path);
    let path = preset_path(&dir, &id)?;
    if !path.exists() {
        return Err(ApiError::not_found(format!("font preset not found: {id}")));
    }
    fs::remove_file(path.as_std_path())
        .map_err(|e| ApiError::internal(anyhow::anyhow!("delete {path}: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

fn presets_dir(data_root: &Utf8Path) -> Utf8PathBuf {
    data_root.join(PRESETS_DIR)
}

fn read_presets(dir: &Utf8Path) -> anyhow::Result<Vec<FontPreset>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut presets = Vec::new();
    for entry in fs::read_dir(dir.as_std_path())? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| anyhow::anyhow!("non-utf8 preset path: {}", path.display()))?;
        if path.extension() != Some("json") {
            continue;
        }
        match fs::read_to_string(path.as_std_path())
            .ok()
            .and_then(|text| serde_json::from_str::<FontPreset>(&text).ok())
        {
            Some(preset) => presets.push(preset),
            None => tracing::warn!(path = %path, "skipping invalid font preset"),
        }
    }
    presets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(presets)
}

fn preset_path(dir: &Utf8Path, id: &str) -> ApiResult<Utf8PathBuf> {
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request("invalid preset id"));
    }
    Ok(dir.join(format!("{id}.json")))
}

fn unique_preset_id(name: &str) -> String {
    let slug = slugify(name);
    let suffix = Uuid::now_v7()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    format!("{slug}-{suffix}")
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        let is_word = ch.is_ascii_alphanumeric();
        if is_word {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "preset".to_string()
    } else {
        slug
    }
}
