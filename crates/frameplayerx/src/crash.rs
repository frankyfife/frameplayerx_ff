//! Crash reports. When the last run crashed (its log has a panic or a fatal
//! signal, see [`crate::logger`]), the next start asks whether to send a
//! report. Nothing is sent unless the user chooses to, and then only
//! [`Report::text`], which they can read first: the versions, and that
//! run's log with the names of videos, paths and server addresses replaced.

use fp_sources::urlutil::last_segment;
use std::collections::HashMap;

/// The relay that files reports as issues in a private GitHub repository
/// (tools/crash-relay; DEVELOPMENT.md sets it up). Without one, nobody is
/// asked: this fork has none, and its crashes are not the original
/// project's to sort out.
pub const REPORT_URL: Option<&str> = None;

/// How much of the log a report carries: its start (versions, devices,
/// decoders) and its end, where the crash is.
const LOG_HEAD: usize = 8 * 1024;
const LOG_TAIL: usize = 40 * 1024;

/// A crash report, ready to show and to send.
#[derive(Clone, Debug)]
pub struct Report {
    /// What happened: the panic or the signal.
    pub summary: String,
    /// Everything that is sent, as the user reads it.
    pub text: String,
}

/// Names a report must not show, by what they stand for.
#[derive(Clone, Debug, Default)]
pub struct Private {
    /// Videos' titles and locations.
    pub videos: Vec<String>,
    /// What sources were set up with: names, addresses, users, passwords.
    pub servers: Vec<String>,
    /// Folders and devices on the headset.
    pub local: Vec<String>,
}

impl Private {
    /// Every video in the library (hidden ones too), every source and the
    /// folders and devices in the settings.
    pub fn gather(
        library: &fp_library::Library,
        sources: &[fp_sources::SourceConfig],
        settings: &crate::settings::Settings,
    ) -> Private {
        let mut p = Private::default();
        for (title, location) in library.names().unwrap_or_default() {
            p.videos.push(title);
            p.videos.push(location);
        }
        for s in sources {
            leaves(&serde_json::to_value(s).unwrap_or_default(), &mut p.servers);
        }
        leaves(
            &serde_json::to_value(&settings.haptic_devices).unwrap_or_default(),
            &mut p.local,
        );
        p.local.extend(std::env::var("HOME"));
        p
    }
}

/// The strings in a JSON value, but for the names of variants.
fn leaves(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|v| leaves(v, out)),
        serde_json::Value::Object(o) => o
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "type" | "kind"))
            .for_each(|(_, v)| leaves(v, out)),
        _ => {}
    }
}

impl Report {
    /// The report on the run that wrote `log`, if it crashed. `system`
    /// describes the headset.
    pub fn from_log(log: &str, private: &Private, system: &str) -> Option<Report> {
        let summary = what_happened(log)?;
        let mut scrub = Scrubber::new(private);
        let summary = scrub.text(&summary);
        let log = scrub.text(&shorten(log));
        let text = format!(
            "FramePlayerX {}\n{system}\n\nWhat happened: {summary}\n\nLog of that session:\n{log}",
            env!("CARGO_PKG_VERSION")
        );
        Some(Report { summary, text })
    }

    /// What the relay is sent.
    pub fn body(&self) -> Vec<u8> {
        serde_json::json!({
            "app": "frameplayerx",
            "version": env!("CARGO_PKG_VERSION"),
            "summary": self.summary,
            "text": self.text,
        })
        .to_string()
        .into_bytes()
    }
}

/// The headset's system, for the report.
pub fn system() -> String {
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        release.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix('=')?;
            Some(v.trim_matches('"').to_string())
        })
    };
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_string()
    };
    let mut s = format!(
        "System: {}",
        field("PRETTY_NAME").unwrap_or_else(|| std::env::consts::OS.into())
    );
    if let Some(build) = field("BUILD_ID").or_else(|| field("VERSION_ID")) {
        s += &format!(" (build {build})");
    }
    s += &format!(
        ", kernel {}, {}",
        read("/proc/sys/kernel/osrelease"),
        std::env::consts::ARCH
    );
    let model = read("/proc/device-tree/model");
    let model = if model.is_empty() {
        read("/sys/class/dmi/id/product_name")
    } else {
        model
    };
    if !model.is_empty() {
        s += &format!(", {model}");
    }
    s
}

/// The panic, else the fatal signal, in the log.
fn what_happened(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    if let Some(i) = lines.iter().rposition(|l| l.contains(" panicked at ")) {
        // "1.234 [ERROR] frameplayerx::logger: thread main panicked at a.rs:1:2:",
        // then the message.
        let head = lines[i].split_once("] ").map_or(lines[i], |(_, rest)| rest);
        let head = head.split_once(": ").map_or(head, |(_, rest)| rest);
        let message = lines.get(i + 1).map_or("", |l| l.trim());
        return Some(format!("{} {message}", head.trim_end_matches(':')));
    }
    let line = lines
        .iter()
        .rev()
        .find(|l| l.contains("[FATAL] crashed with signal"))?;
    let signal: i32 = line.rsplit(' ').next()?.trim().parse().ok()?;
    let name = match signal {
        libc::SIGSEGV => "segmentation fault",
        libc::SIGBUS => "bus error",
        libc::SIGILL => "illegal instruction",
        libc::SIGFPE => "arithmetic error",
        libc::SIGABRT => "abort",
        _ => "fatal signal",
    };
    Some(format!("signal {signal} ({name})"))
}

/// The log's start and end, at line breaks.
fn shorten(log: &str) -> String {
    if log.len() <= LOG_HEAD + LOG_TAIL {
        return log.to_string();
    }
    let mut head = LOG_HEAD;
    while !log.is_char_boundary(head) {
        head -= 1;
    }
    let head = log[..head].rfind('\n').map_or(head, |i| i + 1);
    let mut tail = log.len() - LOG_TAIL;
    while !log.is_char_boundary(tail) {
        tail += 1;
    }
    let tail = log[tail..].find('\n').map_or(tail, |i| tail + i + 1);
    format!("{}…\n{}", &log[..head], &log[tail..])
}

/// Video, subtitle, script and picture files, by extension.
const PRIVATE_EXTENSIONS: &[&str] = &[
    "mp4",
    "m4v",
    "mkv",
    "webm",
    "avi",
    "mov",
    "wmv",
    "flv",
    "ts",
    "m2ts",
    "mts",
    "ssif",
    "iso",
    "mpg",
    "mpeg",
    "3gp",
    "srt",
    "ass",
    "ssa",
    "vtt",
    "sub",
    "funscript",
    "jpg",
    "jpeg",
    "png",
    "webp",
];

/// Where people's own files are.
const PRIVATE_ROOTS: &[&str] = &[
    "/home/",
    "/var/home/",
    "/run/media/",
    "/media/",
    "/mnt/",
    "/tmp/",
    "/storage/",
    "/sdcard/",
    "~/",
];

/// The words placeholders are made of: a name inside one of them would be
/// replaced in the placeholders too.
const PLACEHOLDER_WORDS: &str = "video server local address file path";

/// Replaces private names with placeholders, the same name with the same
/// one: the names it was given, then addresses, paths and media files.
struct Scrubber {
    /// Names, longest first, and what they stand for.
    known: Vec<(String, &'static str)>,
    given: HashMap<String, String>,
    counts: HashMap<&'static str, usize>,
}

impl Scrubber {
    fn new(p: &Private) -> Scrubber {
        let mut known = Vec::new();
        let mut add = |name: &str, kind: &'static str| {
            let name = name.trim();
            // Short or numberless names would replace ordinary words.
            if name.chars().count() >= 4
                && name.chars().any(char::is_alphabetic)
                && !PLACEHOLDER_WORDS.contains(name)
            {
                known.push((name.to_string(), kind));
            }
        };
        for v in &p.videos {
            add(v, "video");
            let file = last_segment(v);
            add(&file, "video");
            add(
                file.rsplit_once('.').map_or(&file, |(stem, _)| stem),
                "video",
            );
        }
        for s in &p.servers {
            add(s, "server");
            if let Some(host) = host(s) {
                add(host, "server");
            }
        }
        for l in &p.local {
            add(l, "local");
        }
        known.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        known.dedup_by(|a, b| a.0 == b.0);
        Scrubber {
            known,
            given: HashMap::new(),
            counts: HashMap::new(),
        }
    }

    fn placeholder(&mut self, kind: &'static str, value: &str) -> String {
        if let Some(p) = self.given.get(value) {
            return p.clone();
        }
        let n = self.counts.entry(kind).or_default();
        *n += 1;
        let p = format!("<{kind} {n}>");
        self.given.insert(value.to_string(), p.clone());
        p
    }

    fn text(&mut self, s: &str) -> String {
        let mut out = s.to_string();
        let known = std::mem::take(&mut self.known);
        for (name, kind) in &known {
            if out.contains(name.as_str()) {
                let p = self.placeholder(kind, name);
                out = out.replace(name.as_str(), &p);
            }
        }
        self.known = known;
        let mut result = String::with_capacity(out.len());
        for piece in out.split_inclusive(char::is_whitespace) {
            let token = piece.trim_end_matches(char::is_whitespace);
            result += &self.token(token);
            result += &piece[token.len()..];
        }
        result
    }

    /// One word: an address, a path or a media file becomes a placeholder,
    /// the quotes and punctuation round it stay.
    fn token(&mut self, token: &str) -> String {
        let start = token
            .find(|c: char| !matches!(c, '"' | '\'' | '(' | '[' | '`'))
            .unwrap_or(token.len());
        let end = token
            .trim_end_matches(['"', '\'', ')', ']', ',', ';', ':', '.', '`'])
            .len()
            .max(start);
        let word = &token[start..end];
        let kind = if word.contains(['<', '>']) {
            // Part of a placeholder already.
            return token.to_string();
        } else if word.contains("://") || is_ip(word) || is_email(word) {
            "address"
        } else if PRIVATE_ROOTS.iter().any(|r| word.starts_with(r)) {
            "path"
        } else if word
            .rsplit_once('.')
            .is_some_and(|(_, ext)| PRIVATE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        {
            "file"
        } else {
            return token.to_string();
        };
        let p = self.placeholder(kind, word);
        format!("{}{p}{}", &token[..start], &token[end..])
    }
}

/// The host of a URL.
fn host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some(host.rsplit_once(':').map_or(host, |(h, _)| h))
}

/// An IPv4 address, with or without a port.
fn is_ip(word: &str) -> bool {
    let ip = word.split_once(':').map_or(word, |(ip, _)| ip);
    let parts: Vec<&str> = ip.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
}

fn is_email(word: &str) -> bool {
    word.split_once('@')
        .is_some_and(|(user, domain)| !user.is_empty() && domain.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
    0.012 [INFO ] frameplayerx: FramePlayerX 1.0.0 on SteamOS
    0.500 [INFO ] fp_sources::mediabrowser: signed in to http://alice:hunter22@jf.lan:8096 as alice
    2.100 [INFO ] frameplayerx::playback: opening /home/deck/Videos/Garden Party_FLAT.webm
    2.300 [INFO ] frameplayerx::playback: format Flat mono (from the file name), 1920x1080
    2.310 [WARN ] frameplayerx::playback: Garden Party_FLAT.webm: no hardware decoder, decoding with vp9
    3.000 [INFO ] fp_media: reading from 192.168.1.20:8096 for Holiday Trip
";

    fn private() -> Private {
        Private {
            videos: vec![
                "Garden Party".into(),
                "/home/deck/Videos/Garden Party_FLAT.webm".into(),
                "Holiday Trip".into(),
                "mb:http://jf.lan:8096#item=42".into(),
            ],
            servers: vec![
                "My Jellyfin".into(),
                "http://jf.lan:8096".into(),
                "alice".into(),
                "hunter22".into(),
            ],
            local: vec!["/home/deck/Videos".into()],
        }
    }

    #[test]
    fn only_a_crashed_run_has_a_report() {
        assert!(Report::from_log(LOG, &private(), "System: test").is_none());
    }

    #[test]
    fn a_panic_says_where_and_why() {
        let log = format!(
            "{LOG}    6.000 [ERROR] frameplayerx::logger: thread decoder panicked at crates/fp-media/src/decode.rs:120:9:\n\
             index out of bounds\n   0: frameplayerx::logger::init::{{closure#0}}\n"
        );
        let r = Report::from_log(&log, &private(), "System: test").unwrap();
        assert_eq!(
            r.summary,
            "thread decoder panicked at crates/fp-media/src/decode.rs:120:9 index out of bounds"
        );
        assert!(r.text.contains("decode.rs:120:9"));
    }

    #[test]
    fn a_signal_is_named() {
        let log = format!("{LOG}          [FATAL] crashed with signal 11\n");
        let r = Report::from_log(&log, &private(), "System: test").unwrap();
        assert_eq!(r.summary, "signal 11 (segmentation fault)");
    }

    #[test]
    fn names_paths_and_addresses_are_replaced() {
        let log = format!("{LOG}          [FATAL] crashed with signal 11\n");
        let r = Report::from_log(&log, &private(), "System: test").unwrap();
        for secret in [
            "Garden",
            "Holiday",
            "alice",
            "hunter22",
            "jf.lan",
            "192.168",
            "/home/deck",
        ] {
            assert!(!r.text.contains(secret), "{secret} in\n{}", r.text);
        }
        // What tells a crash apart stays.
        for kept in [
            "1920x1080",
            "decoding with vp9",
            "Flat mono",
            "FramePlayerX 1.0.0",
        ] {
            assert!(r.text.contains(kept), "{kept} missing from\n{}", r.text);
        }
    }

    #[test]
    fn the_same_name_gets_the_same_placeholder() {
        let mut s = Scrubber::new(&private());
        assert_eq!(
            s.text("Holiday Trip, again Holiday Trip"),
            "<video 1>, again <video 1>"
        );
        assert_eq!(s.text("then Holiday Trip.mkv"), "then <video 1>.mkv");
        assert_eq!(
            s.text("\"/home/deck/Other.mp4\" Clip.MP4: 10.0.0.2"),
            "\"<path 1>\" <file 1>: <address 1>"
        );
    }

    #[test]
    fn long_logs_keep_their_start_and_end() {
        let middle = "0.1 [INFO ] filler line\n".repeat(10_000);
        let log = format!("first line\n{middle}last line\n");
        let short = shorten(&log);
        assert!(short.len() < LOG_HEAD + LOG_TAIL + 10);
        assert!(short.starts_with("first line\n") && short.ends_with("last line\n"));
    }

    #[test]
    fn the_body_carries_exactly_the_report() {
        let log = format!("{LOG}          [FATAL] crashed with signal 6\n");
        let r = Report::from_log(&log, &private(), "System: test").unwrap();
        let body: serde_json::Value = serde_json::from_slice(&r.body()).unwrap();
        assert_eq!(body["text"], r.text.as_str());
        assert_eq!(body["summary"], "signal 6 (abort)");
    }
}
