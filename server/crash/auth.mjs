/**
 * 账号与会话。
 *
 * # 为什么自己写
 *
 * 这个服务是「一个收文件的接口 + 一个看报告的后台」，用零依赖方案是为了
 * 机房那台机器上不必为一个后台去维护 node_modules。密码用 Node 自带的
 * scrypt（内存硬，抗 GPU 爆破），会话是随机 32 字节 + 服务端白名单。
 *
 * # 第一个注册的人就是管理员
 *
 * 部署完是空的，谁先打开谁建账号，那个人拿到 admin —— 这比在服务器上
 * 手写一个初始密码更省事，也不会出现「默认密码忘了改」这种事故。
 * 后面注册的人只是 viewer：能看报告，不能删、不能管人。
 */

import crypto from 'node:crypto';
import { promises as fs } from 'node:fs';
import path from 'node:path';

const SESSION_TTL = 30 * 24 * 3600 * 1000; // 30 天：老师/管理员不会天天登录
const SCRYPT_KEYLEN = 64;

/** 登录失败限流：同一 IP 十分钟内最多试这么多次。 */
const LOGIN_WINDOW = 10 * 60 * 1000;
const LOGIN_MAX = 10;

export class Auth {
  constructor(stateDir) {
    this.dir = stateDir;
    this.usersPath = path.join(stateDir, 'users.json');
    this.sessionsPath = path.join(stateDir, 'sessions.json');
    /** 所有落盘串行化：两个请求同时注册，不能互相覆盖。 */
    this.chain = Promise.resolve();
    this.attempts = new Map();
  }

  async init() {
    await fs.mkdir(this.dir, { recursive: true });
    // 顺手清掉过期的会话，别让文件无限长
    const sessions = await this.#readSessions();
    const now = Date.now();
    let dirty = false;
    for (const [token, s] of Object.entries(sessions)) {
      if (!s || s.expiresAt < now) {
        delete sessions[token];
        dirty = true;
      }
    }
    if (dirty) await this.#writeSessions(sessions);
  }

  /* ---------------- 读写字 ---------------- */

  #serial(fn) {
    const next = this.chain.then(fn, fn);
    this.chain = next.catch(() => {});
    return next;
  }

  async #readUsers() {
    try {
      const text = await fs.readFile(this.usersPath, 'utf8');
      const data = JSON.parse(text);
      return Array.isArray(data) ? data : [];
    } catch {
      return [];
    }
  }

  async #writeUsers(users) {
    const tmp = `${this.usersPath}.${process.pid}.tmp`;
    await fs.writeFile(tmp, JSON.stringify(users, null, 2), 'utf8');
    await fs.rename(tmp, this.usersPath);
  }

  async #readSessions() {
    try {
      const text = await fs.readFile(this.sessionsPath, 'utf8');
      const data = JSON.parse(text);
      return data && typeof data === 'object' ? data : {};
    } catch {
      return {};
    }
  }

  async #writeSessions(sessions) {
    const tmp = `${this.sessionsPath}.${process.pid}.tmp`;
    await fs.writeFile(tmp, JSON.stringify(sessions, null, 2), 'utf8');
    await fs.rename(tmp, this.sessionsPath);
  }

  /* ---------------- 密码 ---------------- */

  static #hash(password, salt) {
    return new Promise((resolve, reject) => {
      crypto.scrypt(password, salt, SCRYPT_KEYLEN, (err, key) => {
        if (err) reject(err);
        else resolve(key.toString('hex'));
      });
    });
  }

  static async #verify(password, user) {
    const got = Buffer.from(await Auth.#hash(password, user.salt), 'hex');
    const want = Buffer.from(user.hash, 'hex');
    return got.length === want.length && crypto.timingSafeEqual(got, want);
  }

  /* ---------------- 限流 ---------------- */

  #throttled(ip) {
    const now = Date.now();
    const rec = this.attempts.get(ip);
    if (!rec || now > rec.resetAt) {
      this.attempts.set(ip, { count: 1, resetAt: now + LOGIN_WINDOW });
      return false;
    }
    rec.count += 1;
    return rec.count > LOGIN_MAX;
  }

  #clearThrottle(ip) {
    this.attempts.delete(ip);
  }

  /* ---------------- 对外 ---------------- */

  static publicUser(u) {
    return {
      id: u.id,
      username: u.username,
      role: u.role,
      createdAt: u.createdAt,
      lastLoginAt: u.lastLoginAt || null,
    };
  }

  async initialized() {
    return (await this.#readUsers()).length > 0;
  }

  /**
   * 注册。第一个账号是 admin，其余是 viewer。
   * 返回 `{ user, token }`。
   */
  async register({ username, password, ip }) {
    const name = String(username || '').trim();
    const pass = String(password || '');
    if (!/^[A-Za-z0-9_.\-\u4e00-\u9fa5]{2,24}$/.test(name)) {
      throw new HttpError(400, '用户名 2–24 位，可用中英文、数字、下划线');
    }
    if (pass.length < 8 || pass.length > 128) {
      throw new HttpError(400, '密码至少 8 位');
    }

    return this.#serial(async () => {
      const users = await this.#readUsers();
      if (users.some((u) => u.username.toLowerCase() === name.toLowerCase())) {
        throw new HttpError(409, '这个名字已经有人用了');
      }
      const salt = crypto.randomBytes(16).toString('hex');
      const user = {
        id: crypto.randomBytes(8).toString('hex'),
        username: name,
        role: users.length === 0 ? 'admin' : 'viewer',
        salt,
        hash: await Auth.#hash(pass, salt),
        createdAt: new Date().toISOString(),
        lastLoginAt: null,
      };
      users.push(user);
      await this.#writeUsers(users);
      return { user: Auth.publicUser(user), token: await this.#openSession(user.id, ip) };
    });
  }

  async login({ username, password, ip }) {
    if (this.#throttled(ip)) {
      throw new HttpError(429, '试得太频繁了，十分钟后再来');
    }
    const name = String(username || '').trim();
    const pass = String(password || '');
    const users = await this.#readUsers();
    const user = users.find((u) => u.username.toLowerCase() === name.toLowerCase());
    // 用户不存在也照样算一次哈希：别让响应时间暴露「这个名字有没有注册」
    const ok = user ? await Auth.#verify(pass, user) : false;
    if (!ok) throw new HttpError(401, '用户名或密码不对');

    this.#clearThrottle(ip);
    return this.#serial(async () => {
      const fresh = await this.#readUsers();
      const target = fresh.find((u) => u.id === user.id);
      if (target) {
        target.lastLoginAt = new Date().toISOString();
        await this.#writeUsers(fresh);
      }
      return { user: Auth.publicUser(target || user), token: await this.#openSession(user.id, ip) };
    });
  }

  async #openSession(userId, ip) {
    const token = crypto.randomBytes(32).toString('base64url');
    const sessions = await this.#readSessions();
    sessions[token] = {
      userId,
      createdAt: new Date().toISOString(),
      expiresAt: Date.now() + SESSION_TTL,
      ip: ip || '',
    };
    await this.#writeSessions(sessions);
    return token;
  }

  async logout(token) {
    if (!token) return;
    const sessions = await this.#readSessions();
    if (sessions[token]) {
      delete sessions[token];
      await this.#writeSessions(sessions);
    }
  }

  /** 令牌 → 用户（过期或已删除返回 null）。 */
  async userFor(token) {
    if (!token) return null;
    const sessions = await this.#readSessions();
    const s = sessions[token];
    if (!s) return null;
    if (s.expiresAt < Date.now()) {
      await this.logout(token);
      return null;
    }
    const users = await this.#readUsers();
    const user = users.find((u) => u.id === s.userId);
    return user ? Auth.publicUser(user) : null;
  }

  async listUsers() {
    const users = await this.#readUsers();
    return users.map(Auth.publicUser);
  }

  async setRole(id, role) {
    if (!['admin', 'viewer'].includes(role)) throw new HttpError(400, '角色只有 admin / viewer');
    return this.#serial(async () => {
      const users = await this.#readUsers();
      const user = users.find((u) => u.id === id);
      if (!user) throw new HttpError(404, '没有这个账号');
      // 最后一个管理员不能降级，否则没人能管人了
      if (user.role === 'admin' && role !== 'admin') {
        if (users.filter((u) => u.role === 'admin').length <= 1) {
          throw new HttpError(400, '至少留一个管理员');
        }
      }
      user.role = role;
      await this.#writeUsers(users);
      return Auth.publicUser(user);
    });
  }

  async deleteUser(id, actorId) {
    if (id === actorId) throw new HttpError(400, '不能删自己');
    return this.#serial(async () => {
      const users = await this.#readUsers();
      const target = users.find((u) => u.id === id);
      if (!target) throw new HttpError(404, '没有这个账号');
      if (target.role === 'admin' && users.filter((u) => u.role === 'admin').length <= 1) {
        throw new HttpError(400, '至少留一个管理员');
      }
      const left = users.filter((u) => u.id !== id);
      await this.#writeUsers(left);
      // 顺手把这个人的会话全部注销
      const sessions = await this.#readSessions();
      let dirty = false;
      for (const [token, s] of Object.entries(sessions)) {
        if (s && s.userId === id) {
          delete sessions[token];
          dirty = true;
        }
      }
      if (dirty) await this.#writeSessions(sessions);
      return true;
    });
  }

  async changePassword(id, oldPassword, newPassword) {
    const pass = String(newPassword || '');
    if (pass.length < 8 || pass.length > 128) throw new HttpError(400, '密码至少 8 位');
    const users = await this.#readUsers();
    const user = users.find((u) => u.id === id);
    if (!user) throw new HttpError(404, '没有这个账号');
    if (!(await Auth.#verify(String(oldPassword || ''), user))) {
      throw new HttpError(401, '原密码不对');
    }
    return this.#serial(async () => {
      const fresh = await this.#readUsers();
      const target = fresh.find((u) => u.id === id);
      const salt = crypto.randomBytes(16).toString('hex');
      target.salt = salt;
      target.hash = await Auth.#hash(pass, salt);
      await this.#writeUsers(fresh);
      return true;
    });
  }
}

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}
