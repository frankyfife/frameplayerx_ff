//! OneDrive source setup: signing in to a Microsoft account with a code.

use super::sources::{name_hint, text_field};
use super::theme::{self, Weight};
use super::widgets::{self, Kind};
use super::{Action, SourceForm};
use fp_sources::OneDriveConfig;
use fp_sources::onedrive::auth::{self, DeviceCode, OneDriveToken};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub enum LinkState {
    #[default]
    Idle,
    Requesting,
    /// Showing the code; Microsoft is asked every `interval` whether it was
    /// entered.
    Waiting {
        code: DeviceCode,
        polled: Instant,
        polling: bool,
    },
    Connected,
    Failed(String),
}

#[derive(Default)]
pub struct Connect {
    /// Tells a job's answer for this sign-in from one for an earlier one.
    pub generation: u64,
    pub state: LinkState,
    /// The Azure app to sign in as, when the build has none built in.
    pub client_id: String,
    /// The signed-in source, ready to save.
    pub selected: Option<OneDriveConfig>,
}

impl Connect {
    /// Drops the sign-in and any job's answer for it.
    pub(super) fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.state = LinkState::Idle;
        self.selected = None;
    }
}

/// The source a finished sign-in makes: the one being edited keeps its id,
/// so its history stays, unless another account signed in.
pub fn signed_in(
    f: &SourceForm,
    id: String,
    (client_id, tenant): (&str, &str),
    token: OneDriveToken,
    account: String,
) -> OneDriveConfig {
    let id = match &f.editing {
        Some(fp_sources::SourceConfig::OneDrive(old)) if old.account == account => old.id.clone(),
        _ => id,
    };
    OneDriveConfig {
        id,
        name: match f.name.trim() {
            "" => "OneDrive".into(),
            name => name.into(),
        },
        account,
        // The built-in app isn't saved: a build with a new one moves to it.
        client_id: if client_id == auth::CLIENT_ID {
            String::new()
        } else {
            client_id.to_string()
        },
        tenant: if tenant == auth::DEFAULT_TENANT {
            String::new()
        } else {
            tenant.to_string()
        },
        token,
        parallel_downloads: f.parallel_downloads,
    }
}

fn sign_in_rows(r: &mut widgets::Rows, f: &mut SourceForm, actions: &mut Vec<Action>) {
    match f.onedrive.state.clone() {
        LinkState::Waiting { code, .. } => {
            r.row_with(
                tr!("quick-connect-code"),
                Some(tr!("onedrive-waiting")),
                |ui| {
                    ui.label(
                        egui::RichText::new(code.verification_uri.trim_start_matches("https://"))
                            .font(theme::font(Weight::Bold, 28.0))
                            .color(theme::ACCENT),
                    );
                },
                |ui| {
                    if widgets::button_sized(ui, None, tr!("common-cancel"), Kind::Secondary, 36.0)
                        .clicked()
                    {
                        f.onedrive.reset();
                    }
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(&code.user_code)
                            .font(theme::font(Weight::Bold, 32.0))
                            .color(theme::ACCENT),
                    );
                },
            );
        }
        LinkState::Idle | LinkState::Requesting | LinkState::Failed(_) => {
            let requesting = matches!(f.onedrive.state, LinkState::Requesting);
            let has_app = auth::client_id(&f.onedrive.client_id).is_some();
            r.row(
                tr!("onedrive-sign-in"),
                Some(tr!("onedrive-sign-in-desc")),
                |ui| {
                    if ui
                        .add_enabled_ui(!requesting && has_app, |ui| {
                            widgets::button(
                                ui,
                                Some(super::icons::KEY_RETURN),
                                tr!("quick-connect-get-code"),
                                Kind::Primary,
                            )
                        })
                        .inner
                        .clicked()
                    {
                        f.onedrive.selected = None;
                        f.onedrive.state = LinkState::Requesting;
                        actions.push(Action::OneDriveStart);
                    }
                    if requesting {
                        ui.spinner();
                    }
                },
            );
        }
        LinkState::Connected => {
            let account = f
                .onedrive
                .selected
                .as_ref()
                .map(|c| c.account.clone())
                .unwrap_or_default();
            let desc = tr!("onedrive-signed-in", account = account.as_str());
            r.row(tr!("onedrive-sign-in"), Some(&desc), |ui| {
                if widgets::button_sized(ui, None, tr!("common-clear"), Kind::Secondary, 36.0)
                    .clicked()
                {
                    f.onedrive.reset();
                }
                ui.label(egui::RichText::new(super::icons::CHECK_CIRCLE).color(theme::OK));
            });
        }
    }
}

/// The source, once it has signed in and Save is pressed.
pub fn form(
    ui: &mut egui::Ui,
    f: &mut SourceForm,
    actions: &mut Vec<Action>,
) -> Option<fp_sources::SourceConfig> {
    if let LinkState::Waiting {
        code,
        polled,
        polling,
    } = &mut f.onedrive.state
    {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        if code.expired() {
            f.onedrive.state = LinkState::Failed(tr!("onedrive-expired").into());
        } else if !*polling && polled.elapsed() >= Duration::from_secs(code.interval) {
            *polling = true;
            *polled = Instant::now();
            actions.push(Action::OneDrivePoll {
                generation: f.onedrive.generation,
                code: code.clone(),
            });
        }
    }
    widgets::rows(ui, |r| {
        r.row(
            tr!("source-name"),
            Some(tr!("source-name-desc-server")),
            |ui| text_field(ui, &mut f.name, name_hint(f.kind), false),
        );
        // A build with no app of its own signs in as the user's.
        if auth::CLIENT_ID.is_empty() {
            r.row(
                tr!("onedrive-client-id"),
                Some(tr!("onedrive-client-id-desc")),
                |ui| {
                    text_field(
                        ui,
                        &mut f.onedrive.client_id,
                        "00000000-0000-0000-0000-000000000000",
                        false,
                    );
                },
            );
        }
        sign_in_rows(r, f, actions);
        r.switch(
            tr!("source-parallel-downloads"),
            Some(tr!("source-parallel-downloads-desc")),
            &mut f.parallel_downloads,
        );
    });
    ui.add_space(12.0);
    if let LinkState::Failed(error) = &f.onedrive.state {
        ui.label(egui::RichText::new(error).color(theme::WARN));
    }
    ui.add_space(16.0);
    if let Some(mut cfg) = f.onedrive.selected.clone()
        && matches!(f.onedrive.state, LinkState::Connected)
    {
        cfg.name = match f.name.trim() {
            "" => "OneDrive".into(),
            name => name.into(),
        };
        cfg.parallel_downloads = f.parallel_downloads;
        if widgets::button(
            ui,
            Some(super::icons::CHECK),
            tr!("common-save"),
            Kind::Primary,
        )
        .clicked()
        {
            return Some(fp_sources::SourceConfig::OneDrive(cfg));
        }
    } else {
        ui.add_enabled_ui(false, |ui| {
            widgets::button(
                ui,
                Some(super::icons::CHECK),
                tr!("common-save"),
                Kind::Primary,
            )
        });
        ui.label(egui::RichText::new(tr!("onedrive-sign-in-required")).color(theme::TEXT_3));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_sources::SourceConfig;

    fn token(access: &str) -> OneDriveToken {
        OneDriveToken {
            access_token: access.into(),
            refresh_token: "refresh".into(),
            expires_at: 1,
        }
    }

    #[test]
    fn signing_in_again_keeps_the_source_only_for_the_same_account() {
        let mut f = SourceForm {
            kind: fp_sources::SourceKind::OneDrive,
            name: " Cloud ".into(),
            ..SourceForm::default()
        };
        let first = signed_in(
            &f,
            "onedrive-1".into(),
            ("my-app", auth::DEFAULT_TENANT),
            token("a"),
            "alice@outlook.com".into(),
        );
        assert_eq!(first.id, "onedrive-1");
        assert_eq!(first.name, "Cloud");
        assert_eq!(first.client_id, "my-app", "a user's own app is kept");
        assert!(first.tenant.is_empty(), "personal accounts by default");
        let mut edit = super::super::sources::edit_form(&SourceConfig::OneDrive(first.clone()));
        assert!(matches!(edit.onedrive.state, LinkState::Connected));
        assert_eq!(edit.onedrive.client_id, "my-app");
        assert_eq!(
            super::super::sources::build_config(&edit, "unused".into()).unwrap(),
            SourceConfig::OneDrive(first.clone())
        );
        let again = signed_in(
            &edit,
            "onedrive-2".into(),
            ("my-app", "common"),
            token("b"),
            "alice@outlook.com".into(),
        );
        assert_eq!(again.id, "onedrive-1", "history stays with the source");
        assert_eq!(again.tenant, "common");
        let other = signed_in(
            &edit,
            "onedrive-2".into(),
            ("my-app", auth::DEFAULT_TENANT),
            token("c"),
            "bob@outlook.com".into(),
        );
        assert_eq!(other.id, "onedrive-2", "another account is another source");
        edit.onedrive.reset();
        assert!(edit.onedrive.selected.is_none());
        assert!(super::super::sources::build_config(&edit, "x".into()).is_err());
        f.onedrive.state = LinkState::Requesting;
        f.onedrive.reset();
        assert!(matches!(f.onedrive.state, LinkState::Idle));
    }
}
