/* 蓝屏诊断窗口的行为。
 *
 * 顺序上有一件事很重要：**先挂监听，再告诉后端「可以开始了」**。
 * 反过来做的话，后端在 WebView 还没注册好监听时就把 `crash-phase`
 * 发出去了，界面会一直停在「正在分析…」—— 看起来就是卡死。
 * 所以后端只负责把窗口叫出来（见 `crash::show_window`），
 * 真正开始分析要等这里的 `crash_ready`。
 */

const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;

const PHASES = {
  analyzing: {
    title: '正在分析…',
    detail: '',
  },
  // 缩到右下角之后：先把修复跑完（可能十几分钟），再上传
  repairing: {
    title: '正在尝试修复…',
    detail: '',
  },
  sending: {
    title: '请稍后，正在将错误发送至服务器…',
    detail: '',
  },
  sent: {
    title: '已发送，谢谢',
    detail: '报告已送达，我们会照着它查',
  },
  local: {
    title: '报告已保存在本机',
    detail: '没有配置上报服务器',
  },
  failed: {
    title: '没能发送出去，报告已留在本机',
    detail: '',
  },
};

const detailEl = document.getElementById('detail');
const skipBtn = document.getElementById('skip-btn');
const notesEl = document.getElementById('notes');

/**
 * 采集进度：一行一项，最多留 4 行。
 *
 * 后端每做完一项就发 `crash-note`（读事件记录 / 采集系统信息 / 快速体检 …）。
 * 上一行打勾、新行淡入 —— 让窗口看起来「在往前走」，而不是一个不动的省略号。
 */
const notes = [];
function pushNote(text) {
  const t = (text || '').trim();
  if (!t || !notesEl) return;
  if (notes[notes.length - 1] === t) return; // 同一项重复报，不刷屏
  notes.push(t);
  if (notes.length > 4) notes.shift();

  const done = notes.slice(0, -1);
  const last = notes[notes.length - 1];
  notesEl.innerHTML = '';
  for (const n of done) {
    const li = document.createElement('li');
    li.className = 'note done';
    li.textContent = n;
    notesEl.appendChild(li);
  }
  const li = document.createElement('li');
  li.className = 'note now';
  li.textContent = last;
  notesEl.appendChild(li);
}

/**
 * 「跳过此环节」的显隐。
 *
 * 后端只在某个环节跑满 20 秒后才发 `crash-skip-available`；任何一个
 * 新环节开始（`crash-phase`）或者整轮修复结束，都立刻收起来 ——
 * 不能让老师点到一个已经过期、点下去毫无反应的按钮。
 */
function setSkip(on) {
  document.body.dataset.skip = on ? 'on' : 'off';
  if (skipBtn) skipBtn.disabled = false;
}

function render(phase, message) {
  const p = PHASES[phase] || PHASES.analyzing;
  document.body.dataset.phase = phase;
  // 阶段一变，上一环节的「跳过」就作废了
  setSkip(false);
  if (phase === 'analyzing') return; // 大卡片那一段不换字
  const titleEl = document.querySelector('.compact-title');
  if (titleEl) titleEl.textContent = p.title;
  if (detailEl) detailEl.textContent = message || p.detail || '';
}

(async () => {
  try {
    await listen('crash-phase', (event) => {
      const payload = (event && event.payload) || {};
      render(payload.phase || 'analyzing', payload.message || '');
    });

    // 采集进度：每做完一项亮一行
    await listen('crash-note', (event) => {
      pushNote(String((event && event.payload) || ''));
    });

    // 某个环节跑太久了：把「跳过此环节」露出来
    await listen('crash-skip-available', (event) => {
      const step = (event && event.payload && event.payload.step) || '';
      setSkip(true);
      if (step && skipBtn) skipBtn.title = `跳过「${step}」`;
    });

    // 整轮修复结束
    await listen('crash-skip-closed', () => setSkip(false));
  } catch (e) {
    console.warn('订阅 crash-phase 失败', e);
  }

  if (skipBtn) {
    skipBtn.addEventListener('click', async () => {
      // 先自己收起来：连点两下不该发两次请求
      setSkip(false);
      skipBtn.disabled = true;
      // 按钮一按就没了，界面必须马上给一句话 ——
      // 否则「点了跳过」和「点了没反应」看起来一模一样。
      if (detailEl) detailEl.textContent = '正在结束这一环节，接着跑下一项…';
      try {
        const accepted = await invoke('crash_skip_step');
        if (!accepted) {
          // 后端说「没有环节在跑」—— 那就不该假装跳过了
          console.warn('跳过请求没被接收：当前没有正在执行的修复环节');
        }
      } catch (e) {
        console.warn('跳过失败', e);
      }
    });
  }

  // 先按「正在分析」画出来，再让后端开始 —— 不留白屏
  render('analyzing', '');

  try {
    await invoke('crash_ready');
  } catch (e) {
    // 后端没能启动分析时，别让窗口一直挂着
    render('failed', String(e));
  }
})();
