//! 诊断视图共享层(log 查看 / events 查看):在 [`zenoh_base`](crate::zenoh_base) 与
//! [`zenoh_arm`](crate::zenoh_arm) 之间复用。
//!
//! 契约来自 hex-controller(REVIEW §0.7 / P1-3 / P1-7):
//! - **事件**(可靠层,proto 编码):控制器逐条 Put 到 `<prefix>/events`;`<prefix>/events/recent`
//!   queryable 一次性回最近 ≤100 条(`EventLog`)。GUI 对 ERROR/FATAL 弹通知。
//! - **日志**(尽力层,纯文本):每进程 Put 到 `hexmeow/<cid>/<proc>/log`,行格式
//!   `<ts_ns> <LEVEL> <target> <msg>`;`.../log/recent` queryable 回多行拼接的文本 blob(≤500 行)。
//!
//! 本模块只放**纯逻辑 + DTO**(键解析 / 行解析 / 环形缓冲),Zenoh I/O 留在各 app。

use std::collections::VecDeque;

use serde::Serialize;

/// 日志环形缓冲容量(控制器每进程留 ~500 行;这里放宽以容纳多进程 + 实时追加)。
pub const LOG_RING_CAP: usize = 2000;
/// 事件环形缓冲容量(控制器留 ~100 条;放宽一档)。
pub const EVENT_RING_CAP: usize = 300;

/// 一行日志(已解析)。`proc` 取自键(如 `arm0`/`base0`/`launcher`);解析失败时 `level`/`target`
/// 为空、`msg` 保留原始整行,永不丢字节。
#[derive(Serialize, Clone)]
pub struct LogLine {
    pub proc: String,
    pub ts_ns: i64,
    pub level: String,
    pub target: String,
    pub msg: String,
}

/// 一条机器事件(镜像 proto `Event`,`severity` 用原始 i32:1=INFO 2=WARNING 3=ERROR 4=FATAL)。
/// `seq` 由 app 侧单调递增分配(用于前端去重/通知水位),`ts_ns` 取自 `Header.stamp_ns`(仅诊断)。
#[derive(Serialize, Clone)]
pub struct RobotEvent {
    pub seq: u64,
    pub severity: i32,
    pub code: String,
    pub text: String,
    pub kv: Vec<(String, String)>,
    pub ts_ns: i64,
}

/// 一次事件快照:环形缓冲全量 + `baseline_seq`(= 下一个待分配 seq)。前端对 `seq >= baseline_seq`
/// 的 ERROR/FATAL 才弹通知——历史/重新拉取的旧事件 seq 恒 `< baseline_seq`,不会误报。
#[derive(Serialize, Clone, Default)]
pub struct EventsSnapshot {
    pub events: Vec<RobotEvent>,
    pub baseline_seq: u64,
}

/// 事件缓冲:环形缓冲 + 单调 `next_seq` + 通知 `baseline`,**同一把锁**下原子操作。
///
/// 把三者合并进一把锁(而非 ring 一把锁 + 两个独立 Atomic)消除竞态:实时 push 与 reseed 互斥,
/// 一个并发实时事件要么整体在 reseed **前**(seq < baseline,归历史)要么整体在 reseed **后**
/// (seq >= baseline,弹通知),不存在"seq 已分配但 baseline 尚未跟上"的中间态。
#[derive(Default)]
pub struct EventBuf {
    ring: VecDeque<RobotEvent>,
    next_seq: u64,
    baseline: u64,
}

impl EventBuf {
    /// 追加一条实时事件(seq 由本缓冲分配,忽略入参的 seq 占位)。
    pub fn push_live(&mut self, mut ev: RobotEvent) {
        ev.seq = self.next_seq;
        self.next_seq += 1;
        push_capped(&mut self.ring, ev, EVENT_RING_CAP);
    }

    /// 用刚拉取的历史重建环形缓冲并重置 baseline —— 与 [`push_live`](Self::push_live) 同锁,故原子:
    /// 并发实时事件不会落进"已入 ring 但 seq < 新 baseline 却本该通知"的缝隙。
    /// (窄窗代价:reseed 前极短暂窗口内到达的实时事件会被这次 clear 覆盖丢掉,但其 seq < baseline
    /// 本就不该弹通知,且控制器 events/recent 仍留有,下次刷新即补回——纯表格瞬态,不误报/不漏报通知。)
    pub fn reseed(&mut self, history: Vec<RobotEvent>) {
        self.ring.clear();
        for mut ev in history {
            ev.seq = self.next_seq;
            self.next_seq += 1;
            push_capped(&mut self.ring, ev, EVENT_RING_CAP);
        }
        self.baseline = self.next_seq;
    }

    /// 清空环形缓冲(聚焦切换时先清,再 reseed)。不动 next_seq/baseline(seq 全程单调)。
    pub fn clear(&mut self) {
        self.ring.clear();
    }

    pub fn snapshot(&self) -> EventsSnapshot {
        EventsSnapshot {
            events: self.ring.iter().cloned().collect(),
            baseline_seq: self.baseline,
        }
    }
}

/// 从机器 prefix(`hexmeow/<cid>/<robot_index>`)取控制器前缀 `hexmeow/<cid>`(用于订阅同控制器
/// 全部进程的日志)。无 `/` 时返回 None。
pub fn cid_prefix(robot_prefix: &str) -> Option<&str> {
    robot_prefix.rsplit_once('/').map(|(head, _)| head)
}

/// 从日志键取 `proc` 段:兼容 `.../<proc>/log` 与 `.../<proc>/log/recent` 两种形态。
pub fn proc_of_log_key(key: &str) -> String {
    let k = key.strip_suffix("/recent").unwrap_or(key);
    let k = k.strip_suffix("/log").unwrap_or(k);
    k.rsplit('/').next().unwrap_or("?").to_string()
}

/// 解析一行 `<ts_ns> <LEVEL> <target> <msg>`。首 token 非数字或字段不全 → 退化:整行进 `msg`,
/// `level`/`target` 留空(尽力层,不丢字节)。
pub fn parse_log_line(proc: &str, raw: &str) -> LogLine {
    let mut it = raw.splitn(4, ' ');
    let ts = it.next().unwrap_or("");
    let level = it.next().unwrap_or("");
    let target = it.next().unwrap_or("");
    let msg = it.next().unwrap_or("");
    match ts.parse::<i64>() {
        Ok(ts_ns) if !level.is_empty() => LogLine {
            proc: proc.into(),
            ts_ns,
            level: level.into(),
            target: target.into(),
            msg: msg.into(),
        },
        _ => LogLine { proc: proc.into(), ts_ns: 0, level: String::new(), target: String::new(), msg: raw.into() },
    }
}

/// 压入环形缓冲,超容量弹最旧。
pub fn push_capped<T>(ring: &mut VecDeque<T>, item: T, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back(item);
}

/// 控制器 RobotMode(proto 枚举)i32 → 稳定短名。base/arm 只读观察统一展示(设计 §3:
/// STANDBY 无 holder / RUNNING API 在控 / OVERTAKEN 被遥控器/安全接管 / FATAL_ERROR 需显式恢复)。
/// 用原始 i32 而非各 app 的 `pb::RobotMode`——两 app 各自 include! 生成的是**不同** Rust 类型,
/// 而枚举整数值同源,故以 i32 收口到一处映射,避免重复。
pub fn robot_mode_name(mode: i32) -> &'static str {
    match mode {
        1 => "STANDBY",
        2 => "RUNNING",
        3 => "OVERTAKEN",
        4 => "FATAL_ERROR",
        _ => "UNSPECIFIED",
    }
}

/// 接管原因 OvertakenMode(proto)i32 → 稳定短名(仅 OVERTAKEN 时有意义)。
pub fn overtaken_mode_name(mode: i32) -> &'static str {
    match mode {
        1 => "JOYSTICK",
        2 => "COLLISION",
        3 => "CALIBRATING",
        4 => "MOTOR_ERROR",
        5 => "BATTERY_ERROR",
        _ => "UNSPECIFIED",
    }
}

/// OVERTAKEN 时给前端的原因文本:优先 `human_readable`,否则退到 OvertakenMode 短名。
pub fn overtaken_text(mode: i32, human: Option<&str>) -> String {
    match human {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => overtaken_mode_name(mode).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_prefix_strips_last_segment() {
        assert_eq!(cid_prefix("hexmeow/ctrl1/arm0"), Some("hexmeow/ctrl1"));
        assert_eq!(cid_prefix("hexmeow/ctrl1/base0"), Some("hexmeow/ctrl1"));
        assert_eq!(cid_prefix("nofoo"), None);
    }

    #[test]
    fn proc_of_log_key_handles_both_shapes() {
        assert_eq!(proc_of_log_key("hexmeow/c/arm0/log"), "arm0");
        assert_eq!(proc_of_log_key("hexmeow/c/base0/log/recent"), "base0");
        assert_eq!(proc_of_log_key("hexmeow/c/launcher/log"), "launcher");
    }

    #[test]
    fn parse_log_line_well_formed() {
        let l = parse_log_line("arm0", "12345 WARN hex_motor::cia402 关节 1 抱 0x8130");
        assert_eq!(l.ts_ns, 12345);
        assert_eq!(l.level, "WARN");
        assert_eq!(l.target, "hex_motor::cia402");
        assert_eq!(l.msg, "关节 1 抱 0x8130");
        assert_eq!(l.proc, "arm0");
    }

    #[test]
    fn parse_log_line_falls_back_on_garbage() {
        let l = parse_log_line("x", "not-a-timestamp line");
        assert_eq!(l.ts_ns, 0);
        assert_eq!(l.level, "");
        assert_eq!(l.msg, "not-a-timestamp line", "解析失败保留原始整行");
    }

    #[test]
    fn robot_mode_name_maps_known_and_unknown() {
        assert_eq!(robot_mode_name(1), "STANDBY");
        assert_eq!(robot_mode_name(2), "RUNNING");
        assert_eq!(robot_mode_name(3), "OVERTAKEN");
        assert_eq!(robot_mode_name(4), "FATAL_ERROR");
        assert_eq!(robot_mode_name(0), "UNSPECIFIED");
        assert_eq!(robot_mode_name(99), "UNSPECIFIED", "未知值退化为 UNSPECIFIED,不 panic");
    }

    #[test]
    fn overtaken_text_prefers_human_then_mode_name() {
        assert_eq!(overtaken_text(1, Some("摇杆接管")), "摇杆接管", "有 human_readable 用之");
        assert_eq!(overtaken_text(1, Some("")), "JOYSTICK", "空 human_readable 退到短名");
        assert_eq!(overtaken_text(2, None), "COLLISION");
        assert_eq!(overtaken_text(0, None), "UNSPECIFIED");
    }

    #[test]
    fn push_capped_evicts_oldest() {
        let mut r: VecDeque<i32> = VecDeque::new();
        for i in 0..5 {
            push_capped(&mut r, i, 3);
        }
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    fn ev(sev: i32, code: &str) -> RobotEvent {
        RobotEvent { seq: 0, severity: sev, code: code.into(), text: String::new(), kv: vec![], ts_ns: 0 }
    }

    #[test]
    fn eventbuf_push_live_assigns_monotonic_seq() {
        let mut b = EventBuf::default();
        b.push_live(ev(4, "a"));
        b.push_live(ev(3, "b"));
        let s = b.snapshot();
        assert_eq!(s.events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(s.baseline_seq, 0, "无 reseed 时 baseline 恒 0 → 首个实时事件(seq0>=0)即可通知");
    }

    #[test]
    fn eventbuf_reseed_history_below_baseline_live_above() {
        let mut b = EventBuf::default();
        // 播种 3 条历史 → 它们 seq 0..2,baseline=3。
        b.reseed(vec![ev(1, "h0"), ev(1, "h1"), ev(4, "h2")]);
        let s = b.snapshot();
        assert_eq!(s.baseline_seq, 3);
        assert!(s.events.iter().all(|e| e.seq < s.baseline_seq), "历史事件 seq 全 < baseline → 不弹通知");
        // 之后一条实时 FATAL → seq3 >= baseline → 应可通知。
        b.push_live(ev(4, "live"));
        let s2 = b.snapshot();
        let live = s2.events.last().unwrap();
        assert_eq!(live.seq, 3);
        assert!(live.seq >= s2.baseline_seq, "reseed 后的实时事件 seq >= baseline → 弹通知");
    }

    #[test]
    fn eventbuf_reseed_keeps_seq_monotonic_across_calls() {
        let mut b = EventBuf::default();
        b.push_live(ev(3, "x")); // seq 0
        b.reseed(vec![ev(1, "h")]); // seq 1, baseline 2
        b.push_live(ev(4, "y")); // seq 2 >= baseline
        let s = b.snapshot();
        assert_eq!(s.baseline_seq, 2);
        assert_eq!(s.events.last().unwrap().seq, 2, "seq 全程单调,重连前不会回退(前端水位不被旧值卡住需重连重置)");
    }
}
