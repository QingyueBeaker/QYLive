//! 最小 AMF0 编码器。仅实现 RTMP 命令消息所需的部分，不做解码。

pub enum Amf0 {
    Number(f64),
    Boolean(bool),
    String(String),
    Null,
    Object(Vec<(String, Amf0)>),
}

pub fn encode(values: &[Amf0], out: &mut Vec<u8>) {
    for v in values {
        encode_value(v, out);
    }
}

fn encode_value(v: &Amf0, out: &mut Vec<u8>) {
    match v {
        Amf0::Number(n) => {
            out.push(0x00);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Amf0::Boolean(b) => {
            out.push(0x01);
            out.push(if *b { 1 } else { 0 });
        }
        Amf0::String(s) => {
            out.push(0x02);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        Amf0::Null => out.push(0x05),
        Amf0::Object(props) => {
            out.push(0x03);
            for (k, val) in props {
                let kb = k.as_bytes();
                // 属性名通常为短字符串，超过 65535 的情况在这里直接截断保护
                let len = kb.len().min(65535) as u16;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(&kb[..len as usize]);
                encode_value(val, out);
            }
            out.extend_from_slice(&[0x00, 0x00, 0x09]);
        }
    }
}

/// 便捷构造：字符串数组。
pub fn s(v: &str) -> Amf0 {
    Amf0::String(v.to_string())
}

pub fn n(v: f64) -> Amf0 {
    Amf0::Number(v)
}
