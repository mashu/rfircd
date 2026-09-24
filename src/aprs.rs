//! APRS message codec.
//!
//! AIRC is the native on-air protocol; this exists so a stock APRS radio
//! (Kenwood, Yaesu, APRSdroid, …) can put a line into a bridged channel
//! without speaking AIRC, and so positions and status beacons heard on
//! frequency can be shown on IRC (receive only — never retransmitted).
//!
//! Wire format (APRS 1.0.1 §14), information field of an AX.25 UI frame:
//!
//! ```text
//! :ADDRESSEE:message text{msgid
//! ```
//!
//! The addressee is nine characters, left-justified and space-padded. The
//! message ID is optional, 1–5 alphanumeric characters. A radio that sent
//! one will retry until it hears `ack<msgid>` addressed back to it; not
//! answering is therefore *more* airtime than the ACK.

use crate::callsign::Callsign;
use crate::irc::message::is_channel_name;

/// AX.25 destination used for APRS UI frames we transmit.
///
/// The addressee lives in the information field, not in the AX.25 header.
/// Stock radios scan every UI frame on frequency for a message to them.
pub fn ax25_destination() -> Callsign {
    Callsign::new("APRS", 0).expect("APRS is a valid AX.25 address")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AprsKind {
    /// Ordinary text, possibly destined for a channel.
    Message,
    /// `ack<msgid>` — a station confirming it got one of ours. Ignored.
    Ack,
    /// `rej<msgid>` — a station refusing one of ours. Ignored.
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AprsMessage {
    pub addressee: String,
    pub text: String,
    pub msgid: Option<String>,
    pub kind: AprsKind,
}

impl AprsMessage {
    /// Parse an AX.25 information field. `None` means this is not an APRS
    /// message (a position report, Mic-E, third-party wrap, noise, …).
    pub fn decode(info: &[u8]) -> Option<Self> {
        let info = trim_trailing_crlf(info);
        if info.first() != Some(&b':') {
            return None;
        }
        // Third-party traffic is `}SRC>DEST:payload`. A leading colon is
        // enough to reject that; a wrapped message would start with `}`.
        let rest = &info[1..];
        // Spec: nine-character addressee then a colon. Some TNCs omit the
        // padding; accept either.
        let (addr, body) = if rest.len() >= 10 && rest[9] == b':' {
            (&rest[..9], &rest[10..])
        } else {
            let colon = rest.iter().position(|&b| b == b':')?;
            if colon == 0 || colon > 9 {
                return None;
            }
            (&rest[..colon], &rest[colon + 1..])
        };
        let addressee = {
            if !addr
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b' ')
            {
                return None;
            }
            let s = std::str::from_utf8(addr).ok()?.trim().to_ascii_uppercase();
            if s.is_empty() {
                return None;
            }
            s
        };
        let body = String::from_utf8_lossy(body);
        let (text, msgid) = split_msgid(body.trim_end());
        let kind = classify(&text);
        Some(Self {
            addressee,
            text,
            msgid,
            kind,
        })
    }

    /// Encode as an information field, padded addressee, no trailing CR.
    pub fn encode(&self) -> Vec<u8> {
        let mut addr = self.addressee.clone();
        addr.truncate(9);
        while addr.len() < 9 {
            addr.push(' ');
        }
        debug_assert_eq!(addr.len(), 9);
        let mut out = String::with_capacity(11 + self.text.len() + 6);
        out.push(':');
        out.push_str(&addr);
        out.push(':');
        out.push_str(&self.text);
        if let Some(id) = &self.msgid {
            out.push('{');
            out.push_str(id);
        }
        out.into_bytes()
    }

    /// True when this message is for `gateway`.
    ///
    /// Exact match, or the addressee is the same station with SSID 0
    /// (`SK0MT` for a gateway `SK0MT-1`). Handheld UIs make omitting the
    /// SSID the common case; `SK0MT-2` is someone else.
    pub fn addressed_to(&self, gateway: &Callsign) -> bool {
        let Ok(dest) = self.addressee.parse::<Callsign>() else {
            return false;
        };
        dest == *gateway || (dest.ssid() == 0 && dest.same_station(gateway))
    }

    pub fn is_ack_or_rej(&self) -> bool {
        matches!(self.kind, AprsKind::Ack | AprsKind::Reject)
    }
}

/// A position or status beacon. Shown on IRC, never retransmitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AprsBeacon {
    /// One line an operator can read without an APRS decoder.
    pub summary: String,
}

impl AprsBeacon {
    /// Parse a position (`! = / @` uncompressed, compressed, or Mic-E) or a
    /// status (`>`) beacon. `None` is anything else: messages, objects, AIRC.
    pub fn decode(info: &[u8]) -> Option<Self> {
        let info = trim_trailing_crlf(info);
        if info.is_empty() {
            return None;
        }
        let dtype = info[0];
        match dtype {
            b':' | b';' | b')' | b'}' | b'T' | b'{' | b'`' | b'\'' => return None,
            b'>' => {
                let text = status_text(&info[1..]);
                if text.is_empty() {
                    return None;
                }
                return Some(Self { summary: text });
            }
            b'!' | b'=' | b'/' | b'@' | b'_' => {}
            _ => return None,
        }
        let rest = match dtype {
            b'/' | b'@' if info.len() > 8 => &info[8..],
            _ => &info[1..],
        };
        let summary = decode_position(rest).or_else(|| {
            // Weather `_` and odd beacons: show the printable remainder.
            let text = printable_comment(rest);
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })?;
        Some(Self { summary })
    }
}

/// Position, status, or Mic-E. Mic-E latitude is in the AX.25 destination.
pub fn decode_beacon(dest: &Callsign, info: &[u8]) -> Option<AprsBeacon> {
    let info = trim_trailing_crlf(info);
    if info.first().is_some_and(|b| matches!(b, b'`' | b'\'')) {
        let summary = decode_mice_frame(dest.base(), info)?;
        return Some(AprsBeacon { summary });
    }
    AprsBeacon::decode(info)
}

/// ACK the sender so a stock radio stops retrying.
pub fn ack_info(to: &Callsign, msgid: &str) -> Vec<u8> {
    AprsMessage {
        addressee: to.to_string(),
        text: format!("ack{msgid}"),
        msgid: None,
        kind: AprsKind::Ack,
    }
    .encode()
}

/// REJ: we received it and will not act on it (kicked, refused).
pub fn rej_info(to: &Callsign, msgid: &str) -> Vec<u8> {
    AprsMessage {
        addressee: to.to_string(),
        text: format!("rej{msgid}"),
        msgid: None,
        kind: AprsKind::Reject,
    }
    .encode()
}

/// A one-shot text reply. No message ID: we do not want an ack-ack.
pub fn reply_info(to: &Callsign, text: &str) -> Vec<u8> {
    let text: String = text.chars().take(67).collect();
    AprsMessage {
        addressee: to.to_string(),
        text,
        msgid: None,
        kind: AprsKind::Message,
    }
    .encode()
}

/// Chat text with a message ID so the far radio will ACK (and we can retry).
pub fn message_info(to: &Callsign, text: &str, msgid: &str) -> Vec<u8> {
    let text: String = text.chars().take(67).collect();
    AprsMessage {
        addressee: to.to_string(),
        text,
        msgid: Some(msgid.to_string()),
        kind: AprsKind::Message,
    }
    .encode()
}

/// `ackNN` / `rejNN` payload id, if this is a control reply to one of ours.
pub fn control_msgid(msg: &AprsMessage) -> Option<&str> {
    let t = msg.text.trim();
    match msg.kind {
        AprsKind::Ack => t.strip_prefix("ack").filter(|id| msgid_ok(id)),
        AprsKind::Reject => t.strip_prefix("rej").filter(|id| msgid_ok(id)),
        AprsKind::Message => None,
    }
}

/// Split `#channel body` from an APRS message body.
///
/// If the text does not start with a channel name and `default_channel` is
/// a channel name, the whole text goes there. Empty body is not a line.
pub fn split_channel_line<'a>(
    text: &'a str,
    default_channel: &'a str,
) -> Option<(&'a str, &'a str)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((first, rest)) = text.split_once(' ') {
        if is_channel_name(first) {
            let rest = rest.trim();
            if rest.is_empty() {
                return None;
            }
            return Some((first, rest));
        }
    } else if is_channel_name(text) {
        return None;
    }
    if is_channel_name(default_channel) {
        return Some((default_channel, text));
    }
    None
}

/// `?` / `HELP` — the radio is asking how to talk to us, not sending a line.
pub fn is_help(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t == "?" || t.eq_ignore_ascii_case("help") || t.eq_ignore_ascii_case("h")
}

fn trim_trailing_crlf(info: &[u8]) -> &[u8] {
    let mut end = info.len();
    while end > 0 && matches!(info[end - 1], b'\r' | b'\n' | 0) {
        end -= 1;
    }
    &info[..end]
}

fn split_msgid(body: &str) -> (String, Option<String>) {
    if let Some(i) = body.rfind('{') {
        let rest = &body[i + 1..];
        let rest = rest.strip_suffix('}').unwrap_or(rest);
        if msgid_ok(rest) {
            return (body[..i].to_string(), Some(rest.to_string()));
        }
    }
    (body.to_string(), None)
}

fn msgid_ok(id: &str) -> bool {
    let n = id.len();
    (1..=5).contains(&n) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn classify(text: &str) -> AprsKind {
    let t = text.trim();
    if let Some(id) = t.strip_prefix("ack") {
        if msgid_ok(id) {
            return AprsKind::Ack;
        }
    }
    if let Some(id) = t.strip_prefix("rej") {
        if msgid_ok(id) {
            return AprsKind::Reject;
        }
    }
    AprsKind::Message
}

fn status_text(body: &[u8]) -> String {
    // Optional DHM timestamp: 7 chars ending in z/h//.
    let body = if body.len() >= 8 && matches!(body[6], b'z' | b'h' | b'/') {
        &body[7..]
    } else {
        body
    };
    printable_comment(body)
}

fn decode_position(rest: &[u8]) -> Option<String> {
    if uncompressed_lat(rest) {
        return decode_uncompressed(rest);
    }
    decode_compressed(rest)
}

fn uncompressed_lat(rest: &[u8]) -> bool {
    rest.len() >= 8
        && rest[0..4].iter().all(u8::is_ascii_digit)
        && rest[4] == b'.'
        && rest[5..7].iter().all(u8::is_ascii_digit)
        && matches!(rest[7], b'N' | b'S' | b'n' | b's')
}

fn decode_uncompressed(rest: &[u8]) -> Option<String> {
    // ddmm.mmN s dddmm.mmW S comment
    if rest.len() < 19 {
        return None;
    }
    let lat = std::str::from_utf8(&rest[0..8]).ok()?;
    let lon = std::str::from_utf8(&rest[9..18]).ok()?;
    if !matches!(rest[8], b'/' | b'\\') {
        return None;
    }
    if lon.len() != 9 || !matches!(lon.as_bytes()[8], b'E' | b'W' | b'e' | b'w') {
        return None;
    }
    let lat = format_uncomp_coord(lat, 2)?;
    let lon = format_uncomp_coord(lon, 3)?;
    let comment = printable_comment(&rest[19..]);
    if comment.is_empty() {
        Some(format!("{lat} {lon}"))
    } else {
        Some(format!("{lat} {lon} - {comment}"))
    }
}

fn format_uncomp_coord(raw: &str, deg_len: usize) -> Option<String> {
    // `deg_len` and the slices below are byte offsets, so a multi-byte
    // character straddling one of them is a panic rather than a rejected
    // frame. Only the latitude is checked by `uncompressed_lat`; the
    // longitude arrives as nine arbitrary octets that merely happened to be
    // valid UTF-8, and a pair such as 0xC3 0xA9 puts a continuation byte
    // exactly where the degrees end. A coordinate is digits and a
    // hemisphere letter, so anything outside ASCII is not one.
    if !raw.is_ascii() {
        return None;
    }
    let hemi = raw.chars().last()?;
    let body = &raw[..raw.len() - 1];
    if body.len() < deg_len + 3 {
        return None;
    }
    let deg = &body[..deg_len];
    let min = &body[deg_len..];
    Some(format!("{deg}°{min}{hemi}"))
}

fn decode_compressed(rest: &[u8]) -> Option<String> {
    // symbol-table yyyy xxxx symbol [cs t] comment
    if rest.len() < 10 {
        return None;
    }
    if !matches!(rest[0], b'/' | b'\\') {
        return None;
    }
    let lat = 90.0 - base91(&rest[1..5])? as f64 / 380926.0;
    let lon = base91(&rest[5..9])? as f64 / 190463.0 - 180.0;
    let comment = printable_comment(&rest[10..]);
    let pos = format_decimal(lat, lon);
    if comment.is_empty() {
        Some(pos)
    } else {
        Some(format!("{pos} - {comment}"))
    }
}

fn base91(s: &[u8]) -> Option<u32> {
    if s.len() != 4 {
        return None;
    }
    let mut n = 0u32;
    for &c in s {
        if !(33..=124).contains(&c) {
            return None;
        }
        n = n.checked_mul(91)?.checked_add(u32::from(c - 33))?;
    }
    Some(n)
}

fn format_decimal(lat: f64, lon: f64) -> String {
    let ns = if lat >= 0.0 { 'N' } else { 'S' };
    let ew = if lon >= 0.0 { 'E' } else { 'W' };
    format!("{:.2}°{ns} {:.2}°{ew}", lat.abs(), lon.abs())
}

/// Mic-E needs the destination callsign (latitude lives there) as well as
/// the information field.
pub fn decode_mice_frame(dest_base: &str, info: &[u8]) -> Option<String> {
    let info = trim_trailing_crlf(info);
    if info.len() < 8 || !matches!(info[0], b'`' | b'\'') {
        return None;
    }
    let dest: Vec<u8> = dest_base
        .as_bytes()
        .iter()
        .copied()
        .filter(|&b| b != b' ')
        .chain(std::iter::repeat(b' '))
        .take(6)
        .collect();
    if dest.len() < 6 {
        return None;
    }
    let mut lat_d = [0u8; 6];
    let mut flags = [false; 6];
    for i in 0..6 {
        let (d, f) = mice_digit(dest[i])?;
        lat_d[i] = d;
        flags[i] = f;
    }
    let lat = mice_lat(lat_d, flags[3]);
    let lon = mice_lon(info, flags[4], flags[5])?;
    let comment = printable_comment(info.get(8..).unwrap_or(&[]));
    let pos = format!("{lat} {lon}");
    if comment.is_empty() {
        Some(pos)
    } else {
        Some(format!("{pos} - {comment}"))
    }
}

fn mice_digit(c: u8) -> Option<(u8, bool)> {
    match c {
        b'0'..=b'9' => Some((c - b'0', false)),
        b'A'..=b'J' => Some((c - b'A', true)),
        b'P'..=b'Y' => Some((c - b'P', true)),
        b'K' | b'Z' => Some((0, true)),
        b'L' => Some((0, false)),
        _ => None,
    }
}

fn mice_lat(d: [u8; 6], north: bool) -> String {
    let hemi = if north { 'N' } else { 'S' };
    format!("{}{}°{}{}.{}{}{hemi}", d[0], d[1], d[2], d[3], d[4], d[5])
}

fn mice_lon(info: &[u8], offset: bool, west: bool) -> Option<String> {
    let mut deg = i16::from(info[1]) - 28;
    if offset {
        deg += 100;
    }
    if (180..=189).contains(&deg) {
        deg -= 80;
    } else if (190..=199).contains(&deg) {
        deg -= 190;
    }
    let mut min = i16::from(info[2]) - 28;
    if min >= 60 {
        min -= 60;
    }
    let hund = i16::from(info[3]) - 28;
    if !(0..=179).contains(&deg) || !(0..=59).contains(&min) || !(0..=99).contains(&hund) {
        return None;
    }
    let hemi = if west { 'W' } else { 'E' };
    Some(format!("{deg:03}°{min:02}.{hund:02}{hemi}"))
}

fn printable_comment(raw: &[u8]) -> String {
    let s: String = raw
        .iter()
        .copied()
        .filter(|&b| (0x20..=0x7e).contains(&b))
        .map(char::from)
        .collect();
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw() -> Callsign {
        "SK0MT-1".parse().unwrap()
    }

    #[test]
    fn padded_message_with_msgid() {
        let m = AprsMessage::decode(b":SK0MT-1  :#rf hello{01").unwrap();
        assert_eq!(m.addressee, "SK0MT-1");
        assert_eq!(m.text, "#rf hello");
        assert_eq!(m.msgid.as_deref(), Some("01"));
        assert_eq!(m.kind, AprsKind::Message);
        assert!(m.addressed_to(&gw()));
    }

    #[test]
    fn unpadded_addressee() {
        let m = AprsMessage::decode(b":SK0MT-1:#rf hello").unwrap();
        assert_eq!(m.addressee, "SK0MT-1");
        assert_eq!(m.text, "#rf hello");
        assert!(m.msgid.is_none());
    }

    #[test]
    fn ssid_zero_matches_gateway() {
        let m = AprsMessage::decode(b":SK0MT    :hello{A1").unwrap();
        assert!(m.addressed_to(&gw()));
        assert!(!m.addressed_to(&"SK0AA-1".parse().unwrap()));
    }

    #[test]
    fn other_ssid_is_not_us() {
        let m = AprsMessage::decode(b":SK0MT-2  :hello").unwrap();
        assert!(!m.addressed_to(&gw()));
    }

    #[test]
    fn ack_and_rej_are_classified() {
        let a = AprsMessage::decode(b":SK0MT-1  :ack01").unwrap();
        assert_eq!(a.kind, AprsKind::Ack);
        let r = AprsMessage::decode(b":SK0MT-1  :rejB2").unwrap();
        assert_eq!(r.kind, AprsKind::Reject);
        assert!(a.is_ack_or_rej() && r.is_ack_or_rej());
    }

    #[test]
    fn position_reports_are_not_messages() {
        assert!(AprsMessage::decode(b"!5930.00N/01803.00E-").is_none());
        assert!(AprsMessage::decode(b"=5930.00N/01803.00E-").is_none());
        assert!(AprsMessage::decode(b">status").is_none());
        assert!(AprsMessage::decode(b"}SM0ABC>APRS:hello").is_none());
        assert!(AprsMessage::decode(b"A1\x05").is_none());
    }

    #[test]
    fn trailing_cr_is_stripped() {
        let m = AprsMessage::decode(b":SK0MT-1  :#rf hi{1\r").unwrap();
        assert_eq!(m.text, "#rf hi");
        assert_eq!(m.msgid.as_deref(), Some("1"));
    }

    #[test]
    fn ack_round_trip() {
        let call: Callsign = "SM0ABC-7".parse().unwrap();
        let info = ack_info(&call, "01");
        let m = AprsMessage::decode(&info).unwrap();
        assert_eq!(m.addressee, "SM0ABC-7");
        assert_eq!(m.kind, AprsKind::Ack);
        assert_eq!(m.text, "ack01");
        assert!(m.msgid.is_none());
        assert_eq!(info, b":SM0ABC-7 :ack01");
    }

    #[test]
    fn split_requires_a_body() {
        assert_eq!(
            split_channel_line("#rf hello from the trail", ""),
            Some(("#rf", "hello from the trail"))
        );
        assert!(split_channel_line("#rf", "").is_none());
        assert!(split_channel_line("hello", "").is_none());
        assert_eq!(split_channel_line("hello", "#rf"), Some(("#rf", "hello")));
        assert!(is_help("HELP"));
        assert!(is_help("?"));
        assert!(is_help(""));
        assert!(!is_help("#rf hello"));
        // Help is classified before the default-channel fallback in the
        // bridge; `?` with a default set would otherwise look like a line.
        assert_eq!(split_channel_line("?", "#rf"), Some(("#rf", "?")));
    }

    #[test]
    fn bulletins_are_not_a_callsign() {
        let m = AprsMessage::decode(b":BLN1     :weather").unwrap();
        assert!(!m.addressed_to(&gw()));
    }

    #[test]
    fn uncompressed_position() {
        let b = AprsBeacon::decode(b"!5930.00N/01803.00E-QTH Kista").unwrap();
        assert_eq!(b.summary, "59°30.00N 018°03.00E - QTH Kista");
        let b = AprsBeacon::decode(b"=5930.00N/01803.00E-").unwrap();
        assert_eq!(b.summary, "59°30.00N 018°03.00E");
        let b = AprsBeacon::decode(b"/092345z5930.00N/01803.00E-hello").unwrap();
        assert_eq!(b.summary, "59°30.00N 018°03.00E - hello");
    }

    /// A longitude field is nine raw octets: `decode_uncompressed` checks
    /// only that they are valid UTF-8 and end in a hemisphere letter, and
    /// then slices the result at byte offsets. A multi-byte character
    /// straddling one of those offsets used to abort the process — and this
    /// runs before the frame is checked for being ours, so any station on
    /// frequency could send it.
    #[test]
    fn a_position_with_a_multibyte_coordinate_is_refused_not_fatal() {
        for lon in [
            "AB\u{e9}1234E",
            "x\u{10348}xA.E",
            "\u{e9}\u{e9}\u{e9}\u{e9}E",
        ] {
            assert_eq!(lon.len(), 9, "test vector must fill the field: {lon:?}");
            let mut info = vec![b'!'];
            info.extend_from_slice(b"5930.00N");
            info.push(b'/');
            info.extend_from_slice(lon.as_bytes());
            info.push(b'X');
            // Returning at all is half the point. The other half is that a
            // field that is not a coordinate is not rendered as one: the
            // fallback shows the printable remainder, without a degree sign.
            let got = AprsBeacon::decode(&info);
            assert!(
                !got.iter().any(|b| b.summary.contains('\u{b0}')),
                "a non-ASCII coordinate was rendered as a position: {got:?}"
            );
        }
        // The ASCII form still decodes.
        assert!(AprsBeacon::decode(b"!5930.00N/01803.00E-").is_some());
    }

    #[test]
    fn status_beacon() {
        let b = AprsBeacon::decode(b">Hello from the hill").unwrap();
        assert_eq!(b.summary, "Hello from the hill");
        let b = AprsBeacon::decode(b">092345zQRT for supper").unwrap();
        assert_eq!(b.summary, "QRT for supper");
    }

    #[test]
    fn compressed_position() {
        // All-33 (ASCII '!') is zeros: 90°N 180°W.
        let b = AprsBeacon::decode(b"!/!!!!!!!!!").unwrap();
        assert!(
            b.summary.contains("90.00°N") && b.summary.contains("180.00°W"),
            "{}",
            b.summary
        );
    }

    #[test]
    fn mice_uses_the_destination() {
        // 59°30.00N, flags N=1 offset=0 E=0 → dest digits 5,9,3,0,0,0
        // with flags 0,0,0,1,0,0 → 593P00
        let dest: Callsign = "593P00".parse().unwrap();
        // lon 18°03.00E: deg 18+28=46, min 3+28=31, hund 0+28=28
        let mut info = vec![b'`', 46, 31, 28, b' ', b' ', b'-', b'/', b'h', b'i'];
        // speed/course bytes 4-5 must be in range; 28+0 = 28
        info[4] = 28;
        info[5] = 28;
        let b = decode_beacon(&dest, &info).unwrap();
        assert!(
            b.summary.starts_with("59°30.00N 018°03.00E"),
            "{}",
            b.summary
        );
        assert!(b.summary.contains("hi"), "{}", b.summary);
    }

    #[test]
    fn messages_and_airc_are_not_beacons() {
        assert!(AprsBeacon::decode(b":SK0MT-1  :hello").is_none());
        assert!(AprsBeacon::decode(b"A1\x05hello").is_none());
        assert!(AprsBeacon::decode(b"!5930.00N/01803.00E-").is_some());
    }
}
