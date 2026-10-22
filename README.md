# 💥 YourMicIsPeaking — MicYou 一键炸麦插件

一键把麦克风**炸了**：全局快捷键（或面板大按钮）瞬间将你的声音推入硬削波破音失真——但**实际音量保持输入音量大小**：输出响度逐帧自动匹配输入 RMS，对方听到的音量和平时一样，只是声音「炸」了。

| | |
|---|---|
| 插件 id | `opss.your-mic-is-peaking` |
| 运行时 | Native（cdylib，C ABI v1 / apiVersion 1） |
| 平台 | **Windows（x86_64）· Linux（x86_64）· macOS（arm64）**；三平台动态库同包分发，宿主按平台自选 |
| 插件类型 | `dsp`（实时处理链节点，`realtimeSafe: true`） |
| 权限 | `dsp.node`、`config.read`、`config.write`（最小化，无网络/剪贴板/文件权限） |
| 构建 | GitHub Actions · windows-latest / ubuntu-latest / macos-latest |
| 依赖 | 无外部运行库（纯 Rust + serde_json；单文件 ~300 KB） |
| 许可 | Unlicense（公有领域） |

## 安装

1. 下载 Release 中的 `plugin.zip`；开发版见每次 push 构建的 Actions artifact `your-mic-is-peaking-development`
2. 把 zip 内容解压到插件目录的 `opss.your-mic-is-peaking/` 子目录（Windows `%APPDATA%\micyou\plugins\`，Linux/macOS `~/.config/micyou/plugins/`）；包内含三个动态库（`.dll/.so/.dylib`），**宿主按平台自动选用**
3. 回到插件页点「刷新」，启用 **YourMicIsPeaking**
4. 启用后宿主会自动把插件节点插入处理链（可在「设置 → 音频 → 处理链」拖到末尾 / VAD 之后，效果最炸）
5. 应用内「检查更新」走 `updateUrl`（release 资产 `plugin.json`；宿主按 `.json→.zip` 推导下载 `plugin.zip`）

## 使用

1. 按 **`Ctrl+Shift+F9`**（默认，可改）→ 系统通知「💥 炸麦开启」，麦克风立刻进入破音失真状态
2. 在**任何**应用里再按同一快捷键 → 「🎙️ 炸麦关闭」，10 ms 淡出恢复干净声音，无咔哒
3. 也可以打开侧边栏面板（💥 YourMicIsPeaking）用大按钮开关、实时调「炸裂强度」与「效果比例」

> **效果只在串流运行时送达对方**：MicYou 需要已连接手机/Web 端（音频线程在处理链上调用本插件）。未串流时开关状态照常切换，声音在开始串流后生效。

## 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `hotkey` | `ctrl+shift+f9` | 全局快捷键（修饰键 + 字母/数字/F1–F24/常用键）。修改后立即生效 |
| `drive` | 24 | 炸裂强度：把信号推入硬削波的倍数。4 = 轻微破音，24 = 典型炸麦，64 = 完全方波 |
| `mix` | 1.0 | 效果比例：100% = 纯炸麦；调低与干净信号混合 |
| `notify` | true | 开关时弹系统通知 |

## 原理（响度保持的炸麦）

详见 [docs/TECHNICAL.md](docs/TECHNICAL.md)。一句话版本：

1. **削波失真**：`y = hard_clip(x · drive)` —— 波形顶被拍平成方波状，产生大量高次谐波（撕裂感）与帧内动态压平，这正是过载麦克风的声音特征；
2. **响度匹配**：每帧统计输入均方与湿信号均方，计算 `makeup = sqrt(in_ms / wet_ms)`（τ = 20 ms 平滑、静音门限保持、激活首帧即收敛），输出 `y · makeup` 的 RMS **恰好等于输入 RMS**——音量不变，只有音色被炸；
3. **无感开关**：10 ms 干湿交叉淡变包络，开/关都不会有咔哒声。

全程零分配、零宿主调用、无锁参数读取，满足实时 DSP 规范（单帧处理 ≪ 1 ms）。

## 平台与已知限制

| 平台 | 快捷键 | 说明 |
|---|---|---|
| Windows | ✅ | 全局快捷键由宿主 global-hotkey 注册 |
| Linux (X11) | ✅ | — |
| Linux (Wayland) | ❌ | 合成器不转发 X11 全局抓取——请用面板大按钮 |
| macOS | ✅ | — |

## 开发

```bash
cargo test --release     # DSP 核心单元测试（响度匹配 / 谐波 / 淡变 / 边界）
cargo build --release    # 产物 target/release/libyour_mic_is_peaking.{so,dylib} / your_mic_is_peaking.dll
```

CI（`.github/workflows/release.yml`）：

- push 到 `main` / `dev` → 三平台矩阵编译 + 单测，产物上传为 artifact `your-mic-is-peaking-development`（可直接解压安装的完整插件包）
- Actions 页面手动 Run workflow → 打 tag 并创建 Release（资产 `plugin.zip` + `plugin.json`）

## 许可

Unlicense — 见 [LICENSE](LICENSE)。
