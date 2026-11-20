# 0020 — token 铸一次用一次，名字在铸的时候给

状态：已采纳（2026-11-20）

## 背景

prism 铸节点 token（`POST /admin/nodes {alias}`），只存它的 SHA-256 digest，token 明文只显示一次。token 走拨号（`?token=…`）；注册表回答 `registered | pending | conflict`；一个 alias 同时只有一个批准的 key，批准第二个就把第一个降级（`nodes.rs` 里的轮换规则）。

场地拿到节点池与 placement 之后（aura ADR-0053），两个缺口露出来：

- **token 绑的是一个名字，不是一台机器。** 拿到 token 的人可以从任何机器上用它：注册表核对了声称的内容，从没核对是谁在出示它。于是"一 token 一节点"只是约定，不是事实。
- **placement 需要人能说出口的名字，而没有地方填。** 人会提"把我笔记本上的文件拷到服务器上"。他没法用序列号提 —— 那不是人话，也不该让 AI 去学。真正的寻址仍按 id；名字只是工作被**要求**时的说法。

## 决定

1. **token 铸一次用一次：第一次成功注册的那台机器占用它。** 注册表行增加 `machine`，在第一次**已批准**的注册时写入，之后永不改动。此后这个 token 的每次注册都必须来自同一台机器；另一台机器拿它来就是 `conflict`，理由把两台机器都点出来。在同一台机器上重启则正常重连 —— "一次"是关于硬件的事实，不是关于进程寿命，所以这不是"消耗式一次性"（一个不能自己重新铸 token 的节点，会把每次无人值守重启都变成操作员任务）。

2. **说不出自己硬件的节点不绑定。** 它照常注册（`machine` 为空），注册表明说这个 token 未绑定、因此不限于一台机器，而它之后第一次带 id 的注册会绑定它。一条默默拒绝所有没有 SMBIOS 的机器的法律，等于让一个硬件细节决定节点能不能存在。

3. **标签在铸 token 时由知道那台机器的人给。** `POST /admin/nodes {alias, labels, owner}`：`labels` 是按原样输入的名字 —— `MyLaptop`、`xxServer`；`owner` 是它们所属的用户。裸名存放，一行一个，于是改名就是改名，而这一行始终是操作者说过的那句话。

4. **用户前缀在准入时加，不在注册表里加。** 执行层（effector mount 写入场地池的那一刻）把 `MyLaptop` 变成 `{owner}:MyLaptop`。注册表保留名字，场地按地址匹配。于是两个用户的 `MyLaptop` 不可能在 placement 里撞车，而两个用户都不需要知道有这个前缀。

5. **名字不寻址任何东西。** 真正的寻址按机器 id（effector ADR-0006），用户永远看不到它。标签是一次**操作**的选择词 —— "我的笔记本"、"那台服务器" —— 而从词到硬件的映射在准入处解析，那是唯一同时知道两者的地方。

## 否决的方案

- **严格的一次性消耗**（第一次成功注册即销毁 token）。那样重启就需要人或启动脚本重新铸；而容器节点重启是常态而不是例外。
- **让节点在 `Register` 里自荐标签。** 理由同 effector ADR-0005 §2b：标签是可猜、共享的词，于是节点自称 `MyLaptop` 与它顶替别人的笔记本无法区分。名字是准入方的，随 token 一起铸。
- **按标签寻址**（调用直接写标签）。那会把一个人话里的词变成路由键：改个名就孤立了历史，而两个用户的名字需要在每个调用点写语法。标签只在一个地方变成地址 —— 准入处。
- **把标签存进按 node id 索引的第二张表。** 载荷是定宽 TLV，没有序列类型；行内一行一个名字，把拆分留在**一个**函数里（`Node::label_list`），而不是加一个集合、一个索引和一场没人要的 join。
- **在铸 token 时就绑到 alias**（拒绝别的 alias）。alias 是连接对外答的名字，machine 是机器。绑到 alias 等于禁止改名，而改名是合法的操作员动作。

## 后果

- **泄露的 token 只是一台机器的 token。** 拿到它的人在别的机器上用它会被拒，且理由里两台机器都有名字。
- **本记录之前铸的 token 在第一次已批准注册前是未绑定的**，然后绑定。既有行照旧可读（缺的字段取 `Default`）。
- **给机器改名就是铸新 token**，这正是"一 token 一次"在操作上的含义：旧行撤销，新行带新名字，而机器 id 让历史保持连续。
- **管理界面是名字诞生的地方**，因此也是操作者词汇被审计的地方。`GET /admin/nodes` 报 `labels`、`owner`、`machine`，正是为此。
- **为了让这件事成立，effector 的帧变了**（`machine`，effector ADR-0006）。标签一个字都没上线。

## 测试

- `crates/prism/src/nodes.rs` —— `a_token_is_bound_to_the_first_machine_that_presents_it`（第一次已批准注册即绑定；同一台机器重连无碍；另一台机器是冲突且理由点名双方）、`a_node_that_cannot_name_its_hardware_stays_unbound`（空 machine 能注册、不绑定，之后第一次带 id 的注册才绑定），以及既有生命周期按三参数 verdict 更新。
- `crates/prism/tests/admin_nodes.rs` —— 铸 → 批准 → 裁决 的 HTTP 生命周期，`machine` 在接缝处给出。
- `crates/prism/tests/effector_handshake.rs`、`effector_mount.rs`、`runner_mount.rs`、`turn_start.rs`、`resource_path.rs` —— 注册携带 `machine`，在场状态通过场地池断言（`realm_set.pool()`）。

## 相关记录

`crates/prism/src/nodes.rs` 的轮换规则（批准第二个 key 就把第一个降级）、effector ADR-0003（一次拨号一个 token）、effector ADR-0005 §2b（标签是准入方的说法）、effector ADR-0006（节点说明它是哪台机器）、aura ADR-0053（池子、前缀、过滤器）。