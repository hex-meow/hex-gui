//! 用 mDNS/DNS-SD 找直连(或同网段)的控制器,产出**可直接粘贴/连接**的 endpoint。
//!
//! # 为什么不靠 zenoh 自己的组播发现
//!
//! zenoh 把本机网卡列表缓存成进程级快照(`zenoh-util` 里的 `lazy_static IFACES`),
//! 且组播网卡的筛选条件要求"该网卡已有 IPv4 地址"。链路本地地址往往在进程起来**之后**
//! 才出现 —— macOS 要等 DHCP 超时才自赋 `169.254`,Linux 的 NetworkManager 也要先激活
//! 成功 —— 于是那块网卡被永久当成不可用,**重启客户端才有救**。先开 GUI 再插网线同理。
//!
//! mDNS 走独立解析器与实时系统 API,不受该快照影响;拿到地址后用**显式** endpoint 连接,
//! 也不走 zenoh 的网卡枚举。这条路径与"进程启动和地址分配谁先谁后"无关。
//!
//! 实测记录见 `hex-controller/todo/direct-cable-discovery.md`。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

/// 控制器广播的 DNS-SD 服务类型。板子侧定义在 br-external 的
/// `overlay/etc/avahi/services/hexmeow-controller.service`。
const SERVICE_TYPE: &str = "_hexmeow-ctrl._tcp.local.";

/// 浏览时长。mDNS 是"边收边报",直连线上通常几百毫秒就有结果;给到 2s 是为了覆盖
/// 无线/交换机上的重传。再长只会让用户干等。
const BROWSE_WINDOW: Duration = Duration::from_millis(2000);

/// 一台被发现的控制器。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiscoveredController {
    /// mDNS 实例名。同链路重名时 avahi 会自动消解成 `<name>-2`,故始终可区分。
    pub instance: String,
    /// 主机名(`<host>.local`)。可直接用于 `ssh root@<host>` 调试。
    pub hostname: String,
    /// 可直接粘进"Zenoh endpoint"输入框的候选,已按可用性排好序。
    pub endpoints: Vec<String>,
}

/// 把一个地址转成 zenoh endpoint。
///
/// IPv6 链路本地必须带 **数字** scope:zenoh 的 endpoint 解析走 Rust `std` 的
/// `SocketAddrV6` 字面量解析,支持 `%<数字>` 但不支持 `%<网卡名>`(后者退化成 DNS
/// 解析并失败)。scope 是**本机** ifindex,对端不可能替我们填。
pub(crate) fn endpoint_for(addr: IpAddr, port: u16, scope: Option<u32>) -> Option<String> {
    match addr {
        IpAddr::V4(v4) => Some(format!("tcp/{v4}:{port}")),
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => {
            scope.map(|idx| format!("tcp/[{v6}%{idx}]:{port}"))
        }
        IpAddr::V6(v6) => Some(format!("tcp/[{v6}]:{port}")),
    }
}

/// endpoint 排序权重:越小越优先展示。
///
/// IPv4 链路本地排最前 —— 它不需要 scope,复制粘贴到别处(另一台机器、ssh、脚本)也照样
/// 能用;带 scope 的 IPv6 只在**本机**有意义,换台机器 ifindex 就变了,放后面。
/// 全局地址排最后:直连场景下它多半是板子的 Wi-Fi 地址,而经 Wi-Fi 进来的消息会被
/// 控制器的 `ignored_interfaces` ACL 拒掉。
pub(crate) fn endpoint_rank(endpoint: &str) -> u8 {
    if endpoint.contains("169.254.") {
        0
    } else if endpoint.contains('%') {
        1
    } else {
        2
    }
}

/// 浏览一轮 mDNS,返回发现的控制器。
///
/// 阻塞 [`BROWSE_WINDOW`];调用方应放到 blocking 线程里。
pub(crate) fn browse() -> anyhow::Result<Vec<DiscoveredController>> {
    let daemon =
        mdns_sd::ServiceDaemon::new().map_err(|e| anyhow::anyhow!("启动 mDNS 浏览器失败:{e}"))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| anyhow::anyhow!("浏览 {SERVICE_TYPE} 失败:{e}"))?;

    // 同一台设备会在多块网卡上各报一次,按实例名归并。
    let mut found: BTreeMap<String, DiscoveredController> = BTreeMap::new();
    let deadline = Instant::now() + BROWSE_WINDOW;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = receiver.recv_timeout(left) else {
            break;
        };
        let mdns_sd::ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        let instance = info.get_fullname().to_string();
        let hostname = info.get_hostname().trim_end_matches('.').to_string();
        let port = info.get_port();
        let entry = found
            .entry(instance.clone())
            .or_insert_with(|| DiscoveredController {
                instance,
                hostname,
                endpoints: Vec::new(),
            });
        for scoped in info.get_addresses() {
            // mdns-sd 直接给出**这条记录是从哪块本机网卡收到的**(InterfaceId.index),
            // 所以链路本地 IPv6 的 scope 不用猜、也不用逐网卡展开再探测 —— 收到它的那块
            // 网卡就是唯一能路由到它的那块。
            let scope = match scoped {
                mdns_sd::ScopedIp::V6(v6) => Some(v6.scope_id().index),
                _ => None,
            };
            if let Some(endpoint) = endpoint_for(scoped.to_ip_addr(), port, scope) {
                if !entry.endpoints.contains(&endpoint) {
                    entry.endpoints.push(endpoint);
                }
            }
        }
    }
    let _ = daemon.shutdown();

    let mut out: Vec<DiscoveredController> = found.into_values().collect();
    for controller in &mut out {
        // 只留真正连得通的,再按"复制出去还能用"排序。留着连不通的候选等于让用户去猜。
        controller.endpoints = crate::zenoh_linklocal::reachable_endpoints(&controller.endpoints);
        controller.endpoints.sort_by_key(|e| endpoint_rank(e));
    }
    // 一个可用 endpoint 都没有的条目仍然保留:它至少告诉用户"设备在,但这条链路不通",
    // 比直接消失有用得多 —— 那正是网卡没配好时的典型症状。
    out.sort_by(|a, b| a.instance.cmp(&b.instance));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_endpoints_per_address_family() {
        assert_eq!(
            endpoint_for("169.254.60.43".parse().unwrap(), 7447, None).as_deref(),
            Some("tcp/169.254.60.43:7447"),
        );
        assert_eq!(
            endpoint_for("2001:db8::1".parse().unwrap(), 7447, None).as_deref(),
            Some("tcp/[2001:db8::1]:7447"),
        );
    }

    /// 链路本地 IPv6 没有 scope 就是不可用的地址,必须**不产出** endpoint,
    /// 而不是产出一条注定连不上的。
    #[test]
    fn link_local_v6_requires_a_numeric_scope() {
        let addr: IpAddr = "fe80::5448:2ff:fe37:5a3c".parse().unwrap();
        assert_eq!(endpoint_for(addr, 7447, None), None);
        assert_eq!(
            endpoint_for(addr, 7447, Some(13)).as_deref(),
            Some("tcp/[fe80::5448:2ff:fe37:5a3c%13]:7447"),
        );
    }

    /// 排序决定用户默认复制到哪一条。IPv4 链路本地换台机器也能用,带 scope 的不行,
    /// 而全局地址在直连场景下多半是会被 ACL 拒掉的 Wi-Fi 地址。
    #[test]
    fn ranks_portable_endpoints_first() {
        let mut endpoints = vec![
            "tcp/172.18.12.146:7447".to_string(),
            "tcp/[fe80::1%13]:7447".to_string(),
            "tcp/169.254.60.43:7447".to_string(),
        ];
        endpoints.sort_by_key(|e| endpoint_rank(e));
        assert_eq!(
            endpoints,
            vec![
                "tcp/169.254.60.43:7447",
                "tcp/[fe80::1%13]:7447",
                "tcp/172.18.12.146:7447",
            ],
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// 真硬件冒烟:需要一台在同一链路上广播 `_hexmeow-ctrl._tcp` 的控制器。
    /// 默认 `#[ignore]`,手动跑:
    /// `cargo test --lib -- --ignored --nocapture mdns_finds`
    #[test]
    #[ignore]
    fn mdns_finds_a_controller_with_a_usable_endpoint() {
        let found = browse().expect("browse");
        println!("发现 {} 台控制器:", found.len());
        for c in &found {
            println!("  实例 {}", c.instance);
            println!("  主机 {}  (ssh root@{})", c.hostname, c.hostname);
            println!("  可用 endpoint {:?}", c.endpoints);
        }
        assert!(
            !found.is_empty(),
            "没发现控制器:确认板子在广播且网卡有链路本地地址"
        );
        assert!(
            found.iter().any(|c| !c.endpoints.is_empty()),
            "发现了设备但没有一条 endpoint 连得通 —— 典型是本机网卡没配好",
        );
    }
}
