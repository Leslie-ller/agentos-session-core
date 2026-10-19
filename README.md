# AgentOS Session Core

在同一条对话中切换 Agent，保留历史消息、回复来源和各自的原生会话。

这是基于 [Codeg](https://github.com/xintaofei/codeg) 的会话扩展。界面、ACP 接入和基础工作区来自上游；本仓库只公开会话交接与连接保留部分。固定上游版本为 `0.33.0`，具体提交记录在 [upstream.json](upstream.json)。

## 做什么

| 功能 | 用户能看到的结果 |
| --- | --- |
| 统一时间线 | 换 Agent 后仍看见前面的讨论，每条历史回复保留原 Agent 的来源 |
| 跨 Agent 交接 | 新 Agent 收到此前的可见对话和工具结果，可以接着处理任务 |
| 保留原生会话 | 切回已用过的 Agent，继续它自己的会话；打开的对话页保留可用连接 |

例如：先用 Codex 分析代码，切到另一个 Agent 实现，再切回 Codex 检查。客户端把这些轮次放在同一条时间线里，各 Agent 的原生 session ID 分别保留。

切换本身不会自动发起任务。下一条用户消息触发后续工作。交接传递的是可见上下文，不传递隐藏推理、模型内部状态或图片字节。

## 快速开始

需要 Python 3.12+、Git、Node.js 24、pnpm（版本由上游 `package.json` 固定）和 Rust stable。桌面构建还需要对应系统的 Tauri 开发依赖。

```sh
git clone https://github.com/Leslie-ller/agentos-session-core.git
cd agentos-session-core
python3 scripts/prepare.py
cd workspace
pnpm install --frozen-lockfile
pnpm build
```

`prepare.py` 下载固定提交，应用接入补丁，再复制核心模块。默认输出到未跟踪的 `workspace/`；也可用 `--output <新目录>` 指定位置。有本地 Codeg 仓库时可加 `--source <仓库路径>`，脚本仅读取固定提交中的文件，不复制本地修改。已有输出目录会拒绝覆盖。

启动本地 Web 客户端：

```sh
mkdir -p .session-data
CODEG_DATA_DIR="$PWD/.session-data" \
CODEG_STATIC_DIR="$PWD/out" \
CODEG_HOST=127.0.0.1 CODEG_PORT=3080 \
cargo run --manifest-path src-tauri/Cargo.toml \
  --no-default-features --features server-bin --bin codeg-server
```

打开 `http://127.0.0.1:3080`，按照服务输出完成首次连接。也可使用 `pnpm tauri dev` 启动桌面开发版。

在设置中配置 Agent 并完成各自的登录，创建一条对话、完成一轮消息后，在输入区的 Agent 选择器切换。回到旧 Agent 时，客户端恢复对应 session ID；只在连接仍存活、工作目录和会话匹配时复用进程。关闭对话页或进程退出后，需要重新连接原生会话。

本版本为 Agent 建立应用自己的持久目录，不自动导入个人客户端的历史和凭据。首次使用需要单独配置。Devin 需要自行安装并注册 `devin` 自定义 ACP Agent，仓库不附带账号、二进制或注册配置。

## 源码入口

- [会话交接](overlay/src-tauri/src/acp/handoff.rs)：切换前检查、锁和旧连接退役。
- [归档与合并](overlay/src-tauri/src/db/service/conversation_handoff.rs)：SQLite 事务、上下文转移和历史去重。
- [原生目录](overlay/src-tauri/src/acp/session_profiles.rs)：应用持有的会话目录。
- [前端交接](overlay/src/lib/conversation-handoff.ts)：失败后的状态核对和回复来源。
- [接入补丁](patches/session-core.patch)：界面选择器、连接停放与恢复、服务路由及生命周期接入。

更完整的说明见 [设计与边界](docs/design.md)。

## 验证

在准备好的 `workspace/` 中执行：

```sh
pnpm test src/contexts/acp-connections-context.test.tsx \
  src/lib/conversation-handoff.test.ts \
  src/hooks/use-conversation-handoff-sync.test.ts
pnpm build
cargo test --manifest-path src-tauri/Cargo.toml \
  --no-default-features --features server-bin,test-utils \
  --test conversation_handoff
```

测试覆盖切回旧会话、历史去重、并发切换冲突、旧连接拒绝发送、连接失效和对话页关闭后的释放。自动测试使用受控连接与合成消息，不需要付费模型账号；实际模型调用需要自己的 Agent 环境。

## 实测：任务接续

2026-10-19 用真实模型测了 30 次跨 Agent 接续，**成功 27 次（90%）**。记录在 [`eval/handoff-30`](eval/handoff-30)。

- 任务：前一个 Agent 运行命令生成随机输入（随机标识、数量、单价），回复里不复述；切换后，后一个 Agent 不调用工具，只凭交接上下文按规则算出小计、运费和总额。
- 覆盖：Claude Code（claude-haiku-5-5）、Codex（gpt-6-luna）、Devin（swe-2-high）之间的 6 个切换方向，每个方向 5 次；每轮新开一条对话，依次走完 6 次切换。
- 判定：回复的 JSON 与期望值完全一致才算成功。失败不重试、不剔除。`audit.py` 从原始结果重新判定，并核对与测试脚本的判定一致。

| 切换方向 | 成功 |
| --- | --- |
| Claude Code → Codex | 5/5 |
| Codex → Devin | 5/5 |
| Devin → Claude Code | 4/5 |
| Claude Code → Devin | 5/5 |
| Devin → Codex | 5/5 |
| Codex → Claude Code | 3/5 |

3 次失败都发生在切到 Claude Code 时：它认为交接过来的历史不是本会话产生的，拒绝采用其中的输入（见 `failures.json`）。这是单一类型任务上的小样本结果，不代表任意任务的成功率。`results.json` 中的对话是这次测试自动生成的，不含个人数据。

重跑需要一个运行中的 AgentOS 服务和三个 Agent 的登录环境：

```sh
AGENTOS_TOKEN=<服务令牌> AGENTOS_API=http://127.0.0.1:3080/api/ python3 eval/handoff-30/run.py
python3 eval/handoff-30/audit.py
```

## 范围

这是会话客户端的一部分。当前交接支持 Codex、Claude Code、Antigravity、DeepSeek Harness 和自定义 Devin；仍在执行、等待用户批准或有委派任务的会话不能切换。跨 Agent 交接后的原生 fork 尚不支持。

后续的自动委派、后台目标、复杂协作、多目录工作区和其他本地实验未纳入这次公开扩展。运行时仍使用 Codeg 的基础客户端，其原有功能属于上游。

## 许可

Apache-2.0。保留 [LICENSE](LICENSE) 和 [NOTICE](NOTICE) 中的来源说明。公开内容以 `overlay/` 和接入补丁为准，不包含个人数据库、真实对话、登录信息和本地运行产物。
