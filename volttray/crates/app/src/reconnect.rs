//! 重枚举与重连裁决（v0.3.9，D-65）：无设备态退避重试、「立即刷新」强制
//! 重枚举、句柄失效自愈的纯逻辑唯一裁决点。
//!
//! v0.3.10（D-67）新增 [`ChannelLiveness`]：通道句柄活性改按「轮」裁决
//! （整轮零应答才记一次连击），修复按「次」累计导致的多设备同睡误判死
//! （P1）；唤醒探测静默单独计数（P3）。
//!
//! v0.3.11（F2，D-68）新增 [`SlotScanStreak`]：通道级「扫槽连败」裁决——
//! 修复「0 槽位 + 死缓存句柄」死锁（槽位为 0 时 ChannelLiveness 永不裁决，
//! 缓存句柄永不重开）。连续 [`SLOT_SCAN_REOPEN_THRESHOLD`] 轮「扫槽全败且
//! 至少一次 I/O 类错误」才强制重开句柄；纯超时轮清零（深睡不误判死）。
//!
//! 背景（真机报告：系统重启后托盘常驻「?」且手动刷新无效）：
//! - v0.3.8 的 IO 循环在开机自启早于 USB 接收器/驱动就绪时枚举到 0 个设备，
//!   随后按轮询间隔（默认 600s）睡眠——「无设备」态下一次自动重试最晚
//!   10 分钟后才发生（启动时序竞争，怀疑方向 1）；
//! - 「立即刷新」仅 `continue` 回到循环头，重枚举受 30s 门控/通道非空跳过，
//!   通道存在但槽位扫描落空时刷新只重复查询缓存句柄（怀疑方向 3）；
//! - HID 句柄打开后缓存复用，驱动重载/接收器重枚举后句柄失效，所有查询
//!   打到死句柄上，3 连败只移除槽位、死通道永驻，重扫描永远落空（怀疑方向 2）。
//!
//! 零鼠标电池约束：本模块裁决的「枚举」全部是主机侧 HID API 操作
//! （`HidApi::new` + 设备列表遍历），不产生任何射频流量；无设备态退避
//! 重试只放松枚举节奏（30s → 60s → 120s 封顶），**不提高**任何查询频率；
//! 对已注册的休眠设备不新增任何主动探测（D-33 休眠 Skip 语义不变）。

use std::time::{Duration, Instant};

/// 有通道（或正常态）时的例行重枚举间隔（沿用 v0.2.0 以来的 30s 门控）。
pub const REENUM_INTERVAL: Duration = Duration::from_secs(30);

/// 无设备态（枚举不到任何 HID++ 候选通道）的退避重试档位：30s → 60s → 120s 封顶。
/// 仅主机侧枚举、不向设备发任何查询，不违反零耗电约束。
pub const ENUM_BACKOFF_STEPS: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];

/// 连续非超时类硬失败（I/O 错误等）达到该次数即判定通道句柄失效：
/// 作废缓存句柄并强制重新枚举一次（主机侧操作，零射频成本）。
/// v0.3.10（D-67）起同时作为「整轮零应答静默连击」的判死轮数。
pub const HANDLE_INVALIDATE_THRESHOLD: u32 = 3;

/// 唤醒探测静默连击判死轮数（v0.3.10，D-67）：设备已知休眠，探测静默
/// 是常态（深睡唤醒慢），单次偶发超时不足以判句柄死——放宽到 2 轮，
/// 代价上限是偶发假离线 ≤ 一个重枚举周期（30s）自愈。
pub const WAKE_PROBE_INVALIDATE_THRESHOLD: u32 = 2;

/// 枚举结果的态迁移（供 IO 线程写诊断日志/事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumTransition {
    /// 本次枚举后进入「无设备」态（此前有设备）。
    EnteredNoDevice,
    /// 本次枚举后退出「无设备」态（重新枚举到设备，恢复正常轮询）。
    ExitedNoDevice,
}

/// 一轮轮询中单个槽位的查询结果分类（v0.3.10，D-67），供
/// [`ChannelLiveness`] 做「整轮零应答」裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOutcome {
    /// 成功读到电量。
    Success,
    /// 有字节应答但无法产出电量（错误帧 / 格式异常 / 映射失败 / 设备消失
    /// 通知等）——证明通道句柄活着，只是这台设备给不出电量。
    Responded,
    /// 查询超时（设备休眠与句柄死亡无法区分，只能按轮裁决）。
    Timeout,
    /// I/O 错误（句柄级证据；同样按轮裁决，避免单次抖动误杀）。
    IoError,
}

/// 通道句柄活性裁决（v0.3.10，D-67）：按「轮」累计，不再按「次」。
///
/// v0.3.9 的通道级超时连击按次累计且跨槽位不去重：同一接收器挂 ≥2 台
/// 设备同时休眠时，第 2 轮轮询内连击就达阈值，把正常休眠误判成句柄死亡
/// （设备假离线、槽位缓存作废，且此后每 30s 对休眠槽位主动握手探测——
/// 违反零鼠标电池红线）。本裁决把口径改为：
///
/// - **整轮零应答才记 1 次静默连击**：一轮内所有被查询槽位全部
///   Timeout/IoError、没有任何 Success/Responded（字节应答证明句柄活着，
///   立即清零）；本轮无可查询槽位（全部休眠 Skip）不裁决、计数冻结；
/// - 普通轮询静默连击连续达 [`HANDLE_INVALIDATE_THRESHOLD`] 轮才判死；
/// - **唤醒探测单独计数**（设备已知休眠、静默是常态）：连续达
///   [`WAKE_PROBE_INVALIDATE_THRESHOLD`] 轮探测全静默才判死——单次偶发
///   探测超时不再导致整通道重建（≤30s 自愈场景也不发生）；
/// - 判死返回 true，由 IO 线程作废通道（句柄 + 槽位缓存）并强制重枚举
///   自愈（主机侧操作，零射频）。
///
/// 每个通道一个实例（IO 线程按 ChannelState 持有），Instant 无关、纯计数。
#[derive(Debug, Clone, Default)]
pub struct ChannelLiveness {
    /// 连续「普通轮询整轮零应答」的轮数。
    silent_rounds: u32,
    /// 连续「唤醒探测整轮静默」的轮数。
    probe_silent_rounds: u32,
}

impl ChannelLiveness {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前普通轮询静默连击轮数（日志/测试观测用）。
    pub fn silent_rounds(&self) -> u32 {
        self.silent_rounds
    }

    /// 当前唤醒探测静默连击轮数（日志/测试观测用）。
    pub fn probe_silent_rounds(&self) -> u32 {
        self.probe_silent_rounds
    }

    /// 登记一轮轮询结果。`normal` = 本轮对非休眠槽位发起查询的结果；
    /// `probe` = 本轮对休眠槽位发起唤醒探测的结果（两类都可能为空——
    /// 全部 Skip 的轮次两个都传空切片，不裁决、计数冻结）。
    /// 返回 true = 判定通道句柄已死，应作废并强制重枚举。
    pub fn note_round(&mut self, normal: &[SlotOutcome], probe: &[SlotOutcome]) -> bool {
        let alive = normal
            .iter()
            .chain(probe.iter())
            .any(|o| matches!(o, SlotOutcome::Success | SlotOutcome::Responded));
        if alive {
            // 有字节应答 = 句柄活着，两类连击都清零。
            self.silent_rounds = 0;
            self.probe_silent_rounds = 0;
            return false;
        }
        // 此后 normal / probe 中只可能剩 Timeout / IoError。
        if !normal.is_empty() {
            self.silent_rounds = self.silent_rounds.saturating_add(1);
        }
        if !probe.is_empty() {
            self.probe_silent_rounds = self.probe_silent_rounds.saturating_add(1);
        }
        self.silent_rounds >= HANDLE_INVALIDATE_THRESHOLD
            || self.probe_silent_rounds >= WAKE_PROBE_INVALIDATE_THRESHOLD
    }
}

/// 连续「扫槽全败且含 I/O 类错误」触发强制重开的轮数（v0.3.11，F2/D-68）。
pub const SLOT_SCAN_REOPEN_THRESHOLD: u32 = 3;

/// Raw Input 唤醒信号触发强制重枚举的最小间隔（v0.3.11，F5）：鼠标持续
/// 移动时唤醒事件每 2s 一次（GUI 侧合并），0 注册槽位期间若每次都强制
/// 枚举会形成枚举风暴——不足该间隔的事件合并（不置位），留待下一次
/// 循环唤醒点统一裁决（与 sleeping_ids 唤醒路径的 2s 合并同族做法）。
pub const WAKE_ENUM_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// 槽位扫描失败的类别（v0.3.11，F1/F2）：区分「设备不应答」与「句柄可疑」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotScanFailure {
    /// 超时类：设备休眠 / 离线 / 不应答——句柄死亡之外有更平常的解释。
    Timeout,
    /// I/O 类错误（含设备消失）：缓存句柄可疑的直接证据。
    Io,
    /// 其它失败（协议错误帧 / 电量未知等）：设备或接收器回了字节，
    /// 句柄本身大概率活着。
    Other,
}

/// 通道级「扫槽连败」裁决（v0.3.11，F2/D-68）——修复「0 槽位 + 死缓存
/// 句柄」死锁：重启后槽位注册不上时 ChannelLiveness 的轮询 outcomes 恒为
/// 空、永不裁决，invalidate_handles 永不触发，30s 重扫永远复用已失效的
/// 缓存句柄（永久「?」，唯一出口是手动刷新）。本裁决在**扫槽路径**上补出
/// 句柄死亡的观测点：
///
/// - **跨槽位去重、按轮裁决**：一轮内全部已扫描槽位失败才记 1 次连败
///   （7 个槽位全败只记 1 轮）；本轮有任一槽位注册成功 = 句柄活着，清零；
///   本轮零扫描（槽位已全部注册）不裁决、计数冻结；
/// - **纯超时（无 I/O 错误）不得触发重开且连败清零**：设备深睡不应答
///   不是句柄死亡——避免把「设备深睡」误判成句柄死亡（v0.3.10 P1 质检
///   打回的同族陷阱）；连败要求每轮都至少一次 I/O 类错误（死句柄的
///   read 每轮都报错，深睡设备的通道每轮都是纯超时，两者可分）；
/// - 连续 [`SLOT_SCAN_REOPEN_THRESHOLD`] 轮连败返回 true，由 IO 线程强制
///   重开该通道句柄（`HidDeviceChannel::open` 是主机侧 HID API 操作，
///   零射频——不对判定休眠的设备发起任何主动查询）。
///
/// 每个通道一个实例（IO 线程按 ChannelState 持有），Instant 无关、纯计数。
#[derive(Debug, Clone, Default)]
pub struct SlotScanStreak {
    /// 连续「扫槽全败且含 I/O 类错误」的轮数。
    streak: u32,
}

impl SlotScanStreak {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前连败轮数（日志/测试观测用）。
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// 登记一轮扫槽结果。`scanned` = 本轮实际发起扫描的槽位数；
    /// `failures` = 其中**失败**槽位的失败类别（成功槽位不在其中）。
    /// 返回 true = 连败达阈值，应对该通道强制重开句柄。
    pub fn note_round(&mut self, scanned: usize, failures: &[SlotScanFailure]) -> bool {
        if scanned == 0 {
            // 本轮零扫描（槽位已全部注册）：不裁决、计数冻结。
            return false;
        }
        if failures.len() < scanned {
            // 有槽位注册成功 = 句柄活着，连败清零。
            self.streak = 0;
            return false;
        }
        if !failures.iter().any(|f| matches!(f, SlotScanFailure::Io)) {
            // 全部失败但纯超时/协议类：设备深睡不应答，不是句柄死亡——
            // 连败清零（深睡不误判死）。
            self.streak = 0;
            return false;
        }
        self.streak = self.streak.saturating_add(1);
        self.streak >= SLOT_SCAN_REOPEN_THRESHOLD
    }

    /// 重开结果处置（F2）：成功 → 连败清零、重新观察，返回 false；
    /// 失败 → 句柄重开无望，返回 true = 整通道作废（调用方移除通道
    /// 并 `Reconnect::invalidate_handles` 走全量重枚举）。
    pub fn note_reopen_result(&mut self, ok: bool) -> bool {
        if ok {
            self.streak = 0;
            false
        } else {
            true
        }
    }
}

/// 死槽冷却轮数（v0.3.17，R-2）：槽位连续 K 轮「握手无设备应答」
/// 后进入冷却，之后每 K 轮放行 1 次扫描。
pub const DEAD_SLOT_COOLDOWN_ROUNDS: u32 = 10;

/// 槽位级「死槽扫描降频」裁决（v0.3.17，R-2；E10 口径修正）——复用
/// [`SlotScanStreak`] 的「纯计数、Instant 无关、按通道持有」模式，解决
/// 空槽位/深睡槽位每轮枚举都被全量重扫的功耗与日志噪声问题：
///
/// - **只累计「握手无设备应答」**：设备深睡 / 槽位空闲的特征——纯超时
///   不应答（Timeout）与接收器 0x08「该槽位无设备」（NoDevice，E10 新增；
///   修复前 0x08 归 Protocol 不置位，接收器空槽永不喂入、冷却形同虚设）。
///   握手成功、I/O 类或协议类失败一律清零——I/O 类证据必须保持热度留给
///   [`SlotScanStreak`] 裁决句柄死亡，不参与本冷却；喂入口径的单一事实源
///   是 `NoResponseCause::feeds_dead_slot_cooldown`（device crate）；
/// - **降频语义**：连续不应答轮数 < [`DEAD_SLOT_COOLDOWN_ROUNDS`] 时
///   每轮照扫；达到后每 K 轮放行 1 次（streak 为 K 的整数倍时扫描）。
///   稳态下新配对设备的自动发现延迟由 ≤30s 放宽到 ≤K 轮（约 5 分钟），
///   手动刷新 / 动鼠标经 force 枚举 [`Self::clear`] 立即穿透；
/// - **跳过轮必须推进连击**（BF-1）：生产接线在 `should_scan` 返回
///   false 的跳过轮同样调用 `note_handshake(slot, true)`——否则
///   streak 冻结、冷却槽永不复扫。本结构的放行节奏以该接线为前提，
///   下方测试①的序列（跳过轮推进连击）与生产一一对应；
/// - **与句柄同寿命**：实例存于 ChannelState，force 枚举句柄重开时
///   清零（新一轮全量补扫）。
///
/// 每个通道一个实例（IO 线程按 ChannelState 持有），Instant 无关、纯计数。
#[derive(Debug, Clone, Default)]
pub struct DeadSlotScan {
    /// 槽位号 → 连续「握手无设备应答」轮数（Timeout 或 NoDevice）。
    streaks: std::collections::HashMap<u8, u32>,
}

impl DeadSlotScan {
    pub fn new() -> Self {
        Self::default()
    }

    /// 本轮是否应扫描该槽位：无记录 / 未达 K 轮 → 扫描；达 K 轮后
    /// 每 K 轮放行 1 次（streak 为 K 的整数倍时扫描）。
    pub fn should_scan(&self, slot: u8) -> bool {
        match self.streaks.get(&slot) {
            None => true,
            Some(&s) => s < DEAD_SLOT_COOLDOWN_ROUNDS || s % DEAD_SLOT_COOLDOWN_ROUNDS == 0,
        }
    }

    /// 登记一轮该槽位的握手结果。`no_device_response` = 握手阶段
    /// 「无设备应答」——纯超时不应答（Timeout）或接收器应答该槽位
    /// 无设备（NoDevice，E10 细分；喂入口径见
    /// `NoResponseCause::feeds_dead_slot_cooldown`）；其余一切结果
    /// （握手成功 / I/O 类 / 协议类）都视为「槽位活着」清零重计。
    pub fn note_handshake(&mut self, slot: u8, no_device_response: bool) {
        if no_device_response {
            let s = self.streaks.entry(slot).or_insert(0);
            *s = s.saturating_add(1);
        } else {
            self.streaks.remove(&slot);
        }
    }

    /// 全部清零（force 枚举句柄重开后调用）：立即恢复全量扫描。
    pub fn clear(&mut self) {
        self.streaks.clear();
    }

    /// 当前连续不应答轮数（日志/测试观测用）。
    pub fn streak(&self, slot: u8) -> u32 {
        self.streaks.get(&slot).copied().unwrap_or(0)
    }
}

/// R-3（v0.3.17）：唤醒装填集裁决（纯函数，IO 线程 WakeSignal 处理的
/// 唯一装填口径，四例矩阵单测覆盖）：
///
/// - **归因成功**（`on_channel = Some`）：装填该通道上的
///   Sleeping ∪ (Online ∧ 灰显)——灰显在线设备电量未知，鼠标已活动，
///   装填一次探测资格让它随探测轮提前刷新；
/// - **归因失败回退 arm-all**（`on_channel = None`）：仅 Sleeping——
///   无归因证据时不对在线设备发任何额外查询（电池红线保守侧）；
///   **豁免**：Online-灰显恰为 1 台时一并装填（此时全系统只有它一台
///   灰显，arm-all 的「误装填他机」代价为零）。
///
/// 返回 (装填 id 列表, 豁免是否生效)；豁免标记供 IO 线程落日志。
pub fn wake_arm_set(
    sleeping: &[String],
    online_gray: &[String],
    on_channel: Option<&std::collections::HashSet<&str>>,
) -> (Vec<String>, bool) {
    match on_channel {
        Some(on_chan) => (
            sleeping
                .iter()
                .chain(online_gray.iter())
                .filter(|id| on_chan.contains(id.as_str()))
                .cloned()
                .collect(),
            false,
        ),
        None => {
            let mut ids: Vec<String> = sleeping.to_vec();
            let exempt = online_gray.len() == 1;
            if exempt {
                ids.extend(online_gray.iter().cloned());
            }
            (ids, exempt)
        }
    }
}

/// 重枚举/重连状态机（IO 线程持有，Instant 注入可测）。
#[derive(Debug, Clone)]
pub struct Reconnect {
    /// 连续「枚举后注册槽位总数为 0」的次数（决定退避档位）。
    empty_streak: u32,
    /// 上次枚举完成的时刻（None = 尚未枚举过，首轮立即枚举）。
    last_enum: Option<Instant>,
    /// 是否处于「无设备」态。
    no_device: bool,
    /// 强制下一轮循环立即重枚举（「立即刷新」/句柄失效自愈置位）。
    force: bool,
    /// 上次「Raw Input 唤醒信号触发强制重枚举」的时刻（v0.3.11，F5
    /// 防抖：事件风暴合并，见 [`Self::request_wake_enum`]）。
    last_wake_enum: Option<Instant>,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self::new()
    }
}

impl Reconnect {
    pub fn new() -> Self {
        Reconnect {
            empty_streak: 0,
            last_enum: None,
            no_device: false,
            force: false,
            last_wake_enum: None,
        }
    }

    /// 「立即刷新」：下一轮循环必须先重新枚举再查询（怀疑方向 3 修复）。
    pub fn request_refresh(&mut self) {
        self.force = true;
    }

    /// Raw Input 唤醒信号裁决（v0.3.11，F5）：唤醒事件到达且当前注册槽位
    /// 为 0 → 置 force 强制下一轮立即重枚举+扫槽（主机侧发起）。此前的
    /// 唤醒路径只 arm `sleeping_ids`（已注册设备的探测资格），未注册设备
    /// 收不到任何信号——重启后鼠标休眠、槽位空时，用户一动鼠标要等下一
    /// 次退避重扫（最长 120s）才可能恢复。此时鼠标已被用户物理唤醒、处于
    /// 活跃态，主机侧补扫不违反零鼠标电池红线（红线禁止「主动唤醒休眠
    /// 设备」，不禁止「设备已醒时补读」）。防抖：距上次**唤醒触发**枚举
    /// 不足 [`WAKE_ENUM_MIN_INTERVAL`] 的事件合并（不置位），事件风暴
    /// 不得形成枚举风暴。
    /// 返回 true = 本次事件触发了强制重枚举（供 IO 线程落日志）。
    pub fn request_wake_enum(&mut self, now: Instant, registered_slots: usize) -> bool {
        if registered_slots > 0 {
            // 已有注册槽位：走原有 sleeping_ids 唤醒探测路径，不触发枚举。
            return false;
        }
        if let Some(t) = self.last_wake_enum {
            if now.saturating_duration_since(t) < WAKE_ENUM_MIN_INTERVAL {
                return false;
            }
        }
        self.last_wake_enum = Some(now);
        self.force = true;
        true
    }

    /// 缓存句柄失效（连续 I/O 硬失败达阈值）：作废句柄并在下一轮
    /// 立即重新枚举（重开句柄、重扫槽位；主机侧操作，零射频成本）。
    pub fn invalidate_handles(&mut self) {
        self.force = true;
    }

    /// 是否有待消费的强制重枚举（IO 线程据此跳过本轮睡眠）。
    pub fn pending_force(&self) -> bool {
        self.force
    }

    pub fn no_device(&self) -> bool {
        self.no_device
    }

    /// 当前退避档位（无设备且无任何候选通道时的重试间隔）。
    pub fn current_backoff(&self) -> Duration {
        let idx = self
            .empty_streak
            .max(1)
            .min(ENUM_BACKOFF_STEPS.len() as u32)
            - 1;
        ENUM_BACKOFF_STEPS[idx as usize]
    }

    /// 本轮循环是否执行重枚举。
    ///
    /// - 有待消费的强制请求 → 枚举；
    /// - 从未枚举过 → 枚举；
    /// - 无设备且连候选通道都没有 → 按 30s/60s/120s 退避（仅主机侧枚举）；
    /// - 其余（正常态 / 有通道但无注册槽位）→ 沿用 30s 门控。
    pub fn should_enumerate(&self, now: Instant, has_channels: bool) -> bool {
        if self.force {
            return true;
        }
        let Some(t) = self.last_enum else {
            return true;
        };
        let elapsed = now.saturating_duration_since(t);
        if self.no_device && !has_channels {
            elapsed >= self.current_backoff()
        } else {
            elapsed >= REENUM_INTERVAL
        }
    }

    /// 登记一次枚举完成。`registered_slots` = 枚举+槽位扫描后全部通道的
    /// 注册槽位总数（用户可见的「有无设备」口径）。返回态迁移供打日志。
    pub fn record_result(
        &mut self,
        now: Instant,
        registered_slots: usize,
    ) -> Option<EnumTransition> {
        self.last_enum = Some(now);
        self.force = false;
        if registered_slots == 0 {
            self.empty_streak = self.empty_streak.saturating_add(1);
            if !self.no_device {
                self.no_device = true;
                return Some(EnumTransition::EnteredNoDevice);
            }
            None
        } else {
            self.empty_streak = 0;
            if self.no_device {
                self.no_device = false;
                return Some(EnumTransition::ExitedNoDevice);
            }
            None
        }
    }

    /// 本轮轮询后的睡眠时长（分片睡眠的总预算）：
    /// - 有待消费的强制重枚举 → 0（立即回到循环头执行枚举）；
    /// - 无设备态 → 距下次退避枚举的剩余时间（无候选通道时 30/60/120s，
    ///   有通道但无槽位时沿用 30s；不产生任何设备查询）；
    /// - 正常态 → 轮询间隔（与 v0.3.8 行为一致，重枚举在醒后随 30s 门控发生）。
    pub fn next_wake(&self, now: Instant, has_channels: bool, poll_interval: Duration) -> Duration {
        if self.force {
            return Duration::ZERO;
        }
        if !self.no_device {
            return poll_interval;
        }
        let due = if self.no_device && !has_channels {
            self.current_backoff()
        } else {
            REENUM_INTERVAL
        };
        match self.last_enum {
            Some(t) => due.saturating_sub(now.saturating_duration_since(t)),
            None => Duration::ZERO,
        }
    }
}

/// v0.3.15（P1-1）：唤醒信号到来时 IO 线程的动作裁决。
///
/// 唤醒信号的唯一职责是给休眠设备装填唤醒探测资格（arm），**不是**
/// 「该整轮重查在线设备了」——v0.3.14 及以前 WakeSignal 无条件
/// `continue 'outer`，用户持续动鼠标时在线设备被以 ~0.5Hz 查电量
/// （1800s 轮询档位的 ~900 倍）。本裁决把「装填」与「起轮」解耦。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAction {
    /// 无休眠设备可装填、枚举也未到期：什么都不做（在线设备零额外查询）。
    StayAsleep,
    /// 起一轮「唤醒探测轮」：只探测被装填资格的休眠设备（+ 枚举新注册
    /// 设备的首读），不重查本轮前已注册的在线设备。`enum_due` 为真时
    /// 该轮顺带执行主机侧重枚举（reconnect.rs:30 的 30s 门控不变）。
    ProbeRound,
}

/// 唤醒信号动作裁决（纯函数，IO 线程在装填资格后调用）：
/// - `armed_sleeping`：本信号是否给至少一台休眠设备装填了探测资格；
/// - `enum_due`：此刻 `should_enumerate` 是否到期（含 force / 30s 门控）。
pub fn wake_action(armed_sleeping: bool, enum_due: bool) -> WakeAction {
    if armed_sleeping || enum_due {
        WakeAction::ProbeRound
    } else {
        WakeAction::StayAsleep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.3.15（P1-1）：arm 与整轮轮询解耦——没有休眠设备可装填且枚举
    /// 未到期时，唤醒信号不得起轮（在线设备零额外查询）；有装填或枚举
    /// 到期时才起探测轮。
    #[test]
    fn wake_signal_decoupled_from_full_round() {
        assert_eq!(wake_action(false, false), WakeAction::StayAsleep);
        assert_eq!(wake_action(true, false), WakeAction::ProbeRound);
        assert_eq!(wake_action(false, true), WakeAction::ProbeRound);
        assert_eq!(wake_action(true, true), WakeAction::ProbeRound);
    }

    /// 枚举失败 → 进入无设备态 → 退避 30s/60s/120s 封顶 → 枚举恢复 →
    /// 退出无设备态并恢复正常轮询节奏（任务 B 用例 1）。
    #[test]
    fn enum_fail_no_device_backoff_then_recovery() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        // 首轮（开机自启）立即枚举。
        assert!(r.should_enumerate(t0, false));
        // 枚举到 0 设备（接收器/驱动未就绪）→ 进入无设备态。
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert!(r.no_device());
        assert_eq!(r.current_backoff(), Duration::from_secs(30));
        // 立即重试被抑制：30s 内不再枚举（不提高频率）。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(29), false));
        assert!(r.should_enumerate(t0 + Duration::from_secs(30), false));
        // 第二次落空 → 退避升 60s。
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(r.record_result(t1, 0), None); // 已在无设备态，无重复迁移事件
        assert_eq!(r.current_backoff(), Duration::from_secs(60));
        assert!(!r.should_enumerate(t1 + Duration::from_secs(59), false));
        assert!(r.should_enumerate(t1 + Duration::from_secs(60), false));
        // 第三次落空 → 120s 封顶。
        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(r.record_result(t2, 0), None);
        assert_eq!(r.current_backoff(), Duration::from_secs(120));
        // 继续落空保持 120s 封顶（不无限增长）。
        let t3 = t2 + Duration::from_secs(120);
        assert_eq!(r.record_result(t3, 0), None);
        assert_eq!(r.current_backoff(), Duration::from_secs(120));
        // 枚举恢复（驱动就绪，注册到槽位）→ 退出无设备态。
        let t4 = t3 + Duration::from_secs(120);
        assert_eq!(r.record_result(t4, 2), Some(EnumTransition::ExitedNoDevice));
        assert!(!r.no_device());
        assert_eq!(r.current_backoff(), Duration::from_secs(30)); // 连击清零
                                                                  // 恢复正常轮询：next_wake 回到轮询间隔，重枚举恢复 30s 门控。
        let poll = Duration::from_secs(600);
        assert_eq!(r.next_wake(t4, true, poll), poll);
        assert!(!r.should_enumerate(t4 + Duration::from_secs(29), true));
        assert!(r.should_enumerate(t4 + Duration::from_secs(30), true));
    }

    /// 「立即刷新」必须先重新枚举再查询（任务 B 用例 2 / 怀疑方向 3）。
    #[test]
    fn refresh_forces_reenumeration() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(r.record_result(t0, 1), None); // 正常态：刚枚举完、有设备
                                                  // 30s 门控内本不该重枚举。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(5), true));
        // 用户点「立即刷新」→ 下一轮立即重枚举、睡眠预算归零。
        r.request_refresh();
        assert!(r.pending_force());
        assert!(r.should_enumerate(t0 + Duration::from_secs(5), true));
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(5), true, Duration::from_secs(600)),
            Duration::ZERO
        );
        // 枚举完成后强制位被消费，恢复正常门控。
        assert_eq!(r.record_result(t0 + Duration::from_secs(5), 1), None);
        assert!(!r.pending_force());
        assert!(!r.should_enumerate(t0 + Duration::from_secs(10), true));
    }

    /// 无设备态下「立即刷新」同样立即重枚举（不受退避抑制——用户显式
    /// 操作，枚举本身是主机侧零射频操作）。
    #[test]
    fn refresh_overrides_no_device_backoff() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert!(!r.should_enumerate(t0 + Duration::from_secs(1), false));
        r.request_refresh();
        assert!(r.should_enumerate(t0 + Duration::from_secs(1), false));
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(1), false, Duration::from_secs(600)),
            Duration::ZERO
        );
    }

    /// 句柄失效自愈（任务 B 用例 3 / 怀疑方向 2）：连续 I/O 硬失败达阈值 →
    /// 作废缓存句柄、强制重枚举一次 → 重开句柄重扫槽位 → 恢复正常。
    #[test]
    fn handle_invalidation_self_heals() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(r.record_result(t0, 2), None); // 两台设备在线
                                                  // 驱动重载导致缓存句柄死亡：IO 线程在硬失败达阈值后调用
                                                  // invalidate_handles（并丢弃旧通道），下一轮立即重枚举。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(3), true));
        r.invalidate_handles();
        assert!(r.should_enumerate(t0 + Duration::from_secs(3), true));
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(3), false, Duration::from_secs(600)),
            Duration::ZERO,
            "旧通道已丢弃、句柄待重开：不得睡眠等待"
        );
        // 重枚举重开句柄、重扫槽位成功 → 正常态，期间无多余的态迁移事件。
        assert_eq!(r.record_result(t0 + Duration::from_secs(4), 2), None);
        assert!(!r.no_device());
        assert!(!r.pending_force());
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(4), true, Duration::from_secs(600)),
            Duration::from_secs(600)
        );
    }

    /// 硬失败达阈值常量：与 IO 线程的 3 连败注销口径一致。
    #[test]
    fn invalidate_threshold_is_three() {
        assert_eq!(HANDLE_INVALIDATE_THRESHOLD, 3);
    }

    /// 接收器通道在、但槽位扫描全落空（驱动未就绪的中间态）：
    /// 无设备态下沿用 30s 重扫节奏（既有行为），不走 60/120 退避。
    #[test]
    fn no_device_with_channels_keeps_30s_rescan() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert_eq!(r.record_result(t0 + Duration::from_secs(30), 0), None);
        // 有通道：30s 门控，不升退避档。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(59), true));
        assert!(r.should_enumerate(t0 + Duration::from_secs(60), true));
        // next_wake 给 30s 剩余，而非 60s 退避。
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(30), true, Duration::from_secs(600)),
            Duration::from_secs(30)
        );
    }

    /// 无设备态的睡眠预算 = 距下次退避枚举的剩余时间（主机侧等待，
    /// 期间零查询）；超过档位立即醒来枚举。
    #[test]
    fn next_wake_tracks_backoff_remaining() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert_eq!(
            r.next_wake(
                t0 + Duration::from_secs(10),
                false,
                Duration::from_secs(600)
            ),
            Duration::from_secs(20)
        );
        assert_eq!(
            r.next_wake(
                t0 + Duration::from_secs(30),
                false,
                Duration::from_secs(600)
            ),
            Duration::ZERO
        );
    }

    use super::{ChannelLiveness, SlotOutcome as SO, WAKE_PROBE_INVALIDATE_THRESHOLD};

    /// v0.3.11（F5）：Raw Input 唤醒信号 → 0 注册槽位时立即强制重枚举。
    /// 重启后鼠标休眠、槽位空，用户一动鼠标（Raw Input 观测到罗技鼠标
    /// 活动 = 设备已被物理唤醒）即触发，不等 30s 门控/退避重扫。
    #[test]
    fn wake_signal_with_zero_slots_forces_reenum() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        // 枚举到 0 槽位（接收器在、鼠标休眠未注册）→ 无设备态。
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        // 30s 门控内本不该重枚举（与唤醒时刻无关——防抖只看上一次
        // 唤醒触发的枚举，不推迟首个唤醒响应）。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(2), true));
        // 用户动鼠标 → 强制重枚举立即生效。
        assert!(r.request_wake_enum(t0 + Duration::from_secs(2), 0));
        assert!(r.pending_force());
        assert!(r.should_enumerate(t0 + Duration::from_secs(2), true));
        // 枚举+扫槽注册成功 → 退出无设备态，强制位被消费。
        assert_eq!(
            r.record_result(t0 + Duration::from_secs(3), 1),
            Some(EnumTransition::ExitedNoDevice)
        );
        assert!(!r.pending_force());
    }

    /// v0.3.11（F5）：已有注册槽位 → 唤醒事件不触发强制重枚举（走原
    /// sleeping_ids 唤醒探测路径，仅装填已注册设备的探测资格）。
    #[test]
    fn wake_signal_with_registered_slots_does_not_force() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(r.record_result(t0, 2), None);
        // 设备休眠后用户动鼠标：唤醒信号到达，但槽位非 0 → 不置 force。
        assert!(!r.request_wake_enum(t0 + Duration::from_secs(10), 2));
        assert!(!r.pending_force());
        assert!(!r.should_enumerate(t0 + Duration::from_secs(10), true));
    }

    /// v0.3.11（F5）：唤醒事件风暴（鼠标持续移动，GUI 侧每 2s 合并一次）
    /// 不得形成枚举风暴——距上次唤醒触发不足 WAKE_ENUM_MIN_INTERVAL 的
    /// 事件合并，不重复置位。
    #[test]
    fn wake_signal_storm_is_coalesced() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        // 第一个事件（距上次枚举仅 1s，仍立即响应）：触发强制枚举。
        assert!(r.request_wake_enum(t0 + Duration::from_secs(1), 0));
        // 枚举消费 force，但注册仍落空（设备唤醒慢/首轮握手超时）。
        assert_eq!(r.record_result(t0 + Duration::from_secs(2), 0), None);
        // 风暴：t0+3 / t0+4 / t0+5 连发三次，均距上次唤醒触发 < 5s → 合并。
        assert!(!r.request_wake_enum(t0 + Duration::from_secs(3), 0));
        assert!(!r.request_wake_enum(t0 + Duration::from_secs(4), 0));
        assert!(!r.request_wake_enum(t0 + Duration::from_secs(5), 0));
        assert!(!r.pending_force(), "风暴期间的合并事件不得重复置位");
        // t0+6（距上次唤醒触发 t0+1 已 5s）→ 下一次触发放行。
        assert!(r.request_wake_enum(t0 + Duration::from_secs(6), 0));
        assert!(r.pending_force());
    }

    /// D-67 回归断言 1（P1）：多设备通道部分/全部槽位正常休眠不判死——
    /// 2 槽位通道两轮各全部超时（恰好是 D-33 的合法休眠边界，
    /// SLEEP_AFTER_TIMEOUTS=2）→ 不得 invalidate；第 3 轮起两槽位都进入
    /// Sleeping、本轮零查询（全 Skip）→ 计数冻结，永不 invalidate。
    /// v0.3.9 按次累计时第 2 轮第 1 个超时就判死（streak 3 >= 3）。
    #[test]
    fn liveness_multi_device_normal_sleep_not_judged_dead() {
        let mut l = ChannelLiveness::new();
        // 第 1 轮：A、B 各超时 1 次 → 整轮零应答，连击 1，不判死。
        assert!(!l.note_round(&[SO::Timeout, SO::Timeout], &[]));
        assert_eq!(l.silent_rounds(), 1);
        // 第 2 轮：A、B 第 2 次超时（合法休眠边界）→ 连击 2，仍不判死。
        assert!(!l.note_round(&[SO::Timeout, SO::Timeout], &[]));
        assert_eq!(l.silent_rounds(), 2);
        // 第 3 轮起：两台都进 Sleeping，全 Skip，本轮零查询 → 不裁决、冻结。
        for _ in 0..10 {
            assert!(!l.note_round(&[], &[]));
        }
        assert_eq!(l.silent_rounds(), 2, "无可查询槽位的轮次不得改变连击");
        // 3 槽位通道同理：一轮 3 次超时只记 1 轮连击，两轮不超阈值。
        let mut l3 = ChannelLiveness::new();
        assert!(!l3.note_round(&[SO::Timeout, SO::Timeout, SO::Timeout], &[]));
        assert!(!l3.note_round(&[SO::Timeout, SO::Timeout, SO::Timeout], &[]));
        assert_eq!(l3.silent_rounds(), 2);
    }

    /// D-67 回归断言 2（P1）：连续 3 轮整轮零应答才判死；任一轮有 1 次
    /// 成功/字节应答即清零。
    #[test]
    fn liveness_only_full_silent_rounds_count_and_any_answer_resets() {
        let mut l = ChannelLiveness::new();
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Timeout], &[]));
        // 第 3 轮整轮静默 → 判死（句柄失效自愈路径）。
        assert!(l.note_round(&[SO::Timeout], &[]));
        // 任一轮有 1 次成功 → 连击清零。
        let mut l = ChannelLiveness::new();
        assert!(!l.note_round(&[SO::Timeout, SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Timeout, SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Success, SO::Timeout], &[]));
        assert_eq!(l.silent_rounds(), 0, "一次成功即清零");
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Timeout], &[]));
        // 有字节应答但给不出电量（错误帧等）同样证明句柄活着 → 清零。
        let mut l = ChannelLiveness::new();
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Responded, SO::Timeout], &[]));
        assert_eq!(l.silent_rounds(), 0, "字节应答（错误帧）即清零");
    }

    /// D-67（P2 配套）：句柄真死（接收器重插/驱动重载）时每个槽位每轮
    /// 都 I/O 报错——IoError 与 Timeout 同属静默证据，3 轮判死并自愈。
    #[test]
    fn liveness_dead_handle_io_errors_invalidate_after_three_rounds() {
        let mut l = ChannelLiveness::new();
        assert!(!l.note_round(&[SO::IoError, SO::IoError], &[]));
        assert!(!l.note_round(&[SO::IoError, SO::IoError], &[]));
        assert!(l.note_round(&[SO::IoError, SO::IoError], &[]));
        // 单设备通道句柄死亡（P2 场景）：同样 3 轮判死 → 重枚举重开句柄。
        let mut l = ChannelLiveness::new();
        assert!(!l.note_round(&[SO::IoError], &[]));
        assert!(!l.note_round(&[SO::IoError], &[]));
        assert!(l.note_round(&[SO::IoError], &[]));
    }

    /// D-67 回归（P3-唤醒探测）：设备已知休眠后的唤醒探测单独计数——
    /// 深睡唤醒慢导致 1 次探测超时不判死（v0.3.9 会整通道重建、假离线），
    /// 连续 2 轮探测全静默才判死；普通连击与探测连击互不混算。
    #[test]
    fn liveness_wake_probe_silence_needs_two_rounds() {
        let mut l = ChannelLiveness::new();
        // 设备经 2 次超时进入休眠（普通连击 2，未达阈值）。
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Timeout], &[]));
        // 休眠期全 Skip 的轮次冻结计数。
        assert!(!l.note_round(&[], &[]));
        // 第 1 次唤醒探测超时（深睡唤醒慢）：不判死——这是与 v0.3.9 的
        // 行为差异点（旧实现此处 streak=3 直接整通道作废）。
        assert!(!l.note_round(&[], &[SO::Timeout]));
        assert_eq!(l.probe_silent_rounds(), 1);
        assert_eq!(l.silent_rounds(), 2, "探测超时不计入普通连击");
        // 连续第 2 轮探测仍静默 → 判死（句柄确死时仍能自愈）。
        assert!(l.note_round(&[], &[SO::Timeout]));
        assert_eq!(WAKE_PROBE_INVALIDATE_THRESHOLD, 2);
        // 探测成功（设备唤醒）→ 两类连击都清零。
        let mut l = ChannelLiveness::new();
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[SO::Timeout], &[]));
        assert!(!l.note_round(&[], &[SO::Timeout]));
        assert!(!l.note_round(&[], &[SO::Success]));
        assert_eq!(l.silent_rounds(), 0);
        assert_eq!(l.probe_silent_rounds(), 0);
    }

    /// D-67 常量锁定：判死阈值口径。
    #[test]
    fn liveness_thresholds_locked() {
        assert_eq!(HANDLE_INVALIDATE_THRESHOLD, 3);
        assert_eq!(WAKE_PROBE_INVALIDATE_THRESHOLD, 2);
    }

    use super::{SlotScanFailure as SF, SlotScanStreak, SLOT_SCAN_REOPEN_THRESHOLD};

    /// F2 用例 1（连败计数触发重开 + 跨槽位去重按轮裁决）：连续 3 轮
    /// 「扫槽全败且含 I/O 类错误」→ 触发强制重开；一轮 7 个槽位全败
    /// 只记 1 轮连败（按轮裁决，不跨槽位累计）。
    #[test]
    fn scan_streak_three_io_rounds_trigger_reopen() {
        let mut s = SlotScanStreak::new();
        let all_io = [SF::Io; 7];
        assert!(!s.note_round(7, &all_io));
        assert_eq!(s.streak(), 1, "一轮 7 槽位全败只记 1 轮（跨槽位去重）");
        assert!(!s.note_round(7, &all_io));
        assert_eq!(s.streak(), 2);
        assert!(s.note_round(7, &all_io), "第 3 轮连败触发强制重开");
        assert_eq!(SLOT_SCAN_REOPEN_THRESHOLD, 3);
    }

    /// F2 用例 2（纯超时不触发）：全部超时（无 I/O 错误）永不触发重开，
    /// 且打断既有连败——设备深睡不应答不是句柄死亡（v0.3.10 P1 质检
    /// 打回的同族陷阱）。
    #[test]
    fn scan_streak_pure_timeout_never_triggers() {
        let mut s = SlotScanStreak::new();
        let all_timeout = [SF::Timeout; 7];
        for _ in 0..10 {
            assert!(!s.note_round(7, &all_timeout));
        }
        assert_eq!(s.streak(), 0, "纯超时轮不得累计连败");
        // 2 轮 I/O 连败后插入纯超时轮 → 连败被打断清零，重新计。
        let mut s = SlotScanStreak::new();
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_round(7, &all_timeout));
        assert_eq!(s.streak(), 0, "纯超时轮打断连败（深睡不误判死）");
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert_eq!(s.streak(), 1);
    }

    /// F2 用例 3（重开失败整通道作废）：连败触发后重开失败 → 作废
    /// （调用方移除通道 + invalidate_handles 全量重枚举）；重开成功 →
    /// 连败清零、不作废。
    #[test]
    fn scan_streak_reopen_failure_scraps_channel() {
        let mut s = SlotScanStreak::new();
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(s.note_round(7, &[SF::Io; 7]));
        assert!(s.note_reopen_result(false), "重开失败 = 整通道作废");
        // 重开成功：连败清零、不作废。
        let mut s = SlotScanStreak::new();
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_reopen_result(true));
        assert_eq!(s.streak(), 0, "重开成功后连败清零重新观察");
    }

    /// F2 用例 4（成功清零 / 零扫描冻结 / 混合失败含 I/O 计数）：
    /// 任一槽位注册成功 = 句柄活着；本轮零扫描（槽位已满）不裁决；
    /// 失败混合中只要含 I/O 类即满足连败条件。
    #[test]
    fn scan_streak_success_resets_and_idle_rounds_freeze() {
        let mut s = SlotScanStreak::new();
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_round(7, &[SF::Io; 7]));
        // 7 扫描 6 失败 1 成功 → 句柄活着，清零。
        assert!(!s.note_round(7, &[SF::Io; 6]));
        assert_eq!(s.streak(), 0, "任一槽位成功即清零");
        // 连败 1 轮后零扫描轮（槽位全部注册）：计数冻结。
        assert!(!s.note_round(7, &[SF::Io; 7]));
        assert!(!s.note_round(0, &[]));
        assert_eq!(s.streak(), 1, "本轮零扫描不得改变连败");
        // 混合失败含 I/O → 计数；第 3 轮触发。
        assert!(!s.note_round(2, &[SF::Io, SF::Timeout]));
        assert!(s.note_round(2, &[SF::Timeout, SF::Io]));
    }

    use super::{DeadSlotScan, DEAD_SLOT_COOLDOWN_ROUNDS};

    /// R-2 用例 1（冷却生效）：连续 K 轮握手无设备应答后进入冷却，
    /// 之后每 K 轮才放行 1 次扫描。
    /// 本用例序列与生产接线一一对应（BF-1）：tray 扫槽循环在
    /// `should_scan` 为 false 的跳过轮同样调用 `note_handshake(true)`
    /// 推进连击——若接线不推进，本测试所锁定的放行节奏在生产中不成立。
    #[test]
    fn dead_slot_enters_cooldown_after_k_timeouts() {
        let mut d = DeadSlotScan::new();
        for _ in 0..DEAD_SLOT_COOLDOWN_ROUNDS {
            assert!(d.should_scan(3), "冷却前每轮都扫");
            d.note_handshake(3, true);
        }
        // streak 首次到达 K：K 的整数倍，放行最后一次。
        assert!(d.should_scan(3));
        d.note_handshake(3, true);
        // 之后 K-1 轮全部跳过。
        for _ in 1..DEAD_SLOT_COOLDOWN_ROUNDS {
            assert!(!d.should_scan(3), "冷却期内跳过");
            d.note_handshake(3, true);
        }
        // 再次到达 K 的整数倍：放行 1 次。
        assert!(d.should_scan(3), "每 K 轮放行 1 次");
    }

    /// R-2 用例 2（应答/非超时清零）：握手成功或 I/O 类失败都视为
    /// 「槽位活着」——冷却计数清零（I/O 证据由 SlotScanStreak 裁决，
    /// 不参与死槽冷却）。
    #[test]
    fn dead_slot_response_or_io_resets_cooldown() {
        let mut d = DeadSlotScan::new();
        for _ in 0..DEAD_SLOT_COOLDOWN_ROUNDS + 5 {
            d.note_handshake(5, true);
        }
        assert!(!d.should_scan(5));
        // 一次非「纯超时不应答」结果 → 清零，下一轮恢复全量扫描。
        d.note_handshake(5, false);
        assert_eq!(d.streak(5), 0);
        assert!(d.should_scan(5));
    }

    /// R-2 用例 3（force 清零穿透）：强制枚举句柄重开后 clear——
    /// 全部槽位立即恢复扫描（手动刷新/动鼠标立即穿透冷却）。
    #[test]
    fn dead_slot_clear_penetrates_cooldown() {
        let mut d = DeadSlotScan::new();
        for _ in 0..DEAD_SLOT_COOLDOWN_ROUNDS + 5 {
            d.note_handshake(6, true);
        }
        assert!(!d.should_scan(6));
        d.clear();
        assert!(d.should_scan(6), "force 后立即穿透冷却");
        assert_eq!(d.streak(6), 0);
    }

    use super::wake_arm_set;

    /// R-3 矩阵 ①（归因命中，过滤含灰显）：只装填命中通道上的
    /// Sleeping ∪ (Online ∧ 灰显)，他通道设备不装填；无豁免标记。
    #[test]
    fn wake_arm_set_attributed_filters_by_channel() {
        let sleeping = vec!["a".to_string(), "b".to_string()];
        let gray = vec!["g1".to_string(), "g2".to_string()];
        let on_chan: std::collections::HashSet<&str> = ["a", "g1"].into_iter().collect();
        let (ids, exempt) = wake_arm_set(&sleeping, &gray, Some(&on_chan));
        assert_eq!(ids, vec!["a".to_string(), "g1".to_string()]);
        assert!(!exempt);
    }

    /// R-3 矩阵 ②（归因命中但通道上无可装填设备）：空集、不触发
    /// 探测轮（armed_any=false），无豁免标记。
    #[test]
    fn wake_arm_set_attributed_empty_channel() {
        let sleeping = vec!["a".to_string()];
        let gray = vec!["g1".to_string()];
        let on_chan: std::collections::HashSet<&str> = ["x"].into_iter().collect();
        let (ids, exempt) = wake_arm_set(&sleeping, &gray, Some(&on_chan));
        assert!(ids.is_empty());
        assert!(!exempt);
    }

    /// R-3 矩阵 ③（arm-all，多台灰显不豁免）：仅装填 Sleeping——
    /// 无归因证据时不对在线设备发额外查询（电池红线保守侧）。
    #[test]
    fn wake_arm_set_arm_all_multi_gray_no_exemption() {
        let sleeping = vec!["a".to_string(), "b".to_string()];
        let gray = vec!["g1".to_string(), "g2".to_string()];
        let (ids, exempt) = wake_arm_set(&sleeping, &gray, None);
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        assert!(!exempt);
    }

    /// R-3 矩阵 ④（arm-all，单灰显豁免）：Online-灰显恰为 1 台时
    /// 一并装填并标记豁免（误装填他机代价为零）。
    #[test]
    fn wake_arm_set_arm_all_single_gray_exempt() {
        let sleeping = vec!["a".to_string()];
        let gray = vec!["g1".to_string()];
        let (ids, exempt) = wake_arm_set(&sleeping, &gray, None);
        assert_eq!(ids, vec!["a".to_string(), "g1".to_string()]);
        assert!(exempt);
    }
}
