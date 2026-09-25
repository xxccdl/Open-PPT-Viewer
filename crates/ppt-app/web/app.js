/* OpenPPTView 前端逻辑
 *
 * 分工：像素由 Rust 产出，这里只负责「显示 + 标注 + 放映」。
 *
 * 三个关键设计：
 *
 * ① 双缓冲由「await 期间不清屏」天然实现。
 *    请求新页时，canvas 上仍是旧位图；新位图就绪后才整体重绘。
 *    因此无论渲染多慢，都不会出现白屏或半渲染画面。
 *
 * ② 标注坐标一律用「幻灯片 pt 空间」。
 *    缩放、平移、全屏切换都只改变「pt → 屏幕」的映射，
 *    标注数据本身不变，因此永远不会错位。
 *
 * ③ 标注层增量绘制。
 *    书写过程中每来一个采样点只画最后一段曲线，不做全量重绘 ——
 *    这既保证了跟手，也让「上百笔的页面」在书写时依然流畅。
 */

const T = window.__TAURI__;
const invoke = T.core.invoke;
const dlg = T.dialog;

/* ---------------- 状态 ---------------- */

const state = {
  /** 文档信息（open_document 的返回值）。 */
  info: null,
  /** 当前页（0 起）。 */
  page: 0,
  /** 每 pt 对应多少 CSS 像素。 */
  scale: 1,
  /** 用户额外的缩放倍数（相对适应窗口）。 */
  zoom: 1,
  /** 适应窗口的基准缩放。 */
  fitScale: 1,
  /** 双指平移的偏移（CSS 像素，相对居中位置）。 */
  panX: 0,
  panY: 0,
  /**
   * 当前工具：pointer / pen / highlighter / eraser。
   *
   * `pointer` 是放映模式的默认值 —— 此时单击用于翻页而非落笔（WPS 的行为）。
   * 窗口模式（编辑视图）里 `pointer` 与 `pen` 等价：点下去就该画。
   */
  tool: 'pen',
  color: '#e01b24',
  width: 3,
  /** 放映模式。 */
  presenting: false,
  /** 当前页的可点击热区（超链接 / 动作按钮），坐标在 pt 空间。 */
  links: [],
  /** 当前页可播放的媒体（视频/音频），同样在 pt 空间。 */
  media: [],
  /**
   * 媒体层是为哪一页建好的，`-1` 表示还没建。
   *
   * 用来判断「重新渲染当前页时要不要重建播放器」——见 `buildMediaLayer`。
   */
  mediaPage: -1,
  /**
   * 本页的动画序列（来自 `page_anim`），空数组表示这一页没有动画。
   *
   * 每项形如 `{ index, trigger, rect, shapeId, targets, exit, durMs }`：
   * `trigger` 是 `click`（点空白）/ `auto`（上一动画之后）/ `shape`（点指定形状）。
   */
  animSteps: [],
  /**
   * 已经播过的步号（从 1 起），**按播放先后**排列。
   *
   * 用列表而不是「推进到第几步」这一个数，是因为触发器动画允许跳着播 ——
   * 老师可能先点按钮播第 3 步，再点空白播第 1 步；单个数表达不了。
   * 后端会把它折成位掩码，这一点对这里是透明的。
   */
  played: [],
  /** 本页的转场（`p:transition`），翻到这一页时播。 */
  animTransition: null,
  /** 标注：page → Stroke[] */
  annotations: {},
  /** 每页的撤销/重做栈：page → { undo: Stroke[], redo: Stroke[] } */
  history: {},
  /** 当前是否已渲染过内容。 */
  hasContent: false,
  /** 计时器。 */
  timer: { running: false, startedAt: 0, elapsed: 0, id: 0 },
};

const COLORS = ['#e01b24', '#ff7800', '#f5c211', '#2ec27e', '#1c71d8', '#9141ac', '#1d1d1f', '#ffffff'];
const WIDTHS = [2, 3, 5, 8];

/** 一次笔画。 */
class Stroke {
  constructor(tool, color, width, opacity) {
    this.tool = tool;
    this.color = color;
    this.width = width;
    this.opacity = opacity;
    /** 点集：[x, y, pressure]，坐标在 pt 空间。 */
    this.points = [];
  }

  add(x, y, pressure) {
    this.points.push([x, y, pressure]);
  }

  get isEmpty() {
    return this.points.length === 0;
  }
}

/* ---------------- DOM ---------------- */

const $ = (id) => document.getElementById(id);

const el = {
  app: $('app'),
  stage: $('stage'),
  slide: $('slide-layer'),
  mediaLayer: $('media-layer'),
  board: $('board'),
  ink: $('ink-layer'),
  laser: $('laser'),
  thumbs: $('thumbs'),
  sidebar: $('sidebar'),
  toolbar: $('toolbar'),
  notes: $('notes'),
  notesBody: $('notes-body'),
  welcome: $('welcome'),
  recent: $('recent-list'),
  toast: $('toast'),
  spinner: $('spinner'),
  prepare: $('prepare'),
  spotlight: $('spotlight'),
  timer: $('timer'),
  fileName: $('file-name'),
  pageInput: $('page-input'),
  pageTotal: $('page-total'),
  stepBadge: $('step-badge'),
  /* 放映专用元素 */
  presentBar: $('present-bar'),
  presentDock: $('p-dock'),
  presentPage: $('present-page'),
  presentSwatches: $('p-swatches'),
  ctxMenu: $('ctx-menu'),
  fade: $('present-fade'),
  linkHint: $('link-hint'),
  /* 设置 */
  settings: $('settings'),
  settingsDot: $('settings-dot'),
  setVersion: $('set-version'),
  setUpdateHint: $('set-update-hint'),
  setNotes: $('set-notes'),
  setNotesBody: $('set-notes-body'),
  updateBar: $('update-bar'),
  updateBarFill: $('update-bar-fill'),
  setCacheTotal: $('set-cache-total'),
  setCacheDetail: $('set-cache-detail'),
  btnDoUpdate: $('btn-do-update'),
};

const slideCtx = el.slide.getContext('2d');
const inkCtx = el.ink.getContext('2d');
const laserCtx = el.laser.getContext('2d');

/* ---------------- 提示 ---------------- */

let toastTimer = 0;

function toast(message, isError = false) {
  el.toast.textContent = message;
  el.toast.classList.toggle('error', isError);
  el.toast.classList.add('on');
  clearTimeout(toastTimer);
  // 报错文案要交代「为什么 + 下一步」，往往是一整句话；
  // 固定 5.2 秒时老师读到一半它就消失了，按长度给时间（并封顶，免得赖着不走）
  const ms = isError ? Math.min(12000, Math.max(5200, message.length * 180)) : 2400;
  toastTimer = setTimeout(() => el.toast.classList.remove('on'), ms);
}

/** 加载指示：延迟 150ms 才显示，避免快机器上闪一下。 */
let spinnerTimer = 0;

function showSpinner() {
  clearTimeout(spinnerTimer);
  spinnerTimer = setTimeout(() => el.spinner.classList.add('on'), 150);
}

function hideSpinner() {
  clearTimeout(spinnerTimer);
  el.spinner.classList.remove('on');
}

/* ---------------- 视图尺寸 ---------------- */

function stageSize() {
  return { w: el.stage.clientWidth, h: el.stage.clientHeight };
}

/** 重新计算适应窗口的缩放。 */
function recomputeFit() {
  if (!state.info) return;
  const { w, h } = stageSize();
  const pad = 48;
  const sx = (w - pad) / state.info.widthPt;
  const sy = (h - pad) / state.info.heightPt;
  state.fitScale = Math.max(0.02, Math.min(sx, sy));
  state.scale = state.fitScale * state.zoom;
}

/** 把两层画布按当前缩放摆好。 */
function layoutCanvases() {
  if (!state.info) return;
  const dpr = window.devicePixelRatio || 1;
  const cssW = state.info.widthPt * state.scale;
  const cssH = state.info.heightPt * state.scale;

  // 黑板也在这一组里：它必须和标注层**完全重合**，
  // 否则会出现「看得见的板子」和「写得上去的范围」对不上
  for (const c of [el.slide, el.mediaLayer, el.board, el.ink]) {
    c.style.width = `${cssW}px`;
    c.style.height = `${cssH}px`;
    // 居中 + 平移全部交给同一个 transform（见 style.css）。
    // 窗口模式与放映模式共用，杜绝两套定位逻辑各自跑偏
    c.style.setProperty('--pan-x', `${state.panX}px`);
    c.style.setProperty('--pan-y', `${state.panY}px`);
  }

  // 标注层用设备像素分辨率，保证笔迹在高分屏上锐利
  const inkW = Math.max(1, Math.round(cssW * dpr));
  const inkH = Math.max(1, Math.round(cssH * dpr));
  if (el.ink.width !== inkW || el.ink.height !== inkH) {
    el.ink.width = inkW;
    el.ink.height = inkH;
  }
  el.laser.width = window.innerWidth;
  el.laser.height = window.innerHeight;

  // 缩放/平移变了，媒体播放器要跟着挪
  layoutMediaBoxes();
}

/** 可视区域尺寸（放映时是整个屏幕，窗口模式是舞台）。 */
function viewportSize() {
  return state.presenting
    ? { w: window.innerWidth, h: window.innerHeight }
    : stageSize();
}

/**
 * 约束平移范围。
 *
 * 只要幻灯片比可视区大，就允许把它拖到任意位置「看局部」；
 * 但必须留一块在屏幕内，否则松手后幻灯片会彻底消失、老师以为崩了。
 */
function clampPan() {
  if (!state.info) return;
  const { w, h } = viewportSize();
  const cssW = state.info.widthPt * state.scale;
  const cssH = state.info.heightPt * state.scale;
  // 放大到超出可视区时按溢出量的一半限制（居中坐标系的边界）
  const maxX = Math.max(0, (cssW - w) / 2);
  const maxY = Math.max(0, (cssH - h) / 2);
  state.panX = clamp(state.panX, -maxX, maxX);
  state.panY = clamp(state.panY, -maxY, maxY);
}

/**
 * 计算放映模式下「适应屏幕」的缩放比。
 *
 * 与窗口模式的差别：放映时铺满整屏、不留边距（窗口模式要留 48px 呼吸空间）。
 */
function recomputeFitPresenting() {
  if (!state.info) return;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const sx = vw / state.info.widthPt;
  const sy = vh / state.info.heightPt;
  // 取较小者：保证整页可见（contain），多余部分补黑边
  state.fitScale = Math.max(0.02, Math.min(sx, sy));
  state.scale = state.fitScale * state.zoom;
}

/** pt → 标注层画布像素。 */
function ptToInk(x, y) {
  const dpr = window.devicePixelRatio || 1;
  return [x * state.scale * dpr, y * state.scale * dpr];
}

/** 屏幕坐标 → pt 空间。 */
function screenToPt(clientX, clientY) {
  const rect = el.ink.getBoundingClientRect();
  return [(clientX - rect.left) / state.scale, (clientY - rect.top) / state.scale];
}

/* ---------------- 页面渲染 ---------------- */

/** 当前帧的位图（用于重绘与缩放时避免重新请求）。 */
let frameBitmap = null;

/**
 * 最近一次翻页请求的序号。
 *
 * 后端命令并发执行，响应不保证按发出顺序返回；`showPage` 用它丢弃晚到的旧结果。
 */
let pageRequestSeq = 0;

/** 上一帧是用多大的分辨率渲染的（pt → 设备像素的总倍数）。 */
let lastRenderScale = 0;

/**
 * 把后端回传的帧转成 ImageBitmap。
 *
 * 两种载荷：
 *
 * - **原始像素帧**：`[宽 u32le][高 u32le][直通 RGBA8…]`
 * - **PNG 文件原样**：办公软件已经写好的那些帧（逐页出图、动画的每一步）
 *
 * # 为什么后者走 PNG 反而更快
 *
 * 那些图本来就是 PNG、就躺在磁盘上。「读出来 → 解码 → 缩放 → 转预乘 →
 * 按原始像素传 7.9MB」每一步都是白花的：后端开销几乎为零、传输量小一个
 * 数量级，解码与缩放交给浏览器（有 GPU）比在后端做快得多。
 * 老师点的每一下、翻的每一页基本都走这条路。
 *
 * 两者靠头 4 字节分辨：PNG 固定以 `89 50 4E 47` 开头，而原始帧的头 4 字节
 * 是宽 —— 要撞上得有 11.9 亿像素宽。
 *
 * 原始帧收的是**直通** alpha，后端已经替我们还原过预乘，
 * 所以这里可以直接构造 `ImageData`，不需要再算。
 */
function decodeFrame(bytes) {
  const u8 = toUint8(bytes);
  if (u8.byteLength >= 4 && u8[0] === 0x89 && u8[1] === 0x50 && u8[2] === 0x4e && u8[3] === 0x47) {
    return createImageBitmap(new Blob([u8], { type: 'image/png' }));
  }
  if (u8.byteLength < 8) return Promise.reject(new Error('帧数据过短'));
  const view = new DataView(u8.buffer, u8.byteOffset, 8);
  const w = view.getUint32(0, true);
  const h = view.getUint32(4, true);
  if (w === 0 || h === 0) return Promise.reject(new Error('帧尺寸为空'));
  const rgba = new Uint8ClampedArray(u8.buffer, u8.byteOffset + 8, w * h * 4);
  return createImageBitmap(new ImageData(rgba, w, h));
}

/**
 * 后端约定的「这一页还在生成中」前缀（见 Rust 的 `NOT_READY_PREFIX`）。
 *
 * Tauri 的命令错误只能是一个字符串，所以用这个不可能出现在正常文案里的
 * 控制字符把「正常等待」和「真故障」区分开。
 */
const NOT_READY_PREFIX = '\u0001not-ready\u0001';

/** 后端是在说「这一页还在生成中」，而不是「这一页渲染不出来」。 */
function isNotReady(e) {
  return typeof e === 'string' && e.startsWith(NOT_READY_PREFIX);
}

/** 取「还在生成」那句话本身（去掉约定前缀）。 */
function notReadyMessage(e) {
  return String(e).slice(NOT_READY_PREFIX.length);
}

/**
 * 「还没轮到这一页」时我们多久重试一次、最多重试多久。
 *
 * 后端只等 250ms 就回话（死等会占住它的工作线程，几个缩略图就能把
 * 线程占满，老师翻页就得排队）；剩下的等待由这里轮询，**轮询不占后端线程**。
 * 所以间隔取得比较密：150ms 一次，翻到没出好的页也只觉得「顿一下」。
 */
const NOT_READY_RETRIES = 60;
const NOT_READY_RETRY_MS = 150;

/**
 * 请求一页的位图。
 *
 * `played` 是「已经播过的步号」；不传表示全部播完 ——
 * 静态语境（首屏、缩略图、导出）都走这条路径。
 * `hide` 是要在这一帧里**摘掉**的形状（强调动画的底帧要用）。
 *
 * # 「这一页还在生成中」不是错误
 *
 * 画面由本机办公软件逐页产出，老师翻到某一页时它可能刚好还没轮到。
 * 这时候**转圈等一下**就对了；报红叉、或者偷偷换成自研渲染的画，
 * 都是错的（后者正是「显示的还是自研内容」的来源）。
 */
async function requestFrame(page, scale, played, hide) {
  const args = { page, scale };
  if (played) args.played = played;
  if (hide && hide.length) args.hide = hide;
  for (let attempt = 0; ; attempt++) {
    try {
      const bytes = await invoke('render_page', args);
      return decodeFrame(bytes);
    } catch (e) {
      if (!isNotReady(e) || attempt >= NOT_READY_RETRIES) {
        // 等不到了：**一定要把转圈收掉**。
        // 有的调用方（推进动画的 `renderPlayed`）没有自己的 finally，
        // 少了这一行就会留下一个永远转下去的圈。
        if (attempt > 0) hideSpinner();
        throw e;
      }
      showSpinner();
      await sleep(NOT_READY_RETRY_MS);
    }
  }
}

/**
 * 请求并显示一页。
 *
 * `mode` 决定动画从哪儿开始，这是「放映」与「翻阅」的分界：
 *
 * - `start`：讲新的一页，一步都不播（放映模式下点出来的页）
 * - `all`：这一页讲完了的样子（窗口模式翻阅、回翻、跳页、缩略图）
 * - `keep`：停在原来的步上（窗口缩放、切主题这类重渲染）
 *
 * 请求期间 canvas 保持旧内容，因此不会白屏。
 */
async function showPage(page, { silent = false, mode = 'start', transition = true } = {}) {
  if (!state.info) return;
  if (page < 0 || page >= state.info.pageCount) return;

  const dpr = window.devicePixelRatio || 1;
  // 渲染分辨率 = pt × CSS 缩放 × 设备像素比
  const renderScale = Math.max(0.05, state.scale * dpr);

  // 翻页请求序号。
  //
  // 后端命令是**并发执行**的（否则重渲染会卡住 UI 线程），
  // 所以「先发的请求先回」并不成立：连翻几页时，第 4 页的结果完全可能
  // 晚于第 5 页到达。少了这道守卫，画面就会**倒着退回去**——
  // 老师看到的是「页序乱了」。
  //
  // 只有序号仍是当前值的那次请求才有资格改动画布，晚到的一律丢弃。
  const seq = ++pageRequestSeq;
  // 同样是「谁在画」的序号：它管的是动画，`seq` 管的是请求。
  // 老师手快时，上一步的动画必须给新的一次绘制让位（见 `paintSeq`）。
  const myPaint = ++paintSeq;
  const alive = () => paintSeq === myPaint;

  if (!silent) showSpinner();

  try {
    // 先拿本页的动画序列与转场：首屏该停在第几步、翻进来该演什么转场，
    // 都取决于这一页作者是怎么设的，所以必须先问清楚再渲染
    const anim = await invoke('page_anim', { page }).catch(() => null);
    if (seq !== pageRequestSeq) return;
    const steps = (anim && anim.steps) || [];
    const pageTransition = (anim && anim.transition) || null;

    const played =
      mode === 'keep'
        ? state.played.slice()
        : mode === 'all'
          ? steps.map((s) => s.index)
          : [];

    const bitmap = await requestFrame(page, renderScale, played);
    // 等渲染的这段时间里老师可能又翻了几页，那这一帧已经过期了
    if (seq !== pageRequestSeq) {
      bitmap.close();
      return;
    }
    const prev = frameBitmap;

    // 只在成功拿到新帧后才替换，保证「无白屏」
    frameBitmap = bitmap;
    lastRenderScale = renderScale;

    state.page = page;
    state.animSteps = steps;
    state.animTransition = pageTransition;
    state.played = played;
    state.hasContent = true;
    hideLinkHint();
    layoutCanvases();
    paintFrame();
    redrawInk();
    updateNavUi();
    updateStepBadge();
    updateNotes(page);
    highlightThumb(page);

    // 放映时每次翻页都提示页码（与 WPS 一致）
    if (state.presenting) showPresentPage();

    // 转场：把这一页「演进来」。旧帧要留到演完才能释放。
    if (transition && prev && !silent && mode !== 'keep') {
      await playTransition(pageTransition, prev, bitmap, alive);
    }
    // 旧帧该由这一帧来关。但**不能关掉此刻正在屏幕上的那一张** ——
    // 老师手快时更晚的那次绘制可能已经把 `frameBitmap` 换成别的了，
    // 而它正拿我们这张位图当底帧在用（关掉它会让对方画出黑屏）。
    if (prev && prev.close && prev !== frameBitmap) prev.close();

    // 作者设了「自动前进」就到点自己翻页（没设则什么都不做）
    armAutoAdvance();

    // 让后端预热相邻页（不阻塞当前渲染）
    invoke('prefetch_around', { page }).catch(() => {});

    // 交互热区：取不到就当本页没有链接（PDF 与多数课件确实没有），
    // 用页号做守卫，避免快速翻页时旧响应覆盖新页的热区
    invoke('page_links', { page })
      .then((links) => {
        if (state.page === page) state.links = links || [];
      })
      .catch(() => {
        state.links = [];
      });

    // 视频/音频播放器（本页没有就什么都不做）
    buildMediaLayer(page);
  } catch (e) {
    toast(
      isNotReady(e)
        ? `${notReadyMessage(e)}（稍等片刻再翻回来即可）`
        : `无法显示第 ${page + 1} 页：${e}`,
      true,
    );
  } finally {
    if (!silent) hideSpinner();
  }
}

/** 把当前帧画到画布上。 */
function paintFrame() {
  if (!frameBitmap) return;
  // 位图可能已经被另一次绘制 `close()` 掉（老师手快时），它的宽高会变成 0。
  // 拿它去画只会把画布也清成 0×0 —— 之后怎么翻页都是黑的。
  if (!frameBitmap.width || !frameBitmap.height) return;
  const w = el.slide.clientWidth || frameBitmap.width;
  const h = el.slide.clientHeight || frameBitmap.height;

  // 用设备像素尺寸作为画布内部尺寸，避免二次插值
  if (el.slide.width !== frameBitmap.width || el.slide.height !== frameBitmap.height) {
    el.slide.width = frameBitmap.width;
    el.slide.height = frameBitmap.height;
  }
  slideCtx.clearRect(0, 0, el.slide.width, el.slide.height);
  slideCtx.drawImage(frameBitmap, 0, 0, el.slide.width, el.slide.height);
  void w;
  void h;
}

/** 缩放/窗口变化后重绘当前帧（无需重新请求）。 */
function repaintAtScale() {
  layoutCanvases();
  paintFrame();
  redrawInk();
  scheduleRerender();
}

/**
 * 分辨率变化较大时重新渲染当前页。
 *
 * `paintFrame()` 只是把旧位图按新的 CSS 尺寸铺开 —— 放大窗口或进入全屏时
 * 位图会被插值放大而发虚。这里以 6% 为阈值判断「值得重渲染」，
 * 并用定时器合并窗口拖拽过程中的连续变化。
 */
let rerenderTimer = 0;

function scheduleRerender() {
  if (!state.info || !state.hasContent) return;
  clearTimeout(rerenderTimer);
  rerenderTimer = setTimeout(() => {
    const dpr = window.devicePixelRatio || 1;
    const next = Math.max(0.05, state.scale * dpr);
    if (lastRenderScale > 0 && Math.abs(next - lastRenderScale) / lastRenderScale < 0.06) return;
    // 停在原来的步上：窗口缩放不该把讲了一半的动画退回开头
    showPage(state.page, { silent: true, mode: 'keep', transition: false });
  }, 220);
}

/** 后端可能返回 ArrayBuffer 或数字数组，统一成 Uint8Array。 */
function toUint8(bytes) {
  if (bytes instanceof Uint8Array) return bytes;
  if (bytes instanceof ArrayBuffer) return new Uint8Array(bytes);
  if (Array.isArray(bytes)) return new Uint8Array(bytes);
  // Tauri 2 在某些平台返回 { 0: .., 1: .. } 形式的对象
  if (bytes && typeof bytes === 'object') return new Uint8Array(Object.values(bytes));
  return new Uint8Array(0);
}

/* ---------------- 标注层绘制 ---------------- */

/** 当前正在书写的笔画。 */
let activeStroke = null;
/** 上一次绘制到的点索引（用于增量绘制）。 */
let drawnUpTo = 0;

/** 是否在黑板上。 */
let boardOn = false;
/** 黑板上翻到第几页（0 开始）。 */
let boardPage = 0;
/** 上板前的笔色（深色会看不清，下板要还回去）。 */
let penColorBeforeBoard = '';

/** 黑板最多几页。不封顶的话，按住「下一页」就会一直长下去。 */
const BOARD_MAX_PAGES = 20;

/**
 * 黑板的笔迹记在哪一格。
 *
 * 标注是按「页」存的（`state.annotations[页码]`）。黑板不是课件的哪一页，
 * 于是给它一串**字符串格子**（`board1`、`board2`…，板书也是多页的）：
 * 撤销、清空、橡皮、重绘全都照旧按 `inkKey()` 取，不必给黑板另写一套
 * 笔迹逻辑；保存课件时和其它标注一起落盘（见 `saveAnnotations`），
 * 所以板书是**跟着课件走**的 —— 换个班上课打开同一份课件，板书还在。
 */
function boardSlot(n) {
  return `board${n + 1}`;
}

/** 这个键是不是黑板的格子。 */
function isBoardSlot(key) {
  return /^board\d+$/.test(key);
}

/** 当前笔迹写在哪一格：平时是页码，上板时是板书的那一页。 */
function inkKey() {
  return boardOn ? boardSlot(boardPage) : state.page;
}

function strokesFor(page) {
  if (!state.annotations[page]) state.annotations[page] = [];
  return state.annotations[page];
}

/** 全量重绘标注层。 */
function redrawInk() {
  inkCtx.setTransform(1, 0, 0, 1, 0, 0);
  inkCtx.clearRect(0, 0, el.ink.width, el.ink.height);
  const list = strokesFor(inkKey());
  for (const s of list) drawStroke(inkCtx, s, 0);
  if (activeStroke) drawStroke(inkCtx, activeStroke, 0);
  drawnUpTo = activeStroke ? activeStroke.points.length : 0;
}

/** 绘制一条笔画（`from` 之前的点跳过，用于增量绘制）。 */
function drawStroke(ctx, stroke, from) {
  const pts = stroke.points;
  if (pts.length === 0) return;

  const dpr = window.devicePixelRatio || 1;
  const unit = state.scale * dpr;

  ctx.save();
  ctx.globalAlpha = stroke.opacity;
  ctx.strokeStyle = stroke.color;
  ctx.lineCap = 'round';
  ctx.lineJoin = 'round';

  if (pts.length === 1) {
    // 单点：画一个圆点（轻点也应留下痕迹）
    const [x, y, p] = pts[0];
    const r = (stroke.width * (0.4 + p * 0.9) * unit) / 2;
    ctx.fillStyle = stroke.color;
    ctx.beginPath();
    ctx.arc(x * unit, y * unit, Math.max(0.6, r), 0, Math.PI * 2);
    ctx.fill();
    ctx.restore();
    return;
  }

  ctx.lineWidth = stroke.width * unit;

  const startIdx = Math.max(1, from);
  if (startIdx === 1) {
    // 首段
    ctx.beginPath();
    ctx.moveTo(pts[0][0] * unit, pts[0][1] * unit);
  } else {
    // 增量：从上一段的起点继续，保证曲线连续
    ctx.beginPath();
    const i0 = Math.max(0, startIdx - 2);
    const mid = midpoint(pts[i0], pts[i0 + 1]);
    ctx.moveTo(mid[0] * unit, mid[1] * unit);
  }

  // 用「相邻点中点」作为二次贝塞尔的端点，得到平滑曲线
  for (let i = startIdx; i < pts.length; i++) {
    const prev = pts[i - 1];
    const cur = pts[i];
    const mid = midpoint(prev, cur);
    ctx.quadraticCurveTo(prev[0] * unit, prev[1] * unit, mid[0] * unit, mid[1] * unit);
  }
  ctx.stroke();
  ctx.restore();
}

function midpoint(a, b) {
  return [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2];
}

/* ---------------- 输入（鼠标 / 触摸 / 手写笔） ---------------- */

/** 活跃指针：id → { x, y, type }，用于双指手势与「笔优先」判定。 */
const pointers = new Map();
/** 手势状态。 */
const gesture = {
  active: false,
  startDist: 0,
  startZoom: 1,
  /** 双指中点与当时的平移量（用于把「拖动」直接映射成平移）。 */
  startMid: [0, 0],
  startPan: [0, 0],
  lastX: 0,
  lastY: 0,
  panning: false,
};

/* ---------------- 掌托抑制 ---------------- */

/**
 * 最近一次手写笔事件的时间戳。
 *
 * 触控一体机上老师写字时手掌会先落在屏上，若不抑制就会在课件上
 * 留下大片笔迹。判定用「时间窗 + 接触面积」两条，
 * 而不是给每根手指单独记状态 —— 手掌的落点每次都不一样。
 */
let lastPenAt = 0;
/** 手写笔抬起后仍继续抑制触摸的时间窗。 */
const PEN_GRACE_MS = 1200;
/** 接触宽度达到这个值（CSS 像素）只可能是手掌，手指通常远小于它。 */
const PALM_CONTACT_PX = 46;

/** 被判定为手掌、整条生命期都要忽略的指针。 */
const ignoredPointers = new Set();

function notePen() {
  lastPenAt = performance.now();
}

function penRecentlyActive() {
  return performance.now() - lastPenAt < PEN_GRACE_MS;
}

/**
 * 这个指针是否应当被当作手掌丢弃。
 *
 * 只对触摸生效：鼠标与手写笔永远可信。
 * `width/height` 在部分环境下为 0（无法取得接触面积），此时按 0 处理 ——
 * 宁可漏判手掌，也不能把所有触摸都吃掉。
 */
function isPalmLike(e) {
  if (e.pointerType !== 'touch') return false;
  if (penRecentlyActive()) return true;
  const contact = Math.max(e.width || 0, e.height || 0);
  return contact >= PALM_CONTACT_PX;
}

function currentToolConfig() {
  switch (state.tool) {
    case 'highlighter':
      // 荧光笔半透明、更粗
      return { opacity: 0.34, widthScale: 3.2, erase: false };
    case 'eraser':
      return { opacity: 1, widthScale: 3, erase: true };
    default:
      // pointer 与 pen 同款笔迹：窗口模式下 pointer 也应当能画
      return { opacity: 1, widthScale: 1, erase: false };
  }
}

el.ink.addEventListener('pointerdown', (e) => {
  if (!state.hasContent) return;

  // 手掌（或手写笔书写期间落下的手指）：整条生命期都忽略，
  // 既不落笔，也不参与双指手势判定
  if (isPalmLike(e) || ignoredPointers.has(e.pointerId)) {
    ignoredPointers.add(e.pointerId);
    return;
  }

  if (e.pointerType === 'pen') {
    notePen();
    // 「掌托先落、笔后落」是写字时最常见的顺序。若此时把手指算作第二指，
    // 就会被当成双指手势 —— 老师一落笔画面反而缩放了。
    // 因此笔一落下，立刻把屏上还按着的触摸请出去。
    for (const [id, p] of [...pointers]) {
      if (p.type === 'touch') {
        ignoredPointers.add(id);
        pointers.delete(id);
      }
    }
    if (activeStroke) {
      activeStroke = null;
      redrawInk();
    }
  }

  pointers.set(e.pointerId, { x: e.clientX, y: e.clientY, type: e.pointerType });

  // 双指：进入手势模式，并丢弃可能刚开始的笔画
  if (pointers.size === 2) {
    if (activeStroke) {
      activeStroke = null;
      redrawInk();
    }
    beginGesture();
    return;
  }
  if (pointers.size > 2) return;

  // 「鼠标」模式不落笔：先记下按下点，抬起时再判定是翻页、点链接还是长按。
  // 两种模式都适用 —— 窗口模式下点形状上的超链接同样应该跳转
  if (!isDrawingTool()) {
    tapPending = {
      id: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      t: performance.now(),
    };
    if (state.presenting) onLongPressStart(e);
    return;
  }

  // 中键或 Alt + 左键 = 平移
  if (e.button === 1 || (e.button === 0 && e.altKey)) {
    gesture.panning = true;
    gesture.lastX = e.clientX;
    gesture.lastY = e.clientY;
    el.ink.classList.add('panning');
    el.ink.setPointerCapture(e.pointerId);
    return;
  }

  // 橡皮：按下即擦，之后拖动持续擦。
  // 触摸屏上没有「按下不动」的余地，必须支持拖着擦，否则一体机上根本擦不干净
  if (state.tool === 'eraser') {
    erasing = true;
    el.ink.setPointerCapture(e.pointerId);
    eraseAt(e.clientX, e.clientY);
    return;
  }

  el.ink.setPointerCapture(e.pointerId);
  startStroke(e);
});

/** 放映时待判定的轻点（用于区分「单击/滑动翻页」与「长按出菜单」）。 */
let tapPending = null;

/** 正在拖动擦除。 */
let erasing = false;

el.ink.addEventListener('pointermove', (e) => {
  if (ignoredPointers.has(e.pointerId)) return;
  if (e.pointerType === 'pen') notePen();
  if (!state.hasContent) return;

  if (pointers.has(e.pointerId)) {
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
  }

  // 长按菜单：手指一挪动就说明本意不是长按，及时取消
  if (tapPending && tapPending.id === e.pointerId) {
    if (Math.hypot(e.clientX - tapPending.x, e.clientY - tapPending.y) > 12) {
      onLongPressEnd();
    }
  }

  // 平移
  if (gesture.panning) {
    const dx = e.clientX - gesture.lastX;
    const dy = e.clientY - gesture.lastY;
    gesture.lastX = e.clientX;
    gesture.lastY = e.clientY;
    el.stage.scrollLeft -= dx;
    el.stage.scrollTop -= dy;
    return;
  }

  // 双指缩放 + 平移
  if (gesture.active && pointers.size >= 2) {
    updateGesture();
    return;
  }

  // 橡皮拖动：合并事件保证快速划过时也能连成一片
  if (erasing) {
    const events = e.getCoalescedEvents ? e.getCoalescedEvents() : [e];
    for (const ev of events) eraseAt(ev.clientX, ev.clientY);
    return;
  }

  // 聚光灯跟随
  if (el.spotlight.classList.contains('on')) {
    const rect = el.stage.getBoundingClientRect();
    el.spotlight.style.setProperty('--spot-x', `${e.clientX - rect.left}px`);
    el.spotlight.style.setProperty('--spot-y', `${e.clientY - rect.top}px`);
  }

  // 激光笔
  if (laserActive) {
    pushLaser(e.clientX, e.clientY);
  }

  if (activeStroke) {
    // 合并事件：手写笔高频采样下，浏览器会把多个点打包进一个事件，
    // 只取主事件会丢失细节、笔迹发折线
    const events = e.getCoalescedEvents ? e.getCoalescedEvents() : [e];
    for (const ev of events) {
      const [x, y] = screenToPt(ev.clientX, ev.clientY);
      const p = ev.pressure > 0 ? ev.pressure : 0.5;
      activeStroke.add(x, y, p);
    }
    // 增量绘制：只画新增段
    drawStroke(inkCtx, activeStroke, drawnUpTo);
    drawnUpTo = activeStroke.points.length;
  }
});

/** 指针结束（抬起 / 取消 / 离开）。只负责清理书写与手势状态。 */
function endPointer(e) {
  pointers.delete(e.pointerId);
  ignoredPointers.delete(e.pointerId);
  if (e.pointerType === 'pen') notePen();

  if (gesture.panning && pointers.size === 0) {
    gesture.panning = false;
    el.ink.classList.remove('panning');
    return;
  }

  if (gesture.active && pointers.size < 2) {
    gesture.active = false;
    el.ink.classList.remove('pan-mode');
  }

  erasing = false;

  if (activeStroke) {
    finishStroke();
  }
}

el.ink.addEventListener('pointerup', endPointer);
el.ink.addEventListener('pointercancel', endPointer);
el.ink.addEventListener('pointerleave', (e) => {
  if (activeStroke || erasing) endPointer(e);
});

/* ---------------- 超链接 / 动作按钮 ----------------
 *
 * 解析层早已把超链接与「动作按钮」解析出来（含 `ppaction://` 的各种跳转），
 * 但一直没有出口，放映时点按钮毫无反应。这里是补上的那一段。
 *
 * 只在**放映模式**响应：窗口模式里点击就是落笔（老师在那儿标注），
 * 若同一击既写字又跳页，就没法安心讲解了。这与 WPS 的「指针/画笔」分工一致。
 */

/** 该屏幕坐标命中的热区；未命中返回 null。 */
function linkAt(clientX, clientY) {
  if (!state.links.length) return null;
  const [px, py] = screenToPt(clientX, clientY);
  // 从后往前找：后画的形状在上层，与 z 序一致
  for (let i = state.links.length - 1; i >= 0; i--) {
    const l = state.links[i];
    if (px >= l.x && px <= l.x + l.w && py >= l.y && py <= l.y + l.h) return l;
  }
  return null;
}

/** 把提示框摆到热区上（热区是 pt，提示框用视口坐标）。 */
function showLinkHint(link) {
  const rect = el.ink.getBoundingClientRect();
  el.linkHint.style.left = `${rect.left + link.x * state.scale}px`;
  el.linkHint.style.top = `${rect.top + link.y * state.scale}px`;
  el.linkHint.style.width = `${link.w * state.scale}px`;
  el.linkHint.style.height = `${link.h * state.scale}px`;
  el.linkHint.title = link.url || link.tooltip || '';
  el.linkHint.classList.add('on');
}

function hideLinkHint() {
  el.linkHint.classList.remove('on');
}

/** 悬停时更新提示：拿起笔、正在书写、正在做手势时都不提示。 */
function updateLinkHover(x, y) {
  if (!state.presenting || isDrawingTool() || activeStroke || gesture.active) {
    hideLinkHint();
    return;
  }
  const hit = linkAt(x, y);
  if (hit) showLinkHint(hit);
  else hideLinkHint();
}

/** 执行一个热区的动作。 */
function followLink(link) {
  hideLinkHint();
  closeCtxMenu();

  switch (link.kind) {
    case 'slide':
      if (typeof link.slide === 'number') goTo(link.slide);
      break;
    case 'next':
      goForward();
      break;
    case 'prev':
      goBack();
      break;
    case 'first':
      goTo(0);
      break;
    case 'last':
      goTo(state.info ? state.info.pageCount - 1 : 0);
      break;
    case 'endShow':
      setPresenting(false);
      break;
    case 'url':
      // 白名单与注入防护都在 Rust 侧（课件是不可信输入）
      invoke('open_external', { url: link.url }).catch((e) => toast(String(e), true));
      break;
    default:
      // OtherFile 只存了 ppaction 原文，没有可用的路径；
      // 与其打开一个错误的东西，不如明确告知
      toast('该链接指向外部文件，暂不支持在放映中打开');
      break;
  }
}

/* ---------------- 内嵌视频 / 音频 ----------------
 *
 * 静态画面由**封页图**负责：PPT 里的视频就挂在一个图片形状上，
 * `blipFill` 是封面帧，图片链路已经把它画对了。
 * 这里只做「点一下原位播放」。
 *
 * 为什么把字节导出成磁盘文件而不是直接丢给前端：
 * 一节课的视频几十上百 MB，塞进 JS 内存既慢又容易把老机器顶爆；
 * 走资产协议（`convertFileSrc`）则是流式读取，还能拖动进度。
 */

/** 清掉当前页的播放器（翻页、退出时调用）。 */
function teardownMedia() {
  for (const p of el.mediaLayer.querySelectorAll('video, audio')) {
    try {
      p.pause();
      p.removeAttribute('src');
      p.load();
    } catch {
      /* 元素已失效，忽略 */
    }
  }
  el.mediaLayer.innerHTML = '';
  state.media = [];
  state.mediaPage = -1;
}

/** 按当前缩放把媒体框摆到热区上。 */
function layoutMediaBoxes() {
  const boxes = el.mediaLayer.children;
  const n = Math.min(boxes.length, state.media.length);
  for (let i = 0; i < n; i++) {
    const m = state.media[i];
    const box = boxes[i];
    box.style.left = `${m.x * state.scale}px`;
    box.style.top = `${m.y * state.scale}px`;
    box.style.width = `${m.w * state.scale}px`;
    box.style.height = `${m.h * state.scale}px`;
  }
}

/** 拉取本页媒体并生成播放器。 */
async function buildMediaLayer(page) {
  // 同一页只是重新渲染（缩放窗口、进出全屏）时**不要重建播放器**。
  //
  // 重建会 `load()` 掉正在播的视频，而视频一旦是全屏元素，
  // `load()` 就会让它退出全屏 —— 表现出来正是「视频刚全屏就被踢出来」：
  // 窗口尺寸一变就触发重渲染（见 `scheduleRerender`），而进全屏本身就是一次尺寸变化。
  // 这种情况只需要把框按新尺寸摆一遍，播放器原封不动。
  if (state.mediaPage === page) {
    layoutMediaBoxes();
    return;
  }

  teardownMedia();
  try {
    const items = (await invoke('page_media', { page })) || [];
    // 翻页竞态：慢响应回来时可能已经翻到别的页了
    if (state.page !== page) return;
    state.media = items;
  } catch {
    return; // 没有媒体、或读取失败，都不该影响看课件
  }
  // 记下「这一页的播放器已经建好了」（本页没有媒体也记，省一次往返）
  state.mediaPage = page;

  for (const m of state.media) {
    const box = document.createElement('div');
    box.className = m.kind === 'audio' ? 'media-box audio' : 'media-box';

    const btn = document.createElement('button');
    btn.className = 'media-play';
    btn.title = m.kind === 'audio' ? '播放音频' : '播放视频';
    btn.innerHTML =
      '<svg width="22" height="22" viewBox="0 0 24 24" fill="currentColor"><use href="#i-play" /></svg>';
    box.appendChild(btn);

    // `preload="none"`：不点就不下载，避免刚翻到这页就拉几十 MB
    const player = document.createElement(m.kind === 'audio' ? 'audio' : 'video');
    player.setAttribute('preload', 'none');
    player.setAttribute('playsinline', '');
    player.controls = true;
    if (m.loopPlay) player.setAttribute('loop', '');
    player.addEventListener('error', () => {
      // 只在真的尝试加载过之后才报错，避免 preload=none 时的空 src 误报
      if (player.getAttribute('src')) {
        toast('这段视频无法解码播放（编码格式不受支持）', true);
      }
    });
    box.appendChild(player);

    // 监听挂在整块热区上而不是按钮上：PowerPoint 里点视频画面的任意位置
    // 都会开始播放，老师不会精准去找那个小三角。
    box.addEventListener('click', (e) => {
      e.stopPropagation();
      startMedia(box, player, m);
    });

    el.mediaLayer.appendChild(box);
  }

  layoutMediaBoxes();
}

/** 导出媒体并开始播放。 */
async function startMedia(box, player, item) {
  if (!player.getAttribute('src')) {
    try {
      const path = await invoke('extract_media', { part: item.part });
      player.setAttribute('src', convertFileSrc(path));
      applyMediaTrim(player, item);
    } catch (e) {
      toast(`无法取出该媒体：${e}`, true);
      return;
    }
  }
  // 播完之后再点一次，要从裁剪起点重来 ——
  // 回到 0 的话放的是老师特意裁掉的片头
  const start = (item.trimStartMs || 0) / 1000;
  if (start > 0 && (player.ended || player.currentTime < start)) {
    player.currentTime = start;
  }
  box.classList.add('playing');
  try {
    await player.play();
  } catch (e) {
    toast(`播放失败：${e}`, true);
  }
}

/**
 * 让播放器只放「裁剪」之后的那一段。
 *
 * PowerPoint 的「裁剪视频」不改动原文件，只在 `p14:trim` 里记一段毫秒区间。
 * 课件里真有把 121 MB 的视频裁成 5 秒的用法 —— 照着区间放，
 * 才是老师在 PowerPoint 里预览到的那几秒；从头放到尾放的是完全不同的内容。
 *
 * `loop` 打开时回到**区间起点**而不是 0，否则每循环一次都要重放一遍被裁掉的片头。
 */
function applyMediaTrim(player, item) {
  const start = (item.trimStartMs || 0) / 1000;
  const end = item.trimEndMs ? item.trimEndMs / 1000 : Infinity;
  if (start === 0 && end === Infinity) return; // 没裁剪过，别白挂监听

  // 元数据到手之前 currentTime 设不进去，起播定位只能等这一刻
  player.addEventListener('loadedmetadata', () => {
    if (player.currentTime < start) player.currentTime = start;
  });
  player.addEventListener('timeupdate', () => {
    if (player.currentTime < end) return;
    if (player.hasAttribute('loop')) {
      player.currentTime = start;
    } else {
      player.pause();
    }
  });
}

/** 本地文件路径 → WebView 可访问的 URL。
 *
 * 走 Tauri 的资产协议：流式读取、可拖动进度。
 * 兜底分支是怕某天全局 API 没开，按资产协议的格式自己拼一个。
 */
function convertFileSrc(path) {
  if (T.core && T.core.convertFileSrc) return T.core.convertFileSrc(path);
  return `http://asset.localhost/${encodeURIComponent(path)}`;
}

/**
 * 「鼠标」模式下抬起指针：判定翻页 / 点链接 / 关菜单。
 *
 * 绑在 `window` 而不是标注层：放映时四周的 letterbox 黑边也要能翻页 ——
 * 投影比例与课件不一致时黑边很宽，老师点到那儿「没反应」会以为程序卡了。
 *
 * 分工：
 * - 横向滑动 → 翻页（仅放映模式；触屏的直觉操作）
 * - 命中热区 → 执行超链接/动作按钮（两种模式都响应）
 * - 轻点 → 放映模式下一页；窗口模式不动（那儿翻页走缩略图/按钮/滚轮）
 */
function onGlobalPointerUp(e) {
  // 这一下只是「点空白处关掉菜单」，不该顺带翻页
  const dismissedMenu = menuDismissAtDown;
  menuDismissAtDown = false;
  if (dismissedMenu) return;
  // 拿笔时这一击就是笔迹，不参与任何点击语义
  if (isDrawingTool()) return;
  if (el.presentBar.contains(e.target) || el.ctxMenu.contains(e.target)) return;
  if (el.toolbar.contains(e.target)) return;
  // 点在播放器上：那是播放器的控件（进度条、音量），既不该翻页也不该跳链接
  if (el.mediaLayer.contains(e.target)) return;

  const pending = tapPending && tapPending.id === e.pointerId ? tapPending : null;
  onLongPressEnd();
  // 没在幻灯片上按下（例如点在黑边上）：只有放映模式的鼠标左键才算「下一页」
  if (!pending && !(state.presenting && e.pointerType === 'mouse' && e.button === 0)) {
    return;
  }
  tapPending = null;

  if (pending) {
    const dx = e.clientX - pending.x;
    const dy = e.clientY - pending.y;
    const dt = performance.now() - pending.t;

    if (state.presenting) {
      const swiped = dt < 700 && Math.abs(dx) > 70 && Math.abs(dx) > Math.abs(dy) * 1.4;
      if (swiped) {
        closeCtxMenu();
        dx < 0 ? goForward() : goBack();
        return;
      }
    }

    // 命中超链接/动作按钮：执行链接动作，而不是翻页。
    // 这一条必须在「热区唤出工具条」与「单击翻页」之前判断 ——
    // 否则课件里带链接的按钮就永远点不动
    const hit = linkAt(pending.x, pending.y);
    if (hit) {
      followLink(hit);
      return;
    }

    // 触发器动画：点的是作者指定的那个形状，就先播它那一步，而不是翻页。
    // 放在链接之后 —— 一个形状同时挂了链接与触发器时，链接更「明确」。
    if (state.presenting) {
      const [px, py] = screenToPt(pending.x, pending.y);
      const trig = triggerAt(px, py);
      if (trig) {
        playStep(trig);
        return;
      }
    }

    // 窗口模式到此为止：点击不是翻页
    if (!state.presenting) return;

    // 触摸落在屏幕最底边：老师是在够工具条，不该顺手翻页。
    //
    // 只对**鼠标为主的设备**这么做 —— 触摸屏上工具条是常驻的，
    // 这一下不可能是「够工具条」，吞掉它反而变成「点了没反应」。
    if (!isTouchMode() && e.pointerType === 'touch' && inPresentHotzone(e.clientY)) {
      return;
    }
  }

  // 长按已经弹出菜单：本次手势用完了
  if (state.presenting && el.ctxMenu.classList.contains('on')) return;

  closeCtxMenu();
  goForward();
}

window.addEventListener('pointerup', onGlobalPointerUp);

/** 开始一条新笔画。 */
function startStroke(e) {
  const cfg = currentToolConfig();
  // pointer 只是「不落笔」的放映态，真正画下来时一律记为画笔
  const tool = state.tool === 'pointer' ? 'pen' : state.tool;
  const stroke = new Stroke(tool, state.color, state.width * cfg.widthScale, cfg.opacity);
  const [x, y] = screenToPt(e.clientX, e.clientY);
  stroke.add(x, y, e.pressure > 0 ? e.pressure : 0.5);
  activeStroke = stroke;
  drawnUpTo = 0;
}

/** 结束并提交一条笔画。 */
function finishStroke() {
  const stroke = activeStroke;
  activeStroke = null;
  if (!stroke || stroke.isEmpty) return;

  const list = strokesFor(inkKey());
  list.push(stroke);

  // 新操作使重做栈失效
  const h = historyFor(inkKey());
  h.redo.length = 0;
  h.undo.push(stroke);

  redrawInk();
  updateToolButtons();
  scheduleAutoSave();
}

/* ---------------- 橡皮 ---------------- */

/** 逐笔擦除：删除命中点的笔画。 */
function eraseAt(clientX, clientY) {
  const [px, py] = screenToPt(clientX, clientY);
  const list = strokesFor(inkKey());
  // 手指比鼠标「粗」，判定半径放大，否则触屏上要擦好几次才中
  const threshold = Math.max(8, state.width * 4);

  for (let i = list.length - 1; i >= 0; i--) {
    const hit = list[i];
    if (strokeHit(hit, px, py, threshold)) {
      list.splice(i, 1);
      const h = historyFor(inkKey());
      h.undo = h.undo.filter((s) => s !== hit);
      redrawInk();
      scheduleAutoSave();
      return;
    }
  }
}

function strokeHit(stroke, x, y, threshold) {
  const t2 = threshold * threshold;
  const pts = stroke.points;
  for (let i = 0; i < pts.length; i++) {
    const dx = pts[i][0] - x;
    const dy = pts[i][1] - y;
    if (dx * dx + dy * dy <= t2) return true;
  }
  return false;
}

/* ---------------- 双指手势 ---------------- */

function beginGesture() {
  const pts = [...pointers.values()];
  if (pts.length < 2) return;
  gesture.active = true;
  gesture.startDist = Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
  gesture.startZoom = state.zoom;
  // 记录双指中点：既能当缩放的原点，也能直接当作平移的抓手
  gesture.startMid = [(pts[0].x + pts[1].x) / 2, (pts[0].y + pts[1].y) / 2];
  gesture.startPan = [state.panX, state.panY];
  el.ink.classList.add('pan-mode');
}

function updateGesture() {
  const pts = [...pointers.values()];
  if (pts.length < 2 || gesture.startDist <= 0) return;

  const dist = Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
  const ratio = dist / gesture.startDist;
  const nextZoom = clamp(gesture.startZoom * ratio, 0.2, 8);

  const midX = (pts[0].x + pts[1].x) / 2;
  const midY = (pts[0].y + pts[1].y) / 2;
  const nextPanX = gesture.startPan[0] + (midX - gesture.startMid[0]);
  const nextPanY = gesture.startPan[1] + (midY - gesture.startMid[1]);

  // 触摸事件的抖动比鼠标大得多，设个死区，避免手指没动画面却一直重绘
  const zoomMoved = Math.abs(nextZoom - state.zoom) / state.zoom >= 0.01;
  const panMoved =
    Math.abs(nextPanX - state.panX) >= 1 || Math.abs(nextPanY - state.panY) >= 1;
  if (!zoomMoved && !panMoved) return;

  state.zoom = nextZoom;
  state.panX = nextPanX;
  state.panY = nextPanY;
  fitAndRepaint();
}

/** 缩放变化后通知后端切换档位（作废旧预取）。 */
let scaleSyncTimer = 0;

function scheduleScaleSync() {
  clearTimeout(scaleSyncTimer);
  scaleSyncTimer = setTimeout(() => {
    if (!state.info) return;
    invoke('set_scale', { scale: state.scale }).catch(() => {});
  }, 260);
}

function clamp(v, lo, hi) {
  return v < lo ? lo : v > hi ? hi : v;
}

/* ---------------- 激光笔 ---------------- */

let laserActive = false;
let laserTrail = [];
let laserRaf = 0;

function pushLaser(clientX, clientY) {
  const rect = el.stage.getBoundingClientRect();
  laserTrail.push({ x: clientX - rect.left, y: clientY - rect.top, t: performance.now() });
  if (laserTrail.length > 40) laserTrail.shift();
  if (!laserRaf) laserRaf = requestAnimationFrame(paintLaser);
}

function paintLaser() {
  laserRaf = 0;
  const now = performance.now();
  laserTrail = laserTrail.filter((p) => now - p.t < 420);

  laserCtx.clearRect(0, 0, el.laser.width, el.laser.height);
  if (laserTrail.length === 0) return;

  for (let i = 0; i < laserTrail.length; i++) {
    const p = laserTrail[i];
    const age = (now - p.t) / 420;
    const alpha = Math.max(0, 1 - age);
    const r = 7 * (1 - age * 0.55);
    laserCtx.beginPath();
    laserCtx.fillStyle = `rgba(255, 59, 48, ${alpha * 0.85})`;
    laserCtx.arc(p.x, p.y, Math.max(1, r), 0, Math.PI * 2);
    laserCtx.fill();
  }
  if (laserTrail.length > 1) laserRaf = requestAnimationFrame(paintLaser);
}

/* ---------------- 动画推进 ---------------- */

/** 自动接续（「上一动画之后」）的步与上一步之间的停顿。 */
const AUTO_ANIM_MS = 260;

/** 一步动画的时长下限：作者没写时长（`dur="1"`）时按瞬变的观感兜底。 */
const MIN_ANIM_MS = 180;

/** 时长上限：异常课件写个几十秒会把放映卡住。 */
const MAX_ANIM_MS = 5000;

/** 转场默认时长（作者没写 `p14:dur` 也没写 `spd` 时）。 */
const TRANSITION_DEFAULT_MS = 500;

let animTimer = 0;

/**
 * 竞态守卫。
 *
 * 连点空格时会有多个渲染请求在飞，回来晚的那个不能覆盖新的。
 */
let stepSeq = 0;

/** 缓出。线性插值看着很「机械」，一眼就是程序画的。 */
function easeOut(t) {
  return 1 - Math.pow(1 - t, 3);
}

/** 把时长夹到合理区间；`0`/`1` 这种「瞬变」按兜底值走。 */
function animDuration(durMs) {
  if (!durMs || durMs <= 1) return MIN_ANIM_MS;
  return Math.min(Math.max(durMs, 60), MAX_ANIM_MS);
}

/**
 * 当前「谁在画这块画布」的序号。
 *
 * 老师手快时，上一步的动画还没演完就点了下一步（甚至翻页）。这时旧动画
 * **必须立刻停手**：两个动画往同一块画布上画会互相覆盖，而且旧动画要画的
 * 位图可能刚被新的一次绘制 `close()` 掉 —— 一旦 `drawImage` 抛异常，
 * 那个 Promise 就永远不会 settle，画布停在「已清空、还没画出来」的样子。
 * 老师看到的「全屏播放时黑屏」就是这么来的。
 */
let paintSeq = 0;

/**
 * 在一段时长里逐帧重画。
 *
 * `draw(progress)` 每帧被调用一次（progress 已缓动，∈ [0,1]），
 * 画之前会清空画布 —— 于是每个 draw 只需描述「这一帧整体长什么样」。
 *
 * `alive()` 返回 `false`（被新的一次绘制接管）时立刻停手。
 * **这个 Promise 必须无论如何都会 settle**：卡住它就等于卡住调用方的
 * `finally`，黑场遮罩/转圈都会跟着留在屏幕上。
 */
function animateFor(durMs, draw, alive = null) {
  return new Promise((resolve) => {
    const w = el.slide.width;
    const h = el.slide.height;
    const still = () => !alive || alive();

    const frame = (raw) => {
      slideCtx.clearRect(0, 0, w, h);
      draw(easeOut(raw));
    };

    if (durMs <= 0) {
      if (still()) frame(1);
      resolve();
      return;
    }
    const start = performance.now();
    const tick = (now) => {
      if (!still()) {
        resolve();
        return;
      }
      const raw = Math.min(1, (now - start) / durMs);
      try {
        frame(raw);
      } catch (e) {
        // 画不出来（多半是位图被另一次绘制关掉了）就停在这一帧。
        // 硬撑下去只会让 Promise 永不 settle。
        console.warn('动画帧绘制失败，停在这里', e);
        resolve();
        return;
      }
      if (raw < 1) requestAnimationFrame(tick);
      else resolve();
    };
    requestAnimationFrame(tick);
  });
}

/**
 * 把当前帧交叉淡入成新帧。
 *
 * 新帧里包含了旧帧的全部内容，只是多了（或少了）这一步该动的对象；
 * 因此「旧帧在下、新帧以 α 渐显在上」出来的效果就是**只有这一步的对象**在动，
 * 其余部分纹丝不动 —— 不需要逐形状的图层也能得到正确的观感。
 */
async function crossFadeTo(bitmap, durMs, alive = null) {
  const old = frameBitmap;
  const w = el.slide.width || bitmap.width;
  const h = el.slide.height || bitmap.height;
  if (!old) return;
  await animateFor(
    durMs,
    (p) => {
      slideCtx.drawImage(old, 0, 0, w, h);
      slideCtx.globalAlpha = p;
      slideCtx.drawImage(bitmap, 0, 0, w, h);
      slideCtx.globalAlpha = 1;
    },
    alive
  );
}

/* ---------------- 幻灯片转场 ---------------- */

/**
 * 播一次页间转场。
 *
 * 旧帧与新帧都在手上，所以这纯粹是「两张图怎么合成」的问题 ——
 * 不需要后端参与，也就不受 IPC 往返的帧率限制，稳定 60fps。
 *
 * 方向词表见 `TransitionDir`：`l/r/u/d` 表示**新内容从哪一侧进来**
 * （`l` 从左侧进、旧内容向右退出），与 PowerPoint 的「推入 · 自左侧」对应。
 */
async function playTransition(t, oldBmp, nextBmp, alive = null) {
  const kind = t && typeof t.kind === 'string' ? t.kind : 'fade';
  if (!t || kind === 'cut') return;

  const w = el.slide.width || nextBmp.width;
  const h = el.slide.height || nextBmp.height;
  const dur = Math.min(Math.max(t.durMs || TRANSITION_DEFAULT_MS, 120), MAX_ANIM_MS);
  const dir = t.dir || 'none';

  // 新内容从哪一侧进来
  const vec =
    {
      left: [-1, 0],
      right: [1, 0],
      up: [0, -1],
      down: [0, 1],
      leftUp: [-1, -1],
      rightUp: [1, -1],
      leftDown: [-1, 1],
      rightDown: [1, 1],
    }[dir] || [0, 0];

  /** 画旧帧（作为底）。 */
  const base = () => slideCtx.drawImage(oldBmp, 0, 0, w, h);
  /** 直接铺新帧。 */
  const full = () => slideCtx.drawImage(nextBmp, 0, 0, w, h);
  /** 新帧以 α 渐显。 */
  const fadeNew = (a) => {
    slideCtx.globalAlpha = a;
    slideCtx.drawImage(nextBmp, 0, 0, w, h);
    slideCtx.globalAlpha = 1;
  };
  /** 用裁剪区域揭示新帧。 */
  const reveal = (buildPath) => {
    slideCtx.save();
    slideCtx.beginPath();
    buildPath(slideCtx);
    slideCtx.clip();
    slideCtx.drawImage(nextBmp, 0, 0, w, h);
    slideCtx.restore();
  };

  // 当前进度。`bands` 要按「第几段、段内进度」算局部时间，进度放在闭包里
  // 比层层传参清楚
  let window_p = 0;

  /** 分成 n 段、每段在自己的时间窗里完成 —— 百叶窗、梳理都靠它。 */
  const bands = (n, buildPath) => {
    for (let i = 0; i < n; i++) {
      const span = 1 / n;
      const local = Math.min(1, Math.max(0, (window_p - i * span) / span));
      if (local > 0) reveal(buildPath(i, local));
    }
  };

  // `thruBlk`（经由黑场）：前半段淡出到黑，后半段再从黑淡入
  if (t.throughBlack) {
    await animateFor(
      dur,
      (p) => {
        if (p < 0.5) {
          slideCtx.globalAlpha = 1 - p * 2;
          slideCtx.drawImage(oldBmp, 0, 0, w, h);
          slideCtx.globalAlpha = 1;
        } else {
          slideCtx.globalAlpha = (p - 0.5) * 2;
          slideCtx.drawImage(nextBmp, 0, 0, w, h);
          slideCtx.globalAlpha = 1;
        }
      },
      alive
    );
    return;
  }

  await animateFor(dur, (p) => {
    window_p = p;
    switch (kind) {
      case 'push':
        // 旧帧被推着退场，新帧紧跟着进来
        slideCtx.drawImage(oldBmp, -vec[0] * w * p, -vec[1] * h * p, w, h);
        slideCtx.drawImage(nextBmp, vec[0] * w * (1 - p), vec[1] * h * (1 - p), w, h);
        break;

      case 'cover':
        // 新帧盖着旧帧滑进来
        base();
        slideCtx.drawImage(nextBmp, vec[0] * w * (1 - p), vec[1] * h * (1 - p), w, h);
        break;

      case 'uncover':
        // 新帧在底下不动，旧帧滑走把它露出来
        full();
        slideCtx.drawImage(oldBmp, -vec[0] * w * p, -vec[1] * h * p, w, h);
        break;

      case 'wipe':
      case 'reveal':
      case 'pan': {
        base();
        if (dir === 'right') reveal((c) => c.rect(w * (1 - p), 0, w * p, h));
        else if (dir === 'up') reveal((c) => c.rect(0, 0, w, h * p));
        else if (dir === 'down') reveal((c) => c.rect(0, h * (1 - p), w, h * p));
        else reveal((c) => c.rect(0, 0, w * p, h));
        break;
      }

      case 'split': {
        base();
        const vert = dir === 'vertOut' || dir === 'vertIn';
        const outward = dir === 'horzOut' || dir === 'vertOut';
        if (vert) {
          const half = h / 2;
          if (outward) {
            reveal((c) => {
              c.rect(0, half - half * p, w, half * p);
              c.rect(0, half, w, half * p);
            });
          } else {
            const keep = half * (1 - p);
            reveal((c) => {
              c.rect(0, half - keep, w, keep);
              c.rect(0, half, w, keep);
            });
          }
        } else {
          const half = w / 2;
          if (outward) {
            reveal((c) => {
              c.rect(half - half * p, 0, half * p, h);
              c.rect(half, 0, half * p, h);
            });
          } else {
            const keep = half * (1 - p);
            reveal((c) => {
              c.rect(half - keep, 0, keep, h);
              c.rect(half, 0, keep, h);
            });
          }
        }
        break;
      }

      case 'zoom':
      case 'window': {
        // 从中心长大的矩形把新帧露出来
        const k = kind === 'window' ? 0.15 + 0.85 * p : 0.3 + 0.7 * p;
        base();
        reveal((c) => c.rect((w * (1 - k)) / 2, (h * (1 - k)) / 2, w * k, h * k));
        break;
      }

      case 'blinds': {
        base();
        const n = 10;
        const alongX = dir !== 'up' && dir !== 'down';
        bands(n, (i, local) =>
          alongX
            ? (c) => c.rect(((i + local) * w) / n - w / n, 0, w / n, h)
            : (c) => c.rect(0, ((i + local) * h) / n - h / n, w, h / n),
        );
        break;
      }

      case 'comb': {
        base();
        const n = 12;
        const alongX = dir !== 'up' && dir !== 'down';
        bands(n, (i, local) =>
          alongX
            ? (c) => c.rect((i * w) / n, 0, w / n, h * local)
            : (c) => c.rect(0, (i * h) / n, w * local, h / n),
        );
        break;
      }

      case 'checker': {
        base();
        const n = 10;
        const m = 8;
        const order = (i, j) => (i + j) / (n + m - 2);
        for (let i = 0; i < n; i++) {
          for (let j = 0; j < m; j++) {
            const t0 = order(i, j) * 0.6;
            if (p > t0) {
              const local = Math.min(1, (p - t0) / 0.4);
              reveal((c) => c.rect((i * w) / n, (j * h) / m, (w / n) * local, (h / m) * local));
            }
          }
        }
        break;
      }

      case 'strips': {
        // 斜向的条带：按「沿对角线」的顺序逐条揭示（近似）
        base();
        const n = 12;
        for (let i = 0; i < n; i++) {
          const t0 = (i / n) * 0.7;
          if (p <= t0) continue;
          const local = Math.min(1, (p - t0) / 0.3);
          reveal((c) => {
            c.save();
            c.translate(-w * 0.3, -h * 0.3);
            c.rotate(-Math.PI / 5);
            const total = w * 1.6;
            c.rect((i * total) / n, 0, (total / n) * local, h * 2);
            c.restore();
          });
        }
        break;
      }

      case 'wheel': {
        base();
        const spokes = Math.max(1, t.spokes || 4);
        const slice = (Math.PI * 2) / spokes;
        for (let i = 0; i < spokes; i++) {
          const t0 = i / spokes;
          if (p <= t0) continue;
          const local = Math.min(1, (p - t0) * spokes);
          reveal((c) => {
            c.moveTo(w / 2, h / 2);
            c.arc(w / 2, h / 2, Math.hypot(w, h), i * slice, i * slice + slice * local);
            c.closePath();
          });
        }
        break;
      }

      case 'circle': {
        base();
        reveal((c) => c.arc(w / 2, h / 2, (Math.hypot(w, h) / 2) * p, 0, Math.PI * 2));
        break;
      }

      case 'diamond': {
        base();
        const rx = (w / 2) * p;
        const ry = (h / 2) * p;
        reveal((c) => {
          c.moveTo(w / 2, h / 2 - ry);
          c.lineTo(w / 2 + rx, h / 2);
          c.lineTo(w / 2, h / 2 + ry);
          c.lineTo(w / 2 - rx, h / 2);
        });
        break;
      }

      case 'plus': {
        base();
        reveal((c) => {
          c.rect((w * (1 - p)) / 2, h * 0.2, w * p, h * 0.6);
          c.rect(w * 0.2, (h * (1 - p)) / 2, w * 0.6, h * p);
        });
        break;
      }

      case 'wedge': {
        base();
        reveal((c) => {
          c.moveTo(w / 2, h / 2);
          c.arc(w / 2, h / 2, Math.hypot(w, h), Math.PI / 4, Math.PI / 4 + Math.PI * p);
          c.closePath();
        });
        break;
      }

      case 'doors': {
        // 旧帧像双开门一样向两边打开，露出新帧
        full();
        slideCtx.drawImage(oldBmp, 0, 0, w / 2, h, 0, 0, (w / 2) * (1 - p), h);
        slideCtx.drawImage(
          oldBmp,
          w / 2, 0, w / 2, h,
          w / 2 + (w / 2) * p, 0, (w / 2) * (1 - p), h,
        );
        break;
      }

      default:
        // 认得出但还没实现的（cube/vortex/honeycomb…）退化为淡入：
        // 名字已在解析层保留，诊断时看得出是「没实现」而不是「解析漏了」
        base();
        fadeNew(p);
        break;
    }
  }, alive);
}

/** 本页动画中，下一个还没播、且需要点击的步。 */
function nextClickStep() {
  for (const s of state.animSteps) {
    if (state.played.includes(s.index)) continue;
    if (s.trigger === 'click') return s;
  }
  return null;
}

/** 命中测试：这一点落在哪个还没播的触发器形状上。 */
function triggerAt(ptX, ptY) {
  let hit = null;
  for (const s of state.animSteps) {
    if (s.trigger !== 'shape' || state.played.includes(s.index)) continue;
    const r = s.rect;
    if (!r) continue;
    if (ptX >= r.x && ptX <= r.x + r.w && ptY >= r.y && ptY <= r.y + r.h) hit = s;
  }
  return hit;
}

/**
 * 播一步：记下它已播，再渲染出这一步的结果。
 *
 * 播完自动接着看下一条「上一动画之后」的步 —— 那种步不用等点击。
 */
async function playStep(step) {
  if (!step || state.played.includes(step.index)) return;
  state.played.push(step.index);
  updateStepBadge();
  await renderPlayed(step);
  scheduleAuto();
  armAutoAdvance();
}

/** 「上一动画之后」的步自己往下播，不用等点击。 */
function scheduleAuto() {
  clearTimeout(animTimer);
  const next = state.animSteps.find((s) => !state.played.includes(s.index));
  if (!next || next.trigger !== 'auto') return;
  animTimer = setTimeout(() => {
    const again = state.animSteps.find((s) => !state.played.includes(s.index));
    if (!again || again.trigger !== 'auto') return;
    playStep(again);
  }, AUTO_ANIM_MS);
}

/** 按当前「已播集合」渲染本页并播这一步的动画。 */
async function renderPlayed(step) {
  const page = state.page;
  const dpr = window.devicePixelRatio || 1;
  const renderScale = Math.max(0.05, state.scale * dpr);
  const seq = ++stepSeq;
  // 也占住画布：翻页与推进动画是两条独立的调用链，谁后开始谁负责画，
  // 先开始的那个必须停手（否则两张动画互相覆盖 = 闪，或者黑屏）
  const myPaint = ++paintSeq;
  const alive = () => state.page === page && seq === stepSeq && paintSeq === myPaint;

  try {
    // 图层与最终帧并行取：图层有几毫秒的光栅化，最终帧是一整页 —— 串行会白等
    const [layers, finalFrame] = await Promise.all([
      step && step.animated
        ? fetchLayers(page, step, renderScale)
        : Promise.resolve([]),
      requestFrame(page, renderScale, state.played),
    ]);
    if (!alive()) {
      layers.forEach((l) => l.bitmap.close?.());
      finalFrame.close?.();
      return;
    }

    if (layers.length > 0) {
      // 强调动画（放大、旋转）的原件要**从底帧里摘掉**，
      // 否则「静止的原件 + 动起来的图层」会叠成重影
      let base = frameBitmap;
      let hiddenBase = null;
      if (step.isEmphasis) {
        hiddenBase = await requestFrame(
          page,
          renderScale,
          state.played,
          layers.map((l) => l.shapeId),
        );
        if (!alive()) {
          hiddenBase.close?.();
          layers.forEach((l) => l.bitmap.close?.());
          finalFrame.close?.();
          return;
        }
        base = hiddenBase;
      }

      await animateLayers(step, layers, base, alive);
      hiddenBase?.close?.();
      layers.forEach((l) => l.bitmap.close?.());
      if (!alive()) {
        finalFrame.close?.();
        return;
      }
    } else {
      // 没有过程的步（纯出现/消失）：整页淡入就够了
      await crossFadeTo(finalFrame, animDuration(step && step.durMs), alive);
      if (!alive()) {
        finalFrame.close?.();
        return;
      }
    }

    const old = frameBitmap;
    frameBitmap = finalFrame;
    lastRenderScale = renderScale;
    paintFrame();
    updateStepBadge();
    if (old && old.close) old.close();
  } catch {
    /* 单步渲染失败不该打断放映：停在上一帧即可 */
  }
}

/** 线性插值。 */
function lerp(a, b, t) {
  return a + (b - a) * t;
}

/**
 * 取一步动画的图层（元信息 + 像素）。
 *
 * 整段动画只在开始时取一次图层，之后全由浏览器合成 ——
 * 逐帧回头端要图会受 IPC 与 PNG 编码拖累，只能到 20fps 上下。
 */
async function fetchLayers(page, step, scale) {
  let metas = [];
  try {
    metas = (await invoke('anim_layers', {
      page,
      step: step.index,
      played: state.played,
    })) || [];
  } catch {
    return [];
  }
  const layers = await Promise.all(
    metas.map(async (m) => {
      const args = {
        page,
        step: step.index,
        shapeId: m.shapeId,
        played: state.played,
        scale,
      };
      if (m.paraRange) {
        args.firstPara = m.paraRange[0];
        args.lastPara = m.paraRange[1];
      }
      try {
        const bytes = await invoke('anim_layer_png', args);
        const bitmap = await decodeFrame(bytes);
        return { ...m, bitmap };
      } catch {
        return null;
      }
    }),
  );
  // 后端在图层渲染不出来时给的是 1×1 占位图，丢掉
  return layers.filter((l) => l && (l.bitmap.width > 1 || l.bitmap.height > 1));
}

/**
 * 把这一步的图层按 `from → to` 插值，逐帧合成到底帧上。
 *
 * 每个图层只有一个仿射变换加一个透明度在变，所以交给浏览器合成就是
 * 满帧率；若让后端逐帧光栅化，一帧几十毫秒，动画只能到 20fps 上下。
 */
async function animateLayers(step, layers, base, alive = null) {
  const w = el.slide.width;
  const h = el.slide.height;
  // pt → 画布像素：画布就是整页的位图，用它反推最稳
  const ptW = state.info ? state.info.widthPt : 0;
  const k = ptW > 0 ? w / ptW : lastRenderScale || 1;

  await animateFor(
    animDuration(step.durMs),
    (p) => {
      if (base) slideCtx.drawImage(base, 0, 0, w, h);
      for (const l of layers) {
        drawLayer(l, {
          opacity: lerp(l.from.opacity, l.to.opacity, p),
          scale: lerp(l.from.scale, l.to.scale, p),
          rotate: lerp(l.from.rotate, l.to.rotate, p),
          dx: lerp(l.from.dx, l.to.dx, p),
          dy: lerp(l.from.dy, l.to.dy, p),
        }, p, k);
      }
    },
    alive
  );
}

/** 画一个图层：绕图层中心平移/缩放/旋转，必要时按擦除方向裁剪。 */
function drawLayer(l, s, p, k) {
  const r = l.rect;
  const cx = (r.x + r.w / 2) * k;
  const cy = (r.y + r.h / 2) * k;
  const dw = r.w * k;
  const dh = r.h * k;

  slideCtx.save();
  slideCtx.globalAlpha = clamp(s.opacity, 0, 1);
  slideCtx.translate(cx + s.dx * k, cy + s.dy * k);
  if (s.rotate) slideCtx.rotate((s.rotate * Math.PI) / 180);
  if (s.scale !== 1) slideCtx.scale(s.scale, s.scale);

  if (l.mask) {
    // 擦除：边缘从一个方向推过去，露出多少由进度决定
    const d = l.mask.dir;
    const x0 = -dw / 2;
    const y0 = -dh / 2;
    slideCtx.beginPath();
    if (d === 'rightToLeft') slideCtx.rect(x0 + dw * (1 - p), y0, dw * p, dh);
    else if (d === 'topToBottom') slideCtx.rect(x0, y0, dw, dh * p);
    else if (d === 'bottomToTop') slideCtx.rect(x0, y0 + dh * (1 - p), dw, dh * p);
    else slideCtx.rect(x0, y0, dw * p, dh);
    slideCtx.clip();
  }

  slideCtx.drawImage(l.bitmap, -dw / 2, -dh / 2, dw, dh);
  slideCtx.restore();
}

/**
 * 下板（回到课件）。
 *
 * 用在「明确要跳到课件的某一页」的入口上（缩略图、输页码、Home/End）：
 * 老师说的是「去第 5 页」，那就该看见课件的第 5 页，而不是板书。
 * 平时按翻页键**不会**走这里 —— 那是翻板书自己的页，见 `boardFlip`。
 */
function leaveBoard() {
  if (boardOn) setBoard(false);
}

/**
 * 在黑板上翻页：翻的是**板书自己**的页。
 *
 * 板书是多页的，翻到最后再按就新开一页 —— 相当于随手翻的草稿本。
 * 这里**不**顺手退出黑板：老师连着按翻页，意思显然是「再给我一页写」，
 * 不是「放我回课件」；要下板按黑板按钮或 Esc。
 *
 * 越界的两头都不动：第一页再往前没有东西，最后一页再往后受
 * `BOARD_MAX_PAGES` 限制（不封顶的话按住不放能翻出上百页空板）。
 */
function boardFlip(step) {
  const next = boardPage + step;
  if (next < 0) return;
  if (next >= BOARD_MAX_PAGES) {
    toast(`板书最多 ${BOARD_MAX_PAGES} 页`);
    return;
  }
  boardPage = next;
  redrawInk();
  updateToolButtons();
  updateNavUi();
  if (state.presenting) showPresentBar();
}

/**
 * 前进：先把本页作者设的动画播完，播完才翻页。
 *
 * 这就是「一页按作者设定的顺序逐条弹出」的落点 ——
 * 老师按空格/点击时，先弹下一条，最后一下才翻页。
 *
 * 窗口模式（翻阅视图）不在这里插入动画：那是浏览不是讲课，
 * 一路点下去还要为动画多点几次会很烦。
 */
function goForward() {
  cancelAutoAdvance();
  // 在黑板上「下一页」= 翻板书（不退出黑板）
  if (boardOn) return boardFlip(1);
  if (state.presenting) {
    const next = nextClickStep();
    if (next) {
      playStep(next);
      return;
    }
  }
  nextPage();
}

/** 后退：放映时还有播过的动画就先退一步，退到头才回上一页。 */
function goBack() {
  cancelAutoAdvance();
  clearTimeout(animTimer);
  if (boardOn) return boardFlip(-1);
  if (state.presenting && state.played.length > 0) {
    state.played.pop();
    updateStepBadge();
    renderPlayed(null);
    return;
  }
  prevPage();
}

/** 页码旁显示本页动画进度（没有动画就不显示）。 */
function updateStepBadge() {
  if (!el.stepBadge) return;
  const total = state.animSteps.length;
  el.stepBadge.textContent = total > 0 ? `动画 ${state.played.length}/${total}` : '';
  el.stepBadge.classList.toggle('on', total > 0);
}

/* ---------------- 自动前进 ---------------- */

let autoAdvanceTimer = 0;

/**
 * 作者设的「自动前进」（`p:transition/@advTm`）：到点自己翻页。
 *
 * PowerPoint 的语义是「本页动画播完后开始计时」，所以每播完一步都要重新计时；
 * 只在放映模式生效，任何手动操作都会把它清掉 —— 老师接管之后不该再被抢。
 */
function armAutoAdvance() {
  clearTimeout(autoAdvanceTimer);
  if (!state.presenting) return;
  const t = state.animTransition;
  if (!t || !t.advanceAfterMs) return;
  // 还有等点击的步就不计时：那是老师还没讲完
  if (nextClickStep()) return;
  autoAdvanceTimer = setTimeout(() => {
    if (state.presenting) goForward();
  }, t.advanceAfterMs);
}

/** 手动操作之后取消自动前进。 */
function cancelAutoAdvance() {
  clearTimeout(autoAdvanceTimer);
}

/* ---------------- 导航 ---------------- */

function goTo(page) {
  if (!state.info) return;
  leaveBoard();
  const p = clamp(page, 0, state.info.pageCount - 1);
  if (p === state.page && state.hasContent) return;
  // 跳页（缩略图、输页码、Home/End）按「这一页讲完了」的样子显示
  showPage(p, { mode: 'all' });
}

function nextPage() {
  if (!state.info) return;
  const p = state.page + 1;
  if (p >= state.info.pageCount) return;
  // 放映时向下翻页要从「一步都没播」开始，否则动画就白设了；
  // 窗口模式是翻阅不是讲课，得直接给全貌 —— 那里没有推进动画的入口，
  // 停在第 0 步会让作者藏起来的答案永远看不到
  showPage(p, { mode: state.presenting ? 'start' : 'all' });
}

function prevPage() {
  if (!state.info) return;
  const p = state.page - 1;
  if (p < 0) return;
  // 回翻时这一页已经讲过了，直接给全部显示的样子
  showPage(p, { mode: 'all' });
}

function updateNavUi() {
  if (!state.info) return;
  const total = state.info.pageCount;
  el.pageInput.value = String(state.page + 1);
  el.pageTotal.textContent = `/ ${total}`;
  // 在黑板上时这块牌子说的是「板书写到第几页」：按翻页键牌子却纹丝不动的话，
  // 老师会以为没反应（课件页码那时候根本不是他关心的事）
  el.presentPage.textContent = boardOn
    ? `板书 ${boardPage + 1}`
    : `${state.page + 1} / ${total}`;

  const atFirst = state.page === 0;
  const atLast = state.page >= total - 1;
  for (const id of ['btn-first', 'btn-prev']) $(id).disabled = atFirst;
  for (const id of ['btn-next', 'btn-last']) $(id).disabled = atLast;

  // 放映工具条上的翻页按钮同步禁用态
  $('p-prev').disabled = atFirst;
  $('p-next').disabled = atLast;

  updateStepBadge();
  highlightThumb(state.page);
}

/* ---------------- 缩略图 ---------------- */

let thumbObserver = null;
const thumbBitmaps = new Map();

function buildThumbs() {
  el.thumbs.innerHTML = '';
  if (thumbObserver) thumbObserver.disconnect();
  thumbBitmaps.clear();

  if (!state.info) return;

  const frag = document.createDocumentFragment();
  for (let i = 0; i < state.info.pageCount; i++) {
    const div = document.createElement('div');
    div.className = 'thumb';
    div.dataset.page = String(i);

    const c = document.createElement('canvas');
    c.width = 2;
    c.height = 2;
    div.appendChild(c);

    const no = document.createElement('span');
    no.className = 'thumb-no';
    no.textContent = String(i + 1);
    div.appendChild(no);

    div.addEventListener('click', () => goTo(i));
    frag.appendChild(div);
  }
  el.thumbs.appendChild(frag);

  // 懒加载：只渲染视口附近的缩略图
  thumbObserver = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          scheduleThumb(Number(entry.target.dataset.page), 0);
        }
      }
    },
    { root: el.sidebar, rootMargin: '160px' }
  );
  for (const node of el.thumbs.children) thumbObserver.observe(node);
}

/**
 * 同时在跑的缩略图请求数上限。
 *
 * # 为什么必须限流
 *
 * 侧栏一露出来就有十来个缩略图同时发请求，而后端每条请求都要占一个工作
 * 线程（没出好的页还要等一会儿）。它们把线程占满之后，老师接着翻页的那次
 * 请求只能排在后面 —— 手感就是「翻页反应慢」，严重时那一帧迟迟不来。
 *
 * 缩略图是背景活，慢一点无所谓；老师的操作必须永远排在最前面。
 */
const THUMB_CONCURRENCY = 2;
let thumbRunning = 0;
const thumbQueue = [];

function scheduleThumb(page, attempt) {
  thumbQueue.push({ page, attempt });
  pumpThumbs();
}

function pumpThumbs() {
  while (thumbRunning < THUMB_CONCURRENCY && thumbQueue.length > 0) {
    const job = thumbQueue.shift();
    thumbRunning++;
    loadThumb(job.page, job.attempt).finally(() => {
      thumbRunning--;
      pumpThumbs();
    });
  }
}

async function loadThumb(page, attempt = 0) {
  if (thumbBitmaps.has(page)) return;
  const node = el.thumbs.children[page];
  if (!node) return;

  thumbBitmaps.set(page, null); // 占位，避免重复请求

  try {
    const bytes = await invoke('render_page', { page, scale: 0.18 });
    const bitmap = await decodeFrame(bytes);
    thumbBitmaps.set(page, bitmap);

    const c = node.querySelector('canvas');
    if (!c) return;
    c.width = bitmap.width;
    c.height = bitmap.height;
    c.getContext('2d').drawImage(bitmap, 0, 0);
  } catch (e) {
    thumbBitmaps.delete(page);
    // 「还在生成中」只是排到后面去了：过一会儿自己再试一次，
    // 缩略图就会随着后台出图一格格亮起来，而不是永远空白
    if (isNotReady(e) && attempt < 40) {
      setTimeout(() => scheduleThumb(page, attempt + 1), 1200);
    }
  }
}

function highlightThumb(page) {
  const prev = el.thumbs.querySelector('.thumb.active');
  if (prev) prev.classList.remove('active');
  const cur = el.thumbs.children[page];
  if (cur) {
    cur.classList.add('active');
    cur.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  }
}

/* ---------------- 备注 ---------------- */

async function updateNotes(page) {
  if (el.notes.classList.contains('hidden')) return;
  try {
    const text = await invoke('page_notes', { page });
    el.notesBody.textContent = text && text.trim() ? text : '（本页没有备注）';
  } catch {
    el.notesBody.textContent = '（无法读取备注）';
  }
}

/* ---------------- 打开文件 ---------------- */

/**
 * 出图档位要按「页面实际占多少物理像素」定。
 *
 * 画面是本机办公软件出的**位图**：分辨率给低了投影到大屏就糊，
 * 给高了白等时间、白占磁盘。舞台的物理像素宽正是老师实际看到的尺寸。
 * 后端还会把它吸附到 160 的整数倍档位上（见 Rust 的 `raster_bucket`），
 * 免得老师拖一下窗口就散出一整套新缓存。
 */
function rasterWidthHint() {
  const dpr = window.devicePixelRatio || 1;
  const { w } = stageSize();
  return Math.max(1, Math.round((w || window.innerWidth) * dpr));
}

/**
 * 当前正在等的「办公软件出的第一页就绪」回调。
 *
 * `openPath` 等待时挂上它，`raster-first-page` 事件到达时调用一次。
 * 必须这样对接：打开课件与出图完成是两个独立的时间点，
 * 谁先发生都不一定 —— 小课件可能我们还没挂上就已经出好了。
 */
let upgradeWaiter = null;

/**
 * 等多久就不再等了，改用内置渲染。
 *
 * 出图分两步：打开课件 + 导出首页。实测一份 174MB / 39 页的课件是
 * `Open` 1.8 秒、首页 0.4 秒，典型小课件 1 秒出头。
 * 给到 20 秒是为了容下 WPS 进程冷启动，再久就说明这台机器上它出不来，
 * 继续等只会让老师干瞪眼。
 */
const UPGRADE_TIMEOUT_MS = 20000;

/**
 * 等本机办公软件把**第一页**出好，就绪后重载课件，换成它出的画面。
 *
 * 返回 `true` 表示已换成办公软件出的画面；`false` 表示超时或失败
 * （调用方继续用自研画面，并明确告诉老师）。
 *
 * # 为什么不像以前那样「先放自研画面再悄悄换掉」
 *
 * 自研内核对复杂课件会出错，而**先给一份错的再换掉，老师只会记住那份错的**——
 * 反复收到「显示的还是自研的内容、还带错」就是这个原因。
 *
 * # 为什么等的是第一页而不是整本
 *
 * 整本导出要 7 秒以上，而老师打开课件时只看得到第一页。
 * 第一页好了就切，剩下的页在后台排队（每页约 0.4 秒，比翻页快）。
 */
function waitForUpgrade(path, viewportWidth) {
  return new Promise((resolve) => {
    hideSpinner();
    // 文案要重置：旧版 .ppt 转换期间显示的是另一句，这里已经进入出图阶段了
    showPrepare('正在生成画面', '用本机办公软件出第一页，通常一两秒；之后就越翻越快');
    let settled = false;
    const finish = (ok) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      // 只清掉「自己的」挂载：期间老师可能又开了另一份课件，
      // 那时 upgradeWaiter 已经换成新的，不能被这一次超时误清
      if (upgradeWaiter === mine) upgradeWaiter = null;
      hidePrepare();
      resolve(ok);
    };
    const timer = setTimeout(() => {
      console.warn('等待办公软件出图超时，改用内置渲染');
      finish(false);
    }, UPGRADE_TIMEOUT_MS);

    const mine = async (payload) => {
      if (!payload || payload.path !== path) return;
      try {
        // 出图目录里已经有第一页了，同路径重开一次就会走「办公软件出的图」
        const info = await invoke('open_document', { path, viewportWidth });
        state.info = info;
        if (state.page >= info.pageCount) state.page = 0;
        finish(true);
      } catch (e) {
        console.warn('切换到办公软件出的画面失败', e);
        finish(false);
      }
    };
    upgradeWaiter = mine;
  });
}

/**
 * 旧版（PowerPoint 97-2003）演示文稿的扩展名。
 *
 * 真正的格式判断在后端按魔数做；这里只看扩展名，唯一用途是
 * **给老师一句解释** —— 说明「为什么要等」和「在等什么」。
 */
function isLegacyPpt(path) {
  return /\.(ppt|pps|pot)$/i.test(path || '');
}

/**
 * 显示等待界面。
 *
 * 文案必须说清**在等什么**：等格式转换和等第一页出图是两件事，
 * 一句笼统的「正在加载」只会让人觉得卡住了。
 */
function showPrepare(title, sub) {
  el.prepare.querySelector('.prepare-title').textContent = title;
  el.prepare.querySelector('.prepare-sub').textContent = sub;
  el.prepare.classList.add('on');
}

function hidePrepare() {
  el.prepare.classList.remove('on');
}

async function openPath(path) {
  if (!path) return;
  showSpinner();
  // 旧版 .ppt：后端会先让本机办公软件把它转成 .pptx 再打开。
  // 这一步要几秒，不说明白老师会以为卡死了。
  const legacy = isLegacyPpt(path);
  if (legacy) {
    showPrepare(
      '正在转换旧版 .ppt',
      '用本机办公软件把它转成 .pptx，首次通常几秒；转好之后就直接打开'
    );
  }
  try {
    // 换课件之前，先把上一份的标注/板书落盘。
    //
    // 自动保存是去抖的（停笔 3 秒才写），而老师完全可能写完最后一笔就
    // 直接点开下一份课件 —— 不补这一下，那几笔就没了。
    // 此刻后端手上还是**旧**那份课件，所以这次保存写在它自己旁边。
    if (state.info && hasUnsavedAnnotations()) {
      await saveAnnotations({ quiet: true });
    }
    clearTimeout(autoSaveTimer);

    // 出图档位按屏幕定，所以要在打开之前算出来一起传下去
    const viewportWidth = rasterWidthHint();
    const info = await invoke('open_document', { path, viewportWidth });
    state.info = info;
    state.page = 0;
    state.zoom = 1;
    state.hasContent = false;
    // 换课件先把黑板收掉：不然新课件一打开就顶着一块空板子。
    // 板书页码也归零 —— 新课件该从板书第一页开始
    setBoard(false);
    boardPage = 0;
    state.annotations = {};
    state.history = {};

    el.fileName.textContent = info.title || info.fileName;
    el.welcome.classList.add('hidden');
    setDocControlsEnabled(true);

    // 换课件等于回到「适应窗口 + 居中」，走 refit 一并把平移量归零
    refit();

    // 一张图都还没有（本机有 WPS/Office，正在出第一页）：**等它**。
    //
    // 这期间刻意什么都不画 —— 不画自研画面、也不做缩略图。
    // 那两件事既是错的画面，又会和后台出图抢 CPU，
    // 把老师等的时间拉得更长（39 页的缩略图要两秒多，比出图本身还久）。
    let upgraded = false;
    if (info.upgrading) {
      upgraded = await waitForUpgrade(path, viewportWidth);
    }

    buildThumbs();

    // 打开的就是 PDF 时，画面由自带的光栅化路径出：每页都得解释一遍内容流
    // （实测 50~360ms）。整本后台预热一下才不会「翻到哪页卡哪页」；
    // 队列是最低优先级，老师一操作就抢先。
    if (state.info && state.info.displaySource === 'pdf') {
      invoke('prewarm_all').catch(() => {});
    }

    // 等不到（超时/失败/本机没装）才用内置渲染 —— 但要说清楚，
    // 别让老师以为看到的是原稿
    if (!upgraded && state.info && state.info.displaySource === 'self') {
      toast(
        state.info.upgrading
          ? '本机办公软件未能及时生成画面，已改用内置渲染，个别排版可能与原稿不同'
          : '未检测到 WPS/PowerPoint，本份课件使用内置渲染，个别排版可能与原稿不同'
      );
    }

    // 打开课件是「先看看内容」，不是开讲：首页给全貌
    await showPage(0, { mode: 'all' });

    await loadAnnotations();
    toast(`已打开：${info.title || info.fileName}（${info.pageCount} 页）`);
  } catch (e) {
    toast(String(e), true);
  } finally {
    // 转换失败（或中途退出）时也必须收掉等待界面，否则会一直盖在屏幕上
    if (legacy) hidePrepare();
    hideSpinner();
  }
}

/**
 * 取一次「另一个进程转交过来的课件」并打开它。
 *
 * 关窗不退出之后，双击课件是**第二个进程**把路径转给已经开着的这个
 * （见 Rust 的 `resident`）。转交同时走两条路 —— 一条 `open-file` 事件、
 * 一条放在后端等着被取：界面刚起来的那一两百毫秒里还没注册监听，
 * 那时候发的事件会丢，得靠取件兜住。取走就没了，所以不会打开两遍。
 */
async function drainPendingOpen() {
  let path = null;
  try {
    path = await invoke('take_pending_open');
  } catch (e) {
    console.warn('取转交文件失败', e);
    return;
  }
  if (!path) return;

  // 放映中途被「又点开一个课件」打断：先退出放映，
  // 否则新课件会以放映态加载，工具条、页码全是从上一份带过来的
  if (state.presenting) await setPresenting(false);
  await openPath(path);
}

async function chooseFile() {
  try {
    const picked = await dlg.open({
      multiple: false,
      filters: [
        {
          name: '课件与文档',
          // OOXML 家族的六种演示文稿都能放（含启用宏的变体），
          // 加上 PDF。文件选择框里列全，老师才不会以为「这种打不开」
          extensions: ['pptx', 'pptm', 'ppsx', 'ppsm', 'potx', 'potm', 'pdf'],
        },
      ],
    });
    if (picked) openPath(typeof picked === 'string' ? picked : picked.path);
  } catch (e) {
    toast(String(e), true);
  }
}

function setDocControlsEnabled(on) {
  for (const id of [
    'btn-first',
    'btn-prev',
    'btn-next',
    'btn-last',
    'btn-zoom-in',
    'btn-zoom-out',
    'btn-zoom-reset',
    'btn-present',
    'btn-notes',
    'btn-save',
  ]) {
    $(id).disabled = !on;
  }
  el.pageInput.disabled = !on;
  el.toolbar.classList.toggle('hidden', !on);
}

/* ---------------- 标注持久化 ---------------- */

async function loadAnnotations() {
  try {
    const raw = await invoke('load_annotations');
    if (!raw) return;
    const data = JSON.parse(raw);
    const map = {};
    for (const [key, strokes] of Object.entries(data.pages || {})) {
      // 页码是数字键；板书是 `board1`/`board2`…，键名原样留着 ——
      // 「第几页板书」就是键名里的那个数（见 `boardSlot`）
      map[isBoardSlot(key) ? key : Number(key)] = strokes.map(deserializeStroke);
    }
    state.annotations = map;
    redrawInk();
  } catch (e) {
    toast(`读取标注失败：${e}`, true);
  }
}

/**
 * 把标注写回课件旁边的旁挂文件。
 *
 * `quiet` 用于「退出前抢救一次」：那条路上不该弹任何提示 ——
 * 老师点的是「退出」，不是「保存」，冒一句「标注已保存」只会让人困惑。
 */
async function saveAnnotations(opts) {
  if (!state.info) return;
  const quiet = opts && opts.quiet === true;
  const pages = {};
  for (const [page, strokes] of Object.entries(state.annotations)) {
    if (strokes.length > 0) {
      // 页码是数字键，板书是 `board1`/`board2`… 这样的字符串键，一起存 ——
      // 板书跟着课件走：换个班上课打开同一份课件，板书还在
      pages[page] = strokes.map(serializeStroke);
    }
  }
  try {
    await invoke('save_annotations', {
      json: JSON.stringify({ version: 1, pages }),
    });
    if (!quiet) toast('标注已保存');
  } catch (e) {
    if (quiet) console.warn('退出前保存标注失败', e);
    else toast(`保存失败：${e}`, true);
  }
}

function serializeStroke(s) {
  return {
    tool: s.tool,
    color: s.color,
    width: s.width,
    opacity: s.opacity,
    // 压感量化到 0..9，显著减小文件体积且视觉上无差别
    points: s.points.map(([x, y, p]) => [
      Math.round(x * 10) / 10,
      Math.round(y * 10) / 10,
      Math.round(p * 9),
    ]),
  };
}

function deserializeStroke(o) {
  const s = new Stroke(o.tool, o.color, o.width, o.opacity);
  for (const [x, y, p] of o.points || []) {
    s.points.push([x, y, (p ?? 5) / 9]);
  }
  return s;
}

function hasUnsavedAnnotations() {
  // 板书也算：它和标注存在同一份旁挂文件里
  return Object.values(state.annotations).some((list) => list.length > 0);
}

/** 自动保存的去抖时长：停笔这么久之后落盘。 */
const AUTO_SAVE_DELAY_MS = 3000;
let autoSaveTimer = 0;

/**
 * 改完标注 / 板书，停笔 3 秒自动落盘。
 *
 * 老师在上课，不能指望他记得按 Ctrl+S（板书更是随手写的）。
 * 这里做去抖：连着写十几笔只会存一次。
 *
 * 存的时候必须 `quiet` —— 自动保存弹一句「标注已保存」会打断讲课，
 * 而且每三秒弹一次能把人烦死。
 */
function scheduleAutoSave() {
  if (!state.info) return;
  clearTimeout(autoSaveTimer);
  autoSaveTimer = setTimeout(() => saveAnnotations({ quiet: true }), AUTO_SAVE_DELAY_MS);
}

/* ---------------- 撤销 / 重做 ---------------- */

function historyFor(page) {
  if (!state.history[page]) state.history[page] = { undo: [], redo: [] };
  return state.history[page];
}

function undo() {
  const h = historyFor(inkKey());
  const list = strokesFor(inkKey());
  if (list.length === 0) return;
  const s = list.pop();
  h.redo.push(s);
  redrawInk();
  scheduleAutoSave();
}

function redo() {
  const h = historyFor(inkKey());
  const list = strokesFor(inkKey());
  if (h.redo.length === 0) return;
  list.push(h.redo.pop());
  redrawInk();
  scheduleAutoSave();
}

function clearPage() {
  const key = inkKey();
  const list = strokesFor(key);
  if (list.length === 0) return;
  list.length = 0;
  state.history[key] = { undo: [], redo: [] };
  redrawInk();
  scheduleAutoSave();
}

/* ---------------- 放映模式（对标 WPS 放映） ---------------- */

/**
 * 正在切换放映状态，用来挡住重入。
 *
 * 切换过程里有 `await`（等 Rust 把窗口切成全屏），这期间**不能再进来一次**：
 * 连点两下「放映」（触摸屏上的抖动很容易触发）会并发跑两遍，
 * 一遍置 true、一遍置 false，最后停在「窗口是全屏、`state.presenting` 却是 false」
 * 这种半吊子状态上 —— 工具条不显示（它要求 `state.presenting`），
 * Esc 也没反应（同一个判断）。触屏上连键盘都没有，老师就被困在全屏里了。
 */
let presentingBusy = false;

/**
 * 进入 / 退出放映。
 *
 * 进入时做四件事：
 * 1. 让 Rust 侧把窗口设为**真全屏 + 置顶 + 隐藏光标**（CSS 全屏做不到这些）；
 * 2. 按屏幕尺寸重算缩放，幻灯片居中并铺满（黑边 letterbox）；
 * 3. 显示页码与计时器，页码短暂后淡出；
 * 4. 点亮放映工具条（默认是左下角的小胶囊）。
 *
 * 自己只是「重入闸」：真正的活在 [`presentOnce`] 里。
 */
async function setPresenting(on) {
  if (state.presenting === on) return;
  if (presentingBusy) return;
  presentingBusy = true;
  try {
    await presentOnce(on);
  } finally {
    presentingBusy = false;
  }
}

async function presentOnce(on) {
  // 短暂黑场遮住布局切换的瞬间，避免看到幻灯片跳位
  el.fade.classList.add('on');
  try {
    await presentOnceInner(on);
  } finally {
    // **必须无条件把黑场收掉。**
    //
    // 中途任何一步抛异常（例如绘制时踩到一张已被关掉的位图）都会让这层
    // 黑幕留在屏幕上 —— 那就是老师说的「全屏播放时黑屏」：
    // 此时 state.presenting 已经是 true，界面进入放映态却什么都看不见，
    // 连再按一次 F5 都因为「已经在放映」而被挡回来。
    el.fade.classList.remove('on');
  }
}

/** 真正干活的：切界面、切全屏、起停计时。 */
async function presentOnceInner(on) {
  await sleep(90);

  state.presenting = on;
  document.body.classList.toggle('presenting', on);

  try {
    await invoke('set_presentation_mode', { on });
  } catch (e) {
    // 原生全屏失败（极少数环境）时退化为浏览器全屏，保证仍能讲课
    console.warn('原生全屏失败，退化为浏览器全屏', e);
    try {
      if (on) await document.documentElement.requestFullscreen();
      else if (document.fullscreenElement) await document.exitFullscreen();
    } catch {
      /* 两种都失败时仍继续，只是不铺满屏幕 */
    }
  }

  if (on) {
    // 放映默认「不落笔」：单击翻页才是讲课时最高频的操作，
    // 想标注时再从工具条点画笔（与 WPS 一致）
    state.tool = 'pointer';
    // 开讲时这一页要从头演一遍，而不是停在刚才翻阅时看到的样子。
    // 不播转场：本页已经在屏幕上了，再「演进来」一次会很怪。
    if (state.animSteps.length > 0) {
      showPage(state.page, { silent: true, mode: 'start', transition: false });
    }
    // 放映时把「适应屏幕」重算为整屏铺满
    state.zoom = 1;
    fitAndRepaint();
    // 原生全屏的窗口尺寸在若干帧之后才稳定，再补两次校准，
    // 否则 WebView 仍按旧窗口尺寸算缩放，幻灯片会偏小或溢出
    setTimeout(fitAndRepaint, 80);
    setTimeout(fitAndRepaint, 320);

    el.timer.classList.add('on');
    startTimer();
    showPresentPage();
    // 放映工具条必须在这里显式点亮。
    //
    // `body.presenting` 会把编辑工具条整个藏掉，放映工具条默认又是
    // 「屏幕外 + 透明」的（见 style.css 的 `#present-bar`），
    // 而它的 `.on` 以前只在点色块、按快捷键时才加上 ——
    // 于是按 F5 进入放映后**一条工具条都看不到**，
    // 只能靠撞键盘快捷键把它叫回来。
    showPresentBar();
    logOnce();
    toast('放映中：单击翻页，右键回翻，Esc 退出');
    // 再确认一次工具条。
    //
    // 全屏后窗口尺寸要几帧才稳定，而「进了全屏却没工具条」对触屏老师
    // 等于被困住（连 Esc 都没有）。这一下几乎不花成本，换来的是不会卡死。
    setTimeout(() => {
      if (state.presenting) showPresentBar();
    }, 400);
  } else {
    el.presentBar.classList.remove('on');
    el.presentPage.classList.remove('on');
    el.ctxMenu.classList.remove('on');
    el.timer.classList.remove('on');
    // 退出放映就把「闲置缩小」的计时停掉，别让它在窗口模式下继续跑
    clearTimeout(presentBarIdleTimer);
    pauseTimer();
    // 退出放映：作者的「自动前进」也不该再替老师翻页了
    cancelAutoAdvance();
    clearTimeout(animTimer);
    // 回到窗口（编辑）视图：恢复画笔，便于顺手补写标注
    state.tool = 'pen';

    // 清掉放映态的临时视觉
    setBoard(false);
    setSpotlight(false);
    hideLinkHint();
    // 光标状态复位（空闲计时器要清掉，否则会在窗口模式下把光标藏起来）
    resetCursorState();
    laserActive = false;
    laserCtx.clearRect(0, 0, el.laser.width, el.laser.height);

    // 恢复窗口模式的布局
    fitAndRepaint();
    setTimeout(fitAndRepaint, 80);
  }

  updateToolButtons();
}

/** 首次进入放映时提示一次快捷键（用 localStorage 记住，不反复打扰）。 */
function logOnce() {
  try {
    const key = 'oppv-present-hint-shown';
    if (localStorage.getItem(key)) return;
    localStorage.setItem(key, '1');
  } catch {
    /* 无 localStorage 时忽略 */
  }
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/** 显示页码。
 *
 * 页码现在**常驻**（工具条也常驻），只在翻页时做一次高亮闪动提示 ——
 * 「讲课时随时知道讲到第几页」比「屏幕干净」更重要。 */
function showPresentPage() {
  if (!state.info) return;
  el.presentPage.textContent = `${state.page + 1} / ${state.info.pageCount}`;
  el.presentPage.classList.add('on');
  // 翻页时轻轻弹一下，不抢注意力但足以让老师确认「翻过去了」
  el.presentPage.classList.remove('flash');
  void el.presentPage.offsetWidth;
  el.presentPage.classList.add('flash');
}

/* ---------------- 操作方式：自动识别 or 老师自己选 ---------------- */

/**
 * 老师的操作方式：`auto` / `touch` / `mouse`，存在本地。
 *
 * # 为什么不干脆每次都用 `@media (pointer: coarse)` 探
 *
 * 一体机、平板、带触摸屏的笔记本对外接鼠标的探测结果并不一致 ——
 * 插上鼠标说自己是鼠标，拔掉又说自己是触摸屏。「用不用手指点」是老师的习惯，
 * 不该随插拔变化。所以首次启动在欢迎页让老师选一次，之后一直按它走；
 * 拿不准时用自动识别当默认。
 */
const INPUT_MODE_KEY = 'oppv-input-mode';

/** 系统报的「主指针是不是粗指针」（触摸屏 / 手写笔）。 */
const SYSTEM_COARSE = window.matchMedia('(pointer: coarse)').matches;

let inputMode = 'auto';

/** 按当前设置算：这次该按触摸屏来吗。 */
function isTouchMode() {
  if (inputMode === 'touch') return true;
  if (inputMode === 'mouse') return false;
  return SYSTEM_COARSE;
}

/** 安装向导里老师选的那一次（读注册表）；绿色版 / 开发时读不到。 */
let installPreference = null;

/** 当前这个值是哪儿来的，用来在界面上如实说明。 */
let inputModeSource = 'auto';

/** 把操作方式同步到 `html.touch-mode`，CSS 只认这一个开关。 */
function applyInputMode() {
  document.documentElement.classList.toggle('touch-mode', isTouchMode());
  for (const chip of document.querySelectorAll('.mode-chip')) {
    chip.classList.toggle('on', chip.dataset.mode === inputMode);
  }

  const detected = SYSTEM_COARSE ? '触摸屏' : '鼠标键盘';
  const origin =
    inputModeSource === 'install'
      ? '安装时选的'
      : inputModeSource === 'saved'
        ? '上次在应用里选的'
        : `自动识别为「${detected}」`;

  const text =
    `${origin}。` +
    (isTouchMode()
      ? '按钮更大、带中文标签。'
      : '界面更紧凑。') +
    '放映时工具条收成左下角的小胶囊（可以拖到顺手的位置），点左侧箭头展开；随时可以在这里改。';

  // 欢迎页与设置页各有一份（靠类名绑定），两处必须显示同一句话
  for (const hint of document.querySelectorAll('.mode-hint')) {
    hint.textContent = text;
  }
}

function setInputMode(mode) {
  if (mode !== 'auto' && mode !== 'touch' && mode !== 'mouse') return;
  inputMode = mode;
  inputModeSource = 'saved';
  try {
    localStorage.setItem(INPUT_MODE_KEY, mode);
  } catch {
    /* 隐私模式下写不进：本次会话仍然按老师选的走 */
  }
  applyInputMode();
}

/**
 * 定下操作方式，优先级：
 *
 * 1. 老师后来在应用里改过的（localStorage）
 * 2. 安装向导里选的那次（注册表）
 * 3. 自动识别
 *
 * 第 2 条是关键：一体机、带触摸屏的笔记本上 `(pointer: coarse)` 会随
 * 插拔鼠标变来变去，而「我用手指点」是习惯，安装时说一次就够了。
 */
async function loadInputMode() {
  let saved = null;
  try {
    saved = localStorage.getItem(INPUT_MODE_KEY);
  } catch {
    /* 隐私模式读不到 localStorage */
  }

  if (saved === 'auto' || saved === 'touch' || saved === 'mouse') {
    inputMode = saved;
    inputModeSource = 'saved';
  } else {
    try {
      installPreference = await invoke('install_preference');
    } catch {
      installPreference = null;
    }
    const fromInstaller = installPreference && installPreference.inputMode;
    if (fromInstaller === 'auto' || fromInstaller === 'touch' || fromInstaller === 'mouse') {
      inputMode = fromInstaller;
      inputModeSource = 'install';
    }
  }

  applyInputMode();
}

/* ---------------- 放映工具条：常驻 + 自动缩小 + 可拖动 ---------------- */

/**
 * 工具条是否收成了左下角的小胶囊。
 *
 * # 这块的行为
 *
 * 1. 放映时工具条**一直在**，默认收成一颗小胶囊 —— 讲课的时候它是
 *    唯一压在课件上的东西，能小就小；
 * 2. 点左侧箭头展开全套工具，**闲置 [`PRESENT_BAR_IDLE_MS`] 之后自己
 *    缩回去**：老师点开「画笔」写完忘了收，横条就会一直挡着板书；
 * 3. 胶囊可以拖（见 `bindPresentBarDrag`），位置记在 localStorage 里 ——
 *    一体机上它正好压在左下角，而那儿常常就是老师写字的地方。
 *
 * # 为什么不做「靠近底部升起、停用后淡出」
 *
 * 触屏上根本没有悬停，工具条一消失老师就得反复去够屏幕最底边。
 * 所以是「常驻的小胶囊 + 闲置后缩小」，而不是「整条消失」。
 */
let presentBarMinimized = true;

/** 展开之后多久没人碰它，就自己缩回小胶囊。 */
const PRESENT_BAR_IDLE_MS = 6000;
let presentBarIdleTimer = 0;

/** 老师把胶囊拖到哪儿了（视口坐标 px）；`null` = 默认贴在左下角。 */
let presentBarPos = null;
const PRESENT_BAR_POS_KEY = 'oppv-present-bar-pos';

/** 读出老师上次拖到的位置。 */
function loadPresentBarPos() {
  try {
    const p = JSON.parse(localStorage.getItem(PRESENT_BAR_POS_KEY) || 'null');
    if (p && typeof p.x === 'number' && typeof p.y === 'number') presentBarPos = p;
  } catch {
    /* 读不出来就用默认位置 */
  }
}

/**
 * 把「拖出来的位置」落到 DOM。
 *
 * 只有小胶囊形态才用它：展开态是一条长工具条，挂在屏幕角落会有一半
 * 露在屏幕外，所以展开时回到默认的「底部居中」。
 */
function applyPresentBarPos() {
  const bar = el.presentBar;
  if (!bar) return;
  if (!presentBarMinimized || !presentBarPos) {
    bar.classList.remove('placed');
    bar.style.left = '';
    bar.style.top = '';
    return;
  }
  // 换过分辨率、改过窗口大小之后老位置可能落在屏幕外，进来先夹一下
  const x = Math.min(presentBarPos.x, Math.max(0, window.innerWidth - bar.offsetWidth));
  const y = Math.min(presentBarPos.y, Math.max(0, window.innerHeight - bar.offsetHeight));
  bar.classList.add('placed');
  bar.style.left = `${x}px`;
  bar.style.top = `${y}px`;
}

/** 记一次「老师还在用工具条」，重新开始空闲计时。 */
function notePresentBarActivity() {
  clearTimeout(presentBarIdleTimer);
  // 已经收着了就没什么可再收的
  if (presentBarMinimized) return;
  presentBarIdleTimer = setTimeout(() => setPresentBarMinimized(true), PRESENT_BAR_IDLE_MS);
}

/** 把最小化态同步到 DOM。 */
function applyPresentBarMinimized() {
  el.presentBar.classList.toggle('docked', presentBarMinimized);
  applyPresentBarPos();
  if (el.presentDock) {
    el.presentDock.title = presentBarMinimized ? '展开工具条' : '最小化工具条';
  }
  notePresentBarActivity();
}

/** 最小化 / 展开工具条。 */
function setPresentBarMinimized(on) {
  presentBarMinimized = on;
  applyPresentBarMinimized();
}

/**
 * 把收起的工具条做成「悬浮球」：按住就能拖，位置记下来。
 *
 * 为什么要能拖：触屏一体机上这条胶囊正好压在课件左下角（老师板书常写
 * 在那儿），不能挪就只能忍着。鼠标模式也一样有用。
 *
 * 6px 的阈值是为了区分「拖」和「点」—— 手指按按钮时多少会抖一下，
 * 一抖就当成拖动的话，按钮就点不实了。
 */
function bindPresentBarDrag() {
  const bar = el.presentBar;
  if (!bar) return;

  let activeId = null;
  let dragging = false;
  let moved = false;
  let startX = 0;
  let startY = 0;
  let originX = 0;
  let originY = 0;

  bar.addEventListener('pointerdown', (e) => {
    if (!state.presenting || !presentBarMinimized) return;
    const r = bar.getBoundingClientRect();
    activeId = e.pointerId;
    dragging = false;
    moved = false;
    startX = e.clientX;
    startY = e.clientY;
    originX = r.left;
    originY = r.top;
  });

  bar.addEventListener('pointermove', (e) => {
    if (activeId === null || e.pointerId !== activeId) return;
    const dx = e.clientX - startX;
    const dy = e.clientY - startY;
    if (!dragging && Math.hypot(dx, dy) < 6) return;
    if (!dragging) {
      dragging = true;
      bar.setPointerCapture?.(e.pointerId);
    }
    moved = true;
    placePresentBar(originX + dx, originY + dy);
  });

  const endDrag = (e) => {
    if (activeId === null || e.pointerId !== activeId) return;
    activeId = null;
    if (!dragging) return;
    dragging = false;
    presentBarPos = { x: el.presentBar.offsetLeft, y: el.presentBar.offsetTop };
    try {
      localStorage.setItem(PRESENT_BAR_POS_KEY, JSON.stringify(presentBarPos));
    } catch {
      /* 存不上也不影响这次讲课 */
    }
  };
  bar.addEventListener('pointerup', endDrag);
  bar.addEventListener('pointercancel', endDrag);

  // 拖完那一下别再顺手把工具条展开 —— 否则每拖一次都要重新收一次
  bar.addEventListener(
    'click',
    (e) => {
      if (!moved) return;
      moved = false;
      e.stopPropagation();
      e.preventDefault();
    },
    true
  );

  // 鼠标停在工具条上就是「正在用」，这时候别缩回去
  bar.addEventListener('pointerenter', notePresentBarActivity);
  bar.addEventListener('pointermove', notePresentBarActivity);
}

/** 把胶囊放过去，别让它跑出屏幕。 */
function placePresentBar(x, y) {
  const bar = el.presentBar;
  const nx = Math.min(Math.max(0, x), Math.max(0, window.innerWidth - bar.offsetWidth));
  const ny = Math.min(Math.max(0, y), Math.max(0, window.innerHeight - bar.offsetHeight));
  bar.classList.add('placed');
  bar.style.left = `${nx}px`;
  bar.style.top = `${ny}px`;
}

function showPresentBar() {
  if (!state.presenting) return;
  el.presentBar.classList.add('on');
  // 页码也一直显示：讲课时老师要随时知道讲到第几页了
  el.presentPage.classList.add('on');
  applyPresentBarMinimized();
}

/** 标记「本次按下只是关掉右键菜单」，用于避免连锁翻页。 */
let menuDismissAtDown = false;

/** 触摸点是否落在屏幕最底边那条带（只是给鼠标为主的设备兜个底）。 */
function inPresentHotzone(clientY) {
  return window.innerHeight - clientY < 110;
}

/**
 * 指针移动：维护光标显隐与链接悬停提示。
 *
 * 用 `pointermove` 而不是 `mousemove` —— 触摸屏上根本没有鼠标移动事件。
 */
function onPresentPointerMove(e) {
  if (!state.presenting) return;
  notePointerActivity();
  updateLinkHover(e.clientX, e.clientY);
}

/* ---------------- 放映模式的光标显隐 ---------------- */

/** 鼠标静置多久后隐藏光标（WPS 的行为：动就显示，停一会儿再收起）。 */
const CURSOR_IDLE_MS = 2500;
let cursorIdleTimer = 0;

/**
 * 记录一次指针活动：立刻显示光标，并重新开始空闲计时。
 *
 * 为什么不用 `set_cursor_visible`：那是**窗口级**隐藏，
 * 之后无论 CSS 写什么光标都换不回来 —— 「鼠标」模式下全屏后
 * 完全看不到光标，老师既点不准按钮也不知道自己在哪。
 * 所以显隐只能由 CSS 做（见 style.css 的 `#ink-layer` 光标规则）。
 */
function notePointerActivity() {
  if (!state.presenting) return;
  el.ink.classList.remove('cursor-hidden');
  clearTimeout(cursorIdleTimer);
  cursorIdleTimer = setTimeout(() => {
    // 拿笔时始终显示十字，否则老师找不到落笔点
    if (state.presenting && !isDrawingTool()) el.ink.classList.add('cursor-hidden');
  }, CURSOR_IDLE_MS);
}

/** 离开放映模式时把光标状态复位。 */
function resetCursorState() {
  clearTimeout(cursorIdleTimer);
  cursorIdleTimer = 0;
  el.ink.classList.remove('cursor-hidden');
}

/* ---------------- 放映时的点击翻页 ---------------- */

/**
 * WPS 的点击语义：
 * - 未选画笔时（`pointer`）：**单击 = 下一页**，右键 = 上一页；
 * - 选了画笔/荧光笔/橡皮时：点击用于书写，不翻页。
 *
 * 这里用「工具是否为画笔类」来判断，与 WPS 一致。
 */
function isDrawingTool() {
  return state.tool === 'pen' || state.tool === 'highlighter' || state.tool === 'eraser';
}

/* ---------------- 右键菜单 ---------------- */

function openCtxMenu(x, y) {
  const menu = el.ctxMenu;
  menu.classList.add('on');

  // 贴边时向内收，避免菜单被屏幕裁掉
  const rect = menu.getBoundingClientRect();
  const px = Math.min(x, window.innerWidth - rect.width - 8);
  const py = Math.min(y, window.innerHeight - rect.height - 8);
  menu.style.left = `${Math.max(8, px)}px`;
  menu.style.top = `${Math.max(8, py)}px`;
}

function closeCtxMenu() {
  el.ctxMenu.classList.remove('on');
}

/** 右键菜单的长按唤出（触屏一体机上没有右键）。 */
let longPressTimer = 0;

function onLongPressStart(e) {
  if (!state.presenting) return;
  if (isDrawingTool()) return;
  const { clientX, clientY } = e.touches ? e.touches[0] : e;
  clearTimeout(longPressTimer);
  longPressTimer = setTimeout(() => openCtxMenu(clientX, clientY), 620);
}

function onLongPressEnd() {
  clearTimeout(longPressTimer);
}

/**
 * 上板 / 下板。
 *
 * # 为什么不是「黑屏」
 *
 * 原来这个位置是「黑屏」—— 把屏幕压黑，讲课中间想临时板书只能干看着一块黑。
 * 黑板把这块屏变成**能写**的：点一下就能讲例题、画图、随手记一笔，
 * 再点一下回到课件。
 *
 * # 板书会自己存下来
 *
 * 板上的字记在 `state.annotations` 的板书格里（`board1`、`board2`…，见
 * `inkKey`），撤销 / 清空 / 橡皮照常可用；写完 3 秒自动落盘到课件旁的
 * 旁挂文件里（见 `scheduleAutoSave`）—— 老师不用记得按保存，
 * 换个班上课打开同一份课件，板书还在。
 *
 * # 上板顺手换个能看清的笔色
 *
 * 面板是深墨绿，老师要是正拿着黑色笔（调色板里第 7 个），上去就是「写了个寂寞」。
 * 所以上板时把暗色悄悄换成白粉笔色，下板再还回去 —— 他自己选的色不该被永久改掉。
 */
function setBoard(on) {
  if (boardOn === on) return;
  // 没打开课件时不认这一下：黑板的尺寸来自课件（见 `layoutCanvases`），
  // 这时候开出来的会是一块 0×0 的板子 —— 按了没反应比没有这个按钮更糟
  if (on && !state.info) return;
  boardOn = on;

  el.board.classList.toggle('on', on);
  el.stage.classList.toggle('board-on', on);

  if (on) {
    penColorBeforeBoard = state.color;
    if (relativeLuminance(state.color) < 0.45) state.color = '#ffffff';
    // 上板就是要写字：手里还握着「鼠标（翻页）」的话先换成笔
    if (state.tool === 'pointer') state.tool = 'pen';
  } else {
    if (penColorBeforeBoard) state.color = penColorBeforeBoard;
    penColorBeforeBoard = '';
    // 下板**不**清板书：它会跟着课件存下来（见 `scheduleAutoSave`），
    // 再上板接着写。要擦用「清空」，要翻页用翻页键。
    // 这里也不主动存一次 —— 笔一停本来就存过了，不必多写一次文件。
  }

  redrawInk();
  updateToolButtons();
  // 页码牌要跟着换说法（上板显示「板书 1」，下板回到课件页码）
  updateNavUi();
  if (state.presenting) showPresentBar();
}

/** 颜色相对亮度（0 = 黑，1 = 白）。用来判断笔色在深色板子上看不看得清。 */
function relativeLuminance(hex) {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex || '');
  if (!m) return 1;
  const n = parseInt(m[1], 16);
  const r = (n >> 16) & 255;
  const g = (n >> 8) & 255;
  const b = n & 255;
  return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
}

function setSpotlight(on) {
  el.spotlight.classList.toggle('on', on);
  updateToolButtons();
}

/* ---------------- 计时器 ---------------- */

function startTimer() {
  if (state.timer.running) return;
  state.timer.running = true;
  state.timer.startedAt = performance.now() - state.timer.elapsed;
  state.timer.id = setInterval(tickTimer, 500);
  tickTimer();
}

function pauseTimer() {
  if (!state.timer.running) return;
  state.timer.running = false;
  state.timer.elapsed = performance.now() - state.timer.startedAt;
  clearInterval(state.timer.id);
  state.timer.id = 0;
}

function tickTimer() {
  const ms = state.timer.running
    ? performance.now() - state.timer.startedAt
    : state.timer.elapsed;
  const total = Math.floor(ms / 1000);
  const m = String(Math.floor(total / 60)).padStart(2, '0');
  const s = String(total % 60).padStart(2, '0');
  el.timer.textContent = `${m}:${s}`;
}

/* ---------------- 工具条 ---------------- */

function buildToolbar() {
  buildSwatches($('swatches'));
  buildSwatches(el.presentSwatches);
  buildWidths();
}

/** 生成颜色按钮组（窗口工具条与放映工具条共用）。 */
function buildSwatches(container) {
  container.innerHTML = '';
  for (const color of COLORS) {
    const b = document.createElement('button');
    b.className = 'swatch' + (color === state.color ? ' on' : '');
    b.style.background = color;
    b.dataset.color = color;
    b.title = color;
    b.addEventListener('click', () => {
      state.color = color;
      // 选颜色意味着要用笔：橡皮切回画笔，放映中的「翻页态」也一并拿起笔
      if (state.tool === 'eraser' || (state.presenting && state.tool === 'pointer')) {
        state.tool = 'pen';
      }
      updateToolButtons();
      if (state.presenting) showPresentBar();
    });
    container.appendChild(b);
  }
}

function buildWidths() {
  const wc = $('widths');
  wc.innerHTML = '';
  for (const w of WIDTHS) {
    const b = document.createElement('button');
    b.className = 'width-btn' + (w === state.width ? ' on' : '');
    b.dataset.width = String(w);
    b.title = `粗细 ${w}`;
    const dot = document.createElement('span');
    dot.className = 'width-dot';
    const size = 4 + w * 1.6;
    dot.style.width = `${size}px`;
    dot.style.height = `${size}px`;
    b.appendChild(dot);
    b.addEventListener('click', () => {
      state.width = w;
      updateToolButtons();
    });
    wc.appendChild(b);
  }
}

/** 同步两处工具条的按钮状态。 */
function updateToolButtons() {
  const isPointer = state.tool === 'pointer';

  $('t-pointer').classList.toggle('on', isPointer);
  $('t-pen').classList.toggle('on', state.tool === 'pen');
  $('t-highlighter').classList.toggle('on', state.tool === 'highlighter');
  $('t-eraser').classList.toggle('on', state.tool === 'eraser');

  // 放映工具条
  $('p-pointer').classList.toggle('on', isPointer);
  $('p-pen').classList.toggle('on', state.tool === 'pen');
  $('p-highlighter').classList.toggle('on', state.tool === 'highlighter');
  $('p-eraser').classList.toggle('on', state.tool === 'eraser');
  $('p-laser').classList.toggle('on', laserActive);
  $('p-undo').disabled = strokesFor(inkKey()).length === 0;
  $('p-clear').disabled = strokesFor(inkKey()).length === 0;

  const spotOn = el.spotlight.classList.contains('on');

  $('p-board').classList.toggle('on', boardOn);
  $('t-board').classList.toggle('on', boardOn);
  $('p-spot').classList.toggle('on', spotOn);
  $('t-spot').classList.toggle('on', spotOn);

  // 所有颜色按钮（两处）的状态
  for (const b of document.querySelectorAll('.swatch')) {
    b.classList.toggle('on', b.dataset.color === state.color);
  }
  for (const b of document.querySelectorAll('.width-btn')) {
    b.classList.toggle('on', Number(b.dataset.width) === state.width);
  }

  $('t-undo').disabled = strokesFor(inkKey()).length === 0;
  $('t-redo').disabled = historyFor(inkKey()).redo.length === 0;

  el.ink.classList.toggle('pan-mode', state.tool === 'eraser');
  // 用画笔时显示十字光标（放映模式下平时是隐藏的）
  el.ink.classList.toggle('show-cursor', isDrawingTool());
  // 拿起笔之后热区不再响应点击，提示框要立刻收掉，避免误导
  if (isDrawingTool()) hideLinkHint();
  // 拿笔时让播放器整层「不可点」，笔迹才能落在视频上
  el.mediaLayer.classList.toggle('ink-active', isDrawingTool());
  // 换到「鼠标」时立刻把光标要回来（否则老师会以为程序卡住了）
  if (state.presenting) notePointerActivity();
}

/* ---------------- 快捷键 ---------------- */

/**
 * 按钮笔（翻页器）认哪些键。
 *
 * 这类笔在系统看来就是个键盘，而**不同牌子发的键不一样**，老师也不会去配。
 * 下一页常见是 `PageDown` / `→` / `空格` / `回车`，「播放」键有的发 `回车`、
 * 有的干脆发媒体键；上一页是 `PageUp` / `←`。这里**全都收下** ——
 * 让老师插上就能用，比让他回去翻说明书重要得多。
 */
const NEXT_KEYS = [
  'PageDown',
  'ArrowRight',
  'ArrowDown',
  ' ',
  'Enter',
  'MediaTrackNext',
  'MediaPlayPause',
];
const PREV_KEYS = ['PageUp', 'ArrowLeft', 'ArrowUp', 'MediaTrackPrevious'];

/**
 * 两次翻页之间的最小间隔（毫秒）。
 *
 * 便宜的笔按住不放时会连发几十个 keydown，不拦一下就一路翻到底；
 * 老师有意快速点几下大约在 150ms 以上，所以这个值只拦「连发」。
 */
const NAV_REPEAT_MS = 130;
let lastNavAt = 0;

/** 这个键是不是翻页键（按钮笔/键盘都算）。 */
function isNavKey(key) {
  return NEXT_KEYS.includes(key) || PREV_KEYS.includes(key);
}

/**
 * 按翻页键前进/后退。返回是否处理了这个键。
 *
 * （黑板由 `goForward/goBack` 里的 `leaveBoard` 负责收掉 ——
 * 那条路同时也被工具条按钮、点击、滑动用着。）
 */
function navFromKey(key) {
  const next = NEXT_KEYS.includes(key);
  if (!next && !PREV_KEYS.includes(key)) return false;

  const now = performance.now();
  if (now - lastNavAt < NAV_REPEAT_MS) return true;
  lastNavAt = now;

  if (next) goForward();
  else goBack();
  return true;
}

function onKeyDown(e) {
  const inInput = e.target instanceof HTMLInputElement;

  if (inInput) {
    // 页码框里回车 = 跳到那一页（这时不能当成「下一页」）
    if (e.key === 'Enter') {
      const n = parseInt(el.pageInput.value, 10);
      if (!Number.isNaN(n)) goTo(n - 1);
      el.pageInput.blur();
      return;
    }
    if (e.key === 'Escape') {
      // 输入框里按 Esc 本来只是失焦。但放映时这一下也该能结束放映 ——
      // 触屏老师按 Esc 的意思就是「出去」，而这时候工具条可能正被收起，
      // Esc 往往是唯一的出口（曾经的 bug：只失焦、不退出）。
      el.pageInput.blur();
      if (state.presenting) setPresenting(false);
      return;
    }
    // 别的键里，翻页键要**放行**：老师可能点过页码框、焦点还留在那儿，
    // 这时候按按钮笔什么都不发生，他会以为笔坏了。
    if (!isNavKey(e.key)) return;
    el.pageInput.blur();
  }

  const ctrl = e.ctrlKey || e.metaKey;

  if (ctrl) {
    switch (e.key.toLowerCase()) {
      case 'o':
        e.preventDefault();
        chooseFile();
        return;
      case 's':
        e.preventDefault();
        saveAnnotations();
        return;
      case 'b':
        e.preventDefault();
        el.sidebar.classList.toggle('collapsed');
        setTimeout(recomputeFit, 240);
        return;
      case 'z':
        e.preventDefault();
        e.shiftKey ? redo() : undo();
        return;
      case 'y':
        e.preventDefault();
        redo();
        return;
      case '=':
      case '+':
        e.preventDefault();
        zoomBy(1.15);
        return;
      case '-':
        e.preventDefault();
        zoomBy(1 / 1.15);
        return;
      case '0':
        e.preventDefault();
        resetZoom();
        return;
      default:
        break;
    }
  }

  // 翻页（按钮笔 + 键盘）统一走这里，见 `navFromKey`
  if (navFromKey(e.key)) {
    e.preventDefault();
    return;
  }

  switch (e.key) {
    case 'Home':
      e.preventDefault();
      goTo(0);
      break;
    case 'End':
      e.preventDefault();
      goTo(state.info ? state.info.pageCount - 1 : 0);
      break;
    case 'F5':
      e.preventDefault();
      setPresenting(!state.presenting);
      break;
    case 'Escape':
      e.preventDefault();
      // 多级退出（与 WPS 一致）：先关掉临时遮挡，最后才结束放映，
      // 避免老师误按一次 Esc 就把放映关掉。
      // 设置排在**最前**：它盖在所有东西上面，Esc 该先关它。
      if (!el.settings.classList.contains('hidden')) {
        closeSettings();
      } else if (boardOn) {
        setBoard(false);
      } else if (el.spotlight.classList.contains('on')) {
        setSpotlight(false);
      } else if (laserActive) {
        laserActive = false;
        laserCtx.clearRect(0, 0, el.laser.width, el.laser.height);
        updateToolButtons();
      } else if (el.ctxMenu.classList.contains('on')) {
        closeCtxMenu();
      } else if (state.presenting) {
        setPresenting(false);
      } else if (state.zoom !== 1) {
        resetZoom();
      }
      break;
    case 'b':
    case 'B':
      // 按钮笔上那颗「黑屏」键大多就发 B：给它一块**能写**的黑板，
      // 比让屏幕单纯变黑有用得多
      setBoard(!boardOn);
      afterShortcut();
      break;
    case 's':
    case 'S':
      setSpotlight(!el.spotlight.classList.contains('on'));
      afterShortcut();
      break;
    case 'l':
    case 'L':
      laserActive = !laserActive;
      if (!laserActive) laserCtx.clearRect(0, 0, el.laser.width, el.laser.height);
      toast(laserActive ? '激光笔已开启' : '激光笔已关闭');
      afterShortcut();
      break;
    case 'p':
    case 'P':
      state.tool = 'pen';
      afterShortcut();
      break;
    case 'v':
    case 'V':
      state.tool = 'pointer';
      afterShortcut();
      break;
    case 'h':
    case 'H':
      state.tool = 'highlighter';
      afterShortcut();
      break;
    case 'e':
    case 'E':
      state.tool = 'eraser';
      afterShortcut();
      break;
    case 'n':
    case 'N':
      toggleNotes();
      break;
    case 'Delete':
      clearPage();
      break;
    default:
      break;
  }
}

/** 快捷键改变状态后统一刷新工具条与放映提示。 */
function afterShortcut() {
  updateToolButtons();
  if (state.presenting) showPresentBar();
}

/** 按当前模式重算适应缩放（放映模式铺满整屏，窗口模式留边距）。 */
function refit() {
  if (state.presenting) recomputeFitPresenting();
  else recomputeFit();
  // 整页可见时一律回正：此时「居中」才是老师预期的位置，
  // 留着上次的平移偏移会让人以为课件跑偏了
  if (state.zoom <= 1.001) {
    state.panX = 0;
    state.panY = 0;
  } else {
    clampPan();
  }
}

/** 重算缩放 → 重排 → 重绘 → 告知后端新档位。 */
function fitAndRepaint() {
  refit();
  repaintAtScale();
  scheduleScaleSync();
}

function zoomBy(factor) {
  state.zoom = clamp(state.zoom * factor, 0.2, 8);
  fitAndRepaint();
}

function resetZoom() {
  state.zoom = 1;
  fitAndRepaint();
}

function toggleNotes() {
  const hidden = el.notes.classList.toggle('hidden');
  $('btn-notes').classList.toggle('on', !hidden);
  if (!hidden) updateNotes(state.page);
}

/* ---------------- 拖拽打开 ---------------- */

window.addEventListener('dragover', (e) => {
  e.preventDefault();
  el.app.classList.add('dragging');
});

window.addEventListener('dragleave', (e) => {
  if (e.relatedTarget === null) el.app.classList.remove('dragging');
});

window.addEventListener('drop', async (e) => {
  e.preventDefault();
  el.app.classList.remove('dragging');
  const files = e.dataTransfer?.files;
  if (!files || files.length === 0) return;

  const file = files[0];
  // 浏览器安全限制：Tauri 会把真实路径挂在 name 上或提供 path 属性
  const path = file.path || file.name;
  if (!path) {
    toast('无法获取文件路径，请使用「选择课件」按钮', true);
    return;
  }
  openPath(path);
});

/* ---------------- 滚轮翻页 ---------------- */

let wheelAcc = 0;
let wheelLock = 0;

el.stage.addEventListener(
  'wheel',
  (e) => {
    if (!state.info) return;

    // Ctrl + 滚轮 = 缩放
    if (e.ctrlKey) {
      e.preventDefault();
      zoomBy(e.deltaY < 0 ? 1.08 : 1 / 1.08);
      return;
    }

    // 触控板的横向滚动不翻页
    if (Math.abs(e.deltaX) > Math.abs(e.deltaY)) return;

    const now = performance.now();
    if (now < wheelLock) return;

    wheelAcc += e.deltaY;
    if (Math.abs(wheelAcc) > 90) {
      wheelAcc > 0 ? goForward() : goBack();
      wheelAcc = 0;
      // 节流：避免触控板惯性滚动一次翻过多页
      wheelLock = now + 280;
    }
  },
  { passive: false }
);

/* ---------------- 触屏滑动翻页 ----------------
 *
 * 只在**放映**时启用，而且由 Pointer Events 统一处理（见 `onPresentPointerUp`）：
 * 窗口模式默认工具就是画笔，手指按下去一律落笔，
 * 再在旁边挂一套滑动翻页只会让「写字」时不时变成「翻页」。
 * 窗口模式翻页走缩略图、顶栏按钮、滚轮或键盘。
 */

/* ---------------- 窗口事件 ---------------- */

window.addEventListener('resize', () => {
  if (!state.info) return;
  // 走 refit：它会按新尺寸夹紧平移量，避免转屏/改窗口后幻灯片被推出去
  refit();
  repaintAtScale();
});

window.addEventListener('keydown', onKeyDown);

/* 放映时：指针靠近屏幕底部唤出工具条（触摸同样有效） */
window.addEventListener('pointermove', onPresentPointerMove);

/* 放映时：右键 = 上一页（WPS 行为）。
 *
 * 为什么把右键给「回翻」而不是「弹菜单」：
 * 老师讲课时回翻看上一页是最高频操作之一，而弹出菜单会打断节奏。
 * 菜单改由触屏长按唤出（见 `onLongPressStart`），
 * 或用键盘的方向键 / PageUp。 */
window.addEventListener('contextmenu', (e) => {
  if (!state.presenting) return;
  e.preventDefault();
  if (isDrawingTool()) return;
  prevPage();
});

/**
 * 当前全屏的是不是**媒体元素**（视频/音频），而不是我们的放映。
 *
 * 放映走的是 `<html>` 全屏（见 `setPresenting`），所以「不是 documentElement」
 * 就等于「是视频自己在全屏」。
 */
function isMediaFullscreen() {
  return document.fullscreenElement != null && document.fullscreenElement !== document.documentElement;
}

/**
 * 进视频全屏之前是不是正在放映。
 *
 * 退出视频全屏时 Tauri 会把**窗口**退出全屏（wry 只看到「没有全屏元素了」），
 * 放映的全屏就跟着丢了。所以记一下，退出时自己把它要回来 ——
 * 否则老师放完视频，放映就莫名其妙结束了。
 */
let presentingBeforeMediaFullscreen = false;

document.addEventListener('fullscreenchange', () => {
  // 视频自己的全屏**不参与对账**。
  //
  // 视频进全屏时，Tauri 会把**窗口**也设成全屏
  // （`tauri-runtime-wry` 监听 `ContainsFullScreenElementChanged` 后发
  //  `WindowMessage::SetFullscreen`）。这时候若拿窗口状态去推断放映状态，
  // 就会把「老师放了个视频」当成「进了放映」，进而 `setPresenting(true)`
  // 对 `<html>` 再要一次全屏 —— 那会**把视频的全屏元素抢走**，
  // 视频当场被踢出全屏。这正是「视频全屏后被强制退出」的另一半原因。
  if (isMediaFullscreen()) {
    presentingBeforeMediaFullscreen = state.presenting;
    return;
  }

  // 刚退出视频全屏：放映的全屏被 Tauri 顺手关掉了，自己再要回来。
  //
  // 这里**不能**走下面的对账 —— 对账看到「窗口不是全屏」，会把整个放映结束掉。
  if (presentingBeforeMediaFullscreen && state.presenting) {
    presentingBeforeMediaFullscreen = false;
    invoke('set_presentation_mode', { on: true }).catch(() => {});
    return;
  }
  presentingBeforeMediaFullscreen = false;

  // 以 **Rust 侧的原生全屏状态**为准，把界面拉回一致。
  //
  // 为什么非要对一次：全屏是原生窗口属性（Rust 改的），而 `body.presenting`
  // 是界面自己的状态，两者只靠 `setPresenting` 同步。一旦哪次切换被并发打断
  // （曾经的重入 bug），就会停在「窗口是全屏、界面却以为没在放映」上 ——
  // 工具条和 Esc 一起失效，触屏老师就被困住了。这里兜一次底，正常不该触发。
  //
  // 不看 `document.fullscreenElement`：原生全屏下它可能为空，会误判。
  invoke('is_presentation_mode')
    .then((fullscreen) => {
      if (fullscreen && !state.presenting && state.info) setPresenting(true);
      else if (!fullscreen && state.presenting) setPresenting(false);
    })
    .catch(() => {});
});

/* 点击空白处关闭右键菜单 */
window.addEventListener('pointerdown', (e) => {
  if (el.ctxMenu.classList.contains('on') && !el.ctxMenu.contains(e.target)) {
    closeCtxMenu();
    // 记下「这一下只是关菜单」，抬起时不要再翻一页
    menuDismissAtDown = true;
  }
});

/* ---------------- 放映工具条与菜单的事件绑定 ---------------- */

function bindPresentUi() {
  loadPresentBarPos();

  // 最小化 / 展开。收起是自动的（闲置），展开只由老师自己点。
  el.presentDock.addEventListener('click', () => {
    setPresentBarMinimized(!presentBarMinimized);
  });

  // 收起态的胶囊可以拖，位置会被记住
  bindPresentBarDrag();

  $('p-prev').addEventListener('click', goBack);
  $('p-next').addEventListener('click', goForward);
  $('p-exit').addEventListener('click', () => setPresenting(false));

  const pickTool = (tool) => {
    state.tool = tool;
    updateToolButtons();
  };
  $('p-pointer').addEventListener('click', () => pickTool('pointer'));
  // 再点一次画笔 = 收起画笔、回到单击翻页（否则老师没有「放下笔」的出口）
  $('p-pen').addEventListener('click', () => pickTool(state.tool === 'pen' ? 'pointer' : 'pen'));
  $('p-highlighter').addEventListener('click', () =>
    pickTool(state.tool === 'highlighter' ? 'pointer' : 'highlighter')
  );
  $('p-eraser').addEventListener('click', () =>
    pickTool(state.tool === 'eraser' ? 'pointer' : 'eraser')
  );

  $('p-undo').addEventListener('click', () => {
    undo();
    updateToolButtons();
  });
  $('p-clear').addEventListener('click', () => {
    clearPage();
    updateToolButtons();
  });

  $('p-board').addEventListener('click', () => {
    setBoard(!boardOn);
  });
  $('p-spot').addEventListener('click', () => {
    setSpotlight(!el.spotlight.classList.contains('on'));
  });
  $('p-laser').addEventListener('click', () => {
    laserActive = !laserActive;
    if (!laserActive) laserCtx.clearRect(0, 0, el.laser.width, el.laser.height);
    updateToolButtons();
  });

  // 右键菜单项
  el.ctxMenu.addEventListener('click', (e) => {
    const item = e.target.closest('.ctx-item');
    if (!item) return;
    const act = item.dataset.act;
    closeCtxMenu();

    switch (act) {
      case 'next':
        nextPage();
        break;
      case 'prev':
        prevPage();
        break;
      case 'pen':
        state.tool = 'pen';
        break;
      case 'pointer':
        state.tool = 'pointer';
        break;
      case 'eraser':
        state.tool = 'eraser';
        break;
      case 'clear':
        clearPage();
        break;
      case 'board':
        setBoard(!boardOn);
        break;
      case 'laser':
        laserActive = !laserActive;
        if (!laserActive) laserCtx.clearRect(0, 0, el.laser.width, el.laser.height);
        break;
      case 'exit':
        setPresenting(false);
        break;
      default:
        break;
    }
    updateToolButtons();
  });
}

/* ---------------- 设置 ----------------
 *
 * 五块内容都属于「老师自己会想看一眼」：版本与升级、缓存占了多少、
 * 默认打开方式、操作习惯、出问题时去哪看日志。
 *
 * 「默认打开方式」与「操作习惯」跟欢迎页那两块共用一套渲染
 * （见 `refreshAssoc` / `applyInputMode`，都按类名绑到所有副本上），
 * 所以不会出现「两个地方显示的状态不一样」。
 */

function openSettings() {
  el.settings.classList.remove('hidden');
  refreshCacheUsage();
  // 打开就查一次：点进来多半就是想看有没有新版
  refreshUpdate();
}

function closeSettings() {
  el.settings.classList.add('hidden');
}

/** 字节数转成人看的形式。 */
function formatBytes(bytes) {
  if (!bytes) return '0 MB';
  const mb = bytes / 1048576;
  if (mb >= 1024) return `${(mb / 1024).toFixed(2)} GB`;
  if (mb >= 1) return `${mb.toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

async function refreshCacheUsage() {
  try {
    const u = await invoke('cache_usage');
    el.setCacheTotal.textContent = formatBytes(u.total);
    el.setCacheDetail.textContent =
      `办公软件出的画面 ${formatBytes(u.raster)}　·　` +
      `内置渲染缓存 ${formatBytes(u.bitmap)}　·　` +
      `旧版 .ppt 转换产物 ${formatBytes(u.legacy)}`;
  } catch (e) {
    el.setCacheDetail.textContent = `读不到缓存占用：${e}`;
  }
}

async function clearCache(btn) {
  btn.disabled = true;
  const before = el.setCacheTotal.textContent;
  try {
    const u = await invoke('clear_cache', { kind: 'all' });
    el.setCacheTotal.textContent = formatBytes(u.total);
    el.setCacheDetail.textContent = `已清理（原来 ${before}）。正在讲的这份课件保留着。`;
    toast('缓存已清理');
  } catch (e) {
    toast(`清理失败：${e}`, true);
  } finally {
    btn.disabled = false;
  }
}

/** 查一次更新，把结果写进「关于与更新」。 */
async function refreshUpdate() {
  el.setUpdateHint.textContent = '正在检查新版本…';
  el.updateBar.hidden = true;
  el.btnDoUpdate.hidden = true;
  el.setNotes.hidden = true;

  try {
    const st = await invoke('check_update');
    el.setVersion.textContent = st.current;

    // latest 为 null = 仓库里还没有发布过任何版本，这不是错误
    if (st.latest === null) {
      el.setUpdateHint.textContent = '仓库里还没有发布过版本，暂时没有可更新的内容。';
      return;
    }
    if (!st.hasUpdate) {
      el.setUpdateHint.textContent = `已是最新版本（${st.current}）。`;
      return;
    }

    el.setUpdateHint.textContent =
      `发现新版本 ${st.latest}（约 ${st.sizeMb.toFixed(1)} MB）。` +
      '更新会自动挑最快的下载线路；装好后会自动重新打开。';
    if (st.notes) {
      el.setNotesBody.textContent = st.notes;
      el.setNotes.hidden = false;
    }
    el.btnDoUpdate.hidden = false;
    el.btnDoUpdate.disabled = false;
    el.btnDoUpdate.textContent = '立即更新';
    markUpdateDot(true);
  } catch (e) {
    el.setUpdateHint.textContent = `检查更新失败：${e}`;
  }
}

/** 工具栏上那个小圆点：有新版才亮。 */
function markUpdateDot(on) {
  el.settingsDot.hidden = !on;
}

async function doUpdate(btn) {
  btn.disabled = true;
  btn.textContent = '准备中…';
  try {
    await invoke('start_update');
    // 之后全靠 `update-progress` 事件推进度
  } catch (e) {
    btn.disabled = false;
    btn.textContent = '立即更新';
    el.setUpdateHint.textContent = `更新失败：${e}`;
    toast(`更新失败：${e}`, true);
  }
}

/** 更新进度：后端每推进一步就调一次。 */
function onUpdateProgress(p) {
  if (!p) return;
  if (p.phase === 'probing') {
    el.setUpdateHint.textContent = '正在挑选最快的下载线路…';
    el.updateBar.hidden = false;
    el.updateBarFill.style.width = '2%';
  } else if (p.phase === 'downloading') {
    // 文案里已经带了进度与速度，直接显示
    el.setUpdateHint.textContent = p.message;
    el.updateBar.hidden = false;
    el.updateBarFill.style.width = `${Math.max(2, Math.min(100, p.percent)).toFixed(1)}%`;
  } else if (p.phase === 'verifying') {
    el.setUpdateHint.textContent = '正在校验安装包…';
    el.updateBar.hidden = false;
    el.updateBarFill.style.width = '100%';
  } else if (p.phase === 'installing') {
    el.setUpdateHint.textContent = '正在安装，请在弹出的系统提示里点「是」。装好后会自动重新打开。';
    el.updateBar.hidden = false;
    el.updateBarFill.style.width = '100%';
  } else if (p.phase === 'failed') {
    el.setUpdateHint.textContent = `更新失败：${p.message}`;
    el.updateBar.hidden = true;
    el.btnDoUpdate.hidden = false;
    el.btnDoUpdate.disabled = false;
    el.btnDoUpdate.textContent = '重试';
  }
}

/**
 * 启动时静默检查一次更新。
 *
 * 只做两件事：有新版就在设置按钮上点一个小圆点，并把结果留给设置页。
 * **不弹窗、不打断**：老师打开课件是要上课，不是来装软件的。
 */
async function checkUpdateOnStart() {
  try {
    const st = await invoke('check_update');
    el.setVersion.textContent = st.current;
    if (st.hasUpdate) {
      markUpdateDot(true);
      console.info(`发现新版本 ${st.latest}（当前 ${st.current}），可在设置里更新`);
    }
  } catch (e) {
    // 没网、被墙、仓库还没发布 —— 都不值得打扰老师
    console.debug('启动时检查更新失败（忽略）', e);
  }
}

/* ---------------- 初始化 ---------------- */

function bindUi() {
  $('btn-open').addEventListener('click', chooseFile);
  $('btn-welcome-open').addEventListener('click', chooseFile);
  $('btn-sidebar').addEventListener('click', () => {
    el.sidebar.classList.toggle('collapsed');
    setTimeout(recomputeFit, 240);
  });

  $('btn-first').addEventListener('click', () => goTo(0));
  $('btn-prev').addEventListener('click', prevPage);
  $('btn-next').addEventListener('click', nextPage);
  $('btn-last').addEventListener('click', () => goTo(state.info ? state.info.pageCount - 1 : 0));

  $('btn-zoom-in').addEventListener('click', () => zoomBy(1.15));
  $('btn-zoom-out').addEventListener('click', () => zoomBy(1 / 1.15));
  $('btn-zoom-reset').addEventListener('click', resetZoom);

  $('btn-present').addEventListener('click', () => setPresenting(!state.presenting));
  $('btn-notes').addEventListener('click', toggleNotes);
  $('btn-save').addEventListener('click', () => saveAnnotations());

  $('t-pointer').addEventListener('click', () => {
    state.tool = 'pointer';
    updateToolButtons();
  });
  $('t-pen').addEventListener('click', () => {
    state.tool = 'pen';
    updateToolButtons();
  });
  $('t-highlighter').addEventListener('click', () => {
    state.tool = 'highlighter';
    updateToolButtons();
  });
  $('t-eraser').addEventListener('click', () => {
    state.tool = 'eraser';
    updateToolButtons();
  });
  $('t-undo').addEventListener('click', undo);
  $('t-redo').addEventListener('click', redo);
  $('t-clear').addEventListener('click', clearPage);
  $('t-board').addEventListener('click', () => setBoard(!boardOn));
  $('t-spot').addEventListener('click', () => setSpotlight(!el.spotlight.classList.contains('on')));

  el.pageInput.addEventListener('focus', () => el.pageInput.select());

  // 设置：工具栏入口、关闭、点遮罩空白处关闭
  $('btn-settings').addEventListener('click', () => {
    if (el.settings.classList.contains('hidden')) openSettings();
    else closeSettings();
  });
  $('btn-settings-close').addEventListener('click', closeSettings);
  el.settings.addEventListener('click', (e) => {
    // 只有点在遮罩本身（不是卡片上）才关
    if (e.target === el.settings) closeSettings();
  });

  $('btn-check-update').addEventListener('click', refreshUpdate);
  $('btn-do-update').addEventListener('click', (e) => doUpdate(e.currentTarget));
  $('btn-clear-cache').addEventListener('click', (e) => clearCache(e.currentTarget));
  $('btn-open-log').addEventListener('click', async () => {
    try {
      await invoke('open_log_dir');
    } catch (e) {
      toast(`打不开日志文件夹：${e}`, true);
    }
  });

  // 文件关联：欢迎页与设置页各有一个入口（靠 data-assoc 绑定）
  for (const btn of document.querySelectorAll('[data-assoc]')) {
    btn.addEventListener('click', () => {
      if (btn.dataset.assoc === 'register') registerAssoc();
      else openAssocSettings();
    });
  }

  // 操作方式：老师选一次，之后一直按它走（见 `applyInputMode`）
  for (const chip of document.querySelectorAll('.mode-chip')) {
    chip.addEventListener('click', () => setInputMode(chip.dataset.mode));
  }
}

async function loadRecent() {
  try {
    const list = await invoke('recent_files');
    el.recent.innerHTML = '';
    for (const path of list.slice(0, 6)) {
      const b = document.createElement('button');
      b.className = 'recent-item';
      b.textContent = path;
      b.title = path;
      b.addEventListener('click', () => openPath(path));
      el.recent.appendChild(b);
    }
  } catch {
    /* 最近列表不可用不影响使用 */
  }
}

/* ---------------- 默认打开方式（文件关联） ----------------
 *
 * Windows 8 起，默认程序由带系统签名哈希的 UserChoice 决定，
 * 任何程序都不能静默改写（WPS 同样绕不过）。因此这里的分工是：
 *   「注册到本应用」= 把 ProgID / 打开方式 / Capabilities 写进 HKCU；
 *   「系统默认应用」= 调起系统设置页，由老师点一次「设为默认值」。
 * 不谎报「已自动设为默认」。
 */

async function refreshAssoc() {
  const lists = document.querySelectorAll('.assoc-list');
  const hints = document.querySelectorAll('.assoc-summary');
  if (lists.length === 0) return;

  let text = '';
  try {
    const items = await invoke('associations_status');

    for (const list of lists) {
      list.innerHTML = '';
      for (const it of items) {
        const chip = document.createElement('span');
        // `pointsHere` 才是「这一份程序真的在管这个格式」：注册表里那条
        // 可能指着同名的另一个副本（按用户装过的那份），双击起来的是它
        const state = it.isDefault ? 'default' : it.pointsHere ? 'registered' : '';
        chip.className = `assoc-chip ${state}`.trim();
        chip.textContent = `.${it.ext}`;
        chip.title = it.isDefault
          ? `${it.label}：已是默认打开方式`
          : it.pointsHere
            ? `${it.label}：已注册，当前默认是 ${it.currentHandler || '其它程序'}`
            : it.registered
              ? `${it.label}：注册指向的是另一个副本，点「注册到本应用」改过来`
              : `${it.label}：尚未注册`;
        list.appendChild(chip);
      }
    }

    const allDefault = items.length > 0 && items.every((i) => i.isDefault);
    if (allDefault) {
      text = '已全部设为默认，双击课件文件即可直接用本应用打开。';
    } else {
      const pending = items.filter((i) => !i.isDefault).map((i) => `.${i.ext}`);
      text = pending.length
        ? `${pending.join(' ')} 还没设为默认。点「注册到本应用」后，` +
          '若系统已锁定默认程序，再到「系统默认应用」里点一次确认即可。'
        : '';
    }
  } catch (e) {
    text = `无法读取关联状态：${e}`;
  }
  for (const hint of hints) {
    hint.textContent = text;
  }
}

async function registerAssoc() {
  // 欢迎页与设置页各有一个入口，一起禁用，免得连点两次
  const buttons = [...document.querySelectorAll('[data-assoc="register"]')];
  for (const b of buttons) b.disabled = true;
  try {
    const outcome = await invoke('register_associations');
    await refreshAssoc();
    if (outcome.allDefault) {
      toast('已把本应用设为这些格式的默认打开方式');
    } else {
      toast('已注册到「打开方式」列表；系统默认值需要在设置里点一次确认');
    }
  } catch (e) {
    toast(`注册失败：${e}`, true);
  } finally {
    for (const b of buttons) b.disabled = false;
  }
}

async function openAssocSettings() {
  try {
    await invoke('open_default_apps_settings');
  } catch (e) {
    toast(`无法打开系统设置：${e}`, true);
  }
}

async function init() {
  document.title = 'OPPV-BUILD-X1';
  bindUi();
  bindPresentUi();
  // 先定下操作方式：它决定后面所有控件的尺寸与标签。
  // 这里要等一下：绿色版读不到注册表，但装过的机器上这一步要去问后台
  await loadInputMode();
  buildToolbar();
  updateToolButtons();
  setDocControlsEnabled(false);

  // 关闭窗口前提醒保存标注
  window.addEventListener('beforeunload', (e) => {
    if (hasUnsavedAnnotations()) {
      e.preventDefault();
      e.returnValue = '';
    }
  });

  // 已经开着的实例收到新课件时，第二个进程会把它转过来（见 `drainPendingOpen`）。
  // 先注册监听再取件，两条路都不会漏。
  try {
    await T.event.listen('open-file', () => drainPendingOpen());
  } catch (e) {
    console.warn('订阅 open-file 失败', e);
  }

  // 更新的进度由后台线程推过来（测速 → 下载 → 校验 → 安装）
  try {
    await T.event.listen('update-progress', (ev) => onUpdateProgress(ev && ev.payload));
  } catch (e) {
    console.warn('订阅 update-progress 失败', e);
  }

  // 静默查一次更新：有新版只在设置入口上点个小圆点，不打断老师
  checkUpdateOnStart();

  // 后台已经用本机办公软件把**当前这份**课件的第一页出好了。
  //
  // 两种情况会走到这里：
  //   1. 打开课件时正在等它（`waitForUpgrade` 挂着回调）→ 交给等待方，
  //      它会重载一次并收起等待界面；
  //   2. 已经等超时了、正在看内置渲染 → 这里悄悄换成办公软件出的那一份。
  //      页码、缩放、平移、标注全都不动：换的只是**像素来源**，
  //      老师的视角不该因为一次后台优化而跳动。
  try {
    await T.event.listen('raster-first-page', async (ev) => {
      const payload = ev && ev.payload;
      if (upgradeWaiter) {
        await upgradeWaiter(payload);
        return;
      }
      if (!payload || !state.info || state.info.path !== payload.path) return;
      try {
        const info = await invoke('open_document', {
          path: payload.path,
          viewportWidth: rasterWidthHint(),
        });
        state.info = info;
        // 页数不会因为出图而变；真不一致就退回首页，
        // 而不是把老师留在一个已经不存在的页号上
        if (state.page >= info.pageCount) state.page = 0;
        // 缩略图缓存是按来源渲染的，必须整批重建
        buildThumbs();
        await showPage(state.page, { silent: true, mode: 'keep', transition: false });
        // 这里**不再** prewarm：还没出好的页由出图任务按顺序补，
        // 显示层遇到它们会如实说「正在生成」，不必再排队占 CPU
        console.info(
          `画面已切换到本机办公软件出的图（已就绪 ${payload.readyPages}/${payload.pageCount} 页）`
        );
      } catch (e) {
        console.warn('切换到办公软件出的画面失败，继续用内置渲染', e);
      }
    });
  } catch (e) {
    console.warn('订阅 raster-first-page 失败', e);
  }
  // 真退出前，后端会给一次落盘标注的机会。
  //
  // 关窗只是隐藏窗口、文档还在内存里，所以那条路根本不会走到这儿；
  // 只有托盘「退出」和闲置自动退出才会 —— 那两种情况下不存就真丢了。
  try {
    await T.event.listen('app-quitting', async () => {
      if (hasUnsavedAnnotations()) await saveAnnotations({ quiet: true });
      // 不管存没存上都得回话，否则后端要白等到超时才退
      try {
        await invoke('ready_to_quit');
      } catch (e) {
        console.warn('回话 ready_to_quit 失败', e);
      }
    });
  } catch (e) {
    console.warn('订阅 app-quitting 失败', e);
  }

  try {
    const info = await invoke('startup_info');
    await loadRecent();
    // 关联状态只读注册表，很便宜；放在欢迎页展示便于老师一次性配好
    await refreshAssoc();

    if (info.file) {
      // 双击课件文件启动：直接打开，不经过欢迎页
      await openPath(info.file);
    } else if (info.renderThreads) {
      // 在欢迎页展示运行环境，便于老师反馈问题时描述机器
      const p = document.createElement('p');
      p.style.fontSize = '11px';
      p.style.color = 'var(--text-faint)';
      p.textContent = `${info.cpuThreads} 核 CPU · ${info.renderThreads} 渲染线程`;
      el.welcome.appendChild(p);
    }

    // 上一次转交过来的课件（如果有）：比命令行参数更新，所以放最后开
    await drainPendingOpen();
  } catch (e) {
    toast(`初始化失败：${e}`, true);
  }
}

init();
