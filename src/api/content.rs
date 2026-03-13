/// Cover image/video resolution.
pub enum CoverRes {
    R80,
    R160,
    R320,
    R640,
    R1280,
}

impl CoverRes {
    fn as_str(&self) -> &'static str {
        match self {
            Self::R80 => "80",
            Self::R160 => "160",
            Self::R320 => "320",
            Self::R640 => "640",
            Self::R1280 => "1280",
        }
    }
}

/// Cover media type.
pub enum CoverType {
    Image,
    Video,
}

/// Options for cover URL generation.
pub struct CoverOpts {
    pub res: CoverRes,
    pub cover_type: CoverType,
}

impl Default for CoverOpts {
    fn default() -> Self {
        Self {
            res: CoverRes::R1280,
            cover_type: CoverType::Image,
        }
    }
}

/// Build a cover URL from a TIDAL resource UUID.
///
/// UUIDs like `"86538ca7-08fd-40ff-9a75-af88d74d1f48"` are
/// transformed into a path by replacing `-` with `/`.
///
/// Result: `https://resources.tidal.com/images/8653/.../1280x1280.jpg`
pub fn format_cover_url(uuid: &str, opts: Option<&CoverOpts>) -> String {
    let defaults = CoverOpts::default();
    let opts = opts.unwrap_or(&defaults);
    let res = opts.res.as_str();
    let (type_plural, ext) = match opts.cover_type {
        CoverType::Image => ("images", "jpg"),
        CoverType::Video => ("videos", "mp4"),
    };
    let path = uuid.replace('-', "/");
    format!("https://resources.tidal.com/{type_plural}/{path}/{res}x{res}.{ext}")
}
