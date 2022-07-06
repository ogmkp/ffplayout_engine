use std::path::Path;

use simplelog::*;

pub mod a_loudnorm;
pub mod ingest_filter;
pub mod v_drawtext;
pub mod v_overlay;

use crate::utils::{get_delta, is_close, Media, PlayoutConfig};

#[derive(Debug, Clone)]
struct Filters {
    audio_chain: Option<String>,
    video_chain: Option<String>,
    audio_map: String,
    video_map: String,
}

impl Filters {
    fn new() -> Self {
        Filters {
            audio_chain: None,
            video_chain: None,
            audio_map: "1:a".to_string(),
            video_map: "0:v".to_string(),
        }
    }

    fn add_filter(&mut self, filter: &str, codec_type: &str) {
        match codec_type {
            "audio" => match &self.audio_chain {
                Some(ac) => {
                    if filter.starts_with(';') || filter.starts_with('[') {
                        self.audio_chain = Some(format!("{ac}{filter}"))
                    } else {
                        self.audio_chain = Some(format!("{ac},{filter}"))
                    }
                }
                None => {
                    if filter.contains("aevalsrc") || filter.contains("anoisesrc") {
                        self.audio_chain = Some(filter.to_string());
                    } else {
                        self.audio_chain = Some(format!("[{}]{filter}", self.audio_map.clone()));
                    }
                    self.audio_map = "[aout1]".to_string();
                }
            },
            "video" => match &self.video_chain {
                Some(vc) => {
                    if filter.starts_with(';') || filter.starts_with('[') {
                        self.video_chain = Some(format!("{vc}{filter}"))
                    } else {
                        self.video_chain = Some(format!("{vc},{filter}"))
                    }
                }
                None => {
                    self.video_chain = Some(format!("[0:v]{filter}"));
                    self.video_map = "[vout1]".to_string();
                }
            },
            _ => (),
        }
    }
}

fn deinterlace(field_order: &Option<String>, chain: &mut Filters) {
    if let Some(order) = field_order {
        if order != "progressive" {
            chain.add_filter("yadif=0:-1:0", "video")
        }
    }
}

fn pad(aspect: f64, chain: &mut Filters, config: &PlayoutConfig) {
    if !is_close(aspect, config.processing.aspect, 0.03) {
        chain.add_filter(
            &format!(
                "pad=max(iw\\,ih*({0}/{1})):ow/({0}/{1}):(ow-iw)/2:(oh-ih)/2",
                config.processing.width, config.processing.height
            ),
            "video",
        )
    }
}

fn fps(fps: f64, chain: &mut Filters, config: &PlayoutConfig) {
    if fps != config.processing.fps {
        chain.add_filter(&format!("fps={}", config.processing.fps), "video")
    }
}

fn scale(v_stream: &ffprobe::Stream, aspect: f64, chain: &mut Filters, config: &PlayoutConfig) {
    // width: i64, height: i64
    if let (Some(w), Some(h)) = (v_stream.width, v_stream.height) {
        if w != config.processing.width || h != config.processing.height {
            chain.add_filter(
                &format!(
                    "scale={}:{}",
                    config.processing.width, config.processing.height
                ),
                "video",
            )
        }

        if !is_close(aspect, config.processing.aspect, 0.03) {
            chain.add_filter(&format!("setdar=dar={}", config.processing.aspect), "video")
        }
    } else {
        chain.add_filter(
            &format!(
                "scale={}:{}",
                config.processing.width, config.processing.height
            ),
            "video",
        );
        chain.add_filter(&format!("setdar=dar={}", config.processing.aspect), "video")
    }
}

fn fade(node: &mut Media, chain: &mut Filters, codec_type: &str) {
    let mut t = "";

    if codec_type == "audio" {
        t = "a"
    }

    if node.seek > 0.0 {
        chain.add_filter(&format!("{t}fade=in:st=0:d=0.5"), codec_type)
    }

    if node.out != node.duration && node.out - node.seek - 1.0 > 0.0 {
        chain.add_filter(
            &format!("{t}fade=out:st={}:d=1.0", (node.out - node.seek - 1.0)),
            codec_type,
        )
    }
}

fn overlay(node: &mut Media, chain: &mut Filters, config: &PlayoutConfig) {
    if config.processing.add_logo
        && Path::new(&config.processing.logo).is_file()
        && &node.category != "advertisement"
    {
        let mut logo_chain = v_overlay::filter_node(config, false);

        if node.last_ad.unwrap() {
            logo_chain.push_str(",fade=in:st=0:d=1.0:alpha=1")
        }

        if node.next_ad.unwrap() {
            logo_chain.push_str(
                format!(",fade=out:st={}:d=1.0:alpha=1", node.out - node.seek - 1.0).as_str(),
            )
        }

        logo_chain
            .push_str(format!("[l];[v][l]{}:shortest=1", config.processing.logo_filter).as_str());

        chain.add_filter(&logo_chain, "video");
    }
}

fn extend_video(node: &mut Media, chain: &mut Filters) {
    if let Some(duration) = node
        .probe
        .as_ref()
        .and_then(|p| p.video_streams.as_ref())
        .and_then(|v| v[0].duration.as_ref())
    {
        let duration_float = duration.clone().parse::<f64>().unwrap();

        if node.out - node.seek > duration_float - node.seek + 0.1 {
            chain.add_filter(
                &format!(
                    "tpad=stop_mode=add:stop_duration={}",
                    (node.out - node.seek) - (duration_float - node.seek)
                ),
                "video",
            )
        }
    }
}

/// add drawtext filter for lower thirds messages
fn add_text(node: &mut Media, chain: &mut Filters, config: &PlayoutConfig) {
    if config.text.add_text
        && (config.text.text_from_filename || config.out.mode.to_lowercase() == "hls")
    {
        let filter = v_drawtext::filter_node(config, node);

        chain.add_filter(&filter, "video");

        if let Some(filters) = &chain.video_chain {
            for (i, f) in filters.split(',').enumerate() {
                if f.contains("drawtext") && !config.text.text_from_filename {
                    debug!("drawtext node is on index: <yellow>{i}</>");
                    break;
                }
            }
        }
    }
}

fn add_audio(node: &mut Media, chain: &mut Filters) {
    if node
        .probe
        .as_ref()
        .and_then(|p| p.audio_streams.as_ref())
        .unwrap_or(&vec![])
        .is_empty()
    {
        warn!("Clip <b><magenta>{}</></b> has no audio!", node.source);
        let audio = format!(
            "aevalsrc=0:channel_layout=stereo:duration={}:sample_rate=48000",
            node.out - node.seek
        );
        chain.add_filter(&audio, "audio");
    }
}

fn extend_audio(node: &mut Media, chain: &mut Filters) {
    if let Some(duration) = node
        .probe
        .as_ref()
        .and_then(|p| p.audio_streams.as_ref())
        .and_then(|a| a[0].duration.as_ref())
    {
        let duration_float = duration.clone().parse::<f64>().unwrap();

        if node.out - node.seek > duration_float - node.seek + 0.1 {
            chain.add_filter(&format!("apad=whole_dur={}", node.out - node.seek), "audio")
        }
    }
}

/// Add single pass loudnorm filter to audio line.
fn add_loudnorm(node: &mut Media, chain: &mut Filters, config: &PlayoutConfig) {
    if config.processing.add_loudnorm
        && !node
            .probe
            .as_ref()
            .and_then(|p| p.audio_streams.as_ref())
            .unwrap_or(&vec![])
            .is_empty()
    {
        let loud_filter = a_loudnorm::filter_node(config);
        chain.add_filter(&loud_filter, "audio");
    }
}

fn audio_volume(chain: &mut Filters, config: &PlayoutConfig) {
    if config.processing.volume != 1.0 {
        chain.add_filter(&format!("volume={}", config.processing.volume), "audio")
    }
}

fn aspect_calc(aspect_string: &Option<String>, config: &PlayoutConfig) -> f64 {
    let mut source_aspect = config.processing.aspect;

    if let Some(aspect) = aspect_string {
        let aspect_vec: Vec<&str> = aspect.split(':').collect();
        let w: f64 = aspect_vec[0].parse().unwrap();
        let h: f64 = aspect_vec[1].parse().unwrap();
        source_aspect = w as f64 / h as f64;
    }

    source_aspect
}

fn fps_calc(r_frame_rate: &str) -> f64 {
    let frame_rate_vec = r_frame_rate.split('/').collect::<Vec<&str>>();
    let rate: f64 = frame_rate_vec[0].parse().unwrap();
    let factor: f64 = frame_rate_vec[1].parse().unwrap();
    let fps: f64 = rate / factor;

    fps
}

/// This realtime filter is important for HLS output to stay in sync.
fn realtime_filter(
    node: &mut Media,
    chain: &mut Filters,
    config: &PlayoutConfig,
    codec_type: &str,
) {
    let mut t = "";

    if codec_type == "audio" {
        t = "a"
    }

    if &config.out.mode.to_lowercase() == "hls" {
        let mut speed_filter = format!("{t}realtime=speed=1");
        let (delta, _) = get_delta(config, &node.begin.unwrap());
        let duration = node.out - node.seek;

        if delta < 0.0 {
            let speed = duration / (duration + delta);

            if speed > 0.0 && speed < 1.1 && delta < config.general.stop_threshold {
                speed_filter = format!("{t}realtime=speed={speed}");
            }
        }

        chain.add_filter(&speed_filter, codec_type);
    }
}

pub fn filter_chains(config: &PlayoutConfig, node: &mut Media) -> Vec<String> {
    let mut filters = Filters::new();

    if let Some(probe) = node.probe.as_ref() {
        if probe.audio_streams.is_some() {
            filters.audio_map = "0:a".to_string();
        }

        if let Some(v_streams) = &probe.video_streams.as_ref() {
            let v_stream = &v_streams[0];

            let aspect = aspect_calc(&v_stream.display_aspect_ratio, config);
            let frame_per_sec = fps_calc(&v_stream.r_frame_rate);

            deinterlace(&v_stream.field_order, &mut filters);
            pad(aspect, &mut filters, config);
            fps(frame_per_sec, &mut filters, config);
            scale(v_stream, aspect, &mut filters, config);
        }

        extend_video(node, &mut filters);

        add_audio(node, &mut filters);
        extend_audio(node, &mut filters);
    }

    add_text(node, &mut filters, config);
    fade(node, &mut filters, "video");
    overlay(node, &mut filters, config);
    realtime_filter(node, &mut filters, config, "video");

    add_loudnorm(node, &mut filters, config);
    fade(node, &mut filters, "audio");
    audio_volume(&mut filters, config);
    realtime_filter(node, &mut filters, config, "audio");

    let mut filter_cmd = vec![];
    let mut filter_str: String = String::new();
    let mut filter_map: Vec<String> = vec![];

    if let Some(v_filters) = filters.video_chain {
        filter_str.push_str(v_filters.as_str());
        filter_str.push_str(filters.video_map.clone().as_str());
        filter_map.append(&mut vec!["-map".to_string(), filters.video_map]);
    } else {
        filter_map.append(&mut vec!["-map".to_string(), "0:v".to_string()]);
    }

    if let Some(a_filters) = filters.audio_chain {
        if filter_str.len() > 10 {
            filter_str.push(';')
        }
        filter_str.push_str(a_filters.as_str());
        filter_str.push_str(filters.audio_map.clone().as_str());
        filter_map.append(&mut vec!["-map".to_string(), filters.audio_map]);
    } else {
        filter_map.append(&mut vec!["-map".to_string(), filters.audio_map]);
    }

    if filter_str.len() > 10 {
        filter_cmd.push("-filter_complex".to_string());
        filter_cmd.push(filter_str);
    }

    filter_cmd.append(&mut filter_map);

    filter_cmd
}