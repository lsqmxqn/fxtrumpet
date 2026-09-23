# FxTrumpet

常驻通知区域（托盘）的音频中枢。一个托盘图标，一件程序，三件以前要装三个软件才能做的事：

| 能力 | 来源 | 说明 |
|---|---|---|
| **音频增强** | [fxmini](https://github.com/lsqmxqn/fxmini) | FxSound 自家 DSP 引擎：31 段 EQ、环绕、低音、清晰度、预设库、虚拟声卡 |
| **混音器** | [EarTrumpet](https://github.com/File-New-Project/EarTrumpet) | 每个程序单独调音量 / 静音，每台输出设备单独调音量，切换默认输出 |
| **按程序路由** | [Audio Router](https://github.com/audiorouterdev/audio-router) | 把某个程序的声音钉到指定输出设备上 |

没有主窗口。默认状态下它只是托盘里的一个图标；需要调的时候点开面板或混音器，关掉即释放。

---

## 状态

**骨架已就位，尚未发布。** 分层、接线、界面都已落地并能编译运行；详见 [`docs/合并设计.md`](docs/合并设计.md)，那里也列了下一轮要做的部分（主要是注入式路由）。

---

## 怎么用

托盘图标：

- **左键** 打开调音面板；**右键** 打开菜单。
- 面板里是增强相关的：频率曲线（拖点）、效果旋钮、预设。
- 菜单里的 **混音器与路由…** 是另外两个能力：上面是输出设备，中间是正在出声的程序，下面是已经生效的路由规则。
- 把某个程序的目的地从「跟随系统（经增强）」改成某台设备，就等于把它的声音钉到那台设备上。

> **注意**：被钉到具体设备的程序会**绕过增强器**。增强器装在"系统默认输出"这条路上，而路由把音频从这条路上摘下来了。混音器里这样的行会明确标出「绕过增强」。

命令行：

```
fxtrumpet.exe                  # 正常启动（托盘）
fxtrumpet.exe --panel          # 启动并直接打开调音面板
fxtrumpet.exe --mixer          # 启动并直接打开混音器
fxtrumpet.exe --restore-output # 把系统默认输出还给真实设备后退出（没声音时的救命开关）
fxtrumpet.exe --install-driver # 装虚拟声卡（会弹 UAC）
fxtrumpet.exe --remove-driver  # 卸虚拟声卡
```

---

## 配置

`%APPDATA%\FxTrumpet\config.json`。除了一般的开关和预设选择，路由规则也在这里：

```json
{
  "routes": [
    {
      "app": "exe:C:\\Program Files\\Spotify\\Spotify.exe",
      "display_name": "Spotify",
      "target": { "kind": "device", "endpoint_id": "{0.0.0.00000000}.{...}" },
      "method": "policy",
      "enabled": true
    }
  ]
}
```

`app` 用的是**跨重启稳定的身份**（可执行文件路径，或打包应用的 AUMID），不是进程 id。

---

## 从源码构建

需要 Rust（stable）和 Visual Studio 2022 的 C++ 生成工具（MSVC + Windows SDK）。

```powershell
cd fxtrumpet
cargo build --release
```

`Cargo.toml` 里的依赖已经在本地 `~/.cargo/registry` 缓存过之后，`cargo build --offline` 会快得多——它会跳过 crates.io 索引更新。

FxSound 的 DSP 是 vendored 的 C++，由 `build.rs` 编成静态库；`.cargo/config.toml` 把 CRT 设成静态链接（`+crt-static`），所以产出的 exe 不依赖 VC++ 运行库。

打包（产出版本化的分发件，版本号读自 `Cargo.toml`）：

```powershell
.\package.ps1                 # 构建 + 校验 + 装配，产出 dist\FxTrumpet-<version>-win64.zip
.\package.ps1 -NoBuild        # 复用已有的 target\release\fxtrumpet.exe
```

它会确认 exe 带着版本资源和图标（缺了说明构建时找不到 `rc.exe`），并用 `dumpbin /dependents` 确认没有 VC++ 运行库导入——动态 CRT 的构建在本机跑得好好的，到干净机器上会起不来，所以这一项是硬失败而非警告。

测试：

```powershell
cargo test                                  # 单元测试
cargo test -- --ignored                     # 会真的开窗口，需要桌面会话
```

诊断工具（**只读，不改机器，不需要权限**）：

```powershell
cargo run --release --bin mixerchk   # 会话、应用身份、per-app 端点 API（混音器那一半）
cargo run --release --bin audioenv   # 虚拟声卡是不是默认输出（增强那一半）
cargo run --release --bin audiochk   # 端点 + 预设 + 跑一会儿引擎
cargo run --release --bin dspcheck   # vendored DSP 能不能真的改变信号
```

`mixerchk` 请在**有程序出声的时候**跑：空列表在有声时才是故障，安静时是正确答案。

---

## 许可

AGPL-3.0-or-later，跟上游一致。增强引擎来自 FxSound，见 `vendor/dsp/`。
