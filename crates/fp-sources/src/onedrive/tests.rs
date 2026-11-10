use super::auth::{DeviceCode, OneDriveAuth, OneDriveToken, now};
use super::*;
use crate::test_server::{Req, TestServer, pattern, resp, serve_bytes};
use std::sync::atomic::AtomicUsize;

fn json(status: u16, v: Value) -> crate::test_server::Resp {
    resp(
        status,
        v.to_string(),
        &[("Content-Type", "application/json")],
    )
}

fn form(req: &Req) -> HashMap<String, String> {
    req.body
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), crate::urlutil::percent_decode(v)))
        .collect()
}

fn config(token: &str, expires_at: u64) -> OneDriveConfig {
    OneDriveConfig {
        id: "od".into(),
        name: "My OneDrive".into(),
        account: "alice@outlook.com".into(),
        client_id: "test-client".into(),
        tenant: String::new(),
        token: OneDriveToken {
            access_token: token.into(),
            refresh_token: "refresh-1".into(),
            expires_at,
        },
        parallel_downloads: false,
    }
}

fn source(server: &TestServer, token: &str, expires_at: u64) -> OneDriveSource {
    let api = OneDriveAuth::with_test_endpoint(&server.base).unwrap();
    OneDriveSource::with_api(&config(token, expires_at), api).unwrap()
}

fn later() -> u64 {
    now() + 3600
}

#[test]
fn signs_in_with_a_code() {
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    let server = TestServer::start(move |req| {
        let f = form(req);
        match req.path() {
            "/consumers/oauth2/v2.0/devicecode" => {
                assert_eq!(f["client_id"], "app-id");
                assert_eq!(f["scope"], "Files.Read offline_access");
                json(
                    200,
                    serde_json::json!({
                        "user_code": "ABCD-EFGH",
                        "device_code": "secret-device-code",
                        "verification_uri": "https://microsoft.com/devicelogin",
                        "expires_in": 900,
                        "interval": 5,
                    }),
                )
            }
            "/consumers/oauth2/v2.0/token" => {
                assert_eq!(
                    f["grant_type"],
                    "urn:ietf:params:oauth:grant-type:device_code"
                );
                assert_eq!(f["device_code"], "secret-device-code");
                if p.fetch_add(1, Ordering::SeqCst) == 0 {
                    json(400, serde_json::json!({"error": "authorization_pending"}))
                } else {
                    json(
                        200,
                        serde_json::json!({
                            "token_type": "Bearer",
                            "access_token": "access-1",
                            "refresh_token": "refresh-1",
                            "expires_in": 3600,
                        }),
                    )
                }
            }
            "/v1.0/me/drive" => {
                assert_eq!(req.header("authorization"), Some("Bearer access-1"));
                json(
                    200,
                    serde_json::json!({"owner": {"user": {"displayName": "Alice", "email": "alice@outlook.com"}}}),
                )
            }
            other => panic!("unexpected {other}"),
        }
    });
    let api = OneDriveAuth::with_test_endpoint(&server.base).unwrap();
    let code = api.start("app-id", "consumers").unwrap();
    assert_eq!(code.user_code, "ABCD-EFGH");
    assert_eq!(code.interval, 5);
    assert_eq!(code.client(), ("app-id", "consumers"));
    let debug = format!("{code:?}");
    assert!(!debug.contains("secret-device-code"), "{debug}");
    assert!(api.poll(&code).unwrap().is_none(), "still waiting");
    let token = api.poll(&code).unwrap().unwrap();
    assert_eq!(token.access_token, "access-1");
    assert_eq!(token.refresh_token, "refresh-1");
    assert!(!token.expiring());
    assert!(!format!("{token:?}").contains("access-1"));
    assert_eq!(
        api.account(&token.access_token).unwrap(),
        "alice@outlook.com"
    );
}

#[test]
fn a_declined_or_bad_sign_in_says_why() {
    let server = TestServer::start(|req| match req.path() {
        "/consumers/oauth2/v2.0/token" => {
            json(400, serde_json::json!({"error": "authorization_declined"}))
        }
        _ => json(
            400,
            serde_json::json!({
                "error": "invalid_client",
                "error_description": "AADSTS7000218: The request body must contain client_assertion.\r\nTrace ID: 1",
            }),
        ),
    });
    let api = OneDriveAuth::with_test_endpoint(&server.base).unwrap();
    let e = api.start("app-id", "consumers").unwrap_err().to_string();
    assert!(e.contains("AADSTS7000218"), "{e}");
    assert!(!e.contains("Trace ID"), "{e}");
    assert!(api.start("bad id/..", "consumers").is_err());
    assert!(api.start("app-id", "../evil").is_err());
}

#[test]
fn test_endpoints_must_be_loopback() {
    assert!(OneDriveAuth::with_test_endpoint("http://127.0.0.1:1").is_ok());
    assert!(OneDriveAuth::with_test_endpoint("https://graph.example.com").is_err());
    assert!(OneDriveAuth::with_test_endpoint("http://evil.example.com").is_err());
}

#[test]
fn an_expired_code_is_not_polled() {
    let server = TestServer::start(|_| panic!("polled"));
    let api = OneDriveAuth::with_test_endpoint(&server.base).unwrap();
    let mut code = test_code();
    code.expires_at = 1;
    assert!(matches!(api.poll(&code), Err(Error::Auth(_))));
}

fn test_code() -> DeviceCode {
    // Built through a sign-in so private fields are filled.
    let server = TestServer::start(|_| {
        json(
            200,
            serde_json::json!({"user_code": "C", "device_code": "D", "expires_in": 60}),
        )
    });
    OneDriveAuth::with_test_endpoint(&server.base)
        .unwrap()
        .start("app-id", "consumers")
        .unwrap()
}

#[test]
fn lists_folders_page_after_page() {
    let server = TestServer::start(|req| {
        assert_eq!(req.header("authorization"), Some("Bearer access-1"));
        match (req.path(), req.url.contains("skiptoken")) {
            ("/v1.0/me/drive/root/children", false) => {
                assert!(req.url.contains("$top="), "{}", req.url);
                assert!(req.url.contains("downloadUrl"), "{}", req.url);
                let host = req.header("host").unwrap().to_string();
                json(
                    200,
                    serde_json::json!({
                        "value": [
                            {"id": "F2", "name": "b10_180_LR.mp4", "size": 10, "file": {},
                             "lastModifiedDateTime": "2024-01-12T10:22:33Z"},
                            {"id": "D1", "name": "VR", "folder": {"childCount": 3}},
                            {"id": "N1", "name": "Notes", "package": {"type": "oneNote"}},
                        ],
                        "@odata.nextLink": format!("http://{host}/v1.0/me/drive/root/children?$skiptoken=2"),
                    }),
                )
            }
            ("/v1.0/me/drive/root/children", true) => json(
                200,
                serde_json::json!({"value": [{"id": "F1", "name": "B2.mp4", "size": 5, "file": {}}]}),
            ),
            (other, _) => panic!("unexpected {other}"),
        }
    });
    let s = source(&server, "access-1", later());
    let entries = s.list(None).unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["VR", "B2.mp4", "b10_180_LR.mp4"]);
    assert_eq!(entries[0].kind, fp_core::source::EntryKind::Directory);
    assert_eq!(entries[1].size, Some(5));
    assert_eq!(entries[2].modified, Some(1_705_054_953));
    assert!(entries[2].format.is_some(), "format from the name");
    let folder = &entries[0].location;
    assert_eq!(folder, "source://source/od/onedrive/D1");
    assert_eq!(
        s.canonical_location(folder).as_deref(),
        Some(folder.as_str())
    );
    assert_eq!(crate::location_owner(folder).as_deref(), Some("od"));
    // Another source's items aren't this one's.
    assert!(s.list(Some("source://source/other/onedrive/D1")).is_err());
    assert!(
        s.canonical_location("source://source/od/webdav/x")
            .is_none()
    );
    assert!(s.updated_config().is_none(), "nothing renewed");
}

#[test]
fn a_page_link_elsewhere_is_not_followed() {
    let server = TestServer::start(|_| {
        json(
            200,
            serde_json::json!({"value": [], "@odata.nextLink": "https://evil.example.com/steal"}),
        )
    });
    let s = source(&server, "access-1", later());
    assert!(matches!(s.list(None), Err(Error::InvalidLocation { .. })));
}

#[test]
fn renews_an_expired_token_and_hands_it_to_be_saved() {
    let server = TestServer::start(|req| match req.path() {
        "/consumers/oauth2/v2.0/token" => {
            let f = form(req);
            assert_eq!(f["grant_type"], "refresh_token");
            assert_eq!(f["refresh_token"], "refresh-1");
            assert_eq!(f["client_id"], "test-client");
            json(
                200,
                serde_json::json!({"access_token": "access-2", "refresh_token": "refresh-2", "expires_in": 3600}),
            )
        }
        "/v1.0/me/drive/root/children" => {
            if req.header("authorization") == Some("Bearer access-2") {
                json(200, serde_json::json!({"value": []}))
            } else {
                json(
                    401,
                    serde_json::json!({"error": {"code": "InvalidAuthenticationToken"}}),
                )
            }
        }
        other => panic!("unexpected {other}"),
    });
    // Saved as working for an hour, but Graph turns it away: renewed once.
    let s = source(&server, "access-1", later());
    assert!(s.list(None).unwrap().is_empty());
    let Some(SourceConfig::OneDrive(saved)) = s.updated_config() else {
        panic!("renewed tokens to save");
    };
    assert_eq!(saved.token.access_token, "access-2");
    assert_eq!(saved.token.refresh_token, "refresh-2");
    assert!(s.updated_config().is_none(), "handed out once");
    // An expired one is renewed before asking.
    let s = source(&server, "access-1", 0);
    assert!(s.list(None).unwrap().is_empty());
    assert_eq!(
        server.count(|r| r.path() == "/consumers/oauth2/v2.0/token"),
        2
    );
}

#[test]
fn a_sign_in_microsoft_no_longer_takes_is_signed_out() {
    let server = TestServer::start(|req| match req.path() {
        "/consumers/oauth2/v2.0/token" => json(
            400,
            serde_json::json!({"error": "invalid_grant", "error_description": "AADSTS70008: expired"}),
        ),
        _ => panic!("Graph asked without a token"),
    });
    let s = source(&server, "access-1", 0);
    match s.list(None) {
        Err(Error::SignedOut(name)) => assert_eq!(name, "My OneDrive"),
        other => panic!("{other:?}"),
    }
}

/// A drive with one 3 MiB video whose download links stop working once
/// `expired` is set: a new link is then the only way to read it.
struct Drive {
    server: TestServer,
    expired: Arc<AtomicBool>,
}

fn drive() -> Drive {
    let data = Arc::new(pattern(3 << 20));
    let expired = Arc::new(AtomicBool::new(false));
    let links = Arc::new(AtomicUsize::new(0));
    let (d, e, l) = (data.clone(), expired.clone(), links.clone());
    let server = TestServer::start(move |req| {
        let host = req.header("host").unwrap().to_string();
        match req.path() {
            "/v1.0/me/drive/items/V1" => {
                assert_eq!(req.header("authorization"), Some("Bearer access-1"));
                let n = l.fetch_add(1, Ordering::SeqCst) + 1;
                json(
                    200,
                    serde_json::json!({
                        "id": "V1", "file": {},
                        "parentReference": {"id": "D1"},
                        "@microsoft.graph.downloadUrl": format!("http://{host}/download/{}", 10 + n),
                    }),
                )
            }
            "/v1.0/me/drive/items/D1/children" => json(
                200,
                serde_json::json!({"value": [
                    {"id": "V1", "name": "movie_180_LR.mp4", "size": d.len(), "file": {},
                     "@microsoft.graph.downloadUrl": format!("http://{host}/download/0")},
                    {"id": "S1", "name": "movie_180_LR.en.srt", "size": 3, "file": {}},
                    {"id": "S2", "name": "other.srt", "size": 3, "file": {}},
                ]}),
            ),
            path if path.starts_with("/download/") => {
                // Links made before it expired are turned away.
                let n: usize = path["/download/".len()..].parse().unwrap();
                if e.load(Ordering::SeqCst) && n == 0 {
                    resp(410, "gone", &[])
                } else {
                    serve_bytes(req, &d, true)
                }
            }
            other => panic!("unexpected {other}"),
        }
    });
    Drive { server, expired }
}

fn read(file: &dyn ByteSource, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0; len];
    let mut done = 0;
    while done < len {
        let n = file
            .read_at(offset + done as u64, &mut buf[done..])
            .unwrap();
        assert!(n > 0, "short read at {}", offset + done as u64);
        done += n;
    }
    buf
}

#[test]
fn plays_from_a_listings_link_and_renews_it_when_it_expires() {
    let d = drive();
    let s = source(&d.server, "access-1", later());
    let entries = s.list(Some("source://source/od/onedrive/D1")).unwrap();
    let video = entries
        .iter()
        .find(|e| e.name == "movie_180_LR.mp4")
        .unwrap();
    let file = s.open(&video.location).unwrap();
    assert!(file.remote());
    assert_eq!(file.size(), Some(3 << 20));
    let data = pattern(3 << 20);
    assert_eq!(read(&*file, 0, 4096), data[..4096]);
    assert_eq!(
        d.server.count(|r| r.path() == "/v1.0/me/drive/items/V1"),
        0,
        "the listing's link was used"
    );
    d.expired.store(true, Ordering::SeqCst);
    let at = (2 << 20) + 17;
    assert_eq!(
        read(&*file, at, 100_000),
        data[at as usize..at as usize + 100_000]
    );
    // One link from Graph for the expired one (others may be renewing too,
    // but each read renews once at most).
    let asked = d.server.count(|r| r.path() == "/v1.0/me/drive/items/V1");
    assert!((1..=2).contains(&asked), "{asked}");
}

#[test]
fn opening_without_a_listing_asks_graph_for_a_link() {
    let d = drive();
    let s = source(&d.server, "access-1", later());
    let file = s.open("source://source/od/onedrive/V1").unwrap();
    assert_eq!(
        read(&*file, 1 << 20, 10),
        pattern(3 << 20)[1 << 20..(1 << 20) + 10]
    );
    assert_eq!(d.server.count(|r| r.path() == "/v1.0/me/drive/items/V1"), 1);
    assert!(s.open("source://source/od/onedrive/root").is_err());
}

#[test]
fn finds_subtitles_beside_a_video() {
    let d = drive();
    let s = source(&d.server, "access-1", later());
    // Found from Graph, with no listing first.
    let video = file_entry(
        "movie_180_LR.mp4",
        "source://source/od/onedrive/V1",
        None,
        None,
    );
    assert_eq!(
        s.parent(&video.location).as_deref(),
        Some("source://source/od/onedrive/D1")
    );
    let sidecars = s.sidecars(&video).unwrap();
    let subtitles: Vec<_> = sidecars
        .subtitles
        .iter()
        .map(|s| s.location.as_str())
        .collect();
    assert_eq!(subtitles, ["source://source/od/onedrive/S1"]);
}
