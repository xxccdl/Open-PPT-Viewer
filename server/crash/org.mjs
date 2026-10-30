/**
 * 学校 / 班级 / 成员 / 机器授权。
 *
 * 这里解决的问题一句话：**谁该看到哪些电脑**。
 * 报告和远程控制原本是全局的（谁登录谁全看得见），分发到多个学校之后，
 * 一个账号能看到别人班的电脑、还能在它上面跑命令 —— 这显然不行。
 *
 * 三层归属：平台（超管）→ 学校 → 班级 → 机器。
 * 所有数据都在 `STATE_DIR` 的两个 JSON 里（`org.json`、`machines.json`），
 * 零依赖、原子写（先写 `.tmp` 再 rename），与 `auth.mjs` 的做法一致。
 *
 * # 机器令牌
 *
 * 每台电脑授权后拿一枚长期令牌；**落盘的只有它的 SHA-256**。
 * 配置文件被拿走也换不回明文，撤销时直接标记 `revokedAt`。
 * 同一台 `hwid` 重新授权会自动作废旧令牌 —— 换班不留后门。
 */

import crypto from 'node:crypto';
import { promises as fs } from 'node:fs';
import path from 'node:path';

/** 授权码活多久（毫秒）。 */
const CODE_TTL = 60 * 1000;
/** 已处理的加入申请保留多久。 */
const REQUEST_KEEP = 90 * 24 * 3600 * 1000;

function bad(status, message) {
  return Object.assign(new Error(message), { status });
}

function now() {
  return new Date().toISOString();
}

function newId() {
  return crypto.randomBytes(6).toString('hex');
}

function sha256(text) {
  return crypto.createHash('sha256').update(text).digest('hex');
}

export class Org {
  constructor(stateDir) {
    this.dir = stateDir;
    this.orgPath = path.join(stateDir, 'org.json');
    this.machinesPath = path.join(stateDir, 'machines.json');
    /** 所有落盘串行化：两条请求同时建班，不能互相覆盖。 */
    this.chain = Promise.resolve();
    this.data = { schools: [], classes: [], members: [], joinRequests: [] };
    this.machines = { machines: [], codes: [] };
  }

  async init() {
    await fs.mkdir(this.dir, { recursive: true });
    const org = await this.#read(this.orgPath, null);
    if (org) this.data = { ...this.data, ...org };
    const machines = await this.#read(this.machinesPath, null);
    if (machines) this.machines = { ...this.machines, ...machines };

    // 过期的授权码、太久以前的申请，开机清一次
    const cutoff = Date.now() - CODE_TTL * 5;
    this.machines.codes = this.machines.codes.filter((c) => new Date(c.at).getTime() > cutoff);
    const keep = Date.now() - REQUEST_KEEP;
    this.data.joinRequests = this.data.joinRequests.filter(
      (r) => r.state === 'pending' || new Date(r.at).getTime() > keep
    );
    await this.#persist();
  }

  /* ---------------- 读写 ---------------- */

  #serial(fn) {
    const next = this.chain.then(fn, fn);
    this.chain = next.catch(() => {});
    return next;
  }

  async #read(file, fallback) {
    try {
      const text = await fs.readFile(file, 'utf8');
      const data = JSON.parse(text);
      return Array.isArray(data) ? { items: data } : data;
    } catch {
      return fallback;
    }
  }

  async #write(file, body) {
    const tmp = `${file}.${process.pid}.tmp`;
    await fs.writeFile(tmp, JSON.stringify(body, null, 2), 'utf8');
    await fs.rename(tmp, file);
  }

  async #persist() {
    await this.#write(this.orgPath, this.data);
    await this.#write(this.machinesPath, this.machines);
  }

  /* ---------------- 查询 ---------------- */

  schools() {
    return this.data.schools.map((s) => ({ ...s }));
  }

  school(id) {
    return this.data.schools.find((s) => s.id === id) || null;
  }

  classes() {
    return this.data.classes.map((c) => this.#classView(c));
  }

  /** 班级 + 学校名 + 成员数，后台表格直接用。 */
  #classView(c) {
    const school = this.school(c.schoolId);
    const members = this.data.members.filter((m) => m.classId === c.id);
    return {
      ...c,
      schoolName: school ? school.name : '（学校已不存在）',
      memberCount: members.length,
      machineCount: this.machines.machines.filter((m) => m.classId === c.id && !m.revokedAt).length,
    };
  }

  class(id) {
    const c = this.data.classes.find((x) => x.id === id);
    return c ? this.#classView(c) : null;
  }

  membersOf(classId) {
    return this.data.members.filter((m) => m.classId === classId).map((m) => ({ ...m }));
  }

  joinRequests(filter = {}) {
    return this.data.joinRequests
      .filter((r) => (filter.pendingOnly ? r.state === 'pending' : true))
      .filter((r) => (filter.classIds ? filter.classIds.includes(r.classId) : true))
      .map((r) => ({ ...r }));
  }

  machinesList() {
    return this.machines.machines.map(({ tokenHash, ...rest }) => ({ ...rest }));
  }

  /* ---------------- 权限 ---------------- */

  /**
   * 这个账号能看见哪些班。
   *
   * - 平台超管：`all`
   * - 学校管理员：本校所有班
   * - 班级管理员 / 成员：自己所在（或管理）的班
   * - 其它：一个都没有
   */
  scopeOf(user) {
    if (!user) return { all: false, classIds: [] };
    if (user.role === 'platform') return { all: true, classIds: [] };

    const mySchools = new Set(
      this.data.schools.filter((s) => s.admins.includes(user.id)).map((s) => s.id)
    );
    const ids = new Set();
    for (const c of this.data.classes) {
      if (mySchools.has(c.schoolId) || c.admins.includes(user.id)) ids.add(c.id);
    }
    for (const m of this.data.members) {
      if (m.userId === user.id) ids.add(m.classId);
    }
    return { all: false, classIds: [...ids] };
  }

  /** 是不是这个班的成员（管理员也算）。 */
  isMember(userId, classId) {
    if (!userId || !classId) return false;
    return this.data.members.some((m) => m.classId === classId && m.userId === userId);
  }

  /** 能不能管这个班（改设置、管成员、删记录）。 */
  canManageClass(user, classId) {
    if (!user) return false;
    if (user.role === 'platform') return true;
    const c = this.data.classes.find((x) => x.id === classId);
    if (!c) return false;
    if (c.admins.includes(user.id)) return true;
    const s = this.school(c.schoolId);
    return Boolean(s && s.admins.includes(user.id));
  }

  /** 能不能远程控制这个班的机器（比管班更窄：只给班级管理员）。 */
  canRemoteClass(user, classId) {
    if (!user) return false;
    if (user.role === 'platform') return true;
    const c = this.data.classes.find((x) => x.id === classId);
    return Boolean(c && c.admins.includes(user.id));
  }

  /** 这个账号是不是某个学校的学校管理员。 */
  isSchoolAdmin(user, schoolId) {
    if (!user) return false;
    if (user.role === 'platform') return true;
    const s = this.school(schoolId);
    return Boolean(s && s.admins.includes(user.id));
  }

  /* ---------------- 学校 ---------------- */

  createSchool(name) {
    const n = String(name || '').trim();
    if (!n) throw bad(400, '学校名不能为空');
    if (n.length > 40) throw bad(400, '学校名最多 40 个字');
    return this.#serial(async () => {
      if (this.data.schools.some((s) => s.name === n)) throw bad(409, '这个学校已经有了');
      const school = { id: newId(), name: n, admins: [], createdAt: now() };
      this.data.schools.push(school);
      await this.#persist();
      return school;
    });
  }

  setSchoolAdmins(schoolId, admins) {
    return this.#serial(async () => {
      const s = this.school(schoolId);
      if (!s) throw bad(404, '没有这个学校');
      s.admins = [...new Set((admins || []).filter((id) => typeof id === 'string'))];
      await this.#persist();
      return s;
    });
  }

  /* ---------------- 班级 ---------------- */

  createClass(schoolId, name, firstAdminId) {
    const n = String(name || '').trim();
    if (!n) throw bad(400, '班级名不能为空');
    if (n.length > 40) throw bad(400, '班级名最多 40 个字');
    return this.#serial(async () => {
      const school = this.school(schoolId);
      if (!school) throw bad(404, '没有这个学校');
      if (this.data.classes.some((c) => c.schoolId === schoolId && c.name === n)) {
        throw bad(409, '这个学校下已经有同名班级');
      }
      const klass = {
        id: newId(),
        schoolId,
        name: n,
        admins: firstAdminId ? [firstAdminId] : [],
        createdAt: now(),
      };
      this.data.classes.push(klass);
      for (const uid of klass.admins) this.#ensureMember(klass.id, uid);
      await this.#persist();
      return this.#classView(klass);
    });
  }

  setClassAdmins(classId, admins) {
    return this.#serial(async () => {
      const c = this.data.classes.find((x) => x.id === classId);
      if (!c) throw bad(404, '没有这个班级');
      c.admins = [...new Set((admins || []).filter((id) => typeof id === 'string'))];
      // 管理员必须是成员：不然「管这个班」和「在这个班里」会打架
      for (const uid of c.admins) this.#ensureMember(c.id, uid);
      await this.#persist();
      return this.#classView(c);
    });
  }

  #ensureMember(classId, userId) {
    if (this.data.members.some((m) => m.classId === classId && m.userId === userId)) return;
    this.data.members.push({ classId, userId, at: now() });
  }

  addMember(classId, userId) {
    return this.#serial(async () => {
      const c = this.data.classes.find((x) => x.id === classId);
      if (!c) throw bad(404, '没有这个班级');
      this.#ensureMember(classId, userId);
      await this.#persist();
      return this.membersOf(classId);
    });
  }

  removeMember(classId, userId) {
    return this.#serial(async () => {
      const c = this.data.classes.find((x) => x.id === classId);
      if (!c) throw bad(404, '没有这个班级');
      this.data.members = this.data.members.filter(
        (m) => !(m.classId === classId && m.userId === userId)
      );
      c.admins = c.admins.filter((id) => id !== userId);
      await this.#persist();
      return this.membersOf(classId);
    });
  }

  /* ---------------- 加入申请 ---------------- */

  requestJoin(classId, userId) {
    return this.#serial(async () => {
      const c = this.data.classes.find((x) => x.id === classId);
      if (!c) throw bad(404, '没有这个班级');
      if (this.data.members.some((m) => m.classId === classId && m.userId === userId)) {
        throw bad(409, '你已经是这个班的成员了');
      }
      const pending = this.data.joinRequests.find(
        (r) => r.classId === classId && r.userId === userId && r.state === 'pending'
      );
      if (pending) return pending;
      const req = { id: newId(), classId, userId, at: now(), state: 'pending' };
      this.data.joinRequests.push(req);
      await this.#persist();
      return req;
    });
  }

  decideJoin(requestId, approve) {
    return this.#serial(async () => {
      const req = this.data.joinRequests.find((r) => r.id === requestId);
      if (!req) throw bad(404, '没有这条申请');
      if (req.state !== 'pending') throw bad(409, '这条申请已经处理过了');
      req.state = approve ? 'approved' : 'rejected';
      req.decidedAt = now();
      if (approve) this.#ensureMember(req.classId, req.userId);
      await this.#persist();
      return { ...req };
    });
  }

  /* ---------------- 机器令牌 ---------------- */

  /** 浏览器点了「授权这台电脑」：发一枚 60 秒的一次性授权码。 */
  issueCode({ state, classId, userId }) {
    return this.#serial(async () => {
      const c = this.data.classes.find((x) => x.id === classId);
      if (!c) throw bad(404, '没有这个班级');
      const code = crypto.randomBytes(8).toString('hex');
      this.machines.codes.push({
        codeHash: sha256(code),
        state: String(state || ''),
        classId,
        userId,
        at: now(),
        usedAt: '',
      });
      // 只留最近的一批，别让文件无限长
      this.machines.codes = this.machines.codes.slice(-200);
      await this.#persist();
      return code;
    });
  }

  /**
   * 授权码 → 机器令牌。
   *
   * 同一台 `hwid` 重新授权时，旧令牌立刻作废：换班之后旧后台看不到它。
   */
  exchange(code, hwid, meta = {}) {
    return this.#serial(async () => {
      const hash = sha256(String(code || ''));
      const rec = this.machines.codes.find((c) => c.codeHash === hash);
      if (!rec) throw bad(400, '授权码无效');
      if (rec.usedAt) throw bad(409, '这个授权码已经用过了');
      if (Date.now() - new Date(rec.at).getTime() > CODE_TTL) throw bad(410, '授权码已过期，请重新授权');

      const klass = this.data.classes.find((x) => x.id === rec.classId);
      if (!klass) throw bad(404, '这个班级已经不存在了');
      rec.usedAt = now();

      for (const m of this.machines.machines) {
        if (m.hwid === hwid && !m.revokedAt) m.revokedAt = now();
      }

      const token = crypto.randomBytes(32).toString('base64url');
      this.machines.machines.push({
        tokenHash: sha256(token),
        hwid: String(hwid || ''),
        name: String(meta.name || '').slice(0, 64),
        version: String(meta.version || '').slice(0, 32),
        classId: klass.id,
        schoolId: klass.schoolId,
        authorizedBy: rec.userId,
        authorizedAt: now(),
        lastSeen: now(),
        revokedAt: '',
      });
      await this.#persist();
      const school = this.school(klass.schoolId);
      return {
        token,
        classId: klass.id,
        schoolId: klass.schoolId,
        className: klass.name,
        schoolName: school ? school.name : '',
      };
    });
  }

  /** 机器令牌 → 归属；失效（撤销 / 不存在）返回 null。 */
  resolveToken(token) {
    if (!token) return null;
    const hash = sha256(String(token));
    const m = this.machines.machines.find((x) => x.tokenHash === hash);
    if (!m || m.revokedAt) return null;
    const c = this.data.classes.find((x) => x.id === m.classId);
    if (!c) return null;
    return { hwid: m.hwid, classId: m.classId, schoolId: m.schoolId };
  }

  touchMachine(hwid) {
    const m = this.machines.machines.find((x) => x.hwid === hwid && !x.revokedAt);
    if (!m) return;
    const t = now();
    // 每 10 分钟写一次盘就够了：心跳是 3 秒一次，不能每次都落盘
    if (t.slice(0, 15) === String(m.lastSeen).slice(0, 15)) return;
    m.lastSeen = t;
    this.#persist().catch(() => {});
  }

  revokeMachine(hwid) {
    return this.#serial(async () => {
      let hit = 0;
      for (const m of this.machines.machines) {
        if (m.hwid === hwid && !m.revokedAt) {
          m.revokedAt = now();
          hit += 1;
        }
      }
      if (!hit) throw bad(404, '没有这台机器的有效授权');
      await this.#persist();
      return hit;
    });
  }

  /** 后台里的「这台机器归谁」：状态、班级、撤销记录。 */
  machine(hwid) {
    const m = this.machines.machines
      .filter((x) => x.hwid === hwid)
      .sort((a, b) => String(b.authorizedAt).localeCompare(String(a.authorizedAt)))[0];
    if (!m) return null;
    const { tokenHash, ...rest } = m;
    return { ...rest, active: !m.revokedAt };
  }
}
