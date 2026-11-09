//! OneDrive sign-in runs on jobs, tied to the open form.

use super::*;
use fp_sources::onedrive::auth::{self, DeviceCode, OneDriveAuth, OneDriveToken};
use ui::onedrive::LinkState;

/// A finished sign-in: its tokens, and who signed in.
pub(super) type SignedIn = Option<(OneDriveToken, String)>;

impl App {
    fn onedrive_form(&mut self, generation: u64) -> Option<&mut ui::SourceForm> {
        self.ui.source_form.as_mut().filter(|f| {
            f.kind == fp_sources::SourceKind::OneDrive && f.onedrive.generation == generation
        })
    }

    pub(super) fn onedrive_start(&mut self) {
        let generation = self.ui.next_seq();
        let Some(f) = self
            .ui
            .source_form
            .as_mut()
            .filter(|f| f.kind == fp_sources::SourceKind::OneDrive)
        else {
            return;
        };
        f.onedrive.generation = generation;
        f.onedrive.state = LinkState::Requesting;
        let Some(client_id) = auth::client_id(&f.onedrive.client_id).map(str::to_owned) else {
            f.onedrive.state = LinkState::Failed(tr!("onedrive-client-id-desc").into());
            return;
        };
        // A source edited keeps the directory it signed in to.
        let tenant = match &f.editing {
            Some(fp_sources::SourceConfig::OneDrive(c)) => c.tenant().to_string(),
            _ => auth::DEFAULT_TENANT.to_string(),
        };
        self.jobs.spawn("onedrive-start", move || {
            Job::OneDriveStarted(
                generation,
                OneDriveAuth::new()
                    .start(&client_id, &tenant)
                    .map_err(|e| ui::errors::source(&e)),
            )
        });
    }

    pub(super) fn onedrive_poll(&mut self, generation: u64, code: DeviceCode) {
        let current = self.onedrive_form(generation).is_some_and(|f| {
            matches!(&f.onedrive.state, LinkState::Waiting { code: c, .. } if c.user_code == code.user_code)
        });
        if !current {
            return;
        }
        self.jobs.spawn("onedrive-poll", move || {
            let api = OneDriveAuth::new();
            let result = api
                .poll(&code)
                .and_then(|token| {
                    token
                        .map(|t| api.account(&t.access_token).map(|account| (t, account)))
                        .transpose()
                })
                .map_err(|e| ui::errors::source(&e));
            Job::OneDrivePolled(generation, code, Box::new(result))
        });
    }

    pub(super) fn onedrive_started(&mut self, generation: u64, result: Result<DeviceCode, String>) {
        let Some(f) = self.onedrive_form(generation) else {
            return;
        };
        if !matches!(f.onedrive.state, LinkState::Requesting) {
            return;
        }
        f.onedrive.state = match result {
            Ok(code) => LinkState::Waiting {
                code,
                // Microsoft asks for `interval` between polls, the first too.
                polled: Instant::now(),
                polling: false,
            },
            Err(e) => LinkState::Failed(e),
        };
        self.panels[MAIN].request_repaint();
    }

    pub(super) fn onedrive_polled(
        &mut self,
        generation: u64,
        code: DeviceCode,
        result: Result<SignedIn, String>,
    ) {
        let id = format!("onedrive-{}", fp_core::playback::now_ms());
        let Some(f) = self.onedrive_form(generation) else {
            return;
        };
        if !matches!(&f.onedrive.state, LinkState::Waiting { code: c, .. } if c.user_code == code.user_code)
        {
            return;
        }
        match result {
            Ok(Some((token, account))) => {
                let cfg = ui::onedrive::signed_in(f, id, code.client(), token, account);
                f.onedrive.selected = Some(cfg);
                f.onedrive.state = LinkState::Connected;
            }
            Ok(None) if code.expired() => {
                f.onedrive.state = LinkState::Failed(tr!("onedrive-expired").into());
            }
            Ok(None) => {
                if let LinkState::Waiting { polling, .. } = &mut f.onedrive.state {
                    *polling = false;
                }
            }
            Err(e) => f.onedrive.state = LinkState::Failed(e),
        }
        self.panels[MAIN].request_repaint();
    }
}
