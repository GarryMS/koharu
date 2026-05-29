//! A loaded project. One `ProjectSession` = one `.khrproj/` directory.
//!
//! Holds:
//!   - an exclusive `.lock` via `fs4` (refuses second opener)
//!   - the in-memory `Scene` behind a `parking_lot::RwLock` (never held across `.await`)
//!   - the `History` behind a `Mutex` (linear, all writes serialized)
//!   - the `BlobStore` (content-addressed images)
//!
//! On-disk layout:
//!   `.khrproj/project.toml`    — TOML-encoded `ProjectMeta`
//!   `.khrproj/scene.bin`       — postcard-encoded `Snapshot { epoch, scene }`
//!   `.khrproj/history.log`     — append-only `LogFrame { epoch, op }`
//!   `.khrproj/blobs/ab/cdef…`  — content-addressed blobs
//!   `.khrproj/.lock`           — fs4 exclusive lock (session lifetime)

use std::fs::File;
use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use atomicwrites::{AtomicFile, OverwriteBehavior};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;
use fs4::FileExt;
use koharu_core::{Scene, op::Op};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::blobs::BlobStore;
use crate::history::{self, History};

const SCENE_FILE: &str = "scene.bin";
const LOG_FILE: &str = "history.log";
const LOCK_FILE: &str = ".lock";
const BLOBS_DIR: &str = "blobs";
const CACHE_DIR: &str = "cache";
const PROJECT_TOML: &str = "project.toml";

/// Snapshot written to `scene.bin`.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    epoch: u64,
    scene: Scene,
}

/// A loaded project.
pub struct ProjectSession {
    pub dir: Utf8PathBuf,
    pub scene: RwLock<Scene>,
    pub history: Mutex<History>,
    pub blobs: Arc<BlobStore>,
    /// Held for the lifetime of the session.
    _lock: File,
}

impl ProjectSession {
    /// Open an existing `.khrproj/` directory.
    pub fn open(dir: impl AsRef<Utf8Path>) -> Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.is_dir() {
            anyhow::bail!("not a project directory: {dir}");
        }
        Self::open_inner(dir, false)
    }

    /// Create a fresh `.khrproj/` at `dir`, failing if it already exists.
    pub fn create(dir: impl AsRef<Utf8Path>, name: impl Into<String>) -> Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(dir.as_std_path())
            .with_context(|| format!("create project dir {dir}"))?;
        // Project should be empty.
        let is_empty = std::fs::read_dir(dir.as_std_path())?.next().is_none();
        if !is_empty {
            anyhow::bail!("project directory not empty: {dir}");
        }
        // Seed the TOML with the name so open_inner can load it.
        let meta = ProjectTomlFile {
            name: name.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        std::fs::write(
            dir.join(PROJECT_TOML).as_std_path(),
            toml::to_string_pretty(&meta)?,
        )?;
        Self::open_inner(dir, true)
    }

    fn open_inner(dir: Utf8PathBuf, creating: bool) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir.join(BLOBS_DIR).as_std_path())?;
        std::fs::create_dir_all(dir.join(CACHE_DIR).as_std_path())?;

        // Exclusive lock — one opener at a time.
        let lock_path = dir.join(LOCK_FILE);
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path.as_std_path())
            .with_context(|| format!("open lock file {}", lock_path))?;
        FileExt::try_lock(&lock).context("project is already open elsewhere")?;

        let blobs = Arc::new(BlobStore::open(dir.join(BLOBS_DIR).as_std_path())?);

        // Load or synthesize the scene + epoch.
        let (mut scene, mut epoch) = load_snapshot(&dir, creating)?;
        // Replay any log frames past the snapshot epoch.
        let log_path = dir.join(LOG_FILE);
        epoch = history::replay(log_path.as_std_path(), epoch, &mut scene)
            .with_context(|| format!("replay log {}", log_path))?;

        let history_obj = History::open(log_path.as_std_path(), epoch)?;

        Ok(Arc::new(Self {
            dir,
            scene: RwLock::new(scene),
            history: Mutex::new(history_obj),
            blobs,
            _lock: lock,
        }))
    }

    // --- scene mutation ----------------------------------------------------

    /// Apply an Op. Returns the epoch after apply.
    pub fn apply(&self, op: Op) -> Result<u64> {
        let mut history = self.history.lock();
        let mut scene = self.scene.write();
        history.apply(&mut scene, op)
    }

    pub fn undo(&self) -> Result<Option<(u64, Op)>> {
        let mut history = self.history.lock();
        let mut scene = self.scene.write();
        history.undo(&mut scene)
    }

    pub fn redo(&self) -> Result<Option<(u64, Op)>> {
        let mut history = self.history.lock();
        let mut scene = self.scene.write();
        history.redo(&mut scene)
    }

    pub fn epoch(&self) -> u64 {
        self.history.lock().epoch()
    }

    /// Cheap clone of the scene for read-only consumers (pipeline engines).
    pub fn scene_snapshot(&self) -> Scene {
        self.scene.read().clone()
    }

    // --- compaction --------------------------------------------------------

    /// Write a new snapshot (scene.bin) and truncate the log. Safe to call
    /// at any time; crash mid-compaction leaves the old snapshot + full log.
    pub fn compact(&self) -> Result<()> {
        let snap = {
            let scene = self.scene.read();
            let epoch = self.history.lock().epoch();
            Snapshot {
                epoch,
                scene: scene.clone(),
            }
        };
        let bytes = postcard::to_allocvec(&snap).context("encode snapshot")?;
        AtomicFile::new(
            self.dir.join(SCENE_FILE).as_std_path(),
            OverwriteBehavior::AllowOverwrite,
        )
        .write(|f| f.write_all(&bytes))
        .context("write scene.bin atomically")?;
        // Log truncation only after snapshot is durably on disk.
        self.history.lock().truncate_log()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Snapshot loading / TOML metadata
// ---------------------------------------------------------------------------

fn load_snapshot(dir: &Utf8Path, creating: bool) -> Result<(Scene, u64)> {
    let scene_path = dir.join(SCENE_FILE);
    if scene_path.exists() {
        let bytes = std::fs::read(scene_path.as_std_path())
            .with_context(|| format!("read {}", scene_path))?;
        let snap: Snapshot = match postcard::from_bytes(&bytes) {
            Ok(snap) => snap,
            Err(current_err) => {
                let legacy = legacy_snapshot::decode(&bytes)
                    .with_context(|| format!("decode legacy {}", scene_path))
                    .with_context(|| format!("decode {}", scene_path))?;
                tracing::warn!(
                    path = %scene_path,
                    error = %current_err,
                    "decoded scene.bin with legacy text style compatibility path"
                );
                legacy
            }
        };
        return Ok((snap.scene, snap.epoch));
    }

    // No snapshot — build one from `project.toml` (or defaults for creation).
    let toml_path = dir.join(PROJECT_TOML);
    let meta = if toml_path.exists() {
        let text = std::fs::read_to_string(toml_path.as_std_path())?;
        toml::from_str::<ProjectTomlFile>(&text).with_context(|| format!("parse {}", toml_path))?
    } else if creating {
        ProjectTomlFile {
            name: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    } else {
        anyhow::bail!("missing project.toml at {}", toml_path);
    };

    let mut scene = Scene::default();
    scene.project.name = meta.name;
    scene.project.created_at = meta.created_at;
    scene.project.updated_at = meta.updated_at;
    Ok((scene, 0))
}

#[derive(Serialize, Deserialize)]
struct ProjectTomlFile {
    name: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

mod legacy_snapshot {
    use anyhow::{Context, Result};
    use indexmap::IndexMap;
    use koharu_core::{
        BlobRef, FontPrediction, ImageData, MaskData, Node, NodeId, NodeKind, Page, PageId,
        ProjectMeta, Scene, TextAlign, TextDirection, TextShaderEffect, TextStrokeStyle, TextStyle,
        Transform,
    };
    use serde::{Deserialize, Serialize};

    pub(super) fn decode(bytes: &[u8]) -> Result<super::Snapshot> {
        match postcard::from_bytes::<Snapshot<LineHeightTextStyle>>(bytes) {
            Ok(snapshot) => return Ok(snapshot.into_current()),
            Err(err) => {
                tracing::debug!(error = %err, "legacy decode without letter spacing failed");
            }
        }

        let snapshot = postcard::from_bytes::<Snapshot<NoLineHeightTextStyle>>(bytes)
            .context("decode legacy text style without line height")?;
        Ok(snapshot.into_current())
    }

    trait IntoCurrentTextStyle {
        fn into_current(self) -> TextStyle;
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(
        rename_all = "camelCase",
        bound(
            serialize = "S: Serialize",
            deserialize = "S: serde::de::DeserializeOwned"
        )
    )]
    struct Snapshot<S> {
        epoch: u64,
        scene: LegacyScene<S>,
    }

    impl<S: IntoCurrentTextStyle> Snapshot<S> {
        fn into_current(self) -> super::Snapshot {
            super::Snapshot {
                epoch: self.epoch,
                scene: self.scene.into_current(),
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(
        rename_all = "camelCase",
        bound(
            serialize = "S: Serialize",
            deserialize = "S: serde::de::DeserializeOwned"
        )
    )]
    struct LegacyScene<S> {
        project: ProjectMeta,
        pages: IndexMap<PageId, LegacyPage<S>>,
    }

    impl<S: IntoCurrentTextStyle> LegacyScene<S> {
        fn into_current(self) -> Scene {
            Scene {
                project: self.project,
                pages: self
                    .pages
                    .into_iter()
                    .map(|(id, page)| (id, page.into_current()))
                    .collect(),
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(
        rename_all = "camelCase",
        bound(
            serialize = "S: Serialize",
            deserialize = "S: serde::de::DeserializeOwned"
        )
    )]
    struct LegacyPage<S> {
        id: PageId,
        name: String,
        width: u32,
        height: u32,
        nodes: IndexMap<NodeId, LegacyNode<S>>,
    }

    impl<S: IntoCurrentTextStyle> LegacyPage<S> {
        fn into_current(self) -> Page {
            Page {
                id: self.id,
                name: self.name,
                width: self.width,
                height: self.height,
                nodes: self
                    .nodes
                    .into_iter()
                    .map(|(id, node)| (id, node.into_current()))
                    .collect(),
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(
        rename_all = "camelCase",
        bound(
            serialize = "S: Serialize",
            deserialize = "S: serde::de::DeserializeOwned"
        )
    )]
    struct LegacyNode<S> {
        id: NodeId,
        #[serde(default)]
        transform: Transform,
        visible: bool,
        kind: LegacyNodeKind<S>,
    }

    impl<S: IntoCurrentTextStyle> LegacyNode<S> {
        fn into_current(self) -> Node {
            Node {
                id: self.id,
                transform: self.transform,
                visible: self.visible,
                kind: self.kind.into_current(),
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(
        rename_all = "camelCase",
        bound(
            serialize = "S: Serialize",
            deserialize = "S: serde::de::DeserializeOwned"
        )
    )]
    enum LegacyNodeKind<S> {
        Image(ImageData),
        Text(LegacyTextData<S>),
        Mask(MaskData),
    }

    impl<S: IntoCurrentTextStyle> LegacyNodeKind<S> {
        fn into_current(self) -> NodeKind {
            match self {
                Self::Image(data) => NodeKind::Image(data),
                Self::Text(data) => NodeKind::Text(data.into_current()),
                Self::Mask(data) => NodeKind::Mask(data),
            }
        }
    }

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    #[serde(
        rename_all = "camelCase",
        bound(
            serialize = "S: Serialize",
            deserialize = "S: serde::de::DeserializeOwned"
        )
    )]
    struct LegacyTextData<S> {
        #[serde(default)]
        confidence: f32,
        #[serde(default)]
        source_lang: Option<String>,
        #[serde(default)]
        source_direction: Option<TextDirection>,
        #[serde(default)]
        rendered_direction: Option<TextDirection>,
        #[serde(default)]
        line_polygons: Option<Vec<[[f32; 2]; 4]>>,
        #[serde(default)]
        rotation_deg: Option<f32>,
        #[serde(default)]
        detected_font_size_px: Option<f32>,
        #[serde(default)]
        detector: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        translation: Option<String>,
        #[serde(default)]
        style: Option<S>,
        #[serde(default)]
        font_prediction: Option<FontPrediction>,
        #[serde(default)]
        sprite: Option<BlobRef>,
        #[serde(default)]
        sprite_transform: Option<Transform>,
        #[serde(default)]
        lock_layout_box: bool,
    }

    impl<S: IntoCurrentTextStyle> LegacyTextData<S> {
        fn into_current(self) -> koharu_core::TextData {
            koharu_core::TextData {
                confidence: self.confidence,
                source_lang: self.source_lang,
                source_direction: self.source_direction,
                rendered_direction: self.rendered_direction,
                line_polygons: self.line_polygons,
                rotation_deg: self.rotation_deg,
                detected_font_size_px: self.detected_font_size_px,
                detector: self.detector,
                text: self.text,
                translation: self.translation,
                style: self.style.map(IntoCurrentTextStyle::into_current),
                font_prediction: self.font_prediction,
                sprite: self.sprite,
                sprite_transform: self.sprite_transform,
                lock_layout_box: self.lock_layout_box,
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LineHeightTextStyle {
        font_families: Vec<String>,
        font_size: Option<f32>,
        color: [u8; 4],
        effect: Option<TextShaderEffect>,
        stroke: Option<TextStrokeStyle>,
        #[serde(default)]
        text_align: Option<TextAlign>,
        #[serde(default)]
        line_height: Option<f32>,
    }

    impl IntoCurrentTextStyle for LineHeightTextStyle {
        fn into_current(self) -> TextStyle {
            TextStyle {
                font_families: self.font_families,
                font_size: self.font_size,
                color: self.color,
                effect: self.effect,
                stroke: self.stroke,
                text_align: self.text_align,
                line_height: self.line_height,
                letter_spacing: None,
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct NoLineHeightTextStyle {
        font_families: Vec<String>,
        font_size: Option<f32>,
        color: [u8; 4],
        effect: Option<TextShaderEffect>,
        stroke: Option<TextStrokeStyle>,
        #[serde(default)]
        text_align: Option<TextAlign>,
    }

    impl IntoCurrentTextStyle for NoLineHeightTextStyle {
        fn into_current(self) -> TextStyle {
            TextStyle {
                font_families: self.font_families,
                font_size: self.font_size,
                color: self.color,
                effect: self.effect,
                stroke: self.stroke,
                text_align: self.text_align,
                line_height: None,
                letter_spacing: None,
            }
        }
    }

    #[cfg(test)]
    pub(super) fn encode_test_snapshot_without_line_height(
        page_id: PageId,
        node_id: NodeId,
    ) -> Vec<u8> {
        encode_test_snapshot(
            page_id,
            node_id,
            NoLineHeightTextStyle {
                font_families: vec!["Arial".to_string()],
                font_size: Some(20.0),
                color: [0, 0, 0, 255],
                effect: Some(TextShaderEffect {
                    italic: true,
                    bold: true,
                }),
                stroke: None,
                text_align: Some(TextAlign::Center),
            },
        )
    }

    #[cfg(test)]
    pub(super) fn encode_test_snapshot_without_letter_spacing(
        page_id: PageId,
        node_id: NodeId,
    ) -> Vec<u8> {
        encode_test_snapshot(
            page_id,
            node_id,
            LineHeightTextStyle {
                font_families: vec!["Arial".to_string()],
                font_size: Some(20.0),
                color: [0, 0, 0, 255],
                effect: Some(TextShaderEffect {
                    italic: true,
                    bold: true,
                }),
                stroke: None,
                text_align: Some(TextAlign::Center),
                line_height: Some(1.2),
            },
        )
    }

    #[cfg(test)]
    fn encode_test_snapshot<S>(page_id: PageId, node_id: NodeId, style: S) -> Vec<u8>
    where
        S: IntoCurrentTextStyle + Serialize,
    {
        let mut nodes = IndexMap::new();
        nodes.insert(
            node_id,
            LegacyNode {
                id: node_id,
                transform: Transform {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 40.0,
                    rotation_deg: 0.0,
                },
                visible: true,
                kind: LegacyNodeKind::Text(LegacyTextData {
                    style: Some(style),
                    font_prediction: Some(FontPrediction::default()),
                    ..Default::default()
                }),
            },
        );

        let mut pages = IndexMap::new();
        pages.insert(
            page_id,
            LegacyPage {
                id: page_id,
                name: "legacy".to_string(),
                width: 800,
                height: 600,
                nodes,
            },
        );

        let snapshot = Snapshot {
            epoch: 7,
            scene: LegacyScene {
                project: ProjectMeta::default(),
                pages,
            },
        };
        postcard::to_allocvec(&snapshot).expect("encode legacy snapshot")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use koharu_core::{
        Node, NodeId, NodeKind, Op, Page, PageId, TextData, TextShaderEffect, TextStyle, Transform,
    };
    use tempfile::tempdir;

    fn tmp_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path.join("proj.khrproj"))
    }

    #[test]
    fn create_apply_close_reopen_preserves_scene() {
        let (_tmp, path) = tmp_dir();
        let page_id: PageId;
        {
            let session = ProjectSession::create(&path, "test").unwrap();
            let page = Page::new("p1", 800, 600);
            page_id = page.id;
            session
                .apply(Op::AddPage { page, at: 0 })
                .expect("apply AddPage");
            session.compact().unwrap();
            // Session drops, lock released.
        }
        let session = ProjectSession::open(&path).unwrap();
        assert_eq!(session.scene.read().pages.len(), 1);
        assert!(session.scene.read().pages.contains_key(&page_id));
    }

    #[test]
    fn reopen_preserves_text_style_effects_in_scene_bin() {
        let (_tmp, path) = tmp_dir();
        let page_id: PageId;
        let node_id: NodeId;
        {
            let session = ProjectSession::create(&path, "styled").unwrap();
            let page = Page::new("p1", 800, 600);
            page_id = page.id;
            session
                .apply(Op::AddPage { page, at: 0 })
                .expect("apply AddPage");

            node_id = NodeId::new();
            let mut scene = session.scene.write();
            let page = scene.pages.get_mut(&page_id).expect("page");
            page.nodes.insert(
                node_id,
                Node {
                    id: node_id,
                    transform: Transform {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 40.0,
                        rotation_deg: 0.0,
                    },
                    visible: true,
                    kind: NodeKind::Text(TextData {
                        style: Some(TextStyle {
                            font_families: vec!["Arial".to_string()],
                            font_size: Some(20.0),
                            color: [0, 0, 0, 255],
                            effect: Some(TextShaderEffect {
                                italic: true,
                                bold: true,
                            }),
                            stroke: None,
                            text_align: None,
                            line_height: None,
                            letter_spacing: None,
                        }),
                        ..Default::default()
                    }),
                },
            );
            drop(scene);
            session.compact().unwrap();
        }

        let session = ProjectSession::open(&path).unwrap();
        let scene = session.scene.read();
        let page = scene.pages.get(&page_id).expect("page");
        let node = page.nodes.get(&node_id).expect("node");
        let NodeKind::Text(text) = &node.kind else {
            panic!("expected text node");
        };
        let effect = text
            .style
            .as_ref()
            .and_then(|style| style.effect)
            .expect("effect");
        assert!(effect.italic);
        assert!(effect.bold);
    }

    #[test]
    fn open_decodes_legacy_text_style_without_line_height() {
        let (_tmp, path) = tmp_dir();
        std::fs::create_dir_all(path.as_std_path()).unwrap();
        let page_id = PageId::new();
        let node_id = NodeId::new();
        let bytes = legacy_snapshot::encode_test_snapshot_without_line_height(page_id, node_id);
        std::fs::write(path.join(SCENE_FILE).as_std_path(), bytes).unwrap();

        let session = ProjectSession::open(&path).unwrap();
        let scene = session.scene.read();
        let page = scene.pages.get(&page_id).expect("page");
        let node = page.nodes.get(&node_id).expect("node");
        let NodeKind::Text(text) = &node.kind else {
            panic!("expected text node");
        };
        let style = text.style.as_ref().expect("style");
        assert_eq!(style.text_align, Some(koharu_core::TextAlign::Center));
        assert_eq!(style.line_height, None);
        assert_eq!(style.letter_spacing, None);
        assert!(text.font_prediction.is_some());
    }

    #[test]
    fn open_decodes_legacy_text_style_without_letter_spacing() {
        let (_tmp, path) = tmp_dir();
        std::fs::create_dir_all(path.as_std_path()).unwrap();
        let page_id = PageId::new();
        let node_id = NodeId::new();
        let bytes = legacy_snapshot::encode_test_snapshot_without_letter_spacing(page_id, node_id);
        std::fs::write(path.join(SCENE_FILE).as_std_path(), bytes).unwrap();

        let session = ProjectSession::open(&path).unwrap();
        let scene = session.scene.read();
        let page = scene.pages.get(&page_id).expect("page");
        let node = page.nodes.get(&node_id).expect("node");
        let NodeKind::Text(text) = &node.kind else {
            panic!("expected text node");
        };
        let style = text.style.as_ref().expect("style");
        assert_eq!(style.text_align, Some(koharu_core::TextAlign::Center));
        assert_eq!(style.line_height, Some(1.2));
        assert_eq!(style.letter_spacing, None);
        assert!(text.font_prediction.is_some());
    }

    #[test]
    fn exclusive_lock_prevents_second_open() {
        let (_tmp, path) = tmp_dir();
        let a = ProjectSession::create(&path, "test").unwrap();
        let err = ProjectSession::open(&path)
            .err()
            .expect("second open must fail");
        assert!(err.to_string().contains("already open"));
        drop(a);
    }
}
