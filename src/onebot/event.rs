//! OneBot v11 事件结构。只解析用得上的字段，其余忽略。
//!
//! 两个容易踩的点，在这里一次性抹平：
//! 1. `message` 可能是字符串（CQ 码）也可能是消息段数组，取决于协议端的
//!    `message_format`；NapCat 默认 array，但 OneBot 规范默认 string。
//! 2. `message_id` / `at.qq` 在不同实现里可能是字符串或数字。

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    pub post_type: Option<String>,
    pub message_type: Option<String>,
    pub sub_type: Option<String>,
    pub meta_event_type: Option<String>,
    pub notice_type: Option<String>,
    pub request_type: Option<String>,
    pub time: Option<i64>,
    pub self_id: Option<i64>,
    pub group_id: Option<i64>,
    pub user_id: Option<i64>,
    pub message_id: Option<i64>,
    pub raw_message: Option<String>,
    /// 统一成消息段数组：字符串格式在这里就地解析成段。
    #[serde(default, deserialize_with = "deserialize_message")]
    pub message: Option<Vec<Segment>>,
    pub sender: Option<Sender>,
    pub target_id: Option<i64>,
    pub operator_id: Option<i64>,
    /// 心跳元事件：到下次心跳的间隔（毫秒）
    pub interval: Option<i64>,
    /// 心跳元事件：status 对象（online / good）
    pub status: Option<Value>,
}

impl Incoming {
    /// 取消息段；`message` 缺失时用 `raw_message`（CQ 码字符串）兜底。
    pub fn segments(&self) -> Vec<Segment> {
        match self.message.as_ref() {
            Some(v) if !v.is_empty() => v.clone(),
            _ => parse_cq(self.raw_message.as_deref().unwrap_or("")),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Segment {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Sender {
    pub user_id: Option<i64>,
    pub nickname: Option<String>,
    pub card: Option<String>,
}

impl Sender {
    pub fn display_name(&self) -> String {
        match (self.card.as_deref(), self.nickname.as_deref()) {
            (Some(c), _) if !c.trim().is_empty() => c.trim().to_string(),
            (_, Some(n)) => n.to_string(),
            _ => String::new(),
        }
    }
}

/// 自定义反序列化：字符串（CQ 码）→ 消息段；数组 → 原样。
fn deserialize_message<'de, D>(deserializer: D) -> Result<Option<Vec<Segment>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;

    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(parse_cq(&s))),
        Value::Array(_) => serde_json::from_value::<Vec<Segment>>(value)
            .map(Some)
            .map_err(D::Error::custom),
        // 规范只允许事件里是字符串/数组；单个对象主要出现在 API 参数里，宽容处理。
        Value::Object(_) => serde_json::from_value::<Segment>(value)
            .map(|seg| Some(vec![seg]))
            .map_err(D::Error::custom),
        other => Err(D::Error::custom(format!("message 字段类型不支持: {other}"))),
    }
}

/// 把 CQ 码字符串解析成消息段。文本里的 `&#91;` 等转义会还原。
pub fn parse_cq(raw: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(pos) = rest.find("[CQ:") {
        let (before, tail) = rest.split_at(pos);
        push_text(&mut out, before);

        let after = &tail[4..];
        let Some(end) = after.find(']') else {
            // 没闭合，剩下整段当普通文本
            push_text(&mut out, tail);
            return out;
        };
        let body = &after[..end];
        rest = &after[end + 1..];

        let mut parts = body.split(',');
        let kind = parts.next().unwrap_or("").trim();
        if kind.is_empty() {
            continue;
        }
        let mut data = serde_json::Map::new();
        for part in parts {
            if let Some((k, v)) = part.split_once('=') {
                data.insert(k.trim().to_string(), Value::String(unescape_cq(v)));
            }
        }
        out.push(Segment {
            kind: kind.to_string(),
            data: Value::Object(data),
        });
    }
    push_text(&mut out, rest);
    out
}

fn push_text(out: &mut Vec<Segment>, text: &str) {
    if text.is_empty() {
        return;
    }
    out.push(Segment {
        kind: "text".into(),
        data: json!({ "text": unescape_cq(text) }),
    });
}

fn unescape_cq(s: &str) -> String {
    // &amp; 必须最后替换，否则会把 &#91; 之类拆坏
    s.replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

/// 把消息段拼成人类可读的纯文本。图片/语音等只留占位符，P0 不做识图。
pub fn render_text(segments: Option<&Vec<Segment>>, raw: &str) -> String {
    let Some(segments) = segments else {
        return raw.to_string();
    };
    if segments.is_empty() {
        return raw.to_string();
    }
    let mut out = String::new();
    for seg in segments {
        match seg.kind.as_str() {
            "text" => {
                if let Some(t) = seg.data.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
            "at" => {
                let qq = match seg.data.get("qq") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                out.push('@');
                out.push_str(&qq);
            }
            "reply" => {}
            "image" => out.push_str("[图片]"),
            "face" | "mface" => out.push_str("[表情]"),
            "record" => out.push_str("[语音]"),
            "video" => out.push_str("[视频]"),
            "file" => out.push_str("[文件]"),
            "forward" | "node" => out.push_str("[合并转发]"),
            "json" | "xml" => out.push_str("[卡片]"),
            "share" => out.push_str("[链接]"),
            "music" => out.push_str("[音乐]"),
            "location" => out.push_str("[位置]"),
            "contact" => out.push_str("[名片]"),
            "dice" => out.push_str("[骰子]"),
            "rps" => out.push_str("[猜拳]"),
            "poke" | "shake" => out.push_str("[戳一戳]"),
            // 未知段型（新版本协议端扩展）直接忽略，不往记忆里灌垃圾
            _ => {}
        }
    }
    out.trim().to_string()
}

/// 从消息段里取被回复的消息 id
pub fn reply_target(segments: Option<&Vec<Segment>>) -> Option<i64> {
    segments?
        .iter()
        .find(|s| s.kind == "reply")
        .and_then(|s| s.data.get("id"))
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| v.as_i64())
        })
}

/// 是否 @ 了指定 QQ。按消息段判等，避免 `@10001` 命中 `@100010`。
/// `qq` 兼容字符串和数字两种实现。
pub fn mentions(segments: &[Segment], qq: i64) -> bool {
    if qq <= 0 {
        return false;
    }
    segments.iter().any(|seg| {
        if seg.kind != "at" {
            return false;
        }
        match seg.data.get("qq") {
            Some(Value::String(s)) => s.trim().parse::<i64>().ok() == Some(qq),
            Some(Value::Number(n)) => n.as_i64() == Some(qq),
            _ => false,
        }
    })
}

/// 被叫到的方式：@ > 昵称/别名出现在正文里。
/// 引用回复需要查库确认被引的是不是自己，由调用方补充（见 `ws`）。
pub fn detect_trigger(
    segments: &[Segment],
    text: &str,
    self_id: i64,
    aliases: &[String],
) -> Option<&'static str> {
    if mentions(segments, self_id) {
        return Some("at");
    }
    if aliases
        .iter()
        .any(|a| !a.is_empty() && text.contains(a.as_str()))
    {
        return Some("named");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_mixed_segments() {
        let segs: Vec<Segment> = serde_json::from_str(
            r#"[{"type":"text","data":{"text":"你好 "}},{"type":"at","data":{"qq":"123"}},{"type":"image","data":{"file":"a.jpg"}}]"#,
        )
        .unwrap();
        assert_eq!(render_text(Some(&segs), ""), "你好 @123[图片]");
    }

    #[test]
    fn render_falls_back_to_raw() {
        assert_eq!(render_text(None, "原始消息"), "原始消息");
    }

    #[test]
    fn reply_target_parsed() {
        let segs: Vec<Segment> =
            serde_json::from_str(r#"[{"type":"reply","data":{"id":"777"}}]"#).unwrap();
        assert_eq!(reply_target(Some(&segs)), Some(777));
    }

    #[test]
    fn cq_string_is_parsed() {
        let raw = "你好 [CQ:at,qq=10001] [CQ:reply,id=777] [CQ:image,file=a.jpg]";
        let segs = parse_cq(raw);
        assert!(mentions(&segs, 10001));
        assert_eq!(reply_target(Some(&segs)), Some(777));
        assert_eq!(render_text(Some(&segs), raw), "你好 @10001  [图片]");
    }

    #[test]
    fn cq_escape_unescaped() {
        let segs = parse_cq("&#91;不是CQ&#93; &#44; &amp;");
        assert_eq!(render_text(Some(&segs), ""), "[不是CQ] , &");
    }

    #[test]
    fn string_message_event_deserializes() {
        let ev: Incoming = serde_json::from_str(
            r#"{"post_type":"message","message_type":"group","group_id":1,"user_id":2,
                "self_id":10001,"message_id":3,"raw_message":"hi [CQ:at,qq=10001]",
                "message":"hi [CQ:at,qq=10001]","sender":{"nickname":"甲"}}"#,
        )
        .unwrap();
        let segs = ev.segments();
        assert!(mentions(&segs, 10001));
        assert_eq!(render_text(Some(&segs), ""), "hi @10001");
    }

    #[test]
    fn array_message_event_deserializes() {
        let ev: Incoming = serde_json::from_str(
            r#"{"post_type":"message","message_type":"group","group_id":1,"user_id":2,
                "self_id":10001,"message_id":3,"raw_message":"",
                "message":[{"type":"at","data":{"qq":10001}}],"sender":{"nickname":"甲"}}"#,
        )
        .unwrap();
        assert!(mentions(ev.message.as_ref().unwrap(), 10001));
    }

    #[test]
    fn at_matching_is_exact() {
        let segs = parse_cq("[CQ:at,qq=100010]");
        assert!(!mentions(&segs, 10001));
        assert_eq!(render_text(Some(&segs), ""), "@100010");
    }

    #[test]
    fn numeric_at_qq_mentions() {
        let segs: Vec<Segment> =
            serde_json::from_str(r#"[{"type":"at","data":{"qq":10001}}]"#).unwrap();
        assert!(mentions(&segs, 10001));
    }

    #[test]
    fn at_all_is_not_me() {
        let segs = parse_cq("[CQ:at,qq=all]");
        assert!(!mentions(&segs, 10001));
        assert_eq!(render_text(Some(&segs), ""), "@all");
    }

    #[test]
    fn trigger_prefers_at_over_alias() {
        let segs = parse_cq("[CQ:at,qq=10001] 半拍");
        assert_eq!(
            detect_trigger(&segs, "半拍", 10001, &["半拍".to_string()]),
            Some("at")
        );
    }

    #[test]
    fn trigger_alias_and_none() {
        let segs = parse_cq("今天半拍来了吗");
        assert_eq!(
            detect_trigger(&segs, "今天半拍来了吗", 10001, &["半拍".to_string()]),
            Some("named")
        );
        assert_eq!(detect_trigger(&segs, "今天天气不错", 10001, &[]), None);
    }

    #[test]
    fn unknown_segments_are_ignored() {
        let segs = parse_cq("前 [CQ:foo,bar=1] 后");
        assert_eq!(render_text(Some(&segs), ""), "前  后");
    }
}
