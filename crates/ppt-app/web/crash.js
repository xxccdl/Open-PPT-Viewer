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

function render(phase, message) {
  const p = PHASES[phase] || PHASES.analyzing;
  document.body.dataset.phase = phase;
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
  } catch (e) {
    console.warn('订阅 crash-phase 失败', e);
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
