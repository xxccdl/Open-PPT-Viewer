/**
 * 报告解读：把「一个错误码 + 一包文件」翻成能照着做事的话。
 *
 * # 为什么要在服务器上做
 *
 * 报告是给老师/运维看的，不是给写驱动的人看的。错误码本身没法读，
 * 日志里几千行也没人翻。这里做三件事：
 *
 * 1. 把 bugcheck 码翻成「这是什么、常见原因、先做什么」；
 * 2. 把修复环节的结果、采集失败、缺转储这些**事实**变成线索清单；
 * 3. 从应用日志里挑出 ERROR/panic 行，去重后给出少量样本。
 *
 * 全部是确定性规则，不猜、不编：查不到的错误码就老实说查不到。
 */

/* ------------------------------------------------------------------ */
/* 错误码词库                                                          */
/* ------------------------------------------------------------------ */

/**
 * 常见蓝屏码。`actions` 是给现场的人看的，按「先做这个」排序。
 * 只收课堂上真的会遇到的那些，冷门码交给应用日志与转储去说。
 */
const CODES = {
  '0X000000EF': {
    name: 'CRITICAL_PROCESS_DIED',
    title: '关键系统进程被终止',
    causes: ['系统文件损坏（最常见）', '第三方安全软件/驱动强行结束进程', '磁盘出现坏块'],
    actions: ['跑 sfc /scannow 与 DISM /RestoreHealth', '卸载近期安装的安全软件或优化工具', 'chkdsk 检查系统盘'],
    severity: 'high',
  },
  '0X0000000A': {
    name: 'IRQL_NOT_LESS_OR_EQUAL',
    title: '驱动访问了不该访问的内存地址',
    causes: ['驱动与当前系统版本不兼容', '内存条接触不良或损坏', '第三方杀毒/虚拟化驱动'],
    actions: ['用驱动体检找出异常设备', '重插内存条，或换一根单条测试', '卸载最近更新的驱动'],
    severity: 'high',
  },
  '0X000000D1': {
    name: 'DRIVER_IRQL_NOT_LESS_OR_EQUAL',
    title: '驱动在过高的中断级别访问了分页内存',
    causes: ['网卡/无线网卡驱动（最常见）', '存储控制器驱动', '虚拟网卡/VPN 客户端'],
    actions: ['更新或回滚网卡驱动', '卸载 VPN/虚拟网卡软件', '查看转储里的驱动名'],
    severity: 'high',
  },
  '0X0000001A': {
    name: 'MEMORY_MANAGEMENT',
    title: '内存管理出错',
    causes: ['内存条故障', '磁盘坏块导致页面文件异常', '驱动破坏了内存结构'],
    actions: ['运行 Windows 内存诊断', 'chkdsk 检查系统盘', '单条内存轮换测试'],
    severity: 'high',
  },
  '0X0000003B': {
    name: 'SYSTEM_SERVICE_EXCEPTION',
    title: '系统服务调用时出现异常',
    causes: ['显卡驱动', '杀毒软件驱动', '刚装的系统补丁'],
    actions: ['更新显卡驱动', '卸载最近的安全软件更新', '回滚最近的系统补丁'],
    severity: 'medium',
  },
  '0X00000050': {
    name: 'PAGE_FAULT_IN_NONPAGED_AREA',
    title: '访问了不存在的内存页',
    causes: ['内存条损坏', '驱动缺陷', '磁盘/文件系统损坏'],
    actions: ['内存诊断', 'chkdsk /scan', '更新或卸载可疑驱动'],
    severity: 'high',
  },
  '0X0000007E': {
    name: 'SYSTEM_THREAD_EXCEPTION_NOT_HANDLED',
    title: '系统线程异常没有被处理',
    causes: ['驱动抛出未处理异常', '系统文件损坏'],
    actions: ['sfc /scannow', '更新最近安装的驱动'],
    severity: 'medium',
  },
  '0X0000009F': {
    name: 'DRIVER_POWER_STATE_FAILURE',
    title: '驱动没能正确响应电源状态切换',
    causes: ['网卡/蓝牙驱动', '显卡驱动', '电源管理策略'],
    actions: ['更新网卡与蓝牙驱动', '关闭「快速启动」', '关闭设备管理器里的「允许计算机关闭此设备以节约电源」'],
    severity: 'medium',
  },
  '0X00000124': {
    name: 'WHEA_UNCORRECTABLE_ERROR',
    title: '硬件报了不可纠正的错误',
    causes: ['CPU 过热或供电不稳', '内存条故障', '主板/电源老化'],
    actions: ['清灰、检查风扇与散热', '内存诊断', '这台机器先别安排上课，联系硬件维修'],
    severity: 'critical',
  },
  '0X00000116': {
    name: 'VIDEO_TDR_FAILURE',
    title: '显卡驱动超时未响应',
    causes: ['显卡驱动与硬件不匹配', '显卡过热', '显存故障'],
    actions: ['更新/回滚显卡驱动', '检查机箱散热', '换一台机器观察是否复现'],
    severity: 'medium',
  },
  '0X00000117': {
    name: 'VIDEO_TDR_TIMEOUT_DETECTED',
    title: '显卡在规定时间内没有响应',
    causes: ['显卡驱动', '显卡硬件老化'],
    actions: ['更新显卡驱动', '降低硬件加速'],
    severity: 'medium',
  },
  '0X00000133': {
    name: 'DPC_WATCHDOG_VIOLATION',
    title: '某个驱动占用时间过长，看门狗触发',
    causes: ['SATA/NVMe 驱动或 SSD 固件', '网卡驱动', '虚拟化软件'],
    actions: ['升级 SSD 固件', '更新存储控制器驱动', '卸载虚拟化/沙盘类软件'],
    severity: 'high',
  },
  '0X00000139': {
    name: 'KERNEL_SECURITY_CHECK_FAILURE',
    title: '内核检测到关键数据结构被破坏',
    causes: ['驱动越界写内存', '系统文件损坏', '内存故障'],
    actions: ['sfc /scannow', '卸载近期新增驱动', '内存诊断'],
    severity: 'high',
  },
  '0X000000C2': {
    name: 'BAD_POOL_CALLER',
    title: '驱动错误地使用内存池',
    causes: ['第三方驱动（安全软件、外设驱动）', '系统文件损坏'],
    actions: ['卸载最近安装的安全软件/外设驱动', 'sfc /scannow'],
    severity: 'medium',
  },
  '0X000000BE': {
    name: 'ATTEMPTED_WRITE_TO_READONLY_MEMORY',
    title: '驱动试图写只读内存',
    causes: ['驱动缺陷', '内存故障'],
    actions: ['更新驱动', '内存诊断'],
    severity: 'medium',
  },
  '0X00000109': {
    name: 'CRITICAL_STRUCTURE_CORRUPTION',
    title: '内核关键结构被修改',
    causes: ['驱动篡改内核', '恶意软件', '内存故障'],
    actions: ['全盘杀毒', '卸载来路不明的驱动', 'sfc /scannow'],
    severity: 'critical',
  },
  '0X0000007B': {
    name: 'INACCESSIBLE_BOOT_DEVICE',
    title: '启动时读不到系统盘',
    causes: ['BIOS 里硬盘模式被改（AHCI/IDE）', '存储控制器驱动丢失', '硬盘接触不良'],
    actions: ['进 BIOS 检查 SATA 模式', '检查硬盘线与供电', '用启动盘修复引导'],
    severity: 'critical',
  },
  '0X000000ED': {
    name: 'UNMOUNTABLE_BOOT_VOLUME',
    title: '系统卷无法挂载',
    causes: ['文件系统损坏', '硬盘坏道'],
    actions: ['chkdsk /f 修复系统盘', '备份数据并检查硬盘健康'],
    severity: 'critical',
  },
  '0X00000024': {
    name: 'NTFS_FILE_SYSTEM',
    title: 'NTFS 文件系统出错',
    causes: ['磁盘坏道', '存储驱动', '非正常关机'],
    actions: ['chkdsk /f', '检查硬盘 SMART'],
    severity: 'high',
  },
  '0X0000004E': {
    name: 'PFN_LIST_CORRUPT',
    title: '内存页帧数据库损坏',
    causes: ['内存条故障', '驱动写坏内存'],
    actions: ['内存诊断', '单条内存测试'],
    severity: 'high',
  },
  '0X000000EA': {
    name: 'THREAD_STUCK_IN_DEVICE_DRIVER',
    title: '某个设备驱动卡死在死循环',
    causes: ['显卡驱动', '外接设备驱动'],
    actions: ['更新显卡驱动', '拔掉非必要外设观察'],
    severity: 'medium',
  },
  '0X00000101': {
    name: 'CLOCK_WATCHDOG_TIMEOUT',
    title: '某个 CPU 核心没有响应',
    causes: ['CPU 过热/供电不稳', '主板或电源老化', '虚拟化冲突'],
    actions: ['清灰散热', '关闭超频/节能策略', '联系硬件维修'],
    severity: 'critical',
  },
};

/** `0xEF`、`0x000000ef`、`0x1000007E` 都归一到同一个键。 */
function normalize(raw) {
  const text = String(raw || '').trim().toUpperCase();
  const m = /0X([0-9A-F]+)/.exec(text);
  if (!m) return null;
  let hex = m[1];
  // 0x1000007E 这类是「带 1 的旧写法」，实际码是 0x7E
  if (hex.length === 9 && hex.startsWith('1')) hex = hex.slice(1);
  hex = hex.replace(/^0+/, '') || '0';
  return { key: `0X${hex.padStart(8, '0')}`, hex: `0x${hex.toUpperCase().padStart(8, '0')}` };
}

export function explain(raw) {
  const n = normalize(raw);
  if (!n) {
    return {
      known: false,
      code: String(raw || ''),
      name: '',
      title: '没有读到有效的错误码',
      causes: ['可能是采集时没拿到管理员权限，或事件日志被清理工具清空'],
      actions: ['确认这台机器装的是不是最新版客户端', '用「原始文件」里的 system.txt 看采集是否完整'],
      severity: 'unknown',
    };
  }
  const hit = CODES[n.key];
  if (!hit) {
    return {
      known: false,
      code: n.hex,
      name: '',
      title: '这个错误码不在常见词库里',
      causes: ['需要看转储文件才能定位到具体驱动'],
      actions: ['下载整包，用 WinDbg 打开 minidumps 分析', '先在应用日志里找出事前的异常'],
      severity: 'unknown',
    };
  }
  return { known: true, code: n.hex, ...hit };
}

/* ------------------------------------------------------------------ */
/* 线索                                                                */
/* ------------------------------------------------------------------ */

const LEVEL_ORDER = { critical: 0, warn: 1, info: 2 };

function finding(level, title, detail) {
  return { level, title, detail };
}

/**
 * 从报告的「事实」里提炼线索。
 *
 * 只写能验证的东西：某个环节失败了、没有管理员权限、没带转储。
 * 不写「可能是显卡驱动」这种猜的结论 —— 那种话应该在错误码词库里，
 * 由码本身决定。
 */
export function diagnose({ meta, summary, logs } = {}) {
  const out = [];
  const system = new Map((summary?.system || []).map((s) => [s.name, s.value]));

  if (summary?.collectError) {
    out.push(
      finding(
        'warn',
        '采集不完整',
        `客户端采集现场时报错：${summary.collectError}。常见原因是没拿到管理员权限，或磁盘写满。`
      )
    );
  }

  const repair = summary?.repair;
  if (repair) {
    if (repair.demo) {
      out.push(finding('info', '这是一份演示数据', '演示模式下不会执行任何修复动作。'));
    } else if (repair.admin === false) {
      out.push(
        finding(
          'warn',
          '修复没有执行',
          '当时进程没有管理员权限。检查安装时登记的计划任务是否还在：设置页「开机自检」应显示「已登记」。'
        )
      );
    }
    for (const step of repair.steps || []) {
      if (step.skipped) {
        out.push(finding('info', `${step.name}：被手动跳过`, step.summary || '运行中被点了「跳过此环节」。'));
      } else if (step.timedOut) {
        out.push(
          finding('warn', `${step.name}：超时`, `跑了 ${step.seconds} 秒仍未结束，已收尾。${step.summary || ''}`)
        );
      } else if (!step.ok) {
        out.push(
          finding(
            'warn',
            `${step.name}：未通过`,
            `${step.summary || '没有输出结论'}${step.exitCode != null ? `（退出码 ${step.exitCode}）` : ''}`
          )
        );
      }
    }
    if (repair.verdict) out.push(finding('info', '修复结论', repair.verdict));
  }

  const dumps = summary?.hasDumpFiles || [];
  if (dumps.length === 0) {
    out.push(
      finding(
        'info',
        '包里没有内核转储',
        '没有 .dmp 就没法定位到具体驱动文件；常见原因是系统里关掉了「小内存转储」或采集时没权限。'
      )
    );
  }

  const ram = /^(\d+)\s*MB$/i.exec(system.get('内存') || '');
  if (ram && Number(ram[1]) < 4096) {
    out.push(finding('info', '内存偏小', `这台机器只有 ${ram[1]} MB 内存，跑大课件时容易触发内存相关蓝屏。`));
  }

  if (meta?.mode === 'demo') {
    out.push(finding('info', '演示报告', '这台机器用 --crash-report-demo 产生，用来验证链路，不必当真去查。'));
  }

  if (logs && (logs.errorCount || logs.lastLine)) {
    const scan = logs.errors ? logs : scanLog(logs.text);
    if (scan.errors.length) {
      out.push(
        finding(
          'warn',
          `应用日志里有 ${scan.errorCount} 行错误`,
          scan.errors
            .slice(0, 3)
            .map((e) => `第 ${e.line} 行：${e.text}`)
            .join('\n')
        )
      );
    }
    if (scan.lastLine) {
      out.push(finding('info', '日志最后一行', scan.lastLine));
    }
  }

  out.sort((a, b) => LEVEL_ORDER[a.level] - LEVEL_ORDER[b.level]);
  return out;
}

/* ------------------------------------------------------------------ */
/* 日志扫描                                                            */
/* ------------------------------------------------------------------ */

const ERROR_RE = /\b(ERROR|FATAL|PANIC|panicked|CRITICAL)\b/;
const TS_RE = /^\s*\[?(\d{4}-\d{2}-\d{2}[ T][\d:.]+)/;

/**
 * 挑出日志里的错误行。
 *
 * 同一句话重复几十次很常见（比如每帧都失败），所以按「去掉时间戳后的文本」
 * 去重，只留第一次出现的位置 —— 后台要的是「出了什么错」，不是「错了几次」。
 */
export function scanLog(text) {
  const lines = String(text || '').split(/\r?\n/);
  const seen = new Set();
  const errors = [];
  let errorCount = 0;
  let lastLine = '';

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim()) lastLine = line.trim().slice(0, 300);
    if (!ERROR_RE.test(line)) continue;
    errorCount += 1;
    const key = line.replace(TS_RE, '').replace(/\s+/g, ' ').trim().slice(0, 200);
    if (seen.has(key)) continue;
    seen.add(key);
    if (errors.length < 20) errors.push({ line: i + 1, text: line.trim().slice(0, 300) });
  }

  return { errors, errorCount, lastLine };
}
