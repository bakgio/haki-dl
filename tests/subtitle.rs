use std::error::Error;

use haki_dl::{
    SubtitleCue, SubtitleFormat, WebVttSubtitle, check_stpp_init, check_wvtt_init,
    extract_stpp_from_segments, extract_ttml_documents, extract_wvtt_from_segments,
    format_subtitle, parse_webvtt, write_image_pngs,
};

mod support;
use support::TempDirectory;

#[test]
fn webvtt_parser_formats_shifts_and_strips_class_tags() -> Result<(), Box<dyn Error>> {
    let text = "WEBVTT\r\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:90000\r\n\r\n00:00:02.000 --> 00:00:04.500 align:start\r\n<c.red>Hello</c>\u{200b}\r\n\r\n";
    let mut sub = parse_webvtt(text, 1000)?;

    assert_eq!(sub.mpegts_timestamp, 90_000);
    assert_eq!(sub.cues[0].start_ms, 1000);
    assert_eq!(sub.cues[0].end_ms, 3500);
    assert_eq!(sub.cues[0].payload, "Hello");
    assert_eq!(sub.cues[0].settings, "align:start");

    sub.left_shift_ms(500);
    assert!(
        format_subtitle(&sub, SubtitleFormat::Vtt)
            .contains("00:00:00.500 --> 00:00:03.000 align:start")
    );
    assert!(format_subtitle(&sub, SubtitleFormat::Srt).contains("00:00:00,500 --> 00:00:03,000"));
    Ok(())
}

#[test]
fn webvtt_add_cues_repairs_mpegts_offsets_and_merges_split_cues() {
    let mut base = WebVttSubtitle {
        cues: Vec::new(),
        mpegts_timestamp: 0,
    };
    base.add_cues_from_one(WebVttSubtitle {
        mpegts_timestamp: 90_000,
        cues: vec![SubtitleCue {
            start_ms: 0,
            end_ms: 1000,
            payload: "same".to_string(),
            settings: String::new(),
        }],
    });
    base.add_cues_from_one(WebVttSubtitle {
        mpegts_timestamp: 0,
        cues: vec![SubtitleCue {
            start_ms: 2000,
            end_ms: 3000,
            payload: "same".to_string(),
            settings: String::new(),
        }],
    });

    assert_eq!(base.cues.len(), 1);
    assert_eq!(base.cues[0].start_ms, 1000);
    assert_eq!(base.cues[0].end_ms, 3000);
}

#[tokio::test]
async fn ttml_extraction_handles_text_italics_images_and_png_writing() -> Result<(), Box<dyn Error>>
{
    let temp = TempDirectory::new("subtitle-images")?;
    let documents = vec![r##"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling" xmlns:smpte="http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt" xmlns:xml="http://www.w3.org/XML/1998/namespace"><head><metadata><smpte:image xml:id="img0">iVBORw0KGgo=</smpte:image></metadata></head><body><div><p begin="00:00:01.000" end="00:00:02.000"><span tts:fontStyle="italic">Hello</span><span>World</span></p><p begin="00:00:03.000" end="00:00:04.000" smpte:backgroundImage="#img0"></p></div></body></tt>"##.to_string()];
    let mut sub = extract_ttml_documents(&documents, 0)?;

    assert_eq!(sub.cues.len(), 2);
    assert_eq!(sub.cues[0].payload, format!("<i>Hello</i>{}World", nl()));
    assert_eq!(sub.cues[1].payload, "Base64::iVBORw0KGgo=");

    let images = write_image_pngs(&mut sub, temp.path()).await?;
    assert_eq!(images.len(), 1);
    assert_eq!(
        std::fs::read(&images[0].path)?,
        vec![137, 80, 78, 71, 13, 10, 26, 10]
    );
    assert_eq!(sub.cues[1].payload, "0.png");
    Ok(())
}

#[test]
fn ttml_extraction_preserves_child_order_and_repairs_invalid_text() -> Result<(), Box<dyn Error>> {
    let ordered = vec![r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="00:00:01.000" end="00:00:02.000">Hello <span>World</span> Again</p></div></body></tt>"#.to_string()];
    let ordered_sub = extract_ttml_documents(&ordered, 0)?;

    assert_eq!(ordered_sub.cues.len(), 1);
    assert_eq!(
        ordered_sub.cues[0].payload,
        format!("Hello{}World{}Again", nl(), nl())
    );

    let repaired = vec![r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="00:00:00.000" end="00:00:01.000">1 < 2 tts:fontStyle="italic"</p></div></body></tt>"#.to_string()];
    let repaired_sub = extract_ttml_documents(&repaired, 0)?;

    assert_eq!(repaired_sub.cues.len(), 1);
    assert_eq!(
        repaired_sub.cues[0].payload,
        "1 < 2 tts:fontStyle=\"italic\""
    );
    Ok(())
}

#[test]
fn ttml_extraction_skips_untimed_live_fragments() -> Result<(), Box<dyn Error>> {
    let documents = vec![
        r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><span>metadata only</span></div></body></tt>"#
            .to_string(),
        r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="00:00:01.000" end="00:00:02.000">Caption</p></div></body></tt>"#
            .to_string(),
    ];
    let sub = extract_ttml_documents(&documents, 0)?;

    assert_eq!(sub.cues.len(), 1);
    assert_eq!(sub.cues[0].payload, "Caption");
    Ok(())
}

#[test]
fn stpp_extraction_reads_ttml_from_mp4_media_data() -> Result<(), Box<dyn Error>> {
    let init = box_bytes(b"moov", box_bytes(b"stpp", Vec::new()));
    let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="00:00:00.000" end="00:00:01.000">Caption</p></div></body></tt>"#;
    let media = box_bytes(b"mdat", ttml.as_bytes().to_vec());
    let sub = extract_stpp_from_segments(&[media], 0, 0)?;

    assert!(check_stpp_init(&init));
    assert_eq!(sub.cues.len(), 1);
    assert_eq!(sub.cues[0].payload, "Caption");
    Ok(())
}

#[test]
fn wvtt_init_and_media_segments_extract_cues() -> Result<(), Box<dyn Error>> {
    let init = wvtt_init_segment();
    let timescale = check_wvtt_init(&init)?.ok_or("missing timescale")?;
    let cue_payload = box_bytes(
        b"vttc",
        [
            box_bytes(b"payl", b"Hello".to_vec()),
            box_bytes(b"sttg", b"line:0".to_vec()),
        ]
        .concat(),
    );
    let media = wvtt_media_segment(cue_payload);
    let sub = extract_wvtt_from_segments(&[media], timescale)?;

    assert_eq!(timescale, 1000);
    assert_eq!(sub.cues.len(), 1);
    assert_eq!(sub.cues[0].start_ms, 1000);
    assert_eq!(sub.cues[0].end_ms, 2000);
    assert_eq!(sub.cues[0].payload, "Hello");
    assert_eq!(sub.cues[0].settings, "line:0");
    Ok(())
}

fn wvtt_init_segment() -> Vec<u8> {
    box_bytes(
        b"moov",
        box_bytes(
            b"trak",
            box_bytes(
                b"mdia",
                [
                    full_box(
                        b"mdhd",
                        0,
                        0,
                        [
                            0_u32.to_be_bytes().as_slice(),
                            0_u32.to_be_bytes().as_slice(),
                            1000_u32.to_be_bytes().as_slice(),
                            0_u32.to_be_bytes().as_slice(),
                        ]
                        .concat(),
                    ),
                    box_bytes(b"minf", box_bytes(b"stbl", box_bytes(b"wvtt", Vec::new()))),
                ]
                .concat(),
            ),
        ),
    )
}

fn wvtt_media_segment(cue_payload: Vec<u8>) -> Vec<u8> {
    box_bytes(
        b"moof",
        box_bytes(
            b"traf",
            [
                full_box(b"tfdt", 0, 0, 1000_u32.to_be_bytes().to_vec()),
                full_box(
                    b"tfhd",
                    0,
                    0x000008,
                    [
                        1_u32.to_be_bytes().as_slice(),
                        1000_u32.to_be_bytes().as_slice(),
                    ]
                    .concat(),
                ),
                full_box(
                    b"trun",
                    0,
                    0x000300,
                    [
                        1_u32.to_be_bytes().as_slice(),
                        1000_u32.to_be_bytes().as_slice(),
                        (cue_payload.len() as u32).to_be_bytes().as_slice(),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    )
    .into_iter()
    .chain(box_bytes(b"mdat", cue_payload))
    .collect()
}

fn box_bytes(name: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend(((payload.len() + 8) as u32).to_be_bytes());
    output.extend(name);
    output.extend(payload);
    output
}

fn full_box(name: &[u8; 4], version: u8, flags: u32, payload: Vec<u8>) -> Vec<u8> {
    let mut full_payload = vec![
        version,
        ((flags >> 16) & 0xff) as u8,
        ((flags >> 8) & 0xff) as u8,
        (flags & 0xff) as u8,
    ];
    full_payload.extend(payload);
    box_bytes(name, full_payload)
}

fn nl() -> &'static str {
    if cfg!(windows) { "\r\n" } else { "\n" }
}
