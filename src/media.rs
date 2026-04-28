
/// Parsed info about an image message (type 3).
#[derive(Debug, Clone, Default)]
pub struct ImageInfo {
    pub aes_key: String,
    pub cdn_thumb_url: String,
    pub cdn_midi_url: String,
    pub cdn_big_url: String,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub raw_length: u64,
}

impl ImageInfo {
    pub fn display(&self) -> String {
        let dims = if self.thumb_width > 0 && self.thumb_height > 0 {
            format!(" {}x{}", self.thumb_width, self.thumb_height)
        } else {
            String::new()
        };
        let size = if self.raw_length > 0 {
            if self.raw_length > 1024 * 1024 {
                format!(" {:.1}MB", self.raw_length as f64 / (1024.0 * 1024.0))
            } else if self.raw_length > 1024 {
                format!(" {}KB", self.raw_length / 1024)
            } else {
                format!(" {}B", self.raw_length)
            }
        } else {
            String::new()
        };
        format!("[图片{}{}]", dims, size)
    }

}

/// Parsed info about a video message (type 43).
#[derive(Debug, Clone, Default)]
pub struct VideoInfo {
    pub aes_key: String,
    pub cdn_video_url: String,
    pub cdn_thumb_url: String,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub play_length: u32,
    pub raw_video_length: u64,
}

impl VideoInfo {
    pub fn display(&self) -> String {
        let dur = if self.play_length > 0 {
            let m = self.play_length / 60;
            let s = self.play_length % 60;
            format!(" {}\u{2032}{:02}\u{2033}", m, s)
        } else {
            String::new()
        };
        let dims = if self.thumb_width > 0 && self.thumb_height > 0 {
            format!(" {}x{}", self.thumb_width, self.thumb_height)
        } else {
            String::new()
        };
        format!("[视频{}{}]", dur, dims)
    }

}

/// Parsed info about a sticker message (type 47).
#[derive(Debug, Clone, Default)]
pub struct StickerInfo {
    pub product_id: String,
    pub url: String,
    pub pack_name: String,
    pub pack_url: String,
    pub has_emojibuf: bool,
}

impl StickerInfo {
    pub fn display(&self) -> String {
        let name = if !self.pack_name.is_empty() {
            format!(" {}", self.pack_name)
        } else if !self.product_id.is_empty() {
            // Shorten the product ID for display
            let short = self.product_id.rsplit('.').next().unwrap_or(&self.product_id);
            if short.len() <= 30 {
                format!(" {}", short)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if self.has_emojibuf {
            format!("[表情{} (含图)]", name)
        } else {
            format!("[表情{}]", name)
        }
    }
}

/// Parsed info about a link (type 49, subtype 5).
#[derive(Debug, Clone, Default)]
pub struct LinkInfo {
    pub title: String,
    pub description: String,
    pub url: String,
}

impl LinkInfo {
    pub fn display(&self) -> String {
        let mut parts = vec!["[链接]".to_string()];
        if !self.title.is_empty() { parts.push(self.title.clone()); }
        if !self.description.is_empty() { parts.push(format!("- {}", self.description)); }
        if !self.url.is_empty() && self.url.len() < 120 {
            parts.push(format!("\n  {}", self.url));
        }
        parts.join(" ")
    }
}

/// Parsed info about a mini program (type 49, subtype 33).
#[derive(Debug, Clone, Default)]
pub struct MiniProgramInfo {
    pub title: String,
    pub app_name: String,
    pub page_path: String,
}

impl MiniProgramInfo {
    pub fn display(&self) -> String {
        let app = if !self.app_name.is_empty() {
            format!(" - {}", self.app_name)
        } else {
            String::new()
        };
        let title = if !self.title.is_empty() {
            self.title.clone()
        } else {
            "小程序".to_string()
        };
        let path = if !self.page_path.is_empty() && self.page_path.len() < 80 {
            format!("\n  path: {}", self.page_path)
        } else {
            String::new()
        };
        format!("[小程序] {}{}{}", title, app, path)
    }
}

/// Parse image metadata from XML (type 3).
pub(crate) fn parse_image_info(xml: &str) -> ImageInfo {
    let mut info = ImageInfo::default();

    if let Some(v) = extract_xml_attr(xml, "aeskey") { info.aes_key = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnthumburl") { info.cdn_thumb_url = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnmidiurl") { info.cdn_midi_url = v; }
    if let Some(v) = extract_xml_attr(xml, "cdndisplaybackupurl") { info.cdn_big_url = v; }

    if let Some(v) = extract_xml_attr(xml, "cdnthumbwidth").and_then(|s| s.parse().ok()) { info.thumb_width = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbheight").and_then(|s| s.parse().ok()) { info.thumb_height = v; }
    if let Some(v) = extract_xml_attr(xml, "rawlength").and_then(|s| s.parse().ok()) { info.raw_length = v; }
    if info.raw_length == 0 {
        if let Some(v) = extract_xml_attr(xml, "cdnmidimagerawlength").and_then(|s| s.parse().ok()) { info.raw_length = v; }
    }

    info
}

/// Parse video metadata from XML (type 43).
pub(crate) fn parse_video_info(xml: &str) -> VideoInfo {
    let mut info = VideoInfo::default();

    if let Some(v) = extract_xml_attr(xml, "aeskey") { info.aes_key = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnvideourl") { info.cdn_video_url = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnthumburl") { info.cdn_thumb_url = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbwidth").and_then(|s| s.parse().ok()) { info.thumb_width = v; }
    if let Some(v) = extract_xml_attr(xml, "cdnthumbheight").and_then(|s| s.parse().ok()) { info.thumb_height = v; }
    if let Some(v) = extract_xml_attr(xml, "playlength").and_then(|s| s.parse().ok()) { info.play_length = v; }
    if let Some(v) = extract_xml_attr(xml, "rawvideolength").and_then(|s| s.parse().ok()) { info.raw_video_length = v; }

    info
}

/// Parse sticker metadata from XML (type 47).
pub(crate) fn parse_sticker_info(xml: &str) -> StickerInfo {
    let mut info = StickerInfo::default();

    if let Some(v) = extract_xml_attr(xml, "productid") { info.product_id = v; }
    if let Some(v) = extract_xml_attr(xml, "url") { info.url = v; }
    if let Some(v) = extract_xml_tag(xml, "packname") { info.pack_name = v; }
    if let Some(v) = extract_xml_attr(xml, "packurl") { info.pack_url = v; }
    info.has_emojibuf = xml.contains("<emojibuf>");

    info
}

/// Parse link info from XML (type 49, subtype 5).
pub(crate) fn parse_link_info(xml: &str) -> Option<LinkInfo> {
    if !xml.contains("<type>5</type>") {
        return None;
    }
    Some(LinkInfo {
        title: extract_xml_tag(xml, "title").unwrap_or_default(),
        description: extract_xml_tag(xml, "des").unwrap_or_default(),
        url: extract_xml_tag(xml, "url").unwrap_or_default(),
    })
}

/// Parse mini program info from XML (type 49, subtype 33).
pub(crate) fn parse_mini_program_info(xml: &str) -> Option<MiniProgramInfo> {
    if !xml.contains("<type>33</type>") {
        return None;
    }
    Some(MiniProgramInfo {
        title: extract_xml_tag(xml, "title").unwrap_or_default(),
        app_name: extract_xml_tag(xml, "appname").unwrap_or_default(),
        page_path: extract_xml_tag(xml, "pagepath").unwrap_or_default(),
    })
}


// ===== XML helpers =====

/// Extract attribute value from a self-closing XML tag like `<tag attr="value" .../>`.
pub(crate) fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"{}=""#, attr);
    let start = xml.find(&pattern)?;
    let value_start = start + pattern.len();
    if value_start >= xml.len() { return None; }
    let rest = &xml[value_start..];
    if !rest.starts_with('"') { return None; }
    let rest = &rest[1..];
    let end = rest.find('"')?;
    let value = rest[..end].to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// Extract text content from `<tag>text</tag>`.
pub(crate) fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)?;
    let value_start = start + open.len();
    if value_start >= xml.len() { return None; }
    let rest = &xml[value_start..];
    let value_end = rest.find(&close)?;
    let value = rest[..value_end].trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// Extract integer content from an XML tag.
pub(crate) fn extract_xml_tag_int(xml: &str, tag: &str) -> Option<i64> {
    let text = extract_xml_tag(xml, tag)?;
    text.parse::<i64>().ok()
}
