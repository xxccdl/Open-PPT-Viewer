/**
 * OpenPPTView 蓝屏报告服务。
 *
 * 零依赖，只用 Node 自带模块 —— 机房服务器上不必为一个后台去维护 node_modules。
 *
 *   node server/crash/server.mjs
 *
 * 环境变量：
 *   HOST      监听地址，默认 127.0.0.1（要外部访问就改成 0.0.0.0）
 *   PORT      端口，默认 8787
 *   TOKEN     可选。设了之后客户端必须带 `Authorization: Bearer <TOKEN>`
 *   DATA_DIR  报告存放目录，默认与本文件同级的 `data/`
 *   STATE_DIR 账号与会话目录，默认 `<DATA_DIR>/_state/`
 *
 * 三块职责：
 *
 * 1. **收包**：`POST /api/v1/crash`，客户端整个 zip 放在请求体里。
 * 2. **后台**：`GET /`（网页）、`/api/v1/crashes` 等，给管理员看报告。
 * 3. **账号**：第一个注册的人是管理员，之后注册的是只读成员。
 *
 * 上传接口不做登录校验（蓝屏时那台机器不可能有账号），
 * 后台接口一律要会话。
 */

import http from 'node:http';
import { createReadStream, createWriteStream, promises as fs } from 'node:fs';
import { pipeline } from 'node:stream/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import crypto from 'node:crypto';

import { Auth, HttpError } from './auth.mjs';
import { Zip } from './zip.mjs';
import { explain, diagnose, scanLog } from './analysis.mjs';
import { createRemoteRoutes } from './remote.mjs';
import { Org } from './org.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOST = process.env.HOST || '127.0.0.1';
const PORT = Number(process.env.PORT || 8787);
const TOKEN = (process.env.TOKEN || '').trim();
const DATA_DIR = path.resolve(process.env.DATA_DIR || path.join(HERE, 'data'));
const STATE_DIR = path.resolve(process.env.STATE_DIR || path.join(DATA_DIR, '_state'));
const WEB_DIR = path.join(HERE, 'web');

/** 单个包的上限。客户端自己封顶 64MB，这里再拦一道，防的是别人乱传。 */
const MAX_BYTES = 96 * 1024 * 1024;
/** 后台请求体上限（只收 JSON）。 */
const MAX_JSON = 1024 * 1024;
/** 日志预览读这么多（整份下载另说）。 */
const LOG_PREVIEW_BYTES = 512 * 1024;

const COOKIE = 'oppv_admin';
const SESSION_MAX_AGE = 30 * 24 * 3600;

const auth = new Auth(STATE_DIR);
/** 学校 / 班级 / 成员 / 机器授权（见 `org.mjs`）。 */
const org = new Org(STATE_DIR);

/** 远程协助：机器心跳、命令、屏幕画面（见 `remote.mjs`）。 */
const remote = createRemoteRoutes({
  json,
  requireUser,
  machineAuth,
  clientIp,
  org,
});

/* ------------------------------------------------------------------ */
/* 小工具                                                              */
/* ------------------------------------------------------------------ */

function json(res, status, body, extraHeaders = {}) {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(text),
    'Cache-Control': 'no-store',
    ...extraHeaders,
  });
  res.end(text);
}

function clientIp(req) {
  const fwd = String(req.headers['x-forwarded-for'] || '')
    .split(',')[0]
    .trim();
  if (fwd) return fwd;
  return String(req.socket.remoteAddress || '').replace(/^::ffff:/, '');
}

function parseCookies(req) {
  const raw = req.headers.cookie || '';
  const out = {};
  for (const part of raw.split(';')) {
    const i = part.indexOf('=');
    if (i < 0) continue;
    out[part.slice(0, i).trim()] = decodeURIComponent(part.slice(i + 1).trim());
  }
  return out;
}

function sessionCookie(token) {
  const bits = [
    `${COOKIE}=${token}`,
    'Path=/',
    'HttpOnly',
    'SameSite=Lax',
    `Max-Age=${token ? SESSION_MAX_AGE : 0}`,
  ];
  // 不加 Secure：后台既走 https，也会在证书生效前用 http 打开。
  // 带 Secure 的 Cookie 在 http 页面里会被浏览器直接丢掉，
  // 表现就是「登录成功后刷新又回到登录页」。
  return bits.join('; ');
}

function bearer(req) {
  const m = /^Bearer\s+(.+)$/i.exec(req.headers.authorization || '');
  return m ? m[1].trim() : '';
}

/**
 * 机器认证：一枚令牌对应「哪台电脑、哪个班」。
 *
 * 三种情况：
 * - 全局 `TOKEN`（环境变量设的那个）：运维用，视为未分类，能上传但不会属于任何班；
 * - 机器令牌（app 里授权后拿到的）：查出 `classId` / `schoolId`，报告与远程都按它归属；
 * - 没带令牌：沿用旧行为 —— 没设 TOKEN 就放行（落未分类），设了就拒绝。
 *
 * `invalid` 单独标出来：令牌失效（被撤销）时**仍然收报告**，只是归到未分类
 * 并回一句提示 —— 装机时还没登录就蓝屏的情况不少，把报告丢掉才是更坏的结果。
 */
async function machineAuth(req) {
  const token = bearer(req);
  if (TOKEN && token === TOKEN) {
    return { ok: true, classId: '', schoolId: '', hwid: '', invalid: false };
  }
  if (token) {
    const hit = org.resolveToken(token);
    if (hit) return { ok: true, ...hit, invalid: false };
    return { ok: false, classId: '', schoolId: '', hwid: '', invalid: true };
  }
  if (!TOKEN) return { ok: true, classId: '', schoolId: '', hwid: '', invalid: false };
  return { ok: false, classId: '', schoolId: '', hwid: '', invalid: false };
}

/** 报告 id 只允许字母数字和 `-_.`：它要变成文件名，不能被路径穿越。 */
function safeId(raw) {
  const id = String(raw || '').trim();
  if (!id) return null;
  if (!/^[A-Za-z0-9._-]{4,80}$/.test(id)) return null;
  return id;
}

async function readJsonBody(req) {
  let size = 0;
  const chunks = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_JSON) throw new HttpError(413, '请求体太大');
    chunks.push(chunk);
  }
  if (!size) return {};
  try {
    return JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    throw new HttpError(400, '不是合法的 JSON');
  }
}

/* ------------------------------------------------------------------ */
/* 账号                                                                */
/* ------------------------------------------------------------------ */

/** 当前登录用户；没有有效会话返回 null。 */
async function currentUser(req) {
  const token = parseCookies(req)[COOKIE] || '';
  return auth.userFor(token);
}

/**
 * 后台接口的统一入口守卫。
 *
 * 除了会话，还要求一个自定义头：跨站表单传不了 `X-Oppv-Client`，
 * 这样 SameSite=Lax 之外又多一道很便宜的 CSRF 防线。
 */
async function requireUser(req, { write = false, admin = false } = {}) {
  const user = await currentUser(req);
  if (!user) throw new HttpError(401, '请先登录');
  if (write && req.headers['x-oppv-client'] !== 'admin') {
    throw new HttpError(403, '缺少客户端标识');
  }
  if (admin && user.role !== 'platform') throw new HttpError(403, '需要平台超管权限');
  return user;
}

/**
 * 这个账号能看见的班级范围。`all` = 平台超管。
 *
 * 未分类（`classId` 为空）不在任何班级范围内 —— 只有平台超管能看，
 * 见 `visibleReport`。
 */
function scopeOf(user) {
  return org.scopeOf(user);
}

/** 一条报告是否在这个范围里。 */
function visibleReport(meta, scope) {
  if (scope.all) return true;
  const classId = String((meta && meta.classId) || '');
  if (!classId) return false;
  return scope.classIds.includes(classId);
}

/** 能不能改这个班的报告（改状态、删除）。 */
function canManageReport(user, meta) {
  if (!user) return false;
  if (user.role === 'platform') return true;
  const classId = String((meta && meta.classId) || '');
  if (!classId) return false;
  return org.canManageClass(user, classId);
}

/* ------------------------------------------------------------------ */
/* 报告存储                                                            */
/* ------------------------------------------------------------------ */

function zipPathOf(id) {
  return path.join(DATA_DIR, `${id}.zip`);
}

function metaPathOf(id) {
  return path.join(DATA_DIR, `${id}.json`);
}

async function readMeta(id) {
  try {
    return JSON.parse(await fs.readFile(metaPathOf(id), 'utf8'));
  } catch {
    return null;
  }
}

/** 所有报告的元数据（按收到时间倒序）。 */
async function readAllMetas() {
  let names = [];
  try {
    names = await fs.readdir(DATA_DIR);
  } catch {
    /* 目录还不存在 = 还没收到过报告 */
  }
  const items = [];
  for (const name of names) {
    if (!name.endsWith('.json')) continue;
    try {
      items.push(JSON.parse(await fs.readFile(path.join(DATA_DIR, name), 'utf8')));
    } catch {
      /* 坏掉的元数据跳过，不影响别的 */
    }
  }
  items.sort((a, b) => String(b.receivedAt).localeCompare(String(a.receivedAt)));
  return items;
}

/**
 * 请求头只能放可见 ASCII，所以客户端把中文（例如「未知（未读到蓝屏事件）」）
 * 百分号编码后加 `u:` 前缀发过来。这里解回去，后台列表才显示得正常。
 */
function decodeHeader(raw) {
  const s = String(raw || '');
  if (!s.startsWith('u:')) return s;
  try {
    return decodeURIComponent(s.slice(2));
  } catch {
    return s.slice(2);
  }
}

/** 「同一场蓝屏」的判定半径：事件与 minidump 的写入时刻差几秒是常事。 */
const SAME_CRASH_MS = 10 * 60 * 1000;

/**
 * 磁盘上有没有同一场蓝屏的报告。
 *
 * 客户端的 `seen` 已经保证「一台机器只报一次」，但装机房里总有还没升级的旧版本，
 * 它们会把同一场蓝屏报两遍（一条来自事件日志、一条来自转储）。后台看到的就是
 * 「时间差几秒、错误码一样」的两条记录 —— 像是两节课之间崩了两次，很误导。
 * 这里按时刻认重：10 分钟内、错误码相同或有一边是「未知」的，算同一场。
 */
async function findTwin(bugcheck, occurredAt) {
  if (!occurredAt) return '';
  const at = new Date(occurredAt).getTime();
  if (Number.isNaN(at)) return '';
  let items = [];
  try {
    items = await readAllMetas();
  } catch {
    return '';
  }
  const unknown = (code) => !code || /^未知/.test(code);
  for (const m of items) {
    const t = new Date(m.occurredAt).getTime();
    if (Number.isNaN(t) || Math.abs(t - at) > SAME_CRASH_MS) continue;
    const code = String(m.bugcheck || '');
    if (code === bugcheck || unknown(code) || unknown(bugcheck)) return m.id;
  }
  return '';
}

/* ---- 处理状态与备注（管理员自己加的，不改原报告） ---- */

const STATUSES = ['new', 'investigating', 'fixed', 'ignored'];
const annotationsPath = path.join(STATE_DIR, 'annotations.json');
let annotations = {};

async function loadAnnotations() {
  try {
    annotations = JSON.parse(await fs.readFile(annotationsPath, 'utf8')) || {};
  } catch {
    annotations = {};
  }
}

async function saveAnnotations() {
  const tmp = `${annotationsPath}.${process.pid}.tmp`;
  await fs.writeFile(tmp, JSON.stringify(annotations, null, 2), 'utf8');
  await fs.rename(tmp, annotationsPath);
}

function annotationOf(id) {
  const a = annotations[id] || {};
  return {
    status: STATUSES.includes(a.status) ? a.status : 'new',
    note: typeof a.note === 'string' ? a.note : '',
    by: a.by || '',
    at: a.at || '',
  };
}

/* ------------------------------------------------------------------ */
/* 上传                                                                */
/* ------------------------------------------------------------------ */

async function handleUpload(req, res) {
  const machine = await machineAuth(req);
  // 带了失效的令牌（被撤销）：仍然收，落未分类，并在响应里提醒客户端重新登录
  if (!machine.ok && !machine.invalid) {
    json(res, 401, { ok: false, error: 'unauthorized' });
    req.resume();
    return;
  }
  if ((req.headers['content-type'] || '').split(';')[0] !== 'application/zip') {
    json(res, 415, { ok: false, error: 'expect application/zip' });
    req.resume();
    return;
  }

  const declared = Number(req.headers['content-length'] || 0);
  if (declared > MAX_BYTES) {
    json(res, 413, { ok: false, error: 'too large' });
    req.resume();
    return;
  }

  let id = safeId(req.headers['x-crash-id']);
  if (!id) {
    // 客户端没给合法 id 也不能丢报告：自己起一个
    id = `anon-${new Date().toISOString().replace(/[-:T]/g, '').slice(0, 15)}-${crypto
      .randomBytes(3)
      .toString('hex')}`;
  }

  const bugcheck = decodeHeader(req.headers['x-bugcheck']);
  const occurredAt = String(req.headers['x-occurred-at'] || '');
  // 同一场蓝屏只留一条记录（见 `findTwin`）：老版本客户端可能报两遍，
  // 那时后台就会出现两条「时间差几秒、错误码一样」的记录，看着像两次独立故障。
  // 演示报告不参与：它是同一台机器上一次一次手点出来的「假蓝屏」，
  // 每一次都该进库里，否则验证流程时会以为「上传又坏了」。
  const twin =
    String(req.headers['x-report-mode'] || 'real') === 'demo'
      ? ''
      : await findTwin(bugcheck, occurredAt);
  if (twin) {
    req.resume();
    console.log(`[${new Date().toISOString()}] 忽略重复上传 ${id}（和 ${twin} 是同一场蓝屏）`);
    json(res, 200, { ok: true, id: twin, duplicated: true });
    return;
  }

  await fs.mkdir(DATA_DIR, { recursive: true });
  const zipPath = zipPathOf(id);
  const metaPath = metaPathOf(id);
  const partPath = `${zipPath}.part`;

  let received = 0;
  req.on('data', (chunk) => {
    received += chunk.length;
    if (received > MAX_BYTES) {
      // 超限：把连接断掉，别把磁盘写满
      req.destroy(new Error('too large'));
    }
  });

  try {
    await pipeline(req, createWriteStream(partPath));
  } catch (e) {
    await fs.rm(partPath, { force: true });
    if (!res.headersSent && !res.writableEnded) {
      json(res, received > MAX_BYTES ? 413 : 400, { ok: false, error: String(e.message || e) });
    }
    return;
  }

  if (received === 0) {
    await fs.rm(partPath, { force: true });
    json(res, 400, { ok: false, error: 'empty body' });
    return;
  }

  await fs.rename(partPath, zipPath);

  const meta = {
    id,
    bytes: received,
    receivedAt: new Date().toISOString(),
    appVersion: String(req.headers['x-app-version'] || ''),
    bugcheck,
    occurredAt,
    mode: String(req.headers['x-report-mode'] || 'real'),
    remote: clientIp(req),
    // 归哪个班：来自机器令牌。没登录 / 令牌失效 → 空 = 未分类（只有平台超管看得到）
    classId: machine.classId || '',
    schoolId: machine.schoolId || '',
  };
  await fs.writeFile(metaPath, JSON.stringify(meta, null, 2), 'utf8');

  console.log(
    `[${meta.receivedAt}] 收到 ${id}（${(received / 1048576).toFixed(2)} MB，` +
      `${meta.bugcheck || '未知错误码'}，${meta.appVersion || '未知版本'}，${meta.mode}）`
  );
  json(res, 200, {
    ok: true,
    id,
    bytes: received,
    classId: meta.classId,
    ...(machine.invalid ? { warning: 'machine-token-invalid' } : {}),
  });
}

/* ------------------------------------------------------------------ */
/* 后台：列表与详情                                                     */
/* ------------------------------------------------------------------ */

function enrich(meta) {
  const info = explain(meta.bugcheck);
  const a = annotationOf(meta.id);
  const klass = meta.classId ? org.class(meta.classId) : null;
  return {
    ...meta,
    analysis: { code: info.code, title: info.title, severity: info.severity, known: info.known },
    status: a.status,
    note: a.note,
    updatedAt: a.at,
    updatedBy: a.by,
    // 后台表格里直接写「XX学校 · XX班」；未分类就是未分类
    className: klass ? klass.name : '',
    schoolName: klass ? klass.schoolName : '',
  };
}

/** 取一份报告并确认在这个账号的可见范围内；看不见就当作不存在。 */
async function visibleMeta(req, id) {
  const user = await requireUser(req);
  const meta = await readMeta(id);
  const scope = scopeOf(user);
  if (!meta || !visibleReport(meta, scope)) throw new HttpError(404, '没有这份报告');
  return { user, meta, scope };
}

async function handleList(req, res, url) {
  const user = await requireUser(req);
  const scope = scopeOf(user);
  const q = (url.searchParams.get('q') || '').trim().toLowerCase();
  const bugcheck = (url.searchParams.get('bugcheck') || '').trim().toUpperCase();
  const version = (url.searchParams.get('version') || '').trim();
  const mode = (url.searchParams.get('mode') || '').trim();
  const status = (url.searchParams.get('status') || '').trim();
  const limit = Math.min(Number(url.searchParams.get('limit') || 100), 500);

  // 先按可见范围裁剪，再筛选 —— 顺序反了的话，别人班的记录会通过「总数」泄露
  let items = (await readAllMetas()).filter((m) => visibleReport(m, scope)).map(enrich);

  const classId = (url.searchParams.get('class') || '').trim();
  if (classId) {
    // `none` = 未分类（没有归属的机器报上来的）。只有平台超管会这么筛
    items = items.filter((m) =>
      classId === 'none' ? !m.classId : String(m.classId || '') === classId
    );
  }

  if (q) {
    items = items.filter((m) =>
      [m.id, m.bugcheck, m.remote, m.appVersion, m.analysis.title, m.note]
        .join(' ')
        .toLowerCase()
        .includes(q)
    );
  }
  if (bugcheck) items = items.filter((m) => String(m.bugcheck).toUpperCase().includes(bugcheck));
  if (version) items = items.filter((m) => m.appVersion === version);
  if (mode) items = items.filter((m) => m.mode === mode);
  if (status) items = items.filter((m) => m.status === status);

  const total = items.length;
  json(res, 200, { ok: true, total, items: items.slice(0, limit) });
}

async function handleDetail(req, res, id) {
  const { meta } = await visibleMeta(req, id);

  const detail = {
    ...enrich(meta),
    files: [],
    summary: null,
    system: [],
    repair: null,
    logs: [],
    error: '',
  };

  try {
    const zip = await Zip.open(zipPathOf(id));
    try {
      detail.files = zip.list();
      const summaryText = await zip.readText('summary.json', 512 * 1024);
      if (summaryText) {
        try {
          detail.summary = JSON.parse(summaryText);
        } catch {
          detail.error = 'summary.json 不是合法 JSON';
        }
      }
      detail.system = detail.summary?.system || [];
      detail.repair = detail.summary?.repair || null;

      const logName = 'logs/openpptview.log';
      const logText = await zip.readText(logName, LOG_PREVIEW_BYTES);
      if (logText) {
        const scan = scanLog(logText);
        detail.logs = [{ name: logName, bytes: logText.length, ...scan }];
      }
    } finally {
      await zip.close();
    }
  } catch (e) {
    detail.error = `报告包读不出来：${String(e.message || e)}`;
  }

  const info = explain(meta.bugcheck);
  detail.analysis = {
    ...info,
    findings: diagnose({ meta, summary: detail.summary, logs: detail.logs[0] }),
  };
  json(res, 200, { ok: true, report: detail });
}

async function handleFile(req, res, id, url) {
  await visibleMeta(req, id);
  const name = String(url.searchParams.get('name') || '');
  if (!name) throw new HttpError(400, '缺少 name');
  const zip = await Zip.open(zipPathOf(id));
  try {
    if (!zip.entries.some((e) => e.name === name)) throw new HttpError(404, '包里没有这个文件');
    const buf = await zip.read(name, LOG_PREVIEW_BYTES);
    res.writeHead(200, {
      'Content-Type': 'text/plain; charset=utf-8',
      'Content-Length': buf.length,
      'Cache-Control': 'no-store',
      'Content-Disposition': `inline; filename*=UTF-8''${encodeURIComponent(path.basename(name))}`,
    });
    res.end(buf);
  } finally {
    await zip.close();
  }
}

async function handleDownload(req, res, id) {
  await visibleMeta(req, id);
  const file = zipPathOf(id);
  let stat;
  try {
    stat = await fs.stat(file);
  } catch {
    throw new HttpError(404, '报告包已经不在磁盘上了');
  }
  res.writeHead(200, {
    'Content-Type': 'application/zip',
    'Content-Length': stat.size,
    'Content-Disposition': `attachment; filename="${id}.zip"`,
  });
  await pipeline(createReadStream(file), res);
}

async function handlePatch(req, res, id) {
  const user = await requireUser(req, { write: true });
  const { meta } = await visibleMeta(req, id);
  // 能看 ≠ 能改：状态和备注是「这个班怎么处理这次蓝屏」，只有管这个班的人能写
  if (!canManageReport(user, meta)) throw new HttpError(403, '只能由班级管理员修改');
  const body = await readJsonBody(req);
  const next = annotationOf(id);
  if (body.status !== undefined) {
    if (!STATUSES.includes(body.status)) throw new HttpError(400, '不认识的状态');
    next.status = body.status;
  }
  if (body.note !== undefined) next.note = String(body.note).slice(0, 4000);
  next.by = user.username;
  next.at = new Date().toISOString();
  annotations[id] = next;
  await saveAnnotations();
  json(res, 200, { ok: true, annotation: next });
}

async function handleDelete(req, res, id) {
  const user = await requireUser(req, { write: true });
  const { meta } = await visibleMeta(req, id);
  if (!canManageReport(user, meta)) throw new HttpError(403, '只能由班级管理员删除');
  await fs.rm(zipPathOf(id), { force: true });
  await fs.rm(metaPathOf(id), { force: true });
  if (annotations[id]) {
    delete annotations[id];
    await saveAnnotations();
  }
  console.log(`[${new Date().toISOString()}] 删除报告 ${id}`);
  json(res, 200, { ok: true });
}

/* ------------------------------------------------------------------ */
/* 后台：统计                                                          */
/* ------------------------------------------------------------------ */

function dayKey(iso) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

async function handleStats(req, res, url) {
  const user = await requireUser(req);
  const scope = scopeOf(user);
  const days = Math.min(Math.max(Number(url.searchParams.get('days') || 14), 7), 60);
  // 统计也只算自己看得到的那些：概览页上的数字同样是信息
  const all = (await readAllMetas()).filter((m) => visibleReport(m, scope)).map(enrich);

  const today = new Date();
  const series = [];
  const index = new Map();
  for (let i = days - 1; i >= 0; i--) {
    const d = new Date(today.getFullYear(), today.getMonth(), today.getDate() - i);
    const key = dayKey(d.toISOString());
    const row = { date: key, total: 0, critical: 0 };
    series.push(row);
    index.set(key, row);
  }

  const byBugcheck = new Map();
  const byVersion = new Map();
  const machines = new Set();
  const statusCount = { new: 0, investigating: 0, fixed: 0, ignored: 0 };
  let demo = 0;
  let last7 = 0;
  const weekAgo = Date.now() - 7 * 24 * 3600 * 1000;

  for (const m of all) {
    const key = dayKey(m.receivedAt);
    const row = index.get(key);
    if (row) {
      row.total += 1;
      if (m.analysis.severity === 'critical' || m.analysis.severity === 'high') row.critical += 1;
    }
    if (new Date(m.receivedAt).getTime() >= weekAgo) last7 += 1;

    const code = m.analysis.code || m.bugcheck || '未知';
    const b = byBugcheck.get(code) || { code, title: m.analysis.title, count: 0, severity: m.analysis.severity };
    b.count += 1;
    byBugcheck.set(code, b);

    const v = m.appVersion || '未知';
    byVersion.set(v, (byVersion.get(v) || 0) + 1);

    if (m.remote) machines.add(m.remote);
    if (m.mode === 'demo') demo += 1;
    statusCount[m.status] = (statusCount[m.status] || 0) + 1;
  }

  const pending = statusCount.new + statusCount.investigating;

  json(res, 200, {
    ok: true,
    totals: {
      all: all.length,
      last7,
      pending,
      machines: machines.size,
      demo,
    },
    status: statusCount,
    series,
    bugchecks: [...byBugcheck.values()].sort((a, b) => b.count - a.count).slice(0, 8),
    versions: [...byVersion.entries()]
      .map(([version, count]) => ({ version, count }))
      .sort((a, b) => b.count - a.count)
      .slice(0, 8),
    recent: all.slice(0, 6),
  });
}

/* ------------------------------------------------------------------ */
/* 账号接口                                                            */
/* ------------------------------------------------------------------ */

async function handleAuth(req, res, url, method) {
  const route = url.pathname.replace(/^\/api\/v1\/auth\//, '');

  if (method === 'GET' && route === 'status') {
    const user = await currentUser(req);
    json(res, 200, { ok: true, initialized: await auth.initialized(), user });
    return;
  }

  if (method === 'POST' && (route === 'register' || route === 'login')) {
    const body = await readJsonBody(req);
    const result =
      route === 'register'
        ? await auth.register({ username: body.username, password: body.password, ip: clientIp(req) })
        : await auth.login({ username: body.username, password: body.password, ip: clientIp(req) });
    console.log(
      `[${new Date().toISOString()}] ${route === 'register' ? '注册' : '登录'}：${result.user.username}（${result.user.role}，${clientIp(req)}）`
    );
    json(res, 200, { ok: true, user: result.user }, {
      'Set-Cookie': sessionCookie(result.token),
    });
    return;
  }

  if (method === 'POST' && route === 'logout') {
    await auth.logout(parseCookies(req)[COOKIE] || '');
    json(res, 200, { ok: true }, { 'Set-Cookie': sessionCookie('') });
    return;
  }

  if (method === 'GET' && route === 'me') {
    const user = await requireUser(req);
    json(res, 200, { ok: true, user });
    return;
  }

  if (method === 'POST' && route === 'password') {
    const user = await requireUser(req, { write: true });
    const body = await readJsonBody(req);
    await auth.changePassword(user.id, body.oldPassword, body.newPassword);
    json(res, 200, { ok: true });
    return;
  }

  throw new HttpError(404, 'not found');
}

async function handleUsers(req, res, url, method) {
  const user = await requireUser(req, { write: method !== 'GET', admin: true });

  if (method === 'GET') {
    json(res, 200, { ok: true, users: await auth.listUsers() });
    return;
  }

  const m = /^\/api\/v1\/users\/([\w-]+)$/.exec(url.pathname);
  if (!m) throw new HttpError(404, 'not found');
  const id = m[1];

  if (method === 'PATCH') {
    const body = await readJsonBody(req);
    json(res, 200, { ok: true, user: await auth.setRole(id, body.role) });
    return;
  }
  if (method === 'DELETE') {
    await auth.deleteUser(id, user.id);
    json(res, 200, { ok: true });
    return;
  }
  throw new HttpError(405, 'method not allowed');
}

/* ------------------------------------------------------------------ */
/* 学校 / 班级 / 成员 / 机器授权                                        */
/* ------------------------------------------------------------------ */

/** 顶部身份与可见范围：后台据此画导航、填「班级」下拉。 */
async function orgSummary(user) {
  const scope = org.scopeOf(user);
  const users = await auth.listUsers();
  const nameOf = (id) => {
    const u = users.find((x) => x.id === id);
    return u ? u.username : '（账号已删除）';
  };
  const schools = org
    .schools()
    .filter((s) => scope.all || s.admins.includes(user.id))
    .map((s) => ({ ...s, adminNames: s.admins.map(nameOf) }));
  const classes = org
    .classes()
    .filter((c) => scope.all || scope.classIds.includes(c.id))
    .map((c) => ({
      ...c,
      adminNames: c.admins.map(nameOf),
      myRole: c.admins.includes(user.id) ? 'admin' : 'member',
    }));
  const pendingJoins = org.joinRequests({
    pendingOnly: true,
    classIds: scope.all ? undefined : scope.classIds,
  }).length;
  return { role: user.role, all: scope.all, schools, classes, pendingJoins };
}

async function handleOrg(req, res, url, method) {
  const write = method !== 'GET';
  const user = await requireUser(req, { write });
  const p = url.pathname;
  const body = write ? await readJsonBody(req) : {};

  // —— 概览 ——
  if (p === '/api/v1/org' && method === 'GET') {
    json(res, 200, { ok: true, ...(await orgSummary(user)) });
    return;
  }

  // —— 按用户名找人（加成员用）：只做精确匹配，避免被拿来枚举全体账号 ——
  if (p === '/api/v1/users/find' && method === 'GET') {
    const name = String(url.searchParams.get('username') || '').trim().toLowerCase();
    if (!name) throw new HttpError(400, '缺少 username');
    const hit = (await auth.listUsers()).find((u) => u.username.toLowerCase() === name);
    json(res, 200, { ok: true, user: hit ? { id: hit.id, username: hit.username } : null });
    return;
  }

  // —— 学校 ——
  if (p === '/api/v1/schools' && method === 'POST') {
    if (user.role !== 'platform') throw new HttpError(403, '只有平台超管能建学校');
    json(res, 200, { ok: true, school: await org.createSchool(body.name) });
    return;
  }
  let m = /^\/api\/v1\/schools\/([\w-]{4,40})$/.exec(p);
  if (m && method === 'PATCH') {
    if (user.role !== 'platform') throw new HttpError(403, '只有平台超管能改学校');
    if (!org.school(m[1])) throw new HttpError(404, '没有这个学校');
    const users = await auth.listUsers();
    if (Array.isArray(body.admins)) {
      await org.setSchoolAdmins(
        m[1],
        body.admins.filter((id) => users.some((u) => u.id === id))
      );
    }
    json(res, 200, { ok: true, school: org.school(m[1]) });
    return;
  }

  // —— 班级 ——
  if (p === '/api/v1/classes' && method === 'POST') {
    const schoolId = String(body.schoolId || '');
    if (!org.isSchoolAdmin(user, schoolId)) {
      throw new HttpError(403, '只有本校管理员或平台超管能建班');
    }
    json(res, 200, { ok: true, class: await org.createClass(schoolId, body.name, user.id) });
    return;
  }
  // 找班（申请加入用）：按名字搜。只给「名字 + 学校 + 人数」，看不到任何报告
  if (p === '/api/v1/classes/find' && method === 'GET') {
    const q = String(url.searchParams.get('q') || '').trim().toLowerCase();
    if (!q) {
      json(res, 200, { ok: true, classes: [] });
      return;
    }
    const items = org
      .classes()
      .filter((c) => `${c.name} ${c.schoolName}`.toLowerCase().includes(q))
      .slice(0, 20)
      .map((c) => ({
        id: c.id,
        name: c.name,
        schoolName: c.schoolName,
        memberCount: c.memberCount,
        joined: org.isMember(user.id, c.id),
      }));
    json(res, 200, { ok: true, classes: items });
    return;
  }

  m = /^\/api\/v1\/classes\/([\w-]{4,40})(\/[a-z]+)?$/.exec(p);
  if (m) {
    const classId = m[1];
    const sub = m[2] || '';
    const klass = org.class(classId);
    if (!klass) throw new HttpError(404, '没有这个班级');

    if (sub === '' && method === 'PATCH') {
      if (!org.isSchoolAdmin(user, klass.schoolId) && !org.canManageClass(user, classId)) {
        throw new HttpError(403, '只能由本校管理员修改班级');
      }
      const users = await auth.listUsers();
      if (Array.isArray(body.admins)) {
        const ids = body.admins.filter((id) => users.some((u) => u.id === id));
        if (!ids.length) throw new HttpError(400, '班级至少留一个管理员');
        await org.setClassAdmins(classId, ids);
      }
      json(res, 200, { ok: true, class: org.class(classId) });
      return;
    }

    if (sub === '/members') {
      if (!org.canManageClass(user, classId)) throw new HttpError(403, '只有班级管理员能管成员');
      if (method === 'GET') {
        const users = await auth.listUsers();
        const members = org.membersOf(classId).map((mem) => {
          const u = users.find((x) => x.id === mem.userId);
          return {
            id: mem.userId,
            username: u ? u.username : '（账号已删除）',
            admin: klass.admins.includes(mem.userId),
            at: mem.at,
          };
        });
        json(res, 200, { ok: true, members });
        return;
      }
      if (method === 'POST') {
        const targetId = String(body.userId || '');
        if (body.action === 'remove') {
          if (klass.admins.includes(targetId) && klass.admins.length <= 1) {
            throw new HttpError(400, '至少要留一个班级管理员');
          }
          await org.removeMember(classId, targetId);
        } else {
          await org.addMember(classId, targetId);
        }
        json(res, 200, { ok: true, class: org.class(classId) });
        return;
      }
    }

    // —— 申请加入 ——
    if (sub === '/join' && method === 'POST') {
      if (org.isMember(user.id, classId)) throw new HttpError(409, '你已经是这个班的成员了');
      json(res, 200, { ok: true, request: await org.requestJoin(classId, user.id) });
      return;
    }
  }

  // —— 加入申请 ——
  if (p === '/api/v1/join-requests' && method === 'GET') {
    const scope = org.scopeOf(user);
    const users = await auth.listUsers();
    const items = org
      .joinRequests({ pendingOnly: true, classIds: scope.all ? undefined : scope.classIds })
      .map((r) => {
        const u = users.find((x) => x.id === r.userId);
        const c = org.class(r.classId);
        return {
          ...r,
          username: u ? u.username : '（账号已删除）',
          className: c ? c.name : '',
          schoolName: c ? c.schoolName : '',
        };
      })
      .filter((r) => org.canManageClass(user, r.classId));
    json(res, 200, { ok: true, requests: items });
    return;
  }
  m = /^\/api\/v1\/join-requests\/([\w-]{4,40})$/.exec(p);
  if (m && method === 'POST') {
    const req0 = org.joinRequests().find((r) => r.id === m[1]);
    if (!req0) throw new HttpError(404, '没有这条申请');
    if (!org.canManageClass(user, req0.classId)) throw new HttpError(403, '只有班级管理员能审批');
    const approved = body.action === 'approve';
    json(res, 200, { ok: true, request: await org.decideJoin(m[1], approved) });
    return;
  }

  // —— 已授权机器 ——
  if (p === '/api/v1/machines' && method === 'GET') {
    const scope = org.scopeOf(user);
    const items = org
      .machinesList()
      .filter((m) => scope.all || scope.classIds.includes(m.classId))
      .map((m) => {
        const c = org.class(m.classId);
        return { ...m, className: c ? c.name : '', schoolName: c ? c.schoolName : '' };
      });
    json(res, 200, { ok: true, machines: items });
    return;
  }
  if (p === '/api/v1/machines/revoke' && method === 'POST') {
    const hwid = String(body.hwid || '');
    const rec = org.machine(hwid);
    if (!rec) throw new HttpError(404, '没有这台机器的授权记录');
    if (!org.canManageClass(user, rec.classId)) throw new HttpError(403, '只能撤销本班机器');
    const n = await org.revokeMachine(hwid);
    json(res, 200, { ok: true, revoked: n });
    return;
  }

  throw new HttpError(404, 'not found');
}

/** 机器侧（授权后拿令牌、退出时撤销）。 */
async function handleMachineApi(req, res, url, method) {
  const p = url.pathname;

  // 换令牌：没有会话，凭一次性授权码
  if (p === '/api/v1/machine/exchange' && method === 'POST') {
    const body = await readJsonBody(req);
    const hwid = String(body.hwid || '').trim();
    if (!/^[A-Za-z0-9_-]{4,64}$/.test(hwid)) throw new HttpError(400, 'hwid 不合法');
    const out = await org.exchange(body.code, hwid, {
      name: body.name,
      version: body.version,
    });
    json(res, 200, { ok: true, ...out });
    return;
  }

  // 授权页要列「我能授权的班级」
  if (p === '/api/v1/machine/choices' && method === 'GET') {
    const user = await requireUser(req);
    const scope = org.scopeOf(user);
    const classes = org
      .classes()
      .filter((c) => scope.all || scope.classIds.includes(c.id))
      .map((c) => ({
        id: c.id,
        name: c.name,
        schoolName: c.schoolName,
        role: c.admins.includes(user.id) ? 'admin' : 'member',
      }));
    json(res, 200, {
      ok: true,
      user: { username: user.username, role: user.role },
      classes,
    });
    return;
  }

  // 浏览器点「授权这台电脑」
  if (p === '/api/v1/machine/authorize' && method === 'POST') {
    const user = await requireUser(req, { write: true });
    const body = await readJsonBody(req);
    const classId = String(body.classId || '');
    const state = String(body.state || '');
    const port = Number(body.port || 0);
    if (!(port >= 1024 && port <= 65535)) throw new HttpError(400, '端口不合法');
    if (!state || state.length > 64) throw new HttpError(400, 'state 不合法');
    if (user.role !== 'platform' && !org.isMember(user.id, classId)) {
      throw new HttpError(403, '你不在这个班里，不能授权它的电脑');
    }
    const code = await org.issueCode({ state, classId, userId: user.id });
    const redirect =
      `http://127.0.0.1:${port}/callback` +
      `?code=${encodeURIComponent(code)}&state=${encodeURIComponent(state)}`;
    json(res, 200, { ok: true, redirect });
    return;
  }

  // 机器退出（撤销自己的令牌）
  if (p === '/api/v1/machine/logout' && method === 'POST') {
    const machine = await machineAuth(req);
    if (!machine.ok || !machine.hwid) throw new HttpError(401, '机器令牌无效');
    await org.revokeMachine(machine.hwid);
    json(res, 200, { ok: true });
    return;
  }

  throw new HttpError(404, 'not found');
}

/* ------------------------------------------------------------------ */
/* 静态文件（后台网页）                                                */
/* ------------------------------------------------------------------ */

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.json': 'application/json; charset=utf-8',
  '.woff2': 'font/woff2',
};

async function serveStatic(req, res, url) {
  let rel = decodeURIComponent(url.pathname);
  if (rel === '/' || rel === '/admin' || rel === '/admin/' || rel === '/index.html') {
    rel = '/index.html';
  }
  // 只允许 web 目录下的文件，挡掉 ../ 之类
  const file = path.join(WEB_DIR, path.normalize(rel).replace(/^([/\\])+/, ''));
  if (!file.startsWith(WEB_DIR)) {
    json(res, 403, { ok: false, error: 'forbidden' });
    return;
  }
  let stat;
  try {
    stat = await fs.stat(file);
  } catch {
    // 单页应用：认不出的路径都交给前端路由（但别再返回 HTML 给 .js）
    if (path.extname(rel)) {
      json(res, 404, { ok: false, error: 'not found' });
      return;
    }
    return serveStatic(req, res, new URL('/', 'http://x'));
  }
  if (!stat.isFile()) {
    json(res, 404, { ok: false, error: 'not found' });
    return;
  }
  const type = MIME[path.extname(file)] || 'application/octet-stream';
  res.writeHead(200, {
    'Content-Type': type,
    'Content-Length': stat.size,
    // 后台随时要更新，别让浏览器拿旧的
    'Cache-Control': 'no-cache',
  });
  if (req.method === 'HEAD') {
    res.end();
    return;
  }
  await pipeline(createReadStream(file), res);
}

/* ------------------------------------------------------------------ */
/* 路由                                                                */
/* ------------------------------------------------------------------ */

const server = http.createServer((req, res) => {
  const url = new URL(req.url || '/', `http://${req.headers.host || 'localhost'}`);
  const p = url.pathname;
  const method = req.method || 'GET';

  const run = (fn) =>
    fn().catch((e) => {
      // `org.mjs` 抛的是带 `status` 的普通 Error（它不该依赖本文件的 HttpError），
      // 只顾 HttpError 的话「授权码已用过」会被当成 500 报出去。
      const status = e instanceof HttpError ? e.status : Number(e && e.status) || 500;
      if (status >= 500) console.error('处理失败：', e);
      if (!res.headersSent) json(res, status, { ok: false, error: String(e.message || e) });
    });

  if (method === 'GET' && p === '/health') {
    json(res, 200, { ok: true, service: 'openpptview-crash', time: new Date().toISOString() });
    return;
  }

  // —— 远程协助 ——（机器接口 + 后台接口都在这里分派）
  if (remote(req, res, url, method)) {
    return;
  }

  // —— 授权页（app 点「登录」时在浏览器里打开的那一页）——
  if ((method === 'GET' || method === 'HEAD') && p === '/authorize') {
    url.pathname = '/authorize.html';
    run(() => serveStatic(req, res, url));
    return;
  }

  // —— 机器侧：换令牌 / 授权 / 退出 ——
  if (p.startsWith('/api/v1/machine/')) {
    run(() => handleMachineApi(req, res, url, method));
    return;
  }

  // —— 学校 / 班级 / 成员 / 机器授权 ——
  if (
    p === '/api/v1/org' ||
    p === '/api/v1/users/find' ||
    p === '/api/v1/schools' ||
    p.startsWith('/api/v1/schools/') ||
    p === '/api/v1/classes' ||
    p.startsWith('/api/v1/classes/') ||
    p === '/api/v1/join-requests' ||
    p.startsWith('/api/v1/join-requests/') ||
    p === '/api/v1/machines' ||
    p.startsWith('/api/v1/machines/')
  ) {
    run(() => handleOrg(req, res, url, method));
    return;
  }

  // —— 收包 ——
  if (method === 'POST' && p === '/api/v1/crash') {
    run(() => handleUpload(req, res));
    return;
  }

  // —— 账号 ——
  if (p.startsWith('/api/v1/auth/')) {
    run(() => handleAuth(req, res, url, method));
    return;
  }
  if (p === '/api/v1/users' || p.startsWith('/api/v1/users/')) {
    run(() => handleUsers(req, res, url, method));
    return;
  }

  // —— 报告 ——
  if (method === 'GET' && p === '/api/v1/crashes') {
    run(() => handleList(req, res, url));
    return;
  }
  if (method === 'GET' && p === '/api/v1/stats') {
    run(() => handleStats(req, res, url));
    return;
  }
  let m = /^\/api\/v1\/crashes\/([A-Za-z0-9._-]{4,80})(\/[a-z]+)?$/.exec(p);
  if (m) {
    const id = m[1];
    const sub = m[2] || '';
    if (method === 'GET' && sub === '') return run(() => handleDetail(req, res, id));
    if (method === 'GET' && sub === '/download') return run(() => handleDownload(req, res, id));
    if (method === 'GET' && sub === '/file') return run(() => handleFile(req, res, id, url));
    if (method === 'PATCH' && sub === '') return run(() => handlePatch(req, res, id));
    if (method === 'DELETE' && sub === '') return run(() => handleDelete(req, res, id));
  }

  // —— 后台网页 ——
  if (method === 'GET' || method === 'HEAD') {
    run(() => serveStatic(req, res, url));
    return;
  }

  json(res, 404, { ok: false, error: 'not found' });
});

/* ------------------------------------------------------------------ */
/* 启动                                                                */
/* ------------------------------------------------------------------ */

await fs.mkdir(DATA_DIR, { recursive: true });
await auth.init();
await org.init();
await loadAnnotations();

server.listen(PORT, HOST, () => {
  console.log(`OpenPPTView 蓝屏报告服务已启动：http://${HOST}:${PORT}`);
  console.log(`收包地址：/api/v1/crash    后台页面：/`);
  console.log(`报告存放：${DATA_DIR}`);
  console.log(`账号数据：${STATE_DIR}${TOKEN ? '（收包需要 Bearer Token）' : '（收包未设 Token）'}`);
});

export { server };
