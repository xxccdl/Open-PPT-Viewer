/**
 * 蓝屏档案 · 后台前端
 *
 * 没有框架：一个路由、几种视图、一个 fetch 包装。后台不是产品界面，
 * 但它每天都要被看几眼，所以排版和层次值得认真做 —— 目标是「一眼看到
 * 哪台机器出事了、出的是什么事、我该做什么」。
 */

/* ------------------------------------------------------------------ */
/* 基础                                                               */
/* ------------------------------------------------------------------ */

const $ = (sel) => document.querySelector(sel);

/** 后端路径：页面在 /crash/ 下时 API 在 /crash/api/v1/ 下，子域名根目录同理。 */
const API_BASE = `${location.pathname.replace(/[^/]*$/, '')}api/v1/`;

const STATUS_TEXT = {
  new: '待处理',
  investigating: '处理中',
  fixed: '已修复',
  ignored: '已忽略',
};

const SEVERITY_TEXT = {
  critical: '致命',
  high: '严重',
  medium: '中等',
  low: '轻微',
  info: '提示',
  unknown: '未知',
};

const state = {
  user: null,
  initialized: false,
  authMode: 'login',
  route: '',
  /** 远程页面的轮询定时器。换页时统一清掉。 */
  remoteTimers: [],
  /** 当前显示的那一帧画面的 blob URL，换页时要撤销，否则内存一直涨。 */
  remoteFrameUrl: null,
};

function esc(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function toast(message, kind = '') {
  const box = $('#toast');
  box.textContent = message;
  box.className = `toast on ${kind}`;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => {
    box.className = 'toast';
  }, 2600);
}

function fmtTime(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return String(iso);
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(
    d.getMinutes()
  )}`;
}

function fmtBytes(n) {
  const v = Number(n || 0);
  if (v < 1024) return `${v} B`;
  if (v < 1024 * 1024) return `${(v / 1024).toFixed(1)} KB`;
  return `${(v / 1024 / 1024).toFixed(2)} MB`;
}

function severityOf(report) {
  const s = report?.analysis?.severity || 'unknown';
  return SEVERITY_TEXT[s] ? s : 'unknown';
}

function sevTag(level) {
  return `<span class="sev ${esc(level)}"><i></i>${esc(SEVERITY_TEXT[level] || '未知')}</span>`;
}

function statusChip(status) {
  const s = STATUS_TEXT[status] ? status : 'new';
  return `<span class="chip ${s}">${esc(STATUS_TEXT[s])}</span>`;
}

async function api(path, { method = 'GET', body } = {}) {
  // 写操作的客户端标识必须**每个非 GET 请求都带**：删除没有请求体，
  // 之前只在有 body 时才加这个头，导致删除一律 403「缺少客户端标识」。
  const headers = { 'X-Oppv-Client': 'admin' };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  // GET 带一个时间戳：这条链路前面挂着 CDN，它没按 Cache-Control 办事，
  // 曾经把「未登录」的状态缓存下来发给所有人。每个请求换一个 URL 最省心。
  const url =
    API_BASE +
    path +
    (method === 'GET' ? `${path.includes('?') ? '&' : '?'}_=${Date.now()}` : '');
  const res = await fetch(url, {
    method,
    headers,
    credentials: 'same-origin',
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let data = {};
  try {
    data = await res.json();
  } catch {
    data = { ok: false, error: '服务器没有返回 JSON' };
  }
  if (!res.ok) {
    const err = new Error(data.error || `请求失败（${res.status}）`);
    err.status = res.status;
    throw err;
  }
  return data;
}

/* ------------------------------------------------------------------ */
/* 登录                                                               */
/* ------------------------------------------------------------------ */

function paintAuth() {
  const reg = state.authMode === 'register';
  $('#auth-title').textContent = reg
    ? state.initialized
      ? '创建账号'
      : '创建管理员账号'
    : '登录后台';
  $('#auth-hint').textContent = reg
    ? state.initialized
      ? '后创建的是只读成员，能看报告，不能删、不能管人。'
      : '这台服务器还没有账号。先建的这个就是管理员。'
    : '输入账号继续。';
  $('#auth-submit').textContent = reg ? (state.initialized ? '创建并进入' : '创建管理员并进入') : '登录';
  $('#auth-switch').textContent = reg ? '已经有账号了，去登录' : '还没有账号？创建一个';
  $('#auth-switch').hidden = !state.initialized && reg;
  $('#auth-pass').autocomplete = reg ? 'new-password' : 'current-password';
  $('#auth-error').textContent = '';
}

function showAuth() {
  $('#boot').classList.add('hidden');
  $('#app').classList.add('hidden');
  $('#auth').classList.remove('hidden');
  state.authMode = state.initialized ? 'login' : 'register';
  paintAuth();
  setTimeout(() => $('#auth-user').focus(), 60);
}

async function submitAuth(event) {
  event.preventDefault();
  const username = $('#auth-user').value.trim();
  const password = $('#auth-pass').value;
  const reg = state.authMode === 'register';
  const btn = $('#auth-submit');
  btn.disabled = true;
  $('#auth-error').textContent = '';
  try {
    const data = await api(reg ? 'auth/register' : 'auth/login', {
      method: 'POST',
      body: { username, password },
    });
    state.user = data.user;
    state.initialized = true;
    $('#auth-pass').value = '';
    await enterApp();
    toast(reg ? `欢迎，${data.user.username}。这是你的后台。` : `欢迎回来，${data.user.username}`);
  } catch (e) {
    $('#auth-error').textContent = e.message;
  } finally {
    btn.disabled = false;
  }
}

/* ------------------------------------------------------------------ */
/* 外壳                                                               */
/* ------------------------------------------------------------------ */

const TITLES = {
  overview: '概览',
  crashes: '报告',
  detail: '报告详情',
  remote: '远程',
  users: '成员',
  account: '账号',
};

async function enterApp() {
  $('#auth').classList.add('hidden');
  $('#boot').classList.add('hidden');
  $('#app').classList.remove('hidden');
  $('#rail-user').textContent = state.user ? `${state.user.username} · ${state.user.role}` : '—';
  $('#who').textContent = state.user?.username || '—';
  $('#nav-users').hidden = state.user?.role !== 'admin';
  refreshPendingBadge();
  if (!location.hash || location.hash === '#/') location.hash = '#/overview';
  await render();
}

async function refreshPendingBadge() {
  try {
    const data = await api('stats?days=14');
    const pending = data.totals?.pending || 0;
    const badge = $('#nav-pending');
    badge.textContent = pending;
    badge.hidden = pending === 0;
  } catch {
    /* 统计失败不必打扰 */
  }
}

function setActiveNav(view) {
  for (const a of document.querySelectorAll('#nav a')) {
    a.classList.toggle('active', a.dataset.view === view);
  }
  $('#crumb-page').textContent = view === 'detail' ? '报告详情' : TITLES[view] || '概览';
}

/* ------------------------------------------------------------------ */
/* 路由                                                               */
/* ------------------------------------------------------------------ */

function parseRoute() {
  const raw = (location.hash || '#/overview').replace(/^#\/?/, '');
  const [head, id] = raw.split('/');
  let view = TITLES[head] ? head : 'overview';
  // #/crashes/<id> 是详情，不是列表
  if (view === 'crashes' && id) view = 'detail';
  return { view, id: id || '' };
}

/** 页面里挂着的轮询定时器。换页 / 重进时必须一次性清掉。 */
function clearRemoteTimers() {
  for (const t of state.remoteTimers || []) clearInterval(t);
  state.remoteTimers = [];
  if (state.remoteFrameUrl) {
    URL.revokeObjectURL(state.remoteFrameUrl);
    state.remoteFrameUrl = null;
  }
}

async function render() {
  const { view, id } = parseRoute();
  state.route = view;
  // 远程页面自带轮询定时器：换页时必须停掉，否则旧页面在后台一直发请求
  clearRemoteTimers();
  if (view === 'users' && state.user?.role !== 'admin') {
    location.hash = '#/overview';
    return;
  }
  setActiveNav(view === 'detail' ? 'crashes' : view);
  const box = $('#view');
  box.innerHTML = '<div class="empty">正在读取…</div>';
  try {
    if (view === 'overview') await viewOverview(box);
    else if (view === 'crashes') await viewCrashes(box);
    else if (view === 'detail') await viewDetail(box, id);
    else if (view === 'remote') await viewRemote(box, id);
    else if (view === 'users') await viewUsers(box);
    else if (view === 'account') viewAccount(box);
  } catch (e) {
    if (e.status === 401) return showAuth();
    box.innerHTML = `<div class="empty"><strong>没能读出来</strong>${esc(e.message)}</div>`;
  }
}

/* ------------------------------------------------------------------ */
/* 概览                                                               */
/* ------------------------------------------------------------------ */

function chartSvg(series) {
  const W = 720;
  const H = 190;
  const pad = { l: 4, r: 4, t: 10, b: 8 };
  const max = Math.max(1, ...series.map((d) => d.total));
  const step = (W - pad.l - pad.r) / series.length;
  const barW = Math.max(3, Math.min(28, step * 0.52));
  const scale = (v) => (v / max) * (H - pad.t - pad.b);

  const bars = series
    .map((d, i) => {
      if (!d.total) return '';
      const h = Math.max(3, scale(d.total));
      const x = pad.l + i * step + (step - barW) / 2;
      const y = H - pad.b - h;
      const cls = d.total ? 'bar' : 'bar zero';
      const title = `${d.date}：${d.total} 份${d.critical ? `（其中严重及以上 ${d.critical}）` : ''}`;
      const highH = d.critical ? Math.min(h, scale(d.critical)) : 0;
      return (
        `<rect class="${cls}" x="${x.toFixed(1)}" y="${y.toFixed(1)}" width="${barW.toFixed(
          1
        )}" height="${h.toFixed(1)}" rx="2"><title>${esc(title)}</title></rect>` +
        (highH > 0
          ? `<rect class="bar hot" x="${x.toFixed(1)}" y="${(H - pad.b - highH).toFixed(1)}" width="${barW.toFixed(
              1
            )}" height="${highH.toFixed(1)}" rx="2" opacity=".95"><title>${esc(
              title
            )}</title></rect>`
          : '')
      );
    })
    .join('');

  const lines = [0, 0.5, 1]
    .map((f) => {
      const y = pad.t + f * (H - pad.t - pad.b);
      return `<line class="grid-line" x1="${pad.l}" y1="${y}" x2="${W - pad.r}" y2="${y}" />`;
    })
    .join('');

  return `<svg class="chart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-label="每日报告数量">
    <defs>
      <linearGradient id="barGrad" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stop-color="#f2a13c" stop-opacity=".55" />
        <stop offset="100%" stop-color="#f2a13c" stop-opacity=".12" />
      </linearGradient>
    </defs>
    ${lines}${bars}
  </svg>`;
}

function chartBlock(series) {
  if (!series.some((d) => d.total)) {
    return '<div class="chart-empty">这段时间没有报告</div>';
  }
  const axis = `<div class="chart-axis"><span>${esc(series[0].date.slice(5))}</span><span>${esc(
    series[Math.floor(series.length / 2)].date.slice(5)
  )}</span><span>${esc(series[series.length - 1].date.slice(5))}</span></div>`;
  return chartSvg(series) + axis;
}

function distList(items) {
  if (!items.length) return '<div class="empty">还没有报告</div>';
  const max = Math.max(...items.map((i) => i.count), 1);
  return `<div class="dist">${items
    .map(
      (i) => `<div class="dist-row">
        <div class="top">
          <span class="code">${esc(i.code || '未知')}</span>
          <span class="name">${esc(i.title || '')}</span>
        </div>
        <div class="num">${i.count}</div>
        <div class="bar-track"><i style="width:${Math.round((i.count / max) * 100)}%"></i></div>
      </div>`
    )
    .join('')}</div>`;
}

function recentTable(items) {
  if (!items.length) {
    return `<div class="empty"><strong>还没有收到报告</strong>装上客户端后，蓝屏的机器会自动把现场传到这里。</div>`;
  }
  return `<div class="table-wrap"><table>
    <thead><tr><th>级别</th><th>错误码</th><th>报告</th><th>机器</th><th>版本</th><th>收到时间</th><th>状态</th></tr></thead>
    <tbody>${items
      .map(
        (r) => `<tr data-id="${esc(r.id)}">
          <td>${sevTag(severityOf(r))}</td>
          <td><span class="chip code">${esc(r.analysis?.code || r.bugcheck || '未知')}</span></td>
          <td><span class="cell-title">${esc(r.analysis?.title || '—')}</span>
            <span class="sub mono">${esc(r.id)}</span></td>
          <td class="mono">${esc(r.remote || '—')}</td>
          <td class="mono">${esc(r.appVersion || '—')}</td>
          <td class="mono">${esc(fmtTime(r.receivedAt))}</td>
          <td>${statusChip(r.status)}</td>
        </tr>`
      )
      .join('')}</tbody>
  </table></div>`;
}

async function viewOverview(box) {
  const data = await api('stats?days=14');
  const t = data.totals;
  const series = data.series || [];
  const days = series.length || 14;

  box.innerHTML = `
    <div class="page-head">
      <div>
        <span class="eyebrow">Overview</span>
        <h1>系统运行档案</h1>
        <p>最近 ${days} 天的蓝屏报告、错误分布与处理进度。</p>
      </div>
      <div class="detail-actions">
        <a class="btn btn-ghost btn-sm" href="#/crashes">查看全部报告</a>
      </div>
    </div>

    <div class="kpis">
      <div class="kpi">
        <div class="label">累计报告</div>
        <div class="value">${t.all}</div>
        <div class="hint">${t.demo ? `含 ${t.demo} 份演示数据` : '全部为真实蓝屏'}</div>
      </div>
      <div class="kpi">
        <div class="label">近 7 天</div>
        <div class="value">${t.last7}</div>
        <div class="hint">${t.last7 ? '最近一周新增' : '最近一周没有新报告'}</div>
      </div>
      <div class="kpi accent">
        <div class="label">待处理</div>
        <div class="value">${t.pending}</div>
        <div class="hint">新报告 + 处理中</div>
      </div>
      <div class="kpi">
        <div class="label">涉及机器</div>
        <div class="value">${t.machines}</div>
        <div class="hint">按来源 IP 去重</div>
      </div>
    </div>

    <div class="grid-2">
      <div class="card">
        <div class="card-head"><h2>每日报告</h2><span class="sub">浅色为全部 · 琥珀色为严重及以上</span></div>
        ${chartBlock(series)}
      </div>
      <div class="card">
        <div class="card-head"><h2>错误码分布</h2><span class="sub">Top ${Math.min(8, data.bugchecks.length)}</span></div>
        ${distList(data.bugchecks || [])}
      </div>
    </div>

    <div class="card">
      <div class="card-head"><h2>最近报告</h2><span class="sub">点一行看详情</span></div>
      ${recentTable(data.recent || [])}
    </div>`;

  bindRows(box);
}

function bindRows(box) {
  for (const tr of box.querySelectorAll('tbody tr[data-id]')) {
    tr.addEventListener('click', () => {
      location.hash = `#/crashes/${tr.dataset.id}`;
    });
  }
}

/* ------------------------------------------------------------------ */
/* 报告列表                                                           */
/* ------------------------------------------------------------------ */

const listFilter = { status: '', q: '', mode: '' };

async function viewCrashes(box) {
  const params = new URLSearchParams();
  if (listFilter.status) params.set('status', listFilter.status);
  if (listFilter.q) params.set('q', listFilter.q);
  if (listFilter.mode) params.set('mode', listFilter.mode);
  const data = await api(`crashes?${params.toString()}`);

  box.innerHTML = `
    <div class="page-head">
      <div>
        <span class="eyebrow">Reports</span>
        <h1>蓝屏报告</h1>
        <p>共 ${data.total} 份。按错误码、机器或版本搜索。</p>
      </div>
    </div>

    <div class="toolbar">
      <div class="search">
        <svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="6.5" /><path d="M20 20l-4.3-4.3" /></svg>
        <input id="f-q" placeholder="搜索错误码、机器 IP、版本或备注" value="${esc(listFilter.q)}" />
      </div>
      <div class="seg" id="f-status">
        ${[
          ['', '全部'],
          ['new', '待处理'],
          ['investigating', '处理中'],
          ['fixed', '已修复'],
          ['ignored', '已忽略'],
        ]
          .map(
            ([k, label]) =>
              `<button data-status="${k}" class="${listFilter.status === k ? 'on' : ''}">${label}</button>`
          )
          .join('')}
      </div>
      <select id="f-mode">
        <option value="">全部来源</option>
        <option value="real" ${listFilter.mode === 'real' ? 'selected' : ''}>真实蓝屏</option>
        <option value="demo" ${listFilter.mode === 'demo' ? 'selected' : ''}>演示数据</option>
      </select>
    </div>

    <div class="card">
      ${
        data.items.length
          ? `<div class="table-wrap"><table>
        <thead><tr><th>级别</th><th>错误码</th><th>报告</th><th>机器</th><th>版本</th><th>收到时间</th><th>状态</th><th></th></tr></thead>
        <tbody>${data.items
          .map(
            (r) => `<tr data-id="${esc(r.id)}">
              <td>${sevTag(severityOf(r))}</td>
              <td><span class="chip code">${esc(r.analysis?.code || r.bugcheck || '未知')}</span></td>
              <td><span class="cell-title">${esc(r.analysis?.title || '—')}</span>
                <span class="sub mono">${esc(r.id)}</span></td>
              <td class="mono">${esc(r.remote || '—')}</td>
              <td class="mono">${esc(r.appVersion || '—')}</td>
              <td class="mono">${esc(fmtTime(r.receivedAt))}</td>
              <td>${statusChip(r.status)}${r.mode === 'demo' ? ' <span class="chip muted">演示</span>' : ''}</td>
              <td class="mono" style="color:var(--muted)">›</td>
            </tr>`
          )
          .join('')}</tbody></table></div>`
          : `<div class="empty"><strong>没有符合条件的报告</strong>换个筛选条件试试。</div>`
      }
    </div>`;

  bindRows(box);

  const seg = $('#f-status');
  seg.addEventListener('click', (e) => {
    const btn = e.target.closest('button[data-status]');
    if (!btn) return;
    listFilter.status = btn.dataset.status;
    for (const b of seg.querySelectorAll('button')) b.classList.toggle('on', b === btn);
    viewCrashes(box);
  });

  const q = $('#f-q');
  q.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    listFilter.q = q.value.trim();
    viewCrashes(box);
  });
  q.addEventListener('blur', () => {
    if (q.value.trim() === listFilter.q) return;
    listFilter.q = q.value.trim();
    viewCrashes(box);
  });

  $('#f-mode').addEventListener('change', (e) => {
    listFilter.mode = e.target.value;
    viewCrashes(box);
  });
}

/* ------------------------------------------------------------------ */
/* 报告详情                                                           */
/* ------------------------------------------------------------------ */

function findingsBlock(findings) {
  if (!findings?.length) return '';
  return `<div class="card">
    <div class="card-head"><h2>线索</h2><span class="sub">${findings.length} 条</span></div>
    <div class="findings">${findings
      .map(
        (f) => `<div class="finding ${esc(f.level)}">
          <span class="dot"></span>
          <div><strong>${esc(f.title)}</strong><p>${esc(f.detail)}</p></div>
        </div>`
      )
      .join('')}</div>
  </div>`;
}

function stepsBlock(repair) {
  if (!repair) return '';
  const steps = repair.steps || [];
  const body = steps.length
    ? `<div class="steps">${steps
        .map((s) => {
          const cls = s.skipped ? 'skip' : s.ok ? 'ok' : 'fail';
          const mark = s.skipped ? '»' : s.ok ? '✓' : '✕';
          return `<div class="step ${cls}">
            <span class="mark">${mark}</span>
            <div>
              <div class="name">${esc(s.name)}<span>${esc(s.key)}</span></div>
              <div class="sum">${esc(s.summary || '—')}</div>
            </div>
            <span class="time">${s.seconds != null ? `${s.seconds}s` : ''}</span>
          </div>`;
        })
        .join('')}</div>`
    : '<div class="empty">这一轮没有跑修复环节</div>';

  const verdict = repair.verdict
    ? `<div class="note" style="margin-bottom:12px">${esc(repair.verdict)}</div>`
    : '';

  return `<div class="card">
    <div class="card-head">
      <h2>自动修复</h2>
      <span class="sub">${repair.admin === false ? '没有管理员权限' : repair.demo ? '演示模式未执行' : '按环节记录'}</span>
    </div>
    ${verdict}${body}
  </div>`;
}

function systemBlock(system) {
  if (!system?.length) return '';
  return `<div class="card">
    <div class="card-head"><h2>系统信息</h2></div>
    <dl class="kv">${system
      .map((s) => `<dt>${esc(s.name)}</dt><dd>${esc(s.value)}</dd>`)
      .join('')}</dl>
  </div>`;
}

function filesBlock(files, id) {
  if (!files?.length) return '';
  return `<div class="card">
    <div class="card-head"><h2>包内文件</h2><span class="sub">${files.length} 个</span></div>
    <div class="files">${files
      .map((f) => {
        const textLike = /\.(txt|log|json|md)$/i.test(f.name) && f.size < 2 * 1024 * 1024;
        return `<div class="file-row" ${
          textLike ? `data-file="${esc(f.name)}"` : ''
        } title="${textLike ? '点击预览' : '下载整包后可查看'}">
          <svg viewBox="0 0 24 24"><path d="M6 3h8l4 4v14H6z" /><path d="M14 3v4h4" /></svg>
          <span>${esc(f.name)}</span>
          <span class="size">${esc(fmtBytes(f.size))}</span>
        </div>`;
      })
      .join('')}</div>
  </div>`;
}

function logsBlock(logs) {
  if (!logs?.length) return '';
  const log = logs[0];
  return `<div class="card">
    <div class="card-head">
      <h2>应用日志</h2>
      <span class="sub">${log.errorCount} 行错误${
        log.errors?.length && log.errors.length < log.errorCount
          ? `（去重 ${log.errors.length} 类）`
          : ''
      } · ${esc(fmtBytes(log.bytes))}</span>
    </div>
    ${
      log.errors?.length
        ? `<div class="findings">${log.errors
            .slice(0, 6)
            .map(
              (e) => `<div class="finding warn"><span class="dot"></span>
                <div><strong>第 ${e.line} 行</strong><p>${esc(e.text)}</p></div></div>`
            )
            .join('')}</div>`
        : '<div class="note">没有挑出错误行。</div>'
    }
    <div style="display:flex;gap:8px;margin-top:14px">
      <button class="btn btn-sm" id="btn-log">查看完整日志</button>
      <span class="sub" style="align-self:center;color:var(--muted)">最多显示 512 KB</span>
    </div>
    <div id="log-view"></div>
  </div>`;
}

async function viewDetail(box, id) {
  const data = await api(`crashes/${encodeURIComponent(id)}`);
  const r = data.report;
  const a = r.analysis || {};
  const sev = severityOf(r);

  box.innerHTML = `
    <div class="detail-head">
      <div>
        <div class="detail-title">
          <h1>${esc(r.id)}</h1>
          <span class="chip ${esc(sev)}">${esc(SEVERITY_TEXT[sev] || '未知')}</span>
          ${statusChip(r.status)}
          ${r.mode === 'demo' ? '<span class="chip muted">演示数据</span>' : ''}
        </div>
        <div class="detail-meta">
          <span>机器 <b>${esc(r.remote || '—')}</b></span>
          <span>版本 <b>${esc(r.appVersion || '—')}</b></span>
          <span>收到 <b>${esc(fmtTime(r.receivedAt))}</b></span>
          <span>发生 <b>${esc(fmtTime(r.occurredAt))}</b></span>
          <span>大小 <b>${esc(fmtBytes(r.bytes))}</b></span>
        </div>
      </div>
      <div class="detail-actions">
        <a class="btn btn-sm" href="${API_BASE}crashes/${encodeURIComponent(r.id)}/download">下载整包</a>
        ${state.user?.role === 'admin' ? '<button class="btn btn-sm btn-danger" id="btn-del">删除</button>' : ''}
      </div>
    </div>

    ${
      r.error
        ? `<div class="note danger" style="margin-bottom:16px">${esc(r.error)}</div>`
        : ''
    }

    <div class="detail-grid">
      <div class="stack">
        <div class="card">
          <div class="card-head"><h2>自动分析</h2><span class="sub">${
            a.known ? '错误码已识别' : '词库里没有这个码'
          }</span></div>
          <div class="analysis-code">${esc(a.code || r.bugcheck || '—')} ${
            a.name ? `/ ${esc(a.name)}` : ''
          }</div>
          <h3 class="analysis-title">${esc(a.title || '—')}</h3>
          <div class="analysis-body">
            <div>
              <span class="list-label">常见原因</span>
              <ul class="bullets">${(a.causes || [])
                .map((c) => `<li>${esc(c)}</li>`)
                .join('')}</ul>
            </div>
            <div>
              <span class="list-label">建议动作</span>
              <ul class="bullets actions">${(a.actions || [])
                .map((c) => `<li>${esc(c)}</li>`)
                .join('')}</ul>
            </div>
          </div>
        </div>

        ${findingsBlock(a.findings)}
        ${stepsBlock(r.repair)}
        ${logsBlock(r.logs)}
      </div>

      <div class="stack">
        <div class="card status-card">
          <div class="card-head"><h2>处理状态</h2><span class="sub">${esc(
            r.updatedBy ? `${r.updatedBy} · ${fmtTime(r.updatedAt)}` : '还没人处理'
          )}</span></div>
          <div class="row">
            <select id="st">
              ${Object.entries(STATUS_TEXT)
                .map(
                  ([k, label]) =>
                    `<option value="${k}" ${r.status === k ? 'selected' : ''}>${label}</option>`
                )
                .join('')}
            </select>
            <button class="btn btn-primary btn-sm" id="btn-save">保存</button>
          </div>
          <textarea id="note" placeholder="写一句处理记录，比如「已换内存条，观察中」" style="margin-top:10px">${esc(
            r.note || ''
          )}</textarea>
        </div>

        ${systemBlock(r.system)}
        ${filesBlock(r.files, r.id)}

        ${
          r.summary?.collectError
            ? `<div class="note warn">采集不完整：${esc(r.summary.collectError)}</div>`
            : ''
        }
      </div>
    </div>`;

  $('#btn-save').addEventListener('click', async () => {
    const btn = $('#btn-save');
    btn.disabled = true;
    try {
      await api(`crashes/${encodeURIComponent(r.id)}`, {
        method: 'PATCH',
        body: { status: $('#st').value, note: $('#note').value },
      });
      toast('已保存');
      refreshPendingBadge();
      await render();
    } catch (e) {
      toast(e.message, 'err');
    } finally {
      btn.disabled = false;
    }
  });

  $('#btn-del')?.addEventListener('click', async () => {
    if (!confirm(`删除 ${r.id}？包和元数据都会从服务器上移除，不能恢复。`)) return;
    try {
      await api(`crashes/${encodeURIComponent(r.id)}`, { method: 'DELETE' });
      toast('已删除');
      location.hash = '#/crashes';
    } catch (e) {
      toast(e.message, 'err');
    }
  });

  $('#btn-log')?.addEventListener('click', async () => {
    const view = $('#log-view');
    view.innerHTML = '<div class="empty">正在读取日志…</div>';
    try {
      const res = await fetch(
        `${API_BASE}crashes/${encodeURIComponent(r.id)}/file?name=${encodeURIComponent(
          'logs/openpptview.log'
        )}&_=${Date.now()}`,
        { credentials: 'same-origin' }
      );
      if (!res.ok) throw new Error(`读取失败（${res.status}）`);
      renderLog(await res.text(), view);
    } catch (e) {
      view.innerHTML = `<div class="note danger">${esc(e.message)}</div>`;
    }
  });

  for (const row of box.querySelectorAll('.file-row[data-file]')) {
    row.addEventListener('click', async () => {
      const name = row.dataset.file;
      const res = await fetch(
        `${API_BASE}crashes/${encodeURIComponent(r.id)}/file?name=${encodeURIComponent(
          name
        )}&_=${Date.now()}`,
        { credentials: 'same-origin' }
      );
      if (!res.ok) return toast('预览失败', 'err');
      const text = await res.text();
      const view = $('#log-view');
      view.innerHTML = '';
      renderLog(text, view, name);
      view.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
    });
  }
}

function renderLog(text, container, name = 'logs/openpptview.log') {
  const lines = String(text || '').split(/\r?\n/);
  const MAX = 2000;
  const body = lines
    .slice(0, MAX)
    .map((line, i) => {
      const cls = /\b(ERROR|FATAL|PANIC|panic)\b/.test(line)
        ? 'err'
        : /\b(WARN|WARNING)\b/.test(line)
          ? 'warn'
          : '';
      return `<div class="log-line ${cls}"><span class="no">${i + 1}</span><span>${esc(
        line || ' '
      )}</span></div>`;
    })
    .join('');
  container.innerHTML = `<div class="card-head" style="margin:16px 0 0"><h2>${esc(
    name
  )}</h2><span class="sub">${lines.length} 行${lines.length > MAX ? `（只显示前 ${MAX} 行）` : ''}</span></div>
  <div class="log-box">${body}</div>`;
}

/* ------------------------------------------------------------------ */
/* 远程协助                                                           */
/* ------------------------------------------------------------------ */

const remoteFilter = { q: '' };

/** 取一帧画面。`since` 是已经看到过的帧号：没变化时服务器回 204，省一条下行。 */
async function fetchFrame(hwid, since) {
  const res = await fetch(
    `${API_BASE}remote/frame?hwid=${encodeURIComponent(hwid)}&since=${since}&_=${Date.now()}`,
    { credentials: 'same-origin', headers: { 'X-Oppv-Client': 'admin' } }
  );
  if (res.status === 204) return null;
  if (!res.ok) {
    let msg = `取画面失败（${res.status}）`;
    try {
      msg = (await res.json()).error || msg;
    } catch {
      /* 不是 JSON 就用手拼的那句 */
    }
    throw new Error(msg);
  }
  const seq = Number(res.headers.get('X-Frame-Seq') || 0);
  const w = Number(res.headers.get('X-Frame-W') || 0);
  const h = Number(res.headers.get('X-Frame-H') || 0);
  const url = URL.createObjectURL(await res.blob());
  return { seq, w, h, url };
}

/** 发一条命令。失败只弹一句，不打断正在做的事。 */
async function sendRemote(hwid, kind, arg) {
  try {
    await api('remote/cmd', { method: 'POST', body: { hwid, kind, arg } });
    return true;
  } catch (e) {
    toast(e.message);
    return false;
  }
}

async function viewRemote(box, hwid) {
  if (hwid) return viewRemoteMachine(box, hwid);

  box.innerHTML = `
    <div class="page-head">
      <div>
        <span class="eyebrow">Remote</span>
        <h1>远程协助</h1>
        <p>装了本程序、开着机、也没关掉「远程协助」的电脑，每隔几秒来报一次到。点一台进去看屏幕、跑命令。</p>
      </div>
    </div>

    <div class="toolbar">
      <div class="search">
        <svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="6.5" /><path d="M20 20l-4.3-4.3" /></svg>
        <input id="rm-q" placeholder="搜计算机名、机器码、用户或 IP" value="${esc(remoteFilter.q)}" />
      </div>
      <span class="sub" id="rm-count">正在读取…</span>
    </div>

    <div class="card"><div id="rm-list"><div class="empty">正在读取…</div></div></div>

    <p class="sub" style="margin-top:14px;line-height:1.8">
      心跳只说明「这台机器在」。机器上设置页的「远程协助」一关，它立刻不再上报，
      这里就会显示离线 —— 离线之后，之前的命令和画面都不会再更新。
    </p>`;

  const listEl = $('#rm-list');
  const countEl = $('#rm-count');

  const paint = async () => {
    try {
      const data = await api('remote/machines');
      const all = data.machines || [];
      const q = remoteFilter.q.toLowerCase();
      const items = q
        ? all.filter((m) =>
            [m.name, m.hwid, m.user, m.ip].join(' ').toLowerCase().includes(q)
          )
        : all;
      countEl.textContent = `${all.filter((m) => m.online).length} 台在线 · 共 ${all.length} 台记录`;
      listEl.innerHTML = items.length
        ? `<div class="table-wrap"><table>
            <thead><tr><th>状态</th><th>计算机</th><th>登录用户</th><th>权限</th><th>版本</th><th>地址</th><th>最后心跳</th><th>屏幕</th></tr></thead>
            <tbody>${items
              .map(
                (m) => `<tr data-hwid="${esc(m.hwid)}">
                  <td>${
                    m.online
                      ? '<span class="chip fixed"><i></i>在线</span>'
                      : '<span class="chip muted">离线</span>'
                  }</td>
                  <td><span class="cell-title">${esc(m.name || '未命名')}</span>
                    <span class="sub mono">${esc(m.hwid)}</span></td>
                  <td>${esc(m.user || '—')}</td>
                  <td>${
                    m.admin
                      ? '<span class="chip high">管理员</span>'
                      : '<span class="chip muted">普通用户</span>'
                  }</td>
                  <td class="mono">${esc(m.version || '—')}</td>
                  <td class="mono">${esc(m.ip || '—')}</td>
                  <td class="mono">${esc(fmtTime(m.lastSeen))}${
                    m.pending ? ` <span class="chip new">待执行 ${m.pending}</span>` : ''
                  }</td>
                  <td>${m.screen ? '<span class="chip code">推流中</span>' : '—'}</td>
                </tr>`
              )
              .join('')}</tbody></table></div>`
        : `<div class="empty"><strong>还没有机器来报到</strong>确认那台机器装了 0.3.13 及以上版本、开着「远程协助」，并且填的上报服务器就是这台。</div>`;
      for (const row of listEl.querySelectorAll('tbody tr[data-hwid]')) {
        row.addEventListener('click', () => {
          location.hash = `#/remote/${row.dataset.hwid}`;
        });
      }
    } catch (e) {
      listEl.innerHTML = `<div class="empty"><strong>读不出机器列表</strong>${esc(e.message)}</div>`;
    }
  };

  await paint();
  state.remoteTimers.push(setInterval(paint, 4000));

  const q = $('#rm-q');
  q.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    remoteFilter.q = q.value.trim();
    paint();
  });
  q.addEventListener('blur', () => {
    if (q.value.trim() === remoteFilter.q) return;
    remoteFilter.q = q.value.trim();
    paint();
  });
}

async function viewRemoteMachine(box, hwid) {
  box.innerHTML = `
    <div class="page-head">
      <div>
        <a class="link-btn" href="#/remote">‹ 返回机器列表</a>
        <h1 id="rm-name">${esc(hwid)}</h1>
        <p id="rm-meta">正在读取…</p>
      </div>
      <div>
        <button class="btn btn-primary" id="rm-screen">打开屏幕</button>
      </div>
    </div>

    <div class="grid-2">
      <div class="card">
        <div class="card-head"><h2>屏幕</h2><span class="sub" id="rm-screen-sub">未打开</span></div>
        <div class="remote-view">
          <div class="empty" id="rm-placeholder"><strong>屏幕还没打开</strong>点上面的「打开屏幕」，机器 1～2 秒后开始推画面。</div>
          <img id="rm-img" hidden alt="远程桌面" />
        </div>
      </div>

      <div class="card">
        <div class="card-head"><h2>命令</h2><span class="sub">以那台机器当前的权限运行 cmd.exe</span></div>
        <div class="remote-term">
          <div class="field row">
            <input id="rm-cmd" placeholder="whoami" spellcheck="false" autocomplete="off" />
            <button class="btn btn-primary btn-sm" id="rm-run">执行</button>
          </div>
          <div class="remote-keys" id="rm-keys">
            ${[
              ['enter', 'Enter'],
              ['esc', 'Esc'],
              ['tab', 'Tab'],
              ['backspace', '退格'],
              ['up', '↑'],
              ['down', '↓'],
              ['left', '←'],
              ['right', '→'],
              ['win', 'Win'],
              ['f5', 'F5'],
            ]
              .map(([k, label]) => `<button class="btn btn-sm" data-key="${k}">${label}</button>`)
              .join('')}
          </div>
          <div class="field row">
            <input id="rm-text" placeholder="往活动窗口打字（中文也可以）" autocomplete="off" />
            <button class="btn btn-sm" id="rm-type">输入</button>
          </div>
          <div class="remote-out" id="rm-out"><span class="sub">还没有命令。</span></div>
        </div>
      </div>
    </div>

    <p class="sub" style="margin-top:14px;line-height:1.8">
      点画面 = 单击，双击 = 双击，右键 = 右键，滚轮 = 上下滚。
      命令结果会随机器下一次心跳（约 3 秒）传回来；单条最长跑 30 秒、输出最多 60 KB。
    </p>`;

  let seq = 0;
  let screenOn = false;
  let busy = false;
  let clickTimer = null;

  const paintHead = async () => {
    try {
      const data = await api('remote/machines');
      const m = (data.machines || []).find((x) => x.hwid === hwid);
      if (!m) {
        $('#rm-meta').textContent = '这台机器还没有报到过，或者记录已经过期。';
        $('#rm-screen').disabled = true;
        return;
      }
      $('#rm-name').textContent = m.name || hwid;
      $('#rm-meta').textContent = [
        m.online ? '在线' : '离线',
        m.user || '—',
        m.admin ? '管理员权限' : '普通权限',
        m.version || '版本未知',
        m.ip || '',
        `最后心跳 ${fmtTime(m.lastSeen)}`,
      ].join(' · ');
      screenOn = Boolean(m.online && m.screen);
      $('#rm-screen').textContent = screenOn ? '关闭屏幕' : '打开屏幕';
      $('#rm-screen').disabled = !m.online;
    } catch (e) {
      $('#rm-meta').textContent = `读不到状态：${e.message}`;
    }
  };

  const paintResults = async () => {
    try {
      const data = await api(`remote/results?hwid=${encodeURIComponent(hwid)}&limit=20`);
      const results = data.results || [];
      $('#rm-out').innerHTML = results.length
        ? results
            .map(
              (r) => `<div class="remote-log ${r.ok ? '' : 'err'}">
                <span class="sub mono">${esc(fmtTime(r.at))} · ${r.ms} ms · ${r.ok ? '成功' : '失败'}</span>
                <pre class="mono">${esc(r.out || '（没有输出）')}</pre>
              </div>`
            )
            .join('')
        : '<span class="sub">还没有命令。在上面的输入框里发一条试试。</span>';
    } catch (e) {
      $('#rm-out').textContent = `读不到命令结果：${e.message}`;
    }
  };

  const step = async () => {
    if (!screenOn || busy) return;
    busy = true;
    try {
      const f = await fetchFrame(hwid, seq);
      if (f) {
        seq = f.seq;
        const img = $('#rm-img');
        if (img) {
          if (state.remoteFrameUrl) URL.revokeObjectURL(state.remoteFrameUrl);
          state.remoteFrameUrl = f.url;
          img.src = f.url;
          img.hidden = false;
          $('#rm-placeholder')?.classList.add('hidden');
          $('#rm-screen-sub').textContent = `${f.w}×${f.h} · 帧 ${f.seq}`;
        }
      }
    } catch (e) {
      const sub = $('#rm-screen-sub');
      if (sub) sub.textContent = e.message;
    }
    busy = false;
  };

  await paintHead();
  await paintResults();
  state.remoteTimers.push(setInterval(paintHead, 2500));
  state.remoteTimers.push(setInterval(paintResults, 2500));
  state.remoteTimers.push(setInterval(step, 1000));

  $('#rm-screen').addEventListener('click', async (e) => {
    const btn = e.currentTarget;
    btn.disabled = true;
    try {
      const data = await api('remote/screen', {
        method: 'POST',
        body: { hwid, on: !screenOn },
      });
      screenOn = Boolean(data.screen);
      if (!screenOn) {
        $('#rm-img').hidden = true;
        $('#rm-placeholder')?.classList.remove('hidden');
        $('#rm-screen-sub').textContent = '未打开';
      } else {
        seq = 0;
        $('#rm-placeholder')?.classList.add('hidden');
        $('#rm-screen-sub').textContent = '正在等第一帧…';
      }
      toast(screenOn ? '已请那台机器开始推画面' : '已停止推画面');
      await paintHead();
    } catch (err) {
      toast(err.message);
    } finally {
      btn.disabled = false;
    }
  });

  // —— 鼠标 ——
  const img = $('#rm-img');
  const at = (e) => {
    const r = img.getBoundingClientRect();
    const clamp = (v) => Math.min(1, Math.max(0, v));
    return {
      x: clamp(r.width ? (e.clientX - r.left) / r.width : 0),
      y: clamp(r.height ? (e.clientY - r.top) / r.height : 0),
    };
  };
  const mouse = (p, a, dy = 0) =>
    sendRemote(hwid, 'mouse', JSON.stringify({ x: p.x, y: p.y, a, dy }));

  img.addEventListener('click', (e) => {
    const p = at(e);
    // 等一会儿再点：如果紧接着来了 dblclick，就只发一次双击，别把界面点乱
    if (clickTimer) return;
    clickTimer = setTimeout(() => {
      clickTimer = null;
      mouse(p, 'left');
    }, 230);
  });
  img.addEventListener('dblclick', (e) => {
    if (clickTimer) {
      clearTimeout(clickTimer);
      clickTimer = null;
    }
    mouse(at(e), 'double');
  });
  img.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    mouse(at(e), 'right');
  });
  img.addEventListener(
    'wheel',
    (e) => {
      e.preventDefault();
      mouse(at(e), 'wheel', Math.sign(e.deltaY) || 1);
    },
    { passive: false }
  );

  // —— 键盘 ——
  for (const btn of $('#rm-keys').querySelectorAll('button[data-key]')) {
    btn.addEventListener('click', () =>
      sendRemote(hwid, 'key', JSON.stringify({ keys: [btn.dataset.key] }))
    );
  }

  const sendText = () => {
    const text = $('#rm-text').value;
    if (!text) return;
    sendRemote(hwid, 'key', JSON.stringify({ text }));
    $('#rm-text').value = '';
  };
  $('#rm-type').addEventListener('click', sendText);
  $('#rm-text').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      sendText();
    }
  });

  const run = () => {
    const text = $('#rm-cmd').value.trim();
    if (!text) return;
    sendRemote(hwid, 'shell', text);
    $('#rm-cmd').value = '';
    toast('已发出，结果会在几秒内回来');
  };
  $('#rm-run').addEventListener('click', run);
  $('#rm-cmd').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      run();
    }
  });
}

/* ------------------------------------------------------------------ */
/* 成员                                                               */
/* ------------------------------------------------------------------ */

async function viewUsers(box) {
  const data = await api('users');
  const me = state.user?.id;

  box.innerHTML = `
    <div class="page-head">
      <div>
        <span class="eyebrow">Members</span>
        <h1>成员</h1>
        <p>第一个注册的账号是管理员。成员只能看报告，不能删除、不能管人。</p>
      </div>
    </div>
    <div class="card">
      <div class="table-wrap"><table>
        <thead><tr><th>用户名</th><th>角色</th><th>创建时间</th><th>最近登录</th><th></th></tr></thead>
        <tbody>${data.users
          .map(
            (u) => `<tr style="cursor:default">
              <td>${esc(u.username)}${u.id === me ? ' <span class="chip muted">你</span>' : ''}</td>
              <td>
                <select data-role="${esc(u.id)}" ${u.id === me ? 'disabled' : ''}>
                  <option value="admin" ${u.role === 'admin' ? 'selected' : ''}>管理员</option>
                  <option value="viewer" ${u.role === 'viewer' ? 'selected' : ''}>成员</option>
                </select>
              </td>
              <td class="mono">${esc(fmtTime(u.createdAt))}</td>
              <td class="mono">${esc(fmtTime(u.lastLoginAt))}</td>
              <td style="text-align:right">${
                u.id === me
                  ? ''
                  : `<button class="btn btn-sm btn-danger" data-del="${esc(u.id)}">删除</button>`
              }</td>
            </tr>`
          )
          .join('')}</tbody>
      </table></div>
    </div>`;

  for (const sel of box.querySelectorAll('select[data-role]')) {
    sel.addEventListener('change', async () => {
      try {
        await api(`users/${sel.dataset.role}`, { method: 'PATCH', body: { role: sel.value } });
        toast('角色已更新');
      } catch (e) {
        toast(e.message, 'err');
        await render();
      }
    });
  }
  for (const btn of box.querySelectorAll('button[data-del]')) {
    btn.addEventListener('click', async () => {
      if (!confirm('删除这个账号？他/她会立即被退出登录。')) return;
      try {
        await api(`users/${btn.dataset.del}`, { method: 'DELETE' });
        toast('已删除');
        await render();
      } catch (e) {
        toast(e.message, 'err');
      }
    });
  }
}

/* ------------------------------------------------------------------ */
/* 账号                                                               */
/* ------------------------------------------------------------------ */

function viewAccount(box) {
  box.innerHTML = `
    <div class="page-head">
      <div>
        <span class="eyebrow">Account</span>
        <h1>账号</h1>
        <p>${esc(state.user?.username || '')} · ${state.user?.role === 'admin' ? '管理员' : '成员'}</p>
      </div>
      <button class="btn btn-sm" id="btn-logout">退出登录</button>
    </div>

    <div class="card" style="max-width:520px">
      <div class="card-head"><h2>修改密码</h2><span class="sub">至少 8 位</span></div>
      <label class="field"><span>原密码</span><input id="pw-old" type="password" autocomplete="current-password" /></label>
      <label class="field" style="margin-top:12px"><span>新密码</span><input id="pw-new" type="password" autocomplete="new-password" /></label>
      <div style="margin-top:14px"><button class="btn btn-primary btn-sm" id="btn-pw">保存</button></div>
    </div>`;

  $('#btn-pw').addEventListener('click', async () => {
    try {
      await api('auth/password', {
        method: 'POST',
        body: { oldPassword: $('#pw-old').value, newPassword: $('#pw-new').value },
      });
      $('#pw-old').value = '';
      $('#pw-new').value = '';
      toast('密码已更新');
    } catch (e) {
      toast(e.message, 'err');
    }
  });

  $('#btn-logout').addEventListener('click', async () => {
    try {
      await api('auth/logout', { method: 'POST' });
    } catch {
      /* 退出失败也照样回登录页 */
    }
    state.user = null;
    showAuth();
  });
}

/* ------------------------------------------------------------------ */
/* 启动                                                               */
/* ------------------------------------------------------------------ */

async function boot() {
  try {
    const data = await api('auth/status');
    state.initialized = data.initialized;
    state.user = data.user || null;
    if (state.user) {
      await enterApp();
      return;
    }
    showAuth();
  } catch (e) {
    $('#boot').innerHTML = `<div class="empty"><strong>连不上服务</strong>${esc(e.message)}</div>`;
  }
}

$('#auth-form').addEventListener('submit', submitAuth);
$('#auth-switch').addEventListener('click', () => {
  state.authMode = state.authMode === 'login' ? 'register' : 'login';
  paintAuth();
});
$('#btn-refresh').addEventListener('click', () => render());
window.addEventListener('hashchange', () => render());
boot();
