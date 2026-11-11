pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Emphasis {
    Bold,
    Italic,
}

impl Emphasis {
    fn tag(self) -> &'static str {
        match self {
            Emphasis::Bold => "b",
            Emphasis::Italic => "i",
        }
    }
}

#[derive(Clone, Copy)]
enum Piece<'a> {
    Text(&'a str),
    Open(Emphasis),
    Close(Emphasis),
}

fn pieces(s: &str) -> Vec<Piece<'_>> {
    const TAGS: [(&str, Piece<'static>); 4] = [
        ("<b>", Piece::Open(Emphasis::Bold)),
        ("</b>", Piece::Close(Emphasis::Bold)),
        ("<i>", Piece::Open(Emphasis::Italic)),
        ("</i>", Piece::Close(Emphasis::Italic)),
    ];
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(at) = rest.find('<') {
        let tail = &rest[at..];
        match TAGS.iter().find(|(literal, _)| tail.starts_with(literal)) {
            Some((literal, piece)) => {
                if at > 0 {
                    out.push(Piece::Text(&rest[..at]));
                }
                out.push(*piece);
                rest = &tail[literal.len()..];
            }
            None => {
                out.push(Piece::Text(&rest[..=at]));
                rest = &tail[1..];
            }
        }
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    out
}

/// Plugin text with bare `<b>` and `<i>` kept where they pair up and nest, everything else
/// escaped. A stray tag stays text: left open, the parser would carry it into the next blocks.
pub(crate) fn emphasis_html(s: &str) -> String {
    let pieces = pieces(s);
    let mut kept = vec![false; pieces.len()];
    let mut open: Vec<(usize, Emphasis)> = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        match *piece {
            Piece::Open(e) => open.push((i, e)),
            Piece::Close(e) => {
                if let Some(&(start, top)) = open.last()
                    && top == e
                {
                    open.pop();
                    kept[start] = true;
                    kept[i] = true;
                }
            }
            Piece::Text(_) => {}
        }
    }
    pieces
        .iter()
        .zip(kept)
        .map(|(piece, kept)| match (*piece, kept) {
            (Piece::Text(t), _) => escape_html(t),
            (Piece::Open(e), true) => format!("<{}>", e.tag()),
            (Piece::Close(e), true) => format!("</{}>", e.tag()),
            (Piece::Open(e), false) => format!("&lt;{}&gt;", e.tag()),
            (Piece::Close(e), false) => format!("&lt;/{}&gt;", e.tag()),
        })
        .collect()
}

/// The author card a plugin's manifest describes, empty when it names no author. Everything in
/// it is the plugin's own claim about itself.
pub(crate) fn author_block(manifest_json: &str) -> String {
    let manifest: serde_json::Value =
        serde_json::from_str(manifest_json).unwrap_or(serde_json::Value::Null);
    let author = manifest.get("author");
    let author_name = author
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let author_url = author
        .and_then(|a| a.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let avatar_url = author
        .and_then(|a| a.get("avatarUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let plugin_desc = manifest
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if author_name.is_empty() {
        return String::new();
    }
    let avatar_html = if !avatar_url.is_empty() {
        format!(
            r#"<img src="{src}" style="width:32px;height:32px;border-radius:50%;flex-shrink:0" onerror="this.style.display='none'">"#,
            src = escape_html(avatar_url)
        )
    } else if author_url.contains("github.com/") {
        let gh_user = author_url
            .split("github.com/")
            .nth(1)
            .and_then(|s| s.split('/').next())
            .unwrap_or("");
        if !gh_user.is_empty() {
            format!(
                r#"<img src="https://github.com/{user}.png?size=64" style="width:32px;height:32px;border-radius:50%;flex-shrink:0" onerror="this.style.display='none'">"#,
                user = escape_html(gh_user)
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let desc_html = if !plugin_desc.is_empty() {
        format!(
            r#"<div style="font-size:12px;color:#999;margin-top:2px">{}</div>"#,
            emphasis_html(plugin_desc)
        )
    } else {
        String::new()
    };

    format!(
        r#"<div style="display:flex;align-items:center;gap:10px;margin:0 0 14px;padding:10px 14px;background:#222;border-radius:4px">
        {avatar}
        <div>
            <div style="font-size:13px;color:#fff;font-weight:600">{name}</div>
            {desc}
        </div>
    </div>"#,
        avatar = avatar_html,
        name = escape_html(author_name),
        desc = desc_html
    )
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/dialog/html.rs"]
mod tests;
