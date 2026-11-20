# 0021 — 场地公布它能调用的东西

> **语言 / Languages:** [English](0021-the-venue-publishes-what-it-can-call.md) (primary) · [中文](0021-the-venue-publishes-what-it-can-call.zh-CN.md)

**Status:** Accepted（2026-11-20）

## Context（背景）

节点是靠 mount 可调用的：每个（alias × 操作语言）一个 `Remote` 类型，其 source 是固定 runner（gravity ADR-0001）。声明放在 `PRISM_MOUNTS`，每次启动重放，因为 `Remote` 定义不持久化。

于是这张表没有一个调用方能问的家。gravity 自己在 turn 的 args 里带着一份副本（"mount 表随 turn 的 args 走，所以每个调用方都带一份"），而 gravity ADR-0001 已经把后果记成 open：*prism 已经持有同一张表（`PRISM_MOUNTS`），每多一个客户端多一份声明就是一个漂移源*。

一张没人能读的表会带来两种失效：

- **客户端分不出"声明过"和"可调用"。** 注册失败的声明照样在清单里、照样在客户端副本里；类型却不在引擎里，调用在很久之后才失败，而混乱从那一刻就开始了。
- **客户端分不出"已注册"和"在场"。** 已注册的 mount，节点回家了，答案就是 "not connected" —— 发生在一次作业中途，一个 op 已经失败了。在场只有场地知道，在它的池子里（aura ADR-0053）。

## Decision（裁决）

### 1. 场地为它持有的表作答

`GET /admin/mounts` 返回它持有的每一条声明，每行三个事实，外加一组拒绝理由：

```
{ "mounts": [ { "type", "alias", "language", "carrier", "runner",
                "requires", "encoding",     // 解析后的答案，不是回声：requires 缺席就是 container
                "registered",               // 引擎收下了这个类型
                "node_connected" } ],       // 它背后的节点此刻在池子里
  "refused": [ "mount [x] (alias y) did not register: …" ] }
```

`registered` 与 `node_connected` 刻意是两行真话：前者说场地把这个类型铸出来了，后者说有人在听。挑 mount 的调用方两个都要，而第二个只有场地知道。

### 2. 表是场地状态，只装一次

清单由 `Gateway::install_mounts` 读取，装完留在场上。在此之前，mount 列表只作为启动期的副作用存在过：注册 mount 的地方有两个就是"什么可调用"有两个答案，所以只能有一个，而且它是场地上的一个方法，不是构造函数。

解析不了的清单仍是启动错误（gravity ADR-0001：声明是部署事实，永不是默认值），而安装失败会让场地保留它原本持有的表 —— 半张表比没有更糟。

### 3. 调用方来读，谁都不留副本

gravity ADR-0001 里那个 open 项按"读"的方向解掉：需要这张表的客户端向场地要。

把表注入 turn 的 args 是另一个候选，下面否决了。

在 gravity 里采纳它（丢掉它今天带的那份表）是 gravity 自己的改动，不是这一条；这条记录定的是场地有答案可给。

## 否决的方案

- **场地在路由 `ui:turn.submit` 时把表注入 turn 的 args。** 场地就得知道一个应用 payload 的形状才能往里写 —— 同一条边界，只是反着越：不让 gravity 学场地的内部，也不该让场地学 gravity 的信封。它还会让这张表变成场地写在别人结构里的一个值，而同一个场地服务的第二个应用就得重来一遍。
- **每个客户端各留一份**（今天的形状）。ADR-0001 已经把它记成漂移源；再说一次不解决问题。
- **只公布声明**（原样的清单文本，不作任何限定）。调用方最不该用的那一行，恰恰就是注册失败的那条声明，而清单说不出这一点。
- **只公布注册成功的。** 那样一个活着但节点不在的 mount 看起来可调用，调用方靠一次 op 失败才知道。

## 后果

- **一张表一个家。** 部署的改动对所有客户端同时可见，启动日志指向客户端读的同一页（`… refused — see /admin/mounts`）。
- **写错的声明仍然很响**：启动失败，或者拒绝理由在这一页上。什么都不会被静默跳过。
- **在场是快照，不是承诺。** 读和调用之间节点可能离开；列表缩小选择范围，它不预留任何东西。
- **这一页也是操作员的**：从同一页就能回答"我的调用为什么说没有这个类型"和"为什么说未连接"。

## 测试

- `crates/prism/tests/mount_listing.rs` —— `the_venue_publishes_what_it_can_call_and_what_it_could_not`：每条声明都列出、`requires` 报的是解析后的答案、节点拨入前 `registered` 为真而 `node_connected` 为假、一条记录进池后在场翻真，以及缺 `runner` 的清单是启动错误且已装的表不受影响。

## 相关记录

gravity ADR-0001 的 §prism 与它的 open 项（mount 表和谁携带它）、aura ADR-0053（池子，在场在哪里被决定）、aura ADR-0027（代码按引用 —— 未触动：runner 就是 mount 的 source）、prism ADR-0017 §7（它走的 `/admin` 前缀）。