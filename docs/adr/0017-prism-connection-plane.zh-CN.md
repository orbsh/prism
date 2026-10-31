# 0017 — Prism 连接面：双重身份、单一连接集合与统一 event 协议

> **Languages:** [English](0017-prism-connection-plane.md) (primary) · [中文](0017-prism-connection-plane.zh-CN.md)

**Status:** Accepted (2026-09-22) — design; implementation pending, see Consequences

## Context

Aura 的集成链路缺少客户端入口。Gravity/krystallizer 摊位 已存在，远程 effector 也已拨入（Phase 3），但测试客户端、浏览器或被投递的应用没有一个统一的连接面。PLAN Phase 8 已将 Prism 定为以 Aura-resident component 形态托管的 WS 网关（"client connections pin here, not on Gravity; turn delivery = realm events"），ADR-0015 的节点审批端点也明确推迟到"Prism 的 auth 存在之后"。

这个设计服务的对象不止 chat。Fluxen 应用（参见 fluxora 的 Envelope 模型——`receiver: Vec<Session>` 包裹 `sender`/`created`/`content` 消息体）是任意应用的构造模式：chat、CMS、看板、商城，可以自由组合。因此连接面不能内建"每条连接都属于一个用户"之类的 chat 假设。

承接先前裁决的约束：

- ADR-0015：节点身份是每 effector 节点一把 ed25519 key；审批通过四个 curl 端点按公钥进行，这四个端点"将来挂在账户（Prism auth）之下"。
- okm key 纪律：`Key` 字段拒绝 `String`（仅定宽）；开放名字是运行时数据，通过 registry 表（`#[kv_index(by_name)]`）解析，不手搓字节 key。
- effector 不持存储、不持数据面凭证（2026-09-20 撤回裁决）——Prism 是引擎所在宿主（Aura 节点本身），可以持有账户注册表。
- long-polling 降级传输已作为架构原则被拒绝；传输契约是 WS-only。

## Decision

### 1. Prism 是连接面，以 Aura-resident component 形态托管

aura 侧新增连接面 crate，持有：WS accept/upgrade、路由分发（`/effector/<alias>`、`/admin/...`、应用路由、`/assets/...`）、身份模型与 event 协议编解码。协议定义按 PLAN Phase 8 归 Prism 侧所有，但连接面是 Aura 代码，回合投递走 realm events。Gravity、krystallizer 与被投递的 fluxen 应用都是*经由* Prism 到达的 摊位，不接进 Prism。

### 2. 双重身份：device 与 account；认证是业务层决策

框架只承认两种身份：

- **设备身份**——首次连接时服务端分配 `device_id`，客户端持久化在 localstorage。所有 event 默认以设备身份作为 sender。看板、浏览、加购物车乃至 chat 本身都在无账户的情况下工作。
- **账户身份**——`login` event（用户名+密码）将 `device_id` 绑定到 `user_id`。绑定后该连接的 sender 是 `user_id`；设备绑定跨重连保留（重新登录即恢复）。

localstorage 中的 `device_id` 是框架维护的锚点：框架的职责只有 device↔user 这条绑定记录。某个 event 是否需要账户是业务决策——每个 摊位 类型按 event 声明是否需要已认证的 sender；框架只保证"当前 sender 是 device 还是 user"可查询，并在"要求认证的 event 遇到匿名 sender"时返回标准错误。商城在"下单"上声明，chat 可以永不声明。

未认证连接不做 idle 踢出：公共服务本就合理地活在匿名连接上。

### 3. 连接集合：一个集合，认证状态是连接的字段

网关持有单个连接集合。每条连接自带认证状态字段：成功 `login` 前为匿名（`None`），之后绑定到 `user_id`。群发（向全体、向已认证、或向声明的子集）遍历这个单集合并按该字段筛选——筛选是每次扇出求值一次的谓词，不是一份需要同步维护的第二个集合。

早期裁决（两个集合、登录时物理移动连接）在此推翻：两个集合同一件事存两份，而每一次状态变化——今天是 login，将来是 logout 或服务端撤销——都必须同时写两处，否则两集合漂移。单集合加 per-connection 字段让漂移无法表达，而两个集合省下的群发遍历在连接量级上并非热路径。

摊位 可声明是否要求认证。身份经投递载荷到达 handler（§7 下 2026-09-22 修正案）：prism 把 sender 信封包进 event args；`Ctx`、`Job`、`InstanceId` 不带身份字段。身份同样刻意不作为存储分区 key：分区可按 `channel_id` 或任何业务维度（partitioning.md §2.1）——地址回答「谁串行处理」，信封回答「谁发起」。

### 4. 单一 event 协议，一个字段，JSON 与 CBOR

线上双向只携带一个字段——`ev`；协议不编码方向。事件就是事件：客户端的 `{"ev": "order.submit", ...}` 与服务端的 `{"ev": "order.created", ...}` 是同一种形状。`emit`/`on` 是各端实现细节：aura 侧是 摊位 的 `@on` 声明与 `emit` 调用；客户端侧是 `ws.send` / `ws.on`。Prism 是 aura 的 event 语义到用户端的自然延伸——不存在「客户端 action / 服务端 event」的词汇分叉需要翻译（aura ADR-0026 命名节）——「action」一词一并废弃。早期草稿的裁决（server→client 帧以自己的字段区分、不复用 client 侧字段）被更强地满足：一个字段，方向在协议层不存在——分发无从依它分支。

业务操作（login、视图操作、下单）就是客户端 emit 一个业务命名的 event——「action」一词废弃：客户端的 action 本来就是 emit 出去的 event，统一词汇把这层关系说得更精确。两种编码：CBOR（默认）与 JSON（调试）。选择发生在握手时通过查询参数（`?protocol=json`）；编解码器在连接生命周期内固定。

WS 常连接；`login` 是其上的普通 event，不是独立的 HTTP 往返。

### 5. admin 前缀与 摊位 上传

`/admin` 之下：`POST` 上传/注册 摊位 代码，加上 ADR-0015 的四个节点审批端点（`POST /nodes`、`GET /nodes`、`POST /nodes/{alias}/approve`、`DELETE /nodes/{alias}`），现在挂在账户之下。krystallizer 与 gravity 各自拿到一个薄上传脚本（拼 JSON、POST）——第一个脚本放在本仓 `scripts/` 验证端点，然后复制到各仓库。

### 6. 远程 effector 入口 `/effector/<alias>`

现有 effector 拨入网关（register / Call / Result / Host 帧）挂载到 `/effector/<alias>`。语义不变：effector 拨出，realm 按 alias 键控 `probes`，residency 身份以 `session = "<booth_type>/<key>"` 传递。

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

- 集成测试获得真实入口：WS 客户端端到端地说 event 协议——经 `/admin` 上传 摊位，连接、`login`、驱动 event、观察 realm events。
- Phase 8 的"turn delivery = realm events"自此成为对 Prism 的约束性契约，不再是计划注记。
- ADR-0015 的第 3 步（节点审批并入账户 auth）变得可实现：四个端点存在且挂在用户注册表之下。
- 后续的 chat/商城应用各自定义自己的按 event 认证声明；框架出厂只带 `login` 这一个 event。
- 实现顺序：`crates/prism`（WS、身份、连接集合、编解码）→ `/admin` + 上传端点 + 节点审批并入 → `/effector/<alias>` 挂载 → 用户注册表 → `/assets/` + 信封形状的视图 event。

## Update — 2026-10-29：`init` 答案的来源，与 bundle 的挂载形状

Phase 1.10 悬着的问题作为一个部署事实收口，不引入新机制。

**两种合法来源，一条路径。** 两者都把布局以普通 `create` 帧投给发问的那条连接，客户端的渲染路径不因来源而分叉。

- `file:<path>`——启动时读取（YAML 或 JSON；YAML 是超集，一套解析器覆盖两者）。walkthrough 的两栏壳（左图、右流）就是随仓库发布的默认文档。
- `event:<type>/<handler>`——向 realm 提交，落在该摊位类型的 SINGLETON 实例上：GLOBAL 摊位，全场一个实例，因为布局不是逐连接的状态。

**`webhook:<url>` 明确拒绝。** 取 HTTP JSON 再转发是上一代（Fluxora）的形状：把一次网络取数放在客户端的第一个 event 上，多一个部署依赖和一个运维可见的配置面，换来的通用性上面两种形态已经覆盖（静态文档、计算出来的文档）。需要该 URL 的 handler 在自己的函数体里发请求——能力归它，不归网关。

**缺失是声明出来的，不给默认值。** 没有 `PRISM_INIT` 时，`init` 回标准错误值并点名该配置，其它面照常服务，启动行说明当前生效的是哪种来源。

**Bundle 挂载。** 产物就是原点：`/` 出它的 `index.html`，其余未被路由认领的路径都是它里面的那个文件；保留前缀（`/ws`、`/channel`、`/code/…`、`/admin/…`、`/effector/<alias>`）先路由，产物永远遮不住动词。这正是 stage 的形状，也是"产物按 trunk 吐出的样子直接用"的前提——不需要 `--public-url`，不改写链接，一份构建同时服务 reflux（页根）与 prism。树里的 `/assets/…` 是产物自己的目录（`main.css`、模块媒体），不是挂载点。

**渲染器腿是 `/channel`**，即渲染器页面本来就连接的路径（`mount()` 的默认值、stage 的端点）：宿主适配渲染器，所以产物零配置，prism 也不必为了一个路径名自带页面。

**布局在连接时推送——没有 `init` 动词。** 渲染器的 socket 一打开，场馆就说出它有什么：`PRISM_INIT=file:<path>` 时把启动读到的帧立即发出（末尾一个空的 `init.result` 收尾）；`PRISM_INIT=event:<name>` 时场馆 **emit** 这个事件（默认名 `ui_init`）就不管了——应用要对它做什么（一次调用、一次查询、一整轮和模型的对话）最终都落在 view 事件上，那才是渲染器画的东西。场馆不为布局去调应用，也不等答复：调用等于把场馆塞进应用的工作中间，而"由客户端索取 `init`"等于让每个宿主的页面去学一个场馆本可以自己说的动词。未声明来源是合法的，回 `init.error`。

**场馆同时是订阅方。** 它为自己注册一个摊位类型（`renderer`）与一个 handler（`view`）：应用 emit `view` 并带上 `{device?: n, frames: <帧或帧列表>}`，这些帧就推给指定的渲染器（不指定则全部）。这就是转发路径的全部——场馆不解释帧，所以布局、append、patch 走同一条路。启动日志会说出这条订阅，因为"什么都不转发的场馆"和"没有东西可说的场馆"看起来一模一样。

**Bundle 挂载。** 产物就是原点：`/` 出它的 `index.html`，其余未被路由认领的路径都是它里面的那个文件；保留前缀（`/ws`、`/channel`、`/code/…`、`/admin/…`、`/effector/<alias>`）先路由，产物永远遮不住动词。这正是 stage 的形状，也是"产物按 trunk 吐出的样子直接用"的前提——不需要 `--public-url`，不改写链接，一份构建同时服务 reflux（页根）与 prism。树里的 `/assets/…` 是产物自己的目录（`main.css`、模块媒体），不是挂载点。

**渲染器腿是 `/channel`**，即渲染器页面本来就连接的路径（`mount()` 的默认值、stage 的端点）：宿主适配渲染器，所以产物零配置，prism 也不必为了一个路径名自带页面。



**是两条腿，不是一个共享 socket。** `/ui` 是渲染器腿，说上述格式；`/ws` 保持程序腿（CLI、对端、测试）的 `{ev, args}`。先例是 stage：UI 走 `/channel`，对端走 `/cli`——两个客户端类别、两套词汇、一条腿一种形状。同一条腿上放两种形状，等于把分裂塞进每一帧的解析器；而对端腿的测试、CLI 与 effector 拨入读的都是 `args`。

**流式输出格式是调用参数。** 由生产端决定流里装什么：gravity 的 turn 调用带 `protocol`（`data`——裸 delta，默认——或 `fluxen`——对布局声明的槽做 `append`/`set` 的视图帧）。应用知识（哪个槽、累积什么）留在摊位；渲染器保持通用，槽名不编进 bundle。fluxen 侧记在 `docs/decisions/0021-external-integration.md` 与 `docs/external-integration.md`。

## Update — 2026-10-29（同日稍晚）：信封归宿主，所以场馆用操作说话

fluxen 去掉了线上信封（fluxen `460d867`，ADR 0021 决策 6）。现在跨过渲染器 socket 的是**动作
列表本身**——`Vec<Content<Accrete>>`，JSON/CBOR 数组，永远是数组（一个动作就是一元素数组）——
而 map 会在渲染器边界被点名拒绝（`frame is an envelope, not an action list`）。因此本记录里
有三件事不再成立，上面写着它们的那几段是被**取代**，不只是被补充：

- **下行不再是 `content::Message{ev, sender, created, content}`。** 信封移到宿主那侧，而 prism
  不需要它：场馆自己要说的内容，用与应用视图**同一套词汇**表达——`inject` 操作（fluxen
  ADR 0011：往页面 vals 平面远程写入，页面上看不出与自发 emit 的区别）。两个槽承载它：
  `identity`（我是谁：`{device, user}`，连接时 / `signup.result` / `login.result` /
  `logout.result`）与 `notice`（拒绝、失败、`turn.done`）。布局用 `kind: local` 绑它们，跟绑
  任何别的事件一样，场馆永远不教页面学动词。
- **没有 `ev != "draw"` 的帧了。** 只剩一种形状：操作数组。想说"已连接"的场馆，要么把它说成
  一个操作，要么不说。
- **没有 `init.result` 收尾帧。** 一条消息就是一份动作列表，列表到齐就是结束。那个空 ack 只
  因为信封可以空着才存在。
- 不变的部分：布局在连接时即推、`PRISM_INIT` 仍有两种合法来源、场馆仍是订阅者
  （`renderer`/`view`）、bundle 仍是原点、turn 仍走这条腿（`protocol=fluxen`，槽位由本仓声明）。
  gravity 的视图帧成批到达，prism **原样成批转发**，因为一轮之内的顺序是语义：文本区的 `set`
  必须先于填它的 patch。

对测试的后果：这条腿的断言读数组（`frame[0]["action"]`），绝不读 `frame["ev"]`。
`crates/prism/tests/ui_leg.rs` 锁住新形状，包括"摊位的答案必须本身是操作才会被画出来"。


## Update — 2026-10-29（再晚一些）：两个方向都是 `view:<名字>`，摊位永不向页面"返回"

渲染器腿两个方向上都只是**传输**，标记是让它安全的东西。下面是定案的形状，取代本记录同日
早前"摊位的答案变成帧"那一读法。

**一套词汇，带标记。** 跨这条腿的一切都叫 `view:<名字>`。标记不是装饰：页面只能寻址、也只能被
`view:` 前缀的名字寻址，发什么都伪造不出内部事件。prism 订阅的是**前缀**（`view:*`——aura 的
wildcard 存的是去掉星号的前缀，所以声明里要带星号），一条 singleton 路由，不是一份需要预先
知道的事件清单。

**上行。** 页面发 `Outflow {slot, id?, data}`（`slot` 是这次交互进哪个槽；`event` 不在这条线上
——fluxen ADR 0023）。场馆把它包起来发 `view:<slot>`——载荷 `{channel_id, sender, data}`——aura
按名字路由到绑定了它的摊位。场馆加上去的那部分知识就是**信封**：哪个频道、谁在说话。摊位靠
自己的声明（`@on("view:add-to-cart")`）知道发生了什么，永远不必在运行时问事件名。

**下行。** 摊位发 `view:<名字>`，载荷 `{name, channel_id, sender, data}`；`name` 也带标记，因为
Rust 摊位的函数体拿不到触发它的事件名（具体名字被派发层消费掉了）。场馆去掉标记，把 `sender`
从 fan-out 里排除——页面不听自己输入的回声——然后交出载荷。

**两种载荷形状都合法。** 操作列表就**画**；其他一律是**数据**，以
`inject {slot: <名字>, name: <名字>, data}` 下发：落点是 vals 平面，跟页面自己发了这个名字完全
一样。场馆除了解码之外不做预校验——宿主再校验一遍，就等于在 fluxen 之上强加第二套词汇，而帧
是否合法是 fluxen 判的。

**这删掉了什么。** 摊位的**返回值**不是通往页面的路。`invoke` 不是这里的通道：解不出操作列表的
答案什么也画不出，场馆也不会给它编名字（`<event>.result` 就是这样一种编造，已删）。留在调用点
的只有一行诊断，因为"答案什么都没画"否则会被当成渲染器的问题去查。

**同日定案（裁决是"退场"）：** 场馆就是一个普通发方。它发 `view:identity`（`{device, user}`）、
`view:notice`（`{detail}`）、`view:turn.done`、`view:turn.error`，自己的 init 布局也走同一条平面
（`view:layout`，载荷就是操作列表）——事实之间的顺序于是归队列管，不再是两个写入方互相抢。
`identity`/`notice` 两个**槽常量**退场，这两个名字只剩"布局可以绑"的约定。事实也不再带
`connected`/`signup.result` 这类分动词后缀：页面绑 `identity` 就永远看到"我是谁"，那本来才是它
要的。

**上行是 emit，而页面从不发带标记的名字。** 渲染器的线协议仍是 `Outflow {dest, id?, data}`——
裸的 `dest`，因为渲染器的不同使用方对名字的理解不同，谁都不该为这个场馆的标记买单。prism 收到后
把它变成 `view:<dest>` 发上平面，信封 `{name, channel_id, sender, args}`；sender 由这条 socket
自己的身份填，页面依旧没有任何字段可以冒称别人。

这需要一条框架规则，已随这次一起落地：**声明可以把函数入口名和它订阅的事件名分开**
（`aura_booth::ReceiveDecl.entry`，写法 `receives: {view:add-to-cart: {entry: add_to_cart}}`）。
没有它，事件名就是 handler 名，带标记的事件等于向脚本要一个叫 `view:add_to_cart` 的函数——语言里
根本写不出来——而且投递被丢掉时一声不响。摊位声明带标记的订阅、函数保持原名，就是标记的全部代价。

**摊位永远不朝页面"返回"**——所以场馆的测试摊位现在是把答案 **emit** 出去，而不是 return。
投递会丢弃返回值；页面只被事件到达。

## Update — 2026-10-31：标记改为 `ui:`

面向界面的事件标记原为 `view:`，现为 `ui:`。规则本身没有变：过到渲染器那边的事件仍然双向带标记，
页面仍然发裸 `dest`，场馆仍然订阅前缀（`ui:*`，通配）并在下行时剥掉标记。换词的缘由是 `view` 在本栈
里已经背了三个意思——krystallizer 的会话投影（`mem_core::view`）、fluxen 的 `forcegraph::View`、
以及这个标记——而标记自己的职责比它们都窄：**这个事件在另一端有一张用户界面。**本日期之前写作
`view:<name>` 的地方（本 ADR 正文、两份 PLAN、gravity 的各 ADR）一律读作 `ui:<name>`。

## Update — 2026-10-31：生命周期的另外三个事实

连接的生命被裁定为一条普通事件流（MODELING：`ui:connect`、`ui:login`、`ui:logout`、
`ui:disconnect`），而过去只有 `connect` 被说出来。另外三个现在也在它们发生的时刻说出，走同一条
`say` 路径——于是每一个都是线上的信封，前缀路由把它带回同名的槽位，绑定 `login` 或 `logout` 的布局
不用问就知道：

- `ui:login {device, user}`，在注册与登录时。`ui:identity` 仍然同时在说，因为 token 是只有它能携带的
  东西（页面必须留住的轮换凭据）；`login` 是应用可绑定的生命周期事实。
- `ui:logout {device}`，绑定结束时。
- `ui:disconnect {device}`，连接消失时，且在连接表忘掉该设备**之前**说出——此刻页面自己的 socket 已经
  没了，这正是它是对应用的事实、而不是场馆给浏览器的一条 notice 的原因。

`tests/ui_leg.rs` 三条都钉住了：鉴权用例在注册后读 `login`、登出后读 `logout`；
`a_disconnect_is_a_fact_for_the_application` 注册一个探针、开一个渲染器、丢掉它，断言平面收到了那个设备。
