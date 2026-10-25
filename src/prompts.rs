//! 提示词外置：`prompts/` 目录里有同名文件就用文件，没有就用编译进来的默认值。
//! 改提示词不需要重新编译，`/reload` 即可生效。

use std::path::Path;

pub const DEFAULT_EXTRACT: &str = include_str!("../prompts/extract.md");
pub const DEFAULT_REPLY: &str = include_str!("../prompts/reply.md");
pub const DEFAULT_PLANNER: &str = include_str!("../prompts/planner.md");
pub const DEFAULT_CAPTION: &str = include_str!("../prompts/caption.md");

#[derive(Debug, Clone)]
pub struct PromptSet {
    pub extract: String,
    pub reply: String,
    pub planner: String,
    pub caption: String,
}

impl Default for PromptSet {
    fn default() -> Self {
        Self {
            extract: DEFAULT_EXTRACT.to_string(),
            reply: DEFAULT_REPLY.to_string(),
            planner: DEFAULT_PLANNER.to_string(),
            caption: DEFAULT_CAPTION.to_string(),
        }
    }
}

impl PromptSet {
    pub fn load(dir: &str) -> Self {
        let dir = Path::new(dir);
        let mut set = Self::default();
        if let Some(text) = read_optional(&dir.join("extract.md")) {
            set.extract = text;
        }
        if let Some(text) = read_optional(&dir.join("reply.md")) {
            set.reply = text;
        }
        if let Some(text) = read_optional(&dir.join("planner.md")) {
            set.planner = text;
        }
        if let Some(text) = read_optional(&dir.join("caption.md")) {
            set.caption = text;
        }
        set
    }

    /// 把 {name} 占位符换成角色名
    pub fn reply_for(&self, name: &str) -> String {
        self.reply.replace("{name}", name)
    }

    pub fn planner_for(&self, name: &str) -> String {
        self.planner.replace("{name}", name)
    }
}

fn read_optional(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(t) if !t.trim().is_empty() => Some(t),
        Ok(_) => None,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("读取提示词 {} 失败: {e}", path.display());
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_built_in() {
        let set = PromptSet::load("/definitely/not/a/dir");
        assert!(set.extract.contains("JSON"));
        assert!(set.reply.contains("{name}"));
    }

    #[test]
    fn name_placeholder_replaced() {
        let set = PromptSet::default();
        let text = set.reply_for("半拍");
        assert!(text.contains("半拍"));
        assert!(!text.contains("{name}"));
    }
}
