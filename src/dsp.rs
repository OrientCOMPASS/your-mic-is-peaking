//! 炸麦 DSP 核心 — 响度保持的硬削波失真（纯计算，可跨平台单测）。
//!
//! 目标：听感上「麦克风炸了」（重度削波失真、动态压平），但**实际音量与
//! 输入保持一致**——输出帧 RMS 自动匹配输入帧 RMS，绝不整体变大声。
//!
//! 算法（每帧两遍，原地处理，零分配、零宿主调用，实时安全）：
//!
//! 1. **Pass 1（统计）**：对未修改的帧计算输入均方 `in_ms` 与
//!    湿信号（`hard_clip(x · drive)`）均方 `wet_ms`。
//! 2. **响度匹配增益**：`target = sqrt(in_ms / wet_ms)`，即让湿信号缩放后
//!    的 RMS 恰好等于输入 RMS。经一阶平滑（τ = 20 ms）得到 `makeup`；
//!    静音帧（`in_ms` 低于门限）保持上一增益，避免 0/0 与噪声底放大；
//!    激活沿直接以当帧 target 初始化，首帧即匹配、无响度冲击。
//! 3. **Pass 2（应用）**：`out = x·(1−mix) + hard_clip(x·drive)·makeup·mix`。
//!
//! 开关为**瞬时切换**（无淡入淡出）：
//! * 关闭 → 立即旁路（数据一个字节都不碰）；开启 → 当帧即全量炸麦。
//! * 由于 makeup 在激活首帧就以当帧统计初始化，切换不会带来响度跳变；
//!   波形本身的接续点可能有一次样本级跳变——这与真实「麦被拍炸」的
//!   瞬间爆裂感一致，是本插件刻意保留的性格。
//!
//! 性质：
//! * 信号被削平 → 波形趋近方波 → 高次谐波（撕裂感）+ 帧内动态压平，
//!   这正是过载麦克风的声音特征；
//! * 每帧能量守恒（RMS 匹配）→ 对方听到的音量与你正常说话一致；
//! * 安静片段（`x·drive` 不触及削波）时 `target ≈ 1/drive`，输出 ≈ 输入，
//!   自然过渡为干净信号——只有真正「顶到天花板」的声音才会炸。

/// 宿主 DSP 链固定 48 kHz（micyou-plugin `PluginDspBridge`）。
pub const SAMPLE_RATE: f32 = 48_000.0;

/// 响度匹配增益的平滑时间常数（秒）。
pub const MAKEUP_TAU: f32 = 0.020;

/// 静音门限（均方值）：低于此电平不更新 makeup（RMS ≈ −100 dBFS）。
pub const SILENCE_GATE: f32 = 1e-10;

/// makeup 安全范围，防止病态帧产生极端增益。
pub const MIN_MAKEUP: f32 = 1.0 / 1024.0;
pub const MAX_MAKEUP: f32 = 8.0;

/// 每帧参数（音频线程从原子变量组装，无锁）。
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// 炸麦开关（快捷键 / 面板按钮）。
    pub active: bool,
    /// 湿推倍数（削波强度），建议 4..=64。
    pub drive: f32,
    /// 干湿混合比例 0..=1（1 = 纯炸麦）。
    pub mix: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            active: false,
            drive: 24.0,
            mix: 1.0,
        }
    }
}

/// 炸麦处理器状态。**只允许宿主实时音频线程触碰**（process 串行执行），
/// init/deinit 的重置发生在链注册/注销的静默窗口内。
pub struct Peaker {
    /// 平滑后的响度匹配增益；NaN = 待下一有效帧重新收敛。
    makeup: f32,
    /// 上一帧是否激活（检测激活沿）。
    was_active: bool,
}

impl Peaker {
    pub const fn new() -> Self {
        Self {
            makeup: f32::NAN,
            was_active: false,
        }
    }

    /// 恢复初始状态（init / deinit 时调用，此时音频线程不可能在 process 内）。
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// 当前响度匹配增益（调试 / 单测用；NaN 表示尚未收敛）。
    pub fn makeup(&self) -> f32 {
        self.makeup
    }

    /// 处理一帧交错 f32（原地）。返回 `false` 表示本帧应旁路（数据未动）。
    ///
    /// 零分配、零系统调用；两遍共 2N 次乘加 + N 次 clamp，480 样本帧
    /// 在现代 CPU 上远低于 1 µs，满足实时预算（10 ms）。
    pub fn process(&mut self, data: &mut [f32], channels: usize, p: &Params) -> bool {
        if data.is_empty() {
            return false;
        }
        // 瞬时开关：未激活 → 立即旁路，数据一个字节都不碰。
        if !p.active {
            self.was_active = false;
            return false;
        }
        let drive = if p.drive.is_finite() { p.drive.max(0.0) } else { 1.0 };
        let mix = if p.mix.is_finite() { p.mix.clamp(0.0, 1.0) } else { 1.0 };
        if mix <= 0.0 {
            // 全干 = 透明：直接旁路（makeup 状态不动，mix 调回即刻可用）。
            return false;
        }
        // 激活沿：丢弃旧增益，本帧统计直接初始化（首帧响度即匹配）。
        if !self.was_active {
            self.makeup = f32::NAN;
        }
        self.was_active = true;

        // ── Pass 1：统计（数据保持原样） ──
        let mut in_ms = 0.0f32;
        let mut wet_ms = 0.0f32;
        for &x0 in data.iter() {
            let x = if x0.is_finite() { x0 } else { 0.0 };
            let y = hard_clip(x * drive);
            in_ms += x * x;
            wet_ms += y * y;
        }
        let n = data.len() as f32;
        let in_mean = in_ms / n;
        let wet_mean = wet_ms / n;

        // ── 响度匹配增益（静音帧保持） ──
        if in_mean > SILENCE_GATE {
            let target = (in_mean / wet_mean.max(1e-12))
                .sqrt()
                .clamp(MIN_MAKEUP, MAX_MAKEUP);
            self.makeup = if self.makeup.is_nan() {
                target
            } else {
                let frames = (data.len() / channels.max(1)).max(1) as f32;
                let dt = frames / SAMPLE_RATE;
                let alpha = 1.0 - (-dt / MAKEUP_TAU).exp();
                self.makeup + (target - self.makeup) * alpha
            };
        }
        let makeup = if self.makeup.is_nan() { 1.0 } else { self.makeup };

        // ── Pass 2：失真 + 响度匹配 + 静态干湿混合 ──
        if mix >= 1.0 {
            for s in data.iter_mut() {
                let x = if s.is_finite() { *s } else { 0.0 };
                *s = hard_clip(x * drive) * makeup;
            }
        } else {
            let dry_w = 1.0 - mix;
            for s in data.iter_mut() {
                let x = if s.is_finite() { *s } else { 0.0 };
                *s = x * dry_w + hard_clip(x * drive) * makeup * mix;
            }
        }
        true
    }
}

/// 硬削波（炸麦的灵魂）：超出 ±1 直接拍平；NaN → 0，±inf → ±1。
#[inline]
fn hard_clip(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else if v > 1.0 {
        1.0
    } else if v < -1.0 {
        -1.0
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成 `frames` 帧（每帧 `frame` 样本）正弦测试信号并逐帧处理，
    /// 返回 (输入, 输出)。旁路帧按原样拷贝（宿主语义）。
    fn run_sine(
        freq: f32,
        amp: f32,
        frames: usize,
        frame: usize,
        p: &Params,
        activate_at: Option<usize>,
    ) -> (Vec<f32>, Vec<f32>) {
        let mut peaker = Peaker::new();
        let total = frames * frame;
        let input: Vec<f32> = (0..total)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE).sin())
            .collect();
        let mut output = vec![0f32; total];
        let mut params = *p;
        for f in 0..frames {
            if let Some(at) = activate_at {
                params.active = f >= at;
            }
            let mut buf = input[f * frame..(f + 1) * frame].to_vec();
            peaker.process(&mut buf, 1, &params);
            output[f * frame..(f + 1) * frame].copy_from_slice(&buf);
        }
        (input, output)
    }

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / v.len() as f64).sqrt()
    }

    /// Goertzel 单频点功率（相对值，仅用于谐波比例断言）。
    fn tone_power(data: &[f32], freq: f32) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq as f64 / SAMPLE_RATE as f64;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0f64, 0f64);
        for &x in data {
            let s = x as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        s1 * s1 + s2 * s2 - coeff * s1 * s2
    }

    #[test]
    fn idle_bypasses_and_leaves_data_untouched() {
        let mut peaker = Peaker::new();
        let mut buf = [0.25f32, -0.5, 0.75, -1.0];
        let before = buf;
        let p = Params { active: false, drive: 24.0, mix: 1.0 };
        assert!(!peaker.process(&mut buf, 1, &p));
        assert_eq!(buf, before);
    }

    #[test]
    fn loudness_matches_input_in_steady_state() {
        // 1 kHz、-20 dBFS（amp 0.1）正弦，drive 24 → 深度削波。
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let (input, output) = run_sine(1000.0, 0.1, 200, 480, &p, None);
        // 跳过收敛段，取最后 100 帧（1 s，整周期 → 无泄漏）。
        let tail_in = &input[100 * 480..];
        let tail_out = &output[100 * 480..];
        let ratio = rms(tail_out) / rms(tail_in);
        assert!(
            (ratio - 1.0).abs() < 0.03,
            "输出/输入 RMS 比 {ratio} 应≈1（响度保持）"
        );
    }

    #[test]
    fn distortion_is_clearly_audible() {
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let (input, output) = run_sine(1000.0, 0.1, 200, 480, &p, None);
        let tail_in = &input[100 * 480..];
        let tail_out = &output[100 * 480..];

        // 3 次谐波能量显著（方波 h3/h1 幅度比 = 1/3 → 功率比 ≈ 1/9）。
        let p1 = tone_power(tail_out, 1000.0);
        let p3 = tone_power(tail_out, 3000.0);
        assert!(p3 / p1 > 0.01, "h3/h1 功率比 {} 太小（不像炸麦）", p3 / p1);

        // 波峰因数被压平：正弦 1.414 → 削波后趋近方波 1.0~1.15。
        let crest_in = tail_in.iter().map(|x| x.abs()).fold(0f32, f32::max) as f64 / rms(tail_in);
        let crest_out = tail_out.iter().map(|x| x.abs()).fold(0f32, f32::max) as f64 / rms(tail_out);
        assert!(crest_out < crest_in - 0.15, "波峰因数 {crest_out} 未压平");
        assert!(crest_out < 1.25, "波峰因数 {crest_out} 应接近方波");
    }

    #[test]
    fn loudness_tracks_input_envelope() {
        // 幅度指数爬升（每帧 ×1.008）的正弦：输出 RMS 应逐段跟随输入 RMS。
        let mut peaker = Peaker::new();
        let frames = 400;
        let frame = 480;
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let mut amp = 0.05f32;
        let mut in_rms = Vec::new();
        let mut out_rms = Vec::new();
        for _ in 0..frames {
            let buf_in: Vec<f32> = (0..frame)
                .map(|i| amp * (2.0 * std::f32::consts::PI * 997.0 * i as f32 / SAMPLE_RATE).sin())
                .collect();
            let mut buf = buf_in.clone();
            peaker.process(&mut buf, 1, &p);
            in_rms.push(rms(&buf_in));
            out_rms.push(rms(&buf));
            amp *= 1.008;
        }
        // 跳过收敛段后，每帧输出/输入 RMS 比应在 ±12% 内（平滑滞后 ≤ τ=20ms）。
        let mut worst = 0f64;
        for f in 20..frames {
            let r = out_rms[f] / in_rms[f];
            worst = worst.max((r - 1.0).abs());
        }
        assert!(worst < 0.12, "包络跟随最大偏差 {worst} 超过 12%");
    }

    #[test]
    fn silence_in_silence_out_and_noise_floor_never_boosted() {
        // 纯零帧：输出必须仍为零、无 NaN，makeup 保持不动。
        let mut peaker = Peaker::new();
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let mut zeros = [0f32; 480];
        assert!(peaker.process(&mut zeros, 1, &p));
        assert!(zeros.iter().all(|&x| x == 0.0));
        assert!(peaker.makeup().is_nan(), "静音帧不应初始化 makeup");

        // −80 dBFS 噪声底：输出 RMS 不得超过输入（不放大底噪）。
        let noise_amp = 1e-4;
        let (_, output) = run_sine(1000.0, noise_amp, 100, 480, &p, None);
        let expected = noise_amp as f64 / 2f64.sqrt();
        assert!(
            rms(&output[50 * 480..]) < expected * 1.5,
            "噪声底被放大了"
        );
    }

    #[test]
    fn activation_is_immediate_and_loudness_matched() {
        // 瞬时开启：第一个激活帧就必须是完全体炸麦（无渐入），且响度已匹配。
        let p = Params { active: false, drive: 24.0, mix: 1.0 };
        let amp = 0.3;
        let (input, output) = run_sine(1000.0, amp, 60, 480, &p, Some(20));

        // 激活帧之前的输出 == 输入（旁路原样）。
        assert_eq!(&input[..20 * 480], &output[..20 * 480]);

        // 第一个激活帧：削波样本必须已经拍在 ±makeup 平台上（无淡入斜坡）。
        let first = &output[20 * 480..21 * 480];
        let dry = &input[20 * 480..21 * 480];
        let plateau = first.iter().map(|x| x.abs()).fold(0f32, f32::max);
        let mut clipped_count = 0;
        for i in 0..480 {
            if (dry[i] * 24.0).abs() >= 1.0 {
                clipped_count += 1;
                assert!(
                    (first[i].abs() - plateau).abs() < 1e-6,
                    "样本 {i} 未在首帧即达到削波平台（疑似渐入）"
                );
            }
        }
        assert!(clipped_count > 100, "测试信号应深度削波");

        // 首帧响度即匹配（makeup 当帧初始化）：RMS 比 ∈ 1±8%，峰值无冲击。
        let ratio = rms(first) as f64 / rms(dry) as f64;
        assert!((ratio - 1.0).abs() < 0.08, "首帧 RMS 比 {ratio}");
        let peak_in = input.iter().map(|x| x.abs()).fold(0f32, f32::max);
        let peak_out = output.iter().map(|x| x.abs()).fold(0f32, f32::max);
        assert!(
            peak_out < peak_in + 0.05,
            "激活瞬间出现响度冲击：peak_out={peak_out} peak_in={peak_in}"
        );
    }

    #[test]
    fn deactivation_bypasses_immediately_and_data_untouched() {
        // 瞬时关闭：下一次 process 立即旁路，数据逐位原样——确定性断言，
        // 不存在任何波形启发式（旧淡出测试的样本差启发式已被整体移除）。
        let mut peaker = Peaker::new();
        let on = Params { active: true, drive: 24.0, mix: 1.0 };
        let off = Params { active: false, drive: 24.0, mix: 1.0 };
        let frame = 480;
        let mut phase = 0usize;
        let sine_frame = |phase: &mut usize| -> Vec<f32> {
            let buf: Vec<f32> = (0..frame)
                .map(|i| {
                    0.3 * (2.0 * std::f32::consts::PI * 1000.0 * (*phase + i) as f32
                        / SAMPLE_RATE)
                        .sin()
                })
                .collect();
            *phase += frame;
            buf
        };
        for _ in 0..40 {
            let mut buf = sine_frame(&mut phase);
            assert!(peaker.process(&mut buf, 1, &on));
        }
        // 关闭：立即旁路 + 数据逐位不变，连续多帧稳定。
        for _ in 0..3 {
            let dry = sine_frame(&mut phase);
            let mut buf = dry.clone();
            assert!(!peaker.process(&mut buf, 1, &off), "关闭后应立即旁路");
            assert_eq!(buf, dry, "旁路帧数据必须逐位原样");
        }
        // 重新开启：又是当帧全量生效。
        let mut buf = sine_frame(&mut phase);
        assert!(peaker.process(&mut buf, 1, &on));
    }

    #[test]
    fn output_matches_reference_formula_exactly() {
        // mix=1 时输出必须逐位等于 hard_clip(x·drive)·makeup（makeup 为
        // 帧后读数：Pass 1 先更新、Pass 2 使用同一值）。
        let mut peaker = Peaker::new();
        let p = Params { active: true, drive: 17.0, mix: 1.0 };
        let frame = 480;
        let mut phase = 0usize;
        for f in 0..50 {
            let dry: Vec<f32> = (0..frame)
                .map(|i| {
                    0.25 * (2.0 * std::f32::consts::PI * 1000.0 * (phase + i) as f32
                        / SAMPLE_RATE)
                        .sin()
                })
                .collect();
            phase += frame;
            let mut buf = dry.clone();
            assert!(peaker.process(&mut buf, 1, &p), "帧 {f} 应被处理");
            let makeup = peaker.makeup();
            for i in 0..frame {
                let expected = hard_clip(dry[i] * 17.0) * makeup;
                assert_eq!(buf[i], expected, "帧 {f} 样本 {i} 与参考公式不一致");
            }
        }
    }

    #[test]
    fn mix_zero_bypasses_transparently() {
        // mix=0：直接旁路（瞬时切换语义下无淡入过程，全程数据原样）。
        let mut peaker = Peaker::new();
        let p = Params { active: true, drive: 24.0, mix: 0.0 };
        let mut buf = vec![0.2f32; 480];
        let before = buf.clone();
        assert!(!peaker.process(&mut buf, 1, &p));
        assert_eq!(buf, before);
        // mix=0.5：干湿各半（相对纯湿响度上浮，属预期）。
        let half = Params { active: true, drive: 24.0, mix: 0.5 };
        let mut buf2 = vec![0.2f32; 480];
        assert!(peaker.process(&mut buf2, 1, &half));
    }

    #[test]
    fn drive_one_is_clean_and_loudness_neutral() {
        // drive=1：不削波，makeup → 1，输出 ≈ 输入。
        let p = Params { active: true, drive: 1.0, mix: 1.0 };
        let (input, output) = run_sine(1000.0, 0.2, 100, 480, &p, None);
        let tail_in = &input[50 * 480..];
        let tail_out = &output[50 * 480..];
        let ratio = rms(tail_out) / rms(tail_in);
        assert!((ratio - 1.0).abs() < 0.02, "drive=1 RMS 比 {ratio}");
        let p3 = tone_power(tail_out, 3000.0);
        let p1 = tone_power(tail_out, 1000.0);
        assert!(p3 / p1 < 1e-6, "drive=1 不应产生谐波");
    }

    #[test]
    fn nan_and_inf_inputs_are_sanitized() {
        let mut peaker = Peaker::new();
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let mut buf = vec![0.2f32; 480];
        buf[10] = f32::NAN;
        buf[20] = f32::INFINITY;
        buf[30] = f32::NEG_INFINITY;
        assert!(peaker.process(&mut buf, 1, &p));
        assert!(buf.iter().all(|x| x.is_finite()), "输出含非有限值");
        assert!(peaker.makeup().is_finite(), "makeup 被 NaN 污染");
    }

    #[test]
    fn hot_input_stays_bounded() {
        // 输入本身已过载（±2 方波）：输出仍被 makeup 压回输入 RMS 量级。
        let mut peaker = Peaker::new();
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let frame = 480;
        let mut last_out = Vec::new();
        for f in 0..50 {
            let mut buf: Vec<f32> = (0..frame)
                .map(|i| if (i + f * frame) % 96 < 48 { 2.0 } else { -2.0 })
                .collect();
            peaker.process(&mut buf, 1, &p);
            last_out = buf;
        }
        let out_rms = rms(&last_out);
        assert!(
            (out_rms - 2.0).abs() < 0.2,
            "输出 RMS {out_rms} 应匹配输入 RMS 2.0"
        );
        let peak = last_out.iter().map(|x| x.abs()).fold(0f32, f32::max);
        assert!(peak < 2.6, "输出峰值 {peak} 越界");
    }

    #[test]
    fn stereo_frames_are_handled() {
        // 双声道交错：统计与处理均不崩，响度仍匹配。
        let mut peaker = Peaker::new();
        let p = Params { active: true, drive: 24.0, mix: 1.0 };
        let frame = 240; // 480 交错样本 = 240 帧 × 2ch
        let mut in_rms_acc = Vec::new();
        let mut out_rms_acc = Vec::new();
        for f in 0..200 {
            let mut buf: Vec<f32> = (0..frame * 2)
                .map(|i| {
                    0.1 * (2.0 * std::f32::consts::PI * 1000.0 * (f * frame * 2 + i) as f32
                        / SAMPLE_RATE)
                        .sin()
                })
                .collect();
            let input = buf.clone();
            peaker.process(&mut buf, 2, &p);
            if f >= 100 {
                in_rms_acc.push(rms(&input));
                out_rms_acc.push(rms(&buf));
            }
        }
        let mean_in = in_rms_acc.iter().sum::<f64>() / in_rms_acc.len() as f64;
        let mean_out = out_rms_acc.iter().sum::<f64>() / out_rms_acc.len() as f64;
        let ratio = mean_out / mean_in;
        assert!((ratio - 1.0).abs() < 0.03, "立体声 RMS 比 {ratio}");
    }
}
