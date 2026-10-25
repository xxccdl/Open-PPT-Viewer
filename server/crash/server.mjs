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

/** 机器上传用的共享令牌（没设 TOKEN 就是全放行）。 */
function machineAuthorized(req) {
  if (!TOKEN) return true;
  return bearer(req) === TOKEN;
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
  if (admin && user.role !== 'admin') throw new HttpError(403, '需要管理员权限');
  return user;
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
  if (!machineAuthorized(req)) {
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
    bugcheck: String(req.headers['x-bugcheck'] || ''),
    occurredAt: String(req.headers['x-occurred-at'] || ''),
    mode: String(req.headers['x-report-mode'] || 'real'),
    remote: clientIp(req),
  };
  await fs.writeFile(metaPath, JSON.stringify(meta, null, 2), 'utf8');

  console.log(
    `[${meta.receivedAt}] 收到 ${id}（${(received / 1048576).toFixed(2)} MB，` +
      `${meta.bugcheck || '未知错误码'}，${meta.appVersion || '未知版本'}，${meta.mode}）`
  );
  json(res, 200, { ok: true, id, bytes: received });
}

/* ------------------------------------------------------------------ */
/* 后台：列表与详情                                                     */
/* ------------------------------------------------------------------ */

function enrich(meta) {
  const info = explain(meta.bugcheck);
  const a = annotationOf(meta.id);
  return {
    ...meta,
    analysis: { code: info.code, title: info.title, severity: info.severity, known: info.known },
    status: a.status,
    note: a.note,
    updatedAt: a.at,
    updatedBy: a.by,
  };
}

async function handleList(req, res, url) {
  await requireUser(req);
  const q = (url.searchParams.get('q') || '').trim().toLowerCase();
  const bugcheck = (url.searchParams.get('bugcheck') || '').trim().toUpperCase();
  const version = (url.searchParams.get('version') || '').trim();
  const mode = (url.searchParams.get('mode') || '').trim();
  const status = (url.searchParams.get('status') || '').trim();
  const limit = Math.min(Number(url.searchParams.get('limit') || 100), 500);

  let items = (await readAllMetas()).map(enrich);

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
  await requireUser(req);
  const meta = await readMeta(id);
  if (!meta) throw new HttpError(404, '没有这份报告');

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
  await requireUser(req);
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
  await requireUser(req);
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
  const meta = await readMeta(id);
  if (!meta) throw new HttpError(404, '没有这份报告');
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
  await requireUser(req, { write: true, admin: true });
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
  await requireUser(req);
  const days = Math.min(Math.max(Number(url.searchParams.get('days') || 14), 7), 60);
  const all = (await readAllMetas()).map(enrich);

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
      const status = e instanceof HttpError ? e.status : 500;
      if (status >= 500) console.error('处理失败：', e);
      if (!res.headersSent) json(res, status, { ok: false, error: String(e.message || e) });
    });

  if (method === 'GET' && p === '/health') {
    json(res, 200, { ok: true, service: 'openpptview-crash', time: new Date().toISOString() });
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
await loadAnnotations();

server.listen(PORT, HOST, () => {
  console.log(`OpenPPTView 蓝屏报告服务已启动：http://${HOST}:${PORT}`);
  console.log(`收包地址：/api/v1/crash    后台页面：/`);
  console.log(`报告存放：${DATA_DIR}`);
  console.log(`账号数据：${STATE_DIR}${TOKEN ? '（收包需要 Bearer Token）' : '（收包未设 Token）'}`);
});

export { server };
