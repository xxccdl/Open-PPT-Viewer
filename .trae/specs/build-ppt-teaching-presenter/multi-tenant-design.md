# 多租户隔离设计：平台 → 学校 → 班级

> 目标：把现在「谁登录谁就能看到全部机器」的后台，改成「每个人只看自己班」。
> 每台电脑的 OpenPPTView 通过浏览器授权加入某个班级，蓝屏记录与远程控制都按班级隔离。

## 一、背景

现在 `crash.xxccdl.cn` 的权限只有两级：第一个注册的人是 admin，后来者是 viewer；
admin 能看到**全部**报告和**全部**远程机器。只要有一个账号，就能看别人学校、别人班的电脑，
能在那台机器上跑命令。这个模型在只有一两个用户时没问题，分发给多个学校之后就不成立了。

所以引入三层归属：**平台（超管）→ 学校 → 班级**，所有读写都按「这台机器属于哪个班」过滤。

## 二、非目标（本轮不做）

- 不做年级 / 系部层级；
- 不做邀请码（用户明确改为「浏览器登录 + 授权」）；
- 不做跨校转移班级（换校 = 在目标校建班 + 机器重新授权）；
- 不做审计日志页面（撤销动作只记在 `machines.json` 的字段里）；
- 不做账号找回 / 邮件验证（沿用现有用户名 + 密码）。

## 三、层级与角色

```
平台（platform 超管，可多个）
 └ 学校 school（由平台超管创建，可指定若干学校管理员）
    └ 班级 class（由学校管理员或平台超管创建，可指定若干班级管理员）
       └ 电脑 machine（在 app 里授权后绑定到某个 class）
```

| 角色 | 能看见 | 能做 |
| --- | --- | --- |
| **平台超管** `role=platform` | 全部学校、全部班级、**未分类**（`classId` 为空的历史记录） | 建学校、指定学校管理员、看/删任何记录、撤销任何机器令牌 |
| **学校管理员**（`school.admins`） | 本校所有班级的记录与机器 | 本校建班、指定班级管理员、撤销本校机器 |
| **班级管理员**（`class.admins`） | **只有本班**的记录与机器 | 审批加入申请、移除成员、撤销本班机器、远程控制本班机器、改记录状态 |
| **班级成员**（`members`） | 只有本班记录（只读） | 可授权本班机器（登录那台电脑）；看不到远程控制入口 |
| **未归属机器** | — | 只能上传报告（落「未分类」，仅平台超管可见），不启动远程心跳 |

注册后的普通账号 `role=user`，默认没有任何班级，登录后看到的是空列表 + 「申请加入班级」。

## 四、数据结构

全部落在 `STATE_DIR`（默认 `/var/lib/openpptview-crash/_state/`），沿用现有 JSON + 原子写（tmp → rename）。

### 4.1 `users.json`（扩展，字段向后兼容）

```json
[
  { "id": "…", "username": "…", "role": "platform|user", "salt": "…", "hash": "…",
    "createdAt": "…", "lastLoginAt": "…" }
]
```

迁移：老的 `role: "admin"` → `"platform"`；`"viewer"` → `"user"`。读取时在 `Auth` 内做一次归一，写回时落新值。

### 4.2 `org.json`（新）

```json
{
  "schools":  [{ "id": "…", "name": "…", "admins": ["userId"], "createdAt": "…" }],
  "classes":  [{ "id": "…", "schoolId": "…", "name": "…", "admins": ["userId"], "createdAt": "…" }],
  "members":  [{ "classId": "…", "userId": "…", "at": "…" }],
  "joinRequests": [{ "id": "…", "classId": "…", "userId": "…", "at": "…", "state": "pending|approved|rejected" }]
}
```

- 学校重名允许（不同学校可能同名），但同一学校内班级名不重复；
- 班级管理员必须在 `members` 里（`admins` 是成员的子集，由写入逻辑保证）；
- `joinRequests` 只保留最近 90 天的记录，`pending` 永远保留。

### 4.3 `machines.json`（新）

```json
{
  "machines": [
    { "tokenHash": "sha256(token) 的前 32 字节 hex", "hwid": "…", "name": "…",
      "classId": "…", "schoolId": "…", "authorizedBy": "userId",
      "authorizedAt": "…", "lastSeen": "…", "revokedAt": "" }
  ]
  ,
  "codes": [{ "codeHash": "…", "state": "…", "classId": "…", "userId": "…",
              "at": "…", "usedAt": "" }]
}
```

- **只存令牌哈希**：配置文件泄露也拿不到明文令牌；
- 同一 `hwid` 重新授权时，旧令牌条目直接 `revokedAt = now`（一台机器同时只有一枚有效令牌）；
- `codes` 里的授权码 60 秒过期、只能用一次，用过写 `usedAt`。

### 4.4 报告元数据（扩展）

`meta.json` 新增两个字段（老记录为空）：

```json
{ "classId": "…", "schoolId": "…" }
```

`classId` 为空 = 未分类，只对平台超管可见。

## 五、授权流程（本机回传）

```
设置页「登录」
   │
   ├─ app 起临时 HTTP 服务（127.0.0.1:随机端口），生成一次性 state
   ├─ 打开系统浏览器 → https://xxccdl.cn/crash/authorize?port=…&state=…
   │
   │   （浏览器）
   ├─ 未登录 → 登录 / 注册
   ├─ 登录后列出「我管理的班级」+「我加入的班级」，选中一个
   ├─ 点「授权这台电脑」→ 服务器 POST /api/v1/machine/authorize
   │     生成一次性 code（60s）→ 302 到 http://127.0.0.1:<port>/callback?code=…&state=…
   │
   ▼
app 接收回调：校验 state → POST /api/v1/machine/exchange {code, hwid, name, version}
   → 返回 { token, schoolName, className } → 写入 settings.json
   → 本地服务关闭，设置页显示「XX学校 · XX班」
```

超时 3 分钟未收到回调 → 关闭本地服务，设置页提示「未完成授权」。

### 5.1 安全要点

1. **state 必须一致**：本地端口是公开可写的，不校验 `state` 就等着别的网页往 `/callback` 塞自己的 code；
2. **code 一次性 + 60 秒**：用过即写 `usedAt`，过期即作废；
3. **授权页要求已登录会话**，且当前用户必须已经是该班成员（管理员或普通成员都可以授权本班机器）；
4. **令牌以 Bearer 走明文 HTTPS**，服务端只存哈希；
5. 撤销后令牌立即失效（心跳与上传都返回 401），客户端清令牌并提示重新登录。

## 六、服务端接口

### 6.1 新增

| 方法 | 路径 | 权限 | 作用 |
| --- | --- | --- | --- |
| GET | `/authorize` | 页面（未登录跳登录页） | 授权页：选班级 + 授权按钮 |
| POST | `/api/v1/machine/authorize` | 登录会话 | 生成 code，返回 `{ redirect }` |
| POST | `/api/v1/machine/exchange` | 无（凭 code） | code → 机器令牌 |
| POST | `/api/v1/machine/logout` | 机器令牌 | 撤销本机令牌 |
| GET/POST | `/api/v1/schools` | 平台超管建；学校管理员读自己的 | 学校列表 / 建校 |
| PATCH | `/api/v1/schools/:id` | 平台超管 | 改名、指定/移除学校管理员 |
| GET/POST | `/api/v1/classes` | 按可见范围 | 班级列表 / 建班（校管理员或平台超管） |
| PATCH | `/api/v1/classes/:id` | 校管理员 / 平台超管 | 改名、指定/移除班级管理员 |
| GET/POST | `/api/v1/classes/:id/members` | 班管理员 / 校管理员 / 平台超管 | 成员列表 / 移除成员 |
| POST | `/api/v1/classes/:id/join` | 普通登录用户 | 申请加入班级（写 joinRequests） |
| GET | `/api/v1/join-requests` | 班管理员及以上 | 待审批列表（按可见范围） |
| POST | `/api/v1/join-requests/:id` | 班管理员及以上 | `{ action: "approve"|"reject" }` |
| GET | `/api/v1/machines` | 按可见范围 | 已授权机器列表（含撤销） |
| POST | `/api/v1/machines/revoke` | 班管理员及以上 | `{ hwid }` |

### 6.2 现有接口改动

- `machineAuthorized(req)` 改为返回 `{ ok, classId, schoolId }`：认全局 `TOKEN`（运维用，视为未分类）或机器令牌；
- `POST /api/v1/crash`：从令牌里取 `classId` 写进 meta；令牌无效时**仍收**（落未分类），但响应里带 `warning: "machine-token-invalid"`，便于客户端提示重新登录；
- `GET /api/v1/crashes`、`/stats`、`/crashes/:id`（含 file/download/patch/delete）：按当前用户可见的 `classId` 集合过滤；平台超管额外可看未分类；
- 远程：`/api/v1/remote/machines` 只列出可见班级的机器；`cmd` / `screen` / `results` / `frame` 校验目标机器属于可见班级且用户是**班级管理员**。

> 注意两个容易混的接口：`/api/v1/remote/machines` 是**现在在线/正在心跳**的机器（内存态，20 秒没心跳就离线）；
> `GET /api/v1/machines` 是**授权记录**（落盘态，包含已撤销的），用来核对「这台机器当初是谁授权的」。

可见范围计算（`scopeOf(user)`）：

```
platform → { all: true }
学校管理员 → 本校所有 classId
班级管理员 / 成员 → 所在 classId
其它 → 空
```

## 七、客户端改动（`crates/ppt-app`）

- `crash::Settings` 新增：

```rust
pub struct MachineLink {
    pub token: String,
    pub school: String,
    pub class: String,
    pub linked_at: String,
}
// Settings 新增字段，serde(default)：老 settings.json 没有它照常解析
pub machine: Option<MachineLink>,
```

- 新模块 `machine_link.rs`：
  - `login()`：起本地 HTTP 服务（随机端口）+ 生成 state + 打开浏览器，返回 `{ ok, url }` 给前端；
  - `wait_callback()`：后台线程等回调（3 分钟超时），校验 state，换令牌并落盘；
  - `logout()`：调 `/machine/logout`，删本地令牌；
  - 纯函数（可测）：`authorize_url(endpoint, port, state)`、`callback_matches(query, state)`。
- 上传：有令牌就带 `Authorization: Bearer …`；
- 远程：`remote::spawn()` 前先检查有没有令牌；无令牌直接不启动；
- 设置页新增「班级」行：未登录 = 「登录」；已登录 = 「XX学校 · XX班」+「退出」；
- 收到 401（令牌已撤销）→ 清本地令牌 + toast「这台电脑的授权已被撤销，请重新登录」。

## 八、后台页面改动

- 导航：概览 / 报告 / 远程 / **学校班级**（按角色显示）/ 成员 / 账号；
- 报告列表：新增「班级」列与筛选；平台超管多一个「未分类」筛选项；
- 远程页：只显示可见班级的机器，列表加「班级」列；
- 学校班级页：
  - 平台超管：建学校、指定学校管理员、建班、看全部班级；
  - 学校管理员：建班、指定班级管理员、查看本校成员；
  - 班级管理员：审批加入申请、移除成员、撤销本班机器；
- 顶部身份提示：`平台超管` / `学校管理员 · XX学校` / `班级管理员 · XX班`。

## 九、迁移与兼容

1. `Auth` 读取 `users.json` 时归一 `role`，写回时落新值（幂等，可重复执行）；
2. 现有 7 份报告没有 `classId` → 自动成为未分类（不写盘，读时为空）；
3. 现有后台会话不受影响（会话里存的是 userId，不存角色）；
4. 现有机器（如果有）没有令牌 → 下次心跳被拒，重新在 app 里登录即可；
5. 部署顺序：先发服务端（不改变旧接口行为，未登录仍可上传）→ 再发客户端 0.3.14。

## 十、测试计划

**服务端冒烟（node 脚本，临时 DATA_DIR）**

- A 班管理员的列表里看不到 B 班记录；
- 平台超管能看到全部 + 未分类；
- 学校管理员能看到本校两个班，看不到别校；
- code 换一次令牌后第二次 exchange 失败；60 秒后失败；
- 撤销令牌后上传被拒/落未分类，远程接口 401；
- 非成员不能授权某班机器。

**Rust 单元测试**

- `authorize_url` 拼装、`callback_matches` 对 state 的严格比较；
- 老 `settings.json`（无 `machine` 字段）能解析；
- 令牌为 None 时不上传 Bearer 头。

**真机**

- 本机装 0.3.14 → 设置页点登录 → 浏览器登录 → 授权 → app 显示班级 → 上传一条演示报告 → 后台该班能看到、另一个班看不到。

## 十一、实施分期

1. **服务端**：`org.mjs` + `machines.json` + 令牌与可见范围过滤 + 新接口（可先不接客户端）；
2. **客户端**：登录/授权/令牌持久化 + 上传与远程带上令牌 + 设置页那一行；
3. **后台**：学校班级页 + 报告/远程的班级列与筛选 + 身份提示；
4. **部署与迁移**：上线服务端 → 发 0.3.14 → 真机验证 → 老数据确认只剩超管可见。

## 十二、取舍说明

- **为什么不做邀请码**：用户明确希望「在浏览器登录授权」，比抄码少一步，也天然要求对方已经是班级成员；
- **为什么令牌按机器而不是按班**：一台机器泄露不影响同班其它机器，换班可精确作废旧令牌；
- **为什么未登录仍允许上传**：装机时常常还没登录就蓝屏了，此时把报告留在「未分类」总比丢掉强，之后可手工指派；
- **为什么成员不能远程**：远程控制能在这台机器上执行任意命令，这一条按班级管理员收口；
- **为什么不删学校**：学校删除牵扯班级与历史报告归属，查询与迁移成本大于收益（YAGNI）。
