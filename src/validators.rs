//! Content validators. Each one answers a narrow question about a payload and
//! returns a human-readable detail string on success.
//!
//! A reachable TLS endpoint is not enough: GitHub's CDN fronts many hostnames,
//! so an IP may handshake for `github.com` yet be unable to serve the asset
//! host. Validating the actual bytes is the only way to promise that the site
//! renders, downloads work and `git` fetches.

use crate::net::Fetch;
use flate2::read::GzDecoder;
use tar::Archive;

pub type Validator = fn(&Fetch) -> Result<String, String>;

pub fn v_html(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    if r.body.len() < 3000 {
        return Err(format!("short body {}B", r.body.len()));
    }
    let low = String::from_utf8_lossy(&r.body[..r.body.len().min(200_000)]).to_lowercase();
    if !low.contains("<html") || !low.contains("github") {
        return Err("not a github html page".into());
    }
    Ok(format!("html {}B", r.body.len()))
}

pub fn v_asset(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    let base = r
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let ok = base.contains("css")
        || base.contains("javascript")
        || base.contains("image")
        || base.contains("font")
        || base.contains("svg");
    if !ok {
        return Err(format!("content-type {base:?}"));
    }
    if r.body.len() < 64 {
        return Err(format!("short body {}B", r.body.len()));
    }
    Ok(format!("{} {}B", base, r.body.len()))
}

pub fn v_raw(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    if !r.body.windows(11).any(|w| w == b"Hello World") {
        return Err("marker missing".into());
    }
    Ok(format!("raw {}B", r.body.len()))
}

pub fn v_image(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    let b = &r.body;
    let kind = if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if b.starts_with(b"\xff\xd8\xff") {
        "jpeg"
    } else if b.starts_with(b"GIF8") {
        "gif"
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "webp"
    } else if b.starts_with(b"<?xml") || b.starts_with(b"<svg") {
        "svg"
    } else {
        return Err(format!("not an image ({}B)", b.len()));
    };
    Ok(format!("{kind} {}B", b.len()))
}

/// Full source-archive path: HTTP 200, real gzip, real tar, at least one member.
pub fn v_targz(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    if !r.body.starts_with(&[0x1f, 0x8b]) {
        return Err("not gzip".into());
    }
    let mut ar = Archive::new(GzDecoder::new(&r.body[..]));
    let entries = ar.entries().map_err(|e| format!("tar: {e}"))?;
    let mut count = 0usize;
    let mut first = Vec::new();
    for ent in entries {
        let ent = ent.map_err(|e| format!("tar entry: {e}"))?;
        count += 1;
        if first.len() < 2 {
            if let Ok(p) = ent.path() {
                first.push(p.display().to_string());
            }
        }
        if count >= 3 {
            break;
        }
    }
    if count == 0 {
        return Err("empty archive".into());
    }
    Ok(format!(
        "tar.gz {}B {} entries [{}]",
        r.body.len(),
        count,
        first.join(", ")
    ))
}

/// Source archives.
///
/// Strictly 200 + gzip + parsable tar. Some Azure edges answer with a 301 to
/// `https://github.com/<repo>/tar.gz/...`, and that URL is a 404 -- accepting
/// the redirect would hand out an IP where "Download ZIP" is broken. A
/// redirect here therefore means "this edge cannot deliver it".
pub fn v_codeload(r: &Fetch) -> Result<String, String> {
    if matches!(r.status, 301 | 302 | 307 | 308) {
        let loc = r.location.as_deref().unwrap_or("?");
        return Err(format!(
            "http {} redirect to {} (not a real archive)",
            r.status, loc
        ));
    }
    v_targz(r)
}

/// Alias hostnames (`www.`, `gist.`) legitimately answer with a 301 to the
/// canonical name, so a redirect to a github hostname counts as healthy.
pub fn v_html_or_redirect(r: &Fetch) -> Result<String, String> {
    if matches!(r.status, 301 | 302 | 307 | 308) {
        return match r.location.as_deref() {
            Some(loc) if loc.contains("github") => Ok(format!("http {} -> {}", r.status, loc)),
            Some(loc) => Err(format!("http {} -> {}", r.status, loc)),
            None => Err(format!("http {} without Location", r.status)),
        };
    }
    v_html(r)
}

pub fn v_zen(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    let txt = String::from_utf8_lossy(&r.body);
    if txt.trim().is_empty() {
        return Err("empty body".into());
    }
    Ok(format!("api {}B", r.body.len()))
}

/// Signed URLs are required for a real payload fetch on the object hosts, so
/// any HTTP response on the correct hostname is treated as proof of service.
pub fn v_reachable(r: &Fetch) -> Result<String, String> {
    if r.status == 0 {
        return Err("no response".into());
    }
    Ok(format!("http {}", r.status))
}

pub fn v_git(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    // A smart-http advertisement starts with the pkt-line
    // `001e# service=git-upload-pack\n`; "service=git-upload-pack" is 23 bytes.
    let has = r.body.windows(23).any(|w| w == b"service=git-upload-pack")
        || r.body.windows(9).any(|w| w == b"version 2")
        || r.body.starts_with(b"000e");
    if has {
        return Ok(format!("upload-pack advert {}B", r.body.len()));
    }
    Err(format!(
        "no advertisement (ct={}, first={:?})",
        r.content_type,
        String::from_utf8_lossy(&r.body[..r.body.len().min(24)])
    ))
}
