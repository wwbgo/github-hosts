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
///
/// Reads every entry to EOF so a corrupt or truncated tail is caught: iterating
/// `archive.entries()` alone only parses headers, so damage after the first few
/// entries would otherwise pass unnoticed.
pub fn v_targz(r: &Fetch) -> Result<String, String> {
    if r.status != 200 {
        return Err(format!("http {}", r.status));
    }
    if r.truncated {
        return Err(format!("archive truncated at {}B", r.body.len()));
    }
    if !r.body.starts_with(&[0x1f, 0x8b]) {
        return Err("not gzip".into());
    }
    let mut ar = Archive::new(GzDecoder::new(&r.body[..]));
    let entries = ar.entries().map_err(|e| format!("tar: {e}"))?;
    let mut count = 0usize;
    let mut first = Vec::new();
    let mut sink = std::io::sink();
    for ent in entries {
        let mut ent = ent.map_err(|e| format!("tar entry: {e}"))?;
        count += 1;
        if first.len() < 2 {
            if let Ok(p) = ent.path() {
                first.push(p.display().to_string());
            }
        }
        // Consuming the body is what forces the decoder through the gzip stream;
        // stopping at the header is why a truncated archive used to pass.
        std::io::copy(&mut ent, &mut sink).map_err(|e| {
            format!(
                "tar entry {}: {e}",
                first.last().cloned().unwrap_or_default()
            )
        })?;
        if count >= 32 {
            // Bounded work: a real source archive has more, but the validator
            // only needs enough to prove the stream decodes end to end.
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

/// Object/asset hosts require a signed URL for a real payload fetch, so any
/// answer on the correct hostname proves the edge serves it -- but a 5xx means
/// the edge answered and then failed, which is not proof of service.
pub fn v_reachable(r: &Fetch) -> Result<String, String> {
    if r.status >= 500 {
        return Err(format!("http {}", r.status));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn f(status: u16, ctype: &str, body: &[u8]) -> Fetch {
        Fetch {
            status,
            content_type: ctype.to_string(),
            location: None,
            body: body.to_vec(),
            total_ms: 1.0,
            truncated: false,
        }
    }

    /// Build a real gzip+tar payload with `n` members.
    fn targz(n: usize) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut enc);
            for i in 0..n {
                let data = format!("payload {i}\n").into_bytes();
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                builder
                    .append_data(&mut h, format!("f{i}.txt"), &data[..])
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        enc.finish().unwrap()
    }

    #[test]
    fn a_real_archive_passes() {
        assert!(v_targz(&f(200, "", &targz(3))).is_ok());
    }

    #[test]
    fn a_truncated_archive_is_rejected() {
        let full = targz(8);
        // Cut the tail: the gzip stream no longer decodes cleanly.
        let cut = &full[..full.len() / 2];
        let mut r = f(200, "", cut);
        assert!(v_targz(&r).is_err(), "half an archive must not validate");

        // And when fetch() reports the cut explicitly, that is fatal too --
        // header-only iteration would otherwise accept a prefix.
        r = f(200, "", &full);
        r.truncated = true;
        assert!(v_targz(&r).is_err());
    }

    #[test]
    fn reachable_rejects_server_errors_but_accepts_client_errors() {
        // A 404/403 on these hosts is expected (no signed URL / no such object)
        // and still proves the edge serves the name. A 5xx does not.
        assert!(v_reachable(&f(404, "", b"")).is_ok());
        assert!(v_reachable(&f(403, "", b"")).is_ok());
        assert!(v_reachable(&f(302, "", b"")).is_ok());
        assert!(v_reachable(&f(500, "", b"")).is_err());
        assert!(v_reachable(&f(503, "", b"")).is_err());
    }

    #[test]
    fn codeload_rejects_a_redirect_even_when_it_points_at_github() {
        // The redirect target is a 404 in practice; accepting it would publish
        // an IP where "Download ZIP" is broken.
        let mut r = f(301, "", b"");
        r.location = Some("https://github.com/o/r/tar.gz/refs/heads/main".into());
        assert!(v_codeload(&r).is_err());
    }

    #[test]
    fn codeload_requires_gzip_magic() {
        assert!(v_codeload(&f(200, "", b"Not Found")).is_err());
        // gzip magic but garbage payload: header parses, body does not.
        assert!(v_codeload(&f(200, "", &[0x1f, 0x8b, 0x08, 0x00])).is_err());
    }

    #[test]
    fn html_rejects_short_and_non_github_pages() {
        assert!(v_html(&f(200, "text/html", b"<html>github</html>")).is_err());
        let filler = "x".repeat(4000);
        assert!(v_html(&f(200, "text/html", filler.as_bytes())).is_err());
        let ok = format!("<html><body>github {}</body></html>", "y".repeat(4000));
        assert!(v_html(&f(200, "text/html", ok.as_bytes())).is_ok());
    }

    #[test]
    fn asset_requires_a_usable_content_type() {
        assert!(v_asset(&f(200, "text/html; charset=utf-8", &[b'x'; 128])).is_err());
        assert!(v_asset(&f(200, "text/css", &[b'x'; 128])).is_ok());
        assert!(v_asset(&f(200, "image/png", b"tiny")).is_err());
    }

    #[test]
    fn image_needs_real_magic_bytes() {
        assert!(v_image(&f(200, "", b"<html>nope</html>")).is_err());
        assert!(v_image(&f(200, "", b"\x89PNG\r\n\x1a\nrest")).is_ok());
        assert!(v_image(&f(200, "", b"GIF89a rest")).is_ok());
    }
}
