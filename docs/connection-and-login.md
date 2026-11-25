# 连接与登录：从握手掌到 ctx 之外的 user

同样的四件事，prism 把它们放在**服务端**：连接一建立，venue 就把世界铺开；页面不需要先说自己是谁。本文给写部署的人与写 booth 的人各一半。

## 一、连接：带着身份出生

prism 的页面是一份**bundle**（`crates/prism/src/assets.rs`：`/` 答 `<root>/index.html`），由渲染器（fluxen）挂载，连到 `/channel`。身份在 **HTTP upgrade** 就解析完了（`Gateway::identify`，`crates/prism/src/lib.rs:626`）：

- 活 token → 命名 `(device, user)` 并滑动过期；
- 没有可用 token → **当场铸一个新 device 和一个新 token**（所以客户端永远带着"下次能交回来的东西"离开）；
- 超过 idle 窗口 → 以匿名连接，但 device id 保留（别名活着、权限没了），并 `say(NOTICE_EVENT, {reason:"idle"})` 告诉页面。

然后 `serve()`（`crates/prism/src/ui.rs:606`）在**读客户端第一个字节之前**依次：

1. 登记连接（单写者）；
2. 该账号有积压帧 → `flush_missed`：没有屏幕在时欠下的帧现在交出去；
3. 盖章活动时间；
4. `say(TOKEN_EVENT, token_fact)`——把凭证用渲染器的形状发给页面；
5. 有 shell → 先 `shell.create()`（外框与空岛），再 `frame_data`（谁在看、导航里有哪些岛、每个岛对应哪一行）；
6. `active_island(user)` → `connect_event`：在平面上 emit `ui:connect`，哪个 booth 声明了它，哪个 booth 拿帧回答。

## 二、登录：三条腿

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

## 三、门与登录面

判定只问**声明**（`requires_auth`，`lib.rs:1572`）：`ReceiveDecl.auth` 注册时变成 `Route.auth`，一次路由查询，不看 booth 内部。匿名连接发一个被 gate 的事件 → 拒绝发生在投递前，应用永远不知道有人试过，框架代它说两句：`ui:auth.required {event}`（auth booth 订阅，这是自动弹登录面的触发点）与一句给人的 `notice`。**auth 动词自己不被 gate**。

登录面是平台保留的唯一槽名 `auth`：shell 声明一个绑定到它的区，booth 往区里画（不能是根，`create` 会替换整页）。

## 四、booth 作者怎么写

```rust
// 一次 call 的两个操作；返回值就是答案，没有第二个回调
match op {
    OP_VERIFY   => /* 校验凭据 → Some(user_id) / None */,
    OP_REGISTER => /* 建账号并返回 user_id，或 None */,
}
// 登录面：订阅 ui:auth.request（SEEKING），画到 ui:auth.screen（ANSWER）
```

booth 只做两件事：给一个 `user id`，画一块面。prism 的 `identity.rs` 不移动——它只接过 booth 说出的那个 id。

## 五、用户的登录信息在哪

| 你要的 | 拿法 | 说明 |
|:--|:--|:--|
| 是谁（服务端） | 每次上行解析出的 `user`，随 envelope 发出 | `sender_json` 里带 `device`、`user`、`display_name` |
| 是谁（页面） | `ui:connect` / 事实里的 `device`、`socket.user` | 页面不自己判断身份，venue 每帧都答 |
| 账号资料 | booth 自己的表（`Account` ns 50、`Credential` ns 55） | 账号名只在 venue 的注册表里，booth 不复制 |
| 台账 | `identity::Registry`（okm） | token、device、user、活动时间 |

## 六、和 reflux 的差别（一句话）

prism 的连接**带着身份出生、由服务端铺开**；reflux 的连接**默认匿名、由页面报出自己的世界**。前者步骤更多但全在一个函数里；后者的复杂度在边界上。两边的登录问答是同一个四边形：**声明决定谁要人 → 投递前拦 → 一个可替换的验证口子 → 凭证与服务端注册表**。
