# 0017 — Prism 连接面：双重身份、单一连接集合与一条腿一种形状

> **Languages:** [English](0017-prism-connection-plane.md) (primary) · [中文](0017-prism-connection-plane.zh-CN.md)

**Status:** Accepted（2026-09-22）——已在 `crates/prism` 实现；渲染器腿的线协议由
`crates/prism/tests/ui_leg.rs` 锁定，对端腿由 `crates/prism/tests/peer_leg.rs` 锁定。

## Context

Aura 的集成链路缺少客户端入口。Gravity/krystallizer 摊位已存在，远程 effector 也已拨入（Phase
3），但测试客户端、浏览器或被投递的应用没有一个统一的连接面。PLAN Phase 8 已将 Prism 定为以
Aura-resident component 形态托管的 WS 网关（"client connections pin here, not on Gravity; turn
delivery = realm events"），ADR-0015 的节点审批端点也明确推迟到"Prism 的 auth 存在之后"。

这个设计服务的对象不止 chat。一个 fluxen 应用就是一页由应用 emit 的动作列表画出来的界面
（`Vec<Content<Accrete>>`，fluxen ADR-0011/0021），背后是它自己的槽位与事件；chat、CMS、看板、
商城都是这个形状，可以自由组合。因此连接面不能内建"每条连接都属于一个用户"之类的 chat 假设。

承接先前裁决的约束：

- ADR-0015：节点身份是每 effector 节点一把 ed25519 key；审批通过四个 curl 端点按公钥进行，这四个
  端点"将来挂在账户（Prism auth）之下"。
- okm key 纪律：`Key` 字段拒绝 `String`（仅定宽）；开放名字是运行时数据，通过 registry 表
  （`#[kv_index(by_name)]`）解析，不手搓字节 key。
- effector 不持存储、不持数据面凭证（2026-09-20 撤回裁决）——Prism 是引擎所在宿主（Aura 节点
  本身），可以持有账户注册表。
- long-polling 降级传输已作为架构原则被拒绝；传输契约是 WS-only。

## Decision

### 1. Prism 是连接面，以 Aura-resident component 形态托管

连接面是本仓的 `crates/prism`，线协议编解码单独成 `crates/prism-protocol`。它启动一个 aura
引擎，并在**同一个** accept loop 上服务所有面向客户端的面：`/ws`（程序）与 `/channel`（渲染器）
的 WS upgrade、`/admin/...`、`/effector/<alias>`、`/code/<sha256>` 导出（ADR-0027）、场馆自己的
`/app.css`，以及兜底的渲染器产物。协议定义按 PLAN Phase 8 归 Prism 侧所有；连接面是 Aura 代码，
回合投递走 realm events。Gravity、krystallizer 与被投递的 fluxen 应用都是*经由* Prism 到达的
摊位，不接进 Prism。

### 2. 双重身份：device 与 account，一份凭据、一只时钟

连接面只承认两种身份与一份凭据：

- **设备**——首次连接时铸造（`Uuid7`），是一条连接的地址，在被登录绑定之前是匿名的。看板、浏览、
  加购物车乃至 chat 本身都在无账户的情况下工作。
- **账户**——`signup` / `login` 把设备绑定到 `user_id`（`Uuid7`）。绑定记在设备行上，所以重连回来
  即是已绑定的，不需要重新认证。
- **token**——凭据（`Uuid4`），握手时以 `?token=` 出示，在注册、登录、登出时轮换。它说出的是设备；
  客户端能猜到的 id（`?device=`）不构成任何认证。

新鲜度是**设备**的，不是 token 的。`Activity`（ns 54，每设备一个 `at`）在消息路径上写、在连接处与
每个上行处读；`Token` 没有过期时间，`Device` 没有 `last_seen`。沉默超过 `PRISM_IDLE_WINDOW`
（秒；默认 30 天）的设备不再是被鉴权的，而 token 即使还活着也不换来任何东西。同一个问题两只时钟
就是两个迟早要打架的答案，所以只有一只。

`Activity` 单独成表而不是它描述的那一行上的字段，因为否则消息路径就是每消息一次设备行的
read-modify-write，会和 alias、绑定互相丢更新。设备行装少变的东西，时钟装常变的东西。

设备熬过自己的超时。`resolve_token` 只说出设备；那个名字是否还带权限由 `idle(device)` 回答。所以
沉默的设备以匿名连接、但保住 id 与 alias，被告知原因（`notice` 带 `reason: idle`），并在同一条连接
上以一次登录重新挣得信任。鉴权动词（`login`、`signup`、`logout`）不受窗口门控——拒绝那条重新挣得
信任的消息，等于让页面无路可回。跨过窗口的 socket 不断：它失去的是上行，不是连接。

没有任何机制会因为一条连接匿名或安静就把它踢掉。公共服务本就合理地活在匿名连接上，而窗口拿走的是
认证，从来不是 socket。

某个 event 是否需要账户是业务决策，而且只写一次——写在订阅本身。脚本里是 `@on("ui:turn.submit", auth=True)`，steel 里是 `(on "ui:turn.submit" (hash "auth" #t) handler)`，Rust 体是 `on_as(...).gated()`：标志骑在**已经写着事件名的那条声明**上（aura `ReceiveDecl.auth`），注册时被搬到路由（`Route.auth`）。于是场馆在投递之前问**路由表**——路由到的事件、其声明带门控的，遇到匿名 sender 就回标准错误值，socket 保持。不存在第二张按事件名索引的表，schema 里也没有：一个写两遍的名字，就是会有两个答案的名字。框架只保证「当前 sender 是 device 还是 user」可查询。商城在「下单」上声明，chat 可以永不声明。

**认证是场馆的；证明一份凭据是摊位的。** 形态是平台内部的 SSO，prism 就是身份提供方。三个动词（`login`、`signup`、`logout`）、device↔user 绑定、token 与那只时钟属于 prism，且不可配置：应用从不认证任何人，从不接触密码、校验值或 token，也从不持有登录流程——它只声明某个 event 需要用户，并收到 `sender {device, user}`。可被部署替换的是「证明」与「界面」：名为 `auth` 的摊位用一次 call（`verify` / `register`）回答「这份凭据指向哪个 user id」，场馆随后绑定设备并轮换 token。接缝是一次 CALL，绝不是事件：每次 emit 都会作为 MQ 行落盘（aura `realm/src/events.rs`），而投递不到任何路由的 payload 会被 `/admin/dead` 以纯 HTTP 原样吐出（§7）——密码不能走那条路。出厂摊位把凭据存在本实例里（§9）；想换成外部 IdP、LDAP 或重定向流程的部署替换的是这份策略，场馆不动。没有 `auth` 摊位的部署是合法的：它照常服务匿名平面，每次登录尝试回标准错误值并点名缺失的类型。（OAuth2 的重定向那一半——离开页面再回来——没有做：渲染器没有「离开本页」的动作，场馆也没有保留回调前缀。）

**登录面也是摊位的，而且只是一个保留区。** 页面或应用用 `ui:auth.request` 请求（`{"mode": "signup"}` 换另一屏），摊位用一条 `set` 回答 `ui:auth.screen`，落进 `auth` 槽。这个槽是**布局**声明的区域——一个绑到它的 `placeholder`——因为摊位拿不到根：`create` 会**替换整页**（fluxen `ui_core/src/ctx.rs`），所以登录面是应用布局里的一座岛，不是第二个根。于是 `auth` 是全平台唯一保留的槽名，也是应用付的全部代价：shell 里一个节点，加一个只会说「打开它」的控件（`dest: auth.request`）。表单自己的 bind 是场馆的动词（`login` / `signup` / `logout`），这正是凭据不上平面的原因；而场馆那条 `ui:login` 事实把屏幕换成已登录行（名字由摊位读自己的账户行）。区域**站在页面哪里**属于页面的编排，写在 `assets/app.css`——与这里其他版式数字同一个文件。

**拒绝会把它打开。** 匿名连接发出的 gated 事件在投递**之前**就被丢掉，所以应用永远不知道发生过——一个只在人找得到门时才打开的登录面，等于在最需要它的时刻缺席。于是场馆把 `ui:auth.required {event}` 说给那台设备，与说给人的 `notice` 并列；摊位订阅前者并作画。应用路径不参与。

### 3. 一个连接集合；认证状态是连接的字段

网关持有**单个**连接集合。每条连接自带认证状态字段：成功 `login` 或一个活着的 token 填入它之前是
匿名（nil uuid）。群发——向全体、向已认证、或向声明的子集——遍历这个单集合并按该字段筛选：筛选是每次
扇出求值一次的谓词，不是一份需要同步维护的第二个集合。两个集合会把同一件事存两份，而每一次状态变化
（login、logout、撤销）都必须同时写两处，否则两集合漂移；单集合加 per-connection 字段让漂移无法
表达，而两个集合省下的群发遍历在连接量级上并非热路径。

渲染器的连接单独一张表——写入方不同、帧类型不同——但携带的是**同一组**身份事实：一个设备，以及
`login` 写入的认证字段。两张表分开，才让一条腿说 Accrete 而不必教会另一条腿。

撤销正是"字段足够"的证明。`POST /admin/revoke {device_id}` 在注册表里清掉绑定并给设备打上标记，
清掉**两条腿**上的认证字段，先给对端连接发 `revoked` 再关闭它，并关闭那个 sender 被丢掉、落在渲染器
腿上的连接——被撤销设备的页面不会继续守着一个已经不存在的会话。

### 4. 两条腿，一条腿一种形状：`/ws` 与 `/channel`

连接面暴露两条客户端腿。没有任何路径携带两种形状。

**`/ws`，程序腿**（CLI、对端、测试）：双向 `{ev, args}`。协议不编码方向——出去的形状就是进来的
形状，分发无从依它分支。裸 `ev` 是**调用**同名摊位类型，回 `<ev>.result`——场馆自己的动词
（`signup`、`login`、`logout`、`broadcast`、`ping`）例外，它们由场馆自己用同一形状回答。以
`ui:` 开头的 `ev` 是向平面上的**emit**，它的投递以 `{ev: "<去掉标记的名字>", args}` 回来，因为
程序读的是数据而不是画面。

**`/channel`，渲染器腿**：这条腿上没有动词。下行是**动作列表本身**——`Vec<Content<Accrete>>`，以
JSON/CBOR 数组形式，永远是数组（一个动作就是一元素数组）。map 会在渲染器边界被点名拒绝
（`frame is an envelope, not an action list`）：信封归宿主，而这里就是宿主。上行是
`Outflow {dest, id?, data}`——fluxen 自己的上行形状；场馆只读 `dest` 与 `data`。

同一条腿上放两种形状，等于把分裂塞进每一帧的解析器；而对端腿的测试、CLI 与 effector 拨入读的都是
`args`。

两条腿都回答连接：`/ws` 回 `{ev: "connected", args: {device_id, user_id, token}}`，`/channel` 用
§6 的那些事实——渲染器的页面也不需要自己的握手动词。

两种编码服务两条腿：CBOR（默认）与 JSON（调试），在握手时选定（`?protocol=json`，`?codec=json`
被视为同一个请求），在连接生命周期内固定。

WS 常连接。`login` 就骑在它上面——`/ws` 上是 `{ev: "login", args}`，`/channel` 上是一个发往
`login` 的 `Outflow`——从不是独立的 HTTP 往返。

### 5. `ui:` 标记：渲染器腿只有一套词汇

过到渲染器那边的一切，两个方向都带标记：`ui:<名字>`。标记不是装饰——页面只能寻址、也只能被
带标记的名字寻址，所以它发什么都伪造不出内部事件。用 `ui` 这个词，是因为标记的职责很窄：**这个
事件在另一端有一张用户界面。**

**下行。** 应用 emit `ui:<名字>`，载荷 `{name, device?, sender?, data}`。`name` 也带标记，因为
Rust 摊位的函数体拿不到触发它的事件名（具体名字被派发层消费掉了）。场馆去掉标记，把 `sender`
从 fan-out 里排除——页面不听自己输入的回声——然后转发载荷。

**两种载荷形状都合法。** 操作列表就**画**；其他一律是**数据**，以
`inject {slot: <名字>, name: <名字>, data}` 下发——落点是 vals 平面，跟页面自己发了这个名字完全
一样。场馆除了解码之外不做预校验：宿主再校验一遍，就等于在 fluxen 之上强加第二套词汇，而帧是否
合法是 fluxen 判的。不是 Accrete 操作的帧会被点名并跳过，所以一处笔误仍然画出其余部分。同一份数据
也会以 `{ev, args}` 到达这条事件所寻址的对端连接——一次 emit，两种呈现。

**上行。** 页面发裸 `dest`；场馆给它加上标记，以 `{name, device, sender: {device, user|null},
data}` 发 `ui:<dest>`。sender 是这条 socket 自己的身份，只查一次：页面没有任何字段可以冒称别人。
在这一侧标记属于场馆，因为渲染器的不同使用方对裸名字的理解不同，谁都不该为这个场馆的拼法买单。`/ws`
上的程序自己写标记；中间没有场馆替它加。

**寻址。** 地址是一个设备，或者没有设备（即所有渲染器）。频道是应用的业务，不是场馆字段
（ADR-0018），所以这条线上的任何载荷都不带它："发给频道 7 的所有人"只有拥有频道 7 的摊位才说得
出，由那个摊位把群翻译成收件人列表。

**一条路由。** 场馆订阅的是**前缀**——`ui:*`，Singleton 路由。声明里带星号，因为 aura 判定
"wildcard"看的是结尾的 `*`；于是场馆只持一条订阅，而不是一份需要预先知道的事件清单。

**声明把入口名分开。** 带标记的名字不是函数名，所以声明可以把函数入口名和它订阅的事件名分开
（`aura_booth::ReceiveDecl.entry`，写法 `receives: {ui:add-to-cart: {entry: add_to_cart}}`）。
没有它，事件名就是 handler 名，脚本会被要求提供一个叫 `ui:add_to_cart` 的函数——语言里根本写不
出来——而且投递会被丢掉。摊位声明带标记的订阅、函数保持原名，就是标记的全部代价。

**身份经载荷到达。** handler 收到 `{name, device, sender: {device, user|null}, data}`。`Ctx`、
`Job`、`InstanceId` 不带身份字段；身份同样刻意不作为存储分区 key：分区可按任何业务维度
（partitioning.md §2.1）——地址回答「谁串行处理」，信封回答「谁发起」。

**摊位永远不朝页面"返回"。** 投递会丢弃返回值；想让页面看见东西的摊位要 **emit** `ui:<名字>`。
解不出操作列表的答案什么也画不出，场馆也不会给它编 `<event>.result`——留在调用点的只有一行诊断，
因为"答案什么都没画"否则会被当成渲染器的问题去查。

### 6. 场馆是一个普通发方：关于连接的事实

场馆说的是跟所有人同一套词汇。它想让页面持有的事实就是 `ui:<事实>`，场馆为自己注册一个摊位类型来
承载它（`renderer`：本进程内的 Rust 函数体，单实例，订阅 `ui:*`）。事实落在与它同名的 vals 槽位，
布局用 `kind: local` 绑定它，跟绑任何别的事件一样。没有第二套词汇，也没有需要页面去学的分动词
名字。

- `ui:token {device, token}`——凭据，连接时说一次，之后每次签发或轮换（注册、登录、登出）都说。
  它不带 user：user 是场馆在每个上行处自己的答案，被告知 user 的页面成了已有答案的第二个家。页面
  真正推导不出、并且必须留住的是 token。
- `ui:connect {device}`——一条连接打开了，在 `/channel` 上说出。谁声明了它，谁就用布局来回答；没有
  这种摊位的场馆什么也不画，而连接照旧保持。
- `ui:login {device, user}`——注册或登录：那条绑定，也是应用可绑定的生命周期事实。
- `ui:logout {device}`——绑定结束。
- `ui:disconnect {device}`——连接消失，且在连接表忘掉该设备**之前**说出。此刻页面自己的 socket 已经
  没了，这正是它是对应用的事实、而不是场馆给浏览器的一条 notice 的原因。
- `ui:notice {detail}`——拒绝与失败：认证拒绝会点名该事件，登录失败带注册表的原话。窗口拿走了认证
  时是 `ui:notice {detail, reason: "idle"}`。

回合的结束是同一条平面上的另外两个事实，由跑这一轮的应用发出、寻址到发问的那个设备：
`ui:turn.done` 与 `ui:turn.error`。`/ws` 上的程序把它们读作 `turn.done` / `turn.error`；渲染器把
它们画进同名槽位。

平面不承诺两个事实之间的顺序。页面两个都绑，并且不假设谁先到。

### 7. admin 前缀：节点审批、撤销与死信环

`/admin` 之下，JSON 进、JSON 出，走同一个 accept loop：ADR-0015 的四个节点审批端点
（`POST /nodes`、`GET /nodes`、`POST /nodes/{alias}/approve`、`DELETE /nodes/{alias}`），加上
`POST /admin/revoke {device_id}` 与 `GET /admin/dead`——引擎的死信环，列出没到达任何人的事件，
以及到了队列却调不动函数体的投递。节点注册表就在 prism 自己的 okm 实例里（ns 52），与身份各表
相邻。

摊位代码不从这个前缀投递。摊位是运行进程里已注册的类型（`Gateway::with_echoes` 挂上各语言 echo
与 gravity 的 turn executor；`PRISM_MOUNTS` 在启动时重放声明的节点挂载），`/admin` 是运维的面，
不是投代码的面。

`/admin` 上没有任何路由认证运维者：现行的规则是 curl-first，边界就是部署边界——启动日志里写出的
`PRISM_IDENTITY` 姿势（`required` 或 `open`）。把 ADR-0015 的第 3 步折进账户，是这个平面尚未作出的
决定。

### 8. 远程 effector 入口 `/effector/<alias>`

现有 effector 拨入网关（register / Call / Result / Host 帧）挂载到 `/effector/<alias>`。语义不变：
effector 拨出，realm 按 alias 键控 `probes`，residency 身份以 `session = "<booth_type>/<key>"`
传递。URL 段**就是** alias 声明，`Register` 帧会重复它；两者必须一致，否则连接被拒——一个放行任意
alias 的路径会让 URL 在此后每一帧的日志里都成为谎言。

### 9. okm 中的身份与账户：id 优先访问，场馆自己的行里没有校验值

prism 自己的 okm 实例持有五张表，分属两个所有者。场馆的是 `Device`（ns 51）：`Uuid7` key；绑定、alias、`first_seen` 与撤销标记；`by_user` 索引供"我的设备"列表用，因为 id 是 `Uuid7`，列表按首次出现的顺序到达。`Token`（ns 53）：`Uuid4` key——凭据要的是不可预测，不是顺序；device、user、`issued_at`、`revoked_at`；`by_device` 索引让轮换、登出与撤销不必扫描就能退役整组。`Activity`（ns 54）：§2 的那只时钟。认证摊位的是 `Account`（ns 50）：`Uuid7` key；用户名、昵称；用户名上的 `by_name` 索引只服务一条热路径——登录时的精确查找。`Credential`（ns 55）：`Uuid7` key = user id；argon2id PHC 哈希，别无一物。

后两张拆开是因为校验值不是资料：账户的列表、dump 或导出不该携带可离线爆破的东西。`Registry::bind_login` 是设备认识用户的唯一入口，它接受的是摊位说出的 id——场馆无法校验密码，也从不持有密码。换掉的摊位把行存在它自己选的地方；场馆只留它一直留着的那些。

key 是 16 字节原始值，`get`/`scan` 会把 key 一并交回，所以没有行重复自己的名字，也不存在只能靠纪律维持的 key-payload 一致。

除用户名查找之外，一切只用 id：会话、消息 sender、授权判断都不碰名字。昵称是 value 数据；框架永不
按昵称解析用户。登录在铸造新 token 之前先撤销该设备旧的，所以泄漏的 token 活不过下一次登录；登出
撤销整组、清掉绑定，并给页面一枚新的匿名 token；撤销给设备打上标记，只有一次全新的登录才清掉它。

### 10. Fluxen 集成：产物就是原点，布局是应用的答案

静态资源在同一个 accept loop 上以纯下载形式提供，不携带 event 语义。**产物就是原点**：
`PRISM_ASSETS` 指定目录，`/` 出它的 `index.html`，其余未被路由认领的路径都是它里面的那个文件——
于是产物按 trunk 吐出的样子直接用，不需要 `--public-url`，不改写链接，一份构建同时服务 reflux
（页根）与 prism。保留前缀（`/ws`、`/channel`、`/code/…`、`/admin/…`、`/effector/<alias>`、
`/app.css`）先路由，所以产物永远遮不住动词。树里的 `/assets/…` 是产物**自己的**目录（`main.css`、
模块媒体），不是挂载点。带哈希的 `.js`/`.wasm` 一年不可变，其余一律重新验证；目录、`..` 与未命中
是同一个 404。场馆自己的样式表编译进二进制，并链接在产物的样式表**之后**，页面的版式靠顺序取胜，
而不是靠特异性。

布局不是部署事实。没有 `init` 动词，也没有需要配置的布局来源：连接时场馆说
`ui:connect {device}`，应用要对它做什么——一次调用、一次查询、一整轮和模型的对话——最终都落在
`ui:<名字>` 的视图事件上，那才是渲染器画的东西。场馆不为布局去调应用，也不等答复；调用等于把场馆
塞进应用的工作中间，而"由客户端索取 `init`"等于让每个宿主的页面去学一个场馆本可以自己说的动词。
没有产物的部署是合法的：它照常服务身份、回合与对端，并在启动时说出来。

帧是渲染器的词汇，场馆不解释它们——所以布局、append 与 patch 走同一条路。gravity 的视图帧成批
到达，prism **原样成批转发**，因为一轮之内的顺序是语义：文本区的 `set` 必须先于填它的 patch。

流式输出格式是调用参数。由生产端决定流里装什么：gravity 的 turn 调用带 `protocol`——`data`（裸
delta，默认）或 `fluxen`（对同一次调用声明的 `slots` 做 `append`/`set` 的视图帧）——或者一个
`event`，指名裸 delta 用哪个事件说出。应用知识（哪个槽、累积什么）留在摊位；渲染器保持通用，槽名
不编进产物。fluxen 侧记在 `docs/decisions/0021-external-integration.md` 与
`docs/external-integration.md`。

## Honest semantic cost

- **群发逐连接做认证筛选。** 单集合之下，每次广播在全部连接上求值认证谓词——O(全体) 而非 O(目标)。
  接受：连接数是机器规模的小量，而替代方案（两个集合、登录时移动）把认证事实复制成两份，并给未来
  每一条撤销路径留下一处必须记住的第二个集合。
- **窗口只摘认证，不断连接。** 空闲页面留着一个它用不了的 socket，直到它登录；而匿名连接根本没有
  上限——匿名洪泛可以无限持有连接。缓解属于部署层（按 IP 的连接上限），不属于协议。
- **设备身份是便利锚点，其损失的边界由业务设计划定，不由框架划定。** 被窃的 device id 冒充设备的
  匿名历史。这有多要紧是逐应用的决定：商城模式把购物车存在本地，登录后才与服务端同步、合并，匿名期
  的历史权威性留在本地。账户凭据才是真秘密；应用自行决定把什么托付给这个可寻址的身份。
- **双编码让编解码面翻倍。** 每种帧类型有两条序列化路径；协议演进必须保持 CBOR 与 JSON 形状兼容
  （相同字段集、不同物理编码），否则调试客户端与生产客户端分叉。
- **标记要付一次声明的代价。** 每条带标记的订阅都需要旁边写上 `entry`，否则投递会因为找不到一个
  语言拼不出的函数而被丢掉。保留朴素函数名的摊位，为每个事件付这一次代价。
- **场馆转发它不解释的帧。** 畸形操作在日志里被点名并跳过，什么也递不出去的载荷只记日志而不拒绝
  ——这条线上除了页面里 fluxen 自己的解码器，没有地方能报告一帧是否合法。

## Consequences

- 渲染器腿的线协议由 `crates/prism/tests/ui_leg.rs` 锁定：连接的两个事实（`token`、`connect`）、
  一条沉默连接跨过窗口并在同一设备上一登录即恢复、`ui:disconnect` 作为对应用的事实、数据落进与事件
  同名的槽位、sender 被排除在自己的 fan-out 之外、按 device 寻址、认证声明先拒后放、撤销关闭
  socket，以及 `/ws` 保持 `{ev, args}`。
- 回合的出口端到端锁定：`crates/prism/tests/peer_leg.rs` 在对端腿上提交 `ui:turn.submit`，读回
  `turn.delta` / `turn.done`，而匿名提交得到标准错误值、socket 保持。
- 集成测试有了真实入口：WS 客户端连接、登录、驱动事件并观察 realm events；同一个场馆在 `/` 上服务
  浏览器页面、在 `/ws` 上服务程序，两边都不必学对方的形状。
- Phase 8 的"turn delivery = realm events"是约束性契约：一次 emit 进、一次 emit 出，device 是
  地址，声明门控摆在它前面。
- 认证声明是两条腿唯一的 ACL 面——场馆读的是每个路由到的类型持久化的 `interface_schema`，没有第二
  张表可以漂移——后续的 chat 或商城应用各自定义自己的按事件声明，框架出厂只带 turn executor 与测试
  摊位的那几份。
- ADR-0018 在线上成立：没有任何载荷携带频道，把群名当地址 emit 的摊位到不了任何渲染器。
- 认证接缝由 `crates/prism/tests/auth_seam.rs` 锁定：没有 `auth` 摊位的场馆把 login 与 signup 回成
  标准错误值、并照常服务匿名平面；换一个摊位实现后，prism 绑定的是它说出的 user id，轮换后的 token
  同时恢复用户与设备。出厂摊位的两张表与「两半失败同一个错误」由 `crates/prism/src/auth.rs` 的单测锁定。
- 一次部署声明三件事：`PRISM_IDENTITY`（必需，`required` 或 `open`）、`PRISM_ASSETS`（可选；缺失
  是合法安装，照常服务身份、回合与对端）、`PRISM_IDLE_WINDOW`（可选，秒，默认 30 天）。库从空开始：
  场馆的 `Device`（51）、`Token`（53）、`Activity`（54）与出厂认证摊位的 `Account`（50）、
  `Credential`（55）就是形状，不读任何更早的行格式。