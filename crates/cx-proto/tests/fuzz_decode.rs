//! 解码路径的模糊测试（确定性变异，不需要额外依赖）。
//!
//! 为什么不用 `cargo-fuzz`：它要 nightly 与专门的运行环境，而这里真正要保证的只有
//! 一件事——**任何输入都不能让解码 panic**。用确定性伪随机做变异，可以在 `cargo test`
//! 里、在三端 CI 上每次都跑，还能复现（种子固定）。
//!
//! 覆盖的解码路径：item 容器、manifest JSON、控制协议 JSON、对象名解析。
//! 这些地方的历史战绩：截断、非法 UTF-8、超长字段、错误的变体标签都曾让解析器出错
//! （见各 crate 的测试注释）。

use cx_proto::{
    Cipher, Item, ItemMeta, ItemName, Kind, Manifest, ManifestEntry, content_hash,
    control::{Reply, Request},
};

/// 确定性伪随机（xorshift64*）：同一 seed 每次得到同一串变异。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// 造一个合法的 item 容器。
fn sample_item() -> Vec<u8> {
    let plain = "剪贴板内容 with ASCII and 🎉".as_bytes();
    let item = Item::new(
        Kind::Text,
        Cipher::None,
        ItemMeta {
            id: vec![7u8; cx_proto::ID_LEN],
            origin_device: "ab12cd34ef5678901234567890abcdef".to_string(),
            origin_name: "测试设备".to_string(),
            created_ms: 1_759_323_998_000,
            seq: 3,
            kind: Kind::Text,
            mime: Some("text/plain".to_string()),
            plain_hash: content_hash(plain).to_vec(),
            plain_len: plain.len() as u32,
            nonce: Vec::new(),
        },
        plain.to_vec(),
    )
    .unwrap();
    item.encode().unwrap()
}

fn sample_manifest() -> Vec<u8> {
    let mut manifest = Manifest::new(
        "ab12cd34ef5678901234567890abcdef",
        "笔记本",
        1_759_323_998_000,
    );
    manifest.push(ManifestEntry {
        name: "1759323998000-ab12cd34-9f3c1e77.cxi".to_string(),
        day: "20251001".to_string(),
        size: 123,
        hash: "a".repeat(64),
        kind: Kind::Text,
        created_ms: 1_759_323_998_000,
        mime: None,
    });
    manifest.to_json().unwrap().into_bytes()
}

/// 对字节串做变异：翻转、截断、插入、删块。
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut bytes = input.to_vec();
    if bytes.is_empty() {
        return bytes;
    }
    let rounds = 1 + rng.below(6);
    for _ in 0..rounds {
        match rng.below(5) {
            0 => {
                // 翻转一位
                let index = rng.below(bytes.len());
                bytes[index] ^= 1 << rng.below(8);
            }
            1 => {
                // 截断
                let keep = rng.below(bytes.len());
                bytes.truncate(keep);
            }
            2 => {
                // 插入随机字节
                let index = rng.below(bytes.len());
                bytes.insert(index, (rng.next() & 0xff) as u8);
            }
            3 => {
                // 删一段
                let start = rng.below(bytes.len());
                let end = (start + 1 + rng.below(bytes.len() - start)).min(bytes.len());
                bytes.drain(start..end);
            }
            _ => {
                // 把一段换成 0xff（容易触发长度/UTF-8 边界）
                let start = rng.below(bytes.len());
                let end = (start + 1 + rng.below(4)).min(bytes.len());
                for byte in &mut bytes[start..end] {
                    *byte = 0xff;
                }
            }
        }
        if bytes.is_empty() {
            break;
        }
    }
    bytes
}

#[test]
fn item_解码在任意输入下都不_panic() {
    let valid = sample_item();
    let mut rng = Rng::new(0xC0FFEE);
    let mut decoded_ok = 0;
    for round in 0..2_000 {
        let input = mutate(&mut rng, &valid);
        // 允许失败，但不许 panic；解出来的东西还必须自洽
        if let Ok(item) = Item::decode(&input) {
            decoded_ok += 1;
            assert!(
                item.payload.len() as u32 == item.meta.plain_len || item.cipher != Cipher::None,
                "第 {round} 轮：明文未被加密时 plain_len 必须等于载荷长度"
            );
        }
    }
    // 变异有概率"没改到关键处"从而仍然解出，这不影响结论；这里只是防止测试静默失效
    assert!(decoded_ok < 2_000, "变异应当至少让一部分输入失败");

    // 纯垃圾输入
    for seed in 0..500u64 {
        let mut local = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let len = local.below(64);
        let junk: Vec<u8> = (0..len).map(|_| (local.next() & 0xff) as u8).collect();
        let _ = Item::decode(&junk);
    }
}

#[test]
fn manifest_解析在任意输入下都不_panic() {
    let valid = sample_manifest();
    let mut rng = Rng::new(0xBEEF);
    for _ in 0..1_500 {
        let input = mutate(&mut rng, &valid);
        if let Ok(text) = std::str::from_utf8(&input) {
            // 允许失败，但不许 panic；解析成功的必须能再序列化回去
            if let Ok(manifest) = Manifest::from_json(text) {
                let round_trip = manifest.to_json().expect("能解析就应当能序列化");
                assert!(round_trip.contains('{'));
            }
        }
    }

    // 结构性变异：合法 JSON 但字段类型错/缺失
    for bad in [
        "{}",
        "[]",
        "null",
        r#"{"v":1}"#,
        r#"{"v":"一","device_id":1,"items":"no"}"#,
        r#"{"v":1,"device_id":"x","items":[{"name":1}]}"#,
        r#"{"v":1,"device_id":"x","items":[{"name":"a","created_ms":-1}]}"#,
        &format!(
            r#"{{"v":1,"device_id":"x","items":[{{"name":"a","hash":"{}"}}]}}"#,
            "z".repeat(5000)
        ),
    ] {
        let _ = Manifest::from_json(bad);
    }
}

#[test]
fn 控制协议_解析在任意输入下都不_panic() {
    let request = br#"{"id":7,"cmd":"history.list","args":{"limit":20}}"#;
    let reply = br#"{"id":7,"ok":true,"data":{"paused":false}}"#;

    let mut rng = Rng::new(0x5EED);
    for _ in 0..1_500 {
        let mutated = mutate(&mut rng, request);
        if let Ok(text) = std::str::from_utf8(&mutated) {
            let _ = serde_json::from_str::<Request>(text);
        }
        let mutated = mutate(&mut rng, reply);
        if let Ok(text) = std::str::from_utf8(&mutated) {
            let _ = serde_json::from_str::<Reply>(text);
        }
    }

    // 深嵌套（防止栈溢出）
    let deep = format!(
        "{{\"id\":1,\"cmd\":\"x\",\"args\":{}{}}}",
        "[".repeat(200),
        "]".repeat(200)
    );
    let _ = serde_json::from_str::<Request>(&deep);
}

#[test]
fn 对象名解析在任意输入下都不_panic() {
    let valid = "1759323998000-ab12cd34-9f3c1e77.cxi";
    let mut rng = Rng::new(0x1234);
    for _ in 0..2_000 {
        let mutated = mutate(&mut rng, valid.as_bytes());
        if let Ok(text) = std::str::from_utf8(&mutated)
            && let Ok(name) = ItemName::parse(text)
        {
            // 解析成功则必须自洽：重新拼出来的名字要能再解析
            let rebuilt = name.file_name();
            assert!(
                ItemName::parse(&rebuilt).is_ok(),
                "解析成功的名字应当能重新解析：{rebuilt}"
            );
        }
    }

    // 边界输入
    for bad in [
        "",
        "-",
        "--",
        "0",
        &"9".repeat(10_000),
        "日本語-あいうえお-かきくけこ.cxi",
        "1759323998000-ab12cd34-9f3c1e77",    // 没有扩展名
        "1759323998000-ab12cd34-9f3c1e77.ci", // 扩展名不完整
    ] {
        let _ = ItemName::parse(bad);
    }
}
