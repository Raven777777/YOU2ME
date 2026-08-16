# Y2M 项目代码审查与优化报告

2026/8/16

## 审查计划表（执行结果）

| 优先级 | 文件/模块 | 问题 | 类型 | 风险 | 修改方案 | 验证方式 | 状态 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| P0 | `src/main.rs` 登录 | 仅按用户名限流，10k 用户名表可被批量伪造登录填满，造成全局登录 DoS | 安全 | 中 | 增加 `ConnectInfo` 来源 IP，按“用户名 5 次/分 + IP 30 次/分”双重限流，成功后清空计数 | 单元测试、API 冒烟测试 | 已完成 |
| P0 | `src/main.rs` 会话 | 单账号可无限登录，挤掉其他用户会话 | 安全/可用性 | 低 | 增加 `MAX_SESSIONS_PER_USER = 20`，只淘汰该用户最旧会话 | 新增单元测试 | 已完成 |
| P0 | `src/main.rs` 恢复 | 结构不兼容的 SQLite 备份会让重启后服务无法启动；并发恢复会写坏 staging | 严重可用性 | 中 | 恢复前做完整性 + 必需表/列结构校验、默认管理员冲突校验；增加 `restore_lock` 串行化 | 新增单元测试、API 实测 400 | 已完成 |
| P0 | `src/main.rs` CSRF | 状态变更请求未检查 `Sec-Fetch-Site`，旧浏览器/登录 CSRF 仍可能绕过 Origin 缺失路径 | 安全 | 低 | 对状态变更请求额外拒绝 `Sec-Fetch-Site: cross-site` | 新增单元测试、curl 实测 403 | 已完成 |
| P1 | `src/app.js` WebSocket | 客户端在订阅确认前拉取增量，存在漏消息竞态 | 功能 Bug | 低 | 收到服务端 `subscribed` 确认后再执行 `syncMessagesAfter` | `node --check`、代码路径检查 | 已完成 |
| P1 | `src/main.rs` 发消息 | 过期清理 + 上限检查 + 插入不在事务中，中途失败可能部分提交 | 数据一致性 | 低 | 整个发送流程放入显式事务，仅成功路径提交 | 编译、测试、API 实测 | 已完成 |
| P1 | `src/main.rs` 恢复 | 恢复时直接删除 WAL/SHM，失败回滚会丢失原库未 checkpoint 数据 | 数据一致性 | 中 | 将 WAL/SHM 同步改名为 `.old`，失败时尽量回滚 | 代码检查、编译 | 已完成 |
| P1 | `src/main.rs` 配置 | 非法环境变量被静默忽略；且环境变量在命令行之前解析，违反“CLI 优先”文档 | 配置错误 | 低 | 先解析 CLI，再只对未覆盖项读取环境变量；非法值显式报错；新增 `Y2M_MAX_MESSAGES_PER_ROOM` | 手工验证 CLI 覆盖非法 env | 已完成 |
| P1 | `src/main.rs` 管理鉴权 | 会话存在但用户已不存在时，管理接口返回 500 而非 401 | Bug | 低 | `.optional()` 处理，用户缺失返回 401 | 编译、测试 | 已完成 |
| P1 | `src/main.rs` 注册 | 用 `error.to_string().contains("UNIQUE")` 判断用户名冲突，脆弱 | Bug | 低 | 使用 `rusqlite::ErrorCode::ConstraintViolation` | 编译、测试 | 已完成 |
| P2 | `src/main.rs` 依赖/重复 | `rand 0.8` 与 `rand 0.9` 重复；随机串生成代码重复 4 处 | 重复/依赖 | 低 | 升级到 `rand 0.9`，新增 `random_alphanumeric` 统一生成 | `cargo tree -d`、单元测试 | 已完成 |
| P2 | `src/app.js` 登出 | 重新登录后残留上一个会话的聊天 UI 和消息 | Bug | 低 | 新增 `showEmptyConversation()`，登出/删房后统一重置 | `node --check`、代码检查 | 已完成 |
| P2 | `src/app.js` 消息加载 | `refresh` 参数及增量刷新分支为死代码 | 死代码 | 低 | 删除 `refresh`/`followNewMessages` 死分支，函数简化为 `(reset, older)` | `node --check` | 已完成 |
| P2 | `src/main.rs` 后台清理 | 清理错误被吞掉；极大保留时长可能造成计时器 Duration 溢出 | 可维护性/边界 | 低 | 记录清理错误；清理周期限制在 1 秒～24 小时 | 编译、测试 | 已完成 |
| P2 | `README.md` | 缺少新环境变量和限流说明 | 文档 | 低 | 补充环境变量、限流、恢复校验说明 | diff 检查 | 已完成 |

---

## 1. 发现的问题

### 严重/安全问题
1. **登录全局 DoS**：`MAX_LOGIN_USERS = 10_000` 的 map 满后，所有新用户名登录都会被 429 拒绝；伪造随机用户名可以快速填满。
2. **会话洪泛**：登录成功会清空限流计数，一个有效账号可无限登录，最终把其他用户的会话从全局 `MAX_SESSIONS` 缓存中淘汰。
3. **恢复功能可让服务永久无法启动**：只要上传一个“SQLite 合法但表结构不兼容”的文件，重启后 `init_db`/`ensure_admin` 会失败。
4. **并发恢复竞态**：两个管理员恢复请求会同时写同一个 `.restore` 文件。
5. **恢复失败回滚会丢失原库 WAL**：旧实现直接删除原库 `-wal`/`-shm`，一旦新库 rename 失败，回滚后的原库会丢失未 checkpoint 数据。
6. **CSRF 防御不完整**：无 Origin 时直接放行，主要依赖 SameSite Cookie；现代浏览器的 `Sec-Fetch-Site: cross-site` 未检查，登录 CSRF 场景仍有残余风险。

### 功能 Bug
7. **WebSocket 漏消息竞态**：前端在 `socket.send(subscribe)` 后立即发起 HTTP 增量拉取，若服务端先处理 HTTP 请求、后处理订阅，中间广播的消息会永久丢失。
8. **登出后 UI 状态残留**：重新登录且无上次房间时，仍会显示上一会话的房名和消息。
9. **发消息非事务**：删除过期消息、检查房间上限、插入消息分属不同隐式事务。
10. **管理鉴权**：会话对应用户被删除时返回 500。
11. **用户名冲突检测依赖错误字符串**。
12. **非法环境变量被静默忽略**，且解析顺序不符合 README 声明的“CLI 优先”。
13. **后台消息清理错误被吞掉**。
14. **极大 `-ms` 值可能导致 `tokio::time::interval` Duration 溢出/panic**。
15. **重复依赖与重复代码**：`rand 0.8` 与 axum 引入的 `rand 0.9` 并存；4 处随机串生成逻辑重复。
16. **`loadMessages` 存在从未使用的 `refresh` 死分支**。

### 已有且确认安全的点
- 所有 SQL 使用参数绑定；LIKE 通配符已用 `escape_like` + `ESCAPE '\'` 处理。
- 前端 DOM 均使用 `textContent`，动态用户名进入 `innerHTML` 前经过 `escapeHtml`，未发现 XSS。
- 密码使用 Argon2，未知用户会校验 dummy hash，注册有 IP 限流。
- 会话 Token 为 48 位随机字母数字，Cookie 为 `HttpOnly; SameSite=Strict`。
- WebSocket 握手检查 Origin 与 Host 一致性。
- 未发现路径遍历、SQL 注入、命令注入。

---

## 2. 已经修改的问题

### `src/main.rs`
- 登录增加 `ConnectInfo`，按用户名 + 来源 IP 双重限流，成功登录后同时清空两个维度的计数。
- 新增 `MAX_SESSIONS_PER_USER`，`insert_session` 只淘汰同用户最旧会话，避免单账号挤掉全局会话。
- 恢复接口新增：
  - `restore_lock` 串行化；
  - `PRAGMA integrity_check` + 必需表/列结构校验；
  - “无管理员且默认管理员用户名被普通用户占用”的备份拒绝；
  - 锁保留到重启任务执行，防止后续请求覆盖待恢复文件。
- `apply_pending_restore` 改为同步搬移 WAL/SHM，失败时尽量回滚。
- `send_message` 全流程包进事务，只有成功发送才提交。
- `admin_user_id` 对用户不存在返回 401。
- 用户名冲突改用 SQLite 约束错误码。
- 环境变量解析重构：先解析 CLI，再解析未被覆盖的环境变量；非法环境变量显式报错；新增 `Y2M_MAX_MESSAGES_PER_ROOM`。
- 状态变更请求拒绝 `Sec-Fetch-Site: cross-site`。
- 升级 `rand 0.9`，新增 `random_alphanumeric` 统一随机串生成。
- 后台清理错误记录日志；清理周期 clamp 到 `[1s, 24h]`。
- 新增 7 个回归测试：会话上限、恢复结构兼容、默认管理员冲突、LIKE 转义、随机串长度/字符集、`Sec-Fetch-Site` 检测。

### `src/app.js`
- WebSocket 收到 `subscribed` 确认后才拉取增量，消除漏消息竞态。
- 新增 `showEmptyConversation()`，登出和删除房间后统一清理 UI。
- 删除 `loadMessages` 的 `refresh` 死分支，简化消息加载逻辑。

### `Cargo.toml` / `Cargo.lock` / `README.md`
- `rand` 0.8 → 0.9，新增 `rand_core 0.6 getrandom`（argon2 `OsRng` 所需 feature）。
- README 补充 `Y2M_MAX_MESSAGES_PER_ROOM`、登录限流、恢复结构校验说明。

---

## 3. 未修改的问题及原因

| 问题 | 未修改原因 |
| --- | --- |
| 已登录用户发送消息、WebSocket 帧无频率限制 | 属于产品策略，固定阈值可能误伤正常聊天；建议增加可配置令牌桶。当前认证后即可无限写入，长期运行且有公网暴露时应优先处理 |
| `list_rooms` 全量返回所有房间 | 需要分页/虚拟滚动设计，属功能级改动，本次避免破坏现有前端协议 |
| 备份文件整体读入内存后再返回 | 大库会有内存峰值；可改为流式响应，但需要 `tokio-util` 或分块 Body，属于后续优化 |
| `ensure_admin` 多进程同时启动存在竞态 | 项目定位为单文件单实例；多实例部署需要文件锁或选主，当前按现状保留 |
| 恢复成功后 `current_exe()` 失败会直接退出服务 | 极罕见，但确实会让服务停止；应改为启动失败时保留旧库并返回错误 |
| 会话仅存内存，重启全部失效 | 现有设计如此，README 已说明恢复会清空会话；持久化会话需引入存储/签名 Token 方案 |
| 默认 `Secure Cookie` 关闭 | 兼容本地 HTTP；HTTPS 部署必须显式 `-cook yes` / `Y2M_SECURE_COOKIE=1` |
| `MAX_LOGIN_USERS` 仍可被大规模分布式僵尸网络填满 | 任何有界 map 限流都有此理论上限；本次已把单 IP 攻击成本显著提高，彻底解决需要滑动窗口/固定桶等专门限流结构 |
| 未添加完整 HTTP/WebSocket 自动化 E2E 测试 | 本次用脚本做了真实 HTTP 冒烟测试；WebSocket 竞态修复通过逻辑审查和 JS 语法检查验证，尚未建立自动化 WS 测试 |
| 未执行 CVE 数据库扫描（如 `cargo audit`） | 当前环境无外网；依赖均为当前较新版本 |

---

## 4. 性能优化结果

- `cargo tree -d` 确认应用直接使用的 `rand 0.8` 重复依赖已消除；剩余 `rand_core 0.6/0.9`、`getrandom 0.2/0.3` 分别来自 argon2 与 tungstenite，属于不可简单合并的传递依赖。
- 随机串生成从 4 处重复代码合并为 1 个 helper。
- 删除了前端从未触发的 `refresh` 增量刷新分支。
- WebSocket 增量同步改为确认后执行，避免无效竞态拉取，但未做量化基准测试。
- 发送消息事务化会带来极小写事务开销，但避免了部分提交带来的逻辑复杂性和潜在修复成本；SQLite 单写连接模型下实际影响可忽略。

---

## 5. 安全检查结果

已确认安全：
- SQL 全部参数化，LIKE 转义有单元测试。
- 未发现 XSS、路径遍历、命令注入、任意文件读写。
- 密码 Argon2、登录计时防护、注册限流、WebSocket Origin 校验保持有效。
- 敏感 Token 不落日志；数据库错误只记录服务端 stderr，客户端只看到通用错误。

本次已增强：
- 登录双重限流，降低暴力破解和全局登录 DoS 风险。
- 单账号会话数上限，防止会话缓存被单账号清空。
- 状态变更请求拒绝 `Sec-Fetch-Site: cross-site`，补强 CSRF 防御。
- 恢复文件结构校验，防止“合法 SQLite 但结构不兼容”的备份把服务打挂。
- 恢复流程串行化，防止并发写坏 staging。

仍存在的安全注意点：
- 公网部署务必启用 `Y2M_SECURE_COOKIE=1` 和 TLS；默认关闭 Secure Cookie 是为本地 HTTP 场景。
- 认证用户可无限发消息；若暴露公网，建议增加发送频率限制和房间消息总量默认上限。
- `MAX_LOGIN_USERS=10_000` 与注册 map 一样，理论上仍可被大规模分布式来源填满。
- 共享 NAT 后的登录限流阈值固定为 30 次/分钟，极端共享出口下可能需要调整常量。

---

## 6. 测试/构建结果

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --all-targets --all-features` | 0 警告 |
| `cargo test --all-targets` | 12/12 通过 |
| `cargo build` | 通过 |
| `cargo build --release` | 通过 |
| `node --check src/app.js` | 通过 |
| `git diff --check` | 无空白错误 |
| HTTP 冒烟测试 | 注册、登录、`/me`、房间列表、加入大厅、发消息、查消息全部通过 |
| 管理功能测试 | 管理员登录、读取/更新注册设置、下载备份、无效备份恢复返回 400 |
| 安全测试 | 伪造 Origin POST 返回 403；`Sec-Fetch-Site: cross-site` 返回 403；非管理员访问管理接口返回 403 |
| 边界测试 | 100KB 恢复请求未被 64KB 默认 BodyLimit 误拦（路由级 256MB 生效） |
| 配置测试 | CLI 参数能覆盖非法环境变量；非法环境变量单独使用时以退出码 2 明确报错；`-h` 不受非法环境变量影响 |

---

## 7. 仍然存在的风险

1. **认证后资源滥用**：消息、WebSocket 订阅/查询没有频率限制，公网暴露时可能造成磁盘、CPU 滥用。
2. **限流 map 理论上可被大规模分布式攻击填满**。
3. **大数据库备份的内存峰值**：备份响应一次性读取整个 DB 文件到内存。
4. **多实例启动竞态**：两个进程同时以空管理员库启动，后一个会启动失败。
5. **恢复后重启失败路径**：`current_exe()` 不可用或子进程启动失败时，旧进程仍会退出，服务会中断。
6. **会话仅内存**：重启/恢复后所有用户需重新登录。
7. **系统时间回拨**会影响会话过期和限流窗口（使用墙上时钟 `SystemTime`）。
8. **共享 IP/NAT 环境**：新增 IP 限流在极端共享出口下可能影响正常用户，阈值当前为代码常量。

---

## 8. 后续建议

1. 为消息发送和 WebSocket 操作增加可配置的令牌桶/滑窗限流（建议做成启动参数或环境变量）。
2. 备份下载改为流式响应，避免大库内存峰值。
3. 增加 `cargo audit` / Dependabot 等依赖漏洞扫描。
4. 补充 HTTP API 集成测试和 WebSocket 端到端测试（可用 axum `tower::ServiceExt` + `tokio-tungstenite`）。
5. 若未来支持多实例，引入 SQLite 文件锁或单实例启动锁，并解决 `ensure_admin` 竞态。
6. 为 HTTPS 部署提供默认更安全的配置提示，或在检测到反向代理 TLS 时建议启用 Secure Cookie。
7. 考虑给房间列表加分页/搜索服务端支持，避免超大量房间时全量传输。
8. `src/main.rs` 约 2000 行，后续可拆分为 `config`、`auth`、`rooms`、`messages`、`restore` 模块；本次为避免大范围重构风险未拆分。