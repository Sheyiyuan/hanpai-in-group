//! 人物设定渲染：把 `[persona]` 转成给模型看的一段 system 块。
//!
//! 这一段是"它本来就是谁"，与从群里学来的表达方式分开：
//! persona 定骨架，表达学习补皮肉。留空的段落不会输出，避免空标签污染提示词。

use crate::config::Persona;

pub fn system_block(p: &Persona, fallback_name: &str) -> String {
    let name = if p.name.trim().is_empty() {
        fallback_name.trim()
    } else {
        p.name.trim()
    };

    let mut lines = Vec::new();
    lines.push(format!("你是{name}。"));

    push(&mut lines, "", &p.identity);
    push(&mut lines, "性格", &p.personality);
    push(&mut lines, "行为准则", &p.behavior);
    push(&mut lines, "说话风格", &p.speech);

    let taboos: Vec<&str> = p
        .taboos
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if !taboos.is_empty() {
        lines.push(format!("绝不：{}", taboos.join("；")));
    }

    lines.join("\n")
}

fn push(lines: &mut Vec<String>, label: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    if label.is_empty() {
        lines.push(value.to_string());
    } else {
        lines.push(format!("{label}：{value}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_persona_falls_back_to_nickname() {
        let block = system_block(&Persona::default(), "hanpai");
        assert_eq!(block, "你是hanpai。");
    }

    #[test]
    fn full_persona_has_all_sections() {
        let p = Persona {
            name: "阿排".into(),
            identity: "群里的老面孔，不是助手。".into(),
            personality: "话少，偶尔毒舌。".into(),
            behavior: "别人聊得起劲时不插嘴。".into(),
            speech: "短句为主。".into(),
            taboos: vec!["不替别人表态".into(), "不复读原话".into()],
        };
        let block = system_block(&p, "ignored");
        assert!(block.starts_with("你是阿排。"));
        assert!(block.contains("性格：话少，偶尔毒舌。"));
        assert!(block.contains("行为准则：别人聊得起劲时不插嘴。"));
        assert!(block.contains("说话风格：短句为主。"));
        assert!(block.contains("绝不：不替别人表态；不复读原话"));
        assert!(!block.contains("ignored"));
    }

    #[test]
    fn blank_sections_are_skipped() {
        let p = Persona {
            name: "".into(),
            identity: "".into(),
            personality: "  ".into(),
            behavior: "".into(),
            speech: "".into(),
            taboos: vec!["  ".into()],
        };
        let block = system_block(&p, "阿鱼");
        assert_eq!(block, "你是阿鱼。");
    }
}
