//! 表情包收藏与复用。
//!
//! 只收藏模型判定为「可复用」的表情：先写到 `tmp/` 算哈希，命中已有就直接删掉临时文件，
//! 没命中就转正——所以磁盘占用只跟收藏数量有关，跟聊天量无关。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::db::{self, StickerRow};
use crate::state::{now_ts, AppState};

pub async fn collect(
    state: &Arc<AppState>,
    group_id: i64,
    url: &str,
    caption: &str,
    emotion: &str,
    reusable: bool,
) -> Option<i64> {
    let cfg = state.cfg();
    if !cfg.sticker.enabled || !reusable || !group_allowed(&cfg, group_id) {
        return None;
    }

    let bytes = download(state, url).await?;
    let hash = sha256_hex(&bytes);

    if let Ok(Some(id)) = db::sticker_by_hash(&state.db, group_id, &hash).await {
        debug!("表情包已收藏过（hash={hash}），跳过");
        return Some(id);
    }

    let dir = PathBuf::from(&cfg.sticker.dir);
    let tmp_dir = dir.join("tmp");
    if let Err(e) = tokio::fs::create_dir_all(&tmp_dir).await {
        warn!("创建表情目录失败: {e}");
        return None;
    }

    let tmp = tmp_dir.join(format!("{hash}.part"));
    if let Err(e) = tokio::fs::write(&tmp, &bytes).await {
        warn!("写临时表情失败: {e}");
        return None;
    }

    if let Ok(count) = db::count_stickers(&state.db, group_id).await {
        if count >= cfg.sticker.max_count as i64 {
            if let Ok(Some(old)) = db::evict_oldest_sticker(&state.db, group_id).await {
                let _ = tokio::fs::remove_file(&old).await;
            }
        }
    }

    let final_path = dir.join(format!("{hash}{}", extension_for(url, &bytes)));
    if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
        warn!("表情转正失败: {e}");
        let _ = tokio::fs::remove_file(&tmp).await;
        return None;
    }

    match db::insert_sticker(
        &state.db,
        group_id,
        &hash,
        &final_path.to_string_lossy(),
        caption,
        emotion,
        now_ts(),
    )
    .await
    {
        Ok(id) => {
            info!(
                "收藏表情包 #{} 群 {}：{}",
                id,
                group_id,
                crate::llm::truncate(caption, 20)
            );
            Some(id)
        }
        Err(e) => {
            warn!("写表情记录失败: {e:#}");
            let _ = tokio::fs::remove_file(&final_path).await;
            None
        }
    }
}

pub async fn pick(state: &Arc<AppState>, group_id: i64) -> Option<StickerRow> {
    let cfg = state.cfg();
    if !cfg.sticker.enabled || !group_allowed(&cfg, group_id) {
        return None;
    }
    if rand01() >= cfg.sticker.send_probability {
        return None;
    }
    let row = db::pick_sticker(&state.db, group_id).await.ok().flatten()?;
    if tokio::fs::metadata(&row.file).await.is_err() {
        return None;
    }
    let _ = db::touch_sticker(&state.db, row.id, now_ts()).await;
    Some(row)
}

/// 读文件并转成 NapCat 认识的 `base64://` 形式。
pub fn base64_data(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("base64://{encoded}"))
}

pub fn group_allowed(cfg: &crate::config::Config, group_id: i64) -> bool {
    cfg.sticker.groups.is_empty() || cfg.sticker.groups.contains(&group_id)
}

async fn download(state: &Arc<AppState>, url: &str) -> Option<Vec<u8>> {
    let cfg = state.cfg();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            cfg.vision.download_timeout_secs.max(1),
        ))
        .build()
        .ok()?;
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        warn!("下载表情失败: HTTP {}", resp.status());
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > cfg.vision.max_bytes.max(1024) {
        warn!("表情太大，放弃收藏：{} 字节", bytes.len());
        return None;
    }
    Some(bytes.to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn extension_for(url: &str, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return ".png";
    }
    if bytes.starts_with(b"GIF8") {
        return ".gif";
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return ".webp";
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return ".jpg";
    }
    let lower = url.to_ascii_lowercase();
    if lower.contains(".png") {
        ".png"
    } else if lower.contains(".gif") {
        ".gif"
    } else if lower.contains(".webp") {
        ".webp"
    } else {
        ".jpg"
    }
}

pub fn rand01() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x % 10_000) as f64 / 10_000.0
}

#[allow(dead_code)]
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable() {
        assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
        assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
    }

    #[test]
    fn extension_by_magic() {
        assert_eq!(extension_for("x", &[0x89, 0x50, 0x4E, 0x47]), ".png");
        assert_eq!(extension_for("x", b"GIF89a"), ".gif");
        assert_eq!(extension_for("x", &[0xFF, 0xD8, 0x00]), ".jpg");
        assert_eq!(extension_for("https://a/b.gif", &[0x00; 4]), ".gif");
    }

    #[test]
    fn random_in_range() {
        for _ in 0..50 {
            let r = rand01();
            assert!((0.0..1.0).contains(&r));
        }
    }
}
