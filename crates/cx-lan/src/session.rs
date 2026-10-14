//! Noise 会话：握手 + 加密帧收发。
//!
//! 用的是 `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`：
//! - **XX**：双方互相认证静态公钥（互相不知道对方公钥也能连上，适合"扫码即连"的场景）；
//! - **psk3**：在第三条消息里混入 PSK ⇒ 没有 PSK 的一方无法完成握手。
//!   PSK 由空间密钥派生，因此**只有同一个空间的设备能建立连接**。
//!
//! 握手消息与数据帧都用同一套长度前缀，避免 TCP 分段导致的半包问题。

use crate::error::LanError;
use crate::proto::{LanMessage, MAX_FRAME_BYTES};
use snow::params::NoiseParams;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// 握手模式串（改它等于换协议版本）。
pub const NOISE_PATTERN: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// 会话角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// 主动连接方（Noise 的 initiator）。
    Initiator,
    /// 接受连接方（Noise 的 responder）。
    Responder,
}

/// 一次已建立的会话。
pub struct Session {
    stream: TcpStream,
    transport: snow::TransportState,
    peer_static: [u8; 32],
    last_send: Instant,
    /// 收缓冲（一帧可能装多条消息，解完再留残余）
    recv_buf: Vec<u8>,
}

/// Noise 参数（每次解析都有开销，这里缓存）。
pub fn noise_params() -> Result<NoiseParams, LanError> {
    NOISE_PATTERN
        .parse()
        .map_err(|err| LanError::Noise(format!("Noise 模式串非法：{err}")))
}

impl Session {
    /// 在已有 TCP 连接上完成握手。
    pub fn handshake(
        mut stream: TcpStream,
        role: SessionRole,
        psk_hex: &str,
        static_private: &[u8; 32],
    ) -> Result<Self, LanError> {
        let raw = hex::decode(psk_hex)
            .map_err(|err| LanError::Config(format!("PSK 不是合法十六进制：{err}")))?;
        // Noise 的 PSK 固定 32 字节；长度不对就是配置错了，早点报出来
        let mut psk = [0u8; 32];
        if raw.len() != 32 {
            return Err(LanError::Config(format!(
                "PSK 应为 32 字节（64 位十六进制），实际 {} 字节",
                raw.len()
            )));
        }
        psk.copy_from_slice(&raw);

        // 防御性设置：即便调用方忘了，也保证握手期间是阻塞 + 低延迟。
        // （Windows 上 accept 出来的 socket 会继承非阻塞模式，见 lib.rs 的 accept_loop）
        stream
            .set_nonblocking(false)
            .map_err(|err| LanError::Io(format!("设置阻塞模式失败：{err}")))?;
        stream
            .set_nodelay(true)
            .map_err(|err| LanError::Io(format!("设置 TCP_NODELAY 失败：{err}")))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| LanError::Io(format!("设置读超时失败：{err}")))?;

        let params = noise_params()?;
        let builder = snow::Builder::new(params)
            .local_private_key(static_private)
            .map_err(|err| LanError::Noise(format!("设置静态私钥失败：{err}")))?
            // 第三条消息混入 PSK：没有 PSK 的一方算不出正确的握手哈希
            .psk(3, &psk)
            .map_err(|err| LanError::Noise(format!("设置 PSK 失败：{err}")))?;

        let mut handshake = match role {
            SessionRole::Initiator => builder
                .build_initiator()
                .map_err(|err| LanError::Noise(format!("创建 initiator 失败：{err}")))?,
            SessionRole::Responder => builder
                .build_responder()
                .map_err(|err| LanError::Noise(format!("创建 responder 失败：{err}")))?,
        };

        let mut buffer = vec![0u8; 65_535];
        if role == SessionRole::Initiator {
            // -> e
            let len = handshake
                .write_message(&[], &mut buffer)
                .map_err(|err| LanError::Noise(format!("写握手消息 1 失败：{err}")))?;
            write_frame(&mut stream, &buffer[..len])?;
            // <- e, ee, s, es
            let message = read_frame(&mut stream)?;
            handshake
                .read_message(&message, &mut buffer)
                .map_err(|err| LanError::Noise(format!("读握手消息 2 失败：{err}")))?;
            // -> s, se（含 psk3）
            let len = handshake
                .write_message(&[], &mut buffer)
                .map_err(|err| LanError::Noise(format!("写握手消息 3 失败：{err}")))?;
            write_frame(&mut stream, &buffer[..len])?;
        } else {
            // <- e
            let message = read_frame(&mut stream)?;
            handshake
                .read_message(&message, &mut buffer)
                .map_err(|err| LanError::Noise(format!("读握手消息 1 失败：{err}")))?;
            // -> e, ee, s, es
            let len = handshake
                .write_message(&[], &mut buffer)
                .map_err(|err| LanError::Noise(format!("写握手消息 2 失败：{err}")))?;
            write_frame(&mut stream, &buffer[..len])?;
            // <- s, se（含 psk3）
            let message = read_frame(&mut stream)?;
            handshake
                .read_message(&message, &mut buffer)
                .map_err(|err| LanError::Noise(format!("读握手消息 3 失败：{err}")))?;
        }

        let remote = handshake
            .get_remote_static()
            .ok_or_else(|| LanError::Noise("握手完成但没有拿到对端静态公钥".to_string()))?
            .to_vec();
        if remote.len() != 32 {
            return Err(LanError::Noise(format!(
                "对端静态公钥长度异常：{}",
                remote.len()
            )));
        }
        let mut peer_static = [0u8; 32];
        peer_static.copy_from_slice(&remote);

        let transport = handshake
            .into_transport_mode()
            .map_err(|err| LanError::Noise(format!("进入传输模式失败：{err}")))?;

        Ok(Self {
            stream,
            transport,
            peer_static,
            last_send: Instant::now(),
            recv_buf: Vec::new(),
        })
    }

    /// 对端静态公钥。
    pub fn peer_static_public(&self) -> &[u8; 32] {
        &self.peer_static
    }

    /// 设置读超时（会话循环里用短超时以便同时处理"要发的东西"）。
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), LanError> {
        self.stream
            .set_read_timeout(timeout)
            .map_err(|err| LanError::Io(format!("设置读超时失败：{err}")))
    }

    /// 发一条消息（加密后写入）。
    pub fn send(&mut self, message: &LanMessage) -> Result<(), LanError> {
        let plaintext = message.encode()?;
        let mut ciphertext = vec![0u8; plaintext.len() + 64];
        let len = self
            .transport
            .write_message(&plaintext, &mut ciphertext)
            .map_err(|err| LanError::Noise(format!("加密 {} 失败：{err}", message.name())))?;
        write_frame(&mut self.stream, &ciphertext[..len])?;
        self.last_send = Instant::now();
        Ok(())
    }

    /// 阻塞读一条消息。
    pub fn receive(&mut self) -> Result<LanMessage, LanError> {
        loop {
            if let Some(message) = self.try_parse()? {
                return Ok(message);
            }
            let frame = read_frame(&mut self.stream)?;
            let mut plaintext = vec![0u8; frame.len() + 64];
            let len = self
                .transport
                .read_message(&frame, &mut plaintext)
                .map_err(|err| LanError::Noise(format!("解密失败：{err}")))?;
            self.recv_buf.extend_from_slice(&plaintext[..len]);
        }
    }

    /// 读一条消息；读超时（且没有完整消息）时返回 `Ok(None)`。
    pub fn receive_opt(&mut self) -> Result<Option<LanMessage>, LanError> {
        if let Some(message) = self.try_parse()? {
            return Ok(Some(message));
        }
        match read_frame_opt(&mut self.stream) {
            Ok(Some(frame)) => {
                let mut plaintext = vec![0u8; frame.len() + 64];
                let len = self
                    .transport
                    .read_message(&frame, &mut plaintext)
                    .map_err(|err| LanError::Noise(format!("解密失败：{err}")))?;
                self.recv_buf.extend_from_slice(&plaintext[..len]);
                self.try_parse()
            }
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// 从缓冲区里尝试解出一条消息（一帧可能包含多条）。
    fn try_parse(&mut self) -> Result<Option<LanMessage>, LanError> {
        if self.recv_buf.is_empty() {
            return Ok(None);
        }
        // postcard 自带"读到哪算哪"的能力：take_from_bytes 返回剩余字节
        match postcard::take_from_bytes::<LanMessage>(&self.recv_buf) {
            Ok((message, rest)) => {
                let consumed = self.recv_buf.len() - rest.len();
                self.recv_buf.drain(..consumed);
                Ok(Some(message))
            }
            Err(postcard::Error::DeserializeUnexpectedEnd) => Ok(None),
            Err(err) => Err(LanError::Protocol(format!("解码消息失败：{err}"))),
        }
    }

    /// 是否该发保活（每 10 秒一次）。
    pub fn tick_ping(&mut self) -> bool {
        if self.last_send.elapsed() >= Duration::from_secs(10) {
            self.last_send = Instant::now();
            true
        } else {
            false
        }
    }
}

/// 写一个长度前缀帧。
fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<(), LanError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(LanError::Protocol(format!(
            "要发送的帧 {} 字节，超过上限 {MAX_FRAME_BYTES}",
            payload.len()
        )));
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    stream
        .write_all(&frame)
        .map_err(|err| LanError::Io(format!("写入失败：{err}")))?;
    stream
        .flush()
        .map_err(|err| LanError::Io(format!("刷新失败：{err}")))
}

/// 读一个长度前缀帧（阻塞，直到读满）。
fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, LanError> {
    let mut header = [0u8; 4];
    read_exact(stream, &mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(LanError::Protocol(format!(
            "对端声称要发 {len} 字节，超过上限 {MAX_FRAME_BYTES}（拒绝分配）"
        )));
    }
    let mut payload = vec![0u8; len];
    read_exact(stream, &mut payload)?;
    Ok(payload)
}

/// 读一个帧；读超时返回 `Ok(None)`。
fn read_frame_opt(stream: &mut TcpStream) -> Result<Option<Vec<u8>>, LanError> {
    let mut header = [0u8; 4];
    if !read_exact_opt(stream, &mut header)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(LanError::Protocol(format!(
            "对端声称要发 {len} 字节，超过上限 {MAX_FRAME_BYTES}（拒绝分配）"
        )));
    }
    let mut payload = vec![0u8; len];
    read_exact(stream, &mut payload)?;
    Ok(Some(payload))
}

fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), LanError> {
    stream
        .read_exact(buffer)
        .map_err(|err| LanError::Io(format!("读取失败：{err}")))
}

/// 读满 `buffer`；第一个字节就超时则返回 `false`。
fn read_exact_opt(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<bool, LanError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(LanError::Io("对端关闭了连接".to_string()));
            }
            Ok(n) => filled += n,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if filled == 0 {
                    return Ok(false);
                }
                // 半包：继续等（否则会把一个帧的两半当成两个帧）
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(LanError::Io(format!("读取失败：{err}"))),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::OfferEntry;
    use std::net::{TcpListener, TcpStream};

    fn identity(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    /// 私钥 [seed;32] 对应的公钥。
    pub(crate) fn public_from_private_seed(seed: u8) -> [u8; 32] {
        curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped([seed; 32]).to_bytes()
    }

    /// 在真实 TCP 回环连接上跑一次握手 + 消息往返。
    fn handshake_pair() -> (Session, Session) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let psk = crate::derive_psk("aabb", None);

        let server = {
            let psk = psk.clone();
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                Session::handshake(stream, SessionRole::Responder, &psk, &identity(2)).unwrap()
            })
        };
        let client_stream = TcpStream::connect(addr).unwrap();
        let client =
            Session::handshake(client_stream, SessionRole::Initiator, &psk, &identity(1)).unwrap();
        let server = server.join().unwrap();
        (client, server)
    }

    #[test]
    fn 握手成功并互认身份() {
        let (mut client, mut server) = handshake_pair();
        // XX 模式下双方都能拿到对端的**静态公钥**（不是私钥）
        let public_of = |seed: u8| super::tests::public_from_private_seed(seed);
        assert_eq!(client.peer_static_public(), &public_of(2));
        assert_eq!(server.peer_static_public(), &public_of(1));

        client
            .send(&LanMessage::Item {
                name: "a.cxi".to_string(),
                bytes: b"hello".to_vec(),
            })
            .unwrap();
        let received = server.receive().unwrap();
        assert_eq!(
            received,
            LanMessage::Item {
                name: "a.cxi".to_string(),
                bytes: b"hello".to_vec()
            }
        );

        // 反向也通
        server
            .send(&LanMessage::Manifest {
                device_id: "dev-b".to_string(),
                entries: vec![OfferEntry {
                    name: "b.cxi".to_string(),
                    created_ms: 1,
                    size: 2,
                    kind: "text".to_string(),
                }],
            })
            .unwrap();
        match client.receive().unwrap() {
            LanMessage::Manifest { device_id, entries } => {
                assert_eq!(device_id, "dev-b");
                assert_eq!(entries.len(), 1);
            }
            other => panic!("期望 Manifest，实际 {}", other.name()),
        }
    }

    #[test]
    fn psk_不一致时握手失败() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let good = crate::derive_psk("aa", None);
        let bad = crate::derive_psk("bb", None);
        assert_ne!(good, bad, "不同材料必须派生出不同 PSK");

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            Session::handshake(stream, SessionRole::Responder, &good, &identity(2))
        });
        let client = Session::handshake(
            TcpStream::connect(addr).unwrap(),
            SessionRole::Initiator,
            &bad,
            &identity(1),
        );
        // 至少有一侧必须失败；理想情况下双方都失败
        let server = server.join().unwrap();
        assert!(
            client.is_err() || server.is_err(),
            "PSK 不一致时不该握手成功（client={:?} server={:?}）",
            client.as_ref().err().map(|e| e.to_string()),
            server.as_ref().err().map(|e| e.to_string())
        );
    }

    #[test]
    fn 篡改密文会被检测出来() {
        let (mut client, mut server) = handshake_pair();
        client
            .send(&LanMessage::Item {
                name: "a.cxi".to_string(),
                bytes: b"payload".to_vec(),
            })
            .unwrap();
        // 从服务端读一帧后翻转一个 bit 再喂回去：AEAD 必须报错
        let mut stream = server.stream.try_clone().unwrap();
        let frame = read_frame(&mut stream).unwrap();
        let mut tampered = frame.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;

        let mut plaintext = vec![0u8; tampered.len() + 64];
        let result = server.transport.read_message(&tampered, &mut plaintext);
        assert!(result.is_err(), "篡改过的密文必须解密失败");
    }

    #[test]
    fn 超大帧被拒绝而不是分配内存() {
        // 用一对裸 TCP：一侧写一个声称 1GB 的帧头，另一侧必须拒绝而不是分配 1GB
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_frame(&mut stream)
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .write_all(&(1024u32 * 1024 * 1024).to_be_bytes())
            .unwrap();
        let err = server.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("拒绝分配"), "{err}");
    }

    #[test]
    fn 一帧可以装多条消息() {
        let (mut client, mut server) = handshake_pair();
        // 手工把两条消息拼成一条明文再发（模拟"读缓冲里有多条"）
        let first = LanMessage::Ping.encode().unwrap();
        let second = LanMessage::Want {
            names: vec!["x".to_string()],
        }
        .encode()
        .unwrap();
        let mut plaintext = first.clone();
        plaintext.extend_from_slice(&second);
        let mut ciphertext = vec![0u8; plaintext.len() + 64];
        let len = client
            .transport
            .write_message(&plaintext, &mut ciphertext)
            .unwrap();
        write_frame(&mut client.stream, &ciphertext[..len]).unwrap();

        assert_eq!(server.receive().unwrap(), LanMessage::Ping);
        assert_eq!(
            server.receive().unwrap(),
            LanMessage::Want {
                names: vec!["x".to_string()]
            }
        );
    }

    #[test]
    fn psk_派生对输入敏感() {
        assert_eq!(derive_psk_for_test("abc"), derive_psk_for_test("abc"));
        assert_ne!(derive_psk_for_test("abc"), derive_psk_for_test("abd"));
        // 配对码优先于空间密钥
        let with_code = crate::derive_psk("abc", Some("1234"));
        assert_ne!(with_code, derive_psk_for_test("abc"));
        assert_eq!(with_code.len(), 64);
    }

    fn derive_psk_for_test(material: &str) -> String {
        crate::derive_psk(material, None)
    }
}
