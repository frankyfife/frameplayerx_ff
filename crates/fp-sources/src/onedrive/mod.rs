//! Microsoft OneDrive, through Microsoft Graph.
//!
//! The source signs in with a code ([`auth`]) and keeps the tokens in its
//! configuration; a renewed token is handed back to be saved through
//! [`Source::updated_config`]. Folders are listed with
//! `/me/drive/items/{id}/children`, page after page. Files are read from
//! the pre-authenticated download link Graph gives each one, with Range
//! requests through the block cache and read-ahead; those links expire after
//! about an hour, so a new one is asked for when one gets old or is turned
//! away. Locations are `source://source/<source>/onedrive/<item id>`, so
//! history and queues keep working when items move.

pub mod auth;
#[cfg(test)]
mod tests;

use crate::config::{OneDriveConfig, SourceConfig, SourceKind};
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpFile, HttpOptions, Renew};
use crate::timeutil::parse_iso8601;
use crate::urlutil::{encode_segment, same_origin};
use crate::{Sidecars, Source, dir_entry, file_entry, location, match_sidecars, sort_entries};
use auth::{OneDriveAuth, OneDriveToken};
use fp_core::source::{ByteSource, Entry};
use fp_core::sync::lock;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The location kind of a drive item.
const KIND: &str = "onedrive";
/// The key of the drive's top folder.
const ROOT: &str = "root";
/// What a listing asks Graph for about each item.
const FIELDS: &str = "id,name,size,folder,file,package,lastModifiedDateTime,parentReference,@microsoft.graph.downloadUrl";
/// Items per page of a listing (Graph's most).
const PAGE: u32 = 999;
const JSON_LIMIT: u64 = 32 << 20;
/// A listing's download link is used for opening only this long: an hour
/// is what Graph gives one.
const LINK_FRESH: Duration = Duration::from_secs(15 * 60);
/// A link being read from is renewed once it is this old.
const LINK_MAX_AGE: Duration = Duration::from_secs(40 * 60);
/// Items remembered from listings (their folder and download link).
const REMEMBERED: usize = 20_000;

/// What a listing told about an item.
#[derive(Clone, Default)]
struct Item {
    /// Its folder's key.
    parent: Option<String>,
    /// Its download link, and when it was got.
    link: Option<(String, Instant)>,
}

struct Inner {
    id: String,
    name: String,
    config: Mutex<OneDriveConfig>,
    api: OneDriveAuth,
    /// Graph, as the current access token.
    graph: Mutex<Option<(String, HttpClient)>>,
    /// Downloads: their links carry their own authorization.
    downloads: HttpClient,
    refreshing: Mutex<()>,
    /// The tokens were renewed and not yet handed out to be saved.
    changed: AtomicBool,
    items: Mutex<HashMap<String, Item>>,
}

/// A signed-in OneDrive. Construction does no I/O.
pub struct OneDriveSource {
    inner: Arc<Inner>,
}

impl fmt::Debug for OneDriveSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneDriveSource")
            .field("config", &*lock(&self.inner.config))
            .finish_non_exhaustive()
    }
}

impl OneDriveSource {
    pub fn new(config: &OneDriveConfig) -> Result<Self> {
        Self::with_api(config, OneDriveAuth::new())
    }

    /// The source against `api`'s services: [`OneDriveAuth::with_test_endpoint`]
    /// for tests.
    #[doc(hidden)]
    pub fn with_api(config: &OneDriveConfig, api: OneDriveAuth) -> Result<Self> {
        if config.id.is_empty() {
            return Err(Error::Config(
                "OneDrive source needs a stable identifier".into(),
            ));
        }
        if config.token.access_token.is_empty() && config.token.refresh_token.is_empty() {
            return Err(Error::Config("OneDrive source is not signed in".into()));
        }
        let downloads = HttpClient::new(
            HttpOptions {
                cache: crate::cache::CacheOptions::for_downloads(config.parallel_downloads),
                ..HttpOptions::default()
            },
            None,
        );
        Ok(Self {
            inner: Arc::new(Inner {
                id: config.id.clone(),
                name: config.name.clone(),
                config: Mutex::new(config.clone()),
                api,
                graph: Mutex::new(None),
                downloads,
                refreshing: Mutex::new(()),
                changed: AtomicBool::new(false),
                items: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Who the drive belongs to, asked of Graph: shows the sign-in works.
    pub fn probe(&self) -> Result<String> {
        let token = self.inner.token(false)?;
        match self.inner.api.account(&token) {
            Err(Error::Auth(_)) => {
                let token = self.inner.token(true)?;
                self.inner.api.account(&token)
            }
            r => r,
        }
        .map_err(|e| self.inner.signed_out(e))
    }

    /// The item key a location of this source names; `None` for the top.
    fn key(&self, location: Option<&str>) -> Result<Option<String>> {
        let Some(location) = location else {
            return Ok(None);
        };
        let l = location::decode(location)
            .filter(|l| l.source_id == self.inner.id && l.kind == KIND)
            .ok_or_else(|| Error::invalid(location, "not owned by this OneDrive source"))?;
        Ok((l.key != ROOT).then_some(l.key))
    }

    fn location(&self, key: &str) -> String {
        location::encode(&self.inner.id, KIND, key)
    }

    fn entry(&self, v: &Value, parent: &str) -> Option<Entry> {
        let id = text(v, "id")?;
        let name = text(v, "name")?;
        // OneNote notebooks and other packages are neither file nor folder.
        if v.get("package").is_some() {
            return None;
        }
        let modified = text(v, "lastModifiedDateTime").and_then(|t| parse_iso8601(&t));
        let location = self.location(&id);
        let entry = if v.get("folder").is_some() {
            dir_entry(name, location, modified)
        } else if v.get("file").is_some() {
            file_entry(
                name,
                location,
                v.get("size").and_then(Value::as_u64),
                modified,
            )
        } else {
            return None;
        };
        let mut items = lock(&self.inner.items);
        if items.len() >= REMEMBERED {
            items.clear();
        }
        items.insert(
            id,
            Item {
                parent: Some(parent.to_string()),
                link: text(v, "@microsoft.graph.downloadUrl").map(|l| (l, Instant::now())),
            },
        );
        Some(entry)
    }

    /// A download link for item `key`: a listing's, while fresh, else
    /// asked of Graph.
    fn link(&self, key: &str, fresh: bool) -> Result<String> {
        if fresh
            && let Some((link, at)) = lock(&self.inner.items)
                .get(key)
                .and_then(|i| i.link.clone())
            && at.elapsed() < LINK_FRESH
        {
            return Ok(link);
        }
        self.inner.download_link(key)
    }
}

impl Inner {
    /// An access token that works for a while yet: renewed first when it is
    /// about to stop (or `force`d, after Graph turned it away).
    fn token(&self, force: bool) -> Result<String> {
        let _guard = lock(&self.refreshing);
        let config = lock(&self.config).clone();
        if !force && !config.token.access_token.is_empty() && !config.token.expiring() {
            return Ok(config.token.access_token);
        }
        let client_id = auth::client_id(&config.client_id)
            .ok_or_else(|| Error::Config("OneDrive needs an app (client) ID".into()))?;
        let token: OneDriveToken = self
            .api
            .refresh(client_id, config.tenant(), &config.token)
            .map_err(|e| self.signed_out(e))?;
        let access = token.access_token.clone();
        lock(&self.config).token = token;
        *lock(&self.graph) = None;
        self.changed.store(true, Ordering::Release);
        Ok(access)
    }

    /// Names this source in a sign-in that no longer works.
    fn signed_out(&self, e: Error) -> Error {
        match e {
            Error::SignedOut(_) => Error::SignedOut(self.name.clone()),
            e => e,
        }
    }

    fn client(&self, token: &str) -> HttpClient {
        let mut graph = lock(&self.graph);
        if let Some((t, client)) = graph.as_ref()
            && t == token
        {
            return client.clone();
        }
        let client = HttpClient::new_with_headers(
            HttpOptions {
                read_timeout: Duration::from_secs(20),
                ..HttpOptions::default()
            },
            vec![
                ("Authorization".into(), format!("Bearer {token}")),
                ("Accept".into(), "application/json".into()),
            ],
            self.api.graph(),
        );
        *graph = Some((token.to_string(), client.clone()));
        client
    }

    /// GETs `url` from Graph as JSON, signing in again once if Graph turns
    /// the token away.
    fn get(&self, url: &str) -> Result<Value> {
        if !same_origin(url, self.api.graph()) {
            return Err(Error::invalid(url, "not a Microsoft Graph address"));
        }
        let once = |token: &str| -> Result<Value> {
            let reply = self
                .client(token)
                .request("GET", url, &[], None, JSON_LIMIT)?
                .error_for_status(url)?;
            Ok(serde_json::from_slice(&reply.body)?)
        };
        match once(&self.token(false)?) {
            Err(Error::Auth(_)) => once(&self.token(true)?),
            r => r,
        }
    }

    fn item_url(&self, key: &str) -> String {
        format!(
            "{}/me/drive/items/{}",
            self.api.graph(),
            encode_segment(key)
        )
    }

    /// A new download link for item `key`.
    fn download_link(&self, key: &str) -> Result<String> {
        let v = self.get(&format!(
            "{}?$select=id,file,@microsoft.graph.downloadUrl",
            self.item_url(key)
        ))?;
        if v.get("file").is_none() {
            return Err(Error::Unsupported("a OneDrive folder is not a file".into()));
        }
        text(&v, "@microsoft.graph.downloadUrl")
            .ok_or_else(|| Error::parse("OneDrive item", "no download link"))
    }
}

impl Source for OneDriveSource {
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn name(&self) -> &str {
        &self.inner.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::OneDrive
    }

    fn describe(&self) -> String {
        SourceConfig::OneDrive(lock(&self.inner.config).clone()).describe()
    }

    fn canonical_location(&self, location: &str) -> Option<String> {
        self.key(Some(location)).ok().map(|_| location.to_string())
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let key = self.key(location)?;
        let folder = key.as_deref().unwrap_or(ROOT);
        let base = match &key {
            Some(k) => self.inner.item_url(k),
            None => format!("{}/me/drive/root", self.inner.api.graph()),
        };
        let mut next = Some(format!("{base}/children?$select={FIELDS}&$top={PAGE}"));
        let mut entries = Vec::new();
        while let Some(url) = next.take() {
            let page = self.inner.get(&url)?;
            for item in page
                .get("value")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                entries.extend(self.entry(item, folder));
            }
            next = text(&page, "@odata.nextLink");
        }
        sort_entries(&mut entries);
        Ok(entries)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        let key = self
            .key(Some(location))?
            .ok_or_else(|| Error::Unsupported("the top of a OneDrive is not a file".into()))?;
        let inner = self.inner.clone();
        let renew_key = key.clone();
        let renew: Renew = Arc::new(move || inner.download_link(&renew_key));
        let open = |link: String| {
            HttpFile::open_renewing(&self.inner.downloads, &link, renew.clone(), LINK_MAX_AGE)
        };
        match open(self.link(&key, true)?) {
            // A listing's link that stopped working early.
            Err(Error::Auth(_) | Error::NotFound(_)) => open(self.link(&key, false)?),
            r => r,
        }
    }

    fn updated_config(&self) -> Option<SourceConfig> {
        self.inner
            .changed
            .swap(false, Ordering::AcqRel)
            .then(|| SourceConfig::OneDrive(lock(&self.inner.config).clone()))
    }

    fn parent(&self, location: &str) -> Option<String> {
        let key = self.key(Some(location)).ok()??;
        let known = lock(&self.inner.items)
            .get(&key)
            .and_then(|i| i.parent.clone());
        let parent = match known {
            Some(p) => p,
            None => {
                let v = self
                    .inner
                    .get(&format!(
                        "{}?$select=id,parentReference",
                        self.inner.item_url(&key)
                    ))
                    .ok()?;
                v.pointer("/parentReference/id")?.as_str()?.to_string()
            }
        };
        Some(self.location(&parent))
    }

    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        // Locations name items, not files: match by the video's name.
        let mut out = Sidecars::from_entry(video);
        if let Some(parent) = self.parent(&video.location) {
            let siblings = self.list(Some(&parent))?;
            out.merge(match_sidecars(&video.name, &siblings));
        }
        Ok(out)
    }
}

fn text(v: &Value, field: &str) -> Option<String> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
