//! 网线直连(免 DHCP)时把控制器广播的 IPv6 链路本地 locator 变成**可连**的 endpoint。
//!
//! # 为什么需要这一层
//!
//! 一根网线直连控制板、两端都没有 DHCP 服务器时,`zenoh::open` 的组播自动发现能收到
//! 控制器的 HELLO,却**连不上它** —— 三条被广播的 locator 没有一条能直接用:
//!
//! - `tcp/[fe80::…]:7447`:链路本地地址**不带 scope 就无法路由**。而 scope 是本机
//!   ifindex,对端不可能替我们填 —— 它在别的主机上是另一个数字。**必须由接收方补**,
//!   这是链路本地地址的固有性质,不是谁的 bug。
//! - `tcp/<Wi-Fi 地址>:7447`:TCP 连得上(经办公室网),但控制器的
//!   `ignored_interfaces` ACL 会把经 Wi-Fi 进来的消息全部 deny,查询永远返回 0 条。
//! - 控制板有线口自动获得的 IPv4 链路本地(`169.254/16`)**根本不会被广播** ——
//!   zenoh 在展开通配 listen 时显式过滤掉它(`zenoh-util/src/net/mod.rs`)。
//!
//! 于是本模块做一件事:把 `fe80::` locator 补上**本机**的 ifindex 再交给 zenoh。
//!
//! # 为什么补的是数字而不是网卡名
//!
//! zenoh 的 endpoint 解析走 Rust `std` 的 `SocketAddrV6` 字面量解析,它支持
//! `%<数字>` 但不支持 `%<网卡名>`;带名字时会退化成 DNS 解析并以
//! `failed to lookup address information` 失败。这是 `std` 的行为,与操作系统无关,
//! 所以 Linux / macOS / Windows 三家都得用数字。
//!
//! 详细实测记录见 `hex-controller/todo/direct-cable-discovery.md`。

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// 逐个候选 endpoint 试探 TCP 的超时。直连线上握手是亚毫秒级,给足余量即可;
/// 宁可短一点 —— 候选数量等于「链路本地 locator 数 × 本机网卡数」,串行试探时
/// 每个不可达的候选都要等满这个时间。
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// 一块可以用来补 scope 的本机网卡。
///
/// `index` 就是 IPv6 endpoint 里 `%` 后面那个数字。GUI 把这张对应表显示出来,
/// 用户一眼就能判断某条 `[fe80::…%2]` 走的是哪块网卡 —— 尤其是"这条是不是 Wi-Fi"。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ScopeCandidate {
    pub name: String,
    pub index: u32,
    /// 该网卡上的地址,逗号分隔。给人看的辅助信息,不参与任何判定。
    pub addrs: Vec<String>,
}

/// 列出可用于补 scope 的本机网卡(排除回环与拿不到 index 的)。
///
/// **不**按「有没有 IPv4 地址」筛:那是组播 scouting 的前提,不是能不能承载
/// 链路本地 TCP 的前提;真正能不能连由 [`reachable_endpoints`] 的探测说了算。
pub(crate) fn scope_candidates() -> Vec<ScopeCandidate> {
    let mut out: Vec<ScopeCandidate> = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return out;
    };
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        let Some(index) = iface.index else { continue };
        let addr = iface.ip().to_string();
        // 同一块网卡会有多个地址,归并到同一条并把地址都收进去。
        if let Some(existing) = out.iter_mut().find(|c| c.index == index) {
            if !existing.addrs.contains(&addr) {
                existing.addrs.push(addr);
            }
            continue;
        }
        out.push(ScopeCandidate {
            name: iface.name,
            index,
            addrs: vec![addr],
        });
    }
    out.sort_by_key(|c| c.index);
    out
}

/// 判断一条 locator 是不是**无 scope 的** IPv6 链路本地(`fe80::/10`)。
///
/// 已经带了 `%` 的不算 —— 那说明来源已经指定了 scope,不该再改。
pub(crate) fn is_unscoped_link_local(locator: &str) -> bool {
    let Some(open) = locator.find('[') else {
        return false;
    };
    let Some(close) = locator[open..].find(']') else {
        return false;
    };
    let host = &locator[open + 1..open + close];
    if host.contains('%') {
        return false;
    }
    host.parse::<std::net::Ipv6Addr>()
        .map(|a| (a.segments()[0] & 0xffc0) == 0xfe80)
        .unwrap_or(false)
}

/// 给一条无 scope 的链路本地 locator 补上 ifindex。非链路本地或已带 scope 时返回 `None`。
///
/// ```text
/// tcp/[fe80::5448:2ff:fe37:5a3c]:7447  +  13
///   → tcp/[fe80::5448:2ff:fe37:5a3c%13]:7447
/// ```
pub(crate) fn scope_link_local(locator: &str, index: u32) -> Option<String> {
    if !is_unscoped_link_local(locator) {
        return None;
    }
    let close = locator.find(']')?;
    Some(format!(
        "{}%{index}{}",
        &locator[..close],
        &locator[close..]
    ))
}

/// 把广播 locator 里的**链路本地**那些展开成候选 endpoint(每块本机网卡一条)。
///
/// **刻意丢掉非链路本地的 locator**:那些 zenoh 自己就能连(组播发现照常开着),不需要
/// 我们插手。硬塞进 `connect/endpoints` 反而有害 —— 控制器的 Wi-Fi 地址就在其中,而经
/// Wi-Fi 进来的消息会被 `ignored_interfaces` ACL 全部 deny;万一那条链路先建起来并赢得
/// 会话(每个 zid 只留一条链路),GUI 就会"看得见控制器、但什么都查不到"。
pub(crate) fn expand_candidates(locators: &[String], scopes: &[ScopeCandidate]) -> Vec<String> {
    let mut out = Vec::new();
    for locator in locators {
        if !is_unscoped_link_local(locator) {
            continue;
        }
        for scope in scopes {
            if let Some(scoped) = scope_link_local(locator, scope.index) {
                if !out.contains(&scoped) {
                    out.push(scoped);
                }
            }
        }
    }
    out
}

/// 从 endpoint 串里抠出 `host:port`,供裸 TCP 探测用。
fn socket_addr_of(endpoint: &str) -> Option<SocketAddr> {
    let addr = endpoint.rsplit('/').next()?; // 去掉 "tcp/" 之类的前缀
    addr.parse::<SocketAddr>().ok()
}

/// 逐个裸 TCP 探测,只留真正连得通的。
///
/// **刻意先自己探再交给 zenoh**:直接把一堆候选塞进 `connect/endpoints`,连不通的那些
/// 会被 zenoh 无限重试(指数退避),日志刷屏且永远不收敛;而且 GUI 也就无从知道究竟是
/// 哪块网卡通了 —— 那正是要显示给用户看的信息。
pub(crate) fn reachable_endpoints(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|endpoint| {
            socket_addr_of(endpoint)
                .map(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unscoped_link_local_only() {
        assert!(is_unscoped_link_local(
            "tcp/[fe80::5448:2ff:fe37:5a3c]:7447"
        ));
        assert!(
            !is_unscoped_link_local("tcp/[fe80::5448:2ff:fe37:5a3c%13]:7447"),
            "已带 scope 的不该再动"
        );
        assert!(
            !is_unscoped_link_local("tcp/[2001:db8::1]:7447"),
            "全局地址不是链路本地"
        );
        assert!(!is_unscoped_link_local("tcp/192.168.3.100:7447"));
        assert!(!is_unscoped_link_local("tcp/[not-an-addr]:7447"));
    }

    /// 必须补**数字**。补网卡名会让 zenoh 退化成 DNS 解析并失败,
    /// 那正是这个模块存在的理由。
    #[test]
    fn scopes_with_numeric_index() {
        assert_eq!(
            scope_link_local("tcp/[fe80::5448:2ff:fe37:5a3c]:7447", 13).as_deref(),
            Some("tcp/[fe80::5448:2ff:fe37:5a3c%13]:7447"),
        );
        assert_eq!(scope_link_local("tcp/192.168.3.100:7447", 13), None);
        assert_eq!(
            scope_link_local("tcp/[fe80::1%3]:7447", 13),
            None,
            "已带 scope 的不该被二次加工"
        );
    }

    #[test]
    fn expands_link_local_per_interface_and_drops_the_rest() {
        let scopes = vec![
            ScopeCandidate {
                name: "eth0".into(),
                index: 2,
                addrs: vec![],
            },
            ScopeCandidate {
                name: "usb0".into(),
                index: 13,
                addrs: vec![],
            },
        ];
        let locators = vec![
            "tcp/[fe80::1]:7447".to_string(),
            "tcp/192.168.3.100:7447".to_string(),
        ];
        assert_eq!(
            expand_candidates(&locators, &scopes),
            vec!["tcp/[fe80::1%2]:7447", "tcp/[fe80::1%13]:7447"],
            "非链路本地的必须被丢掉:zenoh 自己能连,而其中的 Wi-Fi 地址会被 ACL deny",
        );
    }

    /// 同一条链路本地被两块网卡展开后不能重复,否则 zenoh 会对同一个 endpoint 连两次。
    #[test]
    fn expansion_is_deduplicated() {
        let scopes = vec![ScopeCandidate {
            name: "eth0".into(),
            index: 2,
            addrs: vec![],
        }];
        let locators = vec![
            "tcp/[fe80::1]:7447".to_string(),
            "tcp/[fe80::1]:7447".to_string(),
        ];
        assert_eq!(
            expand_candidates(&locators, &scopes),
            vec!["tcp/[fe80::1%2]:7447"]
        );
    }

    #[test]
    fn parses_socket_addr_from_endpoint() {
        assert!(socket_addr_of("tcp/192.168.3.100:7447").is_some());
        assert!(socket_addr_of("tcp/[fe80::1%13]:7447").is_some());
        assert!(socket_addr_of("tcp/nonsense").is_none());
    }
}
