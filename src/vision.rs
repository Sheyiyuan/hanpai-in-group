//! 图片理解与表情识别。
//!
//! 图片只在入库时被「看一眼」：调用多模态模型转述，然后把文本写进消息，
//! 原始图片不落库、不下载（除非提供商不吃外链，那时才会瞬时下载到内存）。
//! 被判定为可复用的表情包是唯一会被存下来的东西。

use std::sync::Arc;

use tracing::warn;

use crate::llm::{Caption, ImagePart, Msg, Role};
use crate::onebot::event::Segment;
use crate::state::AppState;

/// 让模型描述一张图。失败返回 None，调用方退回 `[图片]` 占位。
pub async fn describe(state: &Arc<AppState>, url: &str) -> Option<Caption> {
    let cfg = state.cfg();
    if !cfg.vision.enabled {
        return None;
    }
    let llm = state.llm()?;
    let prompts = state.prompts();

    let messages = vec![
        Msg::system(prompts.caption.clone()),
        Msg::user("看看这张图").with_image(ImagePart::from_url(url)),
    ];

    match llm.chat_json(Role::Caption, &messages).await {
        Ok(value) => {
            let mut caption: Caption = serde_json::from_value(value).unwrap_or_default();
            caption.caption = caption.caption.trim().to_string();
            caption.ocr = caption.ocr.trim().to_string();
            caption.emotion = caption.emotion.trim().to_string();
            if caption.caption.is_empty() {
                return None;
            }
            let limit = cfg.vision.caption_max_chars.max(8) as usize;
            if caption.caption.chars().count() > limit {
                caption.caption = caption.caption.chars().take(limit).collect();
            }
            if caption.ocr.chars().count() > limit {
                caption.ocr = caption.ocr.chars().take(limit).collect();
            }
            Some(caption)
        }
        Err(e) => {
            warn!("图片转述失败: {e:#}");
            None
        }
    }
}

/// 把图片描述拼成落库用的文本。
pub fn render(caption: &Caption) -> String {
    if caption.ocr.is_empty() {
        format!("[图片:{}]", caption.caption)
    } else {
        format!("[图片:{} / 文字:{}]", caption.caption, caption.ocr)
    }
}

/// 是不是表情包（自定义表情 / 商城表情），而不是普通照片。
pub fn is_sticker(segment: &Segment) -> bool {
    if segment.kind == "mface" {
        return true;
    }
    if let Some(sub) = segment.data.get("sub_type").and_then(|v| v.as_i64()) {
        if sub == 1 {
            return true;
        }
    }
    segment.data.get("emoji_id").is_some() || segment.data.get("emoji_package_id").is_some()
}

/// QQ 系统表情 id → 名字。内置常用的一批，查不到就显示 id。
pub fn face_name(id: &str) -> Option<&'static str> {
    let name = match id {
        "0" => "惊讶",
        "1" => "撇嘴",
        "2" => "色",
        "3" => "得意",
        "4" => "流泪",
        "5" => "害羞",
        "6" => "闭嘴",
        "7" => "睡",
        "8" => "大哭",
        "9" => "尴尬",
        "10" => "发怒",
        "11" => "调皮",
        "12" => "呲牙",
        "13" => "微笑",
        "14" => "难过",
        "15" => "酷",
        "16" => "冷汗",
        "17" => "抓狂",
        "18" => "吐",
        "19" => "偷笑",
        "20" => "可爱",
        "21" => "白眼",
        "22" => "傲慢",
        "23" => "饥饿",
        "24" => "困",
        "25" => "惊恐",
        "26" => "流汗",
        "27" => "憨笑",
        "28" => "大兵",
        "29" => "奋斗",
        "30" => "咒骂",
        "31" => "疑问",
        "32" => "嘘",
        "33" => "晕",
        "34" => "折磨",
        "35" => "衰",
        "36" => "骷髅",
        "37" => "敲打",
        "38" => "再见",
        "39" => "擦汗",
        "40" => "抠鼻",
        "41" => "鼓掌",
        "42" => "糗大了",
        "43" => "坏笑",
        "44" => "左哼哼",
        "45" => "右哼哼",
        "46" => "哈欠",
        "47" => "鄙视",
        "48" => "委屈",
        "49" => "快哭了",
        "50" => "吓",
        "51" => "可怜",
        "53" => "西瓜",
        "54" => "啤酒",
        "55" => "篮球",
        "56" => "乒乓",
        "57" => "咖啡",
        "58" => "饭",
        "59" => "猪头",
        "60" => "玫瑰",
        "63" => "爱心",
        "64" => "心碎",
        "66" => "蛋糕",
        "67" => "闪电",
        "68" => "炸弹",
        "69" => "刀",
        "74" => "太阳",
        "75" => "礼物",
        "76" => "抱拳",
        "77" => "委屈",
        "78" => "握手",
        "79" => "胜利",
        "80" => "抱抱",
        "81" => "怒",
        "82" => "调皮",
        "83" => "礼物",
        "84" => "红包",
        "85" => "發",
        "86" => "恭喜",
        "87" => "鞭炮",
        "88" => "灯笼",
        "96" => "冷汗",
        "97" => "擦汗",
        "98" => "抠鼻",
        "99" => "鼓掌",
        "100" => "糗大了",
        "101" => "坏笑",
        "102" => "左哼哼",
        "103" => "右哼哼",
        "104" => "哈欠",
        "105" => "鄙视",
        "106" => "委屈",
        "107" => "快哭了",
        "108" => "阴险",
        "109" => "亲亲",
        "110" => "吓",
        "111" => "可怜",
        "112" => "菜刀",
        "113" => "西瓜",
        "114" => "啤酒",
        "115" => "篮球",
        "116" => "乒乓",
        "117" => "咖啡",
        "118" => "饭",
        "119" => "猪头",
        "120" => "玫瑰",
        "121" => "凋谢",
        "122" => "示爱",
        "123" => "爱心",
        "124" => "心碎",
        "125" => "蛋糕",
        "126" => "闪电",
        "127" => "炸弹",
        "128" => "刀",
        "129" => "足球",
        "130" => "瓢虫",
        "131" => "便便",
        "132" => "月亮",
        "133" => "太阳",
        "134" => "礼物",
        "135" => "抱拳",
        "136" => "握手",
        "137" => "胜利",
        "138" => "抱抱",
        "139" => "强",
        "140" => "弱",
        "141" => "握手",
        "142" => "胜利",
        "143" => "抱拳",
        "144" => "勾引",
        "145" => "拳头",
        "146" => "差劲",
        "147" => "爱你",
        "148" => "NO",
        "149" => "OK",
        "150" => "爱情",
        "151" => "飞吻",
        "152" => "跳跳",
        "153" => "发抖",
        "154" => "怄火",
        "155" => "转圈",
        "156" => "磕头",
        "157" => "回头",
        "158" => "跳绳",
        "159" => "挥手",
        "160" => "激动",
        "161" => "街舞",
        "162" => "献吻",
        "163" => "左太极",
        "164" => "右太极",
        "165" => "双喜",
        "166" => "鞭炮",
        "167" => "灯笼",
        "168" => "K歌",
        "169" => "购物",
        "170" => "邮件",
        "171" => "帅",
        "172" => "喝彩",
        "173" => "祈祷",
        "174" => "爆筋",
        "175" => "棒棒糖",
        "176" => "喝奶",
        "177" => "下面",
        "178" => "香蕉",
        "179" => "飞机",
        "180" => "开车",
        "181" => "高铁",
        "182" => "左车",
        "183" => "右车",
        "184" => "多云",
        "185" => "下雨",
        "186" => "雪花",
        "187" => "雨伞",
        "188" => "彩虹",
        "189" => "求关注",
        "190" => "抱抱",
        "191" => "赞",
        "192" => "踩",
        "193" => "礼物",
        "194" => "握手",
        "195" => "咖啡",
        _ => return None,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_faces() {
        assert_eq!(face_name("14"), Some("难过"));
        assert_eq!(face_name("13"), Some("微笑"));
        assert_eq!(face_name("9999"), None);
    }

    #[test]
    fn sticker_detection() {
        let seg: Segment = serde_json::from_str(r#"{"type":"mface","data":{}}"#).unwrap();
        assert!(is_sticker(&seg));

        let photo: Segment =
            serde_json::from_str(r#"{"type":"image","data":{"file":"a.jpg","sub_type":0}}"#)
                .unwrap();
        assert!(!is_sticker(&photo));

        let sub: Segment =
            serde_json::from_str(r#"{"type":"image","data":{"file":"a.jpg","sub_type":1}}"#)
                .unwrap();
        assert!(is_sticker(&sub));
    }

    #[test]
    fn render_with_and_without_ocr() {
        let mut c = Caption {
            caption: "一只橘猫".into(),
            ..Default::default()
        };
        assert_eq!(render(&c), "[图片:一只橘猫]");
        c.ocr = "TypeError".into();
        assert_eq!(render(&c), "[图片:一只橘猫 / 文字:TypeError]");
    }
}
