# bootwitness

[![CI](https://github.com/FibreCase/bootwitness/actions/workflows/ci.yml/badge.svg)](https://github.com/FibreCase/bootwitness/actions/workflows/ci.yml)

`bootwitness` 是一个面向 Linux/systemd 服务器的开机连续性监测器。它将 Linux
内核的 boot ID、`CLOCK_BOOTTIME` 和持久化心跳写入 SQLite，用于发现重启、断电、
内核崩溃以及监测进程自身的中断。

目标部署平台是 Debian 13 amd64。核心逻辑和数据库测试可以在 macOS 上运行，但真实
的 `daemon` 和 `check` 命令只支持 Linux。

## 判定模型

每次 Linux 启动都会产生新的
`/proc/sys/kernel/random/boot_id`。守护进程启动时将当前 boot ID 与上一次持久化状态
比较：

- boot ID 变化：记录 `boot_changed`，代表 Linux 运行连续性中断。
- boot ID 不变但守护进程重新启动：记录 `monitor_restarted`，不误报为 Linux 重启。
- 同一次启动中 `CLOCK_BOOTTIME` 的心跳间隔超过阈值：记录 `heartbeat_gap`。
- 墙上时钟或 `CLOCK_BOOTTIME` 倒退：记录 `clock_anomaly`。

断电后无法恢复精确的断电秒数。`boot_changed` 保存一个证据窗口：

```text
上一次成功落盘的心跳时间 .. 新内核的估算启动时间
```

默认每 60 秒落盘一次，因此旧系统最后存活时间的误差上限通常约为一分钟。正常收到
SIGTERM/SIGINT 时，事件会标注 `graceful=true`；没有观察到正常停止时标注为
`graceful=false`。后者可能来自断电、内核崩溃、强制复位，或守护进程当时不可用，
不能仅凭这一项证明具体根因。

## 构建与测试

项目使用 Rust 2021 edition，最低支持 Rust 1.85。

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo build --locked --release
```

在 macOS 上，测试通过模拟 boot ID 和时间推进验证状态转换，不读取或伪造 Linux
的 `/proc` 数据。

## Debian 13 安装

在 Debian 13 amd64 主机上安装 Rust 工具链并构建 release 二进制，然后运行：

```bash
sudo ./packaging/debian/install.sh
```

安装脚本会：

1. 安装二进制到 `/usr/bin/bootwitness`。
2. 安装 unit 到 `/usr/lib/systemd/system/bootwitness.service`。
3. 启用并立即启动服务。

也可以安装 `cargo-deb` 后构建 `.deb`：

```bash
cargo install cargo-deb
cargo deb --locked
sudo apt install ./target/debian/bootwitness_0.1.0-1_amd64.deb
sudo systemctl enable --now bootwitness.service
```

systemd 使用 `DynamicUser=yes` 和 `StateDirectory=bootwitness`。数据库位于：

```text
/var/lib/bootwitness/bootwitness.sqlite3
```

管理员通常需要通过 `sudo` 读取该数据库。

## 使用

查看服务和日志：

```bash
systemctl status bootwitness
journalctl -u bootwitness
```

查看当前状态：

```bash
sudo bootwitness status
sudo bootwitness status --json
```

查看事件：

```bash
sudo bootwitness history
sudo bootwitness history --kind boot_changed
sudo bootwitness history --limit 100 --json
```

健康检查：

```bash
sudo bootwitness check --max-heartbeat-age 180
```

退出码约定：

- `0`：当前 boot ID 一致、监测状态为 active、心跳未过期。
- `1`：健康条件不满足。
- `2`：命令参数、平台、权限或数据库错误。

手工运行时可以使用其他数据库路径：

```bash
sudo bootwitness --database /tmp/bootwitness.sqlite3 daemon --interval 10
```

同一个数据库只允许一个 daemon 持有非阻塞文件锁。

## systemd 参数

默认 unit 使用：

```text
--interval 60
--gap-factor 3
```

因此心跳超过 180 秒时生成 `heartbeat_gap`。如需修改，运行
`systemctl edit bootwitness` 覆盖 `ExecStart`，随后执行：

```bash
sudo systemctl daemon-reload
sudo systemctl restart bootwitness
```

## Debian 验收建议

首次启动不应产生事件：

```bash
sudo systemctl start bootwitness
sudo bootwitness history
```

杀死守护进程以验证 systemd 恢复；该操作应产生 `monitor_restarted`，但不能产生
`boot_changed`：

```bash
sudo kill -9 "$(systemctl show -p MainPID --value bootwitness)"
sleep 6
sudo bootwitness history
```

完成一次重启后应恰好增加一个 `boot_changed`：

```bash
sudo systemctl reboot
# 重新登录后
sudo bootwitness history --kind boot_changed
```

强制断电测试具有破坏性，只应在允许进行断电测试的机器上执行。恢复后对应事件通常为
`graceful=false`。

## 数据与卸载

SQLite 使用 WAL 和 `synchronous=FULL`，启动状态转换和事件写入位于同一事务中；
事件指纹带唯一约束，防止重试生成重复的启动事件。数据默认永久保留。

卸载程序时建议先停止并禁用服务，再删除二进制和 unit。数据库不会自动删除，以避免
丢失历史记录：

```bash
sudo systemctl disable --now bootwitness
sudo rm /usr/bin/bootwitness /usr/lib/systemd/system/bootwitness.service
sudo systemctl daemon-reload
```
