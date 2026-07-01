//! Minimal RAOP RTSP client (the TCP control channel).
//!
//! Implements just the verbs a realtime, unencrypted AirPlay-1 session needs:
//! OPTIONS, ANNOUNCE, SETUP, RECORD, SET_PARAMETER (volume), TEARDOWN.
//! No encryption (et=0) and no Apple-Challenge — confirmed sufficient for the
//! target Linkplay/WiiMu speaker.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::codec::Codec;

/// Ports the device tells us to use (from the SETUP response).
#[derive(Debug, Clone, Copy)]
pub struct DevicePorts {
    pub server: u16,  // where we send RTP audio
    pub control: u16, // where we send SYNC / resend replies
    pub timing: u16,  // where we send timing replies
}

struct Response {
    status: u16,
    headers: HashMap<String, String>,
    #[allow(dead_code)]
    body: Vec<u8>,
}

impl Response {
    fn check(&self, method: &str) -> io::Result<()> {
        if (200..300).contains(&self.status) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{} failed: RTSP status {}", method, self.status),
            ))
        }
    }
}

pub struct Rtsp {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    cseq: u32,
    uri: String,
    /// Persistent headers sent on every request.
    base_headers: Vec<(String, String)>,
    /// Session id returned by SETUP, echoed on later requests.
    session: String,
}

impl Rtsp {
    pub fn connect(addr: SocketAddr, client_ip: IpAddr) -> io::Result<Rtsp> {
        let writer = TcpStream::connect(addr)?;
        writer.set_read_timeout(Some(Duration::from_secs(10)))?;
        writer.set_nodelay(true).ok();
        let reader = BufReader::new(writer.try_clone()?);

        let sid: u32 = rand::random();
        let uri = format!("rtsp://{}/{}", client_ip, sid);
        let dacp: u64 = rand::random();
        let active: u32 = rand::random();
        let ci: u64 = rand::random();

        let base_headers = vec![
            ("User-Agent".into(), "iTunes/11.0.5 (Windows; N)".into()),
            ("Client-Instance".into(), format!("{:016X}", ci)),
            ("DACP-ID".into(), format!("{:016X}", dacp)),
            ("Active-Remote".into(), format!("{}", active)),
        ];

        Ok(Rtsp {
            writer,
            reader,
            cseq: 0,
            uri,
            base_headers,
            session: String::new(),
        })
    }

    fn request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        content_type: Option<&str>,
        body: &[u8],
    ) -> io::Result<Response> {
        self.cseq += 1;
        let mut req = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\n", method, uri, self.cseq);
        for (k, v) in &self.base_headers {
            req.push_str(&format!("{}: {}\r\n", k, v));
        }
        for (k, v) in extra {
            req.push_str(&format!("{}: {}\r\n", k, v));
        }
        if let Some(ct) = content_type {
            req.push_str(&format!("Content-Type: {}\r\n", ct));
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");

        eprintln!("raop_send: >> {} {} (CSeq {}, {} body bytes)", method, uri, self.cseq, body.len());
        self.writer.write_all(req.as_bytes())?;
        if !body.is_empty() {
            self.writer.write_all(body)?;
        }
        self.writer.flush()?;
        let resp = self.read_response().map_err(|e| {
            eprintln!("raop_send: << {} FAILED waiting for response: {}", method, e);
            e
        })?;
        eprintln!(
            "raop_send: << {} {} ({} headers, {} body bytes)",
            method,
            resp.status,
            resp.headers.len(),
            resp.body.len()
        );
        Ok(resp)
    }

    fn read_response(&mut self) -> io::Result<Response> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed by device",
            ));
        }
        // "RTSP/1.0 200 OK"
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "bad status line"))?;
        eprintln!("raop_send:    status line: {}", line.trim_end());

        let mut headers = HashMap::new();
        loop {
            let mut l = String::new();
            if self.reader.read_line(&mut l)? == 0 {
                break;
            }
            let t = l.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some((k, v)) = t.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let mut body = Vec::new();
        if let Some(len) = headers
            .get("content-length")
            .and_then(|v| v.parse::<usize>().ok())
        {
            body.resize(len, 0);
            self.reader.read_exact(&mut body)?;
        }

        Ok(Response {
            status,
            headers,
            body,
        })
    }

    pub fn options(&mut self) -> io::Result<()> {
        self.request("OPTIONS", "*", &[], None, &[])?.check("OPTIONS")
    }

    pub fn announce(
        &mut self,
        client_ip: IpAddr,
        device_ip: IpAddr,
        sample_rate: u32,
        codec: Codec,
    ) -> io::Result<()> {
        // Unencrypted session: no a=rsaaeskey / a=aesiv lines. The media
        // description MUST match what run_audio actually sends, otherwise the
        // receiver decodes the wrong format and produces noise/silence.
        let media = match codec {
            Codec::Alac => format!(
                "m=audio 0 RTP/AVP 96\r\n\
                 a=rtpmap:96 AppleLossless\r\n\
                 a=fmtp:96 352 0 16 40 10 14 2 255 0 0 {sr}\r\n",
                sr = sample_rate,
            ),
            // Raw 16-bit big-endian PCM (cn=0). Experimental: not all RAOP
            // servers accept an L16 announce on the legacy path; ALAC is the
            // interoperable default.
            Codec::Pcm => format!(
                "m=audio 0 RTP/AVP 96\r\n\
                 a=rtpmap:96 L16/{sr}/2\r\n",
                sr = sample_rate,
            ),
        };
        let sid = self.uri.rsplit('/').next().unwrap_or("0").to_string();
        let sdp = format!(
            "v=0\r\n\
             o=iTunes {sid} 0 IN IP4 {client}\r\n\
             s=iTunes\r\n\
             c=IN IP4 {device}\r\n\
             t=0 0\r\n\
             {media}",
            sid = sid,
            client = client_ip,
            device = device_ip,
            media = media,
        );
        let uri = self.uri.clone();
        self.request("ANNOUNCE", &uri, &[], Some("application/sdp"), sdp.as_bytes())?
            .check("ANNOUNCE")
    }

    pub fn setup(&mut self, control_port: u16, timing_port: u16) -> io::Result<DevicePorts> {
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
            control_port, timing_port
        );
        let uri = self.uri.clone();
        let resp = self.request("SETUP", &uri, &[("Transport", &transport)], None, &[])?;
        resp.check("SETUP")?;

        if let Some(s) = resp.headers.get("session") {
            self.session = s.clone();
        }
        let transport = resp
            .headers
            .get("transport")
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "SETUP missing Transport"))?;

        let find = |key: &str| -> Option<u16> {
            transport
                .split(';')
                .find_map(|p| p.trim().strip_prefix(key))
                .and_then(|v| v.parse::<u16>().ok())
        };

        Ok(DevicePorts {
            server: find("server_port=")
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no server_port"))?,
            control: find("control_port=")
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no control_port"))?,
            timing: find("timing_port=")
                .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no timing_port"))?,
        })
    }

    pub fn record(&mut self, seq: u16, rtptime: u32) -> io::Result<()> {
        let rtp_info = format!("seq={};rtptime={}", seq, rtptime);
        let session = self.session.clone();
        let uri = self.uri.clone();
        self.request(
            "RECORD",
            &uri,
            &[
                ("Range", "npt=0-"),
                ("RTP-Info", &rtp_info),
                ("Session", &session),
            ],
            None,
            &[],
        )?
        .check("RECORD")
    }

    pub fn set_volume(&mut self, db: f32) -> io::Result<()> {
        let body = format!("volume: {:.6}\r\n", db);
        let session = self.session.clone();
        let uri = self.uri.clone();
        self.request(
            "SET_PARAMETER",
            &uri,
            &[("Session", &session)],
            Some("text/parameters"),
            body.as_bytes(),
        )?
        .check("SET_PARAMETER")
    }

    /// Lightweight keep-alive so an idle RTSP TCP connection is not reaped.
    pub fn keepalive(&mut self) -> io::Result<()> {
        self.options()
    }

    pub fn teardown(&mut self) -> io::Result<()> {
        let session = self.session.clone();
        let uri = self.uri.clone();
        let _ = self.request("TEARDOWN", &uri, &[("Session", &session)], None, &[]);
        Ok(())
    }
}
