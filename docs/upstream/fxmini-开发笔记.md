# 开发笔记

这份文档记录**实现过程本身**：每个里程碑的验证数据、本机工具链为什么需要额外脚本、上游代码里踩到的坑、以及打包自检在防什么。

面向使用者与构建者的说明在 [`../README.md`](../README.md)；架构与选型对比在 [`设计方案.md`](设计方案.md)；上游补丁清单在 [`../patches/README.md`](../patches/README.md)。

---

## 1. 里程碑与验证结果

| | 内容 | 状态 |
|---|---|---|
| M0 | 环境（MSVC）、vendor 源码 | ✅ 完成 |
| M1 | DSP 离线跑通，音色与 FxSound 一致 | ✅ **通过**：94 + 33 个上游单元全部编译链接，真实预设加载并改变信号 **+4.42 dB**，连续 14 次运行退出码全 0（含退出时析构） |
| M2 | 驱动安装/卸载 + WASAPI 回环链路 | ✅ 完成。真机验证：1 kHz 正弦输入 `0.1` → 输出 `peak 0.200`（DSP 真处理，非直通），`drop 0`、`clipped 0`。驱动**安装/卸载**路径需要管理员权限，按既定计划未实机执行 |
| M3 | 托盘常驻 + 预设切换 + 开机自启 | ✅ 完成。托盘态私有内存 **15.9 MB**（目标 < 20 MB），空闲 CPU **1.25%** |
| M4 | 点托盘弹出调音小面板 | ✅ 完成。面板为**常驻线程**（winit 的事件循环是进程级单例，不能一窗一线程），关闭后释放 GL 上下文与字体图集 |
| M5 | 健壮性：热插拔、采样率不匹配、单声道、崩溃恢复 | ✅ 基本完成。采样率错配/单声道有单测并有配置开关；热插拔走 `IMMNotificationClient`；崩溃恢复 = `previous_default_id` 标记 + 下次启动自动归还。热插拔真机场景待补 |
| M6 | 打包分发 | ✅ 完成。`package.ps1` 产出 `FxMini-<version>-win64.zip`（exe + 驱动三件套 + 安装/卸载脚本 + 许可 + SHA256） |
| M7 | 双语界面 + 面板内保存预设（v0.1.1） | ✅ 完成。托盘菜单与调音面板各一套中英文案，默认跟随 `GetUserDefaultUILanguage`，切换语言**就地改写**各项文字（`muda` 有 `set_text`，见第 6 节）；面板底部「保存为预设」经 `savePreset` 写入 `%APPDATA%\FxMini\presets\`，结果用 `presets_revision` 计数从音频线程回传 UI。过程中发现上游 `savePreset` 的第 11 号坑 |
| M8 | 界面整修（v0.1.1 一并发布） | ✅ 完成。调音面板与托盘菜单重做：设计令牌 + 跟随系统主题、均衡器由 31 行滑块改为可拖拽曲线、控制卡片改两列并按内容自适应窗口高度、托盘菜单重新分组。实测窗口打开为 **660 × 788**（旧版固定 440 × 660，同一批卡片按一列排布要 ≥ 1008 点，等于占满一整屏），无滚动条 |
| M9 | 实测问题修复（v0.1.2） | ✅ 完成。用户在一轮 debug 实测里报了四个问题，逐一修掉：控制台窗口、预设下拉「上下移动像跳动」、频段 15 及以上点了空白、面板缺圆角。其中「跳动」**不是一个 bug 而是两个**：行高随指针变化 2 pt，叠加视口不是整数行留下的残行。排查过程见第 7 节 |
| M10 | 自绘标题栏（v0.1.3） | ✅ 完成。用户指出系统标题栏和最小 / 最大 / 关闭按钮与下方的面板不是一套风格。系统栏在 Windows 10 上不可调（`DWMWA_CAPTION_COLOR` 要 build 22000 起，按钮由非客户区绘制），于是关掉装饰自绘。**真正的成本不在画那三个按钮**，而在无边框窗口丢掉的一整块非客户区：移动、双击最大化、缩放边框、最大化撤圆角、窗口描边都得自己补。命中测试踩到的坑见第 8 节。测试 53 → 61 |
| M11 | 实测问题修复（v0.1.4） | ✅ 完成。用户实测反馈「最左侧和顶部有一个约 2–3px 的横条」。逐像素量出来的是**三个叠在一起的成因**，其中两条在 winit 里：无边框窗口的投影标记让客户区下移 1px、`WS_EX_WINDOWEDGE` 被 winit 留下且无法清除；第三条是自己那圈描边 `shrink(0.5)` 把 1pt 摊到了两个像素上。另有 egui `Panel` 默认的分隔线制造了三条「凭空的白/灰边界」。排查过程与结论见第 9 节。测试 61 → 64 |
| M12 | 托盘语言对勾（v0.1.5） | ✅ 完成。用户实测反馈：切到英文后中文的对勾不消失（两项同时带勾），再次点击英文又把它自己的勾去掉。两个成因：语言项对勾**从来没被更新过**（它们用母语自称，是唯一不需要改文字的条目，所以句柄当初就没存），以及 `set_language` 对「当前语言」提前返回，把 Windows 已经切换掉的对勾原样留在屏幕上。修法是对勾双向刷新（设置**并**清除）。过程中抓出一个假的绿测试——测试自己抄了一遍打勾循环，把它提成生产函数后才真正守住回归；随后又发现测试依赖全局 `i18n::CURRENT` 而只在整套跑时蒙对。见第 10 节。测试 64 → 69 |
| M13 | 均衡器频段数恢复到 31（v0.1.6） | ✅ 完成。用户问「频段数只有 5 和 10，能不能加到 31」，并说「之前此项工作因为 10 以上的为空白所以删除了」。**那行解释为什么删掉的注释是错的**：它把「空白」归因于引擎没发布频率，实际引擎对 5/10/15/20/31 各有一张完整频率表。真正的原因是换频段数之后**没人把重算出来的频率抄回来**——只有 `apply_preset` 抄，applier 不抄，于是 `band_freq` 恒为 0，绘图函数直接返回。补一行回读，下拉恢复 5/10/15/20/31。过程中第二个假绿测试也被抓出来（测试内联抄了 applier 的逻辑），以及一个关于 vendored 引擎的硬约束（进程内只能有一个 `Dsp`）。见第 11 节。测试 69 → 70 |

自动化测试：

```
cargo check --all-targets --locked    EXIT=0，零告警
cargo clippy --lib --all-targets --locked  EXIT=0，零告警
cargo test --lib --locked (debug)     69 passed / 1 ignored / 0 failed
cargo test --lib --release --locked   69 passed / 1 ignored / 0 failed
cargo test --lib -- --ignored         1 passed（真实开窗的面板重开测试）
```

`dspcheck` 的实测输出（`Music.fac`）：

```
engine created, 5 effect slots reported
preset name      : 音乐
power            : true  (round-trip of set_power(true))
EQ bands         : 31
effects
                  get 0-1   set 0-10  fac Main
  Fidelity          0.394       3.94        50
  Bass              0.472       4.72        60
signal check (1 kHz sine at -6 dBFS)
  RMS in        : 0.353553
  RMS out       : 0.588367
  change        : +4.42 dB
OK: engine compiled, parsed a real preset, and altered the signal
```

---

## 2. 本机工具链：为什么需要一个 `toolchain.ps1`

本机的 MSVC 与 Windows SDK 都装好了，但**需要显式初始化环境**才能用：

| 组件 | 位置 |
|---|---|
| MSVC 工具集 | `D:\Program Files\VisualStudio\VC\Tools\MSVC\14.42.34433` |
| Windows SDK | `C:\Program Files (x86)\Windows Kits\10`（10.0.26100.0） |
| Rust | 1.94.1，host `x86_64-pc-windows-msvc` |

这台机器的 Visual Studio 装在 `D:\Program Files\VisualStudio` 且**没有向安装器注册**，`vswhere.exe` 查不到、注册表里也没有条目。后果有两个：

1. `cargo` 找不到 `link.exe`，连 hello world 都链不出来
2. `cc` crate 找不到 `cl.exe`，DSP 根本编不了

还有第二个更阴的坑：**Git for Windows 在 `/usr/bin` 里带了一个同名的 `link.exe`**，它是 coreutils 的硬链接工具，不是链接器。PATH 顺序不对时 rustc 会调到它，报出莫名其妙的 `link: extra operand ... Try 'link --help'`。

两个问题都由 `toolchain.ps1` / `toolchain.sh` 解决——它们会自动挑选**真正完整**的工具集版本（本机 `14.50.35717` 有头文件但没有 `lib\x64`，必须跳过），然后把 MSVC 的 bin 目录前置到 PATH。

### 一个 PowerShell 5.1 的坑

`$ErrorActionPreference='Stop'` 会把**原生命令写给 stderr 的内容变成终止性错误**。`cargo` 把进度写到 stderr，所以 `. .\toolchain.ps1; cargo build` 这个最自然的写法会莫名其妙死在 `Compiling ...` 上（报 `NativeCommandError`）。

两个修法都在仓库里：`toolchain.ps1` 在结束时把 `$ErrorActionPreference` 还原给调用方；`build.ps1` 再把「工具链 + cargo」包成一条命令，日常直接用后者即可。

---

## 3. 上游代码的坑（都已在 `capi/` 与 `build.rs` 里抹平）

### API 层面

1. **返回码是反的**：上游 `#define OKAY 0`（`codedefs.h:95`），而且 `NOT_OKAY` 在 debug 构建里展开成一个函数调用。C ABI 统一成 `DFXDSP_OK = 0` / `DFXDSP_ERR = -1`。
2. **样本类型名不符实**：签名写 `short int *`，实际永远是 32 位浮点（`DfxDspPrivate.cpp:184`）。C ABI 直接暴露 `float*`。
3. **`isPowerOn()` 是反的**：它读 BYPASS 键，非零返回 true，即**被旁通时报"开"**（`DfxDspPrivate.cpp:216`）。而 `powerOn(true)` 把 BYPASS 设为 0，所以 getter 和 setter 自相矛盾。C ABI 已反转修正，`dspcheck` 里有回归检查。
4. **音效 getter/setter 值域不对称**（上游设计，非笔误）：

   | | 值域 | 内部 |
   |---|---|---|
   | `getEffectValue()` | **0.0 – 1.0** | 归一化 |
   | `setEffectValue()` | **0.0 – 10.0** | 存 `value / 10` |

   `.fac` 的 `Main`（0–127）由加载器直接写入归一化字段，所以 `Main / 127 == getEffectValue()`，经 setter 还原则是 `Main / 12.7`。

### 构建层面

5. **不能 glob 源码树**：`dsp/` 里有 123 个 `.c/.cpp`，但上游工程只编 **94 个**。剩下的 `Lex32org.c` 之类的 "org" 变体引用的是旧版结构体（`c_Lex.h` 里已经没有 `pre_dly_start_l`），编它直接 C2039。两份清单都由 `vendor.ps1` 从对应 `.vcxproj` 导出。
6. **DSP 不自包含**：它引用辅助层的 `reg*` / `mth*` / `pstr*` / `file*`，缺了会有 **14 个** LNK2019。所以还要编第二批 33 个单元（上游把这批放在 `audiopassthru` 工程里）。
7. **`UNICODE` / `_UNICODE` 藏在 `<CharacterSet>` 里**，不在 `<PreprocessorDefinitions>`。只看后者会漏，然后宽字符串调用点全部 C2664。
8. **别加 `WIN32_LEAN_AND_MEAN`**：它把 `objbase.h` 从 `windows.h` 剔出去，`pstr.cpp` 的 `CoCreateGuid` 就变 C3861。上游没定义它。

### 源码补丁层面

9. **`preset_list_handle_` 从未初始化**：构造函数漏了这一个成员，析构函数却对它调 `prelstFreeUp()`——释放的是堆里的野指针。实测 5 次运行里有 4 次在退出时崩溃（退出码 139，Windows 访问违例）。修法是构造函数补一行，`vendor.ps1` 每次 vendor 时按唯一锚点幂等施加。诊断全文见 [`../patches/README.md`](../patches/README.md)。

### 预设文件层面

10. **`savePreset(name, path)` 的第二个参数是目录，不是文件路径**。`valsSave()` 自己拼路径（`swprintf(L"%s\\%s", dir, filename)`，`Valsfile.cpp:69`），而 `DfxDspPrivate::savePreset()` 又先给 `name` 补上 `.fac`（`DfxDspPreset.cpp:106`）。所以传一个文件路径，实际会去写 `<那个路径>\<name>.fac`。

11. **传非目录路径时这个调用不返回**：不是返回 `NOT_OKAY`，是**一直不出来**。按代码它本该在 `fileOpen_Wide()` 拿到 `NULL` 之后就返回（`Valsfile.cpp:72-74`），实测却挂在那里，原因未在源码里定位到——能确定的是调用点在音频线程上，一挂就是整条命令通道停摆，音频也不再处理。因此拦在跨 FFI 之前：`src/ffi.rs::Dsp::save_preset` 先判 `name.is_empty() || !path.is_dir()` 就返回 `false`，不让这种参数进 C++。回归测试 `engine::tests::saving_a_preset_writes_a_file_our_parser_can_read_back` 把"应当被拒绝"钉住了——在加这道闸之前，它会直接把 `cargo test` 挂死而不是失败。

> `patches/README.md` 另有一节**「已知但故意不修」**的上游缺陷：`.fac` 读取链路上 `valsRead()` 的 10 处提前 `return` 会泄漏 handle（其中 8 处还泄漏已打开的 `FILE*`），且 `loadPreset()` 把所有失败原因抹平成 `NOT_OKAY`。这类问题只记录、不进补丁表，并写明**什么条件下才值得动手**。

### 两个必须记牢的数值语义

- `num_frames` 是**帧数**不是总采样数（依据 `sndDevicesDoCapture.cpp:385`：`*ip_numSampleSets = capturedFramesCount`）
- `loadPreset` 之后 `getNumEqBands()` 返回的是引擎当前段数（默认 31），**不是** `.fac` 里声明的段数（内置预设都是 10 段）。预设曲线会被映射到 31 段网格上——做 UI 时以 `getNumEqBands()` 为准，别照搬 `.fac`。

---

## 4. 「音频没生效」这一类问题（M2 之后补的）

真正让功能生效的那一步——**把系统默认输出设备指向虚拟声卡**——在设计里写了、代码也写了，但**全项目没有一个调用点**。

症状是最难查的一种：进程正常、日志正常、预设已加载、图也建起来了，只是听不出任何区别。根因是虚拟声卡是个死胡同，没人往它写数据，引擎处理的是静音。现在由 [`../src/routing.rs`](../src/routing.rs) 接管与归还，见 [`设计方案.md`](设计方案.md) 第 6 节「坑 2」。

顺带发现 `device.rs` 里手写的 `IPolicyConfig` 少解引用一层（把接口指针当成了 vtable）。这段代码是**第一次真正执行**，一跑就段错误。COM 是两级间接：接口指针指向对象，对象首字才是 vtable。

---

## 5. 打包（M6）在防什么

`package.ps1` 里有**两项刻意检查而不是假设**的关卡：

1. **exe 里必须有图标和版本资源**。没有 `rc.exe` 时构建照样成功，只是产出一个没脸的 exe——这种缺陷在链接日志里完全不可见。脚本读不到 `ProductName` / `FileVersion` 就直接失败；`tools/inspect_resources.py` 可以进一步 dump 出 `RT_ICON` / `RT_GROUP_ICON` / `RT_VERSION` 并把图标存成 PNG 看。
2. **exe 不导入 VC++ 运行库**。`.cargo/config.toml` 打开 `target-feature=+crt-static`，`build.rs` 检测同一个设置并让 vendored C++ 用 `/MT`（MSVC 不支持一个进程里混两种 CRT）。检查方式是 `dumpbin /dependents` 里不能出现 `VCRUNTIME140` / `MSVCP140`。这个依赖在构建日志里完全看不见，只会在别人机器上表现为"程序打不开"。

其余设计取舍（为什么是 zip 而不是 MSIX / 安装器、为什么自启是恢复路径、为什么卸载必须先归还输出）见 [`设计方案.md`](设计方案.md) 第 11 节。

### 两处编码陷阱（都是在 README.txt 上看出来的）

1. **不带 BOM 的 `.ps1`，5.1 按 ANSI 读、7 按 UTF-8 读**。本机是 PowerShell 5.1 + CP936，所以 `package.ps1` 里 here-string 中的中文在**解析期**就被误解码，再以 UTF-8 写出去，成品 `README.txt` 里的中文全是乱码（`预设` 变成 `棰勮` 这类）。最阴的地方是它在 CI 上**不出现**：workflow 用的是 `shell: pwsh`（PowerShell 7，默认 UTF-8），同一份脚本在 CI 上产出的 README 完全正常；本地跑也照样 exit 0，屏幕上那行 `Packaged FxMini 0.1.1` 是纯 ASCII，看着毫无问题。修法是给所有含非 ASCII 的 `.ps1`（`package.ps1` / `build.ps1` / `toolchain.ps1` / `vendor.ps1`）都存成**带 UTF-8 BOM**，两个解释器就一致了。别靠眼睛验收，看字节：`od -A n -t x1 package.ps1 | head -1` 要以 `ef bb bf` 开头；也要看产物，`dist\FxMini\README.txt` 第一行应以 `ef bb bf` 开头、且 `预设` 是 `e9 a2 84 e8 ae be`。（`* text=auto` 不影响 BOM，它只规范化换行。）

2. **`-Encoding UTF8` 在 5.1 与 7 上是两回事**：5.1 写 BOM，7 不写。所以同一份 `README.txt` 在两边的字节数差 3。这本身不破坏解析，但会让两个环境的产物 sha256 不同——别把这种差异当成不确定性 bug 去查。

---

## 6. 界面整修（M8）踩到的坑

### 6.1 egui 侧

1. **`TextStyle::Small` 默认 9 pt**。旧面板底部那几行"看不清"不是颜色问题，是字号——9 pt 的中文几乎糊成一团。新设计把 Small 提到 11.5 pt，并在 `theme.rs` 里把字号写进设计令牌，而不是在各处临时 `.small()`。

2. **`Visuals.weak_text_color` 默认是文字色 60% 透明度**，叠在浅色背景上对比度只有 3:1 上下。把它设成**实色**（而不是调 alpha）是"次要文字看得清"的关键一步；`theme.rs` 的单元测试按 WCAG 算对比度（正文 ≥ 4.5:1、次要 ≥ 3:1），改坏任何一个色值都会红。

3. **`CentralPanel::default()` 的 `Frame` 是透明的**，它不会替你填背景。于是卡片（白）会浮在**窗口清屏色**上，而清屏色取的是 `window_fill`——也是白。结果卡片只剩 1 px 描边能看见。浅色主题的整套配色建立在"灰底 + 白卡"上，所以中央面板必须显式 `.fill(palette.bg)`。这一条是靠截图逐行扫描发现的：全窗口 55.9% 是纯白、`#F2F4F7` 只出现 20 个像素。

4. **`PathStroke` 在 `epaint` 里，`egui` 只重导出 `Stroke`**，写 `egui::PathStroke::new(...)` 会报 E0433。用 `Stroke::new(...)` 即可（有 `From<Stroke> for PathStroke`）。

5. **`Visuals.slider_trailing_fill = true`** 才让滑块画出已填充的那一段。默认关掉时滑杆是一根光秃秃的轨道加一个把手，当前值只能靠旁边那串数字读。

### 6.2 均衡器曲线：Fritsch–Carlson 的两半缺一不可

单调三次插值（Fritsch–Carlson）的限幅器**有两个部分**，只写第二个会留下过冲：

- **符号规则**：在割线变号的内点（局部极值）上切线必须归零。相邻割线是 `-24` 和 `+12` 时，取平均得到 `-6`——这根切线进入一段上升区间，会把曲线拉到起点之下；
- **半径 3 的圆**：切线经符号规则后与所属割线同号，此时 `a² + b² ≤ 9` 才是单调的充分条件。

只写第二条时，`[12, 12, -12, -12, 0, 0, 12, 12, -12, 0]` 这组数据在 x = 8.05 处给出 **-12.21**，而纵轴只到 ±12。这类 bug 肉眼看不出来（差 0.2 dB），但"画出来的增益超过了能设的上限"就是对这个控件说谎——测试因此断言曲线绝不越出自己数据的取值范围。

### 6.3 窗口高度：别写死，量一次

同一批卡片，一列排布要 ≥ 1008 点，两列排布只要 788 点；中文和英文的行高还不一样。**任何一个写死的数字对某个人都是错的**，所以 `fit_to_content` 在第一帧量 `ScrollArea::show()` 的返回值 `content_size`，加上头部与底部两条 `Panel` 的实测高度，用 `ViewportCommand::InnerSize` 要一个正好装下的窗口，并以 `viewport().monitor_size` 封顶（`.with_clamp_size_to_monitor_size(true)` 兜底）。高度计算抽成纯函数 `fitted_height(content, chrome, monitor)`，于是它有单元测试；只有"要窗口"这一步需要真窗口。

### 6.4 怎么给一个 GL 窗口截图（这一节的坑最费时间）

- 本机 PowerShell 的 `Add-Type` / `Reflection.Assembly` 被安全策略拦掉，**用不了 .NET 的屏幕捕获**。可用的路子是隔离 venv 里的 Python + Pillow；
- `PIL.ImageGrab` 抓**硬件加速窗口**会拿到空白或过期内容：同一个窗口，一次抓到 3162 种颜色（真实内容），下一次只有 30 种（一片白）。**不能靠它判断"渲染对不对"**；
- `SetWindowPos(HWND_TOPMOST)` 把窗口置顶之后反而彻底抓成空白——GL 的呈现表面被打断了。要抓就**别动窗口**；
- 可行的是 `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT = 0x02)`：它取的是 DWM 合成后的表面。本次截到 2717 种颜色，调色板各角色命中率合计约 91%（卡片 57.1% / 井 20.1% / 灰底 9.4% / 描边 1.9% / 控件 1.8% / 强调色 0.4%）；
- 而且**模型自己看不了图**。所以校验靠的是像素统计而不是"看一眼"：色值命中率、有无滚动条、以及逐行扫描找两列之间那条 8 px 的灰缝（`x = 334..341`）。

那个会开窗的测试留了 `FXMINI_PANEL_TEST_DWELL_MS`，就是为了让窗口停住好抓图；默认 0，不影响正常跑测试。

---

## 7. 实测问题修复（M9）踩到的坑

用户在一轮 debug 实测里报了四个问题。最费时间的是第二个——「预设下拉点开后，鼠标上下移动时字行像在跳动」——因为它**不是一个 bug 而是两个**，而两个都不在静态截图里。

### 7.0 先说一个排查教训：静态截图测不出交互态

一开始的排查方式是拿用户给的两张截图（分别悬停在列表第 3、4 项）做像素比对：行网格完全一致、逐行差值几乎全为 0，于是判定「没有跳动」。

**这个结论是错的。** 两张图各自停在不同行，而差异恰好落在悬停那一行上，很容易被解释成「本来就该不同」；更要紧的是，跳动是**交互态**现象——只有指针真的落在一行上，egui 才会走进那个分支。截图是同一状态的两个实例，不是两个状态的对比。

改成让程序真的进入那个状态再量：现在这个测试跑**两帧真帧**，第二帧投一个 `Event::PointerMoved` 到某一行中间，断言 `hovered == true` 且该行高度不变。它在修复前会红。

### 7.1 行高会随指针变化（跳动的成因 A）

`Button::selectable` 内部会设 `frame_when_inactive(selected)`，而 `Button` 只在「**既没被选中也没被悬停**」时换成没有描边的 `Frame::new()`。描边是 `Frame::total_margin` 的一部分（`inner + stroke + outer`），所以描边一走，行高就少 2 pt：

| 行的状态 | 行高 |
|---|---|
| 未选中、未悬停 | 29 pt |
| 悬停 / 选中 | 31 pt |

指针扫过时，**变高的是当前行，位移的是它以下的每一行**——整体下移 2 pt，这才是「跳动」的观感来源。

修法是让每行都走带 `Frame` 的路径（`preset_row()` 里 `frame_when_inactive(true)`），再把这个 `Frame` 做成**不可见而不删掉**：填充设透明、描边**保留宽度只去掉颜色**。宽度是行高的一部分，动不得；而选中行的高亮不受影响，因为 `Style::button_style` 在按钮带选中标记时会用 `visuals.selection` 覆盖 `weak_bg_fill`。

### 7.2 弹层视口不是整数行（跳动的成因 B）

`ComboBox::height` 给的是滚动视口的**上限**，egui 默认是一个扁平的 `Spacing::combo_height`（200 pt）。200 不是任何行高的整数倍，于是底部留一条被裁剩的残行；滚动之后残行停在边缘，指针每经过一次就多看一眼「半行」。

改成 `8 × 实测行高 = 248 pt` 之后还在踩第二个坑：`preset_popup_height(ui)` 是在弹层**外面**那个 `ui` 上调用的，而弹层内的 `button_padding` 覆写发生在 `ComboBox::show_ui` **内部**——所以读到的是 5 而不是 8，算出的视口比它自称的 8 行短了 48 pt，实测只显示 6 行加一条残行（残行高度 12 px vs 完整行 17 px，一眼能看出来）。**内边距要从常量读**。

### 7.3 `text_style_height` 量的是字体，不是行

行高最初用 `ui.text_style_height(&TextStyle::Button)` 加内边距来算。这个函数返回的是**字体的行高**，而面板装的 CJK 回退字体度量远高于多数预设名用的拉丁文——按它算 8 行，弹层放出 11 行。

改为 `painter().layout_no_wrap()` 真的排一次样本串（`"Ag低音增"`：带拉丁上伸部/下伸部，也带汉字，两个字体面里高的那个决定行高），拿到的是行控件真正的高度。

### 7.4 Windows 10 没有 DWM 圆角 API

Win11 有 `DWMWA_WINDOW_CORNER_PREFERENCE`，本机是 Windows 10 22H2（build 19045），没有。只能自己裁窗口区域：`GetWindowRect` + `CreateRoundRectRgn` + `SetWindowRgn`。

- 裁的是**窗口矩形**而不是客户区。`GetWindowRect` 含那圈不可见的调整边框；按客户区裁，最外一圈边框仍然是直角，圆角看起来「没生效」；
- 区域是 **1-bit 掩码，不做抗锯齿**。8 pt 半径下四角会有约 1 物理像素的台阶。**这里当时写的是「要平滑只能整窗无边框 + 自绘，代价远大于收益」——v0.1.3 证明那句话只对了一半**：为了标题栏（第 8 节）本来就得无边框，于是圆角仍然靠这个区域裁剪，台阶也仍然在。换句话说无边框并没有换来平滑的圆角，它换来的是标题栏；圆角那 1 pt 台阶是这台机器上省不掉的；
- `SetWindowRgn` 失败时区域对象要自己删（`windows` 的 `HRGN` 实现了 `Free`，`free()` 会调 `DeleteObject`）；
- 尺寸没变就不重裁，避免每帧进一次 GDI。**v0.1.3 新增**：最大化时必须把区域**撤掉**（`SetWindowRgn(hwnd, None, true)`），因为最大化窗口的四角就是屏幕的四个角，切圆角等于在桌面上啃四口；还原时要重裁，所以「已裁尺寸」缓存要跟着清空。

托盘菜单按 Windows 惯例保持方形，没跟着圆。

### 7.5 `windows_subsystem` 只在 release 生效

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
```

这句话的意图是「发布版不要黑窗口」，实际效果是把控制台变成了**只在 debug 存在的缺陷**——而 debug 恰恰是唯一会去看它的场合。日志本来就写文件（`init_logging`），不依赖控制台，所以改成无条件：

```rust
#![windows_subsystem = "windows"]
```


## 8. 自绘标题栏（M10）踩到的坑

用户的反馈只有一句：「标题栏和最小、最大、关闭按钮的风格不够现代化（与下方的风格不一样）」。根因是窗口一直戴着系统装饰，而系统标题栏在 Windows 10 上**不可调**：`DWMWA_CAPTION_COLOR` 要 build 22000 起；按钮画在窗口的非客户区，应用拿不到那份画笔；按钮形状由主题的视觉样式决定，只能换成系统的另一套。所以只能 `with_decorations(false)` 整条自绘。

**这一节的重点不是怎么画那三个按钮（那是 100 行的事），而是无边框之后丢掉的一整块非客户区。**

### 8.1 `ViewportCommand` 的出口在 `viewport_output`，不在 `platform_output`

`FullOutput` 有两个看起来都能放命令的地方，很容易取错：

- `platform_output.commands: Vec<OutputCommand>` —— 只有 `CopyText` / `OpenUrl`；
- `viewport_output: HashMap<ViewportId, ViewportOutput>`，里面那个 `commands: Vec<ViewportCommand>` 才是移动 / 最小化 / 最大化 / 关闭 / 缩放。

取错了不是「拿不到命令」而是**编译错误**（类型就不对），但要先知道该去哪儿找。测试里从前者取，报的是 `expected OutputCommand, found ViewportCommand`。

写测试时的正确姿势：

```rust
full.viewport_output
    .into_values()
    .flat_map(|viewport| viewport.commands)
```

### 8.2 无边框窗口**没有**缩放边框

这是最容易漏掉的一条，因为视觉样式里那个尺寸柄还在，看起来像能拖。实际上 winit 的无边框窗口 `WM_NCCALCSIZE` 把整个窗口申报成客户区，于是不存在任何非客户区供 Windows 做边缘命中测试，`DefWindowProc` 对每个像素都回 `HTCLIENT`。

补法是在窗口内侧自绘 8 个热区（四角 + 四边），按下时发 `ViewportCommand::BeginResize(direction)`。两个细节：

- 厚度取 5 pt，比系统的 8 薄。这块热区**在窗口里面**，它的每一点都是面板上再也点不到的一点，而且缩放不止这一条路（键盘和系统菜单仍然有效）；
- 用 `drag_started()` 触发，**不能用 `clicked()`**。`BeginResize` 会向系统发一次非客户区按下，随后 OS 跑自己的模态循环等释放——若在 click 时才发，那个释放早就过去了，窗口会一直粘在指针上。

### 8.3 热区必须是 widget，不能「拿指针坐标自己判」

热区压在面板上：标题栏的按钮贴着上边，关闭按钮的角就在右上热区里面。如果热区是从 `input` 里读坐标算的，那么在角落按下会**既**开始缩放、**又**把这次按下交给下面的按钮；而 `BeginResize` 之后鼠标已经交给 OS 模态循环，按钮永远收不到释放，会**卡在按下态**。

做成前层 `Area` 里的普通 widget 就没这个问题：egui 按「最上层命中者胜」处理，和 Windows 自己的做法一致。这条有测试钉着（`a_resize_zone_wins_against_a_widget_underneath_it`）。

### 8.4 `Area` 的命中测试慢一帧（这一条最费时间）

新加的真帧测试一开始失败了：在左上角按下，什么命令都没发出来。原因是**`Area` 的命中测试用的是上一帧建好的列表**——第 1 帧才创建的热区，第 2 帧对指针还是隐形的，**要到第 3 帧才活**：

| 帧 | 1（屏幕坐标刚出现） | 2 | 3 | 4（按下） |
|---|---|---|---|---|
| `layer_id_at(角落)` | Background | 该 Area | 该 Area | 该 Area |
| Area 内 widget `hovered` | false | false | **true** | **true** |
| 同位置背景层 widget `hovered` | false | true | false | false |

在真面板里这是启动时约 60 ms 的事，用户看不见；但测试必须在按下前空转两帧，否则量到的是预热而不是边框。代码里那个 `SETTLE_FRAMES` 常量写的就是这个，不是随手凑的 padding。

**顺带一条通用教训**：第一次失败时我只断言了「命令是空的」，查了很久方向都不对。把 `hovered` / `dragged` / `drag_started` / `primary_down` / `layer_id_at(pos)` 五个值一起打出来之后，一眼就看到 `hovered = false`——问题在命中测试，不在拖动机制。**先量状态，再改代码。**

### 8.5 别把「几何测试全绿」当成「控件能用」

`window_chrome.rs` 最初那 5 个测试全是在算矩形（热区在哪、按钮在不在角上、有没有互相盖住）。它们都通过，而且**一个没人拖得动边框的窗口也照样能全部通过**。几何是必要条件，不是充分条件；凡是「这个控件到底能不能点到」，都要有一个真的把命令发出来的测试。
### 8.6 零碎的

- `egui` 0.35 **没有 `ctx.screen_rect()`**，窗口矩形是 `ctx.input(|i| i.viewport_rect())`；最大化状态是 `input.viewport().maximized.unwrap_or(false)`（拿不到时按「没最大化」处理，圆角会在下次拿到时补上）。`InputState` 上也没有 `screen_rect()`，`Memory::areas().order()` 是私有的（数层用 `memory.layer_ids()`）。
- 双击标题栏最大化要 `Sense::click_and_drag()`——双击需要 click 那一半；同时**单击什么都不做**（标题栏对单击有反应是意外，不是功能）。
- `Glyph::Restore`（还原字形）只画前窗和「后窗露出的那两条边」。把后窗整条框画出来，它的轮廓会从前面那张窗里穿过去，看起来是一团。
- **关闭按钮的红色是固定的 `#C42B1C`，不是 `palette.danger`**。`danger` 是为「在白卡片上当文字」挑的，暗色主题下必须够亮；白字形压上去只有 2.8:1。这个红在浅深两色下都有 5.4:1。
- 撤掉装饰后窗口既没投影也没边缘，补了一圈 1 pt 描边，颜色用 `border_strong`——这是窗口和「桌面」之间的边，不是卡片和窗口之间的边，给卡片用的那道淡边贴到壁纸上就没了。
- 标题栏高度 32 pt 取自 Windows 11（按钮 46 × 32 也是），目的是让 FxMini 的窗口和周围的窗口一样高。窗口最小高度因此从 480 提到 **512**。


## 9. 窗口边缘那 2–3 px 的「横条」（M11，v0.1.4）

用户的反馈还是一句：「最左侧和顶部上面有一个约 2-3px 的横条」。

顶部和左侧同时出现、宽度一致、**右边和底部却没有**——这个不对称本身就说明了它不是画出来的东西（画出来的东西至少左右对称）。它由三个独立的成因叠在一起，其中一个来自 winit，一个来自 egui，一个来自我自己上一轮写的那圈描边。

### 9.1 先说方法：怎么量出「2-3px 是什么」

这台机器是 **96 DPI / 100% 缩放**（`GetDpiForWindow: 96`、`GetScaleFactorForDevice(0): 100`），所以 **1 pt = 1 px**，像素尺子可以直接当点尺子用。

量法不是截图看，而是**逐像素取色 + 行程编码**。用隔离 venv 的 Pillow（`ImageGrab`）抓窗口，然后：

- 对 `x = 0..7` 的每一列、`y = 0..7` 的每一行打印色值；
- 对每条边统计「某个颜色占这一行/这一列的百分比」。

最后一条是全轮的转折点，因为它把模糊的「有个条」变成了确切的坐标。修复前的原始读数：

```
TOP    y=0..5 at x=W/2 → 227,227,227 | 255,255,255 | 207,214,223 | ...
LEFT   x=0..5 at y=H/2 → 242,244,247 | 207,214,223 | ...
```

逐条翻译过来：

- `(227,227,227)` —— **灰的，而且 R=G=B**。它不在调色板的任何一个角色里（`bg` 是 242,244,247，`border` 是 227,231,237），是个「无名灰」，只能是系统画的；
- 第 2 行才是 `207,214,223`，也就是 `border_strong`——**我自己那圈描边，但它跑到第 2 行去了**；
- 左侧同理：第 0 列是 `bg`（应该是描边），第 1 列才是描边。

「x=1 是 100% `border_strong`、y=2 是 100% `border_strong`」这一对读数，就是用户看到的那 2–3 px 的真身。

### 9.2 成因 A：无边框窗口的投影标记让客户区**下移 1 px**

`egui-winit` 只要发现没有装饰，就会顺手给窗口要一个「无边框窗口的投影」（`egui-winit-0.35.0/src/lib.rs:2146`）：

```rust
window_attributes = window_attributes.with_undecorated_shadow(!decorations.unwrap_or(true));
```

而 winit 对这个标记的应答是**把客户区矩形往下挪 1 像素**（`winit-0.30.13/src/platform_impl/windows/event_loop.rs:1182`）：

```rust
} else if window_flags.contains(WindowFlags::MARKER_UNDECORATED_SHADOW) {
    // HACK(msiglreith): To add the drop shadow we slightly tweak the non-client area.
    // This leads to a small black 1px border on the top. ...
    params.rgrc[0].top += 1;
    params.rgrc[0].bottom += 1;
}
```

上游注释自己都写了「这会导致顶部出现 1px 黑边」。

这份 **1 px 的错位**把装饰也算成客户区了：我的描边按「窗口矩形」裁切（`window_shape.rs` 的圆角裁剪就是这么做的），但 egui 的客户区已经整体下移，于是顶部多出一条谁都不认领的空隙。**它只影响顶部和左侧**——`top += 1` 只加在 top 上，而左侧那 1 px 是这一条和成因 B 叠加的结果。

**验证方式**是直接问系统：`GetWindowRect` 拿窗口矩形，`GetClientRect` + `ClientToScreen` 拿客户区原点，两者相减就是非客户区的厚度。修复前读数是 `window origin = (130,130)`、`client origin = (130,131)`，**ring 顶部 = 1**。

**修法**：用 winit 自己支持的 API 把这个标记清掉（`winit::platform::windows::WindowExtWindows::set_undecorated_shadow(false)`，`winit-0.30.13/src/platform/windows.rs:279` 声明、`:378` 实现），挂在 `window_shape::RoundedWindow::apply` 里跑一次：

```rust
if !self.shadow_off {
    self.shadow_off = clear_undecorated_shadow(frame);
}
```

三个细节：

- **放在区域裁剪之前**。关掉投影会重跑一次 `WM_NCCALCSIZE`，客户区矩形会变；区域是从窗口矩形上切下来的，得等几何稳定了再切。而且这一句紧挨着「尺寸没变就跳过重裁」的缓存判断，顺序错了会拿到过期几何；
- **用 `shadow_off: bool` 记住已经关过了**。给窗口没有窗口的时候（headless、或者后端拿不到 winit window）返回 `false` 而不是 `true`，这样下一帧还会再试；
- **它是个自由函数，不是方法**。写成 `self.clear_window_edge(frame, &mut self.shadow_off)` 会直接 `error[E0502]`：`&mut self` 和 `&mut self.shadow_off` 同时借。改成自由函数返回 `bool`，调用点写 `self.shadow_off = clear_undecorated_shadow(frame)`，借就分开了。

修完的读数（`client_probe`）：**四条边的 ring 全是 `0`**，`GetWindowRect (75,81)..(735,901)` 与客户区原点完全重合，两个都是 660×820。

### 9.3 成因 B：`WS_EX_WINDOWEDGE` 被留下，而且**清不掉**

winit 给顶层窗口的扩展样式里有 `WS_EX_WINDOWEDGE`——就是 Windows 给「凸起边框」的那个位：

```rust
// winit-0.30.13/src/platform_impl/windows/window_state.rs:259
let mut style_ex = WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES;
```

窗口有系统装饰时这个位是对的（系统自己会画那圈边），无边框之后它就是一条**裸的、系统画的凸起边**。但 winit 清它的那句 `style_ex &= !WS_EX_WINDOWEDGE;`（同文件 `:288`）**写在 `WindowFlags::CHILD` 的分支里**——顶层窗口走不到。

那就自己清。结果是**操作系统拒绝**：`SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !WS_EX_WINDOWEDGE)` 返回旧值、`GetLastError` 是「操作成功完成」，紧接着 `GWL_EXSTYLE` 重读**还是 `0x40110`**，`WS_EX_WINDOWEDGE` 位纹丝不动。

用 `FXMINI_EDGE_DEBUG` 环境变量把这一串打进 stderr 之后拿到 264 行完全相同的记录，没有一次例外：

```
GWL_EXSTYLE=0x40110 windowedge=true
set returned 262416 (err 操作成功完成。 (os error 0)) -> reread 0x40110
```

即使它能清掉也没用：winit 在任何 flag 变化时都会**整个重新推导**扩展样式（`window_state.rs:406–407` 从 `to_window_styles()` 再写一遍 `GWL_STYLE` / `GWL_EXSTYLE`），一次性清除会被覆盖回去。

**结论：这条路放弃了**，连同 `GetWindowLongPtrW` / `SetWindowLongPtrW` / `SetWindowPos` / `GWL_EXSTYLE` / `WS_EX_WINDOWEDGE` 那几个 import 一起删掉，`window_shape.rs` 的 import 回到只有 `GetWindowRect`。**留下来的价值是那条否定结论本身**：`WS_EX_WINDOWEDGE` 在 winit 的无边框顶层窗口上是不可移除的，不要再去试。

也就是说，`WS_EX_WINDOWEDGE` 那 1 px 在 winit 这一档解决不了。好在这台 Windows 10 上它**没有真的画出边**（`GetWindowRect` 与客户区重合就是证据），真正制造可见横条的是另外两条。

### 9.4 成因 C：`StrokeKind::Inside` + `shrink(0.5)` 把 1 pt 摊成了两个像素

上一轮补的那圈描边是这么写的：

```rust
let rect = ctx.input(|i| i.viewport_rect()).shrink(STROKE / 2.0);
// ... rect_stroke(rect, ..., StrokeKind::Inside)
```

`StrokeKind::Inside` 的语义是「线画在矩形**内**侧」。先 `shrink(0.5)` 再画 1pt 内侧线，得到的覆盖区间是 `[0.5, 1.5]`：

- 像素 0 完全没被描边碰到，露出下面那层 fill；
- 像素 0 和 1 **各被盖住一半**，于是抗锯齿把线摊成一条虚的、跨两像素的带子。

这正是「2-3 px 横条」里剩下的那部分，也是为什么它在顶部比在底部明显：顶上还叠着 9.2 那 1 px。

**修法是把视口边缘先吸附到整数像素，再 `shrink` 半个线宽**：

```rust
fn edge_rect(viewport: Rect, stroke: f32, pixels_per_point: f32) -> Rect {
    let scale = if pixels_per_point > 0.0 { pixels_per_point } else { 1.0 };
    let half = stroke / 2.0;
    let snap = |value: f32| (value * scale).round() / scale;
    Rect::from_min_max(
        Pos2::new(snap(viewport.left()), snap(viewport.top())),
        Pos2::new(snap(viewport.right()), snap(viewport.bottom())),
    )
    .shrink(half)
}
```

关键在**顺序**：吸附过的边 `snap(v)` 再 `- half` 得到的起点是 `snap(v) - 0.5`，加上线宽 1pt 正好覆盖 `[snap(v) - 0.5, snap(v) + 0.5]`，也就是**那个整数像素本身**，两侧各溢出半像素、被裁掉。**不能再吸附一次**：`snap(v + 0.5) - 0.5 != snap(v)`（`round(0.5)` 进位），第二次吸附会把第一次的修正抵消掉——这个坑我在测试里踩了，`the_outline_covers_the_outermost_pixel_and_no_more` 在 1× 下报「左边偏了 0.50 px」就是这么来的。

修完的读数（`corner_measure`）：

```
TOP    y=0..5 → 255,255,255 | 207,214,223 | 255,...   （白，描边在 y=1）
LEFT   x=0..5 → 242,244,247 | 207,214,223 | ...        （bg，描边在 x=1）
RIGHT  x=..   → 207,214,223 | 242,...                  （描边在 x=0）
BOTTOM y=..   → 207,214,223 | 255,...                  （描边在 y=0）
```

四条边**每边恰好一个像素**的 `border_strong`，对称、无灰行、无填充空隙。左上角 6× 放大的 `corner_top-left.png` 是一条干净的发丝线贴着圆角，没有叠影。

### 9.5 顺手查出来的第四条：`egui::Panel` 默认会画分隔线

找描边的过程中发现面板上还有三条**凭空的分界线**，在 y=32（标题栏/头部）、y=70（头部/中央）、y=654（中央/页脚）。它们不是 `ui.separator()`——全文件 grep 过，只剩页脚卡片里那一处，而且它被卡片内边距缩进了 12 pt，本来就不在左边缘上。

真凶是 `egui::Panel::show_separator_line`，**默认是 `true`**（`egui-0.35.0/src/containers/panel.rs:185`、`:270`）：每个 `Panel::top` / `Panel::bottom` 都会在自己内侧画一条通栏 1pt 线，颜色取 `visuals.widgets.noninteractive.bg_stroke`（在这里等于 `control_active` #E3E7ED）。而这三处的两个相邻 `Frame` 是**故意填成同一个白色**的——面板本来就该是连续的一条白带，那条线是在拆自己的台。

三处都加 `.show_separator_line(false)`。页脚那处另有一条理由：页脚的卡片本来就有边框，面板再画一条，等于在卡片上方多出一道一模一样但错位的线。

### 9.6 这一轮的方法论

- **不对称的缺陷指向系统，对称的缺陷指向自己。** 顶+左有、右+下没有，一上来就该怀疑「客户区被平移了」，而不是「我哪里画粗了」。这一条把排查时间从「翻绘图代码」缩到「问一次 `GetClientRect`」。
- **把「看」换成「量」。** 这一轮四个结论没有一个来自肉眼：非客户区 ring = 0 是 `GetClientRect` 给的，客户区错位是 `GetWindowRect` 对减出来的，描边位置是逐像素行程编码给的，投影标记是上游源码的行号给的。`(227,227,227)` 这种「无名灰」尤其说明问题——**它不在调色板里，就不可能是我的代码画的**，顺着这条线才找到 winit。
- **上游的注释是文档，不是八卦。** winit 那句 `// This leads to a small black 1px border on the top` 直接写着答案，前提是知道要去读它。
- **量不了的就不要改。** 我先花了一轮去清 `WS_EX_WINDOWEDGE`，写环境变量调试、打 264 行日志，最后确认系统拒绝、且 winit 会覆盖回去。**这个否定结论有价值**（省掉以后所有人重复尝试），但它不该出现在第一轮——应该先花五分钟确认「清掉之后有没有可见变化」，再决定值不值得深入。
- **测试断言要断言「是什么」，不是「看起来像什么」。** 描边那条新测试最初断言「正好盖住 1 个**像素**」，在 1.5× 缩放下必然失败——但 1.5× 下 1pt 本来就该是 1.5px，**是断言错了，不是代码错了**。改成断言**点数**恰好等于 `STROKE`，只在 `scale == 1.0` 时附加检查「那就是 1 个像素」。
- **顺带清掉一条旧账**：`docs/开发笔记.md` 里测试计数还停在 `61 passed`（0.1.3 那一轮加的 3 个测试没并进去），这轮一并改成 `64 passed`。文档里的数字和技术结论一样会漂移。


## 10. 托盘语言菜单的对勾（M12）

### 10.1 现象与两个成因

报告是两句话，但它们是**同一个 bug 的两半**：

> 选择英文时，中文前面的对勾不会消失，而是英文、中文前面都有对勾；再次点击英文，会将英文上面的对勾去掉（这与实际仍是英文状态不符）。

**成因一：语言项的对勾从来没有人更新。** `retitle()` 只做重写文字这一件事。而语言项用的都是**母语自称**（`中文` / `English`）——这是刻意的，目的是让你在切到一个看不懂的语言之后还能找到路回来——所以它们是整个菜单里**唯一不需要改文字**的一对。既然不需要改文字，当初就没把句柄存下来，`retitle()` 里自然也没有任何一行去碰对勾。切语言时唯一被刷新的是那些会变的条目。

**成因二：`set_language` 对「已经是当前语言」提前返回。** 这行提前返回本身没错——翻译不需要重做。但它漏掉了一件事：**Windows 在点击的那一刻就把该项的对勾切换掉了**，事件到达应用时勾已经没了。于是「再点一次英文」这个动作，先把勾去掉，然后应用一看「语言没变」直接 return，错误状态就原样留在屏幕上了。

### 10.2 修法：对勾要「移动」，不能只「设置」

关键认知是：**每个语言项的对勾是彼此独立的**，Windows 不会因为你勾了兄弟项就自动把旧的清掉。所以刷新必须双向——对新语言 `set_checked(true)`，对旧语言 `set_checked(false)`。只做第一个就是原来的 bug。

```rust
fn tick_languages(items: &[(Lang, CheckMenuItem)], current: Lang) {
    for (lang, item) in items {
        item.set_checked(*lang == current);   // 设置 *并* 清除
    }
}
```

这个函数从 `Tray::refresh_language_ticks()` 里拆出来独立成自由函数，不是为了好看：`Tray::new` 会真的建一个托盘图标，需要桌面会话，所以走 `Tray` 的测试只能挂 `#[ignore]`——**回归就会在 CI 里无人看守**。拆出来之后测试直接驱动这个纯函数，在无桌面环境也能跑。

`app.rs` 那边补上另一半：提前返回的分支里先 `refresh_language_ticks()` 再 return。

### 10.3 一个必须遵守的上游约束：`items()` 是线程绑定的

语言项要在 `Tray::new` 里取出来**一直持有**，不能等切语言的时候再用 `Submenu::items()` 回读。

原因在 `muda-0.20.0/src/items/mod.rs:82–89`：`UnsafeMenuItemKind` 里面的 `ManuallyDrop<MenuItemKind>` 手写了一个 `unsafe impl Send`，而文档写得毫不含糊——

> The caller must run on the thread where the wrapped `MenuItemKind` was created … The recovered value must remain on that thread and be dropped there.

也就是说 `items()` 只在**创建这些条目**的线程上合法，在别处把返回的克隆**析构就是 UB**。而 `app.rs` 是**故意**在消息循环线程上切语言的，托盘却是在 UI 线程上建的——在那边回读，正是这条约束禁止的跨线程访问。

顺带记两条容易踩错的：

- `Submenu::append(&dyn IsMenuItem)` **按引用**收参，然后克隆进子项列表（`submenu.rs:173`）。克隆体与原句柄**共享** `MenuId`（`Arc<str>`）和共享状态，所以勾哪一份都生效。既然如此还是走子项列表更好：那才是用户真正看到的东西，**append 失败会表现为列表变短，而不是表现为「勾了一个不在屏幕上的句柄」**。
- `MenuId` 本身是 `Arc<str>`，跨线程是安全的——但**光有一个 id 打不了勾**，`set_checked` 在条目上。存 id 不解决问题。

### 10.4 最值得记的一条：测试「通过」可能纯属巧合

这轮我先写了 5 个测试，全绿。然后按惯例把 bug **故意放回去**，想确认测试会红——**结果还是全绿**。

原因是测试里把打勾的循环**照抄了一遍**：

```rust
// 反例：这段看起来在测，其实是在测自己抄的副本
let ticked = |items: &[(Lang, CheckMenuItem)]| {
    for (lang, item) in items { item.set_checked(*lang == current); }
};
```

它测的是测试自己写的那段逻辑，`item.set_checked(true)` 少写一个 else 分支它也发现不了。**修法不是改断言语义，而是让测试去驱动生产代码**——把循环提成自由函数 `tick_languages`，测试全部改调它。再放 bug 回去，这次两个行为测试分别以 `[Zh, En]`（两项都带勾）和 `[En]`（当前语言反而没勾）失败，并且**失败信息逐字对着用户的两句描述**。

第二个测试还暴露了一个更细的问题：它原本先把当前语言的对勾清掉、再要求恢复，这个空白状态**放 bug 进去也能通过**——因为「只设置」在没有任何东西被勾的时候恰好是对的。而桌面上的真实状态是「勾错了项」。所以改成**从「另一项带勾」出发**：

```rust
// 只设置不清除的实现在空白状态下会碰巧通过；
// 真实桌面状态是「勾错了项」，只有会清除的实现才能修好它。
item.set_checked(false);
other_item.set_checked(true);
```

**教训：故意的 bug 注入必须真的跑一遍。** 「我写了测试」和「我的测试能抓住这个 bug」之间隔着一整次验证，而这轮第一次做注入时就发现隔的是空的。

（注入做完之后还有**第二层**同类问题——修完这层仍然绿，但单独跑会红。见 10.5。）

### 10.5 第二层问题：测试依赖全局可变状态，于是**只在整套跑时绿**

修好上面之后，四个门全绿。但为了确认测试真的守得住，我单独跑了一次 `cargo test --lib ui::tray` ——**两个测试红了**，而它们在全套里是绿的：

```
the_initial_tick_is_on_the_language_in_force
  left: [Zh]   right: [En]
re_picking_the_current_language_restores_its_tick
  left: [En]   right: [Zh]
```

注意数值是**反的**，说明测试当时看到的「当前语言」和我以为的不是同一个。

原因：`i18n::CURRENT` 是一个**进程级 `AtomicU8`**（`i18n.rs:120`），而 `cargo test` 在**多个线程上并行**跑测试。`i18n::tests::selecting_a_language_is_visible_through_t` 会在中途把语言依次设为 `En`、`Zh`，我的托盘测试在同一时刻 `i18n::current()` 去读，读到的就是**别人半途设置的值**。全套跑时线程交错恰好让它蒙对，单独跑就露馅。

这是和 10.4 同一类错误的第二个样本：**测试没有控制自己的前置条件**。修法有两层：

1. **不读全局。** 需要知道语言的地方改成显式传参，测试自己指定 `Lang::Zh` / `Lang::En`，不去问 `i18n::current()`。
2. **要改全局的就先锁住、结束时还原。** 新增一个 `with_language(lang, body)` 帮助函数，内含一个 `static Mutex`，用法与 `i18n.rs` 里那个测试的 `let before = current(); … set(before);` 同构，只是加了锁以避免并行交错。锁中毒时**不当作失败**（`unwrap_or_else(|p| p.into_inner())`）——否则一个测试 panic 会连带把后面每个测试都报成中毒，真实错误反而被淹没。

顺带把覆盖面补成**对两种语言各跑一遍**：`the_initial_tick_is_on_the_language_in_force` 和 `re_picking_…` 都在 `Lang::ALL` 上循环。只测一种语言的话，「永远勾同一个条目」的错误实现也能骗过断言；而 `re_picking_…` 只测一种语言时，一旦 bug 清的是另一种语言的勾就漏掉了。

**这两层合起来的教训：测试的「绿」要能解释清楚为什么绿。** 第一次是测了副本，第二次是读了别人正在改的全局——两次都不是代码错了，而是**测试给了一个自己也无法保证的承诺**。判据很简单：把测试单独跑一遍、把 bug 放回去跑一遍，两次都要给出预期的结果。

### 10.6 质量门

四个门全绿，测试数 **64 → 69**（新增 5 个托盘测试）：

| 门 | 结果 |
|---|---|
| `cargo check --lib --locked` | exit 0 |
| `cargo test --lib --locked` | 69 passed / 1 ignored / 0 failed |
| `cargo clippy --lib --all-targets --locked` | exit 0，无告警 |
| `cargo test --lib --release --locked` | 69 passed / 1 ignored / 0 failed |

另外单独验证过两件事，因为它们是上面两次翻车的直接防线：

- `cargo test --lib --locked ui::tray` —— **单独跑 5 passed**（修之前这里是 2 failed）。
- 把 bug 注回 `tick_languages` —— 两个行为测试分别以 `[Zh, En]` 和 `[Zh, En]` 失败，注入完即还原。

### 10.7 发版：0.1.5

`Cargo.toml` 与 `Cargo.lock` 的 `fxmini` 条目同步到 **0.1.5**（两处必须一起改，CI 与
`package.ps1 -Locked` 都走 `--locked`）。四个门全绿后提交 `f829573`，打标签
`v0.1.5` 并把分支与标签在同一次 push 里送上去：

```
249e700..f829573  main -> main        （4 个提交，fast-forward）
* [new tag]       v0.1.5 -> v0.1.5
```

标签推送触发 `Release` 工作流（它会把标签与 `Cargo.toml` 的版本对一遍，不一致直接
失败），`Release / v0.1.5` 与 `CI / main` **两个工作流都是 success**，GitHub Release
`FxMini 0.1.5` 已发布（`draft=False`）。

**产物对过真字节**，不是只看退出码：

| 项 | 值 |
|---|---|
| 本地包 `dist/FxMini-0.1.5-win64.zip` | 3,707,015 字节，`fad37fd946884887…` |
| exe 大小 / 运行时 | 8,499,200 字节，self-contained（不需要 VC++ 运行库） |
| CI 发布产物 | 3,790,286 字节 |
| 下载回来重算 SHA256 | `78d9e1ace8cb9e74…` = 清单里声明值 ✅ |
| 包内 `fxmini.exe` | `3aec572d6c7907f0…` = 清单对应条目 ✅ |
| 版本资源 | `FileVersion = ProductVersion = 0.1.5` ✅ |

本地包与 CI 包的**字节数不同是正常的**——`Compress-Archive` 的压缩输出在不同机器上
不可复现。清单担保的是**包内每个文件**，而那些确实对上了。

提交 `c0e113d`（2 文件，+235 −16）与 `e3c7939`（文档）。按惯例只提交，不推送、不打标签——本轮用户没有要求发版。


## 11. 均衡器频段数恢复到 31（M13）

### 11.1 现象与那行错误的注释

用户报的是：

> 我发现默认的显示 31 个均衡器频段数，但是后续选择只有 5、10。没有其他的，这个能否增加到 31 个频段？之前此项工作因为 10 以上的为空白所以删除了。

下拉框确实只列了两项，旁边还留着一整段注释解释为什么：

```rust
/// Five and ten only. The engine does take 15, 20 and 31 — `GraphicEqSetNumBands`
/// validates 1..31 — but nothing upstream publishes frequencies for them: the
/// band editors come up blank and dragging a node does nothing, because
/// [`eq_curve`] draws no curve at all until the engine has published a
/// frequency for every band. Offering a choice that leads to an empty plot is
/// worse than not offering it, so the list stops at ten.
const BAND_CHOICES: [usize; 2] = [5, 10];
```

**这段注释是错的**，而且错得很具体：它把「空白」归因于「引擎没有发布频率」。实际引擎对 15/20/31 都有一张**显式频率表**——`GraphicEqSetNumBands` 里 5/10/15/20/31 各有一段 `fCenter[]`，31 段填的就是完整的 ISO 栅格（20, 25, 31.5, …, 16000, 20000）。空白另有原因。

### 11.2 真正的原因：换完频段数，没人把频率抄回来

引擎内部换频段数时会把所有中心频率重算一遍。但**把结果抄进 `SharedParams` 的地方只有一处**：

```rust
// apply_preset()，载入预设时
let bands = dsp.num_bands() as usize;
params.set_num_bands(bands);
for band in 0..bands.min(MAX_BANDS) {
    params.set_band_gain(band, dsp.band_gain(band as i32) as f32);
    params.set_band_freq(band, dsp.band_freq(band as i32) as f32);   // ← 有回读
}
```

而音频线程上那个 applier——用户从界面改频段数时走的正是它——只把频段数推给引擎，**从来不往回读**：

```rust
// 修之前
let bands = params.num_bands();
if self.applied_bands != bands {
    dsp.set_num_bands(bands as i32);
    self.applied_bands = bands;
    self.applied_gains = [None; MAX_BANDS];   // ← 到这就结束了
}
```

于是 `params.band_freq(band)` 对新出现的频段一直是 `0.0`。而绘图函数一看到有频段是 0 就**直接返回**：

```rust
if freqs.iter().any(|hz| *hz <= 0.0) {
    // The engine has not published the grid yet … A curve over an invented
    // axis would be worse than an empty plot.
    return;
}
```

空白就是这么来的：**不是引擎没有栅格，是没人去取**。修法是在 applier 里补上同一段回读——和 `apply_preset` 做完全一样的事。

### 11.3 又抓到一个假绿测试（和 10.4 同一类）

补完回读，我加了两个测试，全绿。按惯例把修复删掉重跑——**还是全绿**。

原因和 10.4 一模一样：测试把 applier 的逻辑**内联抄了一遍**——

```rust
// 反例：这不是在测 applier，是在测自己抄的那段
dsp.set_num_bands(bands as i32);
for band in 0..bands.min(MAX_BANDS) {
    params.set_band_freq(band, dsp.band_freq(band as i32) as f32);   // 自己抄的回读
}
```

把生产代码里那半段删掉，测试自己抄的那半段还在，所以当然测不出来。

**修法是让测试去驱动真正的 `ParamApplier::apply()`**——那才是音频线程会调用的东西。改完之后再删一次修复，这次红了：

```
step 0, 5 bands: band 0 came back as 0 Hz, which makes the plot bail out as empty
```

正是用户看到的那片空白。这条断言现在是这个 bug 真正的看门人。

**这已经是同一类错误的第二个样本了（上一个见 10.4）。** 教训值得再写一遍：**测试内联重写生产逻辑，等于没测。** 判断方法很机械——把 bug 放回去跑一遍，绿的就是假的。

### 11.4 顺带发现的硬约束：一个进程只能有一个 `Dsp`

合并测试的过程中撞上一个卡死：三个测试各自建一个 `Dsp`，单独跑都过，一起跑第三个**卡住超过一分钟**。

原因是 vendored 引擎把频段数存在**进程级全局变量**里，不是句柄字段：

```cpp
int DFXP_GRAPHIC_EQ_NUM_BANDS = 31;              // DfxDspEq.cpp:32
…
DFXP_GRAPHIC_EQ_NUM_BANDS = num_bands;           // GraphicEqSetNumBands，GraphicEqSet.cpp:154
```

`dfxpEq` 那一族入口全都读它。所以两个引擎实例会共用这一个整数，一个句柄发起的 `sos` 段重建会落在另一个句柄正在读的数组上。症状是**卡死**，不是崩溃。

应用本身碰不到这件事——它只建**一个**引擎（音频线程上，活到进程结束）。但在测试里，每个 `#[test]` 各建一个 `Dsp` 看着完全正常，实际会踩。处理方式：

1. 把三个频段测试**合并成一个**，全部走同一个 `Dsp`——这也更贴近应用的真实用法。
2. 加一个 `ENGINE_LOCK` 把碰引擎的测试串行化（`engine::tests` 下的 `with_engine()`），锁中毒时同样不当作失败。
3. 在 `ffi::Dsp` 的文档注释里写明「**每个进程最多一个**」，免得下次再有人按直觉多建一个。

顺带一提，测试里顺行遍历（5→10→15→20→31）、回头再选、以及**下行**（31→20）都被覆盖到了；下行走的是 `GraphicEqSetNumBands` 里另一条分支（把旧增益按相对位置映射到更窄的布局上），值得单独走一遍。

### 11.5 质量门

四个门全绿，测试数 **69 → 70**（三个频段测试合并成 1 个，净 +1）：

| 门 | 结果 |
|---|---|
| `cargo check --all-targets --locked` | exit 0 |
| `cargo test --lib --locked` | 70 passed / 1 ignored / 0 failed |
| `cargo clippy --lib --all-targets --locked` | exit 0，无告警 |
| `cargo test --lib --release --locked` | 70 passed / 1 ignored / 0 failed |

另外单独验证过（这是 10.4／11.3 两次翻车的直接防线）：

- 把 applier 里的回读删掉，`band_counts_round_trip_with_a_full_grid` **红**，报的就是 `band 0 came back as 0 Hz`；注入完即还原。
- 合并前单跑 `shrinking_the_band_count_does_not_hang` 是绿的、一起跑就卡死——这条差异本身也是 11.4 的诊断依据。

### 11.6 一个环境上的坑（记下来省得下次再踩）

这轮调试里 `cargo test` 出现过一个**用旧产物**的假失败：我在进程外用 `Copy-Item` 把 `engine.rs` 还原回带修复的版本，文件内容是对的（`Contains('Publish the new grid')` 为 `True`），但 cargo 认为 debug 产物是最新的（`Finished in 0.75s`，没有重新编译），于是拿**删掉修复时编出来的那个二进制**去跑，报了红。等后续 `clippy` / `release` 两个门重新编译之后又全绿。

**教训：用文件工具在 cargo 背后改源码之后，要显式 invalidate。** 这里用 `(Get-Item …).LastWriteTime = Get-Date` 把时间戳推一下即可；只改内容不一定能触发重建。


