//! Preparation steps for a menu item, and the curated library they come from.
//!
//! A step is a PRESET or a CUSTOM line. A preset owns its animation, its name
//! and its note, so "Steam milk" reads and looks the same on every item that
//! uses it; an item's step only points at one. Typing your own name is what
//! makes a step custom, and a custom step has no animation.
//!
//! The library is not editable at runtime and is not hand-written into the
//! database. The animation files ship inside the backend image under
//! `STEP_ANIMATIONS_DIR`, each with an entry in `manifest.json` giving its name,
//! its note and where it sits in the list. On start [`reconcile`] walks that
//! folder and makes the `recipe_step_presets` table match it: new files become
//! presets, changed files get a new fingerprint, and a file that stopped
//! shipping is retired rather than deleted so the items using it keep their
//! names. Adding a preset is therefore a file, a manifest line and a deploy —
//! no migration, and no release of the POS or the dashboard.
//!
//! Steps reach clients through the recipe data they already read: the POS gets
//! them inside the menu payload it syncs, with each preset step carrying its
//! animation's address and fingerprint, so a device downloads only the handful
//! of animations its own menu actually uses and never the library.

use std::collections::HashMap;
use std::path::Path;

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgExecutor, PgPool};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

/// Where the animation files are served from. Clients join this with the API
/// base they already hold, so no extra configuration has to agree with nginx.
pub const STATIC_URL_PREFIX: &str = "/static/step-animations";

pub fn animations_dir() -> String {
    std::env::var("STEP_ANIMATIONS_DIR").unwrap_or_else(|_| "./static/step-animations".into())
}

/// The address of a preset's animation, fingerprinted so a replaced file is
/// re-fetched by every cache and an unchanged one never is.
fn animation_url(slug: &str, sha256: &str) -> String {
    format!(
        "{STATIC_URL_PREFIX}/{slug}.json?v={}",
        &sha256[..16.min(sha256.len())]
    )
}

// ── The library ─────────────────────────────────────────────────────────────

/// One curated step the dashboard offers.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct RecipeStepPreset {
    pub slug: String,
    pub name: String,
    pub name_ar: String,
    pub note: Option<String>,
    pub note_ar: Option<String>,
    /// Path to the animation, relative to the API base.
    pub animation_url: String,
    pub animation_sha256: String,
    pub bytes: i32,
    pub sort_order: i16,
}

#[derive(sqlx::FromRow)]
struct PresetRow {
    slug: String,
    name: String,
    name_ar: String,
    note: Option<String>,
    note_ar: Option<String>,
    sha256: String,
    bytes: i32,
    sort_order: i16,
}

impl From<PresetRow> for RecipeStepPreset {
    fn from(r: PresetRow) -> Self {
        Self {
            animation_url: animation_url(&r.slug, &r.sha256),
            animation_sha256: r.sha256,
            slug: r.slug,
            name: r.name,
            name_ar: r.name_ar,
            note: r.note,
            note_ar: r.note_ar,
            bytes: r.bytes,
            sort_order: r.sort_order,
        }
    }
}

/// What `manifest.json` holds for one preset.
#[derive(Debug, Clone, Deserialize)]
struct ManifestEntry {
    name: String,
    name_ar: String,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    note_ar: Option<String>,
    #[serde(default)]
    order: Option<i16>,
}

/// One animation file as it shipped: its slug, its names and its bytes' hash.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedPreset {
    pub slug: String,
    pub entry_name: String,
    pub entry_name_ar: String,
    pub note: Option<String>,
    pub note_ar: Option<String>,
    pub sha256: String,
    pub bytes: i32,
    pub sort_order: i16,
}

/// Read the folder: every `<slug>.json` beside a `manifest.json` entry. A file
/// with no entry is skipped with a warning rather than shipped nameless, and a
/// slug that is not `[a-z0-9_]` is skipped so it can never collide with a URL.
/// Pure over the filesystem, so [`reconcile`] stays testable.
pub fn scan(dir: &Path) -> Vec<ScannedPreset> {
    let manifest: HashMap<String, ManifestEntry> =
        std::fs::read_to_string(dir.join("manifest.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, serde_json::Value>>(&s).ok())
            .map(|m| {
                m.into_iter()
                    .filter(|(k, _)| !k.starts_with('_'))
                    .filter_map(|(k, v)| {
                        serde_json::from_value::<ManifestEntry>(v)
                            .ok()
                            .map(|e| (k, e))
                    })
                    .collect()
            })
            .unwrap_or_default();

    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::warn!(dir = %dir.display(), "step animations folder missing — no presets");
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") || slug == "manifest" {
            continue;
        }
        if !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            tracing::warn!(file = %path.display(), "step animation skipped: slug must be [a-z0-9_]");
            continue;
        }
        let Some(names) = manifest.get(slug) else {
            tracing::warn!(slug, "step animation has no manifest entry — skipped");
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            tracing::warn!(file = %path.display(), "step animation unreadable — skipped");
            continue;
        };
        out.push(ScannedPreset {
            slug: slug.to_string(),
            entry_name: names.name.clone(),
            entry_name_ar: names.name_ar.clone(),
            note: names.note.clone(),
            note_ar: names.note_ar.clone(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as i32,
            sort_order: names.order.unwrap_or(0),
        });
    }
    out.sort_by(|a, b| (a.sort_order, &a.slug).cmp(&(b.sort_order, &b.slug)));
    out
}

/// Make the preset table match the folder. Runs once on start, on the owner
/// pool (the library is global reference data). Never fails boot: a folder that
/// cannot be read leaves whatever presets are already recorded in place.
pub async fn reconcile(pool: &PgPool, dir: &Path) -> Result<(), sqlx::Error> {
    let scanned = scan(dir);
    if scanned.is_empty() {
        tracing::warn!("no step animations found — leaving the preset library as it is");
        return Ok(());
    }
    let slugs: Vec<String> = scanned.iter().map(|s| s.slug.clone()).collect();
    for s in &scanned {
        sqlx::query(
            "INSERT INTO recipe_step_presets (slug, name, name_ar, note, note_ar, sha256, bytes, sort_order) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (slug) DO UPDATE SET \
                name = EXCLUDED.name, name_ar = EXCLUDED.name_ar, \
                note = EXCLUDED.note, note_ar = EXCLUDED.note_ar, \
                sha256 = EXCLUDED.sha256, bytes = EXCLUDED.bytes, \
                sort_order = EXCLUDED.sort_order, is_active = true, updated_at = now()",
        )
        .bind(&s.slug)
        .bind(&s.entry_name)
        .bind(&s.entry_name_ar)
        .bind(&s.note)
        .bind(&s.note_ar)
        .bind(&s.sha256)
        .bind(s.bytes)
        .bind(s.sort_order)
        .execute(pool)
        .await?;
    }
    // A preset whose file stopped shipping is retired, never deleted: the items
    // that use it keep showing its name, and the recipe stays readable.
    let retired = sqlx::query(
        "UPDATE recipe_step_presets SET is_active = false, updated_at = now() \
         WHERE is_active AND slug <> ALL($1)",
    )
    .bind(&slugs)
    .execute(pool)
    .await?
    .rows_affected();
    tracing::info!(
        presets = scanned.len(),
        retired,
        "step preset library reconciled"
    );
    Ok(())
}

pub async fn list_presets<'e, E>(
    exec: E,
    include_retired: bool,
) -> Result<Vec<RecipeStepPreset>, AppError>
where
    E: PgExecutor<'e>,
{
    let rows: Vec<PresetRow> = sqlx::query_as(
        "SELECT slug, name, name_ar, note, note_ar, sha256, bytes, sort_order \
         FROM recipe_step_presets WHERE (is_active OR $1) ORDER BY sort_order, slug",
    )
    .bind(include_retired)
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

// ── An item's steps ─────────────────────────────────────────────────────────

/// One step, resolved for display: whatever its kind, it has a name, and a
/// preset step also carries its note and the animation to play.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct RecipeStep {
    pub position: i16,
    /// `preset` | `custom`.
    pub kind: String,
    /// The preset this step uses, if any.
    pub preset_slug: Option<String>,
    /// The preset's name, or the typed name of a custom step.
    pub name: String,
    pub name_ar: String,
    pub note: Option<String>,
    pub note_ar: Option<String>,
    /// Present only for a preset whose animation still ships. `None` on a
    /// custom step, and on a retired preset — clients show the name alone.
    pub animation_url: Option<String>,
    pub animation_sha256: Option<String>,
}

#[derive(sqlx::FromRow)]
struct StepRow {
    position: i16,
    kind: String,
    preset_slug: Option<String>,
    title: Option<String>,
    title_ar: Option<String>,
    preset_name: Option<String>,
    preset_name_ar: Option<String>,
    note: Option<String>,
    note_ar: Option<String>,
    sha256: Option<String>,
    is_active: Option<bool>,
}

impl From<StepRow> for RecipeStep {
    fn from(r: StepRow) -> Self {
        let live = r.is_active.unwrap_or(false);
        let (url, sha) = match (&r.preset_slug, &r.sha256, live) {
            (Some(slug), Some(sha), true) => (Some(animation_url(slug, sha)), Some(sha.clone())),
            _ => (None, None),
        };
        // A custom step is named by whoever typed it; a preset step by the
        // library. Either way one field pair carries the name, so a client
        // renders both kinds the same way. A name typed in one language only
        // shows in both rather than leaving the other blank.
        let name = r
            .preset_name
            .clone()
            .or_else(|| r.title.clone())
            .or_else(|| r.title_ar.clone())
            .unwrap_or_default();
        let name_ar = r
            .preset_name_ar
            .clone()
            .or_else(|| r.title_ar.clone())
            .or_else(|| r.title.clone())
            .unwrap_or_default();
        Self {
            position: r.position,
            kind: r.kind,
            preset_slug: r.preset_slug,
            name,
            name_ar,
            note: r.note,
            note_ar: r.note_ar,
            animation_url: url,
            animation_sha256: sha,
        }
    }
}

pub async fn fetch_item_steps<'e, E>(
    exec: E,
    menu_item_id: Uuid,
) -> Result<Vec<RecipeStep>, AppError>
where
    E: PgExecutor<'e>,
{
    let rows: Vec<StepRow> = sqlx::query_as(
        "SELECT s.position, s.kind, s.preset_slug, s.title, s.title_ar, \
                p.name AS preset_name, p.name_ar AS preset_name_ar, p.note, p.note_ar, \
                p.sha256, p.is_active \
         FROM menu_item_recipe_steps s \
         LEFT JOIN recipe_step_presets p ON p.slug = s.preset_slug \
         WHERE s.menu_item_id = $1 ORDER BY s.position",
    )
    .bind(menu_item_id)
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// The steps of every item in an org, keyed by item — one query for the whole
/// menu, so the list endpoint the POS syncs never runs a query per item.
pub async fn fetch_org_steps<'e, E>(
    exec: E,
    org_id: Uuid,
) -> Result<HashMap<Uuid, Vec<RecipeStep>>, AppError>
where
    E: PgExecutor<'e>,
{
    #[derive(sqlx::FromRow)]
    struct OrgStepRow {
        menu_item_id: Uuid,
        #[sqlx(flatten)]
        step: StepRow,
    }
    let rows: Vec<OrgStepRow> = sqlx::query_as(
        "SELECT s.menu_item_id, s.position, s.kind, s.preset_slug, s.title, s.title_ar, \
                p.name AS preset_name, p.name_ar AS preset_name_ar, p.note, p.note_ar, \
                p.sha256, p.is_active \
         FROM menu_item_recipe_steps s \
         LEFT JOIN recipe_step_presets p ON p.slug = s.preset_slug \
         WHERE s.org_id = $1 \
         ORDER BY s.menu_item_id, s.position",
    )
    .bind(org_id)
    .fetch_all(exec)
    .await?;
    let mut out: HashMap<Uuid, Vec<RecipeStep>> = HashMap::new();
    for r in rows {
        out.entry(r.menu_item_id).or_default().push(r.step.into());
    }
    Ok(out)
}

// ── Writing ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RecipeStepInput {
    /// `preset` | `custom`.
    pub kind: String,
    /// Required for `preset`.
    #[serde(default)]
    pub preset_slug: Option<String>,
    /// The typed name, for `custom`. Either language will do.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_ar: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PutRecipeStepsRequest {
    /// The whole list, in order. Replaces what was there.
    pub steps: Vec<RecipeStepInput>,
}

/// A validated step, ready to insert.
struct PendingStep {
    position: i16,
    kind: &'static str,
    preset_slug: Option<String>,
    title: Option<String>,
    title_ar: Option<String>,
}

const MAX_STEPS: usize = 40;
const MAX_TITLE: usize = 120;

fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

#[utoipa::path(get, path = "/recipes/step-presets", tag = "recipes",
    responses((status = 200, body = Vec<RecipeStepPreset>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_step_presets(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "read").await?;
    Ok(HttpResponse::Ok().json(list_presets(pool.get_ref(), false).await?))
}

#[utoipa::path(get, path = "/recipes/steps/{menu_item_id}", tag = "recipes",
    params(("menu_item_id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, body = Vec<RecipeStep>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_recipe_steps(
    req: HttpRequest,
    pool: crate::db::Db,
    menu_item_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "read").await?;
    super::handlers::require_menu_item_org(pool.get_ref(), &claims, *menu_item_id).await?;
    Ok(HttpResponse::Ok().json(fetch_item_steps(pool.get_ref(), *menu_item_id).await?))
}

/// Replace an item's steps, in one transaction.
#[utoipa::path(put, path = "/recipes/steps/{menu_item_id}", tag = "recipes",
    params(("menu_item_id" = Uuid, Path, description = "Menu item ID")),
    request_body = PutRecipeStepsRequest,
    responses((status = 200, body = Vec<RecipeStep>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_recipe_steps(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Option<web::Data<crate::menu::cache::MenuCache>>,
    menu_item_id: web::Path<Uuid>,
    body: web::Json<PutRecipeStepsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::orgs::handlers::extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "update").await?;
    let org_id =
        super::handlers::require_menu_item_org(pool.get_ref(), &claims, *menu_item_id).await?;
    if body.steps.len() > MAX_STEPS {
        return Err(AppError::BadRequest(format!("At most {MAX_STEPS} steps")));
    }

    // Resolve every step before writing anything, so a bad list leaves the
    // saved one untouched.
    let live: Vec<String> =
        sqlx::query_scalar("SELECT slug FROM recipe_step_presets WHERE is_active")
            .fetch_all(pool.get_ref())
            .await?;
    let mut rows: Vec<PendingStep> = Vec::with_capacity(body.steps.len());
    for (i, s) in body.steps.iter().enumerate() {
        let n = i + 1;
        match s.kind.as_str() {
            "preset" => {
                let slug = clean(&s.preset_slug)
                    .ok_or_else(|| AppError::BadRequest(format!("Step {n} needs a preset")))?;
                if !live.contains(&slug) {
                    return Err(AppError::BadRequest(format!(
                        "Step {n}: no such step '{slug}'"
                    )));
                }
                rows.push(PendingStep {
                    position: n as i16,
                    kind: "preset",
                    preset_slug: Some(slug),
                    title: None,
                    title_ar: None,
                });
            }
            "custom" => {
                let title = clean(&s.title);
                let title_ar = clean(&s.title_ar);
                if title.is_none() && title_ar.is_none() {
                    return Err(AppError::BadRequest(format!("Step {n} needs a name")));
                }
                if title
                    .as_ref()
                    .is_some_and(|t| t.chars().count() > MAX_TITLE)
                    || title_ar
                        .as_ref()
                        .is_some_and(|t| t.chars().count() > MAX_TITLE)
                {
                    return Err(AppError::BadRequest(format!("Step {n}'s name is too long")));
                }
                rows.push(PendingStep {
                    position: n as i16,
                    kind: "custom",
                    preset_slug: None,
                    title,
                    title_ar,
                });
            }
            other => {
                return Err(AppError::BadRequest(format!(
                    "Step {n}: unknown kind '{other}'"
                )));
            }
        }
    }

    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("DELETE FROM menu_item_recipe_steps WHERE menu_item_id = $1")
        .bind(*menu_item_id)
        .execute(&mut *tx)
        .await?;
    for r in &rows {
        sqlx::query(
            "INSERT INTO menu_item_recipe_steps \
                (org_id, menu_item_id, position, kind, preset_slug, title, title_ar) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(org_id)
        .bind(*menu_item_id)
        .bind(r.position)
        .bind(r.kind)
        .bind(&r.preset_slug)
        .bind(&r.title)
        .bind(&r.title_ar)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    // Steps ride inside the menu payload, so the org's cached copy is stale now.
    if let Some(c) = cache {
        c.invalidate(org_id);
    }
    Ok(HttpResponse::Ok().json(fetch_item_steps(pool.get_ref(), *menu_item_id).await?))
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn only_named_lowercase_json_files_become_presets() {
        let dir = tempfile::tempdir().unwrap();
        let write = |n: &str, b: &[u8]| std::fs::write(dir.path().join(n), b).unwrap();
        write("steam_milk.json", br#"{"v":"5.7.4","layers":[]}"#);
        write("stir.json", br#"{"v":"5.7.4","layers":[1]}"#);
        write("Bad-Name.json", b"{}");
        write("orphan.json", b"{}"); // no manifest entry
        write("notes.txt", b"x");
        write(
            "manifest.json",
            r#"{"_comment":"ignored",
                "steam_milk":{"name":"Steam milk","name_ar":"تبخير","note":"60 °C","note_ar":"٦٠°","order":20},
                "stir":{"name":"Stir","name_ar":"تقليب","order":10}}"#
                .as_bytes(),
        );

        let found = scan(dir.path());
        assert_eq!(
            found.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            vec!["stir", "steam_milk"],
            "manifest order decides the list; unnamed and badly-named files are skipped"
        );
        let milk = &found[1];
        assert_eq!(
            (milk.entry_name.as_str(), milk.note.as_deref()),
            ("Steam milk", Some("60 °C"))
        );
        assert_eq!(milk.sha256.len(), 64);
        assert!(milk.bytes > 0);
        // The fingerprint is the file's, so two different files never collide.
        assert_ne!(found[0].sha256, found[1].sha256);
        assert!(scan(Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn the_shipped_library_is_complete_and_addressable() {
        let found = scan(Path::new("static/step-animations"));
        assert!(
            found.len() >= 60,
            "every shipped animation is named in the manifest"
        );
        for p in &found {
            assert!(
                !p.entry_name.is_empty() && !p.entry_name_ar.is_empty(),
                "{} needs both names",
                p.slug
            );
            let url = animation_url(&p.slug, &p.sha256);
            assert!(
                url.starts_with("/static/step-animations/") && url.contains("?v="),
                "{url}"
            );
        }
    }
}
