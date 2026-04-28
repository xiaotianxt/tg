//! Protobuf decoding for Telegram 4.x `packed_info_data` column.

pub mod wire {
    //! Minimal protobuf wire format decoder.

    const WIRE_VARINT: u8 = 0;
    const WIRE_LEN: u8 = 2;

    pub fn decode_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = *data.get(*pos)?;
            *pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Some(result);
            }
        }
    }

    pub fn tag_field(tag: u64) -> (u32, u8) {
        ((tag >> 3) as u32, (tag & 0x07) as u8)
    }

    pub fn skip_field(data: &[u8], pos: &mut usize, wire: u8) -> Option<()> {
        match wire {
            WIRE_VARINT => {
                decode_varint(data, pos)?;
            }
            WIRE_LEN => {
                let len = decode_varint(data, pos)? as usize;
                *pos = pos.checked_add(len)?;
            }
            _ => return None,
        }
        Some(())
    }

    pub fn decode_string(data: &[u8], pos: &mut usize) -> Option<String> {
        let len = decode_varint(data, pos)? as usize;
        let s = std::str::from_utf8(data.get(*pos..pos.checked_add(len)?)?).ok()?;
        *pos += len;
        Some(s.to_string())
    }

    pub fn decode_int32(data: &[u8], pos: &mut usize) -> Option<i32> {
        decode_varint(data, pos).map(|v| v as i32)
    }

    pub fn decode_uint32(data: &[u8], pos: &mut usize) -> Option<u32> {
        decode_varint(data, pos).map(|v| v as u32)
    }

    pub fn decode_submessage<T>(
        data: &[u8],
        pos: &mut usize,
        mut f: impl FnMut(&[u8]) -> Option<T>,
    ) -> Option<T> {
        let len = decode_varint(data, pos)? as usize;
        let sub = data.get(*pos..pos.checked_add(len)?)?;
        *pos += len;
        f(sub)
    }
}

/// Metadata from `packed_info_data` — image (type 3), Telegram 4.0.3+.
#[derive(Debug, Clone, Default)]
pub struct ImageMeta {
    pub width: i32,
    pub height: i32,
    pub filename: String,
}

/// Metadata from `packed_info_data` — video (type 43), Telegram 4.0.3+.
#[derive(Debug, Clone, Default)]
pub struct VideoMeta {
    pub width: i32,
    pub height: i32,
    pub filename: String,
}

/// Metadata from `packed_info_data` — audio (type 34), Telegram 4.0.3+.
#[derive(Debug, Clone, Default)]
pub struct AudioMeta {
    pub audio_text: String,
}

/// Metadata from `packed_info_data` — file (type 62), Telegram 4.0.3+.
#[derive(Debug, Clone, Default)]
pub struct FileMeta {
    pub filename: String,
}

/// Top-level protobuf for `packed_info_data` (Telegram 4.0.3+).
#[derive(Debug, Clone, Default)]
pub struct PackedInfoDataImg2 {
    pub field1: i32,
    pub field2: i32,
    pub image: Option<ImageMeta>,
    pub video: Option<VideoMeta>,
    pub audio: Option<AudioMeta>,
    pub file: Option<FileMeta>,
}

/// Simpler protobuf from Telegram 4.0.x beta.
#[derive(Debug, Clone, Default)]
pub struct PackedInfoDataImg {
    pub field1: i32,
    pub field2: i32,
    pub filename: String,
}

// ===== Parsing =====

pub fn parse_img2(data: &[u8]) -> Option<PackedInfoDataImg2> {
    use wire::*;
    let mut r = PackedInfoDataImg2::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, w) = tag_field(tag);
        match (field, w) {
            (1, 0) => {
                r.field1 = decode_int32(data, &mut pos)?;
            }
            (2, 0) => {
                r.field2 = decode_int32(data, &mut pos)?;
            }
            (3, 2) => {
                r.image = decode_submessage(data, &mut pos, parse_image_meta);
            }
            (4, 2) => {
                r.video = decode_submessage(data, &mut pos, parse_video_meta);
            }
            (5, 2) => {
                r.audio = decode_submessage(data, &mut pos, parse_audio_meta);
            }
            (7, 2) => {
                r.file = decode_submessage(data, &mut pos, parse_file_meta);
            }
            (9, 2) => {
                let len = decode_varint(data, &mut pos)? as usize;
                pos += len;
            }
            _ => {
                skip_field(data, &mut pos, w)?;
            }
        }
    }
    if r.field1 == 0
        && r.field2 == 0
        && r.image.is_none()
        && r.video.is_none()
        && r.audio.is_none()
        && r.file.is_none()
    {
        return None;
    }
    Some(r)
}

pub fn parse_img(data: &[u8]) -> Option<PackedInfoDataImg> {
    use wire::*;
    let mut r = PackedInfoDataImg::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, w) = tag_field(tag);
        match (field, w) {
            (1, 0) => {
                r.field1 = decode_int32(data, &mut pos)?;
            }
            (2, 0) => {
                r.field2 = decode_int32(data, &mut pos)?;
            }
            (3, 2) => {
                r.filename = decode_string(data, &mut pos)?;
            }
            _ => {
                skip_field(data, &mut pos, w)?;
            }
        }
    }
    if r.filename.is_empty() {
        None
    } else {
        Some(r)
    }
}

// ===== Display helpers =====

pub fn display_image(meta: &ImageMeta) -> String {
    let dims = if meta.width > 0 && meta.height > 0 {
        format!(" {}x{}", meta.width, meta.height)
    } else {
        String::new()
    };
    let name = if !meta.filename.is_empty() {
        format!(" {}", meta.filename)
    } else {
        String::new()
    };
    format!("[图片{}{}]", dims, name)
}

pub fn display_video(meta: &VideoMeta) -> String {
    let dims = if meta.width > 0 && meta.height > 0 {
        format!(" {}x{}", meta.width, meta.height)
    } else {
        String::new()
    };
    let name = if !meta.filename.is_empty() {
        format!(" {}", meta.filename)
    } else {
        String::new()
    };
    format!("[视频{}{}]", dims, name)
}

// ===== Internal parsers =====

fn parse_image_meta(data: &[u8]) -> Option<ImageMeta> {
    use wire::*;
    let mut r = ImageMeta::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, w) = tag_field(tag);
        match (field, w) {
            (1, 0) => {
                r.height = decode_int32(data, &mut pos)?;
            }
            (2, 0) => {
                r.width = decode_int32(data, &mut pos)?;
            }
            (4, 2) => {
                r.filename = decode_string(data, &mut pos)?;
            }
            _ => {
                skip_field(data, &mut pos, w)?;
            }
        }
    }
    Some(r)
}

fn parse_video_meta(data: &[u8]) -> Option<VideoMeta> {
    use wire::*;
    let mut r = VideoMeta::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, w) = tag_field(tag);
        match (field, w) {
            (4, 0) => {
                r.height = decode_int32(data, &mut pos)?;
            }
            (5, 0) => {
                r.width = decode_int32(data, &mut pos)?;
            }
            (8, 2) => {
                r.filename = decode_string(data, &mut pos)?;
            }
            _ => {
                skip_field(data, &mut pos, w)?;
            }
        }
    }
    Some(r)
}

fn parse_audio_meta(data: &[u8]) -> Option<AudioMeta> {
    use wire::*;
    let mut r = AudioMeta::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, w) = tag_field(tag);
        match (field, w) {
            (1, 0) => {
                decode_uint32(data, &mut pos)?;
            }
            (2, 2) => {
                r.audio_text = decode_string(data, &mut pos)?;
            }
            _ => {
                skip_field(data, &mut pos, w)?;
            }
        }
    }
    Some(r)
}

fn parse_file_meta(data: &[u8]) -> Option<FileMeta> {
    use wire::*;
    let mut r = FileMeta::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = decode_varint(data, &mut pos)?;
        let (field, w) = tag_field(tag);
        match (field, w) {
            (1, 2) => {
                let _ = decode_submessage(data, &mut pos, |sub| {
                    let mut p = 0;
                    while p < sub.len() {
                        let t = decode_varint(sub, &mut p).unwrap_or(0);
                        let (f, w2) = tag_field(t);
                        if f == 2 && w2 == 2 {
                            r.filename = decode_string(sub, &mut p).unwrap_or_default();
                        } else {
                            let _ = skip_field(sub, &mut p, w2);
                        }
                    }
                    Some(())
                });
            }
            _ => {
                skip_field(data, &mut pos, w)?;
            }
        }
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_img2() {
        let mut buf = Vec::new();
        buf.push(8);
        buf.push(1);
        let img = [
            8, 0xb8, 0x08, 16, 0x80, 0x0f, 34, 8, 116, 101, 115, 116, 46, 106, 112, 103,
        ];
        buf.push(26);
        buf.push(img.len() as u8);
        buf.extend_from_slice(&img);

        let parsed = parse_img2(&buf).unwrap();
        assert_eq!(parsed.field1, 1);
        let m = parsed.image.unwrap();
        assert_eq!(m.height, 1080);
        assert_eq!(m.width, 1920);
        assert_eq!(m.filename, "test.jpg");
    }

    #[test]
    fn test_parse_img_older() {
        let buf = &[8, 1, 16, 0, 26, 7, 112, 105, 99, 46, 112, 110, 103];
        let parsed = parse_img(buf).unwrap();
        assert_eq!(parsed.filename, "pic.png");
    }
}
