use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use image::{
    DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits, imageops::FilterType,
};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;

const IMAGE_HOST: &str = "ir.ozone.ru";
const HTTPS_PORT: u16 = 443;
const MAX_URL_BYTES: usize = 2_048;
const MAX_REDIRECTS: usize = 3;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_PIXELS: u64 = 20_000_000;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;
const MAX_OUTPUT_EDGE: u32 = 1_536;
const MAX_ENCODE_ATTEMPTS: usize = 10;
const FETCH_DEADLINE: Duration = Duration::from_secs(30);
const DOH_HOST: &str = "dns.google";
const DOH_BOOTSTRAP: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
const DOH_RESPONSE_BYTES: usize = 16 * 1024;
const DOH_DEADLINE: Duration = Duration::from_secs(5);
const MAX_DOH_ADDRESSES: usize = 16;
const MAX_CNAME_DEPTH: usize = 8;
const DECODE_WORKERS: usize = 2;
const DECODE_WAITERS: usize = 4;

static DECODE_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(DECODE_WORKERS)));
static DECODE_QUEUE: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(DECODE_WAITERS)));

#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Status")]
    status: u32,
    #[serde(rename = "TC")]
    truncated: bool,
    #[serde(rename = "Question")]
    question: Vec<DohQuestion>,
    #[serde(rename = "Answer", default)]
    answer: Vec<DohAnswer>,
}

#[derive(Deserialize)]
struct DohQuestion {
    name: String,
    #[serde(rename = "type")]
    record_type: u16,
}

#[derive(Deserialize)]
struct DohAnswer {
    name: String,
    #[serde(rename = "type")]
    record_type: u16,
    data: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedImage {
    pub data: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub sha256: String,
    pub retrieved_at: String,
}

pub async fn fetch_image(url: &str, cancel: &CancellationToken) -> Result<FetchedImage> {
    let url = validate_url(url)?;
    let fetch = async move {
        let (bytes, declared_mime) = download(url).await?;
        run_decode_work(move || prepare_image(&bytes, &declared_mime)).await
    };

    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("CANCELLED: image fetch cancelled"),
        result = tokio::time::timeout(FETCH_DEADLINE, fetch) => match result {
            Ok(result) => result,
            Err(_) => bail!("UPSTREAM_TIMEOUT: image fetch exceeded its deadline"),
        },
    }
}

async fn run_decode_work<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    run_decode_work_with(DECODE_SLOTS.clone(), DECODE_QUEUE.clone(), work).await
}

async fn run_decode_work_with<T, F>(
    slots: Arc<Semaphore>,
    queue: Arc<Semaphore>,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let queue_permit = queue
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("SERVER_BUSY: image processing queue is full"))?;
    let worker_permit = slots
        .acquire_owned()
        .await
        .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: image processing is unavailable"))?;
    drop(queue_permit);

    tokio::task::spawn_blocking(move || {
        // Cancellation drops the JoinHandle future, not the blocking work. Keeping
        // this permit inside the closure prevents cancelled decodes from escaping
        // the memory/concurrency bound while they finish.
        let _worker_permit = worker_permit;
        work()
    })
    .await
    .map_err(|_| anyhow::anyhow!("SOURCE_CHANGED: image processing failed"))?
}

async fn download(mut url: Url) -> Result<(Vec<u8>, String)> {
    for redirects in 0..=MAX_REDIRECTS {
        let addresses = resolve_public_addresses().await?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .retry(reqwest::retry::never())
            .resolve_to_addrs(IMAGE_HOST, &addresses)
            .build()
            .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: image transport is unavailable"))?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: image source is unavailable"))?;

        if response.status().is_redirection() {
            if redirects == MAX_REDIRECTS {
                bail!("SOURCE_BLOCKED: image redirect limit exceeded");
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("SOURCE_BLOCKED: invalid image redirect"))?;
            if location.len() > MAX_URL_BYTES {
                bail!("SOURCE_BLOCKED: invalid image redirect");
            }
            let next = url
                .join(location)
                .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: invalid image redirect"))?;
            url = validate_url(next.as_str())?;
            continue;
        }

        if response.status() == StatusCode::NOT_FOUND {
            bail!("NOT_FOUND: image source was not found");
        }
        if !response.status().is_success() {
            bail!("SOURCE_BLOCKED: image source did not return an image");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SOURCE_BYTES as u64)
        {
            bail!("RESULT_TOO_LARGE: compressed image exceeds 4 MiB");
        }

        let declared_mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(supported_mime)
            .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: unsupported image content type"))?
            .to_owned();
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: image download failed"))?;
            append_bounded(&mut bytes, &chunk)?;
        }
        return Ok((bytes, declared_mime));
    }
    unreachable!("redirect loop always returns or rejects")
}

async fn resolve_public_addresses() -> Result<Vec<SocketAddr>> {
    let doh_enabled = doh_fallback_enabled()?;
    let resolved: Vec<_> = tokio::net::lookup_host((IMAGE_HOST, HTTPS_PORT))
        .await
        .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: image host could not be resolved"))?
        .collect();
    if resolved.is_empty() {
        bail!("SOURCE_BLOCKED: image host has no public address");
    }
    if resolved.iter().all(|address| is_tun_fake_ip(address.ip())) {
        if !doh_enabled {
            bail!("SOURCE_BLOCKED: image host resolved through a synthetic DNS address");
        }
        return resolve_image_host_via_doh().await;
    }

    let mut seen = HashSet::new();
    let mut addresses = Vec::new();
    for address in resolved {
        if !is_public_ip(address.ip()) {
            bail!("SOURCE_BLOCKED: image host resolved to a non-public address");
        }
        if seen.insert(address.ip()) {
            addresses.push(SocketAddr::new(address.ip(), HTTPS_PORT));
        }
    }
    if addresses.is_empty() {
        bail!("SOURCE_BLOCKED: image host has no public address");
    }
    Ok(addresses)
}

fn doh_fallback_enabled() -> Result<bool> {
    match std::env::var("OZON_IMAGE_DOH_FALLBACK") {
        Err(std::env::VarError::NotPresent) => Ok(true),
        Ok(value) if value == "auto" => Ok(true),
        Ok(value) if value == "off" => Ok(false),
        _ => bail!("SOURCE_BLOCKED: invalid image DNS fallback setting"),
    }
}

fn is_tun_fake_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if {
        let [a, b, _, _] = ip.octets();
        a == 198 && matches!(b, 18..=19)
    })
}

async fn resolve_image_host_via_doh() -> Result<Vec<SocketAddr>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        .timeout(DOH_DEADLINE)
        .resolve(
            DOH_HOST,
            SocketAddr::new(IpAddr::V4(DOH_BOOTSTRAP), HTTPS_PORT),
        )
        .build()
        .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: secure DNS transport is unavailable"))?;
    let response = client
        .get("https://dns.google/resolve?name=ir.ozone.ru&type=A&edns_client_subnet=0.0.0.0%2F0")
        .header(header::ACCEPT, "application/dns-json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("UPSTREAM_TIMEOUT: secure image DNS lookup timed out")
            } else {
                anyhow::anyhow!("SOURCE_BLOCKED: secure image DNS lookup failed")
            }
        })?;
    if !response.status().is_success() {
        bail!("SOURCE_BLOCKED: secure image DNS lookup failed");
    }
    if response
        .content_length()
        .is_some_and(|length| length > DOH_RESPONSE_BYTES as u64)
    {
        bail!("SOURCE_BLOCKED: secure image DNS response exceeds size limit");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: secure image DNS lookup failed"))?;
        if body.len().saturating_add(chunk.len()) > DOH_RESPONSE_BYTES {
            bail!("SOURCE_BLOCKED: secure image DNS response exceeds size limit");
        }
        body.extend_from_slice(&chunk);
    }
    parse_doh_addresses(&body)
}

fn parse_doh_addresses(body: &[u8]) -> Result<Vec<SocketAddr>> {
    let response: DohResponse = serde_json::from_slice(body)
        .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: invalid secure image DNS response"))?;
    if response.status != 0
        || response.truncated
        || response.question.len() != 1
        || response.question[0].record_type != 1
        || dns_name(&response.question[0].name).as_deref() != Some(IMAGE_HOST)
    {
        bail!("SOURCE_BLOCKED: invalid secure image DNS response");
    }

    let mut aliases = HashMap::new();
    for answer in &response.answer {
        if answer.record_type == 5 {
            let owner = dns_name(&answer.name).ok_or_else(|| {
                anyhow::anyhow!("SOURCE_BLOCKED: invalid secure image DNS answer")
            })?;
            let target = dns_name(&answer.data).ok_or_else(|| {
                anyhow::anyhow!("SOURCE_BLOCKED: invalid secure image DNS answer")
            })?;
            if aliases.insert(owner, target).is_some() {
                bail!("SOURCE_BLOCKED: ambiguous secure image DNS answer");
            }
        } else if answer.record_type != 1 {
            bail!("SOURCE_BLOCKED: unexpected secure image DNS answer");
        }
    }

    let mut reachable = HashSet::from([IMAGE_HOST.to_owned()]);
    let mut current = IMAGE_HOST.to_owned();
    for _ in 0..MAX_CNAME_DEPTH {
        let Some(next) = aliases.get(&current) else {
            break;
        };
        if !reachable.insert(next.clone()) {
            bail!("SOURCE_BLOCKED: cyclic secure image DNS answer");
        }
        current = next.clone();
    }
    if aliases.contains_key(&current) {
        bail!("SOURCE_BLOCKED: secure image DNS alias chain is too deep");
    }

    let mut seen = HashSet::new();
    let mut addresses = Vec::new();
    for answer in response.answer {
        let owner = dns_name(&answer.name)
            .ok_or_else(|| anyhow::anyhow!("SOURCE_BLOCKED: invalid secure image DNS answer"))?;
        if !reachable.contains(&owner) {
            bail!("SOURCE_BLOCKED: unrelated secure image DNS answer");
        }
        if answer.record_type != 1 {
            continue;
        }
        if owner != current {
            bail!("SOURCE_BLOCKED: invalid secure image DNS alias answer");
        }
        let ip: Ipv4Addr = answer
            .data
            .parse()
            .map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: invalid secure image DNS address"))?;
        if !is_public_ipv4(ip) {
            bail!("SOURCE_BLOCKED: secure image DNS returned a non-public address");
        }
        if seen.insert(ip) {
            if addresses.len() == MAX_DOH_ADDRESSES {
                bail!("SOURCE_BLOCKED: secure image DNS returned too many addresses");
            }
            addresses.push(SocketAddr::new(IpAddr::V4(ip), HTTPS_PORT));
        }
    }
    if addresses.is_empty() {
        bail!("SOURCE_BLOCKED: secure image DNS returned no public address");
    }
    Ok(addresses)
}

fn dns_name(value: &str) -> Option<String> {
    let value = value
        .strip_suffix('.')
        .unwrap_or(value)
        .to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 253
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        None
    } else {
        Some(value)
    }
}

fn validate_url(raw: &str) -> Result<Url> {
    if raw.len() > MAX_URL_BYTES {
        bail!("SOURCE_BLOCKED: image URL exceeds size limit");
    }
    let url = Url::parse(raw).map_err(|_| anyhow::anyhow!("SOURCE_BLOCKED: invalid image URL"))?;
    if url.scheme() != "https"
        || url.host_str() != Some(IMAGE_HOST)
        || url.port_or_known_default() != Some(HTTPS_PORT)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("SOURCE_BLOCKED: image URL origin is not allowed");
    }
    Ok(url)
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 88, 99)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = ip.segments();
    let first = segments[0];
    if first & 0xe000 != 0x2000 || first == 0x2002 || first & 0xfff0 == 0x3ff0 {
        return false;
    }
    if first == 0x2001 && (segments[1] == 0 || segments[1] == 2 || segments[1] == 0x0db8) {
        return false;
    }
    true
}

fn append_bounded(target: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    if target.len().saturating_add(chunk.len()) > MAX_SOURCE_BYTES {
        bail!("RESULT_TOO_LARGE: compressed image exceeds 4 MiB");
    }
    target.extend_from_slice(chunk);
    Ok(())
}

fn supported_mime(value: &str) -> Option<&'static str> {
    match value
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" => Some("image/jpeg"),
        "image/png" => Some("image/png"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn prepare_image(source: &[u8], declared_mime: &str) -> Result<FetchedImage> {
    let format = image::guess_format(source)
        .map_err(|_| anyhow::anyhow!("SOURCE_CHANGED: image signature is invalid"))?;
    let (format_mime, format) = match format {
        ImageFormat::Jpeg => ("image/jpeg", ImageFormat::Jpeg),
        ImageFormat::Png => ("image/png", ImageFormat::Png),
        ImageFormat::WebP => ("image/webp", ImageFormat::WebP),
        _ => bail!("SOURCE_CHANGED: unsupported raster image format"),
    };
    if declared_mime != format_mime {
        bail!("SOURCE_CHANGED: image content type does not match its bytes");
    }

    let dimension_reader = ImageReader::with_format(Cursor::new(source), format);
    let (width, height) = dimension_reader
        .into_dimensions()
        .map_err(|_| anyhow::anyhow!("SOURCE_CHANGED: image dimensions are invalid"))?;
    validate_dimensions(width, height)?;

    let mut reader = ImageReader::with_format(Cursor::new(source), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_PIXELS as u32);
    limits.max_image_height = Some(MAX_SOURCE_PIXELS as u32);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(map_decode_error)?;
    let orientation = decoder
        .orientation()
        .map_err(|_| anyhow::anyhow!("SOURCE_CHANGED: image orientation is invalid"))?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(map_decode_error)?;
    image.apply_orientation(orientation);

    if image.width() > MAX_OUTPUT_EDGE || image.height() > MAX_OUTPUT_EDGE {
        image = image.resize(MAX_OUTPUT_EDGE, MAX_OUTPUT_EDGE, FilterType::Lanczos3);
    }
    let encoded = encode_bounded(image, format)?;
    let width = encoded.1.width();
    let height = encoded.1.height();
    let bytes = encoded.0;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(FetchedImage {
        data: STANDARD.encode(&bytes),
        mime_type: format_mime.to_owned(),
        width,
        height,
        sha256,
        retrieved_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| anyhow::anyhow!("RESULT_TOO_LARGE: decoded image dimensions overflow"))?;
    if width == 0 || height == 0 || pixels > MAX_SOURCE_PIXELS {
        bail!("RESULT_TOO_LARGE: decoded image exceeds 20 megapixels");
    }
    Ok(())
}

fn map_decode_error(error: ImageError) -> anyhow::Error {
    if matches!(error, ImageError::Limits(_)) {
        anyhow::anyhow!("RESULT_TOO_LARGE: image cannot be decoded within limits")
    } else {
        anyhow::anyhow!("SOURCE_CHANGED: image decode failed")
    }
}

fn encode_bounded(mut image: DynamicImage, format: ImageFormat) -> Result<(Vec<u8>, DynamicImage)> {
    for _ in 0..MAX_ENCODE_ATTEMPTS {
        let mut bytes = Vec::new();
        match format {
            ImageFormat::Jpeg => image.write_with_encoder(
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 82),
            ),
            ImageFormat::Png => {
                image.write_with_encoder(image::codecs::png::PngEncoder::new(&mut bytes))
            }
            ImageFormat::WebP => {
                image.write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut bytes))
            }
            _ => unreachable!("format checked before encoding"),
        }
        .map_err(|_| anyhow::anyhow!("SOURCE_CHANGED: image encoding failed"))?;
        if bytes.len() <= MAX_OUTPUT_BYTES {
            return Ok((bytes, image));
        }
        if image.width() == 1 && image.height() == 1 {
            break;
        }
        let ratio = ((MAX_OUTPUT_BYTES as f64 / bytes.len() as f64).sqrt() * 0.92).min(0.9);
        let next_width = ((image.width() as f64 * ratio).floor() as u32).max(1);
        let next_height = ((image.height() as f64 * ratio).floor() as u32).max(1);
        image = image.resize(next_width, next_height, FilterType::Lanczos3);
    }
    bail!("RESULT_TOO_LARGE: image cannot fit the encoded payload limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use image::{GenericImageView, ImageBuffer, Rgba};

    fn fixture(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
            Rgba([(x % 251) as u8, (y % 239) as u8, ((x + y) % 233) as u8, 255])
        }));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = u32::MAX;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
                }
            }
            !crc
        }

        let mut png = fixture(ImageFormat::Png, 8, 6);
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    #[test]
    fn accepts_only_the_exact_cdn_https_origin() {
        for valid in [
            "https://ir.ozone.ru/image.jpg",
            "https://IR.OZONE.RU:443/path?q=1",
        ] {
            assert!(validate_url(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "http://ir.ozone.ru/image.jpg",
            "https://user@ir.ozone.ru/image.jpg",
            "https://ir.ozone.ru:444/image.jpg",
            "https://evil.ir.ozone.ru/image.jpg",
            "https://ir.ozone.ru.evil.test/image.jpg",
            "https://127.0.0.1/image.jpg",
        ] {
            assert!(validate_url(invalid).is_err(), "{invalid}");
        }
        assert!(validate_url(&format!("https://ir.ozone.ru/{}", "x".repeat(2048))).is_err());
    }

    #[test]
    fn rejects_non_public_addresses_including_mapped_ipv4() {
        for ip in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.1.1",
            "224.0.0.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn accepts_only_valid_public_doh_cname_answers() {
        let valid = serde_json::to_vec(&serde_json::json!({
            "Status": 0,
            "TC": false,
            "Question": [{"name": "ir.ozone.ru.", "type": 1}],
            "Answer": [
                {"name": "ir.ozone.ru.", "type": 5, "TTL": 60, "data": "cdn.example.net."},
                {"name": "cdn.example.net.", "type": 1, "TTL": 60, "data": "185.73.195.116"},
                {"name": "cdn.example.net.", "type": 1, "TTL": 60, "data": "185.73.193.119"}
            ]
        }))
        .unwrap();
        let addresses = parse_doh_addresses(&valid).unwrap();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.iter().all(|address| is_public_ip(address.ip())));

        for invalid in [
            serde_json::json!({
                "Status": 0, "TC": false,
                "Question": [{"name": "example.com.", "type": 1}],
                "Answer": [{"name": "example.com.", "type": 1, "data": "93.184.216.34"}]
            }),
            serde_json::json!({
                "Status": 0, "TC": false,
                "Question": [{"name": "ir.ozone.ru.", "type": 1}],
                "Answer": [{"name": "ir.ozone.ru.", "type": 1, "data": "127.0.0.1"}]
            }),
            serde_json::json!({
                "Status": 0, "TC": false,
                "Question": [{"name": "ir.ozone.ru.", "type": 1}],
                "Answer": [
                    {"name": "ir.ozone.ru.", "type": 1, "data": "185.73.195.116"},
                    {"name": "attacker.example.", "type": 1, "data": "93.184.216.34"}
                ]
            }),
        ] {
            assert!(parse_doh_addresses(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }

    #[test]
    fn recognizes_only_the_tun_benchmark_range_as_fake_dns() {
        for ip in ["198.18.0.1", "198.19.255.254"] {
            assert!(is_tun_fake_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["10.0.0.1", "192.168.0.1", "198.20.0.1", "2001:db8::1"] {
            assert!(!is_tun_fake_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn rejects_forged_mime_and_decoded_bomb_dimensions() {
        let png = fixture(ImageFormat::Png, 8, 6);
        assert!(
            prepare_image(&png, "image/jpeg")
                .unwrap_err()
                .to_string()
                .starts_with("SOURCE_CHANGED:")
        );
        assert!(
            validate_dimensions(5_000, 4_001)
                .unwrap_err()
                .to_string()
                .starts_with("RESULT_TOO_LARGE:")
        );
        assert!(
            prepare_image(&png_with_dimensions(5_000, 4_001), "image/png")
                .unwrap_err()
                .to_string()
                .starts_with("RESULT_TOO_LARGE:")
        );
    }

    #[test]
    fn normalizes_supported_formats_to_bounded_roundtrippable_payloads() {
        for (format, mime) in [
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Png, "image/png"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let fetched = prepare_image(&fixture(format, 1_800, 1_200), mime).unwrap();
            let decoded = STANDARD.decode(&fetched.data).unwrap();
            assert!(decoded.len() <= MAX_OUTPUT_BYTES);
            assert_eq!(fetched.mime_type, mime);
            assert!(fetched.width <= MAX_OUTPUT_EDGE);
            assert!(fetched.height <= MAX_OUTPUT_EDGE);
            assert!(fetched.width > 0 && fetched.height > 0);
            let aspect_error = (fetched.width as f64 / fetched.height as f64 - 1.5_f64).abs();
            assert!(
                aspect_error < 0.002,
                "aspect ratio drifted by {aspect_error}"
            );
            assert_eq!(fetched.sha256, format!("{:x}", Sha256::digest(&decoded)));
            assert_eq!(image::guess_format(&decoded).unwrap(), format);
            let roundtrip = image::load_from_memory_with_format(&decoded, format).unwrap();
            assert_eq!(roundtrip.dimensions(), (fetched.width, fetched.height));
        }
    }

    #[test]
    fn compressed_download_limit_never_appends_a_partial_chunk() {
        let mut bytes = vec![0; MAX_SOURCE_BYTES - 2];
        assert!(append_bounded(&mut bytes, &[1, 2]).is_ok());
        assert_eq!(bytes.len(), MAX_SOURCE_BYTES);
        assert!(append_bounded(&mut bytes, &[3]).is_err());
        assert_eq!(bytes.len(), MAX_SOURCE_BYTES);
    }

    #[tokio::test]
    async fn pre_cancelled_fetch_stops_before_network_work() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = fetch_image("https://ir.ozone.ru/image.jpg", &cancel)
            .await
            .unwrap_err();
        assert!(error.to_string().starts_with("CANCELLED:"));
    }

    #[tokio::test]
    async fn decode_queue_exhaustion_reports_server_busy() {
        let error = run_decode_work_with(
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(0)),
            || Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "SERVER_BUSY: image processing queue is full"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_caller_does_not_release_a_running_decode_permit() {
        let slots = Arc::new(Semaphore::new(1));
        let queue = Arc::new(Semaphore::new(1));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_slots = slots.clone();
        let task_queue = queue.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let caller = tokio::spawn(async move {
            tokio::select! {
                _ = task_cancel.cancelled() => bail!("CANCELLED: test caller cancelled"),
                result = run_decode_work_with(task_slots, task_queue, move || {
                    let _ = started_tx.send(());
                    release_rx.recv().expect("test releases blocking work");
                    Ok(())
                }) => result,
            }
        });

        started_rx.await.expect("blocking work started");
        cancel.cancel();
        let error = caller.await.unwrap().unwrap_err();
        assert!(error.to_string().starts_with("CANCELLED:"));
        assert_eq!(slots.available_permits(), 0);
        assert!(slots.clone().try_acquire_owned().is_err());

        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(1), slots.clone().acquire_owned())
            .await
            .expect("blocking work releases its permit")
            .unwrap();
        drop(permit);
        assert_eq!(slots.available_permits(), 1);
    }
}
