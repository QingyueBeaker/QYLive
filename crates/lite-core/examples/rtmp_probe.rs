//! RTMP 命令级探针（仅诊断用，不写入仓库数据）。
//!
//! 用途：分阶段发送 connect / releaseStream / FCPublish / createStream / publish，
//! 每一步之后检查服务端是否已经关闭连接（EOF），从而定位是哪条命令触发断开。
//!
//!   $env:LITE_URL="rtmp://host/app";  $env:LITE_KEY="...";  $env:LITE_STAGE="1..4"
//!
//! 不发送任何媒体帧。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use lite_core::config::RunConfig;
use lite_core::stream::amf::{self, Amf0};

const TIMEOUT: Duration = Duration::from_millis(300);

struct Raw {
    s: TcpStream,
    chunk: u32,
    closed: bool,
}

impl Raw {
    fn write_msg(&mut self, csid: u8, msg_type: u8, stream_id: u32, payload: &[u8]) -> bool {
        let mut buf = Vec::new();
        let mut first = true;
        let mut rest = payload;
        while !rest.is_empty() || first {
            let take = rest.len().min(self.chunk as usize);
            let (chunk, tail) = rest.split_at(take);
            if first {
                buf.push(0x00 | (csid & 0x3F));
                buf.extend_from_slice(&[0, 0, 0]); // timestamp 0
                buf.extend_from_slice(&((payload.len() as u32) & 0x00FF_FFFF).to_be_bytes()[1..]);
                buf.push(msg_type);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                first = false;
            } else {
                buf.push(0xC0 | (csid & 0x3F));
            }
            buf.extend_from_slice(chunk);
            rest = tail;
        }
        match self.s.write_all(&buf) {
            Ok(_) => true,
            Err(e) => {
                println!("  写入失败: {e}");
                self.closed = true;
                false
            }
        }
    }

    /// 读取服务端数据 800ms，返回 (收到的字节, 是否 EOF)
    fn read_for(&mut self, label: &str) -> (Vec<u8>, bool) {
        let mut all = Vec::new();
        self.s.set_read_timeout(Some(TIMEOUT)).ok();
        let mut eof = false;
        loop {
            let mut buf = vec![0u8; 4096];
            match self.s.read(&mut buf) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    all.extend_from_slice(&buf[..n]);
                    if all.len() > 65536 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        if eof {
            println!("[{label}] 服务端已关闭连接（EOF），累计收到 {} 字节", all.len());
        } else {
            let text = printable(&all);
            println!("[{label}] 连接存活，收到 {} 字节: {text}", all.len());
        }
        if !all.is_empty() && all.len() < 512 {
            println!("[{label}] hex: {}", all.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
        }
        (all, eof)
    }
}

fn printable(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.split(|b| *b < 0x20 || *b > 0x7e) {
        if chunk.len() >= 4 {
            out.push_str(&String::from_utf8_lossy(chunk));
            out.push('|');
        }
    }
    if out.len() > 300 {
        out.truncate(300);
    }
    out
}

fn main() {
    let url = std::env::var("LITE_URL").expect("缺少 LITE_URL");
    let key = std::env::var("LITE_KEY").expect("缺少 LITE_KEY");
    let stage: u8 = std::env::var("LITE_STAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(4);

    let (host, app) = RunConfig::split_url(&url).expect("地址解析失败");
    println!("目标 {host} app={app} stage={stage}");

    let s = TcpStream::connect(&host).expect("TCP 连接失败");
    s.set_nodelay(true).ok();
    let mut raw = Raw {
        s,
        chunk: 128,
        closed: false,
    };

    // 握手：C0+C1
    let mut c1 = vec![0u8; 1536];
    c1[0] = 0x00;
    for (i, b) in c1.iter_mut().enumerate().skip(8) {
        *b = (i as u8).wrapping_mul(7).wrapping_add(13);
    }
    let mut c0c1 = vec![3u8];
    c0c1.extend_from_slice(&c1);
    raw.s.write_all(&c0c1).expect("握手写失败");

    let mut s0s1s2 = vec![0u8; 1537];
    raw.s.read_exact(&mut s0s1s2).expect("握手读失败");
    let s1 = &s0s1s2[1..1537];
    // C2 必须是 S1 的完整拷贝（1536 字节，不含版本字节）
    raw.s.write_all(s1).expect("C2 写失败");
    println!("[握手] 完成");

    // SetChunkSize 4096
    raw.write_msg(2, 1, 0, &4096u32.to_be_bytes());
    raw.chunk = 4096;
    println!("[控制] 已发 SetChunkSize 4096");

    // stage >= 1：connect
    if stage >= 1 {
        let mut p = Vec::new();
        amf::encode(
            &[
                amf::s("connect"),
                amf::n(1.0),
                Amf0::Object(vec![
                    ("app".into(), amf::s(&app)),
                    ("type".into(), amf::s("nonprivate")),
                    ("flashVer".into(), amf::s("FMLE/3.0 (compatible; FMSc/1.0)")),
                    ("tcUrl".into(), amf::s(&format!("rtmp://{host}/{app}"))),
                    ("fpad".into(), Amf0::Boolean(false)),
                    ("capabilities".into(), amf::n(15.0)),
                    ("audioCodecs".into(), amf::n(4071.0)),
                    ("videoCodecs".into(), amf::n(252.0)),
                    ("videoFunction".into(), amf::n(1.0)),
                ]),
            ],
            &mut p,
        );
        raw.write_msg(3, 20, 0, &p);
        let (_, eof) = raw.read_for("connect");
        if eof {
            return;
        }
    }

    // stage >= 2：releaseStream + FCPublish + createStream
    if stage >= 2 {
        let mut p = Vec::new();
        amf::encode(&[amf::s("releaseStream"), amf::n(2.0), Amf0::Null, amf::s(&key)], &mut p);
        raw.write_msg(3, 20, 0, &p);
        raw.read_for("releaseStream");

        let mut p = Vec::new();
        amf::encode(&[amf::s("FCPublish"), amf::n(3.0), Amf0::Null, amf::s(&key)], &mut p);
        raw.write_msg(3, 20, 0, &p);
        raw.read_for("FCPublish");

        let mut p = Vec::new();
        amf::encode(&[amf::s("createStream"), amf::n(4.0), Amf0::Null], &mut p);
        raw.write_msg(3, 20, 0, &p);
        let (_, eof) = raw.read_for("createStream");
        if eof {
            return;
        }
    }

    // stage >= 3：publish
    if stage >= 3 {
        let mut p = Vec::new();
        amf::encode(
            &[amf::s("publish"), amf::n(5.0), Amf0::Null, amf::s(&key), amf::s("live")],
            &mut p,
        );
        raw.write_msg(3, 20, 1, &p);
        let (_, eof) = raw.read_for("publish");
        if eof {
            return;
        }
    }

    // stage >= 4：publish 后保持 6 秒空转，观察服务端是否因“无数据”断开
    if stage >= 4 {
        println!("[空转] publish 后不发任何媒体帧，观察 6 秒…");
        for i in 1..=6 {
            std::thread::sleep(Duration::from_secs(1));
            let (_, eof) = raw.read_for(&format!("空转 {i}s"));
            if eof {
                println!("[结论] publish 后无媒体数据时服务端会主动断开");
                return;
            }
        }
        println!("[结论] publish 后空转 6 秒连接仍在 → 服务端能接受无数据，问题在媒体帧格式");
    }
}
