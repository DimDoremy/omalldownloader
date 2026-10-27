//! MPEG-DASH: the parts of an MPD a download manager needs.
//!
//! A manifest describes a *presentation*: periods, adaptation sets, representations, and
//! for each one a way to build segment URLs. Download managers only want the last, most
//! bandwidth-heavy video representation, fetched as `initialization` + every media
//! segment, concatenated. That is what this module extracts.

use quick_xml::Reader;
use quick_xml::events::Event;
use url::Url;

/// One representation worth downloading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Representation {
    pub id: String,
    pub bandwidth: u64,
    /// Where the segments live, after resolving against the manifest.
    pub base: Url,
    pub initialization: Option<String>,
    /// `$Number$`, `$Time$` or `$RepresentationID$` templates, unresolved.
    pub media: Option<String>,
    pub start_number: u64,
    pub timescale: u64,
    pub duration: Option<u64>,
    /// `<S t= d= r=>` entries from a `SegmentTimeline`, in order.
    pub timeline: Vec<SegmentTime>,
}

/// The `<SegmentTemplate>` fields that a representation inherits.
type Template = (Option<String>, Option<String>, u64, u64, Option<u64>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentTime {
    pub time: u64,
    pub duration: u64,
    pub repeat: u32,
}

impl Representation {
    /// The segment URLs, in playback order, including the initialization segment.
    pub fn segment_urls(&self) -> Vec<Url> {
        let mut out = Vec::new();
        if let Some(init) = &self.initialization
            && let Ok(url) = self.base.join(&self.fill(init, 0, 0))
        {
            out.push(url);
        }
        let Some(media) = &self.media else {
            // No template: the representation's BaseURL is the whole thing.
            out.push(self.base.clone());
            return out;
        };

        if !self.timeline.is_empty() {
            let mut number = self.start_number;
            for entry in &self.timeline {
                let count = entry.repeat.saturating_add(1);
                for repeat in 0..count {
                    let time = entry.time + entry.duration * u64::from(repeat);
                    if let Ok(url) = self.base.join(&self.fill(media, number, time)) {
                        out.push(url);
                    }
                    number += 1;
                }
            }
            return out;
        }

        // No timeline: either a constant `duration` per segment, or nothing that says where
        // the stream ends. In both cases start at `startNumber`; a downloader that gets a
        // 404 keeps what it has, which is how a live or unlisted stream is fetched.
        let _ = self.duration;
        if let Ok(url) = self.base.join(&self.fill(media, self.start_number, 0)) {
            out.push(url);
        }
        out
    }

    /// The URL of segment `number` (1-based as the manifest numbers them), when the
    /// representation uses a `$Number$` template.
    pub fn segment_url(&self, number: u64) -> Option<Url> {
        let media = self.media.as_ref()?;
        self.base.join(&self.fill(media, number, 0)).ok()
    }

    /// The initialization segment, when there is one.
    pub fn initialization_url(&self) -> Option<Url> {
        self.base
            .join(&self.fill(self.initialization.as_deref()?, 0, 0))
            .ok()
    }

    fn fill(&self, template: &str, number: u64, time: u64) -> String {
        template
            .replace("$RepresentationID$", &self.id)
            .replace("$Bandwidth$", &self.bandwidth.to_string())
            .replace("$Number$", &number.to_string())
            .replace("$Time$", &time.to_string())
            .replace("$$", "$")
    }
}

/// Reads a manifest and picks the video representation with the largest bandwidth.
pub fn parse(text: &str, base: &Url) -> Option<Representation> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut found: Vec<Representation> = Vec::new();
    let mut current: Option<Representation> = None;
    let mut pending_template: Option<Template> = None;
    let mut timeline: Vec<SegmentTime> = Vec::new();
    let mut in_timeline = false;
    let mut period_base = base.clone();

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.name().as_ref().to_vec();

                match name.as_slice() {
                    b"BaseURL" => {
                        let text = read_text(&mut reader);
                        if let Some(inner) = text.filter(|t| !t.is_empty())
                            && let Ok(url) = base.join(&inner)
                        {
                            match current.as_mut() {
                                Some(representation) => representation.base = url,
                                None => period_base = url,
                            }
                        }
                    }
                    b"Representation" => {
                        let id = attribute(&event, b"id").unwrap_or_default();
                        let bandwidth = attribute(&event, b"bandwidth")
                            .and_then(|b| b.parse().ok())
                            .unwrap_or(0);
                        current = Some(Representation {
                            id,
                            bandwidth,
                            base: period_base.clone(),
                            initialization: None,
                            media: None,
                            start_number: 1,
                            timescale: 1,
                            duration: None,
                            timeline: Vec::new(),
                        });
                    }
                    b"SegmentTemplate" => {
                        let initialization = attribute(&event, b"initialization");
                        let media = attribute(&event, b"media");
                        let start_number = attribute(&event, b"startNumber")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(1);
                        let timescale = attribute(&event, b"timescale")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(1);
                        let duration = attribute(&event, b"duration").and_then(|n| n.parse().ok());
                        match current.as_mut() {
                            Some(representation) => {
                                representation.initialization = initialization;
                                representation.media = media;
                                representation.start_number = start_number;
                                representation.timescale = timescale;
                                representation.duration = duration;
                            }
                            None => {
                                pending_template = Some((
                                    initialization,
                                    media,
                                    start_number,
                                    timescale,
                                    duration,
                                ));
                            }
                        }
                    }
                    b"SegmentTimeline" => in_timeline = true,
                    b"S" if in_timeline => {
                        let time = attribute(&event, b"t")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0);
                        let duration = attribute(&event, b"d")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0);
                        let repeat = attribute(&event, b"r")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0);
                        timeline.push(SegmentTime {
                            time,
                            duration,
                            repeat,
                        });
                    }
                    _ => {}
                }
            }
            // An empty element is written `<Representation .../>` in most generated
            // manifests: it never gets an End event, so it is completed here.
            Ok(Event::Empty(event)) => {
                let name = event.name().as_ref().to_vec();
                if name.as_slice() == b"Representation" {
                    let mut representation = Representation {
                        id: attribute(&event, b"id").unwrap_or_default(),
                        bandwidth: attribute(&event, b"bandwidth")
                            .and_then(|b| b.parse().ok())
                            .unwrap_or(0),
                        base: period_base.clone(),
                        initialization: None,
                        media: None,
                        start_number: 1,
                        timescale: 1,
                        duration: None,
                        timeline: std::mem::take(&mut timeline),
                    };
                    // The template belongs to the whole adaptation set, so every
                    // representation inherits it rather than consuming it.
                    if let Some((init, media, start, timescale, duration)) =
                        pending_template.clone()
                    {
                        representation.initialization = init;
                        representation.media = media;
                        representation.start_number = start;
                        representation.timescale = timescale;
                        representation.duration = duration;
                    }
                    found.push(representation);
                } else if name.as_slice() == b"SegmentTemplate" {
                    // A template written empty but not yet attached to a representation.
                    if current.is_none() {
                        pending_template = Some((
                            attribute(&event, b"initialization"),
                            attribute(&event, b"media"),
                            attribute(&event, b"startNumber")
                                .and_then(|n| n.parse().ok())
                                .unwrap_or(1),
                            attribute(&event, b"timescale")
                                .and_then(|n| n.parse().ok())
                                .unwrap_or(1),
                            attribute(&event, b"duration").and_then(|n| n.parse().ok()),
                        ));
                    }
                } else if name.as_slice() == b"S" && in_timeline {
                    // `<S .../>` is empty in every manifest that uses a timeline.
                    timeline.push(SegmentTime {
                        time: attribute(&event, b"t")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0),
                        duration: attribute(&event, b"d")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0),
                        repeat: attribute(&event, b"r")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0),
                    });
                }
            }
            Ok(Event::End(event)) => match event.name().as_ref() {
                b"Representation" => {
                    if let Some(mut representation) = current.take() {
                        if let Some((init, media, start, timescale, duration)) =
                            pending_template.clone()
                        {
                            representation.initialization = init;
                            representation.media = media;
                            representation.start_number = start;
                            representation.timescale = timescale;
                            representation.duration = duration;
                        }
                        representation.timeline = std::mem::take(&mut timeline);
                        found.push(representation);
                    }
                }
                b"SegmentTimeline" => in_timeline = false,
                b"AdaptationSet" | b"Period" => pending_template = None,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    found
        .into_iter()
        .filter(|representation| representation.media.is_some() || representation.base != *base)
        .max_by_key(|representation| representation.bandwidth)
}

fn read_text(reader: &mut Reader<&[u8]>) -> Option<String> {
    match reader.read_event() {
        Ok(Event::Text(text)) => Some(text.decode().ok()?.into_owned()),
        _ => None,
    }
}

fn attribute(event: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    event
        .attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        // The attributes this module reads are ids, numbers and URL templates: no entity
        // references to expand, so the raw bytes are the value.
        .map(|a| String::from_utf8_lossy(a.value.as_ref()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    const MANIFEST: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate timescale="1000" initialization="init-$RepresentationID$.mp4"
        media="seg-$RepresentationID$-$Number$.m4s" startNumber="1"/>
      <Representation id="360" bandwidth="500000" width="640" height="360"/>
      <Representation id="1080" bandwidth="4000000" width="1920" height="1080"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn the_highest_bandwidth_representation_is_chosen() {
        let representation = parse(MANIFEST, &url("https://example.com/a.mpd")).unwrap();
        assert_eq!(representation.id, "1080");
        assert_eq!(representation.bandwidth, 4_000_000);
        assert_eq!(
            representation.initialization.as_deref(),
            Some("init-$RepresentationID$.mp4")
        );
    }

    #[test]
    fn a_segment_timeline_becomes_the_segment_list() {
        let manifest = r#"<MPD><Period><AdaptationSet>
          <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4" startNumber="5">
            <SegmentTimeline><S t="0" d="4000" r="2"/><S d="3000"/></SegmentTimeline>
          </SegmentTemplate>
          <Representation id="v" bandwidth="1"/>
        </AdaptationSet></Period></MPD>"#;
        let representation = parse(manifest, &url("https://example.com/dir/a.mpd")).unwrap();
        let urls: Vec<String> = representation
            .segment_urls()
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/dir/init.mp4",
                "https://example.com/dir/seg-5.m4s",
                "https://example.com/dir/seg-6.m4s",
                "https://example.com/dir/seg-7.m4s",
                "https://example.com/dir/seg-8.m4s",
            ]
        );
    }

    #[test]
    fn a_base_url_is_resolved_against_the_manifest() {
        let manifest = r#"<MPD><Period><BaseURL>video/</BaseURL><AdaptationSet>
          <SegmentTemplate media="$Number$.m4s"/>
          <Representation id="v" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        let representation = parse(manifest, &url("https://example.com/x/a.mpd")).unwrap();
        assert_eq!(
            representation.segment_urls()[0].to_string(),
            "https://example.com/x/video/1.m4s"
        );
    }

    #[test]
    fn a_manifest_without_a_representation_is_none() {
        assert!(parse("<MPD></MPD>", &url("https://example.com/a.mpd")).is_none());
    }
}
