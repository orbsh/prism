# 连接与登录：从握手掌到 ctx 之外的 user

同样的四件事，prism 把它们放在**服务端**：连接一建立，venue 就把世界铺开；页面不需要先说自己是谁。本文给写部署的人与写 booth 的人各一半。

## 一、身份：握手那一刻就有了

prism 的页面是一份**bundle**（`crates/prism/src/assets.rs`：`/` 答 `<root>/index.html`），由渲染器（fluxen）挂载，连到 `/channel`。身份在 **HTTP upgrade** 就解析完了（`Gateway::identify`，`crates/prism/src/lib.rs:626`）：

- 活 token → 命名 `(device, user)` 并滑动过期；
- 没有可用 token → **当场铸一个新 device 和一个新 token**（所以客户端永远带着"下次能交回来的东西"离开）；
- 超过 idle 窗口 → 以匿名连接，但 device id 保留（别名活着、权限没了），并 `say(NOTICE_EVENT, {reason:"idle"})` 告诉页面。

## 二、连接：服务端先铺三层，才轮到 booth

`serve()`（`crates/prism/src/ui.rs:606`）在**读客户端第一个字节之前**依次说三层东西，顺序本身就是设计（注释原文："THE FRAME COMES FIRST"，ADR-0019）：

**第 1 层：外框（部署文件，不是代码）**

```rust
gw.ui_frames(device, ops_of(vec![shell.create()], "shell"));   // ui.rs:710
```

`Shell::create()`（`crates/prism/src/shell.rs:90`）只产出一条：

```json
{"action": "create", "slot": "", "data": <shell 文件的 root>}
```

`slot: ""` 是**整页根**——所以它只能由 venue 发，画它的不能是 booth。root 里有外框（header、导航、页容器、用户位），每个岛一个**空区**，区的 slot 名固定为 `island:<booth>`（`ISLAND = "island:"`）。这一步之后页面上有外框和若干空岛，谁也没画。

**第 2 层：这个观看者的事实（venue 自己算）**

```rust
gw.ui_frames(device, frame_data(&gw, device));                  // ui.rs:711
```

`frame_data()`（`ui.rs:285`）不是配置也不是 booth，是 venue 按这条连接算出来的：`set` 用户位（别名，未绑定就 `短id (guest)`）；对文件里声明的每个 region，`append` 一个导航项（id 是 booth 名，label 来自文件）+ 一个**页容器**，后者的 `bind.source` 是 `island:<booth>`，空着时显示一个 loading canvas。

**第 3 层：连接事件，booth 才开始**

```rust
let island = gw.active_island(user0);
gw.connect_event(device, island).await;                        // ui.rs:713
```

venue 在平面上说 `ui:connect {device, island}`，**哪个 booth 声明了它，哪个 booth 拿帧回答**（通常是往自己的 `island:<booth>` 槽 `set` 一棵树）。没有声明它的 booth，就永远没人画那个岛——连接照样活着，不报错。

**为什么必须是这个顺序**（注释的原话）：区域的槽必须在任何 booth 被要求填之前就存在，因为 booth 的回答是一次 `set` 到那个槽；而 booth 自己画不了外框，因为它持有的是它看不见的其他 booth 的区域。所以：**文件 → 场所的事实 → booth 的画**，三件分开说，而不是一条大帧。

一个容易错过的细节：region 在文件里声明（`regions: [{booth, label}]`），但**没有任何东西从声明里渲染**——venue 遍历 root 树，找出 `bind.source` 以 `island:` 开头的槽才算数（`shell.rs:127`）。声明只提供列表和 label，槽才是真相。

## 三、登录：三条腿

| 谁 | 负责什么 |
|:--|:--|
| prism（venue） | 三个动词 `login`/`signup`/`logout`、device↔user 绑定、token、闲置时钟 |
| auth booth（可替换） | 凭据怎么证明（**call**：`op:"verify"` / `op:"register"`）、登录面长什么样（`ui:auth.screen`） |
| 应用/部署 | 声明哪些事件需要人（`@on` 的 `auth`），以及 shell 里那个 `auth` 区 |

**密码不走事件平面**：每个 emit 都会落一行 MQ，而没有路由消费的载荷会被未鉴权的 `/admin/dead` 服务出去；call 不写队列行。这是这层用 call 而不是 event 的全部理由。

流程（`ui.rs:835`）：

```
页面上行 {dest:"login", data:{...}}
  → gw.login_op(OP_VERIFY, device, &data)        // booth 的 verify call
  → Ok((user_id, fresh_token))
      → auth_set(user, user_id)                  // 这个连接的绑定
      → flush_missed(device, user_id)            // 欠下的帧先交
      → say(TOKEN_EVENT, …)                      // 轮换凭证，旧的当场作废
      → say(login, {device, user})               // 生命周期事件，应用绑它
  → Err(e) → say(NOTICE_EVENT, {detail})         // 失败不是异常
```

登出：撤销该 device 的全部 token，然后**发一个新的匿名 token**——页面手里不留死票。

## 四、门与登录面

判定只问**声明**（`requires_auth`，`lib.rs:1572`）：`ReceiveDecl.auth` 注册时变成 `Route.auth`，一次路由查询，不看 booth 内部。匿名连接发一个被 gate 的事件 → 拒绝发生在投递前，应用永远不知道有人试过，框架代它说两句：`ui:auth.required {event}`（auth booth 订阅，这是自动弹登录面的触发点）与一句给人的 `notice`。**auth 动词自己不被 gate**。

登录面是平台保留的唯一槽名 `auth`：shell 声明一个绑定到它的区，booth 往区里画（不能是根，`create` 会替换整页）。

## 五、booth 作者怎么写

```rust
// 一次 call 的两个操作；返回值就是答案，没有第二个回调
match op {
    OP_VERIFY   => /* 校验凭据 → Some(user_id) / None */,
    OP_REGISTER => /* 建账号并返回 user_id，或 None */,
}
// 登录面：订阅 ui:auth.request（SEEKING），画到 ui:auth.screen（ANSWER）
```

booth 只做两件事：给一个 `user id`，画一块面。prism 的 `identity.rs` 不移动——它只接过 booth 说出的那个 id。

## 六、用户的登录信息在哪

| 你要的 | 拿法 | 说明 |
|:--|:--|:--|
| 是谁（服务端） | 每次上行解析出的 `user`，随 envelope 发出 | `sender_json` 里带 `device`、`user`、`display_name` |
| 是谁（页面） | `ui:connect` / 事实里的 `device`、`socket.user` | 页面不自己判断身份，venue 每帧都答 |
| 账号资料 | booth 自己的表（`Account` ns 50、`Credential` ns 55） | 账号名只在 venue 的注册表里，booth 不复制 |
| 台账 | `identity::Registry`（okm） | token、device、user、活动时间 |

## 七、和 reflux 的差别（一句话）

prism 的连接**带着身份出生、由服务端铺开**；reflux 的连接**默认匿名、由页面报出自己的世界**。前者步骤更多但全在一个函数里；后者的复杂度在边界上。两边的登录问答是同一个四边形：**声明决定谁要人 → 投递前拦 → 一个可替换的验证口子 → 凭证与服务端注册表**。
