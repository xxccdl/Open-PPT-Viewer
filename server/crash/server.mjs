/**
 * OpenPPTView 蓝屏报告接收服务。
 *
 * 零依赖，只用 Node 自带模块 —— 机房服务器上不必为一个收文件的接口
 * 去维护 node_modules。
 *
 *   node server/crash/server.mjs
 *
 * 环境变量：
 *   HOST      监听地址，默认 127.0.0.1（要外部访问就改成 0.0.0.0）
 *   PORT      端口，默认 8787
 *   TOKEN     可选。设了之后客户端必须带 `Authorization: Bearer <TOKEN>`
 *   DATA_DIR  报告存放目录，默认与本文件同级的 `data/`
 *
 * 客户端（应用）那边填的地址就是：
 *   http(s)://<这台机器>/api/v1/crash
 *
 * 协议很朴素：整个 zip 放在请求体里，元数据在请求头。
 * 这样两端都不必实现 multipart —— 少一层解析，就少一处出错的地方。
 *   Content-Type: application/zip
 *   X-Crash-Id / X-App-Version / X-Bugcheck / X-Occurred-At / X-Report-Mode
 */

import http from 'node:http';
import { createWriteStream, promises as fs } from 'node:fs';
import { pipeline } from 'node:stream/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import crypto from 'node:crypto';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOST = process.env.HOST || '127.0.0.1';
const PORT = Number(process.env.PORT || 8787);
const TOKEN = (process.env.TOKEN || '').trim();
const DATA_DIR = path.resolve(process.env.DATA_DIR || path.join(HERE, 'data'));

/** 单个包的上限。客户端自己封顶 64MB，这里再拦一道，防的是别人乱传。 */
const MAX_BYTES = 96 * 1024 * 1024;

function json(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(text),
    'Cache-Control': 'no-store',
  });
  res.end(text);
}

/** 报告 id 只允许字母数字和 `-_.`：它要变成文件名，不能被路径穿越。 */
function safeId(raw) {
  const id = String(raw || '').trim();
  if (!id) return null;
  if (!/^[A-Za-z0-9._-]{4,80}$/.test(id)) return null;
  return id;
}

/**
 * 请求来自哪台机器。
 *
 * 前面挂了 nginx 反代时，`socket.remoteAddress` 永远是 127.0.0.1 ——
 * 所有报告的来源都变成同一个本机地址，机房排查时就不知道是哪台机器蓝屏的。
 * 所以取 `X-Forwarded-For` 的第一段（最靠近客户端的那一跳）。
 */
function clientIp(req) {
  const fwd = String(req.headers['x-forwarded-for'] || '')
    .split(',')[0]
    .trim();
  if (fwd) return fwd;
  return String(req.socket.remoteAddress || '').replace(/^::ffff:/, '');
}

function authorized(req) {
  if (!TOKEN) return true;
  const h = req.headers['authorization'] || '';
  const m = /^Bearer\s+(.+)$/i.exec(h);
  return !!m && m[1].trim() === TOKEN;
}

async function handleUpload(req, res) {
  if (!authorized(req)) {
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
  const zipPath = path.join(DATA_DIR, `${id}.zip`);
  const metaPath = path.join(DATA_DIR, `${id}.json`);
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

async function handleList(req, res) {
  if (!authorized(req)) {
    json(res, 401, { ok: false, error: 'unauthorized' });
    return;
  }
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
      const text = await fs.readFile(path.join(DATA_DIR, name), 'utf8');
      items.push(JSON.parse(text));
    } catch {
      /* 坏掉的元数据跳过，不影响别的 */
    }
  }
  items.sort((a, b) => String(b.receivedAt).localeCompare(String(a.receivedAt)));
  json(res, 200, { ok: true, count: items.length, items });
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url || '/', `http://${req.headers.host || 'localhost'}`);

  if (req.method === 'GET' && url.pathname === '/health') {
    json(res, 200, { ok: true, service: 'openpptview-crash', time: new Date().toISOString() });
    return;
  }
  if (req.method === 'POST' && url.pathname === '/api/v1/crash') {
    handleUpload(req, res).catch((e) => {
      console.error('处理上传失败：', e);
      if (!res.headersSent) json(res, 500, { ok: false, error: 'internal error' });
    });
    return;
  }
  if (req.method === 'GET' && url.pathname === '/api/v1/crashes') {
    handleList(req, res).catch((e) => {
      console.error('列报告失败：', e);
      if (!res.headersSent) json(res, 500, { ok: false, error: 'internal error' });
    });
    return;
  }
  json(res, 404, { ok: false, error: 'not found' });
});

server.listen(PORT, HOST, () => {
  console.log(`OpenPPTView 蓝屏报告服务已启动：http://${HOST}:${PORT}/api/v1/crash`);
  console.log(`报告存放：${DATA_DIR}${TOKEN ? '（需要 Bearer Token）' : '（未设 Token）'}`);
});
