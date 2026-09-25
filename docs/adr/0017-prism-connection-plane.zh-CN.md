# 0017 — Prism 连接面：双重身份、单一连接集合与统一 event 协议

> **Languages:** [English](0017-prism-connection-plane.md) (primary) · [中文](0017-prism-connection-plane.zh-CN.md)

**Status:** Accepted (2026-09-22) — design; implementation pending, see Consequences

## Context

Aura 的集成链路缺少客户端入口。Gravity/krystallizer actor 已存在，远程 probe 也已拨入（Phase 3），但测试客户端、浏览器或被投递的应用没有一个统一的连接面。PLAN Phase 8 已将 Prism 定为以 Aura-resident component 形态托管的 WS 网关（"client connections pin here, not on Gravity; turn delivery = realm events"），ADR-0015 的节点审批端点也明确推迟到"Prism 的 auth 存在之后"。

这个设计服务的对象不止 chat。Fluxen 应用（参见 fluxora 的 Envelope 模型——`receiver: Vec<Session>` 包裹 `sender`/`created`/`content` 消息体）是任意应用的构造模式：chat、CMS、看板、商城，可以自由组合。因此连接面不能内建"每条连接都属于一个用户"之类的 chat 假设。

承接先前裁决的约束：

- ADR-0015：节点身份是每 probe 节点一把 ed25519 key；审批通过四个 curl 端点按公钥进行，这四个端点"将来挂在账户（Prism auth）之下"。
- okm key 纪律：`Key` 字段拒绝 `String`（仅定宽）；开放名字是运行时数据，通过 registry 表（`#[kv_index(by_name)]`）解析，不手搓字节 key。
- probe 不持存储、不持数据面凭证（2026-09-20 撤回裁决）——Prism 是引擎所在宿主（Aura 节点本身），可以持有账户注册表。
- long-polling 降级传输已作为架构原则被拒绝；传输契约是 WS-only。

## Decision

### 1. Prism 是连接面，以 Aura-resident component 形态托管

aura 侧新增连接面 crate，持有：WS accept/upgrade、路由分发（`/probe/<alias>`、`/admin/...`、应用路由、`/assets/...`）、身份模型与 event 协议编解码。协议定义按 PLAN Phase 8 归 Prism 侧所有，但连接面是 Aura 代码，回合投递走 realm events。Gravity、krystallizer 与被投递的 fluxen 应用都是*经由* Prism 到达的 actor，不接进 Prism。

### 2. 双重身份：device 与 account；认证是业务层决策

框架只承认两种身份：

- **设备身份**——首次连接时服务端分配 `device_id`，客户端持久化在 localstorage。所有 event 默认以设备身份作为 sender。看板、浏览、加购物车乃至 chat 本身都在无账户的情况下工作。
- **账户身份**——`login` event（用户名+密码）将 `device_id` 绑定到 `user_id`。绑定后该连接的 sender 是 `user_id`；设备绑定跨重连保留（重新登录即恢复）。

localstorage 中的 `device_id` 是框架维护的锚点：框架的职责只有 device↔user 这条绑定记录。某个 event 是否需要账户是业务决策——每个 actor 类型按 event 声明是否需要已认证的 sender；框架只保证"当前 sender 是 device 还是 user"可查询，并在"要求认证的 event 遇到匿名 sender"时返回标准错误。商城在"下单"上声明，chat 可以永不声明。

未认证连接不做 idle 踢出：公共服务本就合理地活在匿名连接上。

### 3. 连接集合：一个集合，认证状态是连接的字段

网关持有单个连接集合。每条连接自带认证状态字段：成功 `login` 前为匿名（`None`），之后绑定到 `user_id`。群发（向全体、向已认证、或向声明的子集）遍历这个单集合并按该字段筛选——筛选是每次扇出求值一次的谓词，不是一份需要同步维护的第二个集合。

早期裁决（两个集合、登录时物理移动连接）在此推翻：两个集合同一件事存两份，而每一次状态变化——今天是 login，将来是 logout 或服务端撤销——都必须同时写两处，否则两集合漂移。单集合加 per-connection 字段让漂移无法表达，而两个集合省下的群发遍历在连接量级上并非热路径。

actor 可声明是否要求认证。身份经投递载荷到达 handler（§7 下 2026-09-22 修正案）：prism 把 sender 信封包进 event args；`Ctx`、`Job`、`InstanceId` 不带身份字段。身份同样刻意不作为存储分区 key：分区可按 `channel_id` 或任何业务维度（partitioning.md §2.1）——地址回答「谁串行处理」，信封回答「谁发起」。

### 4. 单一 event 协议，一个字段，JSON 与 CBOR

线上双向只携带一个字段——`ev`；协议不编码方向。事件就是事件：客户端的 `{"ev": "order.submit", ...}` 与服务端的 `{"ev": "order.created", ...}` 是同一种形状。`emit`/`on` 是各端实现细节：aura 侧是 actor 的 `@on` 声明与 `emit` 调用；客户端侧是 `ws.send` / `ws.on`。Prism 是 aura 的 event 语义到用户端的自然延伸——不存在「客户端 action / 服务端 event」的词汇分叉需要翻译（aura ADR-0026 命名节）——「action」一词一并废弃。早期草稿的裁决（server→client 帧以自己的字段区分、不复用 client 侧字段）被更强地满足：一个字段，方向在协议层不存在——分发无从依它分支。

业务操作（login、视图操作、下单）就是客户端 emit 一个业务命名的 event——「action」一词废弃：客户端的 action 本来就是 emit 出去的 event，统一词汇把这层关系说得更精确。两种编码：CBOR（默认）与 JSON（调试）。选择发生在握手时通过查询参数（`?protocol=json`）；编解码器在连接生命周期内固定。

WS 常连接；`login` 是其上的普通 event，不是独立的 HTTP 往返。

### 5. admin 前缀与 actor 上传

`/admin` 之下：`POST` 上传/注册 actor 代码，加上 ADR-0015 的四个节点审批端点（`POST /nodes`、`GET /nodes`、`POST /nodes/{alias}/approve`、`DELETE /nodes/{alias}`），现在挂在账户之下。krystallizer 与 gravity 各自拿到一个薄上传脚本（拼 JSON、POST）——第一个脚本放在本仓 `scripts/` 验证端点，然后复制到各仓库。

### 6. 远程 probe 入口 `/probe/<alias>`

现有 probe 拨入网关（register / Call / Result / Host 帧）挂载到 `/probe/<alias>`。语义不变：probe 拨出，realm 按 alias 键控 `probes`，residency 身份以 `session = "<actor_type>/<key>"` 传递。

### 7. okm 中的账户注册表，id 优先访问

专用 registry 表：`user_id`（定宽二进制）作 key；用户名、昵称、密码哈希在 value；`name` 上的 `#[kv_index(by_name)]` 只服务一条热路径——登录时的精确查找。其余一切（会话、消息 sender、授权判断）只用 `user_id`。昵称是 value 数据；框架永不按昵称解析用户，只有账户管理操作才触碰用户名。registry 模式沿用 `crates/realm/src/mq.rs`（proxy id key + name payload + index）。

### 8. Fluxen 集成：信封形状与资源

视图操作是同一通道上的普通 event；消息信封沿用 fluxora 的形状——`Envelope { receiver: Vec<Session>, message: { sender, created, content } }`——被投递的 brick 在两个系统间携带相同结构。静态资源从 `/assets/` 以纯下载形式提供，不携带 event 语义。

## Honest semantic cost

- **群发逐连接做认证筛选。** 单集合之下，每次广播在全部连接上求值认证谓词——O(全体) 而非 O(目标)。接受：连接数是机器规模的小量，而替代方案（两个集合、登录时移动）把认证事实复制成两份，并给未来每一条撤销路径留下一处必须记住的第二个集合。
- **未认证连接不受计量。** 没有 idle 踢出意味着匿名洪泛可以无限持有连接。缓解属于部署层（按 IP 的连接上限），不属于协议。
- **设备身份是便利锚点，其损失的边界由业务设计划定，不由框架划定。** 被窃的 device_id 冒充设备的匿名历史。这有多要紧是逐应用的决定：商城模式把购物车存在本地，登录后才与服务端同步、合并，匿名期的历史权威性留在本地。账户凭证才是真秘密；框架把设备身份当作可寻址的便利身份，应用自行决定托付给它什么。
- **双编码让编解码面翻倍。** 每种帧类型有两条序列化路径；协议演进必须保持 CBOR 与 JSON 形状兼容（相同字段集、不同物理编码），否则调试客户端与生产客户端分叉。

## Consequences

- 集成测试获得真实入口：WS 客户端端到端地说 event 协议——经 `/admin` 上传 actor，连接、`login`、驱动 event、观察 realm events。
- Phase 8 的"turn delivery = realm events"自此成为对 Prism 的约束性契约，不再是计划注记。
- ADR-0015 的第 3 步（节点审批并入账户 auth）变得可实现：四个端点存在且挂在用户注册表之下。
- 后续的 chat/商城应用各自定义自己的按 event 认证声明；框架出厂只带 `login` 这一个 event。
- 实现顺序：`crates/prism`（WS、身份、连接集合、编解码）→ `/admin` + 上传端点 + 节点审批并入 → `/probe/<alias>` 挂载 → 用户注册表 → `/assets/` + 信封形状的视图 event。
