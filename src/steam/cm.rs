// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal anonymous Steam CM client over WebSocket, only for PICS product info.

use std::fmt::Write as _;
use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::net::HttpClient;

const PROTO_MASK: u32 = 0x8000_0000;
const PROTOCOL_VERSION: u32 = 65580;
const ANON_STEAM_ID: u64 = 0x01A0_0000_0000_0000;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_UNZIPPED: usize = 64 * 1024 * 1024;
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

mod emsg {
    pub const MULTI: u32 = 1;
    pub const CLIENT_LOGGED_OFF: u32 = 757;
    pub const CLIENT_LOGON_RESPONSE: u32 = 751;
    pub const CLIENT_LOGON: u32 = 5514;
    pub const PICS_PRODUCT_INFO_REQUEST: u32 = 8903;
    pub const PICS_PRODUCT_INFO_RESPONSE: u32 = 8904;
    pub const PICS_ACCESS_TOKEN_REQUEST: u32 = 8905;
    pub const PICS_ACCESS_TOKEN_RESPONSE: u32 = 8906;
}

#[derive(Clone, PartialEq, prost::Message)]
struct ProtoHeader {
    #[prost(fixed64, optional, tag = "1")]
    steamid: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    client_sidid: Option<i32>,
    #[prost(fixed64, optional, tag = "10")]
    jobid_source: Option<u64>,
    #[prost(fixed64, optional, tag = "11")]
    jobid_target: Option<u64>,
    #[prost(int32, optional, tag = "13")]
    eresult: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ClientLogon {
    #[prost(uint32, optional, tag = "1")]
    protocol_version: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    cell_id: Option<u32>,
    #[prost(string, optional, tag = "6")]
    client_language: Option<String>,
    #[prost(uint32, optional, tag = "7")]
    client_os_type: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ClientLogonResponse {
    #[prost(int32, optional, tag = "1")]
    eresult: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Multi {
    #[prost(uint32, optional, tag = "1")]
    size_unzipped: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    message_body: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AccessTokenRequest {
    #[prost(uint32, repeated, packed = "false", tag = "2")]
    appids: Vec<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AppToken {
    #[prost(uint32, optional, tag = "1")]
    appid: Option<u32>,
    #[prost(uint64, optional, tag = "2")]
    access_token: Option<u64>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AccessTokenResponse {
    #[prost(message, repeated, tag = "3")]
    app_access_tokens: Vec<AppToken>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ProductInfoRequestApp {
    #[prost(uint32, optional, tag = "1")]
    appid: Option<u32>,
    #[prost(uint64, optional, tag = "2")]
    access_token: Option<u64>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ProductInfoRequest {
    #[prost(message, repeated, tag = "2")]
    apps: Vec<ProductInfoRequestApp>,
    #[prost(bool, optional, tag = "3")]
    meta_data_only: Option<bool>,
    #[prost(bool, optional, tag = "7")]
    single_response: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ProductInfoResponseApp {
    #[prost(uint32, optional, tag = "1")]
    appid: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    change_number: Option<u32>,
    #[prost(bool, optional, tag = "3")]
    missing_token: Option<bool>,
    #[prost(bytes = "vec", optional, tag = "4")]
    sha: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "5")]
    buffer: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "7")]
    size: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ProductInfoResponse {
    #[prost(message, repeated, tag = "1")]
    apps: Vec<ProductInfoResponseApp>,
    #[prost(uint32, repeated, packed = "false", tag = "2")]
    unknown_appids: Vec<u32>,
    #[prost(bool, optional, tag = "6")]
    response_pending: Option<bool>,
    #[prost(string, optional, tag = "8")]
    http_host: Option<String>,
}

/// Raw app info text as returned by PICS.
#[derive(Debug, Clone)]
pub struct RawAppInfo {
    pub appid: u32,
    pub change_number: u32,
    pub text: String,
    /// Answered without the access token, so private parts may be missing.
    pub public_only: bool,
}

#[derive(Debug, Default)]
pub struct ProductInfo {
    pub apps: Vec<RawAppInfo>,
    pub unknown: Vec<u32>,
}

struct Packet {
    emsg: u32,
    header: ProtoHeader,
    body: Vec<u8>,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Session {
    ws: Socket,
    steamid: u64,
    client_sid: i32,
    next_job: u64,
    backlog: Vec<Packet>,
}

#[derive(serde::Deserialize)]
struct CmListResponse {
    response: CmList,
}

#[derive(serde::Deserialize)]
struct CmList {
    #[serde(default)]
    serverlist: Vec<CmServer>,
}

#[derive(serde::Deserialize)]
struct CmServer {
    endpoint: String,
    #[serde(rename = "type")]
    kind: String,
}

/// Fetches product info for the given apps in one anonymous session.
pub async fn fetch_product_info(http: &HttpClient, appids: &[u32]) -> Result<ProductInfo> {
    let servers = cm_servers(http).await?;
    let mut last_err = anyhow!("no Steam CM servers available");
    for endpoint in servers.iter().take(3) {
        match timeout(SESSION_TIMEOUT, run_session(http, endpoint, appids)).await {
            Ok(Ok(info)) => return Ok(info),
            Ok(Err(e)) => last_err = e.context(format!("CM {endpoint}")),
            Err(_) => last_err = anyhow!("CM {endpoint}: session timed out"),
        }
    }
    Err(last_err)
}

async fn cm_servers(http: &HttpClient) -> Result<Vec<String>> {
    let list: CmListResponse = http
        .get_json(
            "https://api.steampowered.com/ISteamDirectory/GetCMListForConnect/v1/?cellid=0&cmtype=websockets&realm=steamglobal",
        )
        .await
        .context("CM server list")?;
    let servers: Vec<String> = list
        .response
        .serverlist
        .into_iter()
        .filter(|s| s.kind == "websockets" && valid_endpoint(&s.endpoint))
        .map(|s| s.endpoint)
        .collect();
    if servers.is_empty() {
        bail!("CM server list is empty");
    }
    Ok(servers)
}

fn valid_endpoint(endpoint: &str) -> bool {
    let Some((host, port)) = endpoint.rsplit_once(':') else {
        return false;
    };
    port.parse::<u16>().is_ok()
        && host.ends_with(".steamserver.net")
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

async fn run_session(http: &HttpClient, endpoint: &str, appids: &[u32]) -> Result<ProductInfo> {
    let url = format!("wss://{endpoint}/cmsocket/");
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME);
    config.max_frame_size = Some(MAX_FRAME);
    let (ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false)
        .await
        .context("connect")?;
    let mut session = Session {
        ws,
        steamid: ANON_STEAM_ID,
        client_sid: 0,
        next_job: 1,
        backlog: Vec::new(),
    };
    session.logon().await?;
    let tokens = session.access_tokens(appids).await?;
    let info = session.product_info(http, appids, &tokens).await;
    let _ = session.ws.close(None).await;
    info
}

impl Session {
    async fn send<M: prost::Message>(
        &mut self,
        emsg: u32,
        body: &M,
        job: Option<u64>,
    ) -> Result<()> {
        let header = ProtoHeader {
            steamid: Some(self.steamid),
            client_sidid: Some(self.client_sid),
            jobid_source: job,
            jobid_target: None,
            eresult: None,
        };
        let header = header.encode_to_vec();
        let body = body.encode_to_vec();
        let mut frame = Vec::with_capacity(8 + header.len() + body.len());
        frame.extend_from_slice(&(emsg | PROTO_MASK).to_le_bytes());
        frame.extend_from_slice(&u32::try_from(header.len())?.to_le_bytes());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(&body);
        self.ws.send(WsMessage::Binary(frame.into())).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Packet> {
        if !self.backlog.is_empty() {
            return Ok(self.backlog.remove(0));
        }
        loop {
            let msg = self
                .ws
                .next()
                .await
                .ok_or_else(|| anyhow!("connection closed"))??;
            let data = match msg {
                WsMessage::Binary(data) => data,
                WsMessage::Close(_) => bail!("connection closed by server"),
                _ => continue,
            };
            let mut packets = Vec::new();
            unpack(&data, &mut packets, 0)?;
            if packets.is_empty() {
                continue;
            }
            let first = packets.remove(0);
            self.backlog.extend(packets);
            return Ok(first);
        }
    }

    async fn logon(&mut self) -> Result<()> {
        let logon = ClientLogon {
            protocol_version: Some(PROTOCOL_VERSION),
            cell_id: Some(0),
            client_language: Some("english".into()),
            client_os_type: Some(16),
        };
        self.send(emsg::CLIENT_LOGON, &logon, None).await?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let packet = timeout_at(deadline, self.recv()).await?;
            match packet.emsg {
                emsg::CLIENT_LOGON_RESPONSE => {
                    let resp = ClientLogonResponse::decode(packet.body.as_slice())?;
                    let eresult = resp.eresult.unwrap_or(2);
                    if eresult != 1 {
                        bail!("anonymous logon failed, EResult {eresult}");
                    }
                    self.steamid = packet.header.steamid.unwrap_or(ANON_STEAM_ID);
                    self.client_sid = packet.header.client_sidid.unwrap_or(0);
                    return Ok(());
                }
                emsg::CLIENT_LOGGED_OFF => bail!("logged off by server"),
                _ => {}
            }
        }
    }

    async fn request(
        &mut self,
        emsg_out: u32,
        body: &impl prost::Message,
        emsg_in: u32,
    ) -> Result<Vec<Packet>> {
        let job = self.next_job;
        self.next_job += 1;
        self.send(emsg_out, body, Some(job)).await?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut out = Vec::new();
        loop {
            let packet = timeout_at(deadline, self.recv()).await?;
            if packet.emsg == emsg::CLIENT_LOGGED_OFF {
                bail!("logged off by server");
            }
            if packet.emsg != emsg_in || packet.header.jobid_target != Some(job) {
                continue;
            }
            let pending = emsg_in == emsg::PICS_PRODUCT_INFO_RESPONSE
                && ProductInfoResponse::decode(packet.body.as_slice())
                    .is_ok_and(|r| r.response_pending.unwrap_or(false));
            out.push(packet);
            if !pending {
                return Ok(out);
            }
        }
    }

    async fn access_tokens(&mut self, appids: &[u32]) -> Result<Vec<(u32, u64)>> {
        let req = AccessTokenRequest {
            appids: appids.to_vec(),
        };
        let packets = self
            .request(
                emsg::PICS_ACCESS_TOKEN_REQUEST,
                &req,
                emsg::PICS_ACCESS_TOKEN_RESPONSE,
            )
            .await?;
        let mut tokens = Vec::new();
        for p in packets {
            let resp = AccessTokenResponse::decode(p.body.as_slice())?;
            for t in resp.app_access_tokens {
                if let (Some(appid), Some(token)) = (t.appid, t.access_token) {
                    tokens.push((appid, token));
                }
            }
        }
        Ok(tokens)
    }

    async fn product_info(
        &mut self,
        http: &HttpClient,
        appids: &[u32],
        tokens: &[(u32, u64)],
    ) -> Result<ProductInfo> {
        let req = ProductInfoRequest {
            apps: appids
                .iter()
                .map(|&appid| ProductInfoRequestApp {
                    appid: Some(appid),
                    access_token: tokens.iter().find(|(a, _)| *a == appid).map(|(_, t)| *t),
                })
                .collect(),
            meta_data_only: Some(false),
            single_response: Some(true),
        };
        let packets = self
            .request(
                emsg::PICS_PRODUCT_INFO_REQUEST,
                &req,
                emsg::PICS_PRODUCT_INFO_RESPONSE,
            )
            .await?;
        let mut info = ProductInfo::default();
        for p in packets {
            let resp = ProductInfoResponse::decode(p.body.as_slice())?;
            info.unknown.extend(resp.unknown_appids);
            for app in resp.apps {
                let appid = app.appid.ok_or_else(|| anyhow!("app info without appid"))?;
                let buffer = match app.buffer {
                    Some(b) if !b.is_empty() => {
                        check_inline(appid, &b, app.sha.as_deref(), app.size)?;
                        b
                    }
                    _ => {
                        fetch_http_buffer(
                            http,
                            resp.http_host.as_deref(),
                            appid,
                            app.sha.as_deref(),
                        )
                        .await?
                    }
                };
                let text = String::from_utf8_lossy(&buffer)
                    .trim_end_matches('\0')
                    .to_owned();
                info.apps.push(RawAppInfo {
                    appid,
                    change_number: app.change_number.unwrap_or(0),
                    text,
                    public_only: app.missing_token == Some(true),
                });
            }
        }
        Ok(info)
    }
}

/// Inline app info must match the size and SHA-1 sent with it; the hash leaves out the closing NUL.
fn check_inline(appid: u32, buf: &[u8], sha: Option<&[u8]>, size: Option<u32>) -> Result<()> {
    if size.is_some_and(|s| usize::try_from(s).ok() != Some(buf.len())) {
        bail!("app {appid}: size mismatch");
    }
    if let Some(sha) = sha {
        let body = buf.strip_suffix(&[0]).unwrap_or(buf);
        let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, body);
        if digest.as_ref() != sha {
            bail!("app {appid}: checksum mismatch");
        }
    }
    Ok(())
}

/// Hosts Steam may name for app info downloads.
fn pics_host_allowed(host: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "steamstatic.com",
        "steampowered.com",
        "steamcontent.com",
        "steamserver.net",
    ];
    let host = host.to_ascii_lowercase();
    SUFFIXES
        .iter()
        .any(|s| host == *s || host.strip_suffix(s).is_some_and(|rest| rest.ends_with('.')))
}

async fn fetch_http_buffer(
    http: &HttpClient,
    host: Option<&str>,
    appid: u32,
    sha: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let host = host.ok_or_else(|| anyhow!("app {appid}: no inline data and no HTTP host"))?;
    let sha = sha.ok_or_else(|| anyhow!("app {appid}: no checksum"))?;
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        bail!("app {appid}: invalid HTTP host");
    }
    if !pics_host_allowed(host) {
        bail!("app {appid}: HTTP host is not a Steam host");
    }
    let hex = sha.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    let url = format!("https://{host}/appinfo/{appid}/sha/{hex}.txt.gz");
    let gz = http.get_bytes_any_host(&url).await?;
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(gz.as_ref())
        .take(MAX_UNZIPPED as u64)
        .read_to_end(&mut text)?;
    let sha1 = |b: &[u8]| ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, b);
    let stripped = text.strip_suffix(&[0]).unwrap_or(&text);
    if sha1(&text).as_ref() != sha && sha1(stripped).as_ref() != sha {
        bail!("app {appid}: checksum mismatch");
    }
    Ok(text)
}

async fn timeout_at<T>(
    deadline: Instant,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout_at(deadline, fut)
        .await
        .map_err(|_| anyhow!("timed out waiting for Steam"))?
}

fn unpack(data: &[u8], out: &mut Vec<Packet>, depth: u32) -> Result<()> {
    if depth > 2 {
        bail!("nested Multi too deep");
    }
    let raw_emsg = read_u32(data, 0)?;
    if raw_emsg & PROTO_MASK == 0 {
        return Ok(());
    }
    let emsg = raw_emsg & !PROTO_MASK;
    let header_len = usize::try_from(read_u32(data, 4)?)?;
    let header_end = 8usize
        .checked_add(header_len)
        .filter(|&e| e <= data.len())
        .ok_or_else(|| anyhow!("bad header length"))?;
    let header = ProtoHeader::decode(data.get(8..header_end).unwrap_or_default())?;
    let body = data.get(header_end..).unwrap_or_default();
    if emsg != emsg::MULTI {
        out.push(Packet {
            emsg,
            header,
            body: body.to_vec(),
        });
        return Ok(());
    }
    let multi = Multi::decode(body)?;
    let payload = multi.message_body.unwrap_or_default();
    let payload = if multi.size_unzipped.unwrap_or(0) > 0 {
        let mut buf = Vec::new();
        flate2::read::GzDecoder::new(payload.as_slice())
            .take(MAX_UNZIPPED as u64)
            .read_to_end(&mut buf)?;
        buf
    } else {
        payload
    };
    let mut pos = 0usize;
    while pos < payload.len() {
        let len = usize::try_from(read_u32(&payload, pos)?)?;
        let start = pos + 4;
        let end = start
            .checked_add(len)
            .filter(|&e| e <= payload.len())
            .ok_or_else(|| anyhow!("bad Multi item length"))?;
        unpack(payload.get(start..end).unwrap_or_default(), out, depth + 1)?;
        pos = end;
    }
    Ok(())
}

fn read_u32(data: &[u8], at: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| anyhow!("truncated message"))?;
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_info_is_checked() {
        let text = b"\"appinfo\" {}";
        let mut buf = text.to_vec();
        buf.push(0);
        let sha = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, text);
        let len = u32::try_from(buf.len()).unwrap();
        assert!(check_inline(1, &buf, Some(sha.as_ref()), Some(len)).is_ok());
        assert!(check_inline(1, &buf, None, None).is_ok());
        assert!(check_inline(1, &buf, Some(sha.as_ref()), Some(len + 1)).is_err());
        assert!(check_inline(1, &buf, Some(&[0u8; 20]), Some(len)).is_err());
    }

    #[test]
    fn pics_hosts() {
        assert!(pics_host_allowed("clientconfig.akamai.steamstatic.com"));
        assert!(pics_host_allowed("steampowered.com"));
        assert!(!pics_host_allowed("evilsteamstatic.com"));
        assert!(!pics_host_allowed("steamstatic.com.evil.net"));
        assert!(!pics_host_allowed("127.0.0.1"));
        assert!(!pics_host_allowed("localhost"));
    }

    #[test]
    fn endpoint_validation() {
        assert!(valid_endpoint("cmp1-fra2.steamserver.net:27020"));
        assert!(!valid_endpoint("evil.com:443"));
        assert!(!valid_endpoint("cmp1.steamserver.net"));
        assert!(!valid_endpoint("a/b.steamserver.net:1"));
    }

    #[test]
    fn unpack_rejects_garbage() {
        let mut out = Vec::new();
        assert!(unpack(&[1, 2], &mut out, 0).is_err());
        let mut frame = (PROTO_MASK | 0x2EF).to_le_bytes().to_vec();
        frame.extend_from_slice(&1000u32.to_le_bytes());
        assert!(unpack(&frame, &mut out, 0).is_err());
    }

    #[test]
    fn unpack_multi() {
        let inner_header = ProtoHeader {
            jobid_target: Some(7),
            ..Default::default()
        }
        .encode_to_vec();
        let mut inner = (PROTO_MASK | 0x22CA).to_le_bytes().to_vec();
        inner.extend_from_slice(&u32::try_from(inner_header.len()).unwrap().to_le_bytes());
        inner.extend_from_slice(&inner_header);
        inner.extend_from_slice(b"body");
        let mut payload = u32::try_from(inner.len()).unwrap().to_le_bytes().to_vec();
        payload.extend_from_slice(&inner);
        let multi = Multi {
            size_unzipped: None,
            message_body: Some(payload),
        }
        .encode_to_vec();
        let mut frame = (PROTO_MASK | 0x1).to_le_bytes().to_vec();
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&multi);
        let mut out = Vec::new();
        unpack(&frame, &mut out, 0).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].emsg, 8906);
        assert_eq!(out[0].header.jobid_target, Some(7));
        assert_eq!(out[0].body, b"body");
    }
}
