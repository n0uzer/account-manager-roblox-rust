//! Avatar headshots: download from `thumbnails.roblox.com`, decode, cache on disk.
//!
//! Runs on the worker thread: the UI only receives ready RGBA pixels.

use std::io::Read;
use std::path::PathBuf;

use serde::Deserialize;

use crate::roblox::USER_AGENT;
use crate::store;

/// Ready-to-upload image data.
pub struct AvatarImage {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

#[derive(Deserialize)]
struct ThumbResponse {
    #[serde(default)]
    data: Vec<ThumbEntry>,
}

#[derive(Deserialize)]
struct ThumbEntry {
    #[serde(default, rename = "imageUrl")]
    image_url: Option<String>,
}

fn cache_dir() -> PathBuf {
    store::app_dir().join("avatars")
}

/// Disk cache first, network second.
pub fn load(user_id: u64) -> Result<AvatarImage, String> {
    if user_id == 0 {
        return Err("Нет User ID".into());
    }

    let path = cache_dir().join(format!("{user_id}.png"));
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(image) = decode(&bytes) {
            return Ok(image);
        }
    }

    let bytes = download(user_id)?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &bytes);
    decode(&bytes)
}

fn download(user_id: u64) -> Result<Vec<u8>, String> {
    let meta_url = format!(
        "https://thumbnails.roblox.com/v1/users/avatar-headshot\
         ?userIds={user_id}&size=48x48&format=Png&isCircular=true"
    );

    let response = ureq::get(&meta_url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .call()
        .map_err(|err| format!("Аватар: {err}"))?;

    let meta: ThumbResponse = response
        .into_json()
        .map_err(|err| format!("Аватар: не разобрал ответ ({err})"))?;

    let image_url = meta
        .data
        .into_iter()
        .find_map(|entry| entry.image_url)
        .ok_or_else(|| "Roblox не вернул ссылку на аватар".to_string())?;

    let mut bytes = Vec::new();
    ureq::get(&image_url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|err| format!("Аватар: {err}"))?
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Аватар: {err}"))?;

    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<AvatarImage, String> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|err| format!("Аватар: PNG ({err})"))?;
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(AvatarImage {
        size,
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_png_is_rejected() {
        assert!(decode(b"not a png at all").is_err());
    }

    #[test]
    fn zero_user_id_never_hits_network() {
        assert!(load(0).is_err());
    }
}
