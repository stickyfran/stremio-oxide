use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RankingMode {
    #[default]
    Smart,
    Quality,
    Smallest,
    Seeders,
    Original,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DebridAvailability {
    Cached,
    Uncached,
    Unknown,
    ProviderUnavailable,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RankInput {
    pub id: String,
    pub name: String,
    pub description: String,
    pub addon: String,
    pub original_index: usize,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub debrid: Option<DebridAvailability>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasonKind {
    Positive,
    Negative,
    Neutral,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScoreReason {
    pub kind: ReasonKind,
    pub label: String,
    pub points: i32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ParsedStream {
    pub quality_height: Option<u16>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub hdr: bool,
    pub dolby_vision: bool,
    pub audio: Option<String>,
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    pub suspicious: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RankedStream {
    pub input: RankInput,
    pub parsed: ParsedStream,
    pub score: i32,
    pub reasons: Vec<ScoreReason>,
    pub filtered: bool,
    #[serde(default)]
    pub effective_size: Option<u64>,
    #[serde(default)]
    pub effective_seeders: Option<u32>,
}

pub fn rank_streams(
    inputs: impl IntoIterator<Item = RankInput>,
    mode: RankingMode,
    show_filtered: bool,
) -> Vec<RankedStream> {
    let mut streams = inputs
        .into_iter()
        .map(rank_one)
        .filter(|stream| show_filtered || !stream.filtered)
        .collect::<Vec<_>>();

    match mode {
        RankingMode::Original => streams.sort_by_key(|stream| stream.input.original_index),
        RankingMode::Quality => streams.sort_by(|left, right| {
            let left_cached = left.input.debrid == Some(DebridAvailability::Cached);
            let right_cached = right.input.debrid == Some(DebridAvailability::Cached);
            right_cached
                .cmp(&left_cached)
                .then_with(|| right.parsed.quality_height.cmp(&left.parsed.quality_height))
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
        RankingMode::Smallest => streams.sort_by(|left, right| {
            let left_cached = left.input.debrid == Some(DebridAvailability::Cached);
            let right_cached = right.input.debrid == Some(DebridAvailability::Cached);
            let left_size = left.effective_size;
            let right_size = right.effective_size;
            right_cached
                .cmp(&left_cached)
                .then_with(|| {
                    left_size
                        .unwrap_or(u64::MAX)
                        .cmp(&right_size.unwrap_or(u64::MAX))
                })
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
        RankingMode::Seeders => streams.sort_by(|left, right| {
            let left_cached = left.input.debrid == Some(DebridAvailability::Cached);
            let right_cached = right.input.debrid == Some(DebridAvailability::Cached);
            let left_seeds = left.effective_seeders;
            let right_seeds = right.effective_seeders;
            right_cached
                .cmp(&left_cached)
                .then_with(|| {
                    right_seeds
                        .unwrap_or_default()
                        .cmp(&left_seeds.unwrap_or_default())
                })
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
        RankingMode::Smart => streams.sort_by(|left, right| {
            let left_cached = left.input.debrid == Some(DebridAvailability::Cached);
            let right_cached = right.input.debrid == Some(DebridAvailability::Cached);
            right_cached
                .cmp(&left_cached)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| compare_quality(right, left))
                .then_with(|| left.input.original_index.cmp(&right.input.original_index))
        }),
    }
    streams
}

fn compare_quality(left: &RankedStream, right: &RankedStream) -> Ordering {
    left.parsed.quality_height.cmp(&right.parsed.quality_height)
}

fn rank_one(mut input: RankInput) -> RankedStream {
    if input.debrid.is_none() {
        input.debrid = parse_debrid_availability_from_text(&input.description)
            .or_else(|| parse_debrid_availability_from_text(&input.name));
    }
    let parsed = parse_stream(&input);
    let mut score = 0;
    let mut reasons = Vec::new();
    let mut reason = |kind, label: String, points| {
        score += points;
        reasons.push(ScoreReason {
            kind,
            label,
            points,
        });
    };

    if let Some(height) = parsed.quality_height {
        let points = match height {
            2160.. => 50,
            1440..=2159 => 42,
            1080..=1439 => 34,
            720..=1079 => 22,
            480..=719 => 10,
            _ => 2,
        };
        reason(ReasonKind::Positive, format!("{height}p video"), points);
    }
    if let Some(source) = parsed.source.as_deref() {
        let points = match source {
            "BluRay" | "REMUX" => 18,
            "WEB-DL" => 13,
            "WEBRip" => 8,
            "HDTV" => 4,
            "CAM" | "TS" => -35,
            _ => 0,
        };
        reason(
            if points >= 0 {
                ReasonKind::Positive
            } else {
                ReasonKind::Negative
            },
            format!("{source} source"),
            points,
        );
    }
    if let Some(codec) = parsed.codec.as_deref() {
        let points = match codec {
            "AV1" => 10,
            "HEVC" => 8,
            "H.264" => 4,
            _ => 0,
        };
        reason(ReasonKind::Positive, format!("{codec} codec"), points);
    }
    if parsed.dolby_vision {
        reason(ReasonKind::Positive, "Dolby Vision".to_owned(), 8);
    } else if parsed.hdr {
        reason(ReasonKind::Positive, "HDR video".to_owned(), 6);
    }
    if let Some(audio) = parsed.audio.as_deref() {
        let points = if audio.contains("Atmos") || audio.contains("TrueHD") {
            8
        } else if audio.contains("DTS") {
            6
        } else {
            2
        };
        reason(ReasonKind::Positive, format!("{audio} audio"), points);
    }
    let effective_seeders = input
        .seeders
        .or_else(|| parse_seeders_from_text(&input.description))
        .or_else(|| parse_seeders_from_text(&input.name));
    if let Some(seeders) = effective_seeders {
        let points = match seeders {
            500.. => 18,
            100..=499 => 14,
            25..=99 => 9,
            5..=24 => 4,
            1..=4 => 0,
            0 => -18,
        };
        reason(
            if points >= 0 {
                ReasonKind::Positive
            } else {
                ReasonKind::Negative
            },
            format!("{seeders} seeders"),
            points,
        );
    }
    let effective_size = input
        .size_bytes
        .or_else(|| parse_size_bytes_from_text(&input.description))
        .or_else(|| parse_size_bytes_from_text(&input.name));
    if let Some(size) = effective_size {
        let gib = size as f64 / 1_073_741_824.0;
        let points = if gib <= 0.15 {
            -10
        } else if gib <= 8.0 {
            6
        } else if gib >= 80.0 {
            -12
        } else {
            0
        };
        reason(
            if points >= 0 {
                ReasonKind::Positive
            } else {
                ReasonKind::Negative
            },
            format!("{gib:.1} GiB"),
            points,
        );
    }
    match input.debrid {
        Some(DebridAvailability::Cached) => reason(
            ReasonKind::Positive,
            "Instant on debrid provider".to_owned(),
            100,
        ),
        Some(DebridAvailability::Uncached) => reason(
            ReasonKind::Negative,
            "Not cached on debrid provider".to_owned(),
            -50,
        ),
        Some(DebridAvailability::Unknown | DebridAvailability::ProviderUnavailable) | None => {
            reason(
                ReasonKind::Neutral,
                "Debrid availability unknown".to_owned(),
                0,
            )
        }
    }
    if parsed.suspicious {
        reason(
            ReasonKind::Negative,
            "Potential fake, password, or scam result".to_owned(),
            -1_000,
        );
    }

    RankedStream {
        input,
        parsed: parsed.clone(),
        score,
        reasons,
        filtered: parsed.suspicious,
        effective_size,
        effective_seeders,
    }
}

pub fn parse_stream(input: &RankInput) -> ParsedStream {
    let combined = format!("{} {}", input.name, input.description);
    let upper = combined.to_ascii_uppercase();
    let quality_height = [4320_u16, 2160, 1440, 1080, 720, 576, 540, 480, 360]
        .into_iter()
        .find(|height| {
            let marker = match height {
                4320 => "4320P",
                2160 => "2160P",
                1440 => "1440P",
                1080 => "1080P",
                720 => "720P",
                576 => "576P",
                540 => "540P",
                480 => "480P",
                360 => "360P",
                _ => "",
            };
            upper.contains(marker)
                || (*height == 2160 && upper.contains("4K"))
                || (*height == 4320 && upper.contains("8K"))
        });

    let has_token = |token: &str| -> bool {
        upper
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w == token)
    };

    let source = if upper.contains("REMUX") {
        Some("REMUX".to_owned())
    } else if upper.contains("BLU-RAY") || upper.contains("BLURAY") {
        Some("BluRay".to_owned())
    } else if upper.contains("WEB-DL") || upper.contains("WEBDL") {
        Some("WEB-DL".to_owned())
    } else if upper.contains("WEBRIP") {
        Some("WEBRip".to_owned())
    } else if upper.contains("HDTV") {
        Some("HDTV".to_owned())
    } else if has_token("CAM") || has_token("CAMRIP") || has_token("HDCAM") {
        Some("CAM".to_owned())
    } else if has_token("TELESYNC") || has_token("TS") || has_token("HDTS") {
        Some("TS".to_owned())
    } else {
        None
    };
    let codec = [
        (&["AV1", "AV01"][..], "AV1"),
        (&["HEVC", "H265", "H.265", "X265"][..], "HEVC"),
        (&["H264", "H.264", "X264", "AVC"][..], "H.264"),
    ]
    .into_iter()
    .find_map(|(markers, normalized)| {
        markers
            .iter()
            .any(|marker| upper.contains(marker))
            .then(|| normalized.to_owned())
    });
    let dolby_vision = upper.contains("DOLBY VISION") || upper.contains("DOVI") || has_token("DV");
    let hdr = dolby_vision
        || upper.contains("HDR10+")
        || upper.contains("HDR10")
        || upper.contains("HLG")
        || has_token("HDR");
    let audio = [
        "TRUEHD ATMOS",
        "ATMOS",
        "TRUEHD",
        "DTS-HD",
        "DTS",
        "EAC3",
        "DDP",
        "AAC",
    ]
    .into_iter()
    .find(|marker| upper.contains(marker))
    .map(ToOwned::to_owned);
    let languages = [
        ("ENGLISH", "English"),
        ("HINDI", "Hindi"),
        ("PORTUGUESE", "Portuguese"),
        ("ARABIC", "Arabic"),
        ("SPANISH", "Spanish"),
        ("FRENCH", "French"),
        ("JAPANESE", "Japanese"),
        ("MULTI", "Multi"),
    ]
    .into_iter()
    .filter(|(marker, _)| upper.contains(marker))
    .map(|(_, language)| language.to_owned())
    .collect();
    let release_group = combined
        .split_whitespace()
        .rev()
        .find_map(|token| token.strip_prefix('-'))
        .filter(|group| !group.is_empty() && group.len() <= 24)
        .map(ToOwned::to_owned);
    let suspicious = [
        "PASSWORD",
        "PASSW0RD",
        "CLICK HERE",
        "FREE BITCOIN",
        "INSTALL CODEC",
        "EXE INCLUDED",
        "SCAM",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
        || has_token("FAKE");

    ParsedStream {
        quality_height,
        source,
        codec,
        hdr,
        dolby_vision,
        audio,
        languages,
        release_group,
        suspicious,
    }
}

pub fn parse_debrid_availability_from_text(text: &str) -> Option<DebridAvailability> {
    let upper = text.to_ascii_uppercase();
    if upper.contains("DOWNLOAD]")
        || upper.contains("DOWNLOAD)")
        || upper.contains("RD DOWNLOAD")
        || upper.contains("PM DOWNLOAD")
        || upper.contains("AD DOWNLOAD")
        || upper.contains("TB DOWNLOAD")
        || upper.contains("OC DOWNLOAD")
        || upper.contains("UNCACHED")
    {
        return Some(DebridAvailability::Uncached);
    }
    if upper.contains("[RD+]")
        || upper.contains("[PM+]")
        || upper.contains("[AD+]")
        || upper.contains("[TB+]")
        || upper.contains("[OC+]")
        || upper.contains("[DL+]")
        || upper.contains("[ED+]")
        || upper.contains("RD+")
        || upper.contains("PM+")
        || upper.contains("AD+")
        || upper.contains("TB+")
        || text.contains('⚡')
        || upper.contains("DEBRID CACHED")
    {
        return Some(DebridAvailability::Cached);
    }
    None
}

pub fn parse_size_bytes_from_text(text: &str) -> Option<u64> {
    for marker in ['💾', '📦'] {
        if let Some(idx) = text.find(marker) {
            let after = &text[idx + marker.len_utf8()..];
            if let Some(bytes) = parse_first_size(after) {
                return Some(bytes);
            }
        }
    }
    parse_first_size(text)
}

fn parse_decimal_number(raw: &str) -> Option<f64> {
    let normalized = if !raw.contains('.') && raw.matches(',').count() == 1 {
        let after_comma = raw.split(',').nth(1).unwrap_or("");
        if (1..=3).contains(&after_comma.len()) && after_comma.chars().all(|c| c.is_ascii_digit()) {
            raw.replace(',', ".")
        } else {
            raw.to_owned()
        }
    } else {
        raw.to_owned()
    };
    let num = normalized.parse::<f64>().ok()?;
    (num.is_finite() && num > 0.0).then_some(num)
}

fn parse_first_size(text: &str) -> Option<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let clean_word =
            word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != ',');
        if let Some(bytes) = parse_size_token(clean_word) {
            return Some(bytes);
        }
        if let Some(num) = parse_decimal_number(clean_word)
            && let Some(unit) = words.get(i + 1)
            && let Some(multiplier) =
                unit_multiplier(unit.trim_matches(|c: char| !c.is_ascii_alphabetic()))
        {
            return Some((num * multiplier) as u64);
        }
    }
    None
}

fn unit_multiplier(unit: &str) -> Option<f64> {
    match unit.to_ascii_uppercase().as_str() {
        "TB" | "TIB" => Some(1_099_511_627_776.0),
        "GB" | "GIB" => Some(1_073_741_824.0),
        "MB" | "MIB" => Some(1_048_576.0),
        "KB" | "KIB" => Some(1_024.0),
        "B" | "BYTES" => Some(1.0),
        _ => None,
    }
}

fn parse_size_token(token: &str) -> Option<u64> {
    let upper = token.to_ascii_uppercase();
    for (unit, mult) in [
        ("TIB", 1_099_511_627_776.0),
        ("TB", 1_099_511_627_776.0),
        ("GIB", 1_073_741_824.0),
        ("GB", 1_073_741_824.0),
        ("MIB", 1_048_576.0),
        ("MB", 1_048_576.0),
        ("KIB", 1_024.0),
        ("KB", 1_024.0),
    ] {
        if let Some(num_part) = upper.strip_suffix(unit)
            && let Some(num) = parse_decimal_number(num_part)
        {
            return Some((num * mult) as u64);
        }
    }
    None
}

pub fn parse_seeders_from_text(text: &str) -> Option<u32> {
    if let Some(idx) = text.find('👤') {
        let after = &text[idx + '👤'.len_utf8()..];
        let num_str: String = after
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(val) = num_str.parse::<u32>() {
            return Some(val);
        }
    }
    let upper = text.to_ascii_uppercase();
    for part in upper.split(['\n', '/', '|', '•', ',', ';']) {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        for (i, token) in tokens.iter().enumerate() {
            let clean = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if matches!(clean, "S" | "SEEDS" | "SEEDERS")
                && let Some(next) = tokens.get(i + 1)
                && let Ok(val) = next
                    .trim_matches(|c: char| !c.is_ascii_digit())
                    .parse::<u32>()
            {
                return Some(val);
            }
            if clean.ends_with("SEEDS") || clean.ends_with("SEEDERS") {
                let num_part = clean.trim_end_matches(|c: char| c.is_ascii_alphabetic());
                if let Ok(val) = num_part.parse::<u32>() {
                    return Some(val);
                }
            }
            if let Ok(val) = clean.parse::<u32>()
                && tokens.get(i + 1).is_some_and(|next| {
                    matches!(
                        next.trim_matches(|c: char| !c.is_ascii_alphabetic()),
                        "SEEDS" | "SEEDERS" | "PEERS" | "S"
                    )
                })
            {
                return Some(val);
            }
        }
    }
    None
}

pub fn format_size(bytes: u64) -> String {
    let gib = bytes as f64 / 1_073_741_824.0;
    if gib >= 1.0 {
        format!("{:.2} GB", gib)
    } else {
        let mib = bytes as f64 / 1_048_576.0;
        format!("{:.1} MB", mib)
    }
}

pub fn format_stream_description(
    name: &str,
    description: &str,
    size_bytes: Option<u64>,
    seeders: Option<u32>,
) -> String {
    let seeders = seeders
        .or_else(|| parse_seeders_from_text(description))
        .or_else(|| parse_seeders_from_text(name));
    let size_bytes = size_bytes
        .or_else(|| parse_size_bytes_from_text(description))
        .or_else(|| parse_size_bytes_from_text(name));

    let raw_lines: Vec<&str> = description
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    if raw_lines.is_empty() {
        let mut stats = Vec::new();
        if let Some(s) = seeders {
            stats.push(format!("👤 {s}"));
        }
        if let Some(sz) = size_bytes {
            stats.push(format!("💾 {}", format_size(sz)));
        }
        return stats.join(" ");
    }

    let mut title_lines = Vec::new();
    let mut meta_lines = Vec::new();

    for line in &raw_lines {
        let is_meta = line.contains('👤')
            || line.contains('💾')
            || line.contains('⚙')
            || line.contains('≡')
            || line.contains('🗓')
            || line.contains('📺')
            || line.contains('⚡')
            || line.to_ascii_uppercase().contains("SEEDERS")
            || line.to_ascii_uppercase().contains("SEEDS");
        if is_meta {
            meta_lines.push((*line).to_string());
        } else {
            title_lines.push((*line).to_string());
        }
    }

    let title = if title_lines.is_empty() {
        raw_lines[0].to_string()
    } else {
        title_lines[0].clone()
    };

    let mut meta_line = meta_lines.join(" ");
    let meta_upper = meta_line.to_ascii_uppercase();
    if let Some(s) = seeders
        && !meta_line.contains('👤')
        && !meta_upper.contains("SEED")
    {
        if !meta_line.is_empty() {
            meta_line.push(' ');
        }
        meta_line.push_str(&format!("👤 {s}"));
    }
    if let Some(sz) = size_bytes
        && !meta_line.contains('💾')
        && !meta_upper.contains("GB")
        && !meta_upper.contains("MB")
        && !meta_upper.contains("GIB")
        && !meta_upper.contains("MIB")
    {
        if !meta_line.is_empty() {
            meta_line.push(' ');
        }
        meta_line.push_str(&format!("💾 {}", format_size(sz)));
    }

    if meta_line.is_empty() {
        title
    } else {
        format!("{title}\n{meta_line}")
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamPresentation {
    /// Clean release title without duplicate emoji stat lines.
    pub title: String,
    /// Compact resolution label: "8K", "4K", "1440p", "1080p", "720p", "480p", etc.
    pub resolution_label: String,
    /// Short, never-elided debrid/transport badge: "RD+", "AD+", "PM+", "TB+",
    /// "OC+", "DL+", "ED+", "PK+", "RD ↓", ..., "Debrid+", "No caché", "P2P", or "HTTP".
    pub debrid_badge: String,
    pub is_debrid_cached: bool,
    pub is_debrid_uncached: bool,
    /// Clean addon/source badge (e.g. "Torrentio", "Comet", "MediaFusion") with
    /// redundant suffixes trimmed.
    pub source_badge: String,
    /// Formatted file size (e.g. "12.40 GB", "850.5 MB") or "" if unavailable.
    pub size_label: String,
    /// Compact seeders string (e.g. "👤 142") or "".
    pub seeders_label: String,
    /// Concise technical & language summary (e.g. "REMUX · DV · HEVC · Atmos · Spanish").
    pub tech_summary: String,
}

const DEBRID_PREFIXES: [&str; 8] = ["RD", "AD", "PM", "TB", "OC", "DL", "ED", "PK"];

pub fn parse_debrid_badge(
    name: &str,
    description: &str,
    availability: Option<DebridAvailability>,
    is_torrent: bool,
) -> (String, bool, bool) {
    let combined = format!("{name}\n{description}");
    let upper = combined.to_ascii_uppercase();

    for prefix in DEBRID_PREFIXES {
        let uncached_bracket = format!("[{prefix}]");
        let uncached_dl = format!("{prefix} DOWNLOAD");
        let uncached_paren = format!("({prefix})");
        let uncached_hourglass = format!("{prefix} ⏳");
        if upper.contains(&uncached_dl)
            || upper.contains(&uncached_bracket)
            || upper.contains(&uncached_paren)
            || upper.contains(&uncached_hourglass)
            || (upper.contains("UNCACHED") && upper.contains(prefix))
        {
            return (format!("{prefix} ↓"), false, true);
        }
    }

    for prefix in DEBRID_PREFIXES {
        let plus = format!("{prefix}+");
        let bolt = format!("{prefix}⚡");
        let bracket_bolt = format!("[{prefix}⚡]");
        if upper.contains(&plus) || combined.contains(&bolt) || combined.contains(&bracket_bolt) {
            return (format!("{prefix}+"), true, false);
        }
    }

    match availability {
        Some(DebridAvailability::Cached) => ("Debrid+".to_owned(), true, false),
        Some(DebridAvailability::Uncached) => ("No caché".to_owned(), false, true),
        Some(DebridAvailability::Unknown | DebridAvailability::ProviderUnavailable) | None => {
            if is_torrent {
                ("P2P".to_owned(), false, false)
            } else {
                ("HTTP".to_owned(), false, false)
            }
        }
    }
}

pub fn clean_provider_badge(provider: &str, name: &str) -> String {
    let raw = if !provider.trim().is_empty() && !provider.eq_ignore_ascii_case("Addon") {
        provider.trim()
    } else {
        name.lines().next().unwrap_or(name).trim()
    };

    let mut cleaned = raw
        .replace(['⚡', '📺', '⚙', '💾', '👤', '📦'], " ")
        .trim()
        .to_owned();

    for marker in [" | ElfHosted", " (ElfHosted)", " - ElfHosted", "| ElfHosted"] {
        if let Some(idx) = cleaned.to_ascii_lowercase().find(&marker.to_ascii_lowercase()) {
            cleaned.truncate(idx);
        }
    }

    for prefix in DEBRID_PREFIXES {
        for tag in [
            format!("[{prefix}+]"),
            format!("[{prefix}]"),
            format!("({prefix}+)"),
            format!("({prefix})"),
            format!("{prefix}+"),
            format!("{prefix} download"),
            format!("{prefix} DOWNLOAD"),
        ] {
            cleaned = cleaned.replace(&tag, " ");
        }
    }

    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| matches!(c, '|' | '-' | '·' | '[' | ']')).trim();
    if trimmed.is_empty() {
        raw.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn is_meta_stat_line(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    line.contains('👤')
        || line.contains('💾')
        || line.contains('📦')
        || line.contains('⚙')
        || line.contains('≡')
        || line.contains('🗓')
        || line.contains('📺')
        || line.contains('⚡')
        || upper.contains("SEEDERS")
        || upper.contains("SEEDS")
}

pub fn extract_stream_presentation(
    input: &RankInput,
    parsed: &ParsedStream,
    effective_size: Option<u64>,
    effective_seeders: Option<u32>,
    provider_name: &str,
    is_torrent: bool,
) -> StreamPresentation {
    let title = input
        .description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_meta_stat_line(line))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            let fallback = input
                .name
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if fallback.is_empty() {
                "Stream".to_owned()
            } else {
                fallback
            }
        });

    let resolution_label = match parsed.quality_height {
        Some(h) if h >= 4320 => "8K".to_owned(),
        Some(h) if h >= 2160 => "4K".to_owned(),
        Some(1440) => "1440p".to_owned(),
        Some(1080) => "1080p".to_owned(),
        Some(720) => "720p".to_owned(),
        Some(h) if h > 0 => format!("{h}p"),
        _ => String::new(),
    };

    let (debrid_badge, is_debrid_cached, is_debrid_uncached) =
        parse_debrid_badge(&input.name, &input.description, input.debrid, is_torrent);
    let source_badge = clean_provider_badge(provider_name, &input.name);
    let size_label = effective_size
        .filter(|&bytes| bytes > 0)
        .map(format_size)
        .unwrap_or_default();
    let seeders_label = effective_seeders
        .filter(|&seeders| seeders > 0)
        .map(|seeders| format!("👤 {seeders}"))
        .unwrap_or_default();

    let combined_upper = format!("{} {}", input.name, input.description).to_ascii_uppercase();
    let mut specs = Vec::new();
    if let Some(src) = &parsed.source {
        specs.push(src.clone());
    }
    if parsed.dolby_vision {
        specs.push("DV".to_owned());
    } else if parsed.hdr {
        if combined_upper.contains("HDR10+") {
            specs.push("HDR10+".to_owned());
        } else {
            specs.push("HDR".to_owned());
        }
    }
    if let Some(codec) = &parsed.codec {
        specs.push(codec.clone());
    }
    if let Some(audio) = &parsed.audio {
        specs.push(audio.clone());
    }
    for lang in parsed.languages.iter().take(3) {
        specs.push(lang.clone());
    }
    let tech_summary = specs.join(" · ");

    StreamPresentation {
        title,
        resolution_label,
        debrid_badge,
        is_debrid_cached,
        is_debrid_uncached,
        source_badge,
        size_label,
        seeders_label,
        tech_summary,
    }
}

#[derive(Clone, Debug, Default)]
pub struct StreamMatchProfile {
    pub preferred_addon: String,
    pub preferred_debrid: Option<DebridAvailability>,
    pub preferred_debrid_badge: String,
    pub quality_height: Option<u16>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub hdr: bool,
    pub dolby_vision: bool,
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    pub size_bytes: Option<u64>,
}

impl StreamMatchProfile {
    pub fn from_rank_input(input: &RankInput, is_torrent: bool) -> Self {
        let mut normalized = input.clone();
        if normalized.debrid.is_none() {
            normalized.debrid = parse_debrid_availability_from_text(&normalized.description)
                .or_else(|| parse_debrid_availability_from_text(&normalized.name));
        }
        let parsed = parse_stream(&normalized);
        let (badge, _, _) =
            parse_debrid_badge(&normalized.name, &normalized.description, normalized.debrid, is_torrent);
        let size_bytes = normalized
            .size_bytes
            .filter(|&sz| sz > 0)
            .or_else(|| parse_size_bytes_from_text(&normalized.description))
            .or_else(|| parse_size_bytes_from_text(&normalized.name));
        Self {
            preferred_addon: normalized.addon,
            preferred_debrid: normalized.debrid,
            preferred_debrid_badge: badge,
            quality_height: parsed.quality_height,
            source: parsed.source,
            codec: parsed.codec,
            hdr: parsed.hdr,
            dolby_vision: parsed.dolby_vision,
            languages: parsed.languages,
            release_group: parsed.release_group,
            size_bytes,
        }
    }
}

pub fn score_episode_candidate(
    profile: &StreamMatchProfile,
    candidate: &RankedStream,
    is_torrent: bool,
) -> i32 {
    if candidate.filtered {
        return i32::MIN / 2;
    }

    let mut score = candidate.score.clamp(-200, 250);
    let (cand_badge, _, _) = parse_debrid_badge(
        &candidate.input.name,
        &candidate.input.description,
        candidate.input.debrid,
        is_torrent,
    );

    if profile.preferred_debrid == Some(DebridAvailability::Cached) {
        match candidate.input.debrid {
            Some(DebridAvailability::Cached) => {
                score += 250;
                if !profile.preferred_debrid_badge.is_empty()
                    && cand_badge == profile.preferred_debrid_badge
                {
                    score += 60;
                }
            }
            Some(DebridAvailability::Uncached) => score -= 120,
            Some(DebridAvailability::Unknown | DebridAvailability::ProviderUnavailable) | None => {
                score -= 40;
            }
        }
    } else if candidate.input.debrid == Some(DebridAvailability::Cached) {
        score += 120;
    }

    if !profile.preferred_addon.is_empty() && candidate.input.addon == profile.preferred_addon {
        score += 70;
    }

    if let (Some(pref_group), Some(cand_group)) = (
        profile.release_group.as_deref(),
        candidate.parsed.release_group.as_deref(),
    ) && pref_group.eq_ignore_ascii_case(cand_group)
    {
        score += 90;
    }

    if let (Some(pref_h), Some(cand_h)) = (profile.quality_height, candidate.parsed.quality_height) {
        if pref_h == cand_h {
            score += 80;
        } else {
            let diff = pref_h.abs_diff(cand_h);
            if diff <= 1080 {
                score += 25;
            } else {
                score -= 30;
            }
        }
    }

    if profile.source.is_some() && profile.source == candidate.parsed.source {
        score += 25;
    }
    if profile.codec.is_some() && profile.codec == candidate.parsed.codec {
        score += 20;
    }
    if profile.dolby_vision == candidate.parsed.dolby_vision && profile.hdr == candidate.parsed.hdr
    {
        score += 15;
    }
    if profile
        .languages
        .iter()
        .any(|lang| candidate.parsed.languages.contains(lang))
    {
        score += 30;
    }

    if let (Some(pref_sz), Some(cand_sz)) = (profile.size_bytes, candidate.effective_size)
        && pref_sz > 0
        && cand_sz > 0
    {
        let ratio = cand_sz as f64 / pref_sz as f64;
        if (0.3..=3.0).contains(&ratio) {
            score += 20;
        } else if ratio > 6.0 {
            score -= 25;
        }
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(index: usize, name: &str, size: u64, seeders: u32) -> RankInput {
        RankInput {
            id: index.to_string(),
            name: name.to_owned(),
            description: String::new(),
            addon: "Fixture".to_owned(),
            original_index: index,
            size_bytes: Some(size),
            seeders: Some(seeders),
            debrid: None,
        }
    }

    #[test]
    fn original_mode_exactly_preserves_addon_order() {
        let ranked = rank_streams(
            [
                input(2, "2160p REMUX", 50, 10),
                input(0, "720p WEBRip", 1, 100),
                input(1, "1080p WEB-DL", 2, 50),
            ],
            RankingMode::Original,
            false,
        );
        assert_eq!(
            ranked
                .iter()
                .map(|stream| stream.input.original_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn smart_ranking_is_deterministic_and_uses_original_order_as_final_tie_breaker() {
        let fixture = [
            input(1, "1080p WEB-DL x265", 2_000_000_000, 50),
            input(0, "1080p WEB-DL x265", 2_000_000_000, 50),
        ];
        let first = rank_streams(fixture.clone(), RankingMode::Smart, false);
        let second = rank_streams(fixture, RankingMode::Smart, false);
        assert_eq!(first, second);
        assert_eq!(first[0].input.original_index, 0);
    }

    #[test]
    fn suspicious_streams_are_hidden_but_recoverable() {
        let fake = input(0, "2160p FAKE install codec", 1, 9999);
        assert!(rank_streams([fake.clone()], RankingMode::Smart, false).is_empty());
        let visible = rank_streams([fake], RankingMode::Smart, true);
        assert!(visible[0].filtered);
    }

    #[test]
    fn provider_outage_is_neutral() {
        let mut unavailable = input(0, "1080p", 2_000_000_000, 20);
        unavailable.debrid = Some(DebridAvailability::ProviderUnavailable);
        let mut unknown = unavailable.clone();
        unknown.debrid = Some(DebridAvailability::Unknown);
        assert_eq!(
            rank_streams([unavailable], RankingMode::Smart, false)[0].score,
            rank_streams([unknown], RankingMode::Smart, false)[0].score
        );
    }

    #[test]
    fn parses_seeders_and_size_correctly() {
        let desc = "Musafir.Cafe.2026.S01.1080p.NF.WEB-DL.Multi..DD+.5.1.Atmos.x264-KiN\nMusafir Cafe-S01E01-Arrival.1080p.Multi.WEB-DL.DD+.5.1.Atmos.x264-KiN..mkv\n👤 5 💾 1.24 GB ≡ EZTV";
        assert_eq!(parse_seeders_from_text(desc), Some(5));
        assert_eq!(
            parse_size_bytes_from_text(desc),
            Some((1.24 * 1_073_741_824.0) as u64)
        );

        let formatted = format_stream_description("Torrentio 1080p", desc, None, None);
        assert!(formatted.contains("👤 5"));
        assert!(formatted.contains("💾 1.24 GB"));
        assert_eq!(formatted.lines().count(), 2);
    }

    #[test]
    fn ensures_seeds_and_size_when_only_in_behavior_hints() {
        let formatted = format_stream_description(
            "HTTP Stream",
            "Big Buck Bunny 1080p",
            Some(1_331_691_520),
            Some(12),
        );
        assert!(formatted.contains("👤 12"));
        assert!(formatted.contains("💾 1.24 GB"));
        assert_eq!(formatted.lines().count(), 2);
    }

    #[test]
    fn parses_debrid_availability_from_text_correctly() {
        assert_eq!(
            parse_debrid_availability_from_text("[RD+] Torrentio 1080p"),
            Some(DebridAvailability::Cached)
        );
        assert_eq!(
            parse_debrid_availability_from_text("Torrentio\nRD+ 4K HDR"),
            Some(DebridAvailability::Cached)
        );
        assert_eq!(
            parse_debrid_availability_from_text("MediaFusion ⚡ 1080p"),
            Some(DebridAvailability::Cached)
        );
        assert_eq!(
            parse_debrid_availability_from_text("[RD download] Torrentio 1080p"),
            Some(DebridAvailability::Uncached)
        );
        assert_eq!(
            parse_debrid_availability_from_text("Torrentio\nRD download"),
            Some(DebridAvailability::Uncached)
        );
        assert_eq!(
            parse_debrid_availability_from_text("The Pirate Bay 1080p"),
            None
        );
    }

    #[test]
    fn cached_debrid_streams_always_prioritized_over_torrents() {
        let torrent_4k = input(0, "4K REMUX BluRay Atmos", 50_000_000_000, 500);
        let rd_cached_1080p = input(1, "[RD+] 1080p WEB-DL", 2_000_000_000, 10);
        let rd_uncached = input(2, "[RD download] 4K BluRay", 40_000_000_000, 5);

        let ranked = rank_streams(
            [
                torrent_4k.clone(),
                rd_cached_1080p.clone(),
                rd_uncached.clone(),
            ],
            RankingMode::Smart,
            false,
        );

        // Cached RD must be first even if torrent has 4K REMUX 500 seeds
        assert_eq!(ranked[0].input.id, rd_cached_1080p.id);
        assert_eq!(ranked[0].input.debrid, Some(DebridAvailability::Cached));
    }

    #[test]
    fn parses_comma_decimal_and_box_emoji_sizes() {
        assert_eq!(
            parse_size_bytes_from_text("📦 850,5 MB | Torrentio"),
            Some((850.5 * 1_048_576.0) as u64)
        );
        assert_eq!(
            parse_size_bytes_from_text("Show S01E02 💾 1,24 GB"),
            Some((1.24 * 1_073_741_824.0) as u64)
        );
        assert_eq!(
            parse_size_bytes_from_text("Movie 2.45GB 1080p"),
            Some((2.45 * 1_073_741_824.0) as u64)
        );
    }

    #[test]
    fn extracts_clean_presentation_without_truncating_badges() {
        let mut item = input(
            0,
            "[RD+] Torrentio\n1080p",
            (2.5 * 1_073_741_824.0) as u64,
            42,
        );
        item.description =
            "Severance.S01E02.1080p.WEB-DL.DDP5.1.Atmos.HEVC-NTb.mkv\n👤 42 💾 2.50 GB ⚙️ Torrentio · Spanish"
                .to_owned();
        item.debrid = Some(DebridAvailability::Cached);
        let parsed = parse_stream(&item);
        let pres = extract_stream_presentation(
            &item,
            &parsed,
            item.size_bytes,
            item.seeders,
            "Torrentio | ElfHosted",
            false,
        );
        assert_eq!(pres.resolution_label, "1080p");
        assert_eq!(pres.debrid_badge, "RD+");
        assert!(pres.is_debrid_cached);
        assert_eq!(pres.source_badge, "Torrentio");
        assert_eq!(pres.size_label, "2.50 GB");
        assert_eq!(pres.seeders_label, "👤 42");
        assert!(pres.tech_summary.contains("WEB-DL"));
        assert!(pres.tech_summary.contains("HEVC"));
        assert!(pres.tech_summary.contains("Spanish"));
    }

    #[test]
    fn episode_candidate_scoring_prefers_matching_cached_and_falls_back() {
        let mut seed_input = input(0, "[RD+] Torrentio\n1080p", 2_000_000_000, 30);
        seed_input.addon = "https://torrentio.example/manifest.json".to_owned();
        seed_input.description = "Show.S01E01.1080p.WEB-DL.HEVC-NTb".to_owned();
        seed_input.debrid = Some(DebridAvailability::Cached);
        let profile = StreamMatchProfile::from_rank_input(&seed_input, false);

        let mut exact = input(1, "[RD+] Torrentio\n1080p", 2_100_000_000, 25);
        exact.addon = seed_input.addon.clone();
        exact.description = "Show.S01E02.1080p.WEB-DL.HEVC-NTb".to_owned();
        exact.debrid = Some(DebridAvailability::Cached);

        let mut other_addon_cached = input(2, "[AD+] Comet\n720p", 1_100_000_000, 10);
        other_addon_cached.addon = "https://comet.example/manifest.json".to_owned();
        other_addon_cached.description = "Show.S01E02.720p.WEB-DL.H264".to_owned();
        other_addon_cached.debrid = Some(DebridAvailability::Cached);

        let mut uncached = input(3, "[RD download] Torrentio\n1080p", 2_100_000_000, 5);
        uncached.addon = seed_input.addon.clone();
        uncached.description = "Show.S01E02.1080p.WEB-DL.HEVC-NTb".to_owned();
        uncached.debrid = Some(DebridAvailability::Uncached);

        let ranked = rank_streams([exact, other_addon_cached, uncached], RankingMode::Smart, false);
        let s_exact = score_episode_candidate(&profile, &ranked[0], false);
        let s_other = score_episode_candidate(&profile, &ranked[1], false);
        let s_uncached = score_episode_candidate(&profile, &ranked[2], false);
        assert!(s_exact > s_other);
        assert!(s_other > s_uncached);
    }
}
