//! Minimal one-shot mDNS-SD resolver for `_raop._tcp` (RFC 6762/6763), just
//! enough to turn a friendly AirPlay instance name (e.g. "Kontoret") into an
//! address + port so `--host` can take a name instead of an IP.
//!
//! Queries request a *unicast* reply (the "QU" bit on the question's class)
//! so we can listen on an ephemeral UDP port instead of binding 5353, which
//! avahi-daemon already holds on most Linux boxes. A compliant responder
//! (avahi, Bonjour) answers a `_raop._tcp.local` PTR query with the PTR plus
//! the matching SRV/A records bundled in as additional data (RFC 6763 §12),
//! so one round trip is normally enough; if a responder omits them we send
//! one bounded follow-up query for just that instance's SRV + A.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

const MDNS_ADDR: &str = "224.0.0.251:5353";
const SERVICE: &str = "_raop._tcp.local";

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_SRV: u16 = 33;

const CLASS_IN_QU: u16 = 0x8001; // IN, with the unicast-response bit set

pub struct Discovered {
    pub addr: Ipv4Addr,
    pub port: u16,
    pub instance: String,
}

/// Resolve `name` against `_raop._tcp` instance names on the LAN. Matching
/// is case-insensitive and ignores any "xx:xx:xx:xx:xx:xx@" device-id prefix
/// some RAOP servers put in front of the friendly name.
pub fn resolve(name: &str, timeout: Duration) -> io::Result<Discovered> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(300)))?;

    send_query(&sock, &[(SERVICE, TYPE_PTR)])?;

    let mut db = RecordDb::default();
    let mut buf = [0u8; 4096];

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if recv_into(&sock, &mut buf, &mut db) {
            if let Some(found) = db.find(name) {
                return Ok(found);
            }
        }
    }

    // Responder gave us a matching PTR but not (yet) its SRV/A — ask
    // directly, once, with a short bounded window.
    if let Some(instance) = db.matching_instance(name) {
        send_query(&sock, &[(&instance, TYPE_SRV), (&instance, TYPE_A)])?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if recv_into(&sock, &mut buf, &mut db) {
                if let Some(found) = db.find(name) {
                    return Ok(found);
                }
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no _raop._tcp service matching \"{}\" found via mDNS", name),
    ))
}

fn recv_into(sock: &UdpSocket, buf: &mut [u8], db: &mut RecordDb) -> bool {
    match sock.recv_from(buf) {
        Ok((n, _)) => {
            db.ingest(&buf[..n]);
            true
        }
        Err(_) => false, // timeout (WouldBlock/TimedOut) or a transient recv error
    }
}

fn send_query(sock: &UdpSocket, questions: &[(&str, u16)]) -> io::Result<()> {
    sock.send_to(&build_query(questions), MDNS_ADDR)?;
    Ok(())
}

fn build_query(questions: &[(&str, u16)]) -> Vec<u8> {
    let id: u16 = rand::random();
    let mut msg = Vec::with_capacity(64);
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes()); // flags: standard query
    msg.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes()); // ancount
    msg.extend_from_slice(&0u16.to_be_bytes()); // nscount
    msg.extend_from_slice(&0u16.to_be_bytes()); // arcount
    for (qname, qtype) in questions {
        for label in qname.split('.') {
            msg.push(label.len() as u8);
            msg.extend_from_slice(label.as_bytes());
        }
        msg.push(0);
        msg.extend_from_slice(&qtype.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN_QU.to_be_bytes());
    }
    msg
}

/// Domain name as a sequence of raw labels — kept unsplit so a label
/// containing a literal '.' (rare, but legal in DNS) can't be confused with
/// a label boundary. Used as a HashMap key to correlate PTR -> SRV -> A.
type Labels = Vec<String>;

#[derive(Default)]
struct RecordDb {
    ptr: Vec<Labels>,
    srv: HashMap<Labels, (u16, Labels)>,
    a: HashMap<Labels, Ipv4Addr>,
}

impl RecordDb {
    fn ingest(&mut self, buf: &[u8]) {
        if buf.len() < 12 {
            return;
        }
        let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
        let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
        let nscount = u16::from_be_bytes([buf[8], buf[9]]) as usize;
        let arcount = u16::from_be_bytes([buf[10], buf[11]]) as usize;

        let mut pos = 12usize;
        for _ in 0..qdcount {
            if read_name(buf, &mut pos).is_none() || pos + 4 > buf.len() {
                return;
            }
            pos += 4; // qtype + qclass
        }

        for _ in 0..(ancount + nscount + arcount) {
            let name = match read_name(buf, &mut pos) {
                Some(n) => n,
                None => return,
            };
            if pos + 10 > buf.len() {
                return;
            }
            let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
            pos += 2;
            pos += 2; // rclass
            pos += 4; // ttl
            let rdlen = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
            pos += 2;
            if pos + rdlen > buf.len() {
                return;
            }
            let rdata = &buf[pos..pos + rdlen];

            match rtype {
                TYPE_PTR => {
                    let mut p = pos;
                    if let Some(target) = read_name(buf, &mut p) {
                        self.ptr.push(target);
                    }
                }
                TYPE_SRV if rdata.len() >= 6 => {
                    let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                    let mut p = pos + 6;
                    if let Some(target) = read_name(buf, &mut p) {
                        self.srv.insert(name, (port, target));
                    }
                }
                TYPE_A if rdata.len() == 4 => {
                    self.a
                        .insert(name, Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]));
                }
                _ => {}
            }

            pos += rdlen;
        }
    }

    fn find(&self, name: &str) -> Option<Discovered> {
        for instance in &self.ptr {
            if !friendly_matches(instance, name) {
                continue;
            }
            let (port, target) = self.srv.get(instance)?;
            let addr = self.a.get(target)?;
            return Some(Discovered {
                addr: *addr,
                port: *port,
                instance: instance.join("."),
            });
        }
        None
    }

    fn matching_instance(&self, name: &str) -> Option<String> {
        self.ptr
            .iter()
            .find(|i| friendly_matches(i, name))
            .map(|i| i.join("."))
    }
}

/// `instance` is e.g. `["AABBCCDDEEFF@Kontoret", "_raop", "_tcp", "local"]`.
/// The friendly name is the first label with any "xx@" device-id prefix
/// stripped.
fn friendly_matches(instance: &[String], name: &str) -> bool {
    let first = match instance.first() {
        Some(l) => l.as_str(),
        None => return false,
    };
    let friendly = match first.rfind('@') {
        Some(i) => &first[i + 1..],
        None => first,
    };
    friendly.eq_ignore_ascii_case(name)
}

/// Read a DNS name at `buf[*pos..]`, following compression pointers, and
/// advance `*pos` past the (possibly-compressed) name in the original
/// stream. Returns raw labels (see `Labels`).
fn read_name(buf: &[u8], pos: &mut usize) -> Option<Labels> {
    let mut labels = Vec::new();
    let mut cur = *pos;
    let mut after_pointer: Option<usize> = None;
    let mut jumps = 0;

    loop {
        let len = *buf.get(cur)? as usize;
        if len == 0 {
            cur += 1;
            break;
        } else if len & 0xC0 == 0xC0 {
            let lo = *buf.get(cur + 1)? as usize;
            if after_pointer.is_none() {
                after_pointer = Some(cur + 2);
            }
            jumps += 1;
            if jumps > 64 {
                return None; // guard against a pointer loop
            }
            cur = ((len & 0x3F) << 8) | lo;
        } else {
            let start = cur + 1;
            let end = start + len;
            if end > buf.len() {
                return None;
            }
            labels.push(String::from_utf8_lossy(&buf[start..end]).into_owned());
            cur = end;
        }
    }

    *pos = after_pointer.unwrap_or(cur);
    Some(labels)
}
