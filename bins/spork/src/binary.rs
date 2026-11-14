//! Binary and image diffs (docs/specs/M08 §3): the panel shown instead of the line list when a
//! file's change is binary — side-by-side / swipe / onion-skin / highlight-pixel modes for images,
//! sizes plus "Save as…" / "Open with default app" for any other binary, and "LFS object not
//! downloaded · Fetch" for LFS pointers. One [`BinaryDiff`] entity per diff view (working copy,
//! commit details, search results, file history); every git / disk read and the image decoding run
//! in one background task per load.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::DiffFile;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use spork_git::Cancellable;
use spork_git::lfs::{LfsPointer, lfs_fetch, lfs_object_content, parse_lfs_pointer};
use spork_patch::imagediff::{DecodedImage, decode_image, is_image_path, pixel_diff};

use crate::RepoView;
use crate::lfs::lfs_format_size;

/// Sides larger than this skip the image preview (and are not read into memory until saved),
/// in the spirit of the spec's large-diff summary (M08 §2).
pub const MAX_PREVIEW_BYTES: u64 = 10 * 1024 * 1024;
/// Decoded images beyond this many pixels (or wider / taller than [`MAX_PREVIEW_SIDE`]) are not
/// uploaded to the GPU; the plain binary panel shows instead.
const MAX_PREVIEW_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_PREVIEW_SIDE: u32 = 16_384;
/// The longest LFS pointer file worth parsing (real pointers are ~130 bytes).
const MAX_POINTER_BYTES: usize = 1024;
/// The colour of a differing pixel in the highlight mode.
const HIGHLIGHT_RGBA: [u8; 4] = [255, 0, 0, 255];

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested below)
// ---------------------------------------------------------------------------

/// How a single-file patch from `git diff` / `git show` should be displayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchKind {
    /// Ordinary text hunks: the line list.
    Text,
    /// A binary change (or an LFS pointer change): the binary panel. The oids come from the
    /// `index <old>..<new>` header, as git printed them (abbreviated unless `--full-index`);
    /// `None` for an absent side (all-zero oid) or a patch without an index line.
    Binary {
        old_oid: Option<String>,
        new_oid: Option<String>,
    },
}

/// The two blob oids of an `index <old>..<new>[ <mode>]` header line; an all-zero oid is an
/// absent side (added / deleted file). `None` when the line isn't an index line.
pub fn index_oids(line: &str) -> Option<(Option<String>, Option<String>)> {
    let range = line.strip_prefix("index ")?.split_whitespace().next()?;
    let (old, new) = range.split_once("..")?;
    let oid = |s: &str| -> Option<Option<String>> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some((!s.bytes().all(|b| b == b'0')).then(|| s.to_owned()))
    };
    Some((oid(old)?, oid(new)?))
}

/// Classifies the first file of a patch: binary when git says so (`Binary files … differ`,
/// `GIT binary patch`), or when the hunks are an LFS pointer's lines (git diffs the cleaned
/// pointer text of LFS-tracked files, which is not what the user wants to see).
pub fn classify_patch(patch: &str) -> PatchKind {
    let (mut oids, mut binary, mut seen_file) = ((None, None), false, false);
    let (mut lfs_version, mut lfs_oid) = (false, false);
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            if seen_file {
                break; // only the first file
            }
            seen_file = true;
            continue;
        }
        if let Some(found) = index_oids(line) {
            oids = found;
        } else if (line.starts_with("Binary files ") && line.ends_with(" differ"))
            || line == "GIT binary patch"
        {
            binary = true;
        } else if let Some(body) = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix('-'))
            .or_else(|| line.strip_prefix(' '))
        {
            lfs_version |= body == "version https://git-lfs.github.com/spec/v1";
            lfs_oid |= body.starts_with("oid sha256:");
        }
    }
    if binary || (lfs_version && lfs_oid) {
        PatchKind::Binary {
            old_oid: oids.0,
            new_oid: oids.1,
        }
    } else {
        PatchKind::Text
    }
}

/// Where one side's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobSide {
    /// The file doesn't exist on this side (added / deleted).
    Absent,
    /// A blob in the object database (`git cat-file blob <oid>`).
    Blob(String),
    /// The working-tree file at this repository-relative path.
    WorkTree(String),
}

/// One binary file change to show: its path and the two sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTarget {
    pub path: String,
    pub old: BlobSide,
    pub new: BlobSide,
}

impl BinaryTarget {
    /// The target for a single-file patch, or `None` for a text change. With `worktree_new` the
    /// new side is the working-tree file (unstaged and untracked changes), else the index blob.
    pub fn from_patch(patch: &str, path: &str, worktree_new: bool) -> Option<Self> {
        let PatchKind::Binary { old_oid, new_oid } = classify_patch(patch) else {
            return None;
        };
        Some(Self::from_oids(path, old_oid, new_oid, worktree_new))
    }

    /// The target for one file of a multi-file diff: binary per git, or an LFS-tracked path.
    /// The oids come from the file's `index` extended header.
    pub fn from_diff_file(file: &DiffFile, lfs: bool) -> Option<Self> {
        if !file.is_binary() && !lfs {
            return None;
        }
        let (old, new) = file
            .extended_headers()
            .iter()
            .find_map(|h| index_oids(h))
            .unwrap_or((None, None));
        Some(Self::from_oids(file.path(), old, new, false))
    }

    fn from_oids(path: &str, old: Option<String>, new: Option<String>, worktree_new: bool) -> Self {
        Self {
            path: path.to_owned(),
            old: old.map_or(BlobSide::Absent, BlobSide::Blob),
            new: match (new, worktree_new) {
                // the working tree's oid isn't in the object database; read the file itself
                (Some(_), true) => BlobSide::WorkTree(path.to_owned()),
                (Some(oid), false) => BlobSide::Blob(oid),
                (None, _) => BlobSide::Absent,
            },
        }
    }
}

/// The image modes (M08 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMode {
    SideBySide,
    Swipe,
    Onion,
    Highlight,
}

impl ImageMode {
    pub const ALL: [ImageMode; 4] = [
        ImageMode::SideBySide,
        ImageMode::Swipe,
        ImageMode::Onion,
        ImageMode::Highlight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ImageMode::SideBySide => "Side-by-side",
            ImageMode::Swipe => "Swipe",
            ImageMode::Onion => "Onion skin",
            ImageMode::Highlight => "Highlight pixels",
        }
    }
}

/// The modes that make sense for the decoded sides: comparing modes need both images; an added
/// or deleted image only shows side by side (the one image, centered).
pub fn available_modes(has_old: bool, has_new: bool) -> Vec<ImageMode> {
    match (has_old, has_new) {
        (true, true) => ImageMode::ALL.to_vec(),
        (false, false) => Vec::new(),
        _ => vec![ImageMode::SideBySide],
    }
}

/// The chosen mode if it's available, else side-by-side.
pub fn effective_mode(chosen: ImageMode, has_old: bool, has_new: bool) -> ImageMode {
    if available_modes(has_old, has_new).contains(&chosen) {
        chosen
    } else {
        ImageMode::SideBySide
    }
}

/// The caption under an image: `"640×480 · 12 KB"` (sizes humanized as in the LFS table).
pub fn size_caption(width: u32, height: u32, bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => format!("{width}×{height} · {}", lfs_format_size(bytes)),
        None => format!("{width}×{height}"),
    }
}

/// The size line of the plain binary panel.
pub fn sizes_line(old: Option<u64>, new: Option<u64>) -> String {
    match (old, new) {
        (Some(old), Some(new)) => {
            format!("{} → {}", lfs_format_size(old), lfs_format_size(new))
        }
        (None, Some(new)) => format!("Added · {}", lfs_format_size(new)),
        (Some(old), None) => format!("Deleted · {}", lfs_format_size(old)),
        (None, None) => "No content on either side".to_owned(),
    }
}

/// The new image with every pixel that differs from the old one painted red (tolerance 0), at
/// the common size of [`pixel_diff`] (the smaller image padded with transparent black), and the
/// number of differing pixels.
pub fn highlight_overlay(old: &DecodedImage, new: &DecodedImage) -> (DecodedImage, u64) {
    let diff = pixel_diff(old, new, 0);
    let (width, height) = (old.width.max(new.width), old.height.max(new.height));
    let mut rgba = vec![0u8; diff.mask.len() * 4];
    for (i, differs) in diff.mask.iter().enumerate() {
        let (x, y) = (i % width as usize, i / width as usize);
        let px = if *differs {
            HIGHLIGHT_RGBA
        } else if x < new.width as usize && y < new.height as usize {
            let j = (y * new.width as usize + x) * 4;
            [
                new.rgba[j],
                new.rgba[j + 1],
                new.rgba[j + 2],
                new.rgba[j + 3],
            ]
        } else {
            [0, 0, 0, 0]
        };
        rgba[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    (
        DecodedImage {
            width,
            height,
            rgba,
        },
        diff.differing,
    )
}

/// Scales `width × height` to fit inside `max_w × max_h`, never enlarging.
pub fn fit_size(width: f32, height: f32, max_w: f32, max_h: f32) -> (f32, f32) {
    if width <= 0. || height <= 0. {
        return (0., 0.);
    }
    let scale = (max_w.max(0.) / width).min(max_h.max(0.) / height).min(1.);
    (width * scale, height * scale)
}

/// The swipe divider position for a pointer at `x` over a box starting at `left`, `width` wide.
pub fn swipe_fraction(x: f32, left: f32, width: f32) -> f32 {
    if width <= 0. {
        return 0.5;
    }
    ((x - left) / width).clamp(0., 1.)
}

// ---------------------------------------------------------------------------
// Background loading
// ---------------------------------------------------------------------------

/// What one side turned out to be.
#[derive(Debug, Clone, Default)]
struct SideInfo {
    present: bool,
    /// Size of the real content (the LFS object's size for a pointer).
    size: Option<u64>,
    /// Larger than [`MAX_PREVIEW_BYTES`]: not read, no preview.
    too_large: bool,
    /// The content is an LFS pointer whose object isn't downloaded.
    lfs_missing: Option<LfsPointer>,
    /// The content was read from the LFS object store.
    lfs: bool,
    error: Option<String>,
}

/// A decoded image ready for the GPU.
#[derive(Clone)]
struct Picture {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
}

struct Pictures {
    old: Option<Picture>,
    new: Option<Picture>,
    /// Both sides decoded: the red-overlay image and the differing pixel count.
    highlight: Option<(Picture, u64)>,
}

struct Payload {
    target: BinaryTarget,
    old: SideInfo,
    new: SideInfo,
    /// `None`: not an image (or skipped: too large / LFS object missing); `Some(Err)`: decoding
    /// failed, the plain panel shows with the error.
    pictures: Option<Result<Pictures, String>>,
    /// The working-tree file exists ("Open with default app").
    worktree_file: Option<PathBuf>,
}

fn git_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A side's size, and its bytes when no larger than the read limit.
type RawSide = (u64, Option<Vec<u8>>);

/// The raw bytes of one side, up to `limit` (larger sides report their size only).
fn raw_side(root: &Path, side: &BlobSide, limit: u64) -> Result<Option<RawSide>, String> {
    match side {
        BlobSide::Absent => Ok(None),
        BlobSide::Blob(oid) => {
            let out = spork_git::run(root, &["cat-file", "-s", oid]).map_err(git_err)?;
            let size = String::from_utf8_lossy(&out)
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("git cat-file -s {oid}: {e}"))?;
            if size > limit {
                return Ok(Some((size, None)));
            }
            let bytes = spork_git::run(root, &["cat-file", "blob", oid]).map_err(git_err)?;
            Ok(Some((size, Some(bytes))))
        }
        BlobSide::WorkTree(path) => {
            let file = root.join(path);
            let meta = match std::fs::metadata(&file) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(format!("{}: {e}", file.display())),
            };
            if meta.len() > limit {
                return Ok(Some((meta.len(), None)));
            }
            let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            Ok(Some((meta.len(), Some(bytes))))
        }
    }
}

/// One side's info and (when small enough) its real content, following an LFS pointer into the
/// local object store.
fn load_side(root: &Path, side: &BlobSide, limit: u64) -> (SideInfo, Option<Vec<u8>>) {
    let (size, bytes) = match raw_side(root, side, limit) {
        Ok(Some(found)) => found,
        Ok(None) => return (SideInfo::default(), None),
        Err(error) => {
            let info = SideInfo {
                present: true,
                error: Some(error),
                ..SideInfo::default()
            };
            return (info, None);
        }
    };
    let mut info = SideInfo {
        present: true,
        size: Some(size),
        too_large: bytes.is_none(),
        ..SideInfo::default()
    };
    let pointer = bytes
        .as_deref()
        .filter(|b| b.len() <= MAX_POINTER_BYTES)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(parse_lfs_pointer);
    let Some(pointer) = pointer else {
        return (info, bytes);
    };
    info.size = Some(pointer.size);
    info.lfs = true;
    if pointer.size > limit {
        info.too_large = true;
        return (info, None);
    }
    match lfs_object_content(root, &pointer.oid) {
        Some(content) => (info, Some(content)),
        None => {
            info.lfs_missing = Some(pointer);
            (info, None)
        }
    }
}

/// The full content of one side, for "Save as…".
fn side_bytes(root: &Path, side: &BlobSide) -> Result<Vec<u8>, String> {
    let (info, bytes) = load_side(root, side, u64::MAX);
    if let Some(error) = info.error {
        return Err(error);
    }
    if info.lfs_missing.is_some() {
        return Err("The LFS object is not downloaded; fetch it first.".to_owned());
    }
    bytes.ok_or_else(|| "This side has no content.".to_owned())
}

/// RGBA → the BGRA frame gpui's image element paints.
fn picture(image: DecodedImage) -> Result<Picture, String> {
    let (width, height) = (image.width, image.height);
    if width == 0
        || height == 0
        || width > MAX_PREVIEW_SIDE
        || height > MAX_PREVIEW_SIDE
        || u64::from(width) * u64::from(height) > MAX_PREVIEW_PIXELS
    {
        return Err(format!("{width}×{height} is too large to preview"));
    }
    let mut bgra = image.rgba;
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = ::image::RgbaImage::from_raw(width, height, bgra)
        .ok_or_else(|| "decoded image has the wrong buffer size".to_owned())?;
    Ok(Picture {
        image: Arc::new(RenderImage::new(vec![::image::Frame::new(buffer)])),
        width,
        height,
    })
}

/// Decodes the present sides; any present side that fails fails the preview.
fn decode_pictures(old: Option<&[u8]>, new: Option<&[u8]>) -> Result<Pictures, String> {
    let decode = |bytes: Option<&[u8]>| -> Result<Option<DecodedImage>, String> {
        bytes
            .map(decode_image)
            .transpose()
            .map_err(|e| e.to_string())
    };
    let (old, new) = (decode(old)?, decode(new)?);
    let highlight = match (&old, &new) {
        (Some(o), Some(n)) => {
            let (overlay, differing) = highlight_overlay(o, n);
            Some((picture(overlay)?, differing))
        }
        _ => None,
    };
    Ok(Pictures {
        old: old.map(picture).transpose()?,
        new: new.map(picture).transpose()?,
        highlight,
    })
}

/// Everything the panel needs, in one background pass: both sides' bytes, then the decode.
fn load_payload(root: &Path, target: BinaryTarget) -> Payload {
    let (old, old_bytes) = load_side(root, &target.old, MAX_PREVIEW_BYTES);
    let (new, new_bytes) = load_side(root, &target.new, MAX_PREVIEW_BYTES);
    let previewable = |info: &SideInfo, bytes: &Option<Vec<u8>>| {
        !info.present || (bytes.is_some() && info.error.is_none())
    };
    let pictures = (is_image_path(&target.path)
        && previewable(&old, &old_bytes)
        && previewable(&new, &new_bytes)
        && (old_bytes.is_some() || new_bytes.is_some()))
    .then(|| decode_pictures(old_bytes.as_deref(), new_bytes.as_deref()));
    let worktree_file = Some(root.join(&target.path)).filter(|p| p.is_file());
    Payload {
        target,
        old,
        new,
        pictures,
        worktree_file,
    }
}

/// The remote an LFS fetch uses: `origin` when it exists, else the first remote.
fn default_remote(root: &Path) -> Result<String, String> {
    let out = spork_git::run(root, &["remote"]).map_err(git_err)?;
    let text = String::from_utf8_lossy(&out);
    let remotes: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();
    remotes
        .iter()
        .find(|r| **r == "origin")
        .or_else(|| remotes.first())
        .map(|r| (*r).to_owned())
        .ok_or_else(|| "This repository has no remote to fetch LFS objects from.".to_owned())
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

/// Emitted when the user leaves the panel ("Back to diff" in the multi-file views).
pub enum BinaryDiffEvent {
    Closed,
}

/// The shared binary / image diff panel (M08 §3). The image mode is kept per panel instance
/// across files (not yet persisted per user, unlike the §2 header toggles).
pub struct BinaryDiff {
    root: Option<PathBuf>,
    target: Option<BinaryTarget>,
    payload: Option<Payload>,
    loading: bool,
    /// Shows "Back to diff" (the commit-details and search views open the panel from a file
    /// header and need a way back to the whole commit's diff).
    closable: bool,
    mode: ImageMode,
    /// Swipe divider position, 0 (all new) ..= 1 (all old).
    swipe: f32,
    dragging: bool,
    onion: Entity<SliderState>,
    fetching: bool,
    fetch_cancel: Cancellable,
    /// The body's bounds from the last paint, to fit the images to the pane.
    body_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The swipe box's bounds from the last paint, for the drag math.
    swipe_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    _onion_events: Subscription,
    _load: Option<Task<()>>,
    _op: Option<Task<()>>,
}

impl EventEmitter<BinaryDiffEvent> for BinaryDiff {}

impl BinaryDiff {
    pub fn new(closable: bool, cx: &mut Context<Self>) -> Self {
        let onion = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value(50.)
        });
        let _onion_events = cx.subscribe(&onion, |_, _, _: &SliderEvent, cx| cx.notify());
        Self {
            root: None,
            target: None,
            payload: None,
            loading: false,
            closable,
            mode: ImageMode::SideBySide,
            swipe: 0.5,
            dragging: false,
            onion,
            fetching: false,
            fetch_cancel: Cancellable::default(),
            body_bounds: Rc::new(Cell::new(None)),
            swipe_bounds: Rc::new(Cell::new(None)),
            _onion_events,
            _load: None,
            _op: None,
        }
    }

    /// Whether a binary file is open (the view shows the panel instead of its line list).
    pub fn is_active(&self) -> bool {
        self.target.is_some()
    }

    /// The open file's path, if any.
    pub fn path(&self) -> Option<&str> {
        self.target.as_ref().map(|t| t.path.as_str())
    }

    /// Opens `target` (or closes the panel with `None`) and loads both sides in the background.
    /// Reloading the same file keeps the current content on screen until the new one is in.
    pub fn load(&mut self, root: PathBuf, target: Option<BinaryTarget>, cx: &mut Context<Self>) {
        let Some(target) = target else {
            self.clear(cx);
            return;
        };
        let same_file = self.path() == Some(target.path.as_str());
        if !same_file {
            self.drop_payload(cx);
            self.swipe = 0.5;
        }
        self.root = Some(root.clone());
        self.target = Some(target.clone());
        self.loading = true;
        self._load = Some(cx.spawn(async move |this, cx| {
            let payload = cx
                .background_spawn(async move { load_payload(&root, target) })
                .await;
            this.update(cx, |this, cx| {
                // a stale completion for a file the panel no longer shows
                if this.target.as_ref() != Some(&payload.target) {
                    return;
                }
                this.drop_payload(cx);
                this.payload = Some(payload);
                this.loading = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Closes the panel.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.fetch_cancel.kill();
        self.fetching = false;
        self._load = None;
        self._op = None;
        self.target = None;
        self.loading = false;
        self.drop_payload(cx);
        cx.notify();
    }

    /// Frees the GPU textures of the shown images.
    fn drop_payload(&mut self, cx: &mut Context<Self>) {
        let Some(payload) = self.payload.take() else {
            return;
        };
        if let Some(Ok(pictures)) = payload.pictures {
            let images = [pictures.old, pictures.new, pictures.highlight.map(|h| h.0)];
            for picture in images.into_iter().flatten() {
                cx.drop_image(picture.image, None);
            }
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if let (Some(root), Some(target)) = (self.root.clone(), self.target.clone()) {
            self.load(root, Some(target), cx);
        }
    }

    fn set_mode(&mut self, mode: ImageMode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.clear(cx);
        cx.emit(BinaryDiffEvent::Closed);
    }

    fn drag_swipe(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(bounds) = self.swipe_bounds.get() else {
            return;
        };
        self.swipe = swipe_fraction(
            f32::from(x),
            f32::from(bounds.origin.x),
            f32::from(bounds.size.width),
        );
        cx.notify();
    }

    /// "Save as…": the OS save dialog, then the side's full bytes (git blob, working-tree file or
    /// LFS object) written there in the background.
    fn save_as(&mut self, side: BlobSide, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(target)) = (self.root.clone(), self.target.as_ref()) else {
            return;
        };
        let rel = Path::new(&target.path);
        let dir = rel
            .parent()
            .map(|p| root.join(p))
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| root.clone());
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        self._op = Some(cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(dest))) = picked.await else {
                return;
            };
            let shown = dest.display().to_string();
            let result = cx
                .background_spawn(async move {
                    let bytes = side_bytes(&root, &side)?;
                    std::fs::write(&dest, bytes).map_err(|e| format!("{}: {e}", dest.display()))
                })
                .await;
            cx.update(|window, cx| match result {
                Ok(()) => {
                    window.push_notification(Notification::success(format!("Saved {shown}")), cx)
                }
                Err(e) => window.push_notification(Notification::error(e), cx),
            })
            .ok();
        }));
    }

    /// "Fetch": `git lfs fetch <origin or first remote>`, then the sides are read again.
    fn fetch_lfs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.fetch_cancel.kill();
        let cancel = Cancellable::default();
        self.fetch_cancel = cancel.clone();
        self.fetching = true;
        cx.notify();
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let remote = default_remote(&root)?;
                    let progress = crate::lfs::lfs_progress_file();
                    let fetched = lfs_fetch(&root, &remote, &progress, &cancel).map_err(git_err);
                    let _ = std::fs::remove_file(&progress);
                    fetched.map(|()| remote)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.fetching = false;
                match result {
                    Ok(remote) => {
                        window.push_notification(
                            Notification::success(format!("Fetched LFS objects from {remote}")),
                            cx,
                        );
                        this.reload(cx);
                    }
                    Err(e) => window.push_notification(Notification::error(e), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

/// Records an element's painted bounds; a change triggers one more frame so the layout that
/// depends on them catches up.
fn bounds_probe(store: Rc<Cell<Option<Bounds<Pixels>>>>) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            if store.get() != Some(bounds) {
                store.set(Some(bounds));
                window.on_next_frame(|window, _| window.refresh());
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn picture_element(picture: &Picture, width: f32, height: f32) -> Img {
    img(ImageSource::Render(picture.image.clone()))
        .w(px(width))
        .h(px(height))
        .object_fit(ObjectFit::Fill)
}

impl BinaryDiff {
    fn toolbar(
        &self,
        payload: Option<&Payload>,
        modes: &[ImageMode],
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let mode = self.mode;
        let mut bar = h_flex()
            .flex_none()
            .h(px(36.))
            .px_3()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(border)
            .text_xs()
            .text_color(muted);
        let left = h_flex()
            .gap_2()
            .min_w_0()
            .when(self.closable, |h| {
                h.child(
                    Button::new("binary-back")
                        .ghost()
                        .xsmall()
                        .icon(gpui_kit::assets::IconName::ArrowLeft)
                        .label("Back to diff")
                        .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(self.path().unwrap_or_default().to_owned()),
            );
        bar = bar.child(left);
        // a reload of the shown file (e.g. after a working-tree change) keeps the content up
        let mut right = h_flex()
            .gap_1()
            .items_center()
            .when(self.loading && payload.is_some(), |h| {
                h.child(Spinner::new().xsmall())
            });
        if modes.len() > 1 {
            for (ix, m) in modes.iter().copied().enumerate() {
                right = right.child(
                    Button::new(SharedString::from(format!("binary-mode-{ix}")))
                        .xsmall()
                        .ghost()
                        .label(m.label())
                        .toggled(m == mode)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_mode(m, cx))),
                );
            }
        }
        if let Some(payload) = payload {
            right = right.children(self.file_actions(payload, cx));
        }
        bar.child(right)
    }

    /// "Save as…" per present side and "Open with default app".
    fn file_actions(&self, payload: &Payload, cx: &mut Context<Self>) -> Vec<Button> {
        let target = &payload.target;
        let both = payload.old.present && payload.new.present;
        let mut buttons = Vec::new();
        for (id, info, side, label) in [
            ("binary-save-old", &payload.old, &target.old, "Save old as…"),
            ("binary-save-new", &payload.new, &target.new, "Save new as…"),
        ] {
            if !info.present {
                continue;
            }
            let side = side.clone();
            buttons.push(
                Button::new(id)
                    .xsmall()
                    .ghost()
                    .label(if both { label } else { "Save as…" })
                    .disabled(info.lfs_missing.is_some() || info.error.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.save_as(side.clone(), window, cx)
                    })),
            );
        }
        let file = payload.worktree_file.clone();
        buttons.push(
            Button::new("binary-open")
                .xsmall()
                .ghost()
                .label("Open with default app")
                .tooltip("Open the working-tree file")
                .disabled(file.is_none())
                .on_click(move |_, _, cx| {
                    if let Some(file) = &file {
                        cx.open_with_system(file);
                    }
                }),
        );
        buttons
    }

    /// The plain binary panel: sizes, notes, the LFS fetch.
    fn binary_body(&self, payload: &Payload, note: Option<String>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, red) = (theme.muted_foreground, theme.red);
        let title = match (payload.old.present, payload.new.present) {
            (false, true) => "Binary file added",
            (true, false) => "Binary file deleted",
            _ => "Binary file differs",
        };
        let size = |info: &SideInfo| info.present.then_some(info.size).flatten();
        let lfs_missing = payload
            .old
            .lfs_missing
            .as_ref()
            .or(payload.new.lfs_missing.as_ref());
        let too_large = payload.old.too_large || payload.new.too_large;
        let errors: Vec<String> = [&payload.old.error, &payload.new.error]
            .into_iter()
            .flatten()
            .cloned()
            .chain(note)
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .text_color(muted)
            .child(div().text_color(theme.foreground).child(title))
            .child(sizes_line(size(&payload.old), size(&payload.new)))
            .when(payload.old.lfs || payload.new.lfs, |d| {
                d.child(div().text_xs().child("Stored with Git LFS"))
            })
            .when(too_large && is_image_path(&payload.target.path), |d| {
                d.child(div().text_xs().child(format!(
                    "Larger than {} — no image preview.",
                    lfs_format_size(MAX_PREVIEW_BYTES)
                )))
            })
            .children(
                errors
                    .into_iter()
                    .map(|e| div().text_xs().text_color(red).child(e)),
            )
            .when_some(lfs_missing, |d, pointer| {
                d.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(format!(
                            "LFS object not downloaded ({})",
                            lfs_format_size(pointer.size)
                        ))
                        .child(
                            Button::new("binary-lfs-fetch")
                                .small()
                                .primary()
                                .label("Fetch")
                                .loading(self.fetching)
                                .disabled(self.fetching)
                                .tooltip("git lfs fetch from origin (or the first remote)")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.fetch_lfs(window, cx)),
                                ),
                        ),
                )
            })
    }

    fn caption(picture: &Picture, info: &SideInfo, label: &str) -> String {
        format!(
            "{label} · {}",
            size_caption(picture.width, picture.height, info.size)
        )
    }

    fn image_body(
        &self,
        payload: &Payload,
        pictures: &Pictures,
        mode: ImageMode,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let (muted, border, accent) = (theme.muted_foreground, theme.border, theme.primary);
        let bounds = self.body_bounds.get();
        let (avail_w, avail_h) = bounds.map_or((640., 400.), |b| {
            (f32::from(b.size.width), f32::from(b.size.height))
        });
        let body = v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .items_center()
            .justify_center()
            .gap_2()
            .p_3()
            .text_xs()
            .text_color(muted)
            .child(bounds_probe(self.body_bounds.clone()));
        let framed = |el: Img| div().border_1().border_color(border).child(el);
        match (mode, &pictures.old, &pictures.new) {
            (ImageMode::Swipe, Some(old), Some(new)) => {
                let (w, h) = fit_size(
                    old.width.max(new.width) as f32,
                    old.height.max(new.height) as f32,
                    avail_w - 32.,
                    avail_h - 64.,
                );
                let scale = if old.width.max(new.width) > 0 {
                    w / old.width.max(new.width) as f32
                } else {
                    1.
                };
                let split = w * self.swipe;
                let swipe = div()
                    .id("binary-swipe")
                    .relative()
                    .w(px(w))
                    .h(px(h))
                    .border_1()
                    .border_color(border)
                    .cursor_col_resize()
                    .child(bounds_probe(self.swipe_bounds.clone()))
                    // the new image underneath, the old one clipped to the divider's left
                    .child(div().absolute().top_0().left_0().child(picture_element(
                        new,
                        new.width as f32 * scale,
                        new.height as f32 * scale,
                    )))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .w(px(split))
                            .h(px(h))
                            .overflow_hidden()
                            .child(picture_element(
                                old,
                                old.width as f32 * scale,
                                old.height as f32 * scale,
                            )),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px((split - 1.).max(0.)))
                            .w(px(2.))
                            .h(px(h))
                            .bg(accent),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(h / 2. - 10.))
                            .left(px(split - 10.))
                            .size(px(20.))
                            .rounded_full()
                            .border_2()
                            .border_color(accent)
                            .bg(theme.background),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, cx| {
                            this.dragging = true;
                            this.drag_swipe(e.position.x, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                        if this.dragging && e.pressed_button == Some(MouseButton::Left) {
                            this.drag_swipe(e.position.x, cx);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| this.dragging = false),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| this.dragging = false),
                    );
                body.child(swipe).child(format!(
                    "Old ◀ drag ▶ New   ·   {}   ·   {}",
                    size_caption(old.width, old.height, payload.old.size),
                    size_caption(new.width, new.height, payload.new.size)
                ))
            }
            (ImageMode::Onion, Some(old), Some(new)) => {
                let (w, h) = fit_size(
                    old.width.max(new.width) as f32,
                    old.height.max(new.height) as f32,
                    avail_w - 32.,
                    avail_h - 80.,
                );
                let scale = w / old.width.max(new.width).max(1) as f32;
                let value = self.onion.read(cx).value().start();
                let stack = div()
                    .relative()
                    .w(px(w))
                    .h(px(h))
                    .border_1()
                    .border_color(border)
                    .child(div().absolute().top_0().left_0().child(picture_element(
                        old,
                        old.width as f32 * scale,
                        old.height as f32 * scale,
                    )))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .opacity((value / 100.).clamp(0., 1.))
                            .child(picture_element(
                                new,
                                new.width as f32 * scale,
                                new.height as f32 * scale,
                            )),
                    );
                body.child(stack).child(
                    h_flex()
                        .w(px(w.clamp(200., 360.)))
                        .gap_2()
                        .items_center()
                        .child("Old")
                        .child(div().flex_1().child(Slider::new(&self.onion)))
                        .child("New")
                        .child(div().w(px(40.)).child(format!("{value:.0}%"))),
                )
            }
            (ImageMode::Highlight, Some(_), Some(new)) => {
                let Some((overlay, differing)) = &pictures.highlight else {
                    return body.child("No pixel comparison available.");
                };
                let (w, h) = fit_size(
                    overlay.width as f32,
                    overlay.height as f32,
                    avail_w - 32.,
                    avail_h - 64.,
                );
                let summary = match differing {
                    0 => "No pixels differ".to_owned(),
                    1 => "1 pixel differs".to_owned(),
                    n => format!("{n} pixels differ"),
                };
                body.child(framed(picture_element(overlay, w, h)))
                    .child(format!(
                        "{summary}   ·   {}",
                        size_caption(new.width, new.height, payload.new.size)
                    ))
            }
            (_, old, new) => {
                let both = old.is_some() && new.is_some();
                let col_w = if both {
                    (avail_w - 56.) / 2.
                } else {
                    avail_w - 32.
                };
                let column = |picture: &Picture, info: &SideInfo, label: &str| {
                    let (w, h) = fit_size(
                        picture.width as f32,
                        picture.height as f32,
                        col_w,
                        avail_h - 64.,
                    );
                    v_flex()
                        .items_center()
                        .gap_1()
                        .child(framed(picture_element(picture, w, h)))
                        .child(Self::caption(picture, info, label))
                };
                let (old_label, new_label) = if both {
                    ("Old", "New")
                } else {
                    ("Deleted", "Added")
                };
                body.child(
                    h_flex()
                        .gap_6()
                        .items_center()
                        .justify_center()
                        .children(old.as_ref().map(|p| column(p, &payload.old, old_label)))
                        .children(new.as_ref().map(|p| column(p, &payload.new, new_label))),
                )
            }
        }
    }
}

impl Render for BinaryDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        // take the payload out for the render so the helpers can borrow `self` mutably
        let payload = self.payload.take();
        let (modes, mode) = match payload.as_ref().and_then(|p| p.pictures.as_ref()) {
            Some(Ok(pictures)) => {
                let (old, new) = (pictures.old.is_some(), pictures.new.is_some());
                (
                    available_modes(old, new),
                    effective_mode(self.mode, old, new),
                )
            }
            _ => (Vec::new(), self.mode),
        };
        let toolbar = self.toolbar(payload.as_ref(), &modes, cx);
        let body = match &payload {
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .child(Spinner::new())
                .child(
                    if self.target.as_ref().is_some_and(|t| is_image_path(&t.path)) {
                        "Loading image…"
                    } else {
                        "Loading file…"
                    },
                ),
            Some(p) => match &p.pictures {
                Some(Ok(pictures)) => self.image_body(p, pictures, mode, cx),
                Some(Err(e)) => self.binary_body(p, Some(format!("Can't show the image: {e}")), cx),
                None => self.binary_body(p, None, cx),
            },
        };
        self.payload = payload;
        v_flex().size_full().child(toolbar).child(body)
    }
}

// ---------------------------------------------------------------------------
// Wiring into the four diff views
// ---------------------------------------------------------------------------

/// Which diff view a [`BinaryDiff`] panel belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryPane {
    Changes,
    Details,
    Search,
    FileHistory,
}

impl RepoView {
    /// The panel instance of one diff view.
    pub(crate) fn binary_panel(&self, pane: BinaryPane) -> &Entity<BinaryDiff> {
        match pane {
            BinaryPane::Changes => &self.change_binary,
            BinaryPane::Details => &self.details_binary,
            BinaryPane::Search => &self.search_binary,
            BinaryPane::FileHistory => &self.file_history_binary,
        }
    }

    /// Shows `target` in a view's panel (`None` closes it, back to the line list).
    pub(crate) fn show_binary(
        &mut self,
        pane: BinaryPane,
        target: Option<BinaryTarget>,
        cx: &mut Context<Self>,
    ) {
        let root = match (&self.repo, &target) {
            (Some(Ok(repo)), _) => repo.root.clone(),
            // closing needs no repository
            (_, None) => PathBuf::new(),
            _ => return,
        };
        self.binary_panel(pane)
            .clone()
            .update(cx, |panel, cx| panel.load(root, target, cx));
        cx.notify();
    }

    /// Whether a view shows its binary panel instead of the line list.
    pub(crate) fn binary_active(&self, pane: BinaryPane, cx: &App) -> bool {
        self.binary_panel(pane).read(cx).is_active()
    }

    /// The single binary file of a commit's diff, opened straight away (multi-file commits
    /// open it from the file header's "Preview" entry instead).
    pub(crate) fn binary_for_commit(&self, files: &[DiffFile]) -> Option<BinaryTarget> {
        let [file] = files else {
            return None;
        };
        let lfs = crate::lfs::is_lfs_path(file.path().as_ref(), self.lfs_patterns());
        BinaryTarget::from_diff_file(file, lfs)
    }
}

/// The "Preview" entry of a multi-file diff's file header: binary or LFS files open in the
/// view's binary panel. `None` for text files.
pub(crate) fn preview_entry(
    document: &DiffFile,
    patterns: &[String],
    view: &Entity<RepoView>,
    pane: BinaryPane,
    window: &mut Window,
    muted: Hsla,
) -> Option<Stateful<Div>> {
    let lfs = crate::lfs::is_lfs_path(document.path().as_ref(), patterns);
    let target = BinaryTarget::from_diff_file(document, lfs)?;
    let label = if is_image_path(&target.path) {
        "Preview image"
    } else {
        "Preview"
    };
    let click = window.listener_for::<RepoView, ClickEvent>(view, move |this, _, _, cx| {
        this.show_binary(pane, Some(target.clone()), cx)
    });
    Some(
        div()
            .id(SharedString::from(format!("binary-preview-{pane:?}")))
            .on_click(click)
            .cursor_pointer()
            .text_color(muted)
            .child(label),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryTarget, BlobSide, ImageMode, PatchKind, available_modes, classify_patch,
        effective_mode, fit_size, highlight_overlay, index_oids, size_caption, sizes_line,
        swipe_fraction,
    };
    use spork_patch::imagediff::DecodedImage;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .output()
            .unwrap();
        // `diff --no-index` exits 1 when the files differ
        assert!(out.status.code().is_some_and(|c| c <= 1), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Real `git diff` outputs for a binary file: modified (worktree, cached), added (untracked
    /// `--no-index`), deleted, and `--binary`.
    #[test]
    fn classifies_real_git_diff_outputs_of_a_binary_file() {
        let dir = std::env::temp_dir().join(format!("spork-binary-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@spork.invalid"]);
        git(&dir, &["config", "user.name", "Spork Test"]);
        std::fs::write(dir.join("a.png"), b"\x89PNG\r\n\x1a\n\x00\x00\x01").unwrap();
        std::fs::write(dir.join("t.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "one"]);
        let old_oid = git(&dir, &["rev-parse", "HEAD:a.png"]).trim().to_owned();

        std::fs::write(dir.join("a.png"), b"\x89PNG\r\n\x1a\n\x00\x00\x02\x03").unwrap();
        let base = ["diff", "--no-color", "--no-ext-diff", "--find-renames"];
        let worktree = git(&dir, &[&base[..], &["--", "a.png"]].concat());
        assert!(worktree.contains("Binary files a/a.png and b/a.png differ"));
        let PatchKind::Binary {
            old_oid: Some(old),
            new_oid: Some(_),
        } = classify_patch(&worktree)
        else {
            panic!("worktree diff should be binary with both oids: {worktree}");
        };
        assert!(old_oid.starts_with(&old), "{old} is a prefix of {old_oid}");
        // the working-tree side is read from disk, the old side from the index blob
        let target = BinaryTarget::from_patch(&worktree, "a.png", true).unwrap();
        assert_eq!(target.old, BlobSide::Blob(old.clone()));
        assert_eq!(target.new, BlobSide::WorkTree("a.png".into()));

        git(&dir, &["add", "a.png"]);
        let cached = git(&dir, &[&base[..], &["--cached", "--", "a.png"]].concat());
        let target = BinaryTarget::from_patch(&cached, "a.png", false).unwrap();
        let new_oid = git(&dir, &["rev-parse", ":a.png"]).trim().to_owned();
        let BlobSide::Blob(new) = &target.new else {
            panic!("staged new side is a blob");
        };
        assert!(new_oid.starts_with(new.as_str()));

        // --binary (GIT binary patch) carries full oids
        let full = git(
            &dir,
            &[&base[..], &["--cached", "--binary", "--", "a.png"]].concat(),
        );
        assert!(full.contains("GIT binary patch"));
        assert_eq!(
            classify_patch(&full),
            PatchKind::Binary {
                old_oid: Some(old_oid.clone()),
                new_oid: Some(new_oid.clone()),
            }
        );
        git(&dir, &["commit", "-q", "-m", "two"]);

        // added (untracked, --no-index): no old side
        std::fs::write(dir.join("new.bin"), b"q\x00").unwrap();
        let added = git(
            &dir,
            &[&base[..], &["--no-index", "--", "/dev/null", "new.bin"]].concat(),
        );
        let target = BinaryTarget::from_patch(&added, "new.bin", true).unwrap();
        assert_eq!(target.old, BlobSide::Absent);
        assert_eq!(target.new, BlobSide::WorkTree("new.bin".into()));

        // deleted: no new side
        git(&dir, &["rm", "-q", "a.png"]);
        let deleted = git(&dir, &[&base[..], &["--cached", "--", "a.png"]].concat());
        assert!(matches!(
            classify_patch(&deleted),
            PatchKind::Binary {
                old_oid: Some(_),
                new_oid: None
            }
        ));
        assert_eq!(
            BinaryTarget::from_patch(&deleted, "a.png", true).map(|t| t.new),
            Some(BlobSide::Absent)
        );

        // a text change stays text
        std::fs::write(dir.join("t.txt"), "two\n").unwrap();
        let text = git(&dir, &[&base[..], &["--", "t.txt"]].concat());
        assert_eq!(classify_patch(&text), PatchKind::Text);
        assert_eq!(BinaryTarget::from_patch(&text, "t.txt", true), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let image = ::image::RgbaImage::from_vec(width, height, rgba.to_vec()).unwrap();
        let mut buf = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut buf),
                ::image::ImageFormat::Png,
            )
            .unwrap();
        buf
    }

    /// The one background pass: both sides fetched (index blob + working-tree file), decoded,
    /// and the highlight built; an LFS pointer without its object is reported, not decoded.
    #[test]
    fn payload_reads_both_sides_and_follows_lfs_pointers() {
        let dir = std::env::temp_dir().join(format!("spork-binary-payload-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@spork.invalid"]);
        git(&dir, &["config", "user.name", "Spork Test"]);
        std::fs::write(dir.join("i.png"), png(2, 1, &[0, 0, 0, 255, 0, 0, 0, 255])).unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "one"]);
        std::fs::write(dir.join("i.png"), png(2, 1, &[0, 0, 0, 255, 9, 9, 9, 255])).unwrap();
        let patch = git(
            &dir,
            &["diff", "--no-color", "--no-ext-diff", "--", "i.png"],
        );
        let target = BinaryTarget::from_patch(&patch, "i.png", true).unwrap();
        let payload = super::load_payload(&dir, target);
        assert!(payload.old.present && payload.new.present);
        assert!(payload.worktree_file.is_some());
        let Some(Ok(pictures)) = &payload.pictures else {
            panic!("both sides decode");
        };
        let new = pictures.new.as_ref().unwrap();
        assert_eq!((new.width, new.height), (2, 1));
        assert_eq!(pictures.highlight.as_ref().map(|h| h.1), Some(1));

        // garbage under an image name: the plain panel with the decode error
        std::fs::write(dir.join("i.png"), b"not a png").unwrap();
        let payload = super::load_payload(
            &dir,
            BinaryTarget::from_patch(&patch, "i.png", true).unwrap(),
        );
        assert!(matches!(payload.pictures, Some(Err(_))));

        // an LFS pointer whose object was never fetched
        let oid = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 4096\n");
        std::fs::write(dir.join("p.png"), pointer).unwrap();
        let target = BinaryTarget {
            path: "p.png".into(),
            old: BlobSide::Absent,
            new: BlobSide::WorkTree("p.png".into()),
        };
        let payload = super::load_payload(&dir, target);
        assert_eq!(payload.new.size, Some(4096));
        assert!(payload.new.lfs);
        assert_eq!(
            payload.new.lfs_missing.as_ref().map(|p| p.oid.as_str()),
            Some(oid)
        );
        assert!(payload.pictures.is_none());
        assert!(!payload.old.present);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lfs_pointer_text_diffs_are_binary() {
        // captured from `git diff` on an LFS-tracked png (the cleaned pointer is diffed as text)
        let patch = "diff --git a/a.png b/a.png\nindex deb0ee9..6951724 100644\n--- a/a.png\n+++ b/a.png\n@@ -1,3 +1,3 @@\n version https://git-lfs.github.com/spec/v1\n-oid sha256:5eb794ac4dc0161b788b7138fe88d437a5baf01285abedd56640bb3062109cb4\n-size 12\n+oid sha256:b738ce4e5f75c6d115e9560baf85b56abd2146db93f56077b86f75fe2ab1b758\n+size 13\n";
        assert_eq!(
            classify_patch(patch),
            PatchKind::Binary {
                old_oid: Some("deb0ee9".into()),
                new_oid: Some("6951724".into()),
            }
        );
        // the version line alone (e.g. docs quoting it) is not enough
        let doc = "diff --git a/d.md b/d.md\nindex 1111111..2222222 100644\n--- a/d.md\n+++ b/d.md\n@@ -1 +1 @@\n-x\n+version https://git-lfs.github.com/spec/v1\n";
        assert_eq!(classify_patch(doc), PatchKind::Text);
    }

    #[test]
    fn only_the_first_file_is_classified() {
        let patch = "diff --git a/t b/t\nindex 1111111..2222222 100644\n--- a/t\n+++ b/t\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/p.png b/p.png\nindex 3333333..4444444 100644\nBinary files a/p.png and b/p.png differ\n";
        assert_eq!(classify_patch(patch), PatchKind::Text);
        assert_eq!(classify_patch(""), PatchKind::Text);
    }

    #[test]
    fn index_lines_yield_oids_with_zero_as_absent() {
        assert_eq!(
            index_oids("index 71b06a2..3b7f779 100644"),
            Some((Some("71b06a2".into()), Some("3b7f779".into())))
        );
        assert_eq!(
            index_oids("index 0000000..9f6cfe2"),
            Some((None, Some("9f6cfe2".into())))
        );
        assert_eq!(
            index_oids("index d5d0b8b..0000000"),
            Some((Some("d5d0b8b".into()), None))
        );
        assert_eq!(index_oids("index nothex..3b7f779 100644"), None);
        assert_eq!(index_oids("new file mode 100644"), None);
        assert_eq!(index_oids("index 71b06a2"), None);
    }

    #[test]
    fn size_captions_and_lines_are_humanized() {
        assert_eq!(size_caption(640, 480, Some(12 * 1024)), "640×480 · 12 KB");
        assert_eq!(size_caption(1, 1, Some(68)), "1×1 · 68 B");
        assert_eq!(size_caption(16, 16, None), "16×16");
        assert_eq!(sizes_line(Some(1536), Some(2048)), "1.5 KB → 2.0 KB");
        assert_eq!(sizes_line(None, Some(10)), "Added · 10 B");
        assert_eq!(sizes_line(Some(10), None), "Deleted · 10 B");
        assert_eq!(sizes_line(None, None), "No content on either side");
    }

    #[test]
    fn comparing_modes_need_both_sides() {
        assert_eq!(available_modes(true, true), ImageMode::ALL.to_vec());
        assert_eq!(available_modes(true, false), vec![ImageMode::SideBySide]);
        assert_eq!(available_modes(false, true), vec![ImageMode::SideBySide]);
        assert!(available_modes(false, false).is_empty());
        assert_eq!(
            effective_mode(ImageMode::Onion, true, true),
            ImageMode::Onion
        );
        assert_eq!(
            effective_mode(ImageMode::Swipe, false, true),
            ImageMode::SideBySide
        );
        assert_eq!(
            effective_mode(ImageMode::Highlight, true, false),
            ImageMode::SideBySide
        );
    }

    fn image(width: u32, height: u32, rgba: Vec<u8>) -> DecodedImage {
        DecodedImage {
            width,
            height,
            rgba,
        }
    }

    fn red_positions(img: &DecodedImage) -> Vec<(u32, u32)> {
        (0..img.height)
            .flat_map(|y| (0..img.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let i = ((y * img.width + x) * 4) as usize;
                img.rgba[i..i + 4] == [255, 0, 0, 255]
            })
            .collect()
    }

    #[test]
    fn highlight_paints_differing_pixels_red_over_the_new_image() {
        let old = image(2, 2, [[10, 20, 30, 255]; 4].concat());
        let mut new = old.clone();
        new.rgba[4..8].copy_from_slice(&[11, 20, 30, 255]); // (1, 0) changes by one step
        new.rgba[12..16].copy_from_slice(&[0, 0, 0, 0]); // (1, 1) becomes transparent
        let (overlay, differing) = highlight_overlay(&old, &new);
        assert_eq!((overlay.width, overlay.height), (2, 2));
        assert_eq!(differing, 2);
        assert_eq!(red_positions(&overlay), vec![(1, 0), (1, 1)]);
        // unchanged pixels keep the new image's colour
        assert_eq!(&overlay.rgba[0..4], &[10, 20, 30, 255]);
        assert_eq!(&overlay.rgba[8..12], &[10, 20, 30, 255]);
        // identical images: nothing red
        let (same, none) = highlight_overlay(&old, &old);
        assert_eq!(none, 0);
        assert!(red_positions(&same).is_empty());
    }

    #[test]
    fn highlight_covers_the_common_size_of_different_sized_images() {
        let old = image(1, 1, vec![9, 9, 9, 255]);
        let new = image(2, 1, vec![9, 9, 9, 255, 0, 0, 0, 0]);
        // (1, 0) is transparent in both (old is padded): not a difference
        let (overlay, differing) = highlight_overlay(&old, &new);
        assert_eq!((overlay.width, overlay.height, differing), (2, 1, 0));
        let wide = image(1, 2, vec![9, 9, 9, 255, 1, 2, 3, 255]);
        let (overlay, differing) = highlight_overlay(&old, &wide);
        assert_eq!((overlay.width, overlay.height, differing), (1, 2, 1));
        assert_eq!(red_positions(&overlay), vec![(0, 1)]);
    }

    #[test]
    fn images_fit_without_enlarging_and_swipe_clamps() {
        assert_eq!(fit_size(200., 100., 100., 100.), (100., 50.));
        assert_eq!(fit_size(50., 50., 400., 400.), (50., 50.));
        assert_eq!(fit_size(0., 10., 100., 100.), (0., 0.));
        assert_eq!(swipe_fraction(150., 100., 200.), 0.25);
        assert_eq!(swipe_fraction(50., 100., 200.), 0.);
        assert_eq!(swipe_fraction(400., 100., 200.), 1.);
        assert_eq!(swipe_fraction(10., 0., 0.), 0.5);
    }
}
