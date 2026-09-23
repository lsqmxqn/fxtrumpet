# FxMini 设计方案

> 目标：一个**纯托盘常驻、低占用**的 Windows 音频增强工具，复用 FxSound 的虚拟声卡驱动与 DSP 引擎，但不带 FxSound 的 App 界面，点击托盘图标弹出自己的调音小面板。

---

## 1. 结论先行

**可以，而且比预期容易得多。** 关键发现是：`fxsound2/fxsound-app` 已经以 **AGPL-3.0** 开源了整套东西——不只是 GUI，而是**完整的 DSP 引擎源码**（`dsp/`，约 9000 行 C++，225 个文件）和**完整的 WASAPI 回环层**（`audiopassthru/`）。

这意味着：

- 你不需要自己逆向 FxSound 的音效算法
- 你不需要自己调参去逼近 FxSound 的音色
- 你调出来的声音和 FxSound 官方**逐位一致**（同一份 DSP 代码 + 同一套 `.fac` 预设）

DSP 引擎的公开 API 只有 79 行、约 30 个方法，封装得非常干净。

---

## 2. 三个仓库的真实分工

| 仓库 / 目录 | 内容 | 你怎么处理 |
|---|---|---|
| `fxsound-driver/fxvad` | 虚拟声卡驱动（微软 MSVAD 示例衍生），纯搬运，**零 DSP** | **直接分发** FxSound 已签名的 `inf`/`sys`/`cat` 三件套，不要自己编译 |
| `fxsound-app/dsp` | **DFX DSP 引擎**（Legacy DFX 血统），C++，干净 API | ★ **核心复用对象** |
| `fxsound-app/audiopassthru` | `sndDevices` 设备管理层：枚举、默认设备切换、回环采集、重采样 | ✗ **不建议复用**。Legacy Win32 代码，耦合 `MRY`/`MTH`/`pstr`/`reg`/`SLOUT` 一堆自研工具库，可读性和可移植性都差。自己写更省事 |
| `fxsound-app/fxsound` | JUCE GUI，含 `FxSystemTrayView`、`FxLiteView`、`FxEqualizer` | 当**参考实现**读，或走路线 A 直接 fork |
| `fxsound-app/fxmcp` | Go 写的 MCP server，可编程控制 FxSound | 可参考它的控制面设计 |

### 一个必须澄清的误解

`fxvad.sys` **不做任何音效处理**。它只是创建一个虚拟播放端点。音频链路是：

```
系统默认输出 = 虚拟声卡 → 你的进程用 WASAPI 回环抓 → DSP → 渲染到物理声卡
```

`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:551` 是整条链路的核心调用：

```cpp
p_dfx_dsp_->setSignalFormat(pwfx->wBitsPerSample, pwfx->nChannels, pwfx->nSamplesPerSec, i_valid_bits);
p_dfx_dsp_->processAudio((short int *)fp_buffer, (short int *)fp_buffer, numSampleSets, i_check_for_duplicate_buffers);
```

注意**输入输出是同一块 buffer**——就地处理。

---

## 3. 三条路线对比

| | A. 精简 fork JUCE GUI | **B. Rust 壳 + C++ DSP（推荐）** | C. 纯 Rust 重写 DSP |
|---|---|---|---|
| 做法 | 拿 `fxsound/` 删掉多余界面，只留托盘+小窗 | Rust 写托盘/驱动/回环，DSP 用 FFI 复用 | 把 10 段 EQ + 混响 + 环绕全部移植成 Rust |
| 内存占用 | ~40–80 MB（JUCE） | **~10–20 MB** | ~5–10 MB |
| 工作量 | 1–2 天 | 1–2 周 | 3–6 周 |
| 音色一致 | 完全一致 | 完全一致 | 不可能一致，只能逼近 |
| 调音界面 | JUCE 现成控件 | 需自绘（egui） | 需自绘 |
| 风险 | 低 | 中（FFI + 构建） | 高 |

**推荐路线 B。** 你要的是"低功耗"，A 的内存开销和 FxSound 本体没区别，等于白折腾；C 的工作量不划算且会丢掉 FxSound 预设生态。B 是唯一同时满足"轻"和"音色一致"的。

---

## 4. 路线 B 的推荐架构

### 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| 宿主语言 | **Rust** | 无 GC、无运行时、单文件可执行 |
| DSP | **C++ `dsp/` 静态库 + `extern "C"` shim** | 复用官方算法，避免重写 |
| 音频 API | `windows` crate（`Win32_Media_Audio`） | WASAPI 直接调，无中间层 |
| 托盘 | `tray-icon` crate | 纯 Win32，无 WebView |
| 调音界面 | `egui` / `eframe`（点托盘才创建窗口） | 原生 GPU 绘制，无 WebView；窗口销毁后内存回落 |
| 配置 | `serde` + `serde_json` | |
| 热键 | `global-hotkey` crate | 切预设想做成快捷键 |
| 日志 | `log` + `simplelog` | |

**明确不要用的**：Tauri / Electron / WebView2。NexBox 之所以重，很大一部分就是 WebView2 常驻 100 MB+，跟 FxSound 一点关系没有。

### 模块划分

```
fxmini/
├── Cargo.toml
├── build.rs                  # 调 cc crate 编译 C++ DSP
├── capi/
│   ├── dfxdsp_capi.h         # 给 Rust 的 C ABI
│   └── dfxdsp_capi.cpp       # 包 DfxDsp.h 的 C++ class
├── vendor/
│   └── dsp/                  # 从 fxsound-app 复制过来的 dsp/ 子树
└── src/
    ├── main.rs               # 单实例、托盘、事件循环
    ├── ffi.rs                # C ABI 的 Rust 绑定
    ├── driver.rs             # 驱动检测 / 安装 / 卸载
    ├── device.rs             # IPolicyConfig 切默认设备 + IMMNotificationClient
    ├── engine.rs             # 音频线程：回环采集 → DSP → 渲染
    ├── preset.rs             # .fac 解析 / 保存 / 导入导出
    ├── config.rs             # 设置持久化
    └── ui/
        ├── tray.rs           # 托盘菜单
        └── panel.rs          # 调音小面板（10 段 EQ + 5 个音效滑块）
```

### 线程模型

```
主线程      Tauri 无，纯 Win32 消息循环（托盘 + 窗口）
音频线程     THREAD_PRIORITY_TIME_CRITICAL + AvSetMmThreadCharacteristics("Audio")
            ├─ 事件驱动：SetEventHandle + WaitForSingleObject（不用 Sleep 轮询）
            ├─ 回环采集（AUDCLNT_STREAMFLAGS_LOOPBACK）
            ├─ dfxdsp_process_f32(...)   ← 就地处理
            └─ 渲染到物理设备
UI 线程     仅创建面板时存在，关闭即销毁
```

DSP 参数通过 `Arc<RwLock<DspParams>>` 从 UI 线程传给音频线程，音频线程每帧取一次快照——**绝不在音频线程里加锁等待或分配内存**。

---

## 5. DSP 的 C ABI 封装（关键环节）

`DfxDsp.h` 是 C++ class，带 `std::wstring` 参数，Rust 无法直接绑定。必须写一层 `extern "C"` 薄壳，见 `capi/dfxdsp_capi.h`。

### 三个必须记住的实现细节

1. **样本格式是 float32，不是 int16。**
   签名写的是 `short int *`，但源码注释和实际调用都是 `// Format will always be 32 bit floating point`——FxSound 自己就是把 float buffer 强转成 `short*` 传进去的。所以 `setSignalFormat` 传 `bps=32`，直接传 WASAPI 的混音格式 buffer，**零格式转换**。C ABI 里我用 `float*` 暴露，把这个历史包袱藏起来。

2. **`i_num_sample_sets` 是帧数，不是总采样数。**
   依据：`audiopassthru/src/sndDevices/sndDevicesDoCapture.cpp:385`，`*ip_numSampleSets = cast_handle->capturedFramesCount;`。10 帧立体声 float32 = 20 个 float，传 `10`。

3. **`setSignalFormat` 首次会返回失败，属正常。**
   源码里明确注释了这个行为，FxSound 自己也只是打日志不中断。别当成错误处理，更别弹窗。

4. **`isPowerOn()` 是反的。**
   `DfxDspPrivate.cpp:216` 里它读的是 BYPASS 按钮：`if (value != 0) return true`——也就是**被旁通时返回 true**。而 `powerOn(true)` 把 BYPASS 设为 0。所以 API 自己的 getter 和 setter 互相矛盾。C ABI 里已经反转修正，并加了断言式的回归检查。

5. **音效的 getter 和 setter 值域不对称。**
   这是上游设计，不是笔误（`DfxDspPrivate.cpp:231` vs `:254`）：

   | | 值域 | 内部 |
   |---|---|---|
   | `getEffectValue()` | **0.0 – 1.0** | 归一化值 |
   | `setEffectValue()` | **0.0 – 10.0** | 存 `value / 10` |

   `.fac` 里的 `Main`（MIDI 0–127）由加载器直接写进那个归一化字段，所以
   `Main / 127 == getEffectValue()`，而要经 setter 还原则是 `Main / 12.7`。
   实测 `Music.fac` 的 `Main 0 = 50` → 读出 0.394 → 写回 3.94 → 又是 50，完美往返。

6. **DSP 引擎不是自包含的。**
   `dsp/` 单独编出来是不够的：它会引用上游辅助层的 `reg*` / `mth*` / `pstr*` / `file*` 函数，缺了会报 **14 个** LNK2019。这些实现位于 `audiopassthru/src/{FILE,MRY,MTH,pstr,ptime,reg,SLOUT,operatingSystem}`——33 个编译单元。`build.rs` 把它们编成第二个静态库 `dfxutil`。

   但**不要**把 `audiopassthru` 整个编进来：其中的 `src/AudioPassthru` 和 `src/sndDevices` 是设备层（WASAPI 采集/播放、设备枚举），FxMini 自己实现音频环，这部分是死代码还会拖进它自己的依赖网。

---

## 5.1 编译所需的宏定义（照抄上游，一个都别多）

| 库 | 宏 |
|---|---|
| `dfxdsp`（dsp/） | `NDEBUG` `_LIB` `WIN32` `PT_NON_MFC` `DSPSOFT_TARGET` `PT_DSP_BUILD=PT_DSP_DFX` `UNICODE` `_UNICODE` |
| `dfxutil`（audiopassthru 辅助层） | `NDEBUG` `_LIB` `WIN32` `UNICODE` `_UNICODE` |

三个必须说明的点：

- **`DSPSOFT_TARGET` 不能少。** 缺了它 `boardrv1.h:48` 直接 `#error PC_TARGET or DSP_TARGET or DSPSOFT_TARGET not defined`，一个文件都编不过。
- **`UNICODE` / `_UNICODE` 来自 MSBuild 的 `<CharacterSet>Unicode</CharacterSet>`，不在 `<PreprocessorDefinitions>` 里。** 只看后者会漏掉它们，然后通用 Win32 宏解析成 ANSI 版本，宽字符串调用点全部 C2664 (`wchar_t*` → `LPCSTR`)。
- **不要自作聪明加 `WIN32_LEAN_AND_MEAN`。** 它会把 `objbase.h`（以及整个 OLE）从 `windows.h` 里剔出去，于是 `pstr.cpp` 里的 `CoCreateGuid` / `StringFromGUID2` 变成 C3861 找不到标识符。上游没定义它是有原因的。

链接还需要 `ole32` `winmm` `user32` `advapi32` `shell32` `shlwapi`（最后一个是为了 `PathFileExistsW`）。

---

## 6. 六个必须绕过的坑

### 坑 1：驱动签名（最硬的门槛）

Windows 10 1607 起，开启 Secure Boot 后强制要求内核驱动数字签名。你自己用 WDK 编译出的 `fxvad.sys` **装不上**。

**做法**：直接分发 FxSound 官方签过名的 `fxvad.inf` / `fxvad.sys` / `fxvadntamd64.cat`。NexBox 就是这么干的（`src-tauri/resources/binaries/fxvad/`）。

需要注意：签名用的是 FxSound 的证书，若证书过期或被吊销，新系统上可能安装失败——要准备好降级提示。

### 坑 2：装完驱动，用户突然没声音了

`Root\FXVAD` 设备创建后，Windows 常会自动把默认播放设备切到它。此时音频进了虚拟声卡却没人接，结果是**静音**。

**做法**：安装前先用 `IMMDeviceEnumerator::GetDefaultAudioEndpoint` 记录当前默认设备名，装完用 `IPolicyConfig::SetDefaultEndpoint`（未公开但长期稳定，手工构造 vtable 调用）切回去。NexBox 在 `install_virtual_audio_driver()` 里正是这么处理的。

顺带一提：FxSound 自己的析构函数里还专门处理了「切换设备时虚拟驱动会把音量状态重置并传播到真实音箱」，见 `AudioPassthruPrivate.cpp` 的 `ignoreVolumeCallbacks`。

### 坑 3：单声道设备会让 DSP 崩溃

`AudioPassthruPrivate.cpp:545` 有一段显眼的 2016 年遗留补丁：蓝牙耳机等单声道播放设备会导致崩溃，FxSound 的做法是**直接跳过 DSP 处理**。

**做法**：拿到设备格式后先判 `nChannels == 1`，是就 bypass，别送进 DSP。

### 坑 4：虚拟设备和物理设备采样率不一致

这是 FxSound 用 `audiopassthru` 里整套 `upsampleRatio` 逻辑解决的问题（`sndDevicesDoCapture.cpp:102` 还在专门防御 ratio 越界的蓝牙场景）。NexBox 则选择了偷懒——`audio_engine.rs` 里只是 `warn!` 一句就继续跑，这会导致变调或杂音。

**做法**：二选一
- 简单版：启动时检测两端采样率，不一致就提示用户或在虚拟设备侧强制统一（48 kHz 是安全默认值）
- 正确版：内置一个固定倍率重采样器（线性或 Sinc 插值），在采集侧补齐

### 坑 5：设备热插拔 / 全屏切换 / UAC 弹窗

这些都会触发设备变更，链路一旦断裂就是静音。

**做法**：注册 `IMMNotificationClient`，监听 `OnDefaultDeviceChanged` / `OnDeviceStateChanged`，收到回调后**不要立刻在回调线程里重建**，投递到自己的管理线程做停止→重新枚举→重建，中间加状态机防重入。NexBox 的 `audio_engine.rs` 有可参考的失败重试节奏。

### 坑 6：不要用 Sleep 轮询

`audiopassthru` 的老代码里是 `Sleep(1)` 死循环轮询（`sndDevicesDoCapture.cpp:99`），CPU 占用虽然不高但唤醒频繁，笔记本上白耗电。你要做"低功耗"，就该用 `IAudioClient::SetEventHandle` + `WaitForSingleObject` 做事件驱动，让线程真正睡着。

---

## 7. `.fac` 预设格式

纯文本、**CRLF 换行**、按行号定位的固定结构。以 `Music.fac` 为例：

```
CLASS1 : Effect Type          ← 行 0
9: Version
音乐                           ← 行 2 = 预设名称
0: Double Params Flag
1: Total number of elements
50: Main 0                     ← 行 5  = Fidelity (清晰度)
35: Main 1                     ← 行 6  = Surround (环绕/宽度)
0:  Main 2                     ← 行 7  = 未使用
35: Main 3                     ← 行 8  = Ambience (环境/混响)
20: Main 4                     ← 行 9  = DynamicBoost (动态增强)
60: Main 5                     ← 行 10 = Bass (低音)
...
7: Number of Application Dependent Integers
...
1: Integer[0]                  ← 行 22 起，五个音效开关
1: Integer[1]
1: Integer[2]
1: Integer[3]
1: Integer[4]
0: Integer[5]
2: Integer[6]
10: Number of EQ Bands
1: On/Off Flag                 ← 之后进入 EQ 段
Band 1
   62.5: CF                   ← 中心频率 (Hz)
   0: Boost/Cut               ← 增益 (dB, -12 ~ +12)
...  共 10 段
```

**Main 值的映射**（对应 `DfxDsp::Effect` 枚举）：

| .fac 字段 | Effect 枚举 | 范围 | 语义 |
|---|---|---|---|
| Main 0 | `Fidelity = 0` | MIDI 0–127 | 清晰度 |
| Main 1 | `Surround = 1` | MIDI 0–127 | 环绕宽度 |
| Main 2 | — | — | 保留未用 |
| Main 3 | `Ambience = 2` | MIDI 0–127 | 环境混响（Dattorro plate） |
| Main 4 | `DynamicBoost = 3` | MIDI 0–127 | 动态增强 |
| Main 5 | `Bass = 4` | MIDI 0–127 | 低音增强 |

注意 `DfxDsp::setEffectValue(Effect, float)` 与 `getEffectValue(Effect)` 值域不同（详见 5.5），换算关系：

```
读出（0.0–1.0）  = Main / 127.0
写入（0.0–10.0） = Main / 12.7
```

不要自己写解析器——**直接调 `DfxDsp::loadPreset(宽字符路径)`**，让官方代码去解析。只有做预设列表展示、导入导出、UI 编辑器时才需要自己读文件。

反过来说，实测 `loadPreset` 之后 `getNumEqBands()` 返回的是**引擎当前的段数（默认 31）**，而不是 `.fac` 里声明的段数（那 8 个内置预设都是 10 段）。预设的曲线会被映射到 31 段网格上。做 UI 时如果直接照搬 `.fac` 的段数，会和引擎状态对不上——**以 `getNumEqBands()` 为准**。

### 现成的预设资源

- `fxsound-app/bin/BonusPresets/` —— 20+ 个官方与社区预设（`70's.fac`、`Classical.fac`、`Metal.fac`、`Trap.fac` 等）
- `NexBox/src-tauri/resources/binaries/fxvad/presets/` —— 8 个精简版（`Gaming.fac`、`Music.fac`、`Movie.fac`、`BassBoost.fac` 等）

这些都可以直接作为你的内置预设分发。

---

## 8. 里程碑拆分

| 阶段 | 交付 | 验收标准 |
|---|---|---|
| M0 | 环境 | ✅ 完成。MSVC 工具集 + Windows SDK 定位脚本就位，`vendor/dsp` 与辅助层编出两个静态库 |
| M1 | DSP 打通 | ✅ **通过**。离线跑真实 `.fac` 预设，94+33 个单元编译链接零错误，信号被改变 +4.42 dB；退出析构的崩溃已定位并修复（见下），连续 14 次运行退出码全 0 |
| M2 | 音频链路 | ✅ 完成。回环采集 → DSP → 渲染真机验证：1 kHz 正弦输入 0.1 → `peak 0.200`（约 +6 dB，与预设一致，说明是处理不是直通）；`drop 0`、`clipped 0`。补上了设计里漏掉的**默认输出接管**（见「坑 2」）|
| M3 | 托盘 | ✅ 完成。托盘常驻 15.9 MB（< 20 MB），空闲 CPU 1.25%，右键切预设、开机自启 |
| M4 | 面板 | ✅ 完成。点托盘弹出调音小窗（egui）。事件循环是**进程级单例**，所以面板跑在常驻线程上，关闭时释放 GL 上下文与字体图集 |
| M5 | 健壮性 | ✅ 基本完成。采样率错配（内建重采样 + 配置开关）、单声道 bypass 均有单测；热插拔走 `IMMNotificationClient`；崩溃恢复＝接管前把被顶掉的端点落盘，下次启动自动归还。热插拔真机场景（拔/插设备、驱动装/卸）与驱动安装路径仍需管理员，未实机执行 |
| M6 | 打包 | ✅ 完成。`package.ps1` 产出 `FxMini-<version>-win64.zip`：exe（含图标/版本资源、静态 CRT）+ 驱动三件套 + `install.ps1`/`uninstall.ps1` + 许可 + `SHA256SUMS.txt`。单文件分发而非 MSIX——没有可用的打包工具链，且 per-user 安装本来就不需要 MSIX |
| M7 | 双语界面 + 面板内保存预设 | ✅ 完成（v0.1.1）。文案表集中在 `src/i18n.rs`，默认跟随系统显示语言，手动切换后记入 `config.json`；切换语言**就地改写**菜单文字，不重建托盘。面板底部可把当前设置存成 `.fac`，写盘在音频线程，结果经 `presets_revision` 回传 |
| M8 | 界面整修 | ✅ 完成（v0.1.1 一并发布）。设计令牌（`src/ui/theme.rs`）+ 跟随系统浅深色；均衡器由 31 行滑块改为可拖拽曲线；控制卡片两列排布、窗口按内容自适应高度；托盘菜单重新分组并支持左键打开面板。取舍与踩坑见[开发笔记](开发笔记.md)第 6 节 |
| M9 | 实测问题修复 | ✅ 完成（v0.1.2）。一轮 debug 实测暴露的四个问题：控制台窗口、预设下拉「跳动」、频段选项留白、面板缺圆角。其中「跳动」是**两个独立成因**叠加（行高随悬停变 2 pt + 视口不是整数行），见[开发笔记](开发笔记.md)第 7 节 |
| M10 | 自绘标题栏 | ✅ 完成（v0.1.3）。系统标题栏在 Windows 10 上没有可调的余地（`DWMWA_CAPTION_COLOR` 要 build 22000 起，按钮画在非客户区），所以改用 `with_decorations(false)` 由面板自绘，见[开发笔记](开发笔记.md)第 8 节。代价是整块非客户区都要自己补：移动、最小化 / 最大化 / 关闭、双击最大化、缩放边框、最大化撤圆角、窗口描边 |
| M11 | 窗口边缘修复（v0.1.4） | ✅ 完成。用户实测报「最左侧和顶部有约 2–3px 的横条」。逐像素量出是**三个成因叠加**：winit 为无边框窗口的投影标记把客户区下移 1px、`WS_EX_WINDOWEDGE` 被 winit 留下且无法清除、自绘描边 `shrink(0.5)` 把 1 pt 摊到两个像素。另修掉 egui `Panel` 默认画出的三条分隔线。见[开发笔记](开发笔记.md)第 9 节 |
| M12 | 托盘语言对勾修复（v0.1.5） | ✅ 完成。用户实测报：在中文状态下切到英文，中文的对勾不消失（两项同时带勾）；再次点击英文，反而把它自己的对勾去掉。成因是语言条目的对勾从未被刷新，且 `set_language` 对「当前语言」提前返回，把系统已切换的对勾留在屏幕上。见[开发笔记](开发笔记.md)第 10 节 |
| M13 | 均衡器频段数恢复到 31（v0.1.6） | ✅ 完成。用户实测报：频段数下拉只有 5 和 10，问能不能加到 31；并提到「之前此项工作因为 10 以上的为空白所以删除了」。查下来空白的原因**不是**引擎没有频率表（15/20/31 各有一张完整表，31 段即 20 Hz–20 kHz 的 ISO 栅格），而是**换频段数之后没人把重算出来的频率读回来**——只有 `apply_preset` 做这件事，音频线程上的 applier 不做，于是 `band_freq` 恒为 0，绘制曲线的函数直接返回空。补上回读即可，下拉恢复到 5/10/15/20/31。见[开发笔记](开发笔记.md)第 11 节 |

M1 已经跑完，`dspcheck` 的实际输出：

```
engine created, 5 effect slots reported
preset name      : 音乐           ← .fac 第三行的名字，证明 loadPreset 真的解析了
power            : true           ← 验证了 isPowerOn 反转修正
EQ bands         : 31
effects
                  get 0-1   set 0-10  fac Main
  Fidelity          0.394       3.94        50   ← 与 Music.fac 的 Main 0 = 50 完美往返
signal check (1 kHz sine at -6 dBFS)
  RMS in        : 0.353553
  RMS out       : 0.588367
  change        : +4.42 dB
```

M1 的价值在于它是唯一能快速证伪风险的一步：不碰驱动、不碰 WASAPI，纯离线处理一段正弦波。**如果这一步不过，后面全白搭。**

### M1 收尾时抓到的一个上游崩溃

第一次跑通后，`dspcheck` 约 **80% 的运行会在退出时崩溃**（退出码 139，Windows 访问违例）。日志显示每次都先打印 `releasing engine...`，说明炸点在析构里，而不是处理路径上。

根因在 `dsp/DfxDspPrivate.cpp`：`DfxDspPrivate` 声明了四个指针成员，构造函数只初始化了三个——`preset_list_handle_` 被漏掉；而析构函数偏偏对它调用了 `prelstFreeUp(&preset_list_handle_)`。`data_ = new DfxDspPrivate()`（`DfxDsp.cpp:26`）不会零初始化，这个成员拿到的是堆槽里的残留值，于是析构时把一个野指针交给释放函数。

修法是在构造函数补一行 `preset_list_handle_ = NULL;`。该成员在整个工程里再无任何赋值点（它对应的预设列表功能已被上游删除，只剩析构里的残留调用），所以置空不丢任何东西。这行由 `vendor.ps1` 按唯一锚点幂等施加，完整诊断见 [`patches/README.md`](../patches/README.md)。

打完补丁后连续 14 次运行退出码全 0。**这一步值得单独记录**，因为 DSP 引擎的析构里有十几个手工 `free`，"功能全对、退出才崩"是这类漏初始化的典型形态；M2 把引擎放进常驻进程后，析构路径会被反复执行，问题只会更明显。


---

## 9. 许可提示

`fxsound-driver` 和 `fxsound-app` **都是 AGPL-3.0**（NexBox 自己标 GPL-3.0，两者不兼容，NexBox 那个仓库本身就有许可问题）。

对你的影响：

- **纯自用**：无所谓，随便改
- **要发布**：AGPL 的传染性意味着你的 FxMini 也必须以 AGPL-3.0 开源，包括通过网络提供服务的情形
- 另外，再分发 FxSound 签名的驱动二进制、使用 "FxSound" 名称和图标，涉及商标与证书，需要另行注意——别把产品直接叫 FxSound 或 FxMini 对外发布（内部代号无所谓）

---

## 10. 本机环境现状（已实测打通）

| 项 | 状态 |
|---|---|
| Rust / Cargo | ✅ 1.94.1（host `x86_64-pc-windows-msvc`） |
| MSVC 工具集 | ✅ `D:\Program Files\VisualStudio\VC\Tools\MSVC\14.42.34433` |
| Windows SDK | ✅ `C:\Program Files (x86)\Windows Kits\10`（10.0.26100.0） |
| 编译 DSP 引擎 | ✅ 94 个编译单元全部通过 |
| 编译辅助层 | ✅ 33 个编译单元全部通过 |
| Rust 链接 | ✅ 通过 |

### 四个环境陷阱（都已解决）

**1. Visual Studio 没注册到系统。**
装在 `D:\Program Files\VisualStudio`，但 `vswhere.exe` 返回空、注册表里也没有 `HKLM\...\VisualStudio\SxS\VS7` 条目。后果是 `cargo` 找不到 `link.exe`，`cc` crate 找不到 `cl.exe`——**连 hello world 都链不出来**。

**2. Git for Windows 自带一个同名的 `link.exe`。**
位于 `/usr/bin/link.exe`，它是 coreutils 的硬链接工具，不是链接器。PATH 顺序不对时 rustc 会调到它，报出极具误导性的：

```
link: extra operand 'xxx.rcgu.o'
Try 'link --help' for more information.
```

**3. 工具集有个残缺版本。**
本机有两个：`14.42.34433`（完整）和 `14.50.35717`（有头文件，**没有 `lib\x64`**）。按"取最新"的直觉会选中后者，然后在链接阶段莫名其妙地失败。

**4. PowerShell 5.1 把原生命令的 stderr 当成终止性错误。**
在 `$ErrorActionPreference='Stop'` 下，PowerShell 5.1 会把**重定向后的原生命令 stderr** 包装成 `NativeCommandError` 并终止。这会造成两个很费解的现象：

- `toolchain.ps1` 里探测 `cl.exe` 版本那一行（版本横幅写在 stderr、且无输入时以非零码退出）会直接把脚本打断；
- 更常见的是 `. .\toolchain.ps1; cargo build`：点源（dot-source）会在**调用方作用域**里留下 `Stop`，紧接着 `cargo` 自己的进度输出（`Compiling ...`，走 stderr）就让构建"莫名其妙失败"。

**解法**：`toolchain.ps1` 在开头保存、结尾还原 `$ErrorActionPreference`，探测 `cl.exe` 时单独临时放宽；`build.ps1` 再把两者包成一条命令。

**解法（1–3）**：`toolchain.ps1` / `toolchain.sh`。它们自动挑选**真正含有所需目录**的版本，并把 MSVC 的 bin 目录前置到 PATH 最前面。用法：

```powershell
.\build.ps1                 # 一条命令：工具链 + cargo（推荐）
```

```powershell
. .\toolchain.ps1           # 或手工两步
cargo build --release --bin dspcheck
```

```bash
source toolchain.sh
```

### 一个输出编码的小坑

`.fac` 预设名是 UTF-8（`Music.fac` 名字行的字节是 `E9 9F B3 E4 B9 90`，即「音乐」）。但 Windows 控制台默认停在 ANSI 代码页（本机 936），于是 Rust 写出的 UTF-8 字节被按 GBK 显示成「闊充箰」——**引擎读到的名字始终正确，只有终端在骗人**。`dspcheck` 启动时调 `SetConsoleOutputCP(65001)` 把它掰正，避免把中文预设名误判成解析 bug。

### 一个遗留的小杂物

`vendor/audiopassthru_include/` 是早期版本的产物（当时只拷贝 `audiopassthru/include`）。现在整个 `audiopassthru` 工程都镜像到 `vendor/audiopassthru/` 了，所以那个目录**已不被任何地方引用**，可以手动删掉。`vendor.ps1 -Prune` 也会清理它——但仅在删除功能可用的环境下，因为部分沙箱环境对批量删除会 fail-closed。

---

## 11. M6 打包

`package.ps1` 一条命令产出 `dist/FxMini-<version>-win64.zip`。四个决定值得写下来。

### 11.1 为什么是「便携目录 + zip」而不是 MSIX 或安装器

- 本机没有 WiX / Inno Setup / NSIS，引入任意一个只为了打一个 per-user 安装包不划算；
- MSIX 需要签名证书，且对「安装内核级虚拟声卡驱动」这种越界操作并不友好（它的沙箱模型与 `pnputil` + root-enumerated devnode 相冲）；
- per-user 安装真正需要的东西只有三样：把文件放到用户目录、建快捷方式、启动。一个 `install.ps1` 就够，而且它可读、可审计、不额外引入依赖。

`install.ps1` / `uninstall.ps1` **不碰驱动**。装驱动需要管理员权限，而这条流程（记录当前默认设备 → `pnputil /add-driver /install` → 创建 root devnode → 把默认设备还回去）已经完整地存在于 `driver.rs` 中，并且和托盘菜单、UAC 重入、崩溃标记共用一套代码。在安装脚本里再写一遍，等于养两份都必须记住「装完把默认输出还回去」的实现。

### 11.2 静态 CRT：让「单文件」成立

默认的 `x86_64-pc-windows-msvc` 是动态链接 CRT 的，产出的 exe 会导入 `MSVCP140.dll` / `VCRUNTIME140.dll` / `VCRUNTIME140_1.dll`——目标机器没装「Microsoft Visual C++ 2015-2022 Redistributable」就直接打不开。这个依赖在构建日志里完全看不见，只在别人的机器上表现为「程序一闪而过」。

`.cargo/config.toml` 打开 `-C target-feature=+crt-static`，`build.rs` 读同一个设置并给 vendored C++ 加 `/MT`。**两者必须同步**：MSVC 不支持一个进程里混用两种 CRT，而失败形态不是链接错误，是整个进程里存在两份同名全局状态。

### 11.3 图标与版本资源：`rc.exe` + 一份绘图代码

exe 没有图标是最容易被用户第一时间发现、也最容易在构建成功里漏掉的缺陷。做法是标准的 Win32 流水线：编译期用 Rust 画 `.ico`（16/24/32/48/64/128/256 七个尺寸，BMP 条目而非 PNG——写 PNG 意味着写一个 deflate 压缩器）→ 写 `.rc`（含 `VERSIONINFO`）→ 调 Windows SDK 的 `rc.exe` → `.res` 交给链接器（`cargo:rustc-link-arg-bins`）。

两个细节：

- **绘图代码与托盘图标是同一份**。`src/ui/icon_raster.rs` 被 `build.rs` 用 `include!` 引入。两份实现迟早会画得不一样，而这件事只有在用户桌面上才看得见。
- `.rc` 刻意只用 ASCII、不 `#include`、不用 `LANGUAGE` 常量——`VERSIONINFO` 不需要头文件，而数字形式能让 `rc.exe` 在 SDK 的 include 路径没配好的环境里也能跑。

`rc.exe` 找不到时只警告、不失败：图标不值得因此拒绝编译。但**打包脚本会拒绝**——`package.ps1` 读不到 `ProductName`/`FileVersion` 就报错退出。

### 11.4 开机自启是恢复路径，不是可选项

把默认输出指向虚拟声卡之后，虚拟声卡是个死胡同：没人从它取数据就是彻底的静音。所以「登录了但 FxMini 没起来 + 默认设备还在虚拟声卡上」= 这台机器没有声音。

自启因此默认开启，启动时由 `autostart.rs` 把「期望状态」与注册表对账。注册表有**两处**，只看一处会得出错误结论：

| 位置 | 含义 |
|---|---|
| `HKCU\...\Run` | 条目存在 |
| `HKCU\...\Explorer\StartupApproved\Run` | 用户在任务管理器里把它关了（12 字节 blob，首字节 bit0 = 禁用）|

于是 `is_enabled()` 回答的是真正重要的那个问题——「Windows 会不会启动我们」——而不是「注册表里有没有条目」。对账规则：

- 条目缺失 → 写入；
- 条目指向另一个位置的 exe → **重写**（换目录、开发时跑 `target/` 都会造成这种「开机启动一个不存在的路径」，而且悄无声息）；
- 任务管理器里被禁用 → 原样保留，不抢；并且从托盘再打开时会一并清掉那个禁用标记，否则「重新勾上」根本不会生效。

### 11.5 卸载必须先归还默认输出

托盘程序没有 IPC，卸载只能 `Stop-Process -Force`，而强杀会跳过归还路径——机器留在虚拟声卡上就是没声音，而卸载又正好把开机自启条目删掉，等于把自动修复也一起删了。

所以 `fxmini.exe` 增加了 `--restore-output`（`routing::rescue_output`）：优先按配置里的崩溃标记归还；标记缺失但默认设备仍在虚拟声卡上时，退回到「第一个活动的物理输出」。它被放在单实例互斥体检查**之前**——作为逃生口，它必须在有一个卡住的副本占着互斥体时也能用。

