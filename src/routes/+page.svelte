<script lang="ts">
  import { invoke, convertFileSrc } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { getCurrentWindow, PhysicalSize } from "@tauri-apps/api/window";
  import { open, confirm, message } from "@tauri-apps/plugin-dialog";
  import { onDestroy, onMount, tick, untrack } from "svelte";
  import {
    FilmSlateIcon,
    FolderOpenIcon,
    PlayIcon,
    PauseIcon,
    RewindIcon,
    FastForwardIcon,
    ArrowClockwiseIcon,
    ArrowCounterClockwiseIcon,
    TrashIcon,
    StopIcon,
    ExportIcon,
    CircleNotchIcon,
    CircleIcon,
    CheckCircleIcon,
    XCircleIcon,
    StopCircleIcon,
  } from "phosphor-svelte";

  const WINDOW_ASPECT_RATIO = 16 / 9;
  const SPRITE_MAX_FRAMES = 512;

  const VIDEO_EXTENSIONS = ["mp4", "mov", "mkv", "avi", "webm", "m4v", "flv"];

  let isDraggingOver = $state(false);

  type VideoInfo = {
    duration: number;
    width: number;
    height: number;
    fps: number;
    codec_name: string;
    format_name: string;
    file_size: number;
  };

  type ExtractionResult = {
    output_dir: string;
    ext: string;
    frames: string[];
    sprite_path?: string;
  };

  type QueueStatus =
    | "pending"
    | "loading"
    | "ready"
    | "info-error"
    | "previewing"
    | "previewed"
    | "exporting"
    | "done"
    | "error"
    | "cancelled";

  type QueueItem = {
    path: string;
    info: VideoInfo | null;
    status: QueueStatus;
    error?: string;
    previewResult?: ExtractionResult;
    exportResult?: ExtractionResult;
    filmstripScroll?: number;
    extractionSettings?: ExtractionSettings;
    exportedSettingsKey?: string;
  };

  type ExtractionProgress = {
    frame: number;
    out_time_secs: number;
    done: boolean;
  };

  type Mode = "all" | "frames" | "seconds" | "scene";

  type ExtractionSettings = {
    mode: Mode;
    value: number;
    fixedFps: number;
    sceneThreshold: number;
    startSecs: number | null;
    endSecs: number | null;
    maxSide: number | null;
    chromaKeyEnabled: boolean;
    chromaKeyColor: string;
    chromaKeySimilarity: number;
  };

  type StoredSettings = {
    output_dir: string;
    mode: Mode;
    value: number;
    format: "png" | "jpg";
    prefix: string;
    scene_threshold: number;
    start_secs: number | null;
    end_secs: number | null;
    fixed_fps: number;
    max_side: number | null;
    auto_clear_preview: boolean;
    sprite_sheet: boolean;
    apply_to_all: boolean;
    retouch_brush_size: number;
  };

  let queue = $state<QueueItem[]>([]);
  let queueError = $state("");
  let selectedPath = $state<string | null>(null);

  const selectedItem = $derived(queue.find((item) => item.path === selectedPath) ?? null);

  let outputDir = $state("");
  let mode = $state<Mode>("frames");
  let value = $state(10);
  let fixedFps = $state(1);
  let sceneThreshold = $state(0.4);
  let startSecs = $state<number | null>(null);
  let endSecs = $state<number | null>(null);
  let maxSide = $state<number | null>(null);
  let format = $state<"png" | "jpg">("png");
  let prefix = $state("frame");
  let settingsLoaded = false;

  let chromaKeyEnabled = $state(false);
  let chromaKeyColor = $state("#00ff00");
  let chromaKeySimilarity = $state(0.2);
  let chromaKeyPanelOpen = $state(false);
  let pickingColor = $state(false);
  let previewImgEl: HTMLImageElement | undefined = $state();
  // Sizes the .frame-preview-content wrapper to the image's own aspect
  // ratio (see CSS), so the wrapper's box IS the rendered image bounds —
  // no object-fit letterbox gap left over for a checkerboard/badge to
  // incorrectly bleed into.
  let previewNaturalSize = $state<{ w: number; h: number } | null>(null);

  // Not sent to preview_frames directly (JPG has no alpha channel), but every
  // call site that sends a format needs to know when chroma key is forcing a
  // png override for a given video's settings.
  function effectiveFormat(s: ExtractionSettings): "png" | "jpg" {
    return s.chromaKeyEnabled ? "png" : format;
  }

  const MODE_OPTIONS: { mode: Mode; label: string }[] = [
    { mode: "all", label: "全幀" },
    { mode: "frames", label: "每N幀抽" },
    { mode: "seconds", label: "固定FPS" },
    { mode: "scene", label: "場景關鍵幀" },
  ];

  const estimatedFrames = $derived.by(() => {
    const info = selectedItem?.info;
    if (!info) return null;
    const duration = (endSecs ?? info.duration) - (startSecs ?? 0);
    if (duration <= 0) return 0;
    switch (mode) {
      case "all":
        return Math.round(duration * info.fps);
      case "frames":
        return Math.round((duration * info.fps) / value);
      case "seconds":
        return Math.round(duration * fixedFps);
      case "scene":
        return null;
    }
  });

  const rangeError = $derived.by(() => {
    if (startSecs !== null && endSecs !== null && startSecs >= endSecs) {
      return "起始時間必須小於結束時間";
    }
    const duration = selectedItem?.info?.duration;
    if (duration != null) {
      if (startSecs !== null && startSecs > duration) return "起始時間超過影片長度";
      if (endSecs !== null && endSecs > duration) return "結束時間超過影片長度";
    }
    return "";
  });

  let videoEl: HTMLVideoElement | undefined = $state();
  let isPlaying = $state(false);
  let videoCurrentTime = $state(0);
  let videoDuration = $state(0);
  let previewedFramePath = $state<string | null>(null);
  // Column 2 defaults to a decluttered frame-focused view now — the live
  // video only shows once explicitly revealed (too large / in the way of
  // browsing extracted frames otherwise).
  let videoVisible = $state(false);
  let filmstripEl: HTMLDivElement | undefined = $state();

  let selectedFrames = $state<Set<string>>(new Set());
  let framePlaybackFps = $state(5);
  let framePlaying = $state(false);
  let framePlaybackIndex = $state(0);
  let framePlaybackTotal = $state(0);
  let framePlaybackTimer: ReturnType<typeof setInterval> | undefined;
  let dragMode: "add" | "remove" | null = null;
  let followPlaybackScroll = $state(true);

  // Keep the currently previewed/playing frame's thumbnail in view as it
  // changes, whether that's from playback ticking forward or a manual click.
  // Playback can tick up to 60x/sec, so this only touches the DOM when the
  // thumbnail has actually scrolled out of view instead of scrolling on
  // every single tick.
  $effect(() => {
    const path = previewedFramePath;
    if (!followPlaybackScroll || !path || !selectedItem || !filmstripEl) return;
    const frames = activeResult(selectedItem)?.frames ?? [];
    const idx = frames.indexOf(path);
    if (idx < 0) return;
    const container = filmstripEl;
    const el = container.querySelector<HTMLElement>(`[data-frame-index="${idx}"]`);
    if (!el) return;
    const containerRect = container.getBoundingClientRect();
    const elRect = el.getBoundingClientRect();
    const alreadyVisible = elRect.left >= containerRect.left && elRect.right <= containerRect.right;
    if (!alreadyVisible) {
      el.scrollIntoView({ inline: "center", block: "nearest" });
    }
  });

  // Switching the selected video reuses the same <video> element (only its
  // src changes), so local playback UI state needs an explicit reset instead
  // of relying on load/pause event ordering from the previous source. Each
  // video also keeps its own filmstrip scroll position (item.filmstripScroll)
  // rather than sharing whatever position the scroll container happened to
  // be left at; restoring it has to wait a tick for the new frames to render.
  $effect(() => {
    const item = selectedItem;
    isPlaying = false;
    videoCurrentTime = 0;
    videoDuration = 0;
    previewedFramePath = null;
    videoVisible = false;
    stopFramePlayback();
    selectedFrames = new Set();
    clearFrameBlobUrls();
    pickingColor = false;
    chromaPreviewSourcePath = null;
    chromaPreviewSourceImg = null;
    retouchPanelOpen = false;
    retouchOriginalImg = null;
    retouchSourcePath = null;
    retouchUndoStack = [];
    retouchRedoStack = [];
    retouchUndoAvailable = false;
    retouchRedoAvailable = false;
    previewNaturalSize = null;
    tick().then(() => {
      if (filmstripEl) {
        filmstripEl.scrollLeft = item?.filmstripScroll ?? 0;
      }
    });
  });

  // When settings aren't shared (applyToAll off), each video keeps its own
  // extraction settings. The panel always edits the same set of variables
  // (mode/value/.../maxSide); this effect loads the incoming video's saved
  // settings into them on the way in, and its cleanup — which runs right
  // before the effect re-fires for the next video, i.e. on the way out —
  // saves whatever's currently in those variables back onto the video being
  // left. That cleanup closure reads mode/value/etc at the time it runs, not
  // reactively, so editing settings alone (without switching videos) doesn't
  // retrigger this effect.
  $effect(() => {
    const item = selectedItem;
    if (applyToAll || !item) return;
    // Read non-reactively: this effect should only fire when applyToAll or
    // the selected video changes, not when the cleanup below writes back to
    // this same field (that write would otherwise retrigger this effect on
    // every run, forever).
    const saved = untrack(() => item.extractionSettings);
    if (saved) {
      mode = saved.mode;
      value = saved.value;
      fixedFps = saved.fixedFps;
      sceneThreshold = saved.sceneThreshold;
      startSecs = saved.startSecs;
      endSecs = saved.endSecs;
      maxSide = saved.maxSide;
      chromaKeyEnabled = saved.chromaKeyEnabled;
      chromaKeyColor = saved.chromaKeyColor;
      chromaKeySimilarity = saved.chromaKeySimilarity;
    }
    return () => {
      item.extractionSettings = {
        mode,
        value,
        fixedFps,
        sceneThreshold,
        startSecs,
        endSecs,
        maxSide,
        chromaKeyEnabled,
        chromaKeyColor,
        chromaKeySimilarity,
      };
    };
  });

  onMount(() => {
    window.addEventListener("mouseup", stopDragSelect);
    window.addEventListener("keydown", onRetouchKeydown);
  });
  onDestroy(() => {
    window.removeEventListener("mouseup", stopDragSelect);
    window.removeEventListener("keydown", onRetouchKeydown);
    stopFramePlayback();
    clearFrameBlobUrls();
    clearRetouchedOverrides();
    if (chromaPreviewRaf !== undefined) cancelAnimationFrame(chromaPreviewRaf);
  });

  function stopDragSelect() {
    dragMode = null;
    retouchPainting = false;
  }

  function onFilmstripScroll() {
    if (selectedItem && filmstripEl) {
      selectedItem.filmstripScroll = filmstripEl.scrollLeft;
    }
  }

  function toggleFramePreview(framePath: string) {
    stopFramePlayback();
    pickingColor = false;
    previewedFramePath = previewedFramePath === framePath ? null : framePath;
  }

  function backToVideo() {
    stopFramePlayback();
    previewedFramePath = null;
    videoVisible = false;
  }

  function toggleVideoVisible() {
    if (videoVisible) {
      videoEl?.pause(); // otherwise it keeps playing invisibly after being hidden
      videoVisible = false;
    } else {
      videoVisible = true;
    }
  }

  // Whichever thumbnail starts the gesture decides the mode for the whole
  // drag: if it was already selected, the drag removes; otherwise it adds.
  // No modifier picks the direction anymore — the target's own state does.
  function applyDrag(framePath: string) {
    if (dragMode === "add") {
      const next = new Set(selectedFrames);
      next.add(framePath);
      selectedFrames = next;
    } else if (dragMode === "remove") {
      const next = new Set(selectedFrames);
      next.delete(framePath);
      selectedFrames = next;
    }
  }

  function onThumbMouseDown(e: MouseEvent, framePath: string) {
    if (e.button !== 0) return;
    if (e.ctrlKey || e.metaKey) {
      e.preventDefault(); // stop native image drag from swallowing mouseenter events
      dragMode = selectedFrames.has(framePath) ? "remove" : "add";
      applyDrag(framePath);
    }
  }

  function onThumbMouseEnter(framePath: string) {
    if (dragMode) applyDrag(framePath);
  }

  function onThumbClick(e: MouseEvent, framePath: string) {
    if (e.ctrlKey || e.metaKey) return; // handled by mousedown/mouseenter above
    const next = new Set(selectedFrames);
    if (next.has(framePath)) {
      next.delete(framePath);
    } else {
      next.add(framePath);
    }
    selectedFrames = next;
  }

  function onThumbContextMenu(e: MouseEvent, framePath: string) {
    e.preventDefault();
    toggleFramePreview(framePath);
  }

  function selectAllFrames() {
    const result = selectedItem && activeResult(selectedItem);
    if (!result) return;
    selectedFrames = new Set(result.frames);
  }

  function clearFrameSelection() {
    selectedFrames = new Set();
  }

  function framesToPlay(): string[] {
    const result = selectedItem && activeResult(selectedItem);
    if (!result) return [];
    if (selectedFrames.size > 0) {
      return result.frames.filter((f) => selectedFrames.has(f));
    }
    return result.frames;
  }

  // Path -> blob: URL. Tauri's asset:// protocol isn't guaranteed to be
  // served from the browser's HTTP cache like a normal fetch would be, so
  // simply "preloading" an <img> doesn't reliably stop later src swaps from
  // re-reading the file. Fetching each frame once as a blob and reusing that
  // URL guarantees every subsequent swap is instant, in-memory, no disk I/O.
  let frameBlobUrls = new Map<string, string>();

  // Manual retouch saves straight to the output folder, never touching the
  // staged file on disk — but the user still expects to see the edit when
  // looking at that same frame again in this session, so a successful save
  // stashes a blob URL of the result here, checked before the staged file.
  let retouchedOverrides = new Map<string, string>();
  // retouchedOverrides itself isn't reactive (plain Map), so nothing would
  // otherwise tell Svelte to re-evaluate displaySrcFor() after a save —
  // bumping this $state forces every call site (thumbnails included) to
  // re-run and pick up the new override.
  let retouchedOverridesVersion = $state(0);

  function displaySrcFor(path: string): string {
    void retouchedOverridesVersion;
    return retouchedOverrides.get(path) ?? frameBlobUrls.get(path) ?? convertFileSrc(path);
  }

  function clearFrameBlobUrls() {
    for (const url of frameBlobUrls.values()) {
      URL.revokeObjectURL(url);
    }
    frameBlobUrls = new Map();
  }

  function clearRetouchedOverrides() {
    for (const url of retouchedOverrides.values()) {
      URL.revokeObjectURL(url);
    }
    retouchedOverrides = new Map();
  }

  async function preloadFrame(path: string): Promise<void> {
    if (frameBlobUrls.has(path)) return;
    try {
      const res = await fetch(convertFileSrc(path));
      const blob = await res.blob();
      frameBlobUrls.set(path, URL.createObjectURL(blob));
    } catch {
      // leave it unset; displaySrcFor() falls back to the raw asset URL
    }
  }

  function loadImageElement(src: string): Promise<HTMLImageElement> {
    return new Promise((resolve, reject) => {
      const img = new Image();
      img.onload = () => resolve(img);
      img.onerror = () => reject(new Error("image load failed"));
      img.src = src;
    });
  }

  function armColorPicking() {
    if (!previewedFramePath) {
      // Auto-preview the first extracted frame instead of making the user
      // right-click a thumbnail first just to unlock the eyedropper.
      const result = selectedItem && activeResult(selectedItem);
      const first = result?.frames[0];
      if (!first) return;
      previewedFramePath = first;
    }
    pickingColor = true;
  }

  function cancelColorPicking() {
    pickingColor = false;
  }

  // Maps a click on the on-screen <img> (rendered with object-fit: contain,
  // so it may be letterboxed) to a pixel coordinate in the image's natural
  // resolution. Returns null for clicks that landed in the letterbox padding.
  function mapClickToImageCoords(e: MouseEvent, img: HTMLImageElement): { x: number; y: number } | null {
    const rect = img.getBoundingClientRect();
    const naturalW = img.naturalWidth;
    const naturalH = img.naturalHeight;
    if (!naturalW || !naturalH || rect.width === 0 || rect.height === 0) return null;

    const scale = Math.min(rect.width / naturalW, rect.height / naturalH);
    const renderedW = naturalW * scale;
    const renderedH = naturalH * scale;
    const offsetX = (rect.width - renderedW) / 2;
    const offsetY = (rect.height - renderedH) / 2;
    const clickX = e.clientX - rect.left - offsetX;
    const clickY = e.clientY - rect.top - offsetY;
    if (clickX < 0 || clickY < 0 || clickX > renderedW || clickY > renderedH) return null;

    return {
      x: Math.min(naturalW - 1, Math.floor(clickX / scale)),
      y: Math.min(naturalH - 1, Math.floor(clickY / scale)),
    };
  }

  // Loupe shown near the cursor while picking, for precise pixel targeting.
  // Positioned/drawn via direct DOM writes rather than $state, since
  // mousemove fires far too often to push through Svelte's reactivity.
  const MAGNIFIER_SIZE = 120;
  const MAGNIFIER_ZOOM = 2;
  let magnifierEl: HTMLDivElement | undefined = $state();
  let magnifierCanvas: HTMLCanvasElement | undefined = $state();

  function onPickingMouseMove(e: MouseEvent) {
    if (!pickingColor || !previewImgEl || !magnifierEl || !magnifierCanvas) return;
    const coords = mapClickToImageCoords(e, previewImgEl);

    // Centered exactly on the cursor (no offset) so the crosshair marks
    // precisely where the click will sample, with nothing displaced.
    magnifierEl.style.left = `${e.clientX - MAGNIFIER_SIZE / 2}px`;
    magnifierEl.style.top = `${e.clientY - MAGNIFIER_SIZE / 2}px`;
    magnifierEl.style.visibility = coords ? "visible" : "hidden";
    if (!coords) return;

    const ctx = magnifierCanvas.getContext("2d");
    if (!ctx) return;
    const srcSize = MAGNIFIER_SIZE / MAGNIFIER_ZOOM;
    const sx = coords.x - srcSize / 2;
    const sy = coords.y - srcSize / 2;
    ctx.imageSmoothingEnabled = false;
    ctx.clearRect(0, 0, MAGNIFIER_SIZE, MAGNIFIER_SIZE);
    ctx.drawImage(previewImgEl, sx, sy, srcSize, srcSize, 0, 0, MAGNIFIER_SIZE, MAGNIFIER_SIZE);

    const cx = MAGNIFIER_SIZE / 2;
    const cy = MAGNIFIER_SIZE / 2;
    ctx.strokeStyle = "#ffffff";
    ctx.lineWidth = 1;
    ctx.strokeRect(cx - MAGNIFIER_ZOOM / 2, cy - MAGNIFIER_ZOOM / 2, MAGNIFIER_ZOOM, MAGNIFIER_ZOOM);
    ctx.beginPath();
    ctx.moveTo(cx - 10, cy);
    ctx.lineTo(cx - MAGNIFIER_ZOOM / 2 - 1, cy);
    ctx.moveTo(cx + MAGNIFIER_ZOOM / 2 + 1, cy);
    ctx.lineTo(cx + 10, cy);
    ctx.moveTo(cx, cy - 10);
    ctx.lineTo(cx, cy - MAGNIFIER_ZOOM / 2 - 1);
    ctx.moveTo(cx, cy + MAGNIFIER_ZOOM / 2 + 1);
    ctx.lineTo(cx, cy + 10);
    ctx.stroke();
  }

  function onPickingMouseLeave() {
    if (magnifierEl) magnifierEl.style.visibility = "hidden";
  }

  async function sampleColorAt(e: MouseEvent) {
    const path = previewedFramePath;
    const imgEl = previewImgEl;
    pickingColor = false;
    if (!path || !imgEl) return;

    const coords = mapClickToImageCoords(e, imgEl);
    if (!coords) return;

    // Sample from a freshly fetched blob rather than the on-screen <img>:
    // its src may still be the raw asset:// URL (only frame-sequence
    // playback pre-fetches blobs), and reading canvas pixels drawn from that
    // taints the canvas and throws.
    await preloadFrame(path);
    try {
      const bmp = await loadImageElement(displaySrcFor(path));
      const canvas = document.createElement("canvas");
      canvas.width = bmp.naturalWidth;
      canvas.height = bmp.naturalHeight;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.drawImage(bmp, 0, 0);
      const [r, g, b] = ctx.getImageData(coords.x, coords.y, 1, 1).data;
      chromaKeyColor = `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
    } catch {
      // leave chromaKeyColor unchanged if sampling failed
    }
  }

  function handleFramePreviewClick(e: MouseEvent) {
    if (retouchPanelOpen) return; // painting is handled by mousedown/move/up instead
    if (pickingColor) {
      sampleColorAt(e);
      return;
    }
    backToVideo();
  }

  // Collapsing the panel via its own "－" only hides it (chromaKeyEnabled
  // stays true). If this button always flipped chromaKeyEnabled, reopening
  // a collapsed-but-still-enabled panel would instead turn the effect off.
  // So: closed -> open (and enable if this is the first time), open -> off.
  function toggleChromaKeyPanel() {
    if (chromaKeyPanelOpen) {
      chromaKeyEnabled = false;
      chromaKeyPanelOpen = false;
      pickingColor = false;
    } else {
      chromaKeyEnabled = true;
      chromaKeyPanelOpen = true;
    }
  }

  function closeChromaKeyPanel() {
    chromaKeyPanelOpen = false;
    pickingColor = false;
  }

  // Live, client-side chroma-key preview: toggling/adjusting the panel shows
  // the effect on the already-extracted frame immediately, without needing
  // to re-run ffmpeg. The real output still comes from ffmpeg's colorkey
  // filter at actual preview/export time — this is a preview aid only.
  let chromaPreviewCanvas: HTMLCanvasElement | undefined = $state();
  let chromaPreviewSourcePath: string | null = null;
  let chromaPreviewSourceImg: HTMLImageElement | null = null;
  let chromaPreviewRaf: number | undefined;
  const CHROMA_PREVIEW_BLEND = 0.05;
  const CHECKER_SIZE = 12;

  function drawCheckerboard(ctx: CanvasRenderingContext2D, w: number, h: number) {
    for (let y = 0; y < h; y += CHECKER_SIZE) {
      for (let x = 0; x < w; x += CHECKER_SIZE) {
        const even = (Math.round(x / CHECKER_SIZE) + Math.round(y / CHECKER_SIZE)) % 2 === 0;
        ctx.fillStyle = even ? "#cfcfcf" : "#9a9a9a";
        ctx.fillRect(x, y, CHECKER_SIZE, CHECKER_SIZE);
      }
    }
  }

  async function loadChromaSourceImage(path: string): Promise<HTMLImageElement | null> {
    await preloadFrame(path);
    try {
      return await loadImageElement(displaySrcFor(path));
    } catch {
      return null;
    }
  }

  // Renders the keyed result (checkerboard behind, alpha punched out for
  // matching pixels) from `img` into `canvas`. `maxSize` downscales the
  // working resolution first — thumbnails render at a fraction of the cost
  // of the full-size big preview this way.
  // Pure pixel math, no checkerboard baked in — reused by both the
  // read-only preview canvases (which composite it over a checkerboard) and
  // the retouch canvas (which needs the real alpha channel to paint on and
  // ultimately save).
  function computeChromaKeyedImageData(img: HTMLImageElement, w: number, h: number): ImageData | null {
    const off = document.createElement("canvas");
    off.width = w;
    off.height = h;
    const offCtx = off.getContext("2d");
    if (!offCtx) return null;
    offCtx.drawImage(img, 0, 0, w, h);
    const imageData = offCtx.getImageData(0, 0, w, h);
    const data = imageData.data;

    const hex = chromaKeyColor.replace("#", "");
    const kr = parseInt(hex.slice(0, 2), 16) || 0;
    const kg = parseInt(hex.slice(2, 4), 16) || 0;
    const kb = parseInt(hex.slice(4, 6), 16) || 0;
    const similarity = chromaKeySimilarity;

    for (let i = 0; i < data.length; i += 4) {
      const dr = (data[i] - kr) / 255;
      const dg = (data[i + 1] - kg) / 255;
      const db = (data[i + 2] - kb) / 255;
      const dist = Math.sqrt(dr * dr + dg * dg + db * db) / Math.sqrt(3);
      if (dist <= similarity) {
        data[i + 3] = 0;
      } else if (dist <= similarity + CHROMA_PREVIEW_BLEND) {
        data[i + 3] = Math.round((255 * (dist - similarity)) / CHROMA_PREVIEW_BLEND);
      }
    }
    return imageData;
  }

  function applyChromaKeyToCanvas(canvas: HTMLCanvasElement, img: HTMLImageElement, maxSize?: number) {
    let w = img.naturalWidth;
    let h = img.naturalHeight;
    if (!w || !h) return;
    if (maxSize) {
      const scale = Math.min(1, maxSize / Math.max(w, h));
      w = Math.max(1, Math.round(w * scale));
      h = Math.max(1, Math.round(h * scale));
    }

    const imageData = computeChromaKeyedImageData(img, w, h);
    if (!imageData) return;
    const off = document.createElement("canvas");
    off.width = w;
    off.height = h;
    off.getContext("2d")?.putImageData(imageData, 0, 0);

    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.clearRect(0, 0, w, h);
    drawCheckerboard(ctx, w, h);
    ctx.drawImage(off, 0, 0);
  }

  async function renderChromaPreview() {
    const canvas = chromaPreviewCanvas;
    const path = previewedFramePath;
    if (!canvas || !chromaKeyEnabled || !path) return;

    if (chromaPreviewSourcePath !== path) {
      chromaPreviewSourceImg = await loadChromaSourceImage(path);
      chromaPreviewSourcePath = chromaPreviewSourceImg ? path : null;
      // The awaited load may have outlived a frame switch or a toggle-off.
      if (previewedFramePath !== path || !chromaKeyEnabled) return;
    }
    if (chromaPreviewSourceImg) applyChromaKeyToCanvas(canvas, chromaPreviewSourceImg);
  }

  function scheduleChromaPreviewRender() {
    if (chromaPreviewRaf !== undefined) return;
    chromaPreviewRaf = requestAnimationFrame(() => {
      chromaPreviewRaf = undefined;
      renderChromaPreview();
      scheduleThumbChromaRenders();
    });
  }

  $effect(() => {
    if (!chromaKeyEnabled || !previewedFramePath) return;
    // Read to register as reactive dependencies; renderChromaPreview reads
    // the live values itself once scheduled.
    void chromaKeyColor;
    void chromaKeySimilarity;
    scheduleChromaPreviewRender();
  });

  // Filmstrip thumbnails live-update the same way, in a single batched pass
  // (not one fetch+render per thumbnail per keystroke) — thumbCanvases is
  // populated by the chromaThumb action as thumbnails mount/unmount.
  let thumbCanvases = new Map<string, HTMLCanvasElement>();
  const THUMB_CHROMA_SIZE = 120;

  async function renderThumbChroma(path: string, canvas: HTMLCanvasElement) {
    if (!chromaKeyEnabled) {
      canvas.getContext("2d")?.clearRect(0, 0, canvas.width, canvas.height);
      return;
    }
    const img = await loadChromaSourceImage(path);
    // Bail if the thumbnail was unmounted or reassigned while awaiting.
    if (!img || thumbCanvases.get(path) !== canvas) return;
    applyChromaKeyToCanvas(canvas, img, THUMB_CHROMA_SIZE);
  }

  function scheduleThumbChromaRenders() {
    for (const [path, canvas] of thumbCanvases) {
      renderThumbChroma(path, canvas);
    }
  }

  function chromaThumb(canvas: HTMLCanvasElement, path: string) {
    thumbCanvases.set(path, canvas);
    if (chromaKeyEnabled) renderThumbChroma(path, canvas);
    return {
      update(newPath: string) {
        thumbCanvases.delete(path);
        path = newPath;
        thumbCanvases.set(path, canvas);
        if (chromaKeyEnabled) renderThumbChroma(path, canvas);
      },
      destroy() {
        thumbCanvases.delete(path);
      },
    };
  }

  // Toggling chroma key itself (not just color/similarity, already covered
  // by the effect above) also has to repaint every mounted thumbnail.
  $effect(() => {
    if (chromaKeyEnabled) {
      scheduleThumbChromaRenders();
    } else {
      for (const canvas of thumbCanvases.values()) {
        canvas.getContext("2d")?.clearRect(0, 0, canvas.width, canvas.height);
      }
    }
  });

  // Manual retouch: a standalone, single-frame paint tool. It starts from
  // whatever's currently shown (chroma-keyed or not), lets the user erase
  // (force alpha to 0) or restore (paint the original pixels back) with a
  // brush, and "套用並存檔" writes the result straight to the output folder
  // — it doesn't touch the staged/export pipeline at all.
  const RETOUCH_UNDO_LIMIT = 50;
  let retouchPanelOpen = $state(false);
  let retouchMode = $state<"erase" | "restore">("erase");
  let retouchBrushSize = $state(24);
  let retouchUndoAvailable = $state(false);
  let retouchRedoAvailable = $state(false);
  let retouchSaving = $state(false);
  let retouchCanvas: HTMLCanvasElement | undefined = $state();
  let retouchCursorEl: HTMLDivElement | undefined = $state();

  let retouchOriginalImg: HTMLImageElement | null = null;
  let retouchSourcePath: string | null = null;
  let retouchPainting = false;
  let retouchUndoStack: ImageData[] = [];
  let retouchRedoStack: ImageData[] = [];

  function updateRetouchHistoryFlags() {
    retouchUndoAvailable = retouchUndoStack.length > 0;
    retouchRedoAvailable = retouchRedoStack.length > 0;
  }

  async function initRetouchCanvas() {
    const canvas = retouchCanvas;
    const path = previewedFramePath;
    if (!canvas || !path) return;
    const img = await loadChromaSourceImage(path);
    if (!img) return;
    const w = img.naturalWidth;
    const h = img.naturalHeight;
    if (!w || !h) return;

    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.clearRect(0, 0, w, h);
    if (chromaKeyEnabled) {
      const imageData = computeChromaKeyedImageData(img, w, h);
      if (imageData) ctx.putImageData(imageData, 0, 0);
    } else {
      ctx.drawImage(img, 0, 0);
    }

    retouchOriginalImg = img;
    retouchSourcePath = path;
    retouchUndoStack = [];
    retouchRedoStack = [];
    updateRetouchHistoryFlags();
  }

  async function openRetouchPanel() {
    if (!hasFrames) return;
    if (!previewedFramePath) {
      const result = selectedItem && activeResult(selectedItem);
      const first = result?.frames[0];
      if (!first) return;
      previewedFramePath = first;
    }
    retouchPanelOpen = true;
    await tick();
    await initRetouchCanvas();
  }

  function closeRetouchPanel() {
    retouchPanelOpen = false;
    retouchOriginalImg = null;
    retouchSourcePath = null;
    retouchUndoStack = [];
    retouchRedoStack = [];
    updateRetouchHistoryFlags();
  }

  function toggleRetouchPanel() {
    if (retouchPanelOpen) {
      closeRetouchPanel();
    } else {
      openRetouchPanel();
    }
  }

  function retouchReset() {
    if (retouchSourcePath) initRetouchCanvas();
  }

  // The currently-shown frame changing (a different thumbnail right-clicked,
  // or playback) while the panel stays open re-seeds the canvas for the new
  // frame, discarding any unsaved strokes on the old one.
  $effect(() => {
    const path = previewedFramePath;
    if (!retouchPanelOpen || !path) return;
    if (path !== retouchSourcePath) initRetouchCanvas();
  });

  function paintRetouchDab(x: number, y: number) {
    const canvas = retouchCanvas;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const r = retouchBrushSize / 2;
    ctx.save();
    ctx.beginPath();
    ctx.arc(x, y, r, 0, Math.PI * 2);
    if (retouchMode === "erase") {
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = "rgba(0,0,0,1)";
      ctx.fill();
    } else if (retouchOriginalImg) {
      // Restoring re-draws the pristine original within the brush circle —
      // erasing only zeroes alpha (canvas compositing doesn't reliably keep
      // usable RGB behind fully transparent pixels), so simply flipping
      // alpha back on isn't enough to recover the real color underneath.
      ctx.clip();
      ctx.globalCompositeOperation = "source-over";
      ctx.drawImage(retouchOriginalImg, 0, 0, canvas.width, canvas.height);
    }
    ctx.restore();
  }

  function onFramePreviewMouseDown(e: MouseEvent) {
    if (!retouchPanelOpen || !retouchCanvas || !previewImgEl) return;
    const coords = mapClickToImageCoords(e, previewImgEl);
    if (!coords) return;
    e.preventDefault();
    const ctx = retouchCanvas.getContext("2d");
    if (!ctx) return;
    retouchUndoStack.push(ctx.getImageData(0, 0, retouchCanvas.width, retouchCanvas.height));
    if (retouchUndoStack.length > RETOUCH_UNDO_LIMIT) retouchUndoStack.shift();
    retouchRedoStack = [];
    updateRetouchHistoryFlags();
    retouchPainting = true;
    paintRetouchDab(coords.x, coords.y);
  }

  function updateRetouchCursor(e: MouseEvent) {
    if (!retouchCursorEl || !previewImgEl) return;
    const rect = previewImgEl.getBoundingClientRect();
    const naturalW = previewImgEl.naturalWidth;
    const naturalH = previewImgEl.naturalHeight;
    if (!naturalW || !naturalH || rect.width === 0 || rect.height === 0) return;
    const scale = Math.min(rect.width / naturalW, rect.height / naturalH);
    const screenSize = Math.max(4, retouchBrushSize * scale);
    retouchCursorEl.style.width = `${screenSize}px`;
    retouchCursorEl.style.height = `${screenSize}px`;
    retouchCursorEl.style.left = `${e.clientX - screenSize / 2}px`;
    retouchCursorEl.style.top = `${e.clientY - screenSize / 2}px`;
    retouchCursorEl.style.visibility = "visible";
  }

  function onFramePreviewMouseMove(e: MouseEvent) {
    if (retouchPanelOpen) {
      updateRetouchCursor(e);
      if (retouchPainting && previewImgEl) {
        const coords = mapClickToImageCoords(e, previewImgEl);
        if (coords) paintRetouchDab(coords.x, coords.y);
      }
      return;
    }
    onPickingMouseMove(e);
  }

  function onFramePreviewMouseLeave() {
    if (retouchCursorEl) retouchCursorEl.style.visibility = "hidden";
    onPickingMouseLeave();
  }

  function retouchUndo() {
    const canvas = retouchCanvas;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx || retouchUndoStack.length === 0) return;
    retouchRedoStack.push(ctx.getImageData(0, 0, canvas.width, canvas.height));
    const prev = retouchUndoStack.pop()!;
    ctx.putImageData(prev, 0, 0);
    updateRetouchHistoryFlags();
  }

  function retouchRedo() {
    const canvas = retouchCanvas;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx || retouchRedoStack.length === 0) return;
    retouchUndoStack.push(ctx.getImageData(0, 0, canvas.width, canvas.height));
    const next = retouchRedoStack.pop()!;
    ctx.putImageData(next, 0, 0);
    updateRetouchHistoryFlags();
  }

  function onRetouchKeydown(e: KeyboardEvent) {
    if (!retouchPanelOpen) return;
    if (!(e.ctrlKey || e.metaKey)) return;
    if (e.key.toLowerCase() === "z") {
      e.preventDefault();
      retouchUndo();
    } else if (e.key.toLowerCase() === "y") {
      e.preventDefault();
      retouchRedo();
    }
  }

  async function saveRetouchedFrame() {
    if (!retouchCanvas || !selectedItem || previewedFrameNumber === null || !previewedFramePath) return;
    if (!outputDir) {
      await message("請先選擇輸出資料夾。", { title: "尚未設定輸出資料夾", kind: "warning" });
      return;
    }
    retouchSaving = true;
    try {
      const blob: Blob | null = await new Promise((resolve) => retouchCanvas!.toBlob(resolve, "image/png"));
      if (!blob) throw new Error("轉換圖片失敗");
      const bytes = Array.from(new Uint8Array(await blob.arrayBuffer()));
      await invoke("save_retouched_frame", {
        outputDir,
        inputPath: selectedItem.path,
        prefix,
        frameNumber: previewedFrameNumber,
        data: bytes,
      });
      // So this frame shows the retouched result immediately, here and in
      // the filmstrip, without needing to re-preview (the staged file on
      // disk is untouched — this only overrides what's displayed).
      const oldUrl = retouchedOverrides.get(previewedFramePath);
      retouchedOverrides.set(previewedFramePath, URL.createObjectURL(blob));
      retouchedOverridesVersion++;
      if (oldUrl) URL.revokeObjectURL(oldUrl);
      await message(`已存檔第 ${previewedFrameNumber} 張。`, { title: "微調完成", kind: "info" });
    } catch (e) {
      await message(String(e), { title: "存檔失敗", kind: "error" });
    } finally {
      retouchSaving = false;
    }
  }

  async function startFramePlayback() {
    const frames = framesToPlay();
    if (frames.length === 0) return;
    framePlaying = true;
    framePlaybackTotal = frames.length;
    let idx = Math.max(0, frames.indexOf(previewedFramePath ?? ""));
    framePlaybackIndex = idx;
    previewedFramePath = frames[idx];

    // Without this, every tick waits on a fresh disk read + decode for that
    // frame's image, which is what made playback feel much slower than the
    // configured fps. Preloading once up front means each tick just swaps to
    // an already-fetched, in-memory image.
    await Promise.all(frames.map(preloadFrame));
    if (!framePlaying) return; // user paused while frames were still loading

    framePlaybackTimer = setInterval(() => {
      idx = (idx + 1) % frames.length;
      framePlaybackIndex = idx;
      previewedFramePath = frames[idx];
    }, 1000 / framePlaybackFps);
  }

  function stopFramePlayback() {
    framePlaying = false;
    if (framePlaybackTimer) clearInterval(framePlaybackTimer);
    framePlaybackTimer = undefined;
  }

  function togglePlayback() {
    const inFrameMode = framePlaying || previewedFramePath !== null || selectedFrames.size > 0;
    if (inFrameMode) {
      if (framePlaying) {
        stopFramePlayback();
      } else {
        startFramePlayback();
      }
      return;
    }
    if (!videoEl) return;
    if (videoEl.paused) {
      videoEl.play();
    } else {
      videoEl.pause();
    }
  }

  function skipBy(deltaSecs: number) {
    if (!videoEl) return;
    videoEl.currentTime = Math.max(0, Math.min(videoDuration, videoEl.currentTime + deltaSecs));
  }

  function seekTo() {
    if (!videoEl) return;
    videoEl.currentTime = videoCurrentTime;
  }

  function formatTime(secs: number | null): string {
    if (secs === null) return "";
    const total = Math.max(0, Math.round(secs));
    const m = Math.floor(total / 60);
    const s = total % 60;
    return `${m}:${String(s).padStart(2, "0")}`;
  }

  // undefined = could not parse (caller should revert the display),
  // null = field was cleared, number = parsed seconds.
  function parseTime(text: string): number | null | undefined {
    const trimmed = text.trim();
    if (trimmed === "") return null;
    const mmss = trimmed.match(/^(\d+):(\d{1,2})$/);
    if (mmss) {
      const m = Number(mmss[1]);
      const s = Number(mmss[2]);
      if (s >= 60) return undefined;
      return m * 60 + s;
    }
    if (/^\d+(\.\d+)?$/.test(trimmed)) {
      return Number(trimmed);
    }
    return undefined;
  }

  let startText = $state("");
  let endText = $state("");

  $effect(() => {
    startText = formatTime(startSecs);
  });
  $effect(() => {
    endText = formatTime(endSecs);
  });

  function commitStartText() {
    const parsed = parseTime(startText);
    if (parsed === undefined) {
      startText = formatTime(startSecs);
      return;
    }
    startSecs = parsed;
  }

  function commitEndText() {
    const parsed = parseTime(endText);
    if (parsed === undefined) {
      endText = formatTime(endSecs);
      return;
    }
    endSecs = parsed;
  }

  function setStartFromPlayer() {
    if (!videoEl) return;
    startSecs = videoEl.currentTime;
  }

  function setEndFromPlayer() {
    if (!videoEl) return;
    endSecs = videoEl.currentTime;
  }

  function clearRange() {
    startSecs = null;
    endSecs = null;
  }

  let previewRunning = $state(false);
  let previewCancelled = false;
  let autoClearPreview = $state(true);
  let spriteSheet = $state(false);
  let applyToAll = $state(true);
  let currentIndex = $state(-1);
  let progressSecs = $state(0);

  let exportRunning = $state(false);
  let exportIndex = $state(-1);
  let exportTotal = $state(0);

  onMount(async () => {
    try {
      const saved = await invoke<StoredSettings | null>("load_settings");
      if (saved) {
        outputDir = saved.output_dir;
        mode = saved.mode;
        value = saved.value;
        format = saved.format;
        prefix = saved.prefix ?? "frame";
        sceneThreshold = saved.scene_threshold || 0.4;
        startSecs = saved.start_secs ?? null;
        endSecs = saved.end_secs ?? null;
        fixedFps = saved.fixed_fps || 1;
        maxSide = saved.max_side ?? null;
        autoClearPreview = saved.auto_clear_preview ?? true;
        spriteSheet = saved.sprite_sheet ?? false;
        applyToAll = saved.apply_to_all ?? true;
        retouchBrushSize = saved.retouch_brush_size ?? 24;
      }
    } finally {
      settingsLoaded = true;
    }
  });

  $effect(() => {
    const settings: StoredSettings = {
      output_dir: outputDir,
      mode,
      value,
      format,
      prefix,
      scene_threshold: sceneThreshold,
      start_secs: startSecs,
      end_secs: endSecs,
      fixed_fps: fixedFps,
      max_side: maxSide,
      auto_clear_preview: autoClearPreview,
      sprite_sheet: spriteSheet,
      apply_to_all: applyToAll,
      retouch_brush_size: retouchBrushSize,
    };
    if (!settingsLoaded) return;
    invoke("save_settings", { settings }).catch(() => {
      // best-effort persistence; ignore failures
    });
  });

  const unlistenProgress = listen<ExtractionProgress>("extraction-progress", (event) => {
    progressSecs = event.payload.out_time_secs;
  });
  onDestroy(async () => {
    (await unlistenProgress)();
  });

  const unlistenDragDrop = getCurrentWebview().onDragDropEvent((event) => {
    if (event.payload.type === "over") {
      isDraggingOver = true;
    } else if (event.payload.type === "leave") {
      isDraggingOver = false;
    } else if (event.payload.type === "drop") {
      isDraggingOver = false;
      addPaths(event.payload.paths);
    }
  });
  onDestroy(async () => {
    (await unlistenDragDrop)();
  });

  // Tauri has no native aspect-ratio lock, so this snaps the window back to
  // 16:9 a moment after the user stops dragging, rather than fighting them
  // mid-drag. `correcting` plus the epsilon check below stop the corrective
  // setSize() call from re-triggering this same handler.
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;
  let correctingWindowSize = false;

  const unlistenResize = getCurrentWindow().onResized(({ payload: size }) => {
    if (correctingWindowSize) return;
    if (resizeTimer) clearTimeout(resizeTimer);
    resizeTimer = setTimeout(async () => {
      const { width, height } = size;
      if (width < 100 || height < 100) return; // minimized or mid-transition
      if (await getCurrentWindow().isMaximized()) return; // maximize is intentional, don't fight it
      const targetHeight = Math.round(width / WINDOW_ASPECT_RATIO);
      if (Math.abs(targetHeight - height) < 2) return;
      correctingWindowSize = true;
      try {
        await getCurrentWindow().setSize(new PhysicalSize(width, targetHeight));
      } catch (e) {
        console.error("failed to correct window size", e);
      } finally {
        correctingWindowSize = false;
      }
    }, 1000);
  });
  onDestroy(async () => {
    if (resizeTimer) clearTimeout(resizeTimer);
    (await unlistenResize)();
  });

  function filename(p: string): string {
    return p.split(/[\\/]/).pop() ?? p;
  }

  function statusLabel(item: QueueItem): string {
    switch (item.status) {
      case "pending":
      case "loading":
        return "讀取中…";
      case "info-error":
        return `讀取資訊失敗：${item.error ?? ""}`;
      case "ready":
        return item.info ? `${item.info.duration.toFixed(1)}s` : "就緒";
      case "previewing":
        return "產生預覽中…";
      case "previewed":
        return `預覽完成，${item.previewResult?.frames.length ?? 0} 張`;
      case "exporting":
        return "導出中…";
      case "done": {
        const n = item.exportResult?.frames.length ?? 0;
        if (item.exportResult?.sprite_path) return `已導出，${n} 張（含精靈圖）`;
        if (spriteSheet && n > SPRITE_MAX_FRAMES) return `已導出，${n} 張（幀數超過 ${SPRITE_MAX_FRAMES}，未產生精靈圖）`;
        return `已導出，${n} 張`;
      }
      case "error":
        return `失敗：${item.error ?? ""}`;
      case "cancelled":
        return "已取消";
    }
  }

  function activeResult(item: QueueItem): ExtractionResult | undefined {
    return item.exportResult ?? item.previewResult;
  }

  const hasFrames = $derived(
    !!selectedItem && (activeResult(selectedItem)?.frames.length ?? 0) > 0,
  );

  const previewedFrameNumber = $derived.by(() => {
    if (!previewedFramePath || !selectedItem) return null;
    const idx = activeResult(selectedItem)?.frames.indexOf(previewedFramePath) ?? -1;
    return idx >= 0 ? idx + 1 : null;
  });

  // The filmstrip is a single horizontal row; redirect the mouse wheel's
  // vertical delta into horizontal scroll so users don't need Shift+wheel.
  function onFilmstripWheel(e: WheelEvent) {
    if (e.deltaY === 0) return;
    (e.currentTarget as HTMLElement).scrollLeft += e.deltaY;
    e.preventDefault();
  }

  async function loadInfoFor(item: QueueItem) {
    item.status = "loading";
    try {
      item.info = await invoke<VideoInfo>("get_video_info", { path: item.path });
      item.status = "ready";
    } catch (e) {
      item.status = "info-error";
      item.error = String(e);
    }
  }

  async function addPaths(rawPaths: string[]) {
    queueError = "";
    const resolvedLists = await Promise.all(
      rawPaths.map((p) =>
        invoke<string[]>("resolve_video_paths", { path: p }).catch(() => [] as string[]),
      ),
    );
    const resolved = resolvedLists.flat();
    if (resolved.length === 0) {
      queueError = "找不到影片檔案。";
      return;
    }

    const existing = new Set(queue.map((q) => q.path));
    const newItems: QueueItem[] = [];
    for (const p of resolved) {
      if (existing.has(p)) continue;
      existing.add(p);
      newItems.push({ path: p, info: null, status: "pending" });
    }
    queue.push(...newItems);
    if (!selectedPath && newItems.length > 0) {
      selectedPath = newItems[0].path;
    }
    // Re-read each item back out of `queue` before mutating it: the objects
    // in `newItems` are plain pre-insertion references, and mutating those
    // directly bypasses Svelte's reactivity (updates land on a copy the UI
    // never re-reads until something unrelated forces a fresh render).
    const newPaths = new Set(newItems.map((item) => item.path));
    for (const item of queue.filter((q) => newPaths.has(q.path))) {
      loadInfoFor(item);
    }
  }

  function removeItem(path: string) {
    queue = queue.filter((item) => item.path !== path);
    if (selectedPath === path) {
      selectedPath = queue[0]?.path ?? null;
    }
    invoke("cleanup_staging", { inputPath: path }).catch(() => {
      // best-effort; a leftover staging folder isn't worth surfacing an error
    });
  }

  function clearQueue() {
    for (const item of queue) {
      invoke("cleanup_staging", { inputPath: item.path }).catch(() => {});
    }
    queue = [];
    queueError = "";
    selectedPath = null;
  }

  // Anywhere in column 2 works, but only while the queue is genuinely empty
  // — once a video's been added, double-clicking the preview area shouldn't
  // pop the file picker (it did before when this checked !selectedItem
  // instead: after removing the selected item without picking a new one,
  // the empty state shows again even though the queue still has videos).
  function onColPreviewDblClick() {
    if (queue.length === 0) pickVideos();
  }

  async function pickVideos() {
    const selected = await open({
      multiple: true,
      filters: [{ name: "Video", extensions: VIDEO_EXTENSIONS }],
    });
    if (Array.isArray(selected)) {
      await addPaths(selected);
    } else if (typeof selected === "string") {
      await addPaths([selected]);
    }
  }

  async function pickFolder() {
    const selected = await open({ directory: true });
    if (typeof selected === "string") {
      await addPaths([selected]);
    }
  }

  async function pickOutputDir() {
    const selected = await open({ directory: true });
    if (typeof selected === "string") {
      outputDir = selected;
    }
  }

  // The panel always edits the shared mode/value/.../maxSide variables; when
  // applyToAll is off they only reflect the currently *selected* video (see
  // the settings-sync effect above), so any other video's own settings have
  // to come from its saved snapshot instead, falling back to the live
  // variables if it was never individually customized.
  function extractionSettingsFor(item: QueueItem): ExtractionSettings {
    if (applyToAll || item === selectedItem) {
      return { mode, value, fixedFps, sceneThreshold, startSecs, endSecs, maxSide, chromaKeyEnabled, chromaKeyColor, chromaKeySimilarity };
    }
    return (
      item.extractionSettings ?? {
        mode,
        value,
        fixedFps,
        sceneThreshold,
        startSecs,
        endSecs,
        maxSide,
        chromaKeyEnabled,
        chromaKeyColor,
        chromaKeySimilarity,
      }
    );
  }

  // The fps a frame sequence was effectively sampled at, so playback speed
  // can default to "roughly real-time" for whichever mode produced it.
  // Scene-cut mode has no fixed rate, so it's left alone (returns null).
  function effectiveExtractionFps(): number | null {
    if (!selectedItem?.info) return null;
    const s = extractionSettingsFor(selectedItem);
    const sourceFps = selectedItem.info.fps;
    switch (s.mode) {
      case "all":
        return sourceFps;
      case "seconds":
        return s.fixedFps;
      case "frames":
        return s.value > 0 ? sourceFps / s.value : null;
      case "scene":
        return null;
    }
  }

  $effect(() => {
    const fps = effectiveExtractionFps();
    if (fps && fps > 0) {
      framePlaybackFps = Math.min(60, Math.max(1, Math.round(fps)));
    }
  });

  async function runPreview(event: Event) {
    event.preventDefault();
    if (queue.length === 0 || rangeError) return;

    stopFramePlayback();
    selectedFrames = new Set();
    previewRunning = true;
    previewCancelled = false;

    for (let i = 0; i < queue.length; i++) {
      if (previewCancelled) break;
      const item = queue[i];
      if (!autoClearPreview && (item.status === "previewed" || item.status === "done")) continue;

      currentIndex = i;
      progressSecs = 0;
      item.status = "previewing";
      item.error = undefined;
      item.exportResult = undefined; // stale relative to the preview about to run
      item.exportedSettingsKey = undefined;

      const s = extractionSettingsFor(item);
      try {
        item.previewResult = await invoke<ExtractionResult>("preview_frames", {
          inputPath: item.path,
          mode: s.mode,
          value:
            s.mode === "scene" ? s.sceneThreshold : s.mode === "seconds" ? 1 / s.fixedFps : s.value,
          format,
          prefix,
          startSecs: s.startSecs,
          endSecs: s.endSecs,
          maxSide: s.maxSide,
        });
        item.status = "previewed";
      } catch (e) {
        if (String(e) === "cancelled") {
          item.status = "cancelled";
          previewCancelled = true;
        } else {
          item.status = "error";
          item.error = String(e);
        }
      }
    }

    currentIndex = -1;
    previewRunning = false;
  }

  async function cancelPreview() {
    previewCancelled = true;
    try {
      await invoke("cancel_extraction");
    } catch (e) {
      if (currentIndex >= 0 && queue[currentIndex]) {
        queue[currentIndex].error = String(e);
      }
    }
  }

  // Captures everything that affects export output for a video, so a repeat
  // export with nothing actually changed can be told apart from a real one.
  function exportSettingsKeyFor(item: QueueItem): string {
    const s = extractionSettingsFor(item);
    return JSON.stringify({
      mode: s.mode,
      value: s.value,
      fixedFps: s.fixedFps,
      sceneThreshold: s.sceneThreshold,
      startSecs: s.startSecs,
      endSecs: s.endSecs,
      maxSide: s.maxSide,
      chromaKeyEnabled: s.chromaKeyEnabled,
      chromaKeyColor: s.chromaKeyColor,
      chromaKeySimilarity: s.chromaKeySimilarity,
      format,
      prefix,
      spriteSheet,
    });
  }

  async function runExport() {
    if (!outputDir) return;
    // "done" items stay eligible: the staged frames aren't deleted after
    // export anymore, so re-exporting (e.g. after toggling chroma key)
    // doesn't require re-running 抽幀 first.
    const targets = queue.filter((item) => item.status === "previewed" || item.status === "done");
    if (targets.length === 0) return;

    const unchanged = targets.every(
      (item) => item.status === "done" && item.exportedSettingsKey === exportSettingsKeyFor(item),
    );
    if (unchanged) {
      await message("設定跟上次導出時相同，已經導出過了。", { title: "沒有變更", kind: "info" });
      return;
    }

    const collisions: string[] = [];
    for (const item of targets) {
      const existing = await invoke<number>("count_existing_frames", {
        outputDir,
        inputPath: item.path,
        format: effectiveFormat(extractionSettingsFor(item)),
      });
      if (existing > 0) collisions.push(filename(item.path));
    }
    if (collisions.length > 0) {
      const proceed = await confirm(
        `${collisions.length} 支影片的輸出資料夾已經有圖片，導出會覆蓋同名檔案。要繼續嗎？\n\n${collisions.join("\n")}`,
        { title: "確認覆蓋", kind: "warning" },
      );
      if (!proceed) return;
    }

    exportRunning = true;
    exportTotal = targets.length;

    for (let i = 0; i < targets.length; i++) {
      exportIndex = i;
      const item = targets[i];
      item.status = "exporting";
      item.error = undefined;
      try {
        const s = extractionSettingsFor(item);
        const settingsKey = exportSettingsKeyFor(item);
        item.exportResult = await invoke<ExtractionResult>("export_frames", {
          inputPath: item.path,
          outputDir,
          format,
          prefix,
          spriteSheet,
          chromaKeyColor: s.chromaKeyEnabled ? s.chromaKeyColor : null,
          chromaKeySimilarity: s.chromaKeyEnabled ? s.chromaKeySimilarity : null,
        });
        item.exportedSettingsKey = settingsKey;
        item.status = "done";
      } catch (e) {
        item.status = "error";
        item.error = String(e);
      }
    }

    exportIndex = -1;
    exportTotal = 0;
    exportRunning = false;
  }

  // ffmpeg's -progress out_time is already relative to the trimmed clip when
  // -ss/-to are given (verified: a 3s-8s slice reports out_time up to 5s, not
  // 8s), so the denominator here must be the trimmed range's length too, not
  // the source video's full duration.
  const currentDuration = $derived.by(() => {
    const info = currentIndex >= 0 ? queue[currentIndex]?.info : null;
    if (!info) return 0;
    const end = endSecs ?? info.duration;
    const start = startSecs ?? 0;
    return Math.max(0, end - start);
  });
  const progressPct = $derived(
    currentDuration > 0 ? Math.min(100, (progressSecs / currentDuration) * 100) : 0,
  );

  const canExport = $derived(
    !previewRunning &&
      !exportRunning &&
      !!outputDir &&
      queue.some((item) => item.status === "previewed" || item.status === "done"),
  );
</script>

<div class="app" class:drag-over={isDraggingOver}>
  <div class="top-row">
    <aside class="col col-queue">
      <h1>takeframe</h1>

      <div class="picker-row">
        <button type="button" class="primary" onclick={pickVideos}>
          <FilmSlateIcon size={16} weight="bold" />
          選擇影片
        </button>
        <button type="button" onclick={pickFolder}>
          <FolderOpenIcon size={16} />
          資料夾
        </button>
      </div>

      {#if queueError}
        <p class="error">{queueError}</p>
      {/if}

      <div class="queue-box">
        {#if queue.length > 0}
        <ul class="queue">
          {#each queue as item (item.path)}
            <li class="queue-entry" class:selected={item.path === selectedPath}>
              <div class="queue-row">
                <button
                  type="button"
                  class="select-row"
                  onclick={() => (selectedPath = item.path)}
                >
                  <span class="status-icon status-{item.status}" title={statusLabel(item)} aria-hidden="true">
                    {#if item.status === "pending" || item.status === "loading" || item.status === "exporting"}
                      <CircleNotchIcon size={13} class="spin" />
                    {:else if item.status === "ready"}
                      <CircleIcon size={13} />
                    {:else if item.status === "previewing"}
                      <PlayIcon size={13} weight="fill" />
                    {:else if item.status === "previewed"}
                      <CheckCircleIcon size={13} />
                    {:else if item.status === "done"}
                      <CheckCircleIcon size={13} weight="fill" />
                    {:else if item.status === "error" || item.status === "info-error"}
                      <XCircleIcon size={13} weight="fill" />
                    {:else if item.status === "cancelled"}
                      <StopCircleIcon size={13} weight="fill" />
                    {/if}
                  </span>
                  <span class="name" title={item.path}>{filename(item.path)}</span>
                </button>
                <div class="row-actions">
                  {#if item.status === "info-error"}
                    <button
                      type="button"
                      class="icon-btn"
                      title="重試"
                      aria-label="重試"
                      onclick={() => loadInfoFor(item)}
                      disabled={previewRunning || exportRunning}
                    >
                      <ArrowClockwiseIcon size={13} />
                    </button>
                  {/if}
                  <button
                    type="button"
                    class="icon-btn danger"
                    title="移除"
                    aria-label="移除"
                    onclick={() => removeItem(item.path)}
                    disabled={previewRunning || exportRunning}
                  >
                    <TrashIcon size={13} />
                  </button>
                </div>
              </div>
            </li>
          {/each}
        </ul>
        {:else}
          <p class="meta empty-hint">拖曳影片到這裡，或用上方按鈕選擇</p>
        {/if}
      </div>

      <div class="side-block">
        <h3>抽幀設置</h3>

        <div class="mode-grid">
          {#each MODE_OPTIONS as opt (opt.mode)}
            <button
              type="button"
              class="mode-btn"
              class:active={mode === opt.mode}
              onclick={() => (mode = opt.mode)}
              disabled={previewRunning || exportRunning}
            >
              {opt.label}
            </button>
          {/each}
        </div>

        {#if mode === "frames"}
          <div class="slider-row">
            <input type="range" min="1" max="60" step="1" bind:value />
            <span class="slider-value">每 {value} 幀</span>
          </div>
        {:else if mode === "seconds"}
          <div class="slider-row">
            <input type="range" min="0.1" max="60" step="0.1" bind:value={fixedFps} />
            <span class="slider-value">{fixedFps.toFixed(1)} FPS</span>
          </div>
        {:else if mode === "scene"}
          <div class="slider-row">
            <input type="range" min="0" max="1" step="0.05" bind:value={sceneThreshold} />
            <span class="slider-value">{sceneThreshold.toFixed(2)}</span>
          </div>
        {:else}
          <div class="slider-row">
            <input type="range" disabled value="0" />
            <span class="slider-value">不適用</span>
          </div>
        {/if}

        <label>
          最大邊長限制
          <select bind:value={maxSide}>
            <option value={null}>不限制(原始尺寸)</option>
            <option value={3840}>3840px</option>
            <option value={1920}>1920px</option>
            <option value={1280}>1280px</option>
            <option value={800}>800px</option>
            <option value={480}>480px</option>
          </select>
        </label>

        <div class="time-range">
          <span class="time-range-label">時間區間(選填)</span>
          <div class="row">
            <button
              type="button"
              onclick={setStartFromPlayer}
              disabled={!selectedItem || !videoVisible || previewRunning || exportRunning}
              title={videoVisible ? undefined : "請先顯示影片"}
            >
              設為起點
            </button>
            <button
              type="button"
              onclick={setEndFromPlayer}
              disabled={!selectedItem || !videoVisible || previewRunning || exportRunning}
              title={videoVisible ? undefined : "請先顯示影片"}
            >
              設為終點
            </button>
          </div>
          <div class="row">
            <input
              type="text"
              class="time-input"
              placeholder="0:00"
              bind:value={startText}
              onblur={commitStartText}
            />
            <span class="meta">→</span>
            <input
              type="text"
              class="time-input"
              placeholder="結尾"
              bind:value={endText}
              onblur={commitEndText}
            />
            <button
              type="button"
              class="ghost"
              onclick={clearRange}
              disabled={previewRunning || exportRunning}
            >
              清除
            </button>
          </div>
        </div>
        {#if rangeError}
          <p class="error">{rangeError}</p>
        {/if}

        <div class="estimate-row">
          <span class="meta" class:warning={estimatedFrames !== null && estimatedFrames > 500}>
            {#if !selectedItem}
              請先選擇影片
            {:else if estimatedFrames === null}
              無法預估(依畫面變化而定)
            {:else}
              約 {estimatedFrames} 張{estimatedFrames > 500 ? "，數量較多，可能要等一下" : ""}
            {/if}
          </span>
          <button
            type="button"
            class="primary"
            onclick={runPreview}
            disabled={previewRunning || exportRunning || queue.length === 0 || !!rangeError}
          >
            <PlayIcon size={16} weight="fill" />
            {previewRunning
              ? `抽幀中…(${currentIndex + 1}/${queue.length})`
              : "開始抽幀"}
          </button>
        </div>

        {#if previewRunning}
          <div class="row">
            <div class="progress-bar">
              <div class="progress-fill" style="width: {progressPct.toFixed(1)}%"></div>
            </div>
            <button type="button" onclick={cancelPreview}>
              <StopIcon size={16} />
              取消
            </button>
          </div>
          <p class="meta">{progressPct.toFixed(0)}%</p>
        {/if}
      </div>

      <label class="checkbox-row">
        <input type="checkbox" bind:checked={autoClearPreview} />
        產生時自動清空舊預覽
      </label>
      <label class="checkbox-row">
        <input type="checkbox" bind:checked={applyToAll} />
        套用至全部（取消後每支影片可獨立設定抽幀參數）
      </label>

      <div class="actions">
        <button
          type="button"
          class="ghost danger"
          onclick={clearQueue}
          disabled={previewRunning || exportRunning || queue.length === 0}
        >
          <TrashIcon size={16} />
          清空佇列
        </button>
      </div>
    </aside>

    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <section class="col col-preview" class:drag-over={isDraggingOver} ondblclick={onColPreviewDblClick}>
      {#if selectedItem}
        {#if selectedItem.status === "error" || selectedItem.status === "info-error"}
          <p class="error banner">{statusLabel(selectedItem)}</p>
        {/if}
        <div class="player">
          <div class="player-media">
            {#if previewedFramePath}
              <button
                type="button"
                class="frame-preview"
                class:picking={pickingColor}
                class:retouching={retouchPanelOpen}
                onclick={handleFramePreviewClick}
                onmousedown={onFramePreviewMouseDown}
                onmousemove={onFramePreviewMouseMove}
                onmouseleave={onFramePreviewMouseLeave}
                title={pickingColor || retouchPanelOpen ? undefined : "點一下返回"}
              >
                <div
                  class="frame-preview-content"
                  style={previewNaturalSize ? `aspect-ratio:${previewNaturalSize.w}/${previewNaturalSize.h}` : ""}
                >
                  <img
                    bind:this={previewImgEl}
                    src={displaySrcFor(previewedFramePath)}
                    alt="抽出的幀"
                    onload={() => {
                      if (previewImgEl) {
                        previewNaturalSize = { w: previewImgEl.naturalWidth, h: previewImgEl.naturalHeight };
                      }
                    }}
                  />
                  {#if chromaKeyEnabled && !retouchPanelOpen}
                    <canvas bind:this={chromaPreviewCanvas} class="chroma-preview-canvas"></canvas>
                  {/if}
                  {#if retouchPanelOpen}
                    <canvas bind:this={retouchCanvas} class="retouch-canvas"></canvas>
                  {/if}
                  {#if previewedFrameNumber !== null}
                    <span class="thumb-badge frame-preview-badge">{previewedFrameNumber}</span>
                  {/if}
                </div>
              </button>
            {:else if videoVisible}
              <!-- svelte-ignore a11y_media_has_caption -->
              <video
                bind:this={videoEl}
                src={convertFileSrc(selectedItem.path)}
                ontimeupdate={() => (videoCurrentTime = videoEl?.currentTime ?? 0)}
                onloadedmetadata={() => (videoDuration = videoEl?.duration ?? 0)}
                onplay={() => (isPlaying = true)}
                onpause={() => (isPlaying = false)}
              ></video>
            {:else}
              <div class="player-idle">
                {#if hasFrames}
                  <img
                    src={displaySrcFor(activeResult(selectedItem)?.frames[0] ?? "")}
                    alt="抽出的幀"
                    class="player-idle-thumb"
                  />
                  <p class="meta">點下方縮圖查看</p>
                {:else}
                  <FilmSlateIcon size={32} />
                  <p class="meta">尚未抽幀</p>
                {/if}
              </div>
            {/if}

            {#if !previewedFramePath}
              <div class="video-toggle-zone">
                <button
                  type="button"
                  class="primary video-toggle"
                  class:auto-hide={isPlaying}
                  onclick={toggleVideoVisible}
                >
                  {videoVisible ? "隱藏影片" : "顯示影片"}
                </button>
              </div>
            {/if}

            {#if pickingColor}
              <div class="picking-hint">
                <span>點一下取色</span>
                <button type="button" class="icon-btn" onclick={cancelColorPicking} title="取消吸色">
                  <XCircleIcon size={16} />
                </button>
              </div>
              <div class="magnifier" bind:this={magnifierEl}>
                <canvas bind:this={magnifierCanvas} width={MAGNIFIER_SIZE} height={MAGNIFIER_SIZE}></canvas>
              </div>
            {/if}

            {#if chromaKeyPanelOpen}
              <div class="chroma-panel">
                <div class="chroma-panel-header">
                  <span>去背景</span>
                  <button type="button" class="icon-btn" onclick={closeChromaKeyPanel} title="收合面板">
                    －
                  </button>
                </div>
                <div class="chroma-panel-row">
                  <span class="swatch" style="background:{chromaKeyColor}"></span>
                  <button
                    type="button"
                    class="ghost"
                    onclick={armColorPicking}
                    disabled={!hasFrames}
                    title={hasFrames ? "點大圖吸取顏色" : "請先抽幀"}
                  >
                    吸取顏色
                  </button>
                </div>
                <div class="chroma-panel-row">
                  <span class="meta">相似度</span>
                  <input
                    type="range"
                    min="0"
                    max="100"
                    step="1"
                    value={Math.round(chromaKeySimilarity * 100)}
                    oninput={(e) => (chromaKeySimilarity = Number(e.currentTarget.value) / 100)}
                  />
                  <span class="meta similarity-value">{Math.round(chromaKeySimilarity * 100)}%</span>
                </div>
              </div>
            {/if}

            {#if retouchPanelOpen}
              <div class="retouch-cursor" bind:this={retouchCursorEl} class:restore={retouchMode === "restore"}></div>
              <div class="retouch-panel">
                <div class="chroma-panel-header">
                  <span>手動微調</span>
                  <button type="button" class="icon-btn" onclick={closeRetouchPanel} title="關閉">
                    －
                  </button>
                </div>
                <div class="chroma-panel-row">
                  <button
                    type="button"
                    class="retouch-mode-btn"
                    class:active={retouchMode === "erase"}
                    onclick={() => (retouchMode = "erase")}
                  >
                    橡皮擦
                  </button>
                  <button
                    type="button"
                    class="retouch-mode-btn"
                    class:active={retouchMode === "restore"}
                    onclick={() => (retouchMode = "restore")}
                  >
                    還原筆刷
                  </button>
                </div>
                <div class="chroma-panel-row">
                  <span class="meta">筆刷大小</span>
                  <input type="range" min="4" max="80" step="2" bind:value={retouchBrushSize} />
                  <span class="meta similarity-value">{retouchBrushSize}px</span>
                </div>
                <div class="chroma-panel-row">
                  <button
                    type="button"
                    class="icon-btn"
                    onclick={retouchUndo}
                    disabled={!retouchUndoAvailable}
                    title="復原 (Ctrl+Z)"
                  >
                    <ArrowCounterClockwiseIcon size={16} />
                  </button>
                  <button
                    type="button"
                    class="icon-btn"
                    onclick={retouchRedo}
                    disabled={!retouchRedoAvailable}
                    title="重做 (Ctrl+Y)"
                  >
                    <ArrowClockwiseIcon size={16} />
                  </button>
                </div>
                <div class="chroma-panel-row">
                  <button
                    type="button"
                    class="ghost"
                    onclick={retouchReset}
                    disabled={!retouchUndoAvailable && !retouchRedoAvailable}
                  >
                    重設
                  </button>
                  <button type="button" class="primary" onclick={saveRetouchedFrame} disabled={retouchSaving}>
                    {retouchSaving ? "存檔中…" : "套用並存檔"}
                  </button>
                </div>
              </div>
            {/if}
          </div>

          <div class="player-controls">
            {#if videoVisible && !previewedFramePath}
              <button type="button" class="icon-btn" title="倒退 5 秒" onclick={() => skipBy(-5)}>
                <RewindIcon size={16} />
              </button>
            {/if}
            <button
              type="button"
              class="icon-btn"
              title={isPlaying || framePlaying ? "暫停" : "播放"}
              onclick={togglePlayback}
            >
              {#if isPlaying || framePlaying}
                <PauseIcon size={18} weight="fill" />
              {:else}
                <PlayIcon size={18} weight="fill" />
              {/if}
            </button>
            {#if previewedFramePath}
              <span class="meta">
                {#if framePlaying}
                  {framePlaybackIndex + 1} 張／{framePlaybackTotal} 張
                {:else}
                  幀預覽，點大圖返回
                {/if}
              </span>
            {:else if videoVisible}
              <button type="button" class="icon-btn" title="快轉 5 秒" onclick={() => skipBy(5)}>
                <FastForwardIcon size={16} />
              </button>
              <span class="meta">{formatTime(videoCurrentTime)}</span>
              <input
                type="range"
                min="0"
                max={videoDuration || 0}
                step="0.1"
                bind:value={videoCurrentTime}
                oninput={seekTo}
              />
              <span class="meta">{formatTime(videoDuration)}</span>
            {:else}
              <span class="meta">
                {selectedFrames.size > 0 ? `已選 ${selectedFrames.size} 幀` : "點下方縮圖查看，或按「顯示影片」設定時間"}
              </span>
            {/if}
          </div>
        </div>
      {:else}
        <div
          class="empty-state"
          role="button"
          tabindex="0"
          onkeydown={(e) => e.key === "Enter" && pickVideos()}
          title="雙擊選擇影片"
        >
          <FilmSlateIcon size={32} />
          <p>選擇或拖曳影片開始</p>
        </div>
      {/if}
    </section>

    <aside class="col col-export">
      {#if selectedItem?.info}
        <div class="side-block">
          <h3>檔案資訊</h3>
          <dl class="info-list">
            <div><dt>解析度</dt><dd>{selectedItem.info.width}×{selectedItem.info.height}</dd></div>
            <div><dt>時長</dt><dd>{selectedItem.info.duration.toFixed(1)}s</dd></div>
            <div><dt>幀率</dt><dd>{selectedItem.info.fps.toFixed(2)}</dd></div>
            <div><dt>編碼</dt><dd>{selectedItem.info.codec_name}</dd></div>
            <div><dt>大小</dt><dd>{(selectedItem.info.file_size / 1_000_000).toFixed(1)} MB</dd></div>
          </dl>
        </div>
      {/if}

      <h3>導出設定</h3>
      <label>
        輸出資料夾
        <input placeholder="尚未選擇資料夾" bind:value={outputDir} readonly />
        <button type="button" onclick={pickOutputDir}>
          <FolderOpenIcon size={16} />
          選擇資料夾
        </button>
      </label>
      <label>
        輸出格式
        <select bind:value={format}>
          <option value="png">PNG</option>
          <option value="jpg">JPG</option>
        </select>
      </label>
      {#if chromaKeyEnabled}
        <p class="meta">去背景時，導出的圖會強制存成 PNG（不受此設定影響，需要透明度）</p>
      {/if}
      <label>
        檔名前綴
        <input type="text" bind:value={prefix} placeholder="frame" />
      </label>
      <label class="checkbox-row">
        <input type="checkbox" bind:checked={spriteSheet} />
        同時輸出精靈圖（單張幀數超過 {SPRITE_MAX_FRAMES} 張時會略過）
      </label>

      <button type="button" class="primary" onclick={runExport} disabled={!canExport}>
        <ExportIcon size={16} weight="bold" />
        {exportRunning ? `導出中…(${exportIndex + 1}/${exportTotal})` : "導出"}
      </button>
    </aside>
  </div>

  <div class="filmstrip-container">
    <div class="filmstrip-toolbar">
      <div class="toolbar-left">
        <div class="slider-row small">
          <input type="range" min="1" max="60" step="1" bind:value={framePlaybackFps} disabled={!hasFrames} />
          <span class="slider-value">{framePlaybackFps} fps</span>
        </div>
        <button type="button" class="ghost" onclick={selectAllFrames} disabled={!hasFrames}>
          全選
        </button>
        <button
          type="button"
          class="ghost"
          onclick={clearFrameSelection}
          disabled={selectedFrames.size === 0}
        >
          清空選取
        </button>
        <span class="meta">已選 {selectedFrames.size} 幀</span>
        <label class="checkbox-row">
          <input type="checkbox" bind:checked={followPlaybackScroll} />
          跟隨抽幀圖滾動
        </label>
        <span class="toolbar-divider"></span>
        <button
          type="button"
          class="ghost chroma-toggle"
          onclick={toggleChromaKeyPanel}
          disabled={!hasFrames}
          title={hasFrames ? "去背景" : "請先抽幀"}
        >
          <span class="status-dot" class:active={chromaKeyEnabled}></span>
          去背景
        </button>
        <button
          type="button"
          class="ghost chroma-toggle"
          onclick={toggleRetouchPanel}
          disabled={!hasFrames}
          title={hasFrames ? "手動微調" : "請先抽幀"}
        >
          <span class="status-dot" class:active={retouchPanelOpen}></span>
          手動微調
        </button>
      </div>
      <span class="meta hint">左鍵：加選／已選則取消 · Ctrl+左鍵：加選／已選則取消(可拖曳) · 右鍵：預覽</span>
    </div>

    <div class="filmstrip-row" bind:this={filmstripEl} onwheel={onFilmstripWheel} onscroll={onFilmstripScroll}>
      {#if hasFrames}
        {@const result = activeResult(selectedItem!)!}
        <div class="filmstrip">
          {#each result.frames as framePath, i (framePath)}
            <button
              type="button"
              class="thumb"
              class:active={previewedFramePath === framePath}
              class:selected={selectedFrames.has(framePath)}
              data-frame-index={i}
              onmousedown={(e) => onThumbMouseDown(e, framePath)}
              onmouseenter={() => onThumbMouseEnter(framePath)}
              onclick={(e) => onThumbClick(e, framePath)}
              oncontextmenu={(e) => onThumbContextMenu(e, framePath)}
            >
              <img
                src={displaySrcFor(framePath)}
                alt="frame {i + 1}"
                loading="lazy"
                draggable="false"
              />
              {#if chromaKeyEnabled}
                <canvas class="thumb-chroma-canvas" use:chromaThumb={framePath}></canvas>
              {/if}
              <span class="thumb-badge">{i + 1}</span>
              {#if selectedFrames.has(framePath)}
                <span class="thumb-check">✓</span>
              {/if}
            </button>
          {/each}
        </div>
      {:else}
        <p class="meta">尚未產生預覽</p>
      {/if}
    </div>
  </div>
</div>

<style>
  :root {
    --space-1: 0.25rem;
    --space-2: 0.5rem;
    --space-3: 0.75rem;
    --space-4: 1rem;
    --space-5: 1.5rem;
    --space-6: 2rem;

    --radius: 6px;

    --font-sans: -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;

    --color-bg: #f4f5f6;
    --color-surface: #ffffff;
    --color-border: #e0e2e5;
    --color-border-strong: #c9ccd1;
    --color-text: #16181c;
    --color-text-secondary: #6b7280;

    --accent: #0d9488;
    --accent-hover: #0f766e;
    --accent-active: #115e59;
    --accent-tint: rgba(13, 148, 136, 0.1);

    --color-success: #1f8b4c;
    --color-error: #c0392b;
    --color-warning: #b8860b;

    font-family: var(--font-sans);
    color: var(--color-text);
    background-color: var(--color-bg);
  }

  @media (prefers-color-scheme: dark) {
    :root {
      --color-bg: #17181a;
      --color-surface: #202226;
      --color-border: #2e3136;
      --color-border-strong: #3d4147;
      --color-text: #eceef0;
      --color-text-secondary: #9aa0a6;

      --accent: #2dd4bf;
      --accent-hover: #5eead4;
      --accent-active: #14b8a6;
      --accent-tint: rgba(45, 212, 191, 0.14);
    }
  }

  :global(html),
  :global(body) {
    margin: 0;
    padding: 0;
  }

  .app {
    display: grid;
    grid-template-rows: 4fr 1fr;
    height: 100vh;
    box-sizing: border-box;
    border: 2px dashed transparent;
    transition: border-color 0.15s, background-color 0.15s;
  }

  .app.drag-over {
    border-color: var(--accent);
    background-color: var(--accent-tint);
  }

  .top-row {
    display: grid;
    grid-template-columns: 15fr 70fr 15fr;
    min-height: 0;
    min-width: 0;
  }

  .col {
    min-width: 0;
    box-sizing: border-box;
    padding: var(--space-3);
    overflow-y: auto;
    overflow-x: hidden;
  }

  .col-queue {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    background: var(--color-surface);
    border-right: 1px solid var(--color-border);
  }

  .col-preview {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-2);
    border-right: 1px solid var(--color-border);
    border-top: 2px dashed transparent;
    border-bottom: 2px dashed transparent;
    border-left: 2px dashed transparent;
    transition: border-color 0.15s, background-color 0.15s;
  }

  .col-preview.drag-over {
    border-top-color: var(--accent);
    border-bottom-color: var(--accent);
    border-left-color: var(--accent);
    background-color: var(--accent-tint);
  }

  .col-preview .banner {
    width: 100%;
    box-sizing: border-box;
    flex-shrink: 0;
  }

  .col-export {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    background: var(--color-surface);
  }

  .col-export label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: 0.8rem;
    color: var(--color-text-secondary);
  }

  .col-export label.checkbox-row {
    flex-direction: row;
    align-items: center;
  }

  h1 {
    margin: 0;
    font-size: 1.1rem;
    font-weight: 600;
  }

  h3 {
    margin: 0 0 var(--space-1);
    font-size: 0.85rem;
    font-weight: 600;
    color: var(--color-text);
  }

  .meta {
    color: var(--color-text-secondary);
    font-size: 0.8rem;
  }

  .meta.warning {
    color: var(--color-warning);
  }

  .picker-row {
    display: flex;
    gap: var(--space-2);
  }

  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  input,
  select {
    width: 100%;
    box-sizing: border-box;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius);
    border: 1px solid var(--color-border-strong);
    background: var(--color-surface);
    color: var(--color-text);
    font-family: inherit;
    font-size: 0.85rem;
  }

  input[readonly] {
    background: var(--color-bg);
    color: var(--color-text-secondary);
    cursor: default;
  }

  input:focus-visible,
  select:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: var(--space-1);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius);
    border: 1px solid var(--color-border-strong);
    background: var(--color-surface);
    color: var(--color-text);
    font-family: inherit;
    font-size: 0.8rem;
    cursor: pointer;
    white-space: nowrap;
    transition: background-color 0.1s ease, border-color 0.1s ease, transform 0.1s ease;
  }

  button:hover:not(:disabled) {
    border-color: var(--accent);
  }

  button:active:not(:disabled) {
    transform: translateY(1px);
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  button:disabled {
    opacity: 0.5;
    cursor: default;
  }

  button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: white;
  }

  button.primary:hover:not(:disabled) {
    background: var(--accent-hover);
    border-color: var(--accent-hover);
  }

  button.primary:active:not(:disabled) {
    background: var(--accent-active);
    border-color: var(--accent-active);
  }

  button.ghost {
    background: transparent;
    border-color: transparent;
    color: var(--color-text-secondary);
  }

  button.ghost:hover:not(:disabled) {
    background: var(--color-bg);
    border-color: var(--color-border);
  }

  .picker-row button {
    flex: 1;
  }

  .error {
    color: var(--color-error);
    white-space: pre-wrap;
    margin: 0;
    font-size: 0.8rem;
  }

  /* Queue list */

  .queue-box {
    border: 1px dashed var(--color-border-strong);
    border-radius: var(--radius);
    overflow: hidden;
  }

  .queue-box .empty-hint {
    margin: 0;
    padding: var(--space-4) var(--space-3);
    text-align: center;
  }

  .queue {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 220px;
    overflow-y: auto;
  }

  .queue-entry {
    border-bottom: 1px solid var(--color-border);
  }

  .queue-entry:last-child {
    border-bottom: none;
  }

  .queue-row {
    display: flex;
    align-items: center;
  }

  .select-row {
    flex: 1;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
    padding: var(--space-2) var(--space-2) var(--space-2) var(--space-3);
    border: none;
    border-radius: 0;
    background: transparent;
    color: var(--color-text);
    text-align: left;
    justify-content: flex-start;
  }

  .select-row:hover:not(:disabled) {
    background: var(--color-bg);
    border-color: transparent;
  }

  .select-row:active:not(:disabled) {
    transform: none;
  }

  .queue-entry.selected .select-row {
    background: var(--accent-tint);
    box-shadow: inset 3px 0 0 var(--accent);
  }

  .status-icon {
    display: inline-flex;
    flex-shrink: 0;
  }

  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.8rem;
  }

  .row-actions {
    display: flex;
    padding-right: var(--space-1);
  }

  .icon-btn {
    padding: var(--space-1);
    border: none;
    background: transparent;
    color: var(--color-text-secondary);
  }

  .icon-btn:hover:not(:disabled) {
    background: var(--color-bg);
    border-color: transparent;
    color: var(--color-text);
  }

  button.danger:hover:not(:disabled) {
    color: var(--color-error);
    border-color: var(--color-error);
  }

  .checkbox-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 0.8rem;
    color: var(--color-text-secondary);
  }

  .checkbox-row input[type="checkbox"] {
    width: auto;
  }

  .status-previewing,
  .status-previewed,
  .status-exporting {
    color: var(--accent);
  }

  .status-done {
    color: var(--color-success);
  }

  .status-error,
  .status-info-error {
    color: var(--color-error);
  }

  .status-cancelled {
    color: var(--color-warning);
  }

  :global(.spin) {
    animation: spin 1s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .side-block {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .side-block label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: 0.8rem;
    color: var(--color-text-secondary);
  }

  .info-list {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .info-list div {
    display: flex;
    justify-content: space-between;
    font-size: 0.78rem;
  }

  .info-list dt {
    color: var(--color-text-secondary);
  }

  .info-list dd {
    margin: 0;
    color: var(--color-text);
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-top: auto;
    padding-top: var(--space-3);
    border-top: 1px solid var(--color-border);
  }

  .actions button {
    flex: 1 1 auto;
  }

  .progress-bar {
    flex: 1;
    height: 6px;
    border-radius: calc(var(--radius) / 2);
    background: var(--color-border);
    overflow: hidden;
  }

  .mode-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
  }

  .mode-btn {
    padding: var(--space-2);
    font-size: 0.78rem;
  }

  .mode-btn.active {
    background: var(--accent-tint);
    border-color: var(--accent);
    color: var(--accent);
  }

  .slider-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .slider-row input[type="range"] {
    flex: 1;
    padding: 0;
    border: none;
    background: transparent;
  }

  .slider-value {
    flex-shrink: 0;
    font-size: 0.78rem;
    color: var(--color-text-secondary);
    min-width: 4.5em;
    text-align: right;
  }

  .estimate-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }

  .time-range {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .time-range-label {
    font-size: 0.8rem;
    color: var(--color-text-secondary);
  }

  .time-range .row {
    margin: 0;
  }

  .time-input {
    width: 4.5em;
    flex: 0 0 auto;
    text-align: center;
  }

  .progress-fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.2s linear;
  }

  /* Preview column */

  .empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-2);
    color: var(--color-text-secondary);
    cursor: pointer;
  }

  .empty-state p {
    margin: 0;
    font-size: 0.9rem;
  }

  .player {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    width: 100%;
    flex: 1;
    min-height: 0;
  }

  .player-media {
    position: relative;
    display: flex;
    flex: 1;
    min-height: 0;
  }

  .player video {
    width: 100%;
    flex: 1;
    min-height: 0;
    border-radius: var(--radius);
    background: #000;
  }

  .player-idle {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-2);
    width: 100%;
    flex: 1;
    min-height: 0;
    color: var(--color-text-secondary);
  }

  .player-idle-thumb {
    max-width: 60%;
    max-height: 60%;
    border-radius: var(--radius);
    object-fit: contain;
  }

  .video-toggle-zone {
    position: absolute;
    top: 0;
    right: 0;
    width: 110px;
    height: 60px;
    display: flex;
    align-items: flex-start;
    justify-content: flex-end;
    padding: var(--space-2);
    z-index: 10;
  }

  .video-toggle {
    transition: transform 0.25s ease;
  }

  .video-toggle.auto-hide {
    transform: translateX(150%);
  }

  .video-toggle-zone:hover .video-toggle.auto-hide {
    transform: translateX(0);
  }

  .frame-preview {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 100%;
    flex: 1;
    min-height: 0;
    padding: 0;
    border: none;
    background: #fff;
    border-radius: var(--radius);
    cursor: pointer;
  }

  /* Sized to the image's own aspect ratio (inline style), so this box IS
     the rendered image's bounds — object-fit's letterbox gap no longer
     exists for a checkerboard/badge to spill into; .frame-preview's own
     white background shows for real aspect-ratio mismatches instead. */
  .frame-preview-content {
    position: relative;
    max-width: 100%;
    max-height: 100%;
  }

  .frame-preview-content img,
  .frame-preview-content canvas {
    display: block;
    width: 100%;
    height: 100%;
    border-radius: var(--radius);
  }

  .chroma-preview-canvas {
    position: absolute;
    inset: 0;
    pointer-events: none;
  }

  .picking-hint {
    position: absolute;
    top: var(--space-2);
    right: var(--space-2);
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: rgba(0, 0, 0, 0.75);
    border: 1.5px solid var(--accent);
    border-radius: var(--radius);
    padding: var(--space-1) var(--space-2);
    color: #fff;
    font-size: 0.8rem;
    pointer-events: none;
  }

  .picking-hint .icon-btn {
    pointer-events: auto;
    color: #fff;
  }

  .frame-preview.picking,
  .frame-preview.retouching {
    cursor: none;
  }

  .retouch-canvas {
    position: absolute;
    inset: 0;
    pointer-events: none;
    background-image: linear-gradient(45deg, #999 25%, transparent 25%),
      linear-gradient(-45deg, #999 25%, transparent 25%), linear-gradient(45deg, transparent 75%, #999 75%),
      linear-gradient(-45deg, transparent 75%, #999 75%);
    background-size: 12px 12px;
    background-position: 0 0, 0 6px, 6px -6px, -6px 0px;
    background-color: #ccc;
  }

  .retouch-cursor {
    position: fixed;
    border-radius: 50%;
    border: 2px solid #ff6b6b;
    pointer-events: none;
    z-index: 100;
    visibility: hidden;
  }

  .retouch-cursor.restore {
    border-color: #4ade80;
  }

  .retouch-panel {
    position: absolute;
    left: var(--space-2);
    bottom: var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 190px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    padding: var(--space-2);
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
  }

  .retouch-mode-btn {
    flex: 1;
    font-size: 0.75rem;
    padding: var(--space-1) 0;
  }

  .retouch-mode-btn.active {
    border-color: var(--accent);
    color: var(--accent);
  }

  .magnifier {
    position: fixed;
    width: 120px;
    height: 120px;
    border-radius: 50%;
    overflow: hidden;
    border: 2px solid #fff;
    box-shadow: 0 2px 10px rgba(0, 0, 0, 0.5);
    pointer-events: none;
    z-index: 100;
    visibility: hidden;
  }

  .magnifier canvas {
    width: 100%;
    height: 100%;
  }

  .chroma-panel {
    position: absolute;
    right: var(--space-2);
    bottom: var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 160px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    padding: var(--space-2);
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
  }

  .chroma-panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: 0.8rem;
    font-weight: 600;
  }

  .chroma-panel-header .icon-btn {
    padding: 0 var(--space-1);
  }

  .chroma-panel-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .chroma-panel-row input[type="range"] {
    width: 90px;
    flex: none;
    padding: 0;
    border: none;
    background: transparent;
  }

  .similarity-value {
    min-width: 34px;
    text-align: right;
  }

  .swatch {
    width: 18px;
    height: 18px;
    border-radius: 4px;
    border: 1px solid var(--color-border-strong);
    flex-shrink: 0;
  }

  .toolbar-divider {
    width: 1px;
    align-self: stretch;
    background: var(--color-border);
  }

  .chroma-toggle {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  button.chroma-toggle {
    /* button.ghost's own rule has the same specificity (element+class) and
       would otherwise win on source order alone, keeping this transparent. */
    border-color: var(--color-border);
  }

  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--color-border-strong);
  }

  .status-dot.active {
    background: var(--accent);
  }

  .player-controls {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .player-controls input[type="range"] {
    flex: 1;
  }

  /* Filmstrip */

  .filmstrip-container {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
    border-top: 1px solid var(--color-border);
    background: var(--color-surface);
  }

  .filmstrip-toolbar {
    flex-shrink: 0;
    box-sizing: border-box;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-border);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .toolbar-left {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  .slider-row.small {
    width: 140px;
  }

  .hint {
    white-space: nowrap;
  }

  .filmstrip-row {
    flex: 1;
    min-height: 0;
    min-width: 0;
    box-sizing: border-box;
    padding: var(--space-2) var(--space-3);
    overflow-x: auto;
    overflow-y: hidden;
    display: flex;
    align-items: center;
  }

  .filmstrip {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .filmstrip .thumb {
    flex-shrink: 0;
    width: 96px;
  }

  .thumb {
    position: relative;
    padding: 0;
    border: none;
    background: transparent;
  }

  .thumb img {
    width: 100%;
    aspect-ratio: 1;
    display: block;
    object-fit: cover;
    border-radius: var(--radius);
    border: 1px solid var(--color-border);
  }

  .thumb-chroma-canvas {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    aspect-ratio: 1;
    object-fit: cover;
    border-radius: var(--radius);
    pointer-events: none;
  }

  .thumb.active img {
    border-color: var(--accent);
    border-width: 2px;
  }

  .thumb.selected img {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .thumb-badge {
    position: absolute;
    top: 4px;
    left: 4px;
    min-width: 18px;
    height: 18px;
    padding: 0 4px;
    border-radius: 9px;
    background: rgba(0, 0, 0, 0.65);
    color: #fff;
    font-size: 0.7rem;
    line-height: 18px;
    text-align: center;
  }

  .frame-preview-badge {
    top: var(--space-2);
    left: var(--space-2);
    min-width: 26px;
    height: 26px;
    line-height: 26px;
    border-radius: 13px;
    font-size: 0.85rem;
  }

  .thumb-check {
    position: absolute;
    bottom: 4px;
    right: 4px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--accent);
    color: #fff;
    font-size: 0.7rem;
    line-height: 18px;
    text-align: center;
  }
</style>
