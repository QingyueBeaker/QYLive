//! 自研轻量 RTMP 客户端（纯 Rust）。
//!
//! 只实现推流所需的最小子集：简单握手、connect / createStream / publish、
//! SetChunkSize、Acknowledgement、FLV tag 写入。
//! 不解析服务端业务消息，只做错误关键字扫描与字节计数（保持最小实现）。
//!
//! 合规说明：本模块只使用公开的 RTMP 1.0 规范，不含任何平台私有接口调用。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::stats::Stats;
use crate::stream::amf::{self, Amf0};
use crate::stream::flv::{FlvTag, TAG_AUDIO, TAG_DATA, TAG_VIDEO};

/// 出站 chunk 大小。4096 过大时部分直播服务端（含 B 站链路）会在 Publish.Start 后直接断流，
/// 取 1360 是各服务端实测兼容的值。
const OUT_CHUNK_SIZE: u32 = 1360;
const CSID_CONTROL: u8 = 2;
const CSID_COMMAND: u8 = 3;
const CSID_AUDIO: u8 = 4;
const CSID_DATA: u8 = 5;
const CSID_VIDEO: u8 = 6;

pub struct RtmpConnection {
    stream: TcpStream,
    chunk_size: u32,
    stream_id: u32,
    stats: Stats,
    /// 累计从服务端读到的字节（用于发送 Acknowledgement，防止服务端限流）
    read_bytes: Arc<AtomicU64>,
    acked: u64,
}

impl RtmpConnection {
    pub async fn connect(host_port: &str, app: &str, key: &str, stats: &Stats) -> Result<Self, String> {
        let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(host_port))
            .await
            .map_err(|_| "连接 RTMP 服务器超时".to_string())?
            .map_err(|e| format!("连接 RTMP 服务器失败: {e}"))?;
        let _ = stream.set_nodelay(true);
        let read_bytes = Arc::new(AtomicU64::new(0));
        let mut conn = Self {
            stream,
            chunk_size: 128,
            stream_id: 0,
            stats: stats.clone(),
            read_bytes,
            acked: 0,
        };
        conn.handshake().await?;
        conn.send_set_chunk_size().await?;
        conn.command_connect(app).await?;
        // FMLE 兼容序列：多数国内直播服务端（含 B 站链路）要求 releaseStream + FCPublish，
        // 缺失时会出现“Publish.Start 之后立刻被服务端断开”的现象。
        conn.command_release_stream(key).await?;
        conn.command_fc_publish(key).await?;
        conn.command_create_stream().await?;
        conn.command_publish(key).await?;
        Ok(conn)
    }

    async fn handshake(&mut self) -> Result<(), String> {
        let mut c1 = vec![0u8; 1536];
        // C1: 4 字节时间戳 + 4 字节零 + 随机填充
        c1[0..4].fill(0);
        c1[4..8].fill(0);
        fill_random(&mut c1[8..]);
        self.stream
            .write_all(&[0x03])
            .await
            .map_err(|e| format!("握手写入失败: {e}"))?;
        self.stream
            .write_all(&c1)
            .await
            .map_err(|e| format!("握手写入失败: {e}"))?;

        let mut s0 = [0u8; 1];
        self.stream
            .read_exact(&mut s0)
            .await
            .map_err(|e| format!("握手读取失败: {e}"))?;
        if s0[0] != 0x03 {
            return Err("服务端 RTMP 版本不支持（非明文 RTMP）".to_string());
        }
        let mut s1 = vec![0u8; 1536];
        self.stream
            .read_exact(&mut s1)
            .await
            .map_err(|e| format!("握手读取失败: {e}"))?;
        let mut s2 = vec![0u8; 1536];
        self.stream
            .read_exact(&mut s2)
            .await
            .map_err(|e| format!("握手读取失败: {e}"))?;
        let _ = s2;
        // C2 回显 S1
        self.stream
            .write_all(&s1)
            .await
            .map_err(|e| format!("握手写入失败: {e}"))?;
        Ok(())
    }

    async fn send_set_chunk_size(&mut self) -> Result<(), String> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&OUT_CHUNK_SIZE.to_be_bytes());
        self.write_message(CSID_CONTROL, 1, 0, 0, &payload)
            .await?;
        self.chunk_size = OUT_CHUNK_SIZE;
        Ok(())
    }

    async fn command_connect(&mut self, app: &str) -> Result<(), String> {
        let tc_url = self.tc_url_hint(app);
        let mut payload = Vec::new();
        amf::encode(
            &[
                amf::s("connect"),
                amf::n(1.0),
                Amf0::Object(vec![
                    ("app".into(), amf::s(app)),
                    ("type".into(), amf::s("nonprivate")),
                    (
                        "flashVer".into(),
                        amf::s("FMLE/3.0 (compatible; FMSc/1.0)"),
                    ),
                    ("tcUrl".into(), amf::s(&tc_url)),
                    ("fpad".into(), Amf0::Boolean(false)),
                    ("capabilities".into(), amf::n(15.0)),
                    ("audioCodecs".into(), amf::n(4071.0)),
                    ("videoCodecs".into(), amf::n(252.0)),
                    ("videoFunction".into(), amf::n(1.0)),
                ]),
            ],
            &mut payload,
        );
        self.write_message(CSID_COMMAND, 20, 0, 0, &payload).await?;
        self.drain_for(Duration::from_millis(220)).await;
        Ok(())
    }

    async fn command_release_stream(&mut self, key: &str) -> Result<(), String> {
        let mut payload = Vec::new();
        amf::encode(
            &[amf::s("releaseStream"), amf::n(2.0), Amf0::Null, amf::s(key)],
            &mut payload,
        );
        self.write_message(CSID_COMMAND, 20, 0, 0, &payload).await?;
        self.drain_for(Duration::from_millis(90)).await;
        Ok(())
    }

    async fn command_fc_publish(&mut self, key: &str) -> Result<(), String> {
        let mut payload = Vec::new();
        amf::encode(
            &[amf::s("FCPublish"), amf::n(3.0), Amf0::Null, amf::s(key)],
            &mut payload,
        );
        self.write_message(CSID_COMMAND, 20, 0, 0, &payload).await?;
        self.drain_for(Duration::from_millis(90)).await;
        Ok(())
    }

    async fn command_create_stream(&mut self) -> Result<(), String> {
        let mut payload = Vec::new();
        amf::encode(&[amf::s("createStream"), amf::n(4.0), Amf0::Null], &mut payload);
        self.write_message(CSID_COMMAND, 20, 0, 0, &payload).await?;
        self.drain_for(Duration::from_millis(150)).await;
        self.stream_id = 1;
        Ok(())
    }

    async fn command_publish(&mut self, key: &str) -> Result<(), String> {
        let mut payload = Vec::new();
        amf::encode(
            &[amf::s("publish"), amf::n(5.0), Amf0::Null, amf::s(key), amf::s("live")],
            &mut payload,
        );
        self.write_message(CSID_COMMAND, 20, self.stream_id, 0, &payload)
            .await?;
        self.drain_for(Duration::from_millis(260)).await;
        Ok(())
    }

    fn tc_url_hint(&self, app: &str) -> String {
        // 仅用于 connect 的 tcUrl 字段，不影响实际连接
        format!("rtmp://{}/{}", self.stream.peer_addr().map(|a| a.to_string()).unwrap_or_default(), app)
    }

    /// 写一条 RTMP message（消息体内部按 chunk 切分，fmt0 头 + 后续 fmt3 续块）。
    async fn write_message(
        &mut self,
        csid: u8,
        msg_type: u8,
        msg_stream_id: u32,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), String> {
        let mut buf: Vec<u8> = Vec::with_capacity(payload.len() + 16);
        let mut first = true;
        let mut rest = payload;
        loop {
            let take = rest.len().min(self.chunk_size as usize);
            let (chunk, tail) = rest.split_at(take);
            if first {
                buf.push(0x00 | (csid & 0x3F));
                buf.extend_from_slice(&timestamp.to_be_bytes()[1..]);
                buf.extend_from_slice(&((payload.len() as u32) & 0x00FF_FFFF).to_be_bytes()[1..]);
                buf.push(msg_type);
                buf.extend_from_slice(&msg_stream_id.to_le_bytes());
                first = false;
            } else {
                buf.push(0xC0 | (csid & 0x3F));
            }
            buf.extend_from_slice(chunk);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        self.stream
            .write_all(&buf)
            .await
            .map_err(|e| format!("推流写入失败: {e}"))?;
        self.stats.add_bytes_sent(buf.len() as u64);
        Ok(())
    }

    /// 非阻塞地读走服务端数据，统计字节数并扫描错误关键字。
    async fn drain_for(&mut self, dur: Duration) {
        let deadline = tokio::time::Instant::now() + dur;
        let mut buf = vec![0u8; 4096];
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, self.stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    self.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    self.trace_server(&buf[..n]);
                    if looks_like_error(&buf[..n]) {
                        log::warn!("RTMP 服务端拒绝推流（onStatus 报错），将在 2 秒后重连");
                    }
                }
                Ok(Ok(_)) => {
                    log::warn!("RTMP 服务端关闭了连接（EOF）");
                    break;
                }
                Ok(Err(e)) => {
                    log::warn!("RTMP 读取错误：{e}");
                    break;
                }
                Err(_) => break,
            }
        }
    }

    /// 把服务端回包里可读的字符串片段抽出来打日志，便于定位拒绝原因。
    fn trace_server(&self, data: &[u8]) {
        let interesting = [
            b"onStatus".as_slice(),
            b"_result".as_slice(),
            b"onBWDone".as_slice(),
            b"NetStream".as_slice(),
        ];
        if !interesting.iter().any(|n| contains(data, n)) {
            return;
        }
        let mut text = String::new();
        for chunk in data.split(|b| *b < 0x20 || *b > 0x7e) {
            if chunk.len() >= 4 {
                text.push_str(&String::from_utf8_lossy(chunk));
                text.push('|');
            }
        }
        log::info!("RTMP 服务端消息: {text}");
    }

    async fn send_ack_if_needed(&mut self) -> Result<(), String> {        let total = self.read_bytes.load(Ordering::Relaxed);
        if total.saturating_sub(self.acked) >= 2_000_000 {
            let payload = (total as u32).to_be_bytes().to_vec();
            self.write_message(CSID_CONTROL, 3, 0, 0, &payload).await?;
            self.acked = total;
        }
        Ok(())
    }

    pub async fn write_tag(&mut self, tag: &FlvTag, ts: u32) -> Result<(), String> {
        let csid = match tag.tag_type {
            TAG_AUDIO => CSID_AUDIO,
            TAG_VIDEO => CSID_VIDEO,
            TAG_DATA => CSID_DATA,
            _ => CSID_DATA,
        };
        self.write_message(csid, tag.tag_type, self.stream_id, ts, &tag.payload)
            .await
    }
}

fn looks_like_error(buf: &[u8]) -> bool {
    // 服务端拒绝推流时会回 onStatus，其中 level=error。这里做保守的关键字检查。
    let needles = [b"BadName".as_slice(), b"NetStream.Publish.Rejected".as_slice()];
    needles.iter().any(|n| contains(buf, n))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn fill_random(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    let mut x = seed | 1;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xFF) as u8;
    }
}

/// 推流主循环：持续消费 mpsc 中的 FLV tag，写入 RTMP 连接。
/// 只有网络层真正失败时才重连（重连不会重建编码器、采集与帧调度，因此不会重开播）。
pub async fn publish_loop(
    host_port: String,
    app: String,
    key: String,
    mut rx: UnboundedReceiver<FlvTag>,
    stats: Stats,
    events: tokio::sync::mpsc::UnboundedSender<crate::events::EngineEvent>,
) -> Result<(), String> {
    // 序列头缓存：网络断开重连后必须先补发，否则解码器无法起播
    let mut video_sh: Option<FlvTag> = None;
    let mut audio_sh: Option<FlvTag> = None;
    let mut first_attempt = true;
    // 重连退避：连续失败时 2/4/8/15 秒递增，避免秒级硬连触发平台风控
    let mut fail_streak: u32 = 0;

    let emit = |state: &str, detail: String| {
        let _ = events.send(crate::events::EngineEvent::RtmpStatus {
            state: state.to_string(),
            detail,
        });
    };

    loop {
        if !first_attempt {
            stats.inc_reconnect();
            let delay = (2u64 << fail_streak.min(3)).min(15);
            log::warn!("RTMP 连接断开，{delay} 秒后重连（编码/采集/调度不重启）");
            emit("retrying", format!("连接已断开，{delay} 秒后重连"));
            tokio::time::sleep(Duration::from_secs(delay)).await;
            if rx.is_closed() {
                emit("stopped", "推流已停止".into());
                return Ok(());
            }
        }
        first_attempt = false;
        fail_streak = fail_streak.saturating_add(1);

        emit("connecting", format!("正在连接 {host_port}/{app}"));

        // 首次连接失败不能让整个推流任务静默结束（否则 UI 还显示“推流中”）
        let mut conn = match RtmpConnection::connect(&host_port, &app, &key, &stats).await {
            Ok(c) => c,
            Err(e) => {
                log::warn!("RTMP 连接失败：{e}");
                emit("failed", e);
                continue;
            }
        };
        log::info!("RTMP 已建立并发布：{host_port}/{app}");
        emit("publishing", "服务端已接受推流（Publish.Start）".into());

        if let Some(sh) = &video_sh {
            if let Err(e) = conn.write_tag(sh, 0).await {
                log::warn!("补发视频序列头失败：{e}");
            }
        }
        if let Some(sh) = &audio_sh {
            if let Err(e) = conn.write_tag(sh, 0).await {
                log::warn!("补发音频序列头失败：{e}");
            }
        }

        let mut ack_tick = tokio::time::interval(Duration::from_secs(1));
        ack_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                tag = rx.recv() => {
                    match tag {
                        Some(tag) => {
                            if tag.tag_type == TAG_VIDEO && tag.payload.len() >= 2 && tag.payload[1] == 0x00 {
                                video_sh = Some(FlvTag { tag_type: TAG_VIDEO, ts_ms: 0, payload: tag.payload.clone() });
                            } else if tag.tag_type == TAG_AUDIO && tag.payload.len() >= 2 && tag.payload[1] == 0x00 {
                                audio_sh = Some(FlvTag { tag_type: TAG_AUDIO, ts_ms: 0, payload: tag.payload.clone() });
                            }
                            if let Err(e) = conn.write_tag(&tag, tag.ts_ms).await {
                                log::warn!("{e}");
                                emit("retrying", format!("写入失败：{e}"));
                                break; // 跳出内层，进入重连
                            }
                            fail_streak = 0; // 真正发出数据了，重置退避
                            let _ = conn.send_ack_if_needed().await;
                        }
                        None => {
                            log::info!("推流通道已关闭，结束发送");
                            emit("stopped", "推流已停止".into());
                            return Ok(());
                        }
                    }
                }
                _ = ack_tick.tick() => {
                    conn.drain_for(Duration::from_millis(2)).await;
                    let _ = conn.send_ack_if_needed().await;
                }
            }
        }
    }
}
