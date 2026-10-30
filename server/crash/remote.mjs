/**
 * 远程协助服务端。
 *
 * 方向是**机器主动连上来**：每台装了 OpenPPTView 的电脑每隔几秒 `POST /poll`
 * 说一声「我在」，顺便领走管理员发下的命令、以及「要不要推屏幕」。
 * 服务器从不主动连学校 —— 机房没有公网 IP，也不该为这件事开端口。
 *
 * 状态全在内存里：重启一次，在线列表清空，机器几秒后自己全部回来。
 * 唯一需要跨机器保留的是**命令结果的历史**，那也只在内存里留最近几十条：
 * 它是一次会话的东西，不是档案。真要留证据，让机器把命令结果写进自己的日志。
 *
 * 权限：
 * - `/poll`、`/result`、`POST /frame` 是机器接口，吃 `Authorization: Bearer <TOKEN>`；
 * - 其余都是后台接口，要管理员会话 + `X-Oppv-Client: admin`。
 */

import crypto from 'node:crypto';

/** 多少毫秒没心跳就算离线。心跳 3 秒一次、屏幕开着 1 秒一次，20 秒已经很宽容。 */
const OFFLINE_AFTER = 20 * 1000;
/** 一台机器的记录在内存里留多久（离线之后）。 */
const KEEP_AFTER = 24 * 3600 * 1000;
/** 待执行命令最多排这么多，防止有人往队列里灌。 */
const QUEUE_MAX = 32;
/** 一条命令在队列里等多久就作废：机器关机了，明天开机不该执行前一晚的命令。 */
const CMD_TTL = 3 * 60 * 1000;
/** 每台机器保留多少条结果。 */
const RESULT_MAX = 60;
/** 一次心跳最多带回多少条命令。 */
const BATCH = 8;
/** 一帧最多多大（1920 宽 JPEG 通常 100～300 KB，留足余量）。 */
const MAX_FRAME = 8 * 1024 * 1024;

function now() {
  return new Date().toISOString();
}

/** 机器码要能当 Map 的键，也要能出现在 URL 里。 */
function validHwid(raw) {
  const id = String(raw || '').trim();
  return /^[A-Za-z0-9_-]{4,64}$/.test(id) ? id : '';
}

function online(m) {
  return Date.now() - new Date(m.lastSeen).getTime() < OFFLINE_AFTER;
}

export class RemoteHub {
  constructor() {
    /** @type {Map<string, any>} */
    this.machines = new Map();
    this.seq = 0;
  }

  ensure(hwid) {
    let m = this.machines.get(hwid);
    if (!m) {
      m = {
        hwid,
        name: '',
        user: '',
        admin: false,
        version: '',
        ip: '',
        firstSeen: now(),
        lastSeen: now(),
        commands: [],
        results: [],
        screen: false,
        frame: null,
        frameSeq: 0,
      };
      this.machines.set(hwid, m);
    }
    return m;
  }

  /** 机器报到：更新身份、领命令、拿「要不要推屏幕」。 */
  poll(meta) {
    this.prune();
    const m = this.ensure(meta.hwid);
    m.name = String(meta.name || m.name || '').slice(0, 64);
    m.user = String(meta.user || m.user || '').slice(0, 64);
    m.admin = Boolean(meta.admin);
    m.version = String(meta.version || m.version || '').slice(0, 32);
    m.ip = String(meta.ip || m.ip || '');
    m.lastSeen = now();

    // 过期命令直接丢掉：机器关机一整晚，早上开机不该执行昨晚那条 reboot
    const cutoff = Date.now() - CMD_TTL;
    m.commands = m.commands.filter((c) => new Date(c.at).getTime() > cutoff);

    const commands = m.commands.splice(0, BATCH);
    return { commands, screen: m.screen };
  }

  /** 机器回传执行结果。 */
  result(hwid, results) {
    const m = this.ensure(hwid);
    for (const r of results || []) {
      m.results.unshift({
        id: String(r.id || '').slice(0, 40),
        ok: Boolean(r.ok),
        out: String(r.out || '').slice(0, 70_000),
        ms: Number(r.ms) || 0,
        at: now(),
      });
    }
    m.results = m.results.slice(0, RESULT_MAX);
    m.lastSeen = now();
    return m.results.length;
  }

  /** 机器推上来一帧桌面。 */
  frame(hwid, buf, seq, w, h) {
    const m = this.ensure(hwid);
    m.frame = { buf, seq: Number(seq) || 0, w: Number(w) || 0, h: Number(h) || 0, at: now() };
    m.frameSeq = Math.max(m.frameSeq, m.frame.seq);
    m.lastSeen = now();
  }

  /** 后台发一条命令。 */
  push(hwid, kind, arg) {
    const m = this.ensure(hwid);
    if (!online(m)) throw Object.assign(new Error('这台机器现在不在线'), { status: 409 });
    if (m.commands.length >= QUEUE_MAX) {
      throw Object.assign(new Error('命令排得太多了，等它执行完再发'), { status: 429 });
    }
    const cmd = {
      id: crypto.randomBytes(6).toString('hex'),
      kind,
      arg: String(arg || '').slice(0, 4000),
      at: now(),
    };
    m.commands.push(cmd);
    return cmd;
  }

  /** 后台开关屏幕。 */
  setScreen(hwid, on) {
    const m = this.ensure(hwid);
    if (!online(m)) throw Object.assign(new Error('这台机器现在不在线'), { status: 409 });
    m.screen = Boolean(on);
    if (!m.screen) m.frame = null;
    return m.screen;
  }

  list() {
    this.prune();
    return [...this.machines.values()]
      .sort((a, b) => String(b.lastSeen).localeCompare(String(a.lastSeen)))
      .map((m) => ({
        hwid: m.hwid,
        name: m.name,
        user: m.user,
        admin: m.admin,
        version: m.version,
        ip: m.ip,
        firstSeen: m.firstSeen,
        lastSeen: m.lastSeen,
        online: online(m),
        screen: m.screen && online(m),
        frameSeq: m.frameSeq,
        pending: m.commands.length,
        results: m.results.length,
      }));
  }

  /** 丢掉离线太久、也没人再看的记录。 */
  prune() {
    const cutoff = Date.now() - KEEP_AFTER;
    for (const [hwid, m] of this.machines) {
      if (new Date(m.lastSeen).getTime() < cutoff && !m.commands.length) {
        this.machines.delete(hwid);
      }
    }
  }
}

/* ------------------------------------------------------------------ */
/* HTTP 处理                                                            */
/* ------------------------------------------------------------------ */

async function readJson(req, max = 1024 * 1024) {
  let size = 0;
  const chunks = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > max) throw Object.assign(new Error('请求体太大'), { status: 413 });
    chunks.push(chunk);
  }
  if (!size) return {};
  try {
    return JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    throw Object.assign(new Error('不是合法的 JSON'), { status: 400 });
  }
}

async function readRaw(req, max) {
  let size = 0;
  const chunks = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > max) throw Object.assign(new Error('图片太大'), { status: 413 });
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

/** 远程路由的处理器：主服务按路径把请求交给它。 */
export function createRemoteRoutes({ json, requireUser, machineAuthorized, clientIp }) {
  const hub = new RemoteHub();

  // —— 机器接口 ——
  async function poll(req, res) {
    if (!machineAuthorized(req)) return json(res, 401, { ok: false, error: 'unauthorized' });
    const body = await readJson(req, 256 * 1024);
    const hwid = validHwid(body.hwid);
    if (!hwid) return json(res, 400, { ok: false, error: 'hwid 不合法' });
    const out = hub.poll({ ...body, hwid, ip: clientIp(req) });
    return json(res, 200, { ok: true, commands: out.commands, screen: out.screen });
  }

  async function result(req, res) {
    if (!machineAuthorized(req)) return json(res, 401, { ok: false, error: 'unauthorized' });
    const body = await readJson(req, 1024 * 1024);
    const hwid = validHwid(body.hwid);
    if (!hwid) return json(res, 400, { ok: false, error: 'hwid 不合法' });
    hub.result(hwid, body.results);
    return json(res, 200, { ok: true });
  }

  async function framePost(req, res) {
    if (!machineAuthorized(req)) return json(res, 401, { ok: false, error: 'unauthorized' });
    const hwid = validHwid(req.headers['x-hwid']);
    if (!hwid) return json(res, 400, { ok: false, error: 'hwid 不合法' });
    const buf = await readRaw(req, MAX_FRAME);
    if (!buf.length) return json(res, 400, { ok: false, error: '空图片' });
    hub.frame(
      hwid,
      buf,
      req.headers['x-frame-seq'],
      req.headers['x-frame-w'],
      req.headers['x-frame-h']
    );
    return json(res, 200, { ok: true, seq: Number(req.headers['x-frame-seq']) || 0 });
  }

  // —— 后台接口 ——
  async function machines(req, res) {
    await requireUser(req);
    return json(res, 200, { ok: true, machines: hub.list() });
  }

  async function cmd(req, res) {
    // 远程执行命令是这台服务上权限最高的一件事：只给管理员。
    // 只读成员能看报告、能看机器列表，但不能碰别人的电脑。
    await requireUser(req, { write: true, admin: true });
    const body = await readJson(req, 256 * 1024);
    const hwid = validHwid(body.hwid);
    if (!hwid) return json(res, 400, { ok: false, error: 'hwid 不合法' });
    const kind = String(body.kind || '');
    if (!['shell', 'mouse', 'key'].includes(kind)) {
      return json(res, 400, { ok: false, error: '不认识的命令类型' });
    }
    const command = hub.push(hwid, kind, body.arg);
    return json(res, 200, { ok: true, command });
  }

  async function screen(req, res) {
    // 能让别人的屏幕开始往这儿推，也是管理员的事
    await requireUser(req, { write: true, admin: true });
    const body = await readJson(req, 256 * 1024);
    const hwid = validHwid(body.hwid);
    if (!hwid) return json(res, 400, { ok: false, error: 'hwid 不合法' });
    const on = hub.setScreen(hwid, body.on);
    return json(res, 200, { ok: true, screen: on });
  }

  async function frameGet(req, res, url) {
    await requireUser(req);
    const hwid = validHwid(url.searchParams.get('hwid'));
    if (!hwid) throw Object.assign(new Error('hwid 不合法'), { status: 400 });
    const m = hub.machines.get(hwid);
    if (!m || !m.frame) throw Object.assign(new Error('这台机器还没推上来画面'), { status: 404 });

    // 管理员那边轮询：帧号没变就不要重复传一份同样的图（白耗一条上行）
    const since = Number(url.searchParams.get('since') || 0);
    if (since && since >= m.frame.seq) {
      res.writeHead(204, { 'Cache-Control': 'no-store' });
      res.end();
      return;
    }
    res.writeHead(200, {
      'Content-Type': 'image/jpeg',
      'Content-Length': m.frame.buf.length,
      'Cache-Control': 'no-store',
      'X-Frame-Seq': String(m.frame.seq),
      'X-Frame-W': String(m.frame.w),
      'X-Frame-H': String(m.frame.h),
      'X-Frame-At': m.frame.at,
    });
    res.end(m.frame.buf);
  }

  async function results(req, res, url) {
    await requireUser(req);
    const hwid = validHwid(url.searchParams.get('hwid'));
    if (!hwid) throw Object.assign(new Error('hwid 不合法'), { status: 400 });
    const m = hub.machines.get(hwid);
    if (!m) return json(res, 200, { ok: true, results: [] });
    const limit = Math.min(Number(url.searchParams.get('limit') || 30), RESULT_MAX);
    return json(res, 200, { ok: true, results: m.results.slice(0, limit) });
  }

  /** 主服务的路由表：返回 `true` 表示这个请求归远程管。 */
  return function dispatch(req, res, url, method) {
    if (!url.pathname.startsWith('/api/v1/remote/')) return false;
    const route = url.pathname.slice('/api/v1/remote/'.length);
    const table = {
      'POST poll': poll,
      'POST result': result,
      'POST frame': framePost,
      'GET frame': frameGet,
      'GET machines': machines,
      'POST cmd': cmd,
      'POST screen': screen,
      'GET results': results,
    };
    const fn = table[`${method} ${route}`];
    if (!fn) return false;
    fn(req, res, url).catch((e) => {
      const status = e?.status || 500;
      if (status >= 500) console.error('远程协助处理失败：', e);
      if (!res.headersSent) {
        json(res, status, { ok: false, error: String((e && e.message) || e) });
      }
    });
    return true;
  };
}
