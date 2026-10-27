//! Slack 已生成的多档预览与保留格式的原件；版本编号不包含平台地址。

use super::*;
use hhh_im_protocol::{ResourceKind, ResourceVariant, ResourceVariants};
use std::time::Duration;

/// 元数据只在当前账号的客户端内暂存，覆盖正在浏览的图片，避免描述后下载又查一次。
const FILE_DETAILS_MAX: usize = 128;
const FILE_DETAILS_TTL: Duration = Duration::from_secs(5 * 60);
const PREVIEWS: &[&str] = &[
    "thumb_360",
    "thumb_480",
    "thumb_720",
    "thumb_800",
    "thumb_960",
    "thumb_1024",
];

pub(super) type FileDetails = Mutex<HashMap<String, (Instant, Value)>>;

fn preview_mime(url: &str) -> Option<String> {
    let path = Url::parse(url).ok()?.path().to_ascii_lowercase();
    let mime = if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else {
        return None;
    };
    Some(mime.into())
}

pub(super) fn variants(file: &Value, kind: ResourceKind) -> ResourceVariants {
    let mut found = Vec::new();
    let mime = rich::str_of(file, "mimetype");
    let video_cover = kind == ResourceKind::Image && translate::is_video_mime(mime);
    let animated = mime == "image/gif";
    if kind == ResourceKind::Image {
        let fields = if video_cover {
            &["thumb_video"][..]
        } else if animated {
            &["thumb_360_gif", "thumb_480_gif"][..]
        } else {
            PREVIEWS
        };
        for field in fields {
            let address = rich::str_of(file, field);
            if address.is_empty() {
                continue;
            }
            let width = file
                .get(format!("{field}_w"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            let height = file
                .get(format!("{field}_h"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            let dimensions = width.zip(height);
            found.push(ResourceVariant {
                id: (*field).into(),
                preview: true,
                mime: preview_mime(address),
                width: dimensions.map(|d| d.0),
                height: dimensions.map(|d| d.1),
                ..Default::default()
            });
        }
    }
    if !video_cover
        && kind != ResourceKind::Sticker
        && ["url_private_download", "url_private"]
            .iter()
            .any(|key| !rich::str_of(file, key).is_empty())
    {
        let displayable = matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
        );
        let dimensions = file
            .get("original_w")
            .and_then(Value::as_u64)
            .zip(file.get("original_h").and_then(Value::as_u64))
            .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)));
        found.push(ResourceVariant {
            id: "original".into(),
            original: true,
            preview: found.is_empty() && kind == ResourceKind::Image && displayable,
            mime: (!mime.is_empty()).then(|| mime.into()),
            name: file.get("name").and_then(Value::as_str).map(str::to_owned),
            bytes: file.get("size").and_then(Value::as_u64),
            width: dimensions.map(|d| d.0),
            height: dimensions.map(|d| d.1),
        });
    }
    ResourceVariants { variants: found }
}

pub(super) fn address<'a>(
    file: &'a Value,
    kind: ResourceKind,
    variant: Option<&str>,
) -> Result<(&'a str, Option<u64>)> {
    if let Some(id) = variant {
        if !variants(file, kind).variants.iter().any(|v| v.id == id) {
            return Err(Error::new(
                ErrorKind::NotFound,
                "Slack 没有提供这个图片版本。",
            ));
        }
        if id != "original" {
            return Ok((rich::str_of(file, id), None));
        }
    } else if kind == ResourceKind::Image
        && translate::is_video_mime(rich::str_of(file, "mimetype"))
    {
        return Ok((rich::str_of(file, "thumb_video"), None));
    }
    let address = ["url_private_download", "url_private"]
        .iter()
        .map(|field| rich::str_of(file, field))
        .find(|url| !url.is_empty())
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "Slack 没有给出这个文件的下载地址。"))?;
    Ok((address, file.get("size").and_then(Value::as_u64)))
}

impl SlackClient {
    pub(super) async fn file_details(&self, key: &str) -> Result<Value> {
        if let Some(file) = self
            .resource_files
            .lock()
            .expect("Slack 文件资料锁中毒")
            .get(key)
            .filter(|(at, _)| at.elapsed() < FILE_DETAILS_TTL)
            .map(|(_, file)| file.clone())
        {
            return Ok(file);
        }
        let value = self.query("files.info", &[("file", key)]).await?;
        let source = value.get("file").cloned().unwrap_or_default();
        let file = Value::Object(
            source
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .filter(|(key, _)| {
                            key.starts_with("thumb_")
                                || matches!(
                                    key.as_str(),
                                    "name"
                                        | "size"
                                        | "mimetype"
                                        | "original_w"
                                        | "original_h"
                                        | "url_private"
                                        | "url_private_download"
                                )
                        })
                        .filter(|(key, _)| key.as_str() != "thumb_tiny")
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        );
        let mut cache = self.resource_files.lock().expect("Slack 文件资料锁中毒");
        if cache.len() >= FILE_DETAILS_MAX
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, v)| v.0)
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key.into(), (Instant::now(), file.clone()));
        Ok(file)
    }

    pub(crate) async fn describe_resource(
        &self,
        key: &str,
        kind: ResourceKind,
    ) -> Result<ResourceVariants> {
        if !translate::is_slack_id(key) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Slack 文件编号不正确。",
            ));
        }
        let file = self.file_details(key).await?;
        Ok(variants(&file, kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heic_uses_generated_png_previews_and_keeps_original_bytes_separate() {
        // 真实只读形状：两张 HEIC 均有 PNG 预览；编号、地址和名字已脱敏。
        let file = json!({"name":"photo.HEIC","mimetype":"image/heic","size":2256293,
            "original_w":4284,"original_h":5712,
            "url_private_download":"https://files.slack.com/files-pri/test/photo.HEIC",
            "thumb_360":"https://files.slack.com/files-tmb/test/photo_360.png","thumb_360_w":270,"thumb_360_h":360,
            "thumb_1024":"https://files.slack.com/files-tmb/test/photo_1024.png","thumb_1024_w":768,"thumb_1024_h":1024});
        let description = variants(&file, ResourceKind::Image);
        assert!(description.validate().is_ok());
        assert_eq!(description.preview(512).unwrap().id, "thumb_1024");
        assert_eq!(
            description.original().unwrap().name.as_deref(),
            Some("photo.HEIC")
        );
        assert_eq!(
            address(&file, ResourceKind::Image, Some("thumb_1024"))
                .unwrap()
                .1,
            None
        );
        assert_eq!(
            address(&file, ResourceKind::Image, Some("original"))
                .unwrap()
                .1,
            Some(2256293)
        );
        assert!(address(&file, ResourceKind::Image, Some("missing")).is_err());
    }

    #[test]
    fn a_video_cover_is_only_a_preview_and_a_single_jpeg_can_be_reused() {
        let video = json!({"mimetype":"video/quicktime","thumb_video":"https://files.slack.com/test/cover.jpeg","url_private":"https://files.slack.com/test/movie.mov"});
        assert!(variants(&video, ResourceKind::Image).original().is_none());
        let jpeg =
            json!({"mimetype":"image/jpeg","url_private":"https://files.slack.com/test/photo.jpg"});
        let same = variants(&jpeg, ResourceKind::Image);
        assert_eq!(same.preview(512).unwrap().id, same.original().unwrap().id);
    }
}
