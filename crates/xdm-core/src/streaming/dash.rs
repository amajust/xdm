//! MPEG-DASH manifest parsing, port of `MpdParser.kt` / `RepresentationParser`
//! / `TemplateParser` (subset: single Period, highest-bandwidth
//! Representation, SegmentTemplate with `$Number$`/`$Time$`/`%0Nd` width and
//! SegmentTimeline, SegmentList, BaseURL chains).

use roxmltree::Node;
use url::Url;

/// The selected representation with its full segment URL plan.
#[derive(Debug, Clone, PartialEq)]
pub struct DashRepresentation {
    pub id: String,
    pub bandwidth: u64,
    /// mimeType, e.g. `video/mp4` — decides the output container.
    pub mime: String,
    pub init_url: Option<String>,
    pub segments: Vec<String>,
}

pub fn parse_mpd(text: &str, base: &Url) -> Result<DashRepresentation, String> {
    let doc = roxmltree::Document::parse(text).map_err(|e| format!("xml: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "MPD" {
        return Err("root element is not MPD".to_owned());
    }

    let period = root
        .children()
        .find(|n| n.has_tag_name("Period"))
        .ok_or("no Period element")?;
    let period_secs = period
        .attribute("duration")
        .and_then(parse_iso_duration)
        .or_else(|| root.attribute("mediaPresentationDuration").and_then(parse_iso_duration));

    let mut best: Option<(u64, String, String)> = None; // (bandwidth, id, mime)
    for adaptation in period.children().filter(|n| n.has_tag_name("AdaptationSet")) {
        let set_mime = adaptation.attribute("mimeType").unwrap_or_default();
        for repr in adaptation.children().filter(|n| n.has_tag_name("Representation")) {
            let bandwidth: u64 = repr
                .attribute("bandwidth")
                .and_then(|v| v.parse().ok())
                .or_else(|| adaptation.attribute("bandwidth").and_then(|v| v.parse().ok()))
                .unwrap_or(0);
            let mime = repr
                .attribute("mimeType")
                .unwrap_or(set_mime)
                .to_owned();
            if best.as_ref().is_none_or(|(bw, _, _)| bandwidth > *bw) {
                let id = repr.attribute("id").unwrap_or_default().to_owned();
                best = Some((bandwidth, id, mime));
            }
        }
    }
    let (bandwidth, repr_id, mime) = best.ok_or("no Representation elements")?;

    // Re-find the chosen representation node to resolve its segments,
    // keeping its adaptation set for inheritance and BaseURL resolution.
    let repr = period
        .children()
        .filter(|n| n.has_tag_name("AdaptationSet"))
        .flat_map(|a| a.children().filter(|n| n.has_tag_name("Representation")))
        .find(|n| n.attribute("id") == Some(repr_id.as_str()))
        .ok_or("representation vanished")?;
    let adaptation = repr
        .parent()
        .ok_or("representation has no parent AdaptationSet")?;

    let base = base_url_chain(root, period, adaptation, repr, base);

    // SegmentList: explicit URL list.
    if let Some(list) = repr
        .children()
        .find(|n| n.has_tag_name("SegmentList"))
        .or_else(|| adaptation.children().find(|n| n.has_tag_name("SegmentList")))
    {
        let init = list
            .children()
            .find(|n| n.has_tag_name("Initialization"))
            .and_then(|n| n.attribute("sourceURL"))
            .map(|u| base.join(u).map(|u| u.to_string()))
            .transpose()
            .map_err(|e| format!("init url: {e}"))?;
        let segments = list
            .children()
            .filter(|n| n.has_tag_name("SegmentURL"))
            .filter_map(|n| n.attribute("media"))
            .map(|u| base.join(u).map(|u| u.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("segment url: {e}"))?;
        if segments.is_empty() {
            return Err("SegmentList has no SegmentURL entries".to_owned());
        }
        return Ok(DashRepresentation { id: repr_id, bandwidth, mime, init_url: init, segments });
    }

    // SegmentTemplate: on the representation, else inherited from the
    // adaptation set or period.
    let tmpl = repr
        .children()
        .find(|n| n.has_tag_name("SegmentTemplate"))
        .or_else(|| adaptation.children().find(|n| n.has_tag_name("SegmentTemplate")))
        .or_else(|| period.children().find(|n| n.has_tag_name("SegmentTemplate")))
        .ok_or("no SegmentTemplate or SegmentList")?;

    let media_tmpl = tmpl.attribute("media").ok_or("SegmentTemplate missing media")?;
    let init_tmpl = tmpl.attribute("initialization").map(|t| {
        substitute(t, &repr_id, bandwidth, None, None)
    });
    let init_url = match init_tmpl {
        Some(t) => Some(base.join(&t).map(|u| u.to_string()).map_err(|e| format!("init url: {e}"))?),
        None => None,
    };

    let timescale: f64 = tmpl.attribute("timescale").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let start_number: u64 = tmpl.attribute("startNumber").and_then(|v| v.parse().ok()).unwrap_or(1);

    let mut segments = Vec::new();
    if let Some(timeline) = tmpl.children().find(|n| n.has_tag_name("SegmentTimeline")) {
        let mut t: u64 = 0;
        let mut number = start_number;
        for s in timeline.children().filter(|n| n.has_tag_name("S")) {
            if let Some(tv) = s.attribute("t").and_then(|v| v.parse().ok()) {
                t = tv;
            }
            let d: u64 = s
                .attribute("d")
                .and_then(|v| v.parse().ok())
                .ok_or("SegmentTimeline S missing d")?;
            let r: i64 = s.attribute("r").and_then(|v| v.parse().ok()).unwrap_or(0);
            for _ in 0..=r.max(0) {
                let url = substitute(media_tmpl, &repr_id, bandwidth, Some(number), Some(t));
                segments.push(base.join(&url).map(|u| u.to_string()).map_err(|e| format!("segment url: {e}"))?);
                t += d;
                number += 1;
            }
        }
    } else {
        let seg_dur_attr: f64 = tmpl
            .attribute("duration")
            .and_then(|v| v.parse().ok())
            .ok_or("SegmentTemplate missing duration and SegmentTimeline")?;
        let seg_secs = seg_dur_attr / timescale;
        let period_secs = period_secs.ok_or("no period/mediaPresentationDuration to count segments")?;
        // Epsilon-guarded ceil so float noise doesn't add a phantom segment
        // (the original's MissingDurationCalculator worry).
        let count = ((period_secs / seg_secs) - 1e-6).ceil().max(1.0) as u64;
        for i in 0..count {
            let url = substitute(media_tmpl, &repr_id, bandwidth, Some(start_number + i), None);
            segments.push(base.join(&url).map(|u| u.to_string()).map_err(|e| format!("segment url: {e}"))?);
        }
    }

    Ok(DashRepresentation { id: repr_id, bandwidth, mime, init_url, segments })
}

/// Cumulative BaseURL resolution: MPD → Period → AdaptationSet → Representation.
fn base_url_chain(
    root: Node<'_, '_>,
    period: Node<'_, '_>,
    adaptation: Node<'_, '_>,
    repr: Node<'_, '_>,
    doc_base: &Url,
) -> Url {
    let mut base = doc_base.clone();
    for node in [root, period, adaptation, repr] {
        if let Some(text) = node.children().find(|n| n.has_tag_name("BaseURL")).and_then(|n| n.text())
            && let Ok(joined) = base.join(text.trim())
        {
            base = joined;
        }
    }
    base
}

/// Substitutes `$RepresentationID$`, `$Bandwidth$`, `$Number[..]$`,
/// `$Time$`, and `$$` per the DASH URL template spec.
fn substitute(tmpl: &str, repr_id: &str, bandwidth: u64, number: Option<u64>, time: Option<u64>) -> String {
    let mut out = String::with_capacity(tmpl.len() + 16);
    let mut rest = tmpl;
    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
            continue;
        }
        match after.find('$') {
            Some(end) => {
                let token = &after[..end];
                match token {
                    "RepresentationID" => out.push_str(repr_id),
                    "Bandwidth" => out.push_str(&bandwidth.to_string()),
                    "Time" => {
                        if let Some(t) = time {
                            out.push_str(&t.to_string());
                        }
                    }
                    t if t.starts_with("Number") => {
                        if let Some(n) = number {
                            let width = token
                                .strip_prefix("Number")
                                .and_then(|f| f.strip_prefix('%'))
                                .and_then(|f| f.strip_suffix('d'))
                                .and_then(|f| f.parse::<usize>().ok());
                            match width {
                                Some(w) => out.push_str(&format!("{n:0w$}")),
                                None => out.push_str(&n.to_string()),
                            }
                        }
                    }
                    _ => {} // unknown tokens are dropped
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses ISO 8601 durations (`PT1H2M3.5S`); the date component is ignored.
fn parse_iso_duration(s: &str) -> Option<f64> {
    let rest = s.trim().strip_prefix('P')?;
    let time = match rest.split_once('T') {
        Some((_, t)) => t,
        None => rest,
    };
    let mut total = 0.0f64;
    let mut num = String::new();
    for ch in time.chars() {
        match ch {
            '0'..='9' | '.' | ',' => num.push(if ch == ',' { '.' } else { ch }),
            'H' => {
                total += num.parse::<f64>().ok()? * 3600.0;
                num.clear();
            }
            'M' => {
                total += num.parse::<f64>().ok()? * 60.0;
                num.clear();
            }
            'S' => {
                total += num.parse::<f64>().ok()?;
                num.clear();
            }
            _ => return None,
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="720p" bandwidth="2000000" mimeType="video/mp4">
        <SegmentTemplate timescale="1" duration="4" startNumber="1"
            initialization="init-$RepresentationID$.m4s" media="seg-$Number%05d$.m4s"/>
      </Representation>
      <Representation id="1080p" bandwidth="5000000" mimeType="video/mp4">
        <SegmentTemplate timescale="1" duration="4" startNumber="1"
            initialization="init-$RepresentationID$.m4s" media="seg-$Number%05d$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn picks_highest_bandwidth_and_expands_template() {
        let base = Url::parse("https://cdn.example.com/video/manifest.mpd").unwrap();
        let r = parse_mpd(MPD, &base).unwrap();
        assert_eq!(r.id, "1080p");
        assert_eq!(r.bandwidth, 5000000);
        assert_eq!(r.init_url.as_deref(), Some("https://cdn.example.com/video/init-1080p.m4s"));
        assert_eq!(r.segments.len(), 3);
        assert_eq!(r.segments[0], "https://cdn.example.com/video/seg-00001.m4s");
        assert_eq!(r.segments[2], "https://cdn.example.com/video/seg-00003.m4s");
    }

    #[test]
    fn segment_timeline_uses_time_substitution() {
        let mpd = r#"<MPD><Period duration="PT9S">
            <AdaptationSet mimeType="video/mp4"><Representation id="a" bandwidth="1" mimeType="video/mp4">
              <SegmentTemplate timescale="1" startNumber="0" media="s-$Time$.m4s">
                <SegmentTimeline><S t="0" d="4"/><S d="3" r="1"/></SegmentTimeline>
              </SegmentTemplate>
            </Representation></AdaptationSet>
        </Period></MPD>"#;
        let base = Url::parse("https://x.test/a.mpd").unwrap();
        let r = parse_mpd(mpd, &base).unwrap();
        assert_eq!(r.segments, vec![
            "https://x.test/s-0.m4s",
            "https://x.test/s-4.m4s",
            "https://x.test/s-7.m4s",
        ]);
    }

    #[test]
    fn segment_list_uses_explicit_urls() {
        let mpd = r#"<MPD><Period>
            <AdaptationSet mimeType="video/mp4"><Representation id="a" bandwidth="1" mimeType="video/mp4">
              <SegmentList duration="4">
                <Initialization sourceURL="init.webm"/>
                <SegmentURL media="chunk1.webm"/><SegmentURL media="chunk2.webm"/>
              </SegmentList>
            </Representation></AdaptationSet>
        </Period></MPD>"#;
        let base = Url::parse("https://x.test/a.mpd").unwrap();
        let r = parse_mpd(mpd, &base).unwrap();
        assert_eq!(r.init_url.as_deref(), Some("https://x.test/init.webm"));
        assert_eq!(r.segments.len(), 2);
    }

    #[test]
    fn iso_durations() {
        assert_eq!(parse_iso_duration("PT12S"), Some(12.0));
        assert_eq!(parse_iso_duration("PT1H2M3.5S"), Some(3600.0 + 120.0 + 3.5));
        assert_eq!(parse_iso_duration("PT4M"), Some(240.0));
        assert_eq!(parse_iso_duration("bogus"), None);
    }

    #[test]
    fn template_substitution_forms() {
        assert_eq!(substitute("v$RepresentationID$_$Bandwidth$", "hi", 1234, None, None), "vhi_1234");
        assert_eq!(substitute("$Number%03d$.ts", "r", 1, Some(7), None), "007.ts");
        assert_eq!(substitute("$Number$.ts", "r", 1, Some(7), None), "7.ts");
        assert_eq!(substitute("a$$b", "r", 1, None, None), "a$b");
    }
}
