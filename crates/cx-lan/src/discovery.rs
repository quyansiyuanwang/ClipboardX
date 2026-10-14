//! mDNS 发现（`_clipboardx._tcp.local`）。
//!
//! 只通告**必要信息**：mDNS 是明文广播，网段里任何人都能看见。因此：
//! - 通告设备名、设备 id、端口；
//! - 空间只通告**哈希前缀**（用于过滤"不是我的设备"，但不直接暴露空间 id）；
//! - 真正的身份认证与加密在 Noise 层（PSK 由空间密钥派生，没加入空间的人连不上）。
//!
//! 发现是"尽力而为"：很多企业网络与容器环境禁掉多播，因此显式对端地址永远是
//! 可用的兜底（见 [`crate::LanConfig::peers`]）。

use crate::error::LanError;
use crate::space_hash;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// 一条发现结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// 服务实例名（展示用）。
    pub name: String,
    /// 设备 id（TXT 记录）。
    pub device_id: Option<String>,
    /// 空间哈希（用于确认是同一空间）。
    pub space_hash: Option<String>,
    /// 可连接的地址。
    pub addr: SocketAddr,
}

/// mDNS 发现句柄。
pub struct Discovery {
    daemon: ServiceDaemon,
    /// mdns-sd 的浏览通道（内部就是 flume）。
    receiver: mdns_sd::Receiver<ServiceEvent>,
    /// 本机设备 id（用于过滤自己）。
    local_device_id: String,
    /// 本机空间哈希（用于过滤不属于本空间的实例）。
    local_space_hash: Option<String>,
}

impl Discovery {
    /// 注册本机并开始浏览。
    pub fn start(config: &crate::LanConfig, port: u16) -> Result<Self, LanError> {
        let daemon =
            ServiceDaemon::new().map_err(|err| LanError::Io(format!("启动 mDNS 失败：{err}")))?;

        let instance = format!("clipboardx-{}", &config.device_name);
        let host = format!("{}.local.", hostname_fallback());
        let mut properties = vec![
            ("v".to_string(), "1".to_string()),
            ("name".to_string(), config.device_name.clone()),
            ("device_id".to_string(), config.device_id.clone()),
        ];
        if let Some(space_id) = &config.space_id {
            properties.push(("space".to_string(), space_hash(space_id)));
        }

        // mdns-sd 不接受 0.0.0.0，这里枚举本机可用地址；在无网卡的 CI 上会返回空集合，
        // 此时注册会失败——那也是"没有局域网"的正常情况，按失败处理即可。
        let addresses = local_ip_addresses();
        if addresses.is_empty() {
            return Err(LanError::Io(
                "本机没有可用的非回环地址（可能没有网卡）".to_string(),
            ));
        }
        let info = ServiceInfo::new(
            crate::SERVICE_TYPE,
            &instance,
            &host,
            addresses.as_slice(),
            port,
            Some(properties.into_iter().collect()),
        )
        .map_err(|err| LanError::Config(format!("构造 mDNS 服务信息失败：{err}")))?;

        daemon
            .register(info)
            .map_err(|err| LanError::Io(format!("注册 mDNS 服务失败：{err}")))?;
        let receiver = daemon
            .browse(crate::SERVICE_TYPE)
            .map_err(|err| LanError::Io(format!("浏览 mDNS 服务失败：{err}")))?;

        Ok(Self {
            daemon,
            receiver,
            local_device_id: config.device_id.clone(),
            local_space_hash: config.space_id.as_deref().map(space_hash),
        })
    }

    /// 取下一个发现到的对端（最多等 500ms）。
    pub fn next_peer(&self) -> Result<DiscoveredPeer, LanError> {
        let event = self
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .map_err(|_| LanError::Timeout)?;
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let props = info.get_properties();
                let device_id = props
                    .get_property_val_str("device_id")
                    .map(|v| v.to_string());
                let name = props
                    .get_property_val_str("name")
                    .unwrap_or(info.get_fullname())
                    .to_string();
                let peer_space = props.get_property_val_str("space").map(|v| v.to_string());

                // 过滤：不属于本空间实例的，不看
                if let (Some(mine), Some(theirs)) = (&self.local_space_hash, &peer_space)
                    && mine != theirs
                {
                    return Err(LanError::Timeout);
                }
                // 过滤自己（同一个进程注册的实例有时也会被自己发现）
                if device_id.as_deref() == Some(self.local_device_id.as_str()) {
                    return Err(LanError::Timeout);
                }

                // 优先取 IPv4 非回环地址（mdns-sd 0.21 给的是 ScopedIp，需转成 IpAddr）
                let candidates: Vec<IpAddr> = info
                    .get_addresses()
                    .iter()
                    .map(|scoped| scoped.to_ip_addr())
                    .collect();
                let ip = candidates
                    .iter()
                    .find(|ip| ip.is_ipv4() && !ip.is_loopback())
                    .or_else(|| candidates.iter().find(|ip| !ip.is_loopback()))
                    .or_else(|| candidates.first())
                    .copied()
                    .ok_or(LanError::Timeout)?;
                Ok(DiscoveredPeer {
                    name,
                    device_id,
                    space_hash: peer_space,
                    addr: SocketAddr::new(ip, info.get_port()),
                })
            }
            _ => Err(LanError::Timeout),
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(crate::SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

/// 本机可用的非回环地址。
fn local_ip_addresses() -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    if let Ok(interfaces) = if_addrs_workaround() {
        for addr in interfaces {
            if !addr.is_loopback() {
                addresses.push(addr);
            }
        }
    }
    addresses
}

/// 拿本机地址：用一个"连 UDP 到公网地址"的小技巧拿到出接口地址——
/// 不依赖任何额外 crate，也不需要真的发包（UDP connect 不发数据）。
fn if_addrs_workaround() -> Result<HashSet<IpAddr>, std::io::Error> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    // 203.0.113.0/24 是文档保留地址，不会真的到达；只为让内核选出接口
    let _ = socket.connect("203.0.113.1:9");
    let mut addresses = HashSet::new();
    if let Ok(local) = socket.local_addr()
        && !local.ip().is_loopback()
    {
        addresses.insert(local.ip());
    }
    Ok(addresses)
}

/// 主机名兜底（`hostname` 不一定存在）。
fn hostname_fallback() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "clipboardx".to_string())
        .to_ascii_lowercase()
        .replace([' ', '_'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 主机名兜底不会是空串() {
        assert!(!hostname_fallback().is_empty());
        assert!(!hostname_fallback().contains(' '));
    }

    #[test]
    fn 本机地址不含回环() {
        for addr in local_ip_addresses() {
            assert!(!addr.is_loopback(), "不应包含回环地址：{addr}");
        }
    }
}
