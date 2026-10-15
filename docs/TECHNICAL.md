# 技术设计 — 响度保持的一键炸麦

本文记录 `opss.your-mic-is-peaking` 的 DSP 设计与工程决策。核心实现在
[`src/dsp.rs`](../src/dsp.rs)（纯计算、可单测），FFI / 会话状态在
[`src/lib.rs`](../src/lib.rs)。

## 1. 需求拆解

「炸麦」= 麦克风过载的声音特征：

* 波形顶部被拍平（硬削波）→ 高次谐波丰富、听感撕裂；
* 帧内动态被压平 → 趋近方波，波峰因数（crest factor）从正弦的 1.414 掉到 ≈1.0；
* 一切发生在**不改变整体音量**的前提下（需求：实际音量保持输入音量大小）。

因此本插件 = **硬削波失真 + 逐帧 RMS 响度匹配**，外加零延迟的瞬时开关。

## 2. 每帧处理流程（两遍，原地，零分配）

宿主契约：`micyou_plugin_process(data, samples, channels, queued_ms, bypass)`，
48 kHz 固定采样率，`samples` 为交错样本总数，帧长由宿主决定（实测 480~960）。

```text
Pass 1（统计，数据不动）
    in_ms  = Σ x² / N                       # 输入均方
    wet_ms = Σ hard_clip(x·drive)² / N      # 湿信号均方

响度匹配增益
    if in_ms > SILENCE_GATE:                # 静音帧保持上一增益（防 0/0、防底噪放大）
        target = clamp( sqrt(in_ms / wet_ms), MIN_MAKEUP, MAX_MAKEUP )
        makeup = isnan(makeup) ? target     # 激活首帧直接收敛 → 无响度冲击
                               : makeup + (target − makeup)·α
        α = 1 − exp(−dt / τ),  τ = 20 ms,  dt = 帧时长

Pass 2（应用）
    out = x·(1−mix) + hard_clip(x·drive)·makeup·mix    # 静态干湿混合

开关（瞬时，无淡变）
    active=false → 立即旁路（bypass，数据不碰）
    active=true  → 当帧全量生效；makeup 于激活首帧以当帧统计初始化
```

稳态时 `mean(out²) = wet_ms · makeup² = in_ms`——**输出帧 RMS 恒等于输入帧
RMS**，响度保持是数学恒等式而非近似；τ=20 ms 的一阶平滑只影响动态突变时
的收敛速度（包络跟随偏差实测 < 12%，见单测 `loudness_tracks_input_envelope`）。

## 3. 关键决策

| 决策 | 理由 |
| --- | --- |
| 硬削波（而非 tanh 软饱和） | 软饱和听感是「温暖的过载」；炸麦需要的是拍平的方波撕裂感。谐波结构上硬削波 ≈ 真实麦克风胶囊/前置放大过载 |
| 对称削波 ±1 | 非对称削波会引入直流分量，吃掉 RMS 预算并可能在开关时产生噗声；对称削波对交流信号无直流残留 |
| 两遍法而非「上一帧增益」 | 单遍法用上一帧统计会有 10 ms 的响度滞后，激活首帧可能满幅爆一下；两遍法当帧统计当帧用，首帧即精确匹配（clip 重算一次的成本可忽略：480 样本 ×2 ≪ 1 µs） |
| RMS 匹配而非峰值匹配 | 需求是「音量」保持——响度感知对应 RMS/能量；峰值匹配会让深度削波时输出变得极小声 |
| 静音门限（−100 dBFS）| `0/0` 数值保护 + 不把底噪当信号放大；门限以下 makeup 冻结，说话恢复时增益立即可用 |
| makeup 限幅 [1/1024, 8] | 病态帧（极端电平）下的安全网；正常语音 RMS 范围内永不触顶 |
| 瞬时开关（无淡入淡出） | 一键炸麦要的就是零延迟的爆裂感：关闭当帧旁路、开启当帧全量。响度不会跳变（makeup 激活首帧即按当帧统计初始化）；波形接续点的一次样本级跳变正是「麦被拍炸」瞬间的性格，刻意保留。τ=20 ms 的 makeup 平滑只作用于响度匹配，足够快以跟上音节动态、又足够慢以消除逐帧增益抖动（泵机效应） |

### 自然感：安静片段不炸

`drive=24` 时，只有 `|x| > 1/24 ≈ 0.042` 的样本触及削波。耳语级输入
（RMS ≈ 0.001）完全不削波，此时 `target = 1/drive`，输出 ≈ 输入——安静片段
保持干净，响一点的语音才逐渐「炸」起来，与真实麦克风过载的行为一致，也避免
把底噪炸成持续嘶吼。

## 4. 实时安全

* `process` 全程：3 次原子读（active/drive/mix）+ 两遍逐样本乘加，无堆分配、
  无锁、无系统调用、无宿主 API 调用；`exp` 每帧一次；
* 处理状态 `Peaker`（makeup/was_active）放在 `UnsafeCell` 静态中，仅宿主
  实时音频线程触碰（`process` 串行执行）；init/deinit 的重置发生在链注册/注销
  的静默窗口内（宿主注销 DSP 节点会等待在途帧结束）；
* 所有 FFI 入口 `catch_unwind`，panic 绝不跨 ABI 边界（cdylib 与宿主同进程，
  `panic=abort` 被明确禁止——见 Cargo.toml 注释）;
* 输入含 NaN/±inf 时按 0 处理并输出有限值；宿主在节点输出后还有一道
  NaN 消毒（`micyou-plugin/dsp.rs run_node`），双保险；
* 空闲快速路径：未激活 → 当帧立即 `bypass=1`，宿主整帧跳过，零开销。

## 5. 线程与状态

| 线程 | 触碰 | Host API |
| --- | --- | --- |
| 宿主总线线程（init / deinit / handle_message） | SESSION（Mutex）、参数原子写、PEAKER 重置 | ✅ 仅此三处 |
| 宿主实时音频线程（process） | PEAKER、参数原子读 | ❌ 绝不 |

消息路由：

* `hotkey:<id>` → 校验句柄 id 为当前有效注册后 toggle（ABI 无 unregister，
  改键后旧注册的残留消息按 id 过滤）；
* `ui:toggle` / `ui:on` / `ui:off` → 面板按钮；`ui:apply` → 配置热更新
  （drive/mix/notify 原子写 + 快捷键活体重规划）；
* 状态经 `ymip_status` 配置键持久化，面板每 1 s 轮询 `get_config` 渲染。

## 6. 测试

DSP 核心是纯函数式的 `Peaker::process`，单测在任何平台 `cargo test --release`
即可运行（CI 三平台矩阵各跑一遍）：

| 用例 | 断言 |
| --- | --- |
| `loudness_matches_input_in_steady_state` | 稳态输出/输入 RMS 比 ∈ 1±3% |
| `distortion_is_clearly_audible` | Goertzel h3/h1 功率比 > 1%；波峰因数被压平（< 1.25 且显著低于输入） |
| `loudness_tracks_input_envelope` | 幅度指数爬升时每帧 RMS 跟随偏差 < 12% |
| `silence_in_silence_out_and_noise_floor_never_boosted` | 零输入零输出、makeup 不初始化；−80 dBFS 底噪不被放大 |
| `activation_is_immediate_and_loudness_matched` | 激活首帧削波样本已全部拍在 ±makeup 平台（无渐入）、RMS 比 ∈ 1±8%、全程峰值不超过输入峰值（无响度冲击） |
| `deactivation_bypasses_immediately_and_data_untouched` | 关闭后下一次 process 立即旁路且数据逐位原样（确定性断言，无波形启发式），重开当帧再生效 |
| `output_matches_reference_formula_exactly` | 稳态输出逐位等于 `hard_clip(x·drive)·makeup` 参考公式 |
| `mix_zero_bypasses_transparently` | mix=0 直接旁路、数据逐位原样 |
| `drive_one_is_clean_and_loudness_neutral` | drive=1 无谐波、RMS 比 1±2% |
| `nan_and_inf_inputs_are_sanitized` | 病态输入下输出全有限 |
| `hot_input_stays_bounded` | 已过载输入（±2 方波）输出 RMS 仍匹配、峰值有界 |
| `stereo_frames_are_handled` | 双声道交错帧响度匹配同样成立 |
